use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use base64::prelude::*;

use super::message::{IrcCommand, IrcMessage};

/// Read a line from the reader, handling non-UTF8 data gracefully
async fn read_line_lossy<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    buf.clear();
    reader.read_until(b'\n', buf).await
}

/// Convert bytes to string with lossy UTF-8 conversion
fn bytes_to_string_lossy(buf: &[u8]) -> String {
    String::from_utf8_lossy(buf).trim().to_string()
}

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
            nick: "Linefeed_User".to_string(),
            username: "rustirc".to_string(),
            realname: "Linefeed".to_string(),
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

        // Send connecting message (non-blocking)
        let _ = incoming_tx.try_send(IrcMessage {
            tags: None,
            prefix: None,
            command: IrcCommand::Notice("*".to_string(), format!("Connecting to {}...", addr)),
            raw: String::new(),
        });

        let stream = match TcpStream::connect(&addr).await {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("Failed to connect to {}: {}", addr, e);
                tracing::error!("{}", msg);
                return Err(msg.into());
            }
        };

        // Enable TCP_NODELAY for lower latency (disable Nagle's algorithm)
        if let Err(e) = stream.set_nodelay(true) {
            tracing::warn!("Failed to set TCP_NODELAY: {}", e);
        }

        tracing::info!("TCP connected to {}", addr);
        let _ = incoming_tx.try_send(IrcMessage {
            tags: None,
            prefix: None,
            command: IrcCommand::Notice("*".to_string(), "TCP connection established".to_string()),
            raw: String::new(),
        });

        if self.config.use_tls {
            let _ = incoming_tx.try_send(IrcMessage {
                tags: None,
                prefix: None,
                command: IrcCommand::Notice("*".to_string(), "Starting TLS handshake...".to_string()),
                raw: String::new(),
            });
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
        let _ = incoming_tx.try_send(IrcMessage {
            tags: None,
            prefix: None,
            command: IrcCommand::Notice("*".to_string(), "TLS connection established".to_string()),
            raw: String::new(),
        });

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
            use tokio::time::{timeout, Duration};
            loop {
                tokio::select! {
                    Some(cmd) = outgoing_rx.recv() => {
                        let line = format!("{}\r\n", cmd);
                        tracing::debug!("> {}", line.trim());
                        if let Err(e) = writer.write_all(line.as_bytes()).await {
                            tracing::error!("Write error: {}", e);
                            break;
                        }
                        // Drain any queued commands before flushing (batch writes)
                        while let Ok(Some(cmd)) = timeout(Duration::from_micros(100), outgoing_rx.recv()).await {
                            let line = format!("{}\r\n", cmd);
                            tracing::debug!("> {}", line.trim());
                            if let Err(e) = writer.write_all(line.as_bytes()).await {
                                tracing::error!("Write error: {}", e);
                                break;
                            }
                        }
                        let _ = writer.flush().await;
                    }
                    Some(raw) = send_rx.recv() => {
                        // PONG and other raw messages - write immediately (time-sensitive)
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

        // Always do CAP negotiation to request IRCv3 features
        self.do_cap_negotiation(writer, reader, incoming_tx).await?;

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

        let _ = incoming_tx.try_send(IrcMessage {
            tags: None,
            prefix: None,
            command: IrcCommand::Notice("*".to_string(), "Registration sent, waiting for response...".to_string()),
            raw: String::new(),
        });

        Ok(())
    }

    /// Negotiate IRCv3 capabilities including SASL authentication
    async fn do_cap_negotiation<W, R>(
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
        let want_sasl = config.sasl_username.is_some() && config.sasl_password.is_some();

        // IRCv3 capabilities we want to request
        const DESIRED_CAPS: &[&str] = &[
            "away-notify",      // Get notified when users go away/back
            "account-notify",   // Get notified when users log in/out
            "extended-join",    // Get account and realname on JOIN
            "server-time",      // Timestamps from server (for bouncers)
            "batch",            // Grouped messages
            "chghost",          // Host change notifications
        ];

        let _ = incoming_tx.try_send(IrcMessage {
            tags: None,
            prefix: None,
            command: IrcCommand::Notice("*".to_string(), "Negotiating IRCv3 capabilities...".to_string()),
            raw: String::new(),
        });

        // Step 1: Request capability list
        let cap_ls = "CAP LS 302\r\n";
        tracing::debug!("> {}", cap_ls.trim());
        writer.write_all(cap_ls.as_bytes()).await?;
        writer.flush().await?;

        // Step 2: Read CAP LS response and collect available capabilities
        let mut buf = Vec::with_capacity(512);
        let mut available_caps = String::new();

        loop {
            if read_line_lossy(reader, &mut buf).await? == 0 {
                return Err("Connection closed during CAP negotiation".into());
            }
            let trimmed = bytes_to_string_lossy(&buf);
            tracing::debug!("< {}", trimmed);

            if let Some(msg) = IrcMessage::parse(&trimmed) {
                let _ = incoming_tx.try_send(msg.clone());

                if let IrcCommand::Cap(_target, subcmd, params) = &msg.command {
                    if subcmd == "LS" {
                        if let Some(caps) = params {
                            available_caps.push_str(caps);
                            available_caps.push(' ');
                        }
                        break;  // Got final LS response
                    } else if subcmd == "*" {
                        // Multi-line CAP LS - params contains "LS <caps>"
                        if let Some(rest) = params {
                            // Extract caps after "LS " if present
                            let caps_part = if rest.starts_with("LS ") {
                                &rest[3..]
                            } else {
                                rest.as_str()
                            };
                            available_caps.push_str(caps_part);
                            available_caps.push(' ');
                        }
                        // Don't break - wait for final LS
                    }
                }
            }
        }

        let available_lower = available_caps.to_lowercase();
        tracing::info!("Server capabilities: {}", available_caps.trim());

        // Step 3: Build list of capabilities to request
        let mut caps_to_request: Vec<&str> = Vec::new();

        for cap in DESIRED_CAPS {
            if available_lower.contains(*cap) {
                caps_to_request.push(cap);
            }
        }

        // Add SASL if available and we want it
        let sasl_available = available_lower.contains("sasl");
        if want_sasl && sasl_available {
            caps_to_request.push("sasl");
        }

        if caps_to_request.is_empty() {
            let _ = incoming_tx.try_send(IrcMessage {
                tags: None,
                prefix: None,
                command: IrcCommand::Notice("*".to_string(), "No IRCv3 capabilities available".to_string()),
                raw: String::new(),
            });
            let cap_end = "CAP END\r\n";
            tracing::debug!("> {}", cap_end.trim());
            writer.write_all(cap_end.as_bytes()).await?;
            writer.flush().await?;
            return Ok(());
        }

        // Step 4: Request capabilities
        let cap_req = format!("CAP REQ :{}\r\n", caps_to_request.join(" "));
        tracing::debug!("> {}", cap_req.trim());
        writer.write_all(cap_req.as_bytes()).await?;
        writer.flush().await?;

        // Step 5: Wait for CAP ACK/NAK
        let mut acked_caps = String::new();
        loop {
            if read_line_lossy(reader, &mut buf).await? == 0 {
                return Err("Connection closed during CAP negotiation".into());
            }
            let trimmed = bytes_to_string_lossy(&buf);
            tracing::debug!("< {}", trimmed);

            if let Some(msg) = IrcMessage::parse(&trimmed) {
                let _ = incoming_tx.try_send(msg.clone());

                if let IrcCommand::Cap(_target, subcmd, params) = &msg.command {
                    if subcmd == "ACK" {
                        if let Some(caps) = params {
                            acked_caps = caps.clone();
                        }
                        break;
                    } else if subcmd == "NAK" {
                        let _ = incoming_tx.try_send(IrcMessage {
                            tags: None,
                            prefix: None,
                            command: IrcCommand::Notice("*".to_string(), "Server rejected some capabilities".to_string()),
                            raw: String::new(),
                        });
                        break;
                    }
                }
            }
        }

        let acked_lower = acked_caps.to_lowercase();
        tracing::info!("Enabled capabilities: {}", acked_caps);

        // Step 6: If SASL was ACKed and we want it, do SASL authentication
        if want_sasl && acked_lower.contains("sasl") {
            self.do_sasl_auth_inner(writer, reader, incoming_tx).await?;
        } else if want_sasl && !sasl_available {
            let _ = incoming_tx.try_send(IrcMessage {
                tags: None,
                prefix: None,
                command: IrcCommand::Notice("*".to_string(), "Server does not support SASL".to_string()),
                raw: String::new(),
            });
        }

        // Report enabled capabilities
        let enabled: Vec<&str> = DESIRED_CAPS.iter()
            .filter(|cap| acked_lower.contains(*cap))
            .copied()
            .collect();
        if !enabled.is_empty() {
            let _ = incoming_tx.try_send(IrcMessage {
                tags: None,
                prefix: None,
                command: IrcCommand::Notice("*".to_string(), format!("IRCv3: {}", enabled.join(", "))),
                raw: String::new(),
            });
        }

        // Step 7: Send CAP END
        let cap_end = "CAP END\r\n";
        tracing::debug!("> {}", cap_end.trim());
        writer.write_all(cap_end.as_bytes()).await?;
        writer.flush().await?;

        Ok(())
    }

    /// Perform SASL PLAIN authentication (called after CAP REQ sasl is ACKed)
    async fn do_sasl_auth_inner<W, R>(
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
        let mut buf = Vec::with_capacity(512);

        let _ = incoming_tx.try_send(IrcMessage {
            tags: None,
            prefix: None,
            command: IrcCommand::Notice("*".to_string(), "Starting SASL authentication...".to_string()),
            raw: String::new(),
        });

        // Start SASL PLAIN
        let auth_plain = "AUTHENTICATE PLAIN\r\n";
        tracing::debug!("> {}", auth_plain.trim());
        writer.write_all(auth_plain.as_bytes()).await?;
        writer.flush().await?;

        // Wait for AUTHENTICATE +
        loop {
            if read_line_lossy(reader, &mut buf).await? == 0 {
                return Err("Connection closed during SASL".into());
            }
            let trimmed = bytes_to_string_lossy(&buf);
            tracing::debug!("< {}", trimmed);

            if let Some(msg) = IrcMessage::parse(&trimmed) {
                let _ = incoming_tx.try_send(msg.clone());

                if let IrcCommand::Authenticate(data) = &msg.command {
                    if data == "+" {
                        break;
                    }
                }
            }
        }

        // Send credentials (base64 of \0username\0password)
        let credentials = format!("\0{}\0{}", sasl_user, sasl_pass);
        let encoded = BASE64_STANDARD.encode(credentials.as_bytes());
        let auth_creds = format!("AUTHENTICATE {}\r\n", encoded);
        tracing::debug!("> AUTHENTICATE <credentials>");
        writer.write_all(auth_creds.as_bytes()).await?;
        writer.flush().await?;

        // Wait for 903 (success) or 904 (failure)
        loop {
            if read_line_lossy(reader, &mut buf).await? == 0 {
                return Err("Connection closed during SASL".into());
            }
            let trimmed = bytes_to_string_lossy(&buf);
            tracing::debug!("< {}", trimmed);

            if let Some(msg) = IrcMessage::parse(&trimmed) {
                let _ = incoming_tx.try_send(msg.clone());

                if let IrcCommand::Numeric(num, _params) = &msg.command {
                    match *num {
                        903 => {
                            let _ = incoming_tx.try_send(IrcMessage {
                                tags: None,
                                prefix: None,
                                command: IrcCommand::Notice("*".to_string(), "SASL authentication successful!".to_string()),
                                raw: String::new(),
                            });
                            return Ok(());
                        }
                        904 | 905 | 906 => {
                            let _ = incoming_tx.try_send(IrcMessage {
                                tags: None,
                                prefix: None,
                                command: IrcCommand::Notice("*".to_string(), "SASL authentication failed!".to_string()),
                                raw: String::new(),
                            });
                            return Ok(());
                        }
                        900 => continue,  // RPL_LOGGEDIN
                        _ => {}
                    }
                }
            }
        }
    }

    async fn read_loop<R: tokio::io::AsyncBufRead + Unpin>(
        &mut self,
        mut reader: R,
        incoming_tx: mpsc::Sender<IrcMessage>,
    ) {
        let mut buf = Vec::with_capacity(2048);
        loop {
            buf.clear();
            // Read bytes until newline to handle non-UTF8 encodings (common on older IRC networks)
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => {
                    tracing::info!("Connection closed by server (EOF)");
                    let _ = incoming_tx.try_send(IrcMessage {
                        tags: None,
                        prefix: None,
                        command: IrcCommand::Notice("*".to_string(), "Connection closed by server".to_string()),
                        raw: String::new(),
                    });
                    break;
                }
                Ok(n) => {
                    tracing::trace!("Read {} bytes", n);
                    // Convert to UTF-8 lossily (replaces invalid sequences with replacement char)
                    let line = String::from_utf8_lossy(&buf);
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        tracing::debug!("< {}", trimmed);
                        if let Some(msg) = IrcMessage::parse(trimmed) {
                            // Handle PING automatically
                            if let IrcCommand::Ping(server) = &msg.command {
                                let pong = format!("PONG :{}\r\n", server);
                                if let Some(ref tx) = self.tx {
                                    let _ = tx.try_send(pong);
                                }
                            }
                            // Use try_send to never block the read loop
                            if incoming_tx.try_send(msg).is_err() {
                                tracing::warn!("Message buffer full, dropping message");
                            }
                        }
                    }
                }
                Err(e) => {
                    let err_msg = format!("{}", e);
                    tracing::error!("Read error: {}", err_msg);
                    let _ = incoming_tx.try_send(IrcMessage {
                        tags: None,
                        prefix: None,
                        command: IrcCommand::Notice("*".to_string(), format!("[v3] Read error: {}", err_msg)),
                        raw: String::new(),
                    });
                    break;
                }
            }
        }
        tracing::info!("Read loop exited");
    }
}
