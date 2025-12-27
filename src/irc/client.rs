use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use base64::prelude::*;

use super::message::{IrcCommand, IrcMessage};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub use_tls: bool,
    pub accept_invalid_certs: bool,
    pub nick: String,
    pub username: String,
    pub realname: String,
    pub password: Option<String>,
    // SASL authentication
    pub sasl_username: Option<String>,
    pub sasl_password: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "irc.afternet.org".to_string(),
            port: 6697,
            use_tls: true,
            accept_invalid_certs: false,
            nick: "fmIRC_User".to_string(),
            username: "rustirc".to_string(),
            realname: "fmIRC".to_string(),
            password: None,
            sasl_username: None,
            sasl_password: None,
        }
    }
}

pub struct IrcClient {
    config: ServerConfig,
    tx: Option<mpsc::Sender<String>>,
}

impl IrcClient {
    pub fn new(config: ServerConfig) -> Self {
        Self { config, tx: None }
    }

    pub async fn connect(
        &mut self,
        incoming_tx: mpsc::Sender<IrcMessage>,
        outgoing_rx: mpsc::Receiver<IrcCommand>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        tracing::info!("Connecting to {} (TLS: {})", addr, self.config.use_tls);

        // Send connecting message
        let _ = incoming_tx.send(IrcMessage {
            tags: None,
            prefix: None,
            command: IrcCommand::Notice("*".to_string(), format!("Connecting to {}...", addr)),
            raw: String::new(),
        }).await;

        let stream = match TcpStream::connect(&addr).await {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("Failed to connect to {}: {}", addr, e);
                tracing::error!("{}", msg);
                return Err(msg.into());
            }
        };

        tracing::info!("TCP connected to {}", addr);
        let _ = incoming_tx.send(IrcMessage {
            tags: None,
            prefix: None,
            command: IrcCommand::Notice("*".to_string(), "TCP connection established".to_string()),
            raw: String::new(),
        }).await;

        if self.config.use_tls {
            let _ = incoming_tx.send(IrcMessage {
                tags: None,
                prefix: None,
                command: IrcCommand::Notice("*".to_string(), "Starting TLS handshake...".to_string()),
                raw: String::new(),
            }).await;
            self.handle_tls_connection(stream, incoming_tx, outgoing_rx).await
        } else {
            self.handle_plain_connection(stream, incoming_tx, outgoing_rx).await
        }
    }

    async fn handle_tls_connection(
        &mut self,
        stream: TcpStream,
        incoming_tx: mpsc::Sender<IrcMessage>,
        outgoing_rx: mpsc::Receiver<IrcCommand>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Set up native TLS
        let mut tls_builder = native_tls::TlsConnector::builder();

        if self.config.accept_invalid_certs {
            tracing::warn!("Accepting invalid certificates (insecure!)");
            tls_builder.danger_accept_invalid_certs(true);
            tls_builder.danger_accept_invalid_hostnames(true);
        }

        let tls_connector = tls_builder.build()
            .map_err(|e| format!("Failed to create TLS connector: {}", e))?;

        let connector = tokio_native_tls::TlsConnector::from(tls_connector);

        tracing::info!("Starting TLS handshake with {}", self.config.host);

        let tls_stream = match connector.connect(&self.config.host, stream).await {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("TLS handshake failed: {}. Try disabling TLS or enabling 'Accept invalid certs'", e);
                tracing::error!("{}", msg);
                return Err(msg.into());
            }
        };

        tracing::info!("TLS handshake complete");
        let _ = incoming_tx.send(IrcMessage {
            tags: None,
            prefix: None,
            command: IrcCommand::Notice("*".to_string(), "TLS connection established".to_string()),
            raw: String::new(),
        }).await;

        self.run_connection(tls_stream, incoming_tx, outgoing_rx).await
    }

    async fn handle_plain_connection(
        &mut self,
        stream: TcpStream,
        incoming_tx: mpsc::Sender<IrcMessage>,
        outgoing_rx: mpsc::Receiver<IrcCommand>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.run_connection(stream, incoming_tx, outgoing_rx).await
    }

    /// Common connection handler for both TLS and plain connections
    async fn run_connection<S>(
        &mut self,
        stream: S,
        incoming_tx: mpsc::Sender<IrcMessage>,
        mut outgoing_rx: mpsc::Receiver<IrcCommand>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);

        // Send registration (may include SASL handshake)
        self.send_registration(&mut writer, &mut reader, &incoming_tx).await?;

        // Create channel for sending
        let (send_tx, mut send_rx) = mpsc::channel::<String>(100);
        self.tx = Some(send_tx);

        // Spawn writer task
        let writer_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(cmd) = outgoing_rx.recv() => {
                        let line = format!("{}\r\n", cmd);
                        tracing::debug!("> {}", line.trim());
                        if let Err(e) = writer.write_all(line.as_bytes()).await {
                            tracing::error!("Write error: {}", e);
                            break;
                        }
                        let _ = writer.flush().await;
                    }
                    Some(raw) = send_rx.recv() => {
                        tracing::debug!("> {}", raw.trim());
                        if let Err(e) = writer.write_all(raw.as_bytes()).await {
                            tracing::error!("Write error: {}", e);
                            break;
                        }
                        let _ = writer.flush().await;
                    }
                    else => break,
                }
            }
        });

        // Read loop
        self.read_loop(reader, incoming_tx).await;

        writer_handle.abort();
        Ok(())
    }

    async fn send_registration<W, R>(
        &self,
        writer: &mut W,
        reader: &mut R,
        incoming_tx: &mpsc::Sender<IrcMessage>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        W: tokio::io::AsyncWrite + Unpin,
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let config = &self.config;

        // SASL authentication if configured
        if config.sasl_username.is_some() && config.sasl_password.is_some() {
            self.do_sasl_auth(writer, reader, incoming_tx).await?;
        }

        // Send PASS if configured
        if let Some(ref pass) = config.password {
            let pass_cmd = IrcCommand::Pass(pass.clone());
            let line = format!("{}\r\n", pass_cmd);
            tracing::debug!("> {}", line.trim());
            writer.write_all(line.as_bytes()).await?;
        }

        // Send NICK
        let nick_cmd = IrcCommand::Nick(config.nick.clone());
        let nick_line = format!("{}\r\n", nick_cmd);
        tracing::debug!("> {}", nick_line.trim());
        writer.write_all(nick_line.as_bytes()).await?;

        // Send USER
        let user_cmd = IrcCommand::User {
            username: config.username.clone(),
            realname: config.realname.clone(),
        };
        let user_line = format!("{}\r\n", user_cmd);
        tracing::debug!("> {}", user_line.trim());
        writer.write_all(user_line.as_bytes()).await?;

        writer.flush().await?;

        let _ = incoming_tx.send(IrcMessage {
            tags: None,
            prefix: None,
            command: IrcCommand::Notice("*".to_string(), "Registration sent, waiting for response...".to_string()),
            raw: String::new(),
        }).await;

        Ok(())
    }

    /// Perform SASL PLAIN authentication
    async fn do_sasl_auth<W, R>(
        &self,
        writer: &mut W,
        reader: &mut R,
        incoming_tx: &mpsc::Sender<IrcMessage>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        W: tokio::io::AsyncWrite + Unpin,
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let config = &self.config;
        let sasl_user = config.sasl_username.as_ref().unwrap();
        let sasl_pass = config.sasl_password.as_ref().unwrap();

        let _ = incoming_tx.send(IrcMessage {
            tags: None,
            prefix: None,
            command: IrcCommand::Notice("*".to_string(), "Starting SASL authentication...".to_string()),
            raw: String::new(),
        }).await;

        // Step 1: Request capabilities
        let cap_ls = "CAP LS 302\r\n";
        tracing::debug!("> {}", cap_ls.trim());
        writer.write_all(cap_ls.as_bytes()).await?;
        writer.flush().await?;

        // Step 2: Read CAP LS response and check for SASL
        let mut line = String::new();
        let mut sasl_available = false;

        loop {
            line.clear();
            if reader.read_line(&mut line).await? == 0 {
                return Err("Connection closed during CAP negotiation".into());
            }
            let trimmed = line.trim();
            tracing::debug!("< {}", trimmed);

            if let Some(msg) = IrcMessage::parse(trimmed) {
                // Forward to GUI for display
                let _ = incoming_tx.send(msg.clone()).await;

                if let IrcCommand::Cap(subcmd, params) = &msg.command {
                    if subcmd == "LS" || subcmd == "*" {
                        if let Some(caps) = params {
                            // Check if sasl is in the capability list
                            let caps_lower = caps.to_lowercase();
                            if caps_lower.contains("sasl") {
                                sasl_available = true;
                            }
                        }
                        // CAP LS can be multi-line, wait for one without * prefix
                        if subcmd == "LS" {
                            break;
                        }
                    }
                }
            }
        }

        if !sasl_available {
            let _ = incoming_tx.send(IrcMessage {
                tags: None,
                prefix: None,
                command: IrcCommand::Notice("*".to_string(), "Server does not support SASL, continuing without authentication".to_string()),
                raw: String::new(),
            }).await;
            // Send CAP END and continue
            let cap_end = "CAP END\r\n";
            tracing::debug!("> {}", cap_end.trim());
            writer.write_all(cap_end.as_bytes()).await?;
            writer.flush().await?;
            return Ok(());
        }

        // Step 3: Request SASL capability
        let cap_req = "CAP REQ :sasl\r\n";
        tracing::debug!("> {}", cap_req.trim());
        writer.write_all(cap_req.as_bytes()).await?;
        writer.flush().await?;

        // Step 4: Wait for CAP ACK
        loop {
            line.clear();
            if reader.read_line(&mut line).await? == 0 {
                return Err("Connection closed during SASL".into());
            }
            let trimmed = line.trim();
            tracing::debug!("< {}", trimmed);

            if let Some(msg) = IrcMessage::parse(trimmed) {
                let _ = incoming_tx.send(msg.clone()).await;

                if let IrcCommand::Cap(subcmd, params) = &msg.command {
                    if subcmd == "ACK" {
                        if let Some(caps) = params {
                            if caps.to_lowercase().contains("sasl") {
                                break;
                            }
                        }
                    } else if subcmd == "NAK" {
                        let _ = incoming_tx.send(IrcMessage {
                            tags: None,
                            prefix: None,
                            command: IrcCommand::Notice("*".to_string(), "Server rejected SASL capability".to_string()),
                            raw: String::new(),
                        }).await;
                        let cap_end = "CAP END\r\n";
                        writer.write_all(cap_end.as_bytes()).await?;
                        writer.flush().await?;
                        return Ok(());
                    }
                }
            }
        }

        // Step 5: Start SASL PLAIN
        let auth_plain = "AUTHENTICATE PLAIN\r\n";
        tracing::debug!("> {}", auth_plain.trim());
        writer.write_all(auth_plain.as_bytes()).await?;
        writer.flush().await?;

        // Step 6: Wait for AUTHENTICATE +
        loop {
            line.clear();
            if reader.read_line(&mut line).await? == 0 {
                return Err("Connection closed during SASL".into());
            }
            let trimmed = line.trim();
            tracing::debug!("< {}", trimmed);

            if let Some(msg) = IrcMessage::parse(trimmed) {
                let _ = incoming_tx.send(msg.clone()).await;

                if let IrcCommand::Authenticate(data) = &msg.command {
                    if data == "+" {
                        break;
                    }
                }
            }
        }

        // Step 7: Send credentials (base64 of \0username\0password)
        let credentials = format!("\0{}\0{}", sasl_user, sasl_pass);
        let encoded = BASE64_STANDARD.encode(credentials.as_bytes());
        let auth_creds = format!("AUTHENTICATE {}\r\n", encoded);
        tracing::debug!("> AUTHENTICATE <credentials>");
        writer.write_all(auth_creds.as_bytes()).await?;
        writer.flush().await?;

        // Step 8: Wait for 903 (success) or 904 (failure)
        loop {
            line.clear();
            if reader.read_line(&mut line).await? == 0 {
                return Err("Connection closed during SASL".into());
            }
            let trimmed = line.trim();
            tracing::debug!("< {}", trimmed);

            if let Some(msg) = IrcMessage::parse(trimmed) {
                let _ = incoming_tx.send(msg.clone()).await;

                if let IrcCommand::Numeric(num, _params) = &msg.command {
                    match *num {
                        903 => {
                            // RPL_SASLSUCCESS
                            let _ = incoming_tx.send(IrcMessage {
                                tags: None,
                                prefix: None,
                                command: IrcCommand::Notice("*".to_string(), "SASL authentication successful!".to_string()),
                                raw: String::new(),
                            }).await;
                            break;
                        }
                        904 | 905 | 906 => {
                            // ERR_SASLFAIL, ERR_SASLTOOLONG, ERR_SASLABORTED
                            let _ = incoming_tx.send(IrcMessage {
                                tags: None,
                                prefix: None,
                                command: IrcCommand::Notice("*".to_string(), "SASL authentication failed!".to_string()),
                                raw: String::new(),
                            }).await;
                            break;
                        }
                        900 => {
                            // RPL_LOGGEDIN - also indicates success
                            continue;
                        }
                        _ => {}
                    }
                }
            }
        }

        // Step 9: Send CAP END
        let cap_end = "CAP END\r\n";
        tracing::debug!("> {}", cap_end.trim());
        writer.write_all(cap_end.as_bytes()).await?;
        writer.flush().await?;

        Ok(())
    }

    async fn read_loop<R: tokio::io::AsyncBufRead + Unpin>(
        &mut self,
        mut reader: R,
        incoming_tx: mpsc::Sender<IrcMessage>,
    ) {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    tracing::info!("Connection closed by server");
                    let _ = incoming_tx.send(IrcMessage {
                        tags: None,
                        prefix: None,
                        command: IrcCommand::Notice("*".to_string(), "Connection closed by server".to_string()),
                        raw: String::new(),
                    }).await;
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        tracing::debug!("< {}", trimmed);
                        if let Some(msg) = IrcMessage::parse(trimmed) {
                            // Handle PING automatically
                            if let IrcCommand::Ping(server) = &msg.command {
                                let pong = format!("PONG :{}\r\n", server);
                                if let Some(ref tx) = self.tx {
                                    let _ = tx.send(pong).await;
                                }
                            }
                            let _ = incoming_tx.send(msg).await;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Read error: {}", e);
                    let _ = incoming_tx.send(IrcMessage {
                        tags: None,
                        prefix: None,
                        command: IrcCommand::Notice("*".to_string(), format!("Read error: {}", e)),
                        raw: String::new(),
                    }).await;
                    break;
                }
            }
        }
    }
}
