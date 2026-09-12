use base64::prelude::*;
use hmac::{Hmac, KeyInit, Mac};
use pbkdf2::pbkdf2_hmac;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

use super::message::{IrcCommand, IrcMessage};
use super::numerics::*;

/// Maximum bytes we will buffer for a single line from the server. Classic IRC
/// lines are 512 bytes; IRCv3 message tags and long CAP LS lists raise this, so
/// we allow generous headroom. A server that never sends a newline cannot grow
/// our read buffer past this bound.
const MAX_LINE_LEN: usize = 16 * 1024;
const HANDSHAKE_STEP_TIMEOUT: Duration = Duration::from_secs(15);
const STS_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Aggregate CAP responses are bounded separately from individual IRC lines.
/// This permits large, legitimate capability lists without allowing a server
/// to grow negotiation state until the registration deadline expires.
const MAX_CAP_BYTES: usize = 256 * 1024;
const MAX_CAP_TOKENS: usize = 4096;
const MAX_CAP_LINES: usize = 64;
const IRC_WIRE_LINE_BYTES: usize = 512;

fn cap_request_lines(caps: &[&str]) -> Vec<String> {
    const PREFIX: &str = "CAP REQ :";
    const SUFFIX: &str = "\r\n";
    let mut lines = Vec::new();
    let mut current = String::new();
    for cap in caps {
        let separator = usize::from(!current.is_empty());
        if PREFIX.len() + current.len() + separator + cap.len() + SUFFIX.len() > IRC_WIRE_LINE_BYTES
            && !current.is_empty()
        {
            lines.push(format!("{PREFIX}{current}{SUFFIX}"));
            current.clear();
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(cap);
    }
    if !current.is_empty() {
        lines.push(format!("{PREFIX}{current}{SUFFIX}"));
    }
    lines
}

fn cap_token_name(token: &str) -> String {
    let token = token.trim_start_matches(['-', '~', '=']);
    token
        .split_once('=')
        .map_or(token, |(name, _)| name)
        .to_ascii_lowercase()
}

/// Overall deadline for the whole registration handshake (CAP + SASL + NICK/USER).
/// The per-step timeout resets on every received line, so without this a server
/// that drips unrelated lines could keep the handshake running forever.
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the read loop tolerates total silence before probing with a PING.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(240);
/// How long to wait for any data after the keepalive probe before declaring
/// the connection dead.
const PONG_GRACE: Duration = Duration::from_secs(30);

/// Read bytes up to and including the next `\n`, appending to `buf` and
/// enforcing an upper bound so a newline-less flood cannot exhaust memory.
/// Returns the total bytes buffered (0 only at EOF with nothing pending). If
/// `max` is reached before a newline, returns `InvalidData` so the caller
/// drops the connection.
///
/// `buf` is NOT cleared here: the caller clears it after consuming a complete
/// line. That way a caller whose read future is cancelled (e.g. by a timeout)
/// resumes accumulating the partially-read line on the next call instead of
/// silently losing the consumed bytes and misparsing the line's tail.
async fn read_until_lf_bounded<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<usize> {
    loop {
        if buf.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "IRC line exceeded maximum length",
            ));
        }
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(buf.len()); // EOF (buf may hold a partial, newline-less line)
        }
        if let Some(i) = available.iter().position(|&b| b == b'\n') {
            let consumed = i + 1;
            let new_len = buf.len().checked_add(consumed).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "IRC line exceeded maximum length",
                )
            })?;
            if new_len > max {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "IRC line exceeded maximum length",
                ));
            }
            buf.extend_from_slice(&available[..=i]);
            reader.consume(consumed);
            return Ok(buf.len());
        }
        let n = available.len();
        let new_len = buf.len().checked_add(n).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "IRC line exceeded maximum length",
            )
        })?;
        if new_len > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "IRC line exceeded maximum length",
            ));
        }
        buf.extend_from_slice(available);
        reader.consume(n);
    }
}

/// Read a line from the reader, handling non-UTF8 data gracefully
async fn read_line_lossy<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    read_until_lf_bounded(reader, buf, MAX_LINE_LEN).await
}

/// Convert bytes to string with lossy UTF-8 conversion
fn bytes_to_string_lossy(buf: &[u8]) -> String {
    let line = String::from_utf8_lossy(buf);
    if let Some(without_lf) = line.strip_suffix('\n') {
        without_lf
            .strip_suffix('\r')
            .unwrap_or(without_lf)
            .to_string()
    } else {
        line.into_owned()
    }
}

#[derive(Default)]
struct CapBudget {
    bytes: usize,
    tokens: usize,
    lines: usize,
}

impl CapBudget {
    fn observe(&mut self, payload: &str) -> Result<(), &'static str> {
        self.bytes = self
            .bytes
            .checked_add(payload.len())
            .ok_or("CAP negotiation exceeded aggregate limits")?;
        self.tokens = self
            .tokens
            .checked_add(payload.split_whitespace().count())
            .ok_or("CAP negotiation exceeded aggregate limits")?;
        self.lines = self
            .lines
            .checked_add(1)
            .ok_or("CAP negotiation exceeded aggregate limits")?;

        if self.bytes > MAX_CAP_BYTES || self.tokens > MAX_CAP_TOKENS || self.lines > MAX_CAP_LINES
        {
            return Err("CAP negotiation exceeded aggregate limits");
        }
        Ok(())
    }
}

fn format_server_addr(host: &str, port: u16) -> String {
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Watch the command channel while a connection phase is otherwise blocked.
/// A queued QUIT or a closed channel is cancellation; other commands are kept
/// in order and delivered once registration succeeds.
async fn wait_for_disconnect(
    outgoing_rx: &mut mpsc::UnboundedReceiver<IrcCommand>,
    pending: &mut VecDeque<IrcCommand>,
) {
    loop {
        match outgoing_rx.recv().await {
            Some(IrcCommand::Quit(_)) | None => return,
            Some(command) => pending.push_back(command),
        }
    }
}

/// Redact credentials from an outgoing line before it reaches the log output.
/// PASS carries the server/bouncer password, AUTHENTICATE carries the base64
/// SASL payload, and NickServ IDENTIFY/GHOST/REGAIN/RECOVER lines (from
/// auto-perform or /ns) carry the account password.
fn redact_for_log(line: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = line.trim_end();
    let upper = trimmed.to_uppercase();
    if upper.starts_with("PASS ") {
        return "PASS <redacted>".into();
    }
    if upper.starts_with("AUTH ") || upper.starts_with("LOGIN ") || upper.starts_with("OPER ") {
        return format!(
            "{} <redacted>",
            trimmed.split_whitespace().next().unwrap_or("AUTH")
        )
        .into();
    }
    if let Some(arg) = trimmed.strip_prefix("AUTHENTICATE ")
        && arg != "+"
        && !arg.eq_ignore_ascii_case("PLAIN")
    {
        return "AUTHENTICATE <redacted>".into();
    }
    let service_auth = upper.starts_with("PRIVMSG NICKSERV")
        || upper.starts_with("NICKSERV")
        || upper.starts_with("PRIVMSG Q@")
        || upper.starts_with("PRIVMSG X@");
    if service_auth
        && ["IDENTIFY", "AUTH", "LOGIN", "GHOST", "REGAIN", "RECOVER"]
            .iter()
            .any(|w| upper.contains(w))
    {
        return "PRIVMSG NickServ :<redacted>".into();
    }
    trimmed.into()
}

/// Read the next protocol message during the registration handshake: parse it,
/// forward a copy to the GUI, and transparently answer server PINGs (which can
/// arrive mid-handshake, before the writer task that normally handles them
/// exists). Loops until it has a non-PING message to return, or errors if the
/// connection closes.
/// Read and handle one handshake line. Answers a server PING inline (and
/// forwards it), returning `None` so the caller re-arms its fresh per-step
/// budget; returns `Some(msg)` for anything else.
async fn read_handshake_msg<W, R>(
    writer: &mut W,
    reader: &mut R,
    buf: &mut Vec<u8>,
    incoming_tx: &mpsc::Sender<IrcMessage>,
) -> Result<Option<IrcMessage>, Box<dyn std::error::Error + Send + Sync>>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncBufRead + Unpin,
{
    // The GUI dropping its receiver is a disconnect request; honor it even
    // though the writer task (which normally carries QUIT) doesn't exist yet.
    if incoming_tx.is_closed() {
        return Err("Connection cancelled during registration".into());
    }
    let n = tokio::select! {
        _ = incoming_tx.closed() => {
            return Err("Connection cancelled during registration".into());
        }
        result = read_line_lossy(reader, buf) => result?,
    };
    if n == 0 || !buf.ends_with(b"\n") {
        // EOF, possibly mid-line: never parse a truncated fragment as a
        // complete message (mirrors the main read loop).
        return Err("Connection closed during registration".into());
    }
    let trimmed = bytes_to_string_lossy(buf);
    buf.clear();
    tracing::debug!("< {}", trimmed);
    match IrcMessage::parse(&trimmed) {
        Some(msg) => {
            if let IrcCommand::Ping(token) = &msg.command {
                let pong = format!("PONG :{}\r\n", token);
                writer.write_all(pong.as_bytes()).await?;
                writer.flush().await?;
            }
            incoming_tx
                .send(msg.clone())
                .await
                .map_err(|_| "Connection cancelled during registration")?;
            if matches!(&msg.command, IrcCommand::Ping(_)) {
                Ok(None)
            } else {
                Ok(Some(msg))
            }
        }
        None => Ok(None),
    }
}

async fn read_handshake_msg_timed<W, R>(
    writer: &mut W,
    reader: &mut R,
    buf: &mut Vec<u8>,
    incoming_tx: &mpsc::Sender<IrcMessage>,
) -> Result<Option<IrcMessage>, Box<dyn std::error::Error + Send + Sync>>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncBufRead + Unpin,
{
    // The per-step budget covers a single line, not the whole wait for the
    // message we actually care about: a server that PINGs during registration
    // (ZNC-style bouncers do) must not consume the budget for its delayed CAP
    // or SASL reply. The overall registration deadline still bounds the total.
    loop {
        match timeout(
            HANDSHAKE_STEP_TIMEOUT,
            read_handshake_msg(writer, reader, buf, incoming_tx),
        )
        .await
        {
            Ok(Ok(Some(msg))) => return Ok(Some(msg)),
            // PING answered (or junk line skipped): re-arm a fresh budget.
            Ok(Ok(None)) => continue,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Ok(None),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SaslMechanism {
    #[default]
    Auto,
    Plain,
    External,
    ScramSha256,
}

impl SaslMechanism {
    fn wire_name(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Plain => Some("PLAIN"),
            Self::External => Some("EXTERNAL"),
            Self::ScramSha256 => Some("SCRAM-SHA-256"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaslAttemptOutcome {
    Success,
    Failed,
    Welcome,
}

enum SaslServerEvent {
    Challenge(String),
    Success,
    Failed,
    Welcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StsPolicy {
    port: u16,
    duration_secs: u64,
    expires_at: u64,
}

fn sts_store_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|path| path.join("linefeed").join("sts_policies.json"))
}

fn load_sts_policies() -> HashMap<String, StsPolicy> {
    let Some(path) = sts_store_path() else {
        return HashMap::new();
    };
    std::fs::read(path)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn save_sts_policies(policies: &HashMap<String, StsPolicy>) -> std::io::Result<()> {
    let Some(path) = sts_store_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(policies)?)?;
    std::fs::rename(temporary, path)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_sts_value(value: &str) -> (Option<u16>, Option<u64>) {
    let mut port = None;
    let mut duration = None;
    for token in value.split(',') {
        if let Some(value) = token.strip_prefix("port=") {
            port = value.parse::<u16>().ok().filter(|port| *port > 0);
        } else if let Some(value) = token.strip_prefix("duration=") {
            duration = value.parse().ok();
        }
    }
    (port, duration)
}

fn update_sts_policy(host: &str, port: u16, duration: u64) {
    let mut policies = load_sts_policies();
    let key = host.to_ascii_lowercase();
    if duration == 0 {
        policies.remove(&key);
    } else {
        policies.insert(
            key,
            StsPolicy {
                port,
                duration_secs: duration,
                expires_at: unix_now().saturating_add(duration),
            },
        );
    }
    if let Err(error) = save_sts_policies(&policies) {
        tracing::warn!("Failed to save STS policy: {error}");
    }
}

fn active_sts_policy(host: &str) -> Option<StsPolicy> {
    let mut policies = load_sts_policies();
    let key = host.to_ascii_lowercase();
    let policy = policies
        .get(&key)
        .filter(|policy| policy.expires_at > unix_now())
        .cloned();
    if policy.is_none() && policies.remove(&key).is_some() {
        let _ = save_sts_policies(&policies);
    }
    policy
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

fn scram_sha256_response(
    password: &str,
    client_nonce: &str,
    client_first_bare: &str,
    server_first: &str,
) -> Result<(String, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
    let mut nonce = None;
    let mut salt = None;
    let mut iterations = None;
    for attribute in server_first.split(',') {
        if let Some(value) = attribute.strip_prefix("r=") {
            if nonce.replace(value).is_some() {
                return Err("Duplicate SCRAM nonce".into());
            }
        } else if let Some(value) = attribute.strip_prefix("s=") {
            if salt.replace(value).is_some() {
                return Err("Duplicate SCRAM salt".into());
            }
        } else if let Some(value) = attribute.strip_prefix("i=") {
            if iterations.replace(value).is_some() {
                return Err("Duplicate SCRAM iteration count".into());
            }
        } else if attribute.starts_with("m=") {
            return Err("Unsupported mandatory SCRAM extension".into());
        }
    }
    let nonce = nonce.ok_or("SCRAM challenge omitted nonce")?;
    if nonce.len() <= client_nonce.len() || !nonce.starts_with(client_nonce) {
        return Err("SCRAM server nonce does not extend client nonce".into());
    }
    let salt = BASE64_STANDARD
        .decode(salt.ok_or("SCRAM challenge omitted salt")?.as_bytes())
        .map_err(|_| "Invalid SCRAM salt")?;
    let iterations: u32 = iterations
        .ok_or("SCRAM challenge omitted iteration count")?
        .parse()
        .map_err(|_| "Invalid SCRAM iteration count")?;
    if !(4096..=1_000_000).contains(&iterations) {
        return Err("Unsafe SCRAM iteration count".into());
    }

    let client_final_without_proof = format!("c=biws,r={nonce}");
    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");
    let mut salted_password = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, iterations, &mut salted_password);
    let client_key = hmac_sha256(&salted_password, b"Client Key");
    let stored_key = Sha256::digest(&client_key);
    let client_signature = hmac_sha256(&stored_key, auth_message.as_bytes());
    let proof: Vec<u8> = client_key
        .iter()
        .zip(client_signature)
        .map(|(key, signature)| key ^ signature)
        .collect();
    let server_key = hmac_sha256(&salted_password, b"Server Key");
    let server_signature = hmac_sha256(&server_key, auth_message.as_bytes());
    Ok((
        format!(
            "{client_final_without_proof},p={}",
            BASE64_STANDARD.encode(proof)
        ),
        server_signature,
    ))
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
    pub sasl_mechanism: SaslMechanism,
    pub sasl_required: bool,
    pub tls_client_cert_path: Option<String>,
    pub tls_client_key_path: Option<String>,
    pub pre_away_message: Option<String>,
    pub persistence_profile: Option<String>,
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
            sasl_mechanism: SaslMechanism::Auto,
            sasl_required: false,
            tls_client_cert_path: None,
            tls_client_key_path: None,
            pre_away_message: None,
            persistence_profile: None,
        }
    }
}

pub struct IrcClient {
    config: ServerConfig,
    tx: Option<mpsc::Sender<String>>,
}

/// Capabilities whose semantics Linefeed implements safely. Keep this list
/// shared with post-registration CAP NEW handling in the GUI.
pub const DESIRED_CAPS: &[&str] = &[
    "multi-prefix",
    "userhost-in-names",
    "away-notify",
    "account-notify",
    "extended-join",
    "server-time",
    "echo-message",
    "batch",
    "message-tags",
    "account-tag",
    "standard-replies",
    "cap-notify",
    "labeled-response",
    "no-implicit-names",
    "draft/chathistory",
    "draft/multiline",
    "draft/event-playback",
    "draft/read-marker",
    "draft/message-redaction",
    "draft/channel-rename",
    "evilnet/channel-relocate",
    "draft/extended-isupport",
    "draft/pre-away",
    "draft/bouncer",
    "draft/persistence",
    "draft/authtoken",
    "draft/metadata-2",
    "draft/webpush",
    "draft/account-registration",
    "setname",
    "invite-notify",
    "draft/oper-tag",
    "chghost",
    "extended-monitor",
];

impl IrcClient {
    pub fn new(config: ServerConfig) -> Self {
        Self { config, tx: None }
    }

    pub async fn connect(
        &mut self,
        incoming_tx: mpsc::Sender<IrcMessage>,
        mut outgoing_rx: mpsc::UnboundedReceiver<IrcCommand>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut pending_commands = VecDeque::new();
        // The loopback protocol tests use one-shot listeners and do not model
        // the preliminary STS connection. Keep policy I/O out of those tests;
        // STS parsing and probing have focused unit coverage below.
        if !cfg!(test) {
            if let Some(policy) = active_sts_policy(&self.config.host) {
                self.config.use_tls = true;
                self.config.accept_invalid_certs = false;
                self.config.port = policy.port;
            } else if !self.config.use_tls
                && let Some(port) = Self::probe_sts_upgrade(
                    &self.config.host,
                    self.config.port,
                    &incoming_tx,
                    &mut outgoing_rx,
                    &mut pending_commands,
                )
                .await?
            {
                let _ = incoming_tx.try_send(IrcMessage {
                    tags: None,
                    prefix: None,
                    command: IrcCommand::Notice(
                        "*".to_string(),
                        format!("Server requested STS; reconnecting securely on port {port}"),
                    ),
                    raw: String::new(),
                });
                self.config.use_tls = true;
                self.config.accept_invalid_certs = false;
                self.config.port = port;
            }
        }
        if self
            .config
            .pre_away_message
            .as_ref()
            .is_some_and(|message| {
                message.len() > 300
                    || message
                        .bytes()
                        .any(|byte| matches!(byte, b'\r' | b'\n' | 0))
            })
        {
            return Err("Pre-away message must be one IRC line of at most 300 bytes".into());
        }
        if self
            .config
            .persistence_profile
            .as_ref()
            .is_some_and(|profile| {
                profile.is_empty()
                    || profile.len() > 32
                    || !profile
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            })
        {
            return Err("Persistence profile must be 1-32 letters, digits, '_' or '-'".into());
        }
        let addr = format_server_addr(&self.config.host, self.config.port);
        tracing::info!("Connecting to {} (TLS: {})", addr, self.config.use_tls);

        // Send connecting message (non-blocking)
        let _ = incoming_tx.try_send(IrcMessage {
            tags: None,
            prefix: None,
            command: IrcCommand::Notice("*".to_string(), format!("Connecting to {}...", addr)),
            raw: String::new(),
        });

        if incoming_tx.is_closed() {
            return Err("Connection cancelled before TCP connect".into());
        }
        let connect_result = tokio::select! {
            biased;
            _ = incoming_tx.closed() => {
                return Err("Connection cancelled during TCP connect".into());
            }
            _ = wait_for_disconnect(&mut outgoing_rx, &mut pending_commands) => {
                return Err("Connection cancelled during TCP connect".into());
            }
            result = TcpStream::connect((self.config.host.as_str(), self.config.port)) => result,
        };
        let stream = match connect_result {
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
                command: IrcCommand::Notice(
                    "*".to_string(),
                    "Starting TLS handshake...".to_string(),
                ),
                raw: String::new(),
            });
            self.handle_tls_connection(stream, incoming_tx, outgoing_rx, pending_commands)
                .await
        } else {
            self.handle_plain_connection(stream, incoming_tx, outgoing_rx, pending_commands)
                .await
        }
    }

    async fn probe_sts_upgrade(
        host: &str,
        port: u16,
        incoming_tx: &mpsc::Sender<IrcMessage>,
        outgoing_rx: &mut mpsc::UnboundedReceiver<IrcCommand>,
        pending_commands: &mut VecDeque<IrcCommand>,
    ) -> Result<Option<u16>, Box<dyn std::error::Error + Send + Sync>> {
        let stream = tokio::select! {
            biased;
            _ = incoming_tx.closed() => return Err("Connection cancelled during STS probe".into()),
            _ = wait_for_disconnect(outgoing_rx, pending_commands) => {
                return Err("Connection cancelled during STS probe".into());
            }
            result = timeout(STS_PROBE_TIMEOUT, TcpStream::connect((host, port))) => {
                match result {
                    Ok(Ok(stream)) => stream,
                    _ => return Ok(None),
                }
            }
        };
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);
        writer.write_all(b"CAP LS 302\r\n").await?;
        writer.flush().await?;
        let mut buf = Vec::with_capacity(512);
        for _ in 0..MAX_CAP_LINES {
            buf.clear();
            let read = tokio::select! {
                biased;
                _ = incoming_tx.closed() => {
                    return Err("Connection cancelled during STS probe".into());
                }
                _ = wait_for_disconnect(outgoing_rx, pending_commands) => {
                    return Err("Connection cancelled during STS probe".into());
                }
                result = timeout(
                    STS_PROBE_TIMEOUT,
                    read_until_lf_bounded(&mut reader, &mut buf, MAX_LINE_LEN),
                ) => result,
            };
            let Ok(Ok(count)) = read else {
                return Ok(None);
            };
            if count == 0 {
                return Ok(None);
            }
            let line = String::from_utf8_lossy(&buf);
            let Some(message) = IrcMessage::parse(&line) else {
                continue;
            };
            if let IrcCommand::Ping(token) = message.command {
                writer
                    .write_all(format!("PONG :{token}\r\n").as_bytes())
                    .await?;
                writer.flush().await?;
                continue;
            }
            if let IrcCommand::Cap(_, subcommand, params) = message.command
                && subcommand.eq_ignore_ascii_case("LS")
            {
                if let Some(port) = params
                    .last()
                    .into_iter()
                    .flat_map(|caps| caps.split_whitespace())
                    .find_map(|cap| {
                        cap.strip_prefix("sts=")
                            .and_then(|value| parse_sts_value(value).0)
                    })
                {
                    return Ok(Some(port));
                }
                if params.first().is_none_or(|value| value != "*") {
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }

    async fn handle_tls_connection(
        &mut self,
        stream: TcpStream,
        incoming_tx: mpsc::Sender<IrcMessage>,
        mut outgoing_rx: mpsc::UnboundedReceiver<IrcCommand>,
        mut pending_commands: VecDeque<IrcCommand>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Set up native TLS
        let mut tls_builder = native_tls::TlsConnector::builder();

        if self.config.accept_invalid_certs {
            tracing::warn!("Accepting invalid certificates (insecure!)");
            tls_builder.danger_accept_invalid_certs(true);
            tls_builder.danger_accept_invalid_hostnames(true);
        }

        if let Some(cert_path) = self.config.tls_client_cert_path.as_deref() {
            let cert = std::fs::read(cert_path)
                .map_err(|e| format!("Failed to read TLS client certificate {cert_path:?}: {e}"))?;
            let key_path = self
                .config
                .tls_client_key_path
                .as_deref()
                .unwrap_or(cert_path);
            let key = std::fs::read(key_path)
                .map_err(|e| format!("Failed to read TLS client key {key_path:?}: {e}"))?;
            let identity = native_tls::Identity::from_pkcs8(&cert, &key)
                .map_err(|e| format!("Invalid TLS client certificate/key: {e}"))?;
            tls_builder.identity(identity);
        }

        let tls_connector = tls_builder
            .build()
            .map_err(|e| format!("Failed to create TLS connector: {}", e))?;

        let connector = tokio_native_tls::TlsConnector::from(tls_connector);

        tracing::info!("Starting TLS handshake with {}", self.config.host);

        let tls_result = tokio::select! {
            biased;
            _ = incoming_tx.closed() => {
                return Err("Connection cancelled during TLS handshake".into());
            }
            _ = wait_for_disconnect(&mut outgoing_rx, &mut pending_commands) => {
                return Err("Connection cancelled during TLS handshake".into());
            }
            result = connector.connect(&self.config.host, stream) => result,
        };
        let tls_stream = match tls_result {
            Ok(s) => s,
            Err(e) => {
                let msg = format!(
                    "TLS handshake failed: {}. Try disabling TLS or enabling 'Accept invalid certs'",
                    e
                );
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

        self.run_connection(tls_stream, incoming_tx, outgoing_rx, pending_commands)
            .await
    }

    async fn handle_plain_connection(
        &mut self,
        stream: TcpStream,
        incoming_tx: mpsc::Sender<IrcMessage>,
        outgoing_rx: mpsc::UnboundedReceiver<IrcCommand>,
        pending_commands: VecDeque<IrcCommand>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.run_connection(stream, incoming_tx, outgoing_rx, pending_commands)
            .await
    }

    /// Common connection handler for both TLS and plain connections
    async fn run_connection<S>(
        &mut self,
        stream: S,
        incoming_tx: mpsc::Sender<IrcMessage>,
        mut outgoing_rx: mpsc::UnboundedReceiver<IrcCommand>,
        mut pending_commands: VecDeque<IrcCommand>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);

        // The deadline covers the complete registration state machine through
        // RPL_WELCOME, not merely writing NICK/USER. Dropping the GUI receiver
        // cancels blocked writes and reads immediately as well.
        tokio::select! {
            biased;
            _ = wait_for_disconnect(&mut outgoing_rx, &mut pending_commands) => {
                return Err("Connection cancelled during registration".into());
            }
            result = self.register_with_deadline(
                &mut writer,
                &mut reader,
                &incoming_tx,
                REGISTRATION_TIMEOUT,
            ) => result?,
        }

        // Create channel for sending
        let (send_tx, mut send_rx) = mpsc::channel::<String>(100);
        self.tx = Some(send_tx);

        // Spawn writer task
        let mut writer_handle = tokio::spawn(async move {
            use tokio::time::{Duration, timeout};
            while let Some(cmd) = pending_commands.pop_front() {
                let should_close = matches!(cmd, IrcCommand::Quit(_));
                let line = format!("{}\r\n", cmd);
                tracing::debug!("> {}", redact_for_log(&line));
                if let Err(e) = writer.write_all(line.as_bytes()).await {
                    tracing::error!("Write error: {}", e);
                    return;
                }
                if should_close {
                    let _ = writer.flush().await;
                    return;
                }
            }
            if let Err(e) = writer.flush().await {
                tracing::error!("Write error: {}", e);
                return;
            }
            loop {
                tokio::select! {
                    Some(cmd) = outgoing_rx.recv() => {
                        let should_close = matches!(cmd, IrcCommand::Quit(_));
                        let line = format!("{}\r\n", cmd);
                        tracing::debug!("> {}", redact_for_log(&line));
                        if let Err(e) = writer.write_all(line.as_bytes()).await {
                            tracing::error!("Write error: {}", e);
                            break;
                        }
                        if should_close {
                            let _ = writer.flush().await;
                            break;
                        }
                        // Drain any queued commands before flushing (batch writes)
                        while let Ok(Some(cmd)) = timeout(Duration::from_micros(100), outgoing_rx.recv()).await {
                            let should_close = matches!(cmd, IrcCommand::Quit(_));
                            let line = format!("{}\r\n", cmd);
                            tracing::debug!("> {}", redact_for_log(&line));
                            if let Err(e) = writer.write_all(line.as_bytes()).await {
                                // Exit the whole task, matching the outer loop: a
                                // plain `break` would only leave this drain loop and
                                // keep consuming (and dropping) queued commands.
                                tracing::error!("Write error: {}", e);
                                return;
                            }
                            if should_close {
                                let _ = writer.flush().await;
                                return;
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

        // Whichever side finishes first closes the whole connection. In
        // particular, a locally-sent QUIT must not leave the reader blocked on
        // a server that keeps the socket open indefinitely.
        tokio::select! {
            _ = self.read_loop(reader, incoming_tx) => {
                writer_handle.abort();
                let _ = writer_handle.await;
            }
            result = &mut writer_handle => {
                if let Err(e) = result
                    && !e.is_cancelled()
                {
                    tracing::warn!("Writer task failed: {}", e);
                }
            }
        }
        self.tx = None;
        Ok(())
    }

    async fn register_with_deadline<W, R>(
        &self,
        writer: &mut W,
        reader: &mut R,
        incoming_tx: &mpsc::Sender<IrcMessage>,
        deadline: Duration,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        W: tokio::io::AsyncWrite + Unpin,
        R: tokio::io::AsyncBufRead + Unpin,
    {
        tokio::select! {
            _ = incoming_tx.closed() => {
                Err("Connection cancelled during registration".into())
            }
            result = timeout(
                deadline,
                self.send_registration(writer, reader, incoming_tx),
            ) => match result {
                Ok(result) => result,
                Err(_) => Err(
                    "Registration timed out; server never sent a welcome reply".into(),
                ),
            },
        }
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

        let _ = incoming_tx.try_send(IrcMessage {
            tags: None,
            prefix: None,
            command: IrcCommand::Notice(
                "*".to_string(),
                "Negotiating IRCv3 capabilities...".to_string(),
            ),
            raw: String::new(),
        });

        // CAP must be the first command, but it must not delay the mandatory
        // registration commands: CAP suspends completion until CAP END while
        // legacy servers can proceed as soon as they see NICK and USER.
        let cap_ls = "CAP LS 302\r\n";
        tracing::debug!("> {}", cap_ls.trim_end());
        writer.write_all(cap_ls.as_bytes()).await?;

        // Send PASS if configured (never log the password itself)
        if let Some(ref pass) = config.password {
            let pass_cmd = IrcCommand::Pass(pass.clone());
            let line = format!("{}\r\n", pass_cmd);
            tracing::debug!("> PASS <redacted>");
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
            command: IrcCommand::Notice(
                "*".to_string(),
                "Registration sent, waiting for response...".to_string(),
            ),
            raw: String::new(),
        });

        let welcome_received = self.do_cap_negotiation(writer, reader, incoming_tx).await?;
        if !welcome_received {
            self.wait_for_welcome(writer, reader, incoming_tx).await?;
        }

        Ok(())
    }

    async fn wait_for_welcome<W, R>(
        &self,
        writer: &mut W,
        reader: &mut R,
        incoming_tx: &mpsc::Sender<IrcMessage>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        W: tokio::io::AsyncWrite + Unpin,
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let mut buf = Vec::with_capacity(512);
        loop {
            let msg = read_handshake_msg(writer, reader, &mut buf, incoming_tx)
                .await?
                .expect("non-PING handshake message");
            if matches!(msg.command, IrcCommand::Numeric(RPL_WELCOME, _)) {
                return Ok(());
            }
        }
    }

    /// Negotiate IRCv3 capabilities including SASL authentication
    async fn do_cap_negotiation<W, R>(
        &self,
        writer: &mut W,
        reader: &mut R,
        incoming_tx: &mpsc::Sender<IrcMessage>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>
    where
        W: tokio::io::AsyncWrite + Unpin,
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let config = &self.config;
        let has_password = config.sasl_username.is_some() && config.sasl_password.is_some();
        let has_external = config.tls_client_cert_path.is_some();
        let want_sasl = match config.sasl_mechanism {
            SaslMechanism::Auto => has_password || has_external,
            SaslMechanism::Plain | SaslMechanism::ScramSha256 => has_password,
            SaslMechanism::External => has_external,
        };

        // CAP LS and NICK/USER have already been sent together. Read the CAP
        // response while registration completion remains suspended.
        let mut buf = Vec::with_capacity(512);
        let mut available_caps = String::new();
        let mut cap_budget = CapBudget::default();

        loop {
            let Some(msg) = read_handshake_msg_timed(writer, reader, &mut buf, incoming_tx).await?
            else {
                if config.sasl_required {
                    return Err("Required SASL authentication could not be negotiated".into());
                }
                let _ = incoming_tx.try_send(IrcMessage {
                    tags: None,
                    prefix: None,
                    command: IrcCommand::Notice(
                        "*".to_string(),
                        "CAP negotiation timed out; continuing without IRCv3 capabilities"
                            .to_string(),
                    ),
                    raw: String::new(),
                });
                let cap_end = "CAP END\r\n";
                tracing::debug!("> {}", cap_end.trim());
                writer.write_all(cap_end.as_bytes()).await?;
                writer.flush().await?;
                return Ok(false);
            };

            // A legacy server may ignore CAP entirely and register immediately.
            // The welcome was already forwarded to the GUI, so do not consume it
            // and then wait forever for a second copy.
            if matches!(&msg.command, IrcCommand::Numeric(RPL_WELCOME, _)) {
                if config.sasl_required {
                    return Err("Server completed registration without required SASL".into());
                }
                return Ok(true);
            }

            if let IrcCommand::Cap(_target, subcmd, rest) = &msg.command {
                if subcmd == "LS" {
                    // Multiline form `CAP * LS * :caps` -> rest = ["*", caps];
                    // final form `CAP * LS :caps`       -> rest = [caps].
                    let (is_more, caps) = match rest.first().map(String::as_str) {
                        Some("*") => (true, rest.get(1)),
                        _ => (false, rest.first()),
                    };
                    if let Some(c) = caps {
                        cap_budget.observe(c)?;
                        available_caps.push_str(c);
                        available_caps.push(' ');
                    } else {
                        cap_budget.observe("")?;
                    }
                    if !is_more {
                        break; // got the final LS line
                    }
                }
            } else if let IrcCommand::Numeric(ERR_UNKNOWNCOMMAND, params) = &msg.command
                && params.iter().any(|p| p.eq_ignore_ascii_case("CAP"))
            {
                if config.sasl_required {
                    return Err("Server does not support CAP required for SASL".into());
                }
                let _ = incoming_tx.try_send(IrcMessage {
                    tags: None,
                    prefix: None,
                    command: IrcCommand::Notice(
                        "*".to_string(),
                        "Server does not support CAP; continuing without IRCv3 capabilities"
                            .to_string(),
                    ),
                    raw: String::new(),
                });
                return Ok(false);
            }
        }

        // Match capabilities by whitespace-delimited token (stripping any `=value`
        // suffix) so e.g. "sasl" does not spuriously match "no-sasl".
        let available_tokens: std::collections::HashMap<String, Option<String>> = available_caps
            .split_whitespace()
            .map(|t| {
                let (name, value) = t
                    .split_once('=')
                    .map(|(name, value)| (name, Some(value.to_string())))
                    .unwrap_or((t, None));
                (name.to_ascii_lowercase(), value)
            })
            .collect();
        tracing::info!("Server capabilities: {}", available_caps.trim());
        if config.use_tls
            && !config.accept_invalid_certs
            && let Some(Some(sts)) = available_tokens.get("sts")
            && let Some(duration) = parse_sts_value(sts).1
        {
            update_sts_policy(&config.host, config.port, duration);
        }

        // Step 3: Build list of capabilities to request
        let mut caps_to_request: Vec<&str> = Vec::new();

        for cap in DESIRED_CAPS {
            if available_tokens.contains_key(*cap) {
                caps_to_request.push(cap);
            }
        }

        // Add SASL if available and we want it
        let sasl_available = available_tokens.contains_key("sasl");
        let offered_sasl: Option<Vec<String>> = available_tokens
            .get("sasl")
            .and_then(|value| value.as_ref())
            .map(|value| {
                value
                    .split(',')
                    .map(|mech| mech.to_ascii_uppercase())
                    .collect()
            });
        let offered = |mechanism: SaslMechanism| {
            offered_sasl.as_ref().is_none_or(|offered| {
                mechanism
                    .wire_name()
                    .is_some_and(|name| offered.iter().any(|item| item == name))
            })
        };
        let mut sasl_mechanisms = Vec::new();
        match config.sasl_mechanism {
            SaslMechanism::Auto => {
                if has_external && offered(SaslMechanism::External) {
                    sasl_mechanisms.push(SaslMechanism::External);
                }
                // Do not probe SCRAM on servers that omit the mechanism list;
                // older services commonly support only PLAIN.
                if has_password && offered_sasl.is_some() && offered(SaslMechanism::ScramSha256) {
                    sasl_mechanisms.push(SaslMechanism::ScramSha256);
                }
                if has_password && offered(SaslMechanism::Plain) {
                    sasl_mechanisms.push(SaslMechanism::Plain);
                }
            }
            mechanism if offered(mechanism) => sasl_mechanisms.push(mechanism),
            _ => {}
        }
        if want_sasl && !sasl_mechanisms.is_empty() {
            caps_to_request.push("sasl");
        }

        if caps_to_request.is_empty() {
            if config.sasl_required {
                return Err("Server does not offer the required SASL mechanism".into());
            }
            let _ = incoming_tx.try_send(IrcMessage {
                tags: None,
                prefix: None,
                command: IrcCommand::Notice(
                    "*".to_string(),
                    "No IRCv3 capabilities available".to_string(),
                ),
                raw: String::new(),
            });
            let cap_end = "CAP END\r\n";
            tracing::debug!("> {}", cap_end.trim());
            writer.write_all(cap_end.as_bytes()).await?;
            writer.flush().await?;
            return Ok(false);
        }

        // Step 4: Request capabilities
        let cap_reqs = cap_request_lines(&caps_to_request);
        for cap_req in &cap_reqs {
            tracing::debug!("> {}", cap_req.trim());
            writer.write_all(cap_req.as_bytes()).await?;
        }
        writer.flush().await?;

        // Step 5: Wait for CAP ACK/NAK
        let mut acked_caps = String::new();
        let mut rejected_caps = Vec::new();
        let mut pending_caps: std::collections::HashSet<String> =
            caps_to_request.iter().map(|cap| cap.to_string()).collect();
        loop {
            let Some(msg) = read_handshake_msg_timed(writer, reader, &mut buf, incoming_tx).await?
            else {
                if config.sasl_required && pending_caps.contains("sasl") {
                    return Err("Required SASL capability was not acknowledged".into());
                }
                let _ = incoming_tx.try_send(IrcMessage {
                    tags: None,
                    prefix: None,
                    command: IrcCommand::Notice(
                        "*".to_string(),
                        "CAP ACK timed out; continuing registration".to_string(),
                    ),
                    raw: String::new(),
                });
                break;
            };

            if matches!(&msg.command, IrcCommand::Numeric(RPL_WELCOME, _)) {
                if config.sasl_required {
                    return Err("Server completed registration without required SASL".into());
                }
                let cap_end = "CAP END\r\n";
                tracing::debug!("> {}", cap_end.trim());
                writer.write_all(cap_end.as_bytes()).await?;
                writer.flush().await?;
                return Ok(true);
            }

            if let IrcCommand::Cap(_target, subcmd, rest) = &msg.command {
                if subcmd.eq_ignore_ascii_case("ACK") {
                    // The trailing element is the acked cap list (after any "*" marker).
                    if let Some(caps) = rest.last() {
                        cap_budget.observe(caps)?;
                        if !acked_caps.is_empty() {
                            acked_caps.push(' ');
                        }
                        acked_caps.push_str(caps);
                        for cap in caps.split_whitespace() {
                            pending_caps.remove(&cap_token_name(cap));
                        }
                    }
                    if pending_caps.is_empty() {
                        break;
                    }
                } else if subcmd.eq_ignore_ascii_case("NAK") {
                    // The trailing element is the rejected cap list (after any "*" marker).
                    if let Some(caps) = rest.last() {
                        cap_budget.observe(caps)?;
                        rejected_caps.push(caps.clone());
                        for cap in caps.split_whitespace() {
                            pending_caps.remove(&cap_token_name(cap));
                        }
                    }
                    if pending_caps.is_empty() {
                        break;
                    }
                }
            }
        }

        if !rejected_caps.is_empty() {
            let message = format!("Server rejected capabilities: {}", rejected_caps.join(" "));
            let _ = incoming_tx.try_send(IrcMessage {
                tags: None,
                prefix: None,
                command: IrcCommand::Notice("*".to_string(), message),
                raw: String::new(),
            });
        }

        let acked_tokens: std::collections::HashSet<String> = acked_caps
            .split_whitespace()
            .map(|t| t.split('=').next().unwrap_or(t).to_ascii_lowercase())
            .collect();
        tracing::info!("Enabled capabilities: {}", acked_caps);

        // Step 6: If SASL was ACKed and we want it, do SASL authentication
        let (welcome_received, sasl_authenticated) =
            if want_sasl && acked_tokens.contains("sasl") && !sasl_mechanisms.is_empty() {
                self.do_sasl_auth_inner(writer, reader, incoming_tx, &sasl_mechanisms)
                    .await?
            } else if want_sasl {
                let message = if !sasl_available {
                    "Server does not support SASL"
                } else {
                    "Server does not offer a configured SASL mechanism"
                };
                let _ = incoming_tx.try_send(IrcMessage {
                    tags: None,
                    prefix: None,
                    command: IrcCommand::Notice("*".to_string(), message.to_string()),
                    raw: String::new(),
                });
                (false, false)
            } else {
                (false, false)
            };

        if config.sasl_required && !sasl_authenticated {
            return Err("Required SASL authentication did not succeed".into());
        }

        // Registration-only extensions run after their CAP ACK. Persistence
        // profiles also need the SASL account established, and both commands
        // must precede CAP END.
        if acked_tokens.contains("draft/persistence")
            && let Some(profile) = config
                .persistence_profile
                .as_deref()
                .filter(|profile| !profile.is_empty())
        {
            let line = format!("PERSISTENCE ATTACH {}\r\n", profile);
            tracing::debug!("> PERSISTENCE ATTACH {}", profile);
            writer.write_all(line.as_bytes()).await?;
        }
        if acked_tokens.contains("draft/pre-away")
            && let Some(message) = config.pre_away_message.as_deref()
        {
            let line = if message.is_empty() {
                "AWAY\r\n".to_string()
            } else {
                format!("AWAY :{}\r\n", message)
            };
            tracing::debug!("> AWAY :<pre-connect status>");
            writer.write_all(line.as_bytes()).await?;
        }

        // Report enabled capabilities
        let enabled: Vec<&str> = DESIRED_CAPS
            .iter()
            .filter(|cap| acked_tokens.contains(**cap))
            .copied()
            .collect();
        if !enabled.is_empty() {
            let _ = incoming_tx.try_send(IrcMessage {
                tags: None,
                prefix: None,
                command: IrcCommand::Notice(
                    "*".to_string(),
                    format!("IRCv3: {}", enabled.join(", ")),
                ),
                raw: String::new(),
            });
        }

        // Step 7: Send CAP END
        let cap_end = "CAP END\r\n";
        tracing::debug!("> {}", cap_end.trim());
        writer.write_all(cap_end.as_bytes()).await?;
        writer.flush().await?;

        Ok(welcome_received)
    }

    /// Try configured SASL mechanisms in preference order. A failed
    /// mechanism may be followed by another attempt without ending CAP.
    async fn do_sasl_auth_inner<W, R>(
        &self,
        writer: &mut W,
        reader: &mut R,
        incoming_tx: &mpsc::Sender<IrcMessage>,
        mechanisms: &[SaslMechanism],
    ) -> Result<(bool, bool), Box<dyn std::error::Error + Send + Sync>>
    where
        W: tokio::io::AsyncWrite + Unpin,
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let mut buf = Vec::with_capacity(512);
        for mechanism in mechanisms {
            let name = mechanism.wire_name().expect("Auto is never attempted");
            let _ = incoming_tx.try_send(IrcMessage {
                tags: None,
                prefix: None,
                command: IrcCommand::Notice(
                    "*".to_string(),
                    format!("Starting SASL {name} authentication..."),
                ),
                raw: String::new(),
            });
            match self
                .do_sasl_attempt(writer, reader, incoming_tx, &mut buf, *mechanism)
                .await?
            {
                SaslAttemptOutcome::Success => return Ok((false, true)),
                SaslAttemptOutcome::Welcome => return Ok((true, false)),
                SaslAttemptOutcome::Failed => continue,
            }
        }
        let _ = incoming_tx.try_send(IrcMessage {
            tags: None,
            prefix: None,
            command: IrcCommand::Notice(
                "*".to_string(),
                "SASL authentication failed for every configured mechanism".to_string(),
            ),
            raw: String::new(),
        });
        Ok((false, false))
    }

    async fn do_sasl_attempt<W, R>(
        &self,
        writer: &mut W,
        reader: &mut R,
        incoming_tx: &mpsc::Sender<IrcMessage>,
        buf: &mut Vec<u8>,
        mechanism: SaslMechanism,
    ) -> Result<SaslAttemptOutcome, Box<dyn std::error::Error + Send + Sync>>
    where
        W: tokio::io::AsyncWrite + Unpin,
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let name = mechanism.wire_name().expect("Auto is never attempted");
        writer
            .write_all(format!("AUTHENTICATE {name}\r\n").as_bytes())
            .await?;
        writer.flush().await?;

        match self
            .read_sasl_event(writer, reader, incoming_tx, buf)
            .await?
        {
            SaslServerEvent::Challenge(challenge) if challenge == "+" => {}
            SaslServerEvent::Success => return Ok(SaslAttemptOutcome::Success),
            SaslServerEvent::Welcome => return Ok(SaslAttemptOutcome::Welcome),
            _ => return Ok(SaslAttemptOutcome::Failed),
        }

        let mut expected_server_signature: Option<Vec<u8>> = None;
        match mechanism {
            SaslMechanism::Plain => {
                let user = self.config.sasl_username.as_deref().unwrap_or_default();
                let password = self.config.sasl_password.as_deref().unwrap_or_default();
                let payload = format!("\0{user}\0{password}");
                Self::write_sasl_payload(writer, payload.as_bytes()).await?;
            }
            SaslMechanism::External => {
                // Empty authzid asks services to use the identity from the TLS
                // client certificate.
                Self::write_sasl_payload(writer, b"").await?;
            }
            SaslMechanism::ScramSha256 => {
                let user = self.config.sasl_username.as_deref().unwrap_or_default();
                let password = self.config.sasl_password.as_deref().unwrap_or_default();
                let mut random = [0u8; 18];
                getrandom::fill(&mut random).map_err(|e| format!("OS randomness failed: {e}"))?;
                let nonce = BASE64_STANDARD.encode(random).replace('=', "");
                let escaped_user = user.replace('=', "=3D").replace(',', "=2C");
                let client_first_bare = format!("n={escaped_user},r={nonce}");
                let client_first = format!("n,,{client_first_bare}");
                Self::write_sasl_payload(writer, client_first.as_bytes()).await?;

                let server_first = match self
                    .read_sasl_event(writer, reader, incoming_tx, buf)
                    .await?
                {
                    SaslServerEvent::Challenge(challenge) => String::from_utf8(
                        BASE64_STANDARD
                            .decode(challenge.as_bytes())
                            .map_err(|_| "Invalid base64 SCRAM challenge")?,
                    )
                    .map_err(|_| "Non-UTF-8 SCRAM challenge")?,
                    SaslServerEvent::Welcome => return Ok(SaslAttemptOutcome::Welcome),
                    _ => return Ok(SaslAttemptOutcome::Failed),
                };
                let (client_final, signature) =
                    scram_sha256_response(password, &nonce, &client_first_bare, &server_first)?;
                expected_server_signature = Some(signature);
                Self::write_sasl_payload(writer, client_final.as_bytes()).await?;
            }
            SaslMechanism::Auto => unreachable!(),
        }

        let mut server_verified = expected_server_signature.is_none();
        loop {
            match self
                .read_sasl_event(writer, reader, incoming_tx, buf)
                .await?
            {
                SaslServerEvent::Challenge(challenge) => {
                    let Some(expected) = expected_server_signature.as_deref() else {
                        return Ok(SaslAttemptOutcome::Failed);
                    };
                    let decoded = BASE64_STANDARD
                        .decode(challenge.as_bytes())
                        .ok()
                        .and_then(|bytes| String::from_utf8(bytes).ok());
                    let supplied = decoded
                        .as_deref()
                        .and_then(|value| value.split(',').find_map(|part| part.strip_prefix("v=")))
                        .and_then(|value| BASE64_STANDARD.decode(value.as_bytes()).ok());
                    server_verified = supplied
                        .as_deref()
                        .is_some_and(|actual| constant_time_eq(actual, expected));
                    if !server_verified {
                        writer.write_all(b"AUTHENTICATE *\r\n").await?;
                        writer.flush().await?;
                        return Ok(SaslAttemptOutcome::Failed);
                    }
                }
                SaslServerEvent::Success if server_verified => {
                    let _ = incoming_tx.try_send(IrcMessage {
                        tags: None,
                        prefix: None,
                        command: IrcCommand::Notice(
                            "*".to_string(),
                            format!("SASL {name} authentication successful"),
                        ),
                        raw: String::new(),
                    });
                    return Ok(SaslAttemptOutcome::Success);
                }
                SaslServerEvent::Success | SaslServerEvent::Failed => {
                    return Ok(SaslAttemptOutcome::Failed);
                }
                SaslServerEvent::Welcome => return Ok(SaslAttemptOutcome::Welcome),
            }
        }
    }

    async fn read_sasl_event<W, R>(
        &self,
        writer: &mut W,
        reader: &mut R,
        incoming_tx: &mpsc::Sender<IrcMessage>,
        buf: &mut Vec<u8>,
    ) -> Result<SaslServerEvent, Box<dyn std::error::Error + Send + Sync>>
    where
        W: tokio::io::AsyncWrite + Unpin,
        R: tokio::io::AsyncBufRead + Unpin,
    {
        loop {
            let Some(msg) = read_handshake_msg_timed(writer, reader, buf, incoming_tx).await?
            else {
                return Ok(SaslServerEvent::Failed);
            };
            match &msg.command {
                IrcCommand::Authenticate(data) => {
                    return Ok(SaslServerEvent::Challenge(data.clone()));
                }
                IrcCommand::Numeric(RPL_WELCOME, _) => return Ok(SaslServerEvent::Welcome),
                IrcCommand::Numeric(RPL_SASLSUCCESS, _) => return Ok(SaslServerEvent::Success),
                IrcCommand::Numeric(
                    RPL_NICKLOCKED | ERR_SASLFAIL | ERR_SASLTOOLONG | ERR_SASLABORTED
                    | ERR_SASLALREADY,
                    _,
                ) => return Ok(SaslServerEvent::Failed),
                // 908 advertises the mechanisms supported by the server. It
                // can accompany a failed attempt and is not itself terminal.
                IrcCommand::Numeric(RPL_LOGGEDIN | RPL_SASLMECHS, _) => continue,
                _ => continue,
            }
        }
    }

    async fn write_sasl_payload<W>(
        writer: &mut W,
        payload: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let encoded = BASE64_STANDARD.encode(payload);
        tracing::debug!("> AUTHENTICATE <credentials>");
        if encoded.is_empty() {
            writer.write_all(b"AUTHENTICATE +\r\n").await?;
        } else {
            for chunk in encoded.as_bytes().chunks(400) {
                writer.write_all(b"AUTHENTICATE ").await?;
                writer.write_all(chunk).await?;
                writer.write_all(b"\r\n").await?;
            }
            if encoded.len().is_multiple_of(400) {
                writer.write_all(b"AUTHENTICATE +\r\n").await?;
            }
        }
        writer.flush().await?;
        Ok(())
    }

    async fn read_loop<R: tokio::io::AsyncBufRead + Unpin>(
        &mut self,
        mut reader: R,
        incoming_tx: mpsc::Sender<IrcMessage>,
    ) {
        let mut buf = Vec::with_capacity(2048);
        // Dead-connection detection: if nothing arrives for READ_IDLE_TIMEOUT we
        // probe with a PING; if the server stays silent through PONG_GRACE the
        // link is treated as dead. Without this, silent TCP death (suspend/resume,
        // expired NAT entries) leaves the app showing "connected" forever.
        let mut probe_sent = false;
        loop {
            if incoming_tx.is_closed() {
                tracing::info!("GUI receiver dropped; stopping read loop");
                break;
            }
            let idle_limit = if probe_sent {
                PONG_GRACE
            } else {
                READ_IDLE_TIMEOUT
            };
            // Read bytes until newline to handle non-UTF8 encodings (common on older
            // IRC networks); bounded so a newline-less flood cannot exhaust memory.
            // A timeout here cancels the read mid-line safely: read_until_lf_bounded
            // keeps partial bytes in `buf` and the next call resumes accumulating.
            let timed_read = tokio::select! {
                _ = incoming_tx.closed() => {
                    tracing::info!("GUI receiver dropped; stopping read loop");
                    break;
                }
                result = timeout(
                    idle_limit,
                    read_until_lf_bounded(&mut reader, &mut buf, MAX_LINE_LEN),
                ) => result,
            };
            let read_result = match timed_read {
                Ok(result) => result,
                Err(_) if probe_sent => {
                    tracing::warn!("No data from server after keepalive probe; connection is dead");
                    let _ = incoming_tx.try_send(IrcMessage {
                        tags: None,
                        prefix: None,
                        command: IrcCommand::Notice(
                            "*".to_string(),
                            "Connection timed out (no response from server)".to_string(),
                        ),
                        raw: String::new(),
                    });
                    break;
                }
                Err(_) => {
                    probe_sent = true;
                    if let Some(ref tx) = self.tx {
                        let _ = tx.send("PING :linefeed-keepalive\r\n".to_string()).await;
                    }
                    continue;
                }
            };
            match read_result {
                Ok(0) => {
                    tracing::info!("Connection closed by server (EOF)");
                    let _ = incoming_tx.try_send(IrcMessage {
                        tags: None,
                        prefix: None,
                        command: IrcCommand::Notice(
                            "*".to_string(),
                            "Connection closed by server".to_string(),
                        ),
                        raw: String::new(),
                    });
                    break;
                }
                Ok(n) => {
                    tracing::trace!("Read {} bytes", n);
                    probe_sent = false;
                    if !buf.ends_with(b"\n") {
                        // EOF reached mid-line: the trailing fragment is an incomplete
                        // message, so discard it rather than parsing a truncated line.
                        tracing::info!("Connection closed by server (partial line discarded)");
                        let _ = incoming_tx.try_send(IrcMessage {
                            tags: None,
                            prefix: None,
                            command: IrcCommand::Notice(
                                "*".to_string(),
                                "Connection closed by server".to_string(),
                            ),
                            raw: String::new(),
                        });
                        break;
                    }
                    // Convert to UTF-8 lossily (replaces invalid sequences with replacement char)
                    {
                        let line = bytes_to_string_lossy(&buf);
                        if !line.is_empty() {
                            tracing::debug!("< {}", line);
                            if let Some(msg) = IrcMessage::parse(&line) {
                                if self.config.use_tls
                                    && !self.config.accept_invalid_certs
                                    && let IrcCommand::Cap(_, subcommand, params) = &msg.command
                                    && subcommand.eq_ignore_ascii_case("NEW")
                                    && let Some(sts) = params
                                        .last()
                                        .into_iter()
                                        .flat_map(|caps| caps.split_whitespace())
                                        .find_map(|cap| cap.strip_prefix("sts="))
                                    && let Some(duration) = parse_sts_value(sts).1
                                {
                                    update_sts_policy(
                                        &self.config.host,
                                        self.config.port,
                                        duration,
                                    );
                                }
                                // Handle PING automatically. Awaiting the send
                                // (instead of try_send) means a saturated writer
                                // queue delays the PONG rather than silently
                                // dropping it and risking a server ping-timeout.
                                if let IrcCommand::Ping(server) = &msg.command {
                                    let pong = format!("PONG :{}\r\n", server);
                                    if let Some(ref tx) = self.tx
                                        && tx.send(pong).await.is_err()
                                    {
                                        tracing::warn!("Writer task gone; cannot send PONG");
                                    }
                                }
                                // Awaiting send applies backpressure (and naturally throttles
                                // our socket reads) instead of silently dropping messages when
                                // the GUI is briefly behind.
                                if incoming_tx.send(msg).await.is_err() {
                                    tracing::warn!("GUI receiver dropped; stopping read loop");
                                    break;
                                }
                            }
                        }
                    }
                    buf.clear();
                }
                Err(e) => {
                    let err_msg = format!("{}", e);
                    tracing::error!("Read error: {}", err_msg);
                    let _ = incoming_tx.try_send(IrcMessage {
                        tags: None,
                        prefix: None,
                        command: IrcCommand::Notice(
                            "*".to_string(),
                            format!("[v3] Read error: {}", err_msg),
                        ),
                        raw: String::new(),
                    });
                    break;
                }
            }
        }
        tracing::info!("Read loop exited");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    #[test]
    fn scram_sha256_matches_rfc_7677_vector() {
        let client_nonce = "rOprNGfwEbeRWgbNEkqO";
        let client_first_bare = "n=user,r=rOprNGfwEbeRWgbNEkqO";
        let server_first = concat!(
            "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,",
            "s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096"
        );
        let (client_final, server_signature) =
            scram_sha256_response("pencil", client_nonce, client_first_bare, server_first).unwrap();

        assert_eq!(
            client_final,
            concat!(
                "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,",
                "p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
            )
        );
        assert_eq!(
            BASE64_STANDARD.encode(server_signature),
            "6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
        );
    }

    #[test]
    fn sts_values_and_sensitive_commands_are_parsed_safely() {
        assert_eq!(
            parse_sts_value("port=6697,duration=86400"),
            (Some(6697), Some(86400))
        );
        assert_eq!(parse_sts_value("port=0,duration=nope"), (None, None));
        assert_eq!(redact_for_log("AUTH account secret\r\n"), "AUTH <redacted>");
        assert_eq!(
            redact_for_log("PRIVMSG Q@CServe.quakenet.org :AUTH account secret\r\n"),
            "PRIVMSG NickServ :<redacted>"
        );
    }

    #[test]
    fn sts_probe_reads_the_advertised_tls_port_without_registering() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                write_half
                    .write_all(b":srv CAP * LS :sts=port=6697\r\n")
                    .await
                    .unwrap();
            });
            let (incoming_tx, _incoming_rx) = mpsc::channel(1);
            let (_outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel();
            let mut pending = VecDeque::new();
            let port = IrcClient::probe_sts_upgrade(
                "127.0.0.1",
                address.port(),
                &incoming_tx,
                &mut outgoing_rx,
                &mut pending,
            )
            .await
            .unwrap();
            assert_eq!(port, Some(6697));
            assert!(pending.is_empty());
            server.await.unwrap();
        });
    }

    #[test]
    fn complete_nefarious_cap_request_is_split_within_irc_line_limit() {
        let mut caps = DESIRED_CAPS.to_vec();
        caps.push("sasl");
        let lines = cap_request_lines(&caps);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| line.len() <= IRC_WIRE_LINE_BYTES));

        let requested: Vec<&str> = lines
            .iter()
            .flat_map(|line| {
                line.trim_end_matches("\r\n")
                    .strip_prefix("CAP REQ :")
                    .unwrap()
                    .split_whitespace()
            })
            .collect();
        assert_eq!(requested, caps);
    }

    #[test]
    fn split_cap_requests_wait_for_every_ack() {
        test_runtime().block_on(async {
            let expected_requests = cap_request_lines(DESIRED_CAPS).len();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                assert_eq!(read_wire_line(&mut reader).await, "NICK tester");
                assert!(read_wire_line(&mut reader).await.starts_with("USER "));
                write_half
                    .write_all(format!(":srv CAP * LS :{}\r\n", DESIRED_CAPS.join(" ")).as_bytes())
                    .await
                    .unwrap();

                for _ in 0..expected_requests {
                    let request = read_wire_line(&mut reader).await;
                    assert!(request.len() + 2 <= IRC_WIRE_LINE_BYTES);
                    let caps = request.strip_prefix("CAP REQ :").unwrap();
                    write_half
                        .write_all(format!(":srv CAP * ACK :{caps}\r\n").as_bytes())
                        .await
                        .unwrap();
                }
                assert_eq!(read_wire_line(&mut reader).await, "CAP END");
                write_half
                    .write_all(b":srv 001 tester :Welcome\r\n")
                    .await
                    .unwrap();
            });

            let config = ServerConfig {
                host: "127.0.0.1".into(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".into(),
                ..Default::default()
            };
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            timeout(Duration::from_secs(1), client.connect(msg_tx, cmd_rx))
                .await
                .expect("split CAP negotiation timed out")
                .unwrap();
            server.await.unwrap();
        });
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    async fn read_wire_line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> String {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        line.trim_end_matches(['\r', '\n']).to_string()
    }

    /// End-to-end: CAP negotiation, registration, and message forwarding
    /// against a scripted fake server over a real (plaintext) socket.
    #[test]
    fn plain_connection_registers_and_forwards_messages() {
        let rt = test_runtime();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                assert_eq!(read_wire_line(&mut reader).await, "PASS server-pass");
                assert_eq!(read_wire_line(&mut reader).await, "NICK tester");
                assert!(read_wire_line(&mut reader).await.starts_with("USER "));

                write_half
                    .write_all(
                        b":srv CAP * LS :multi-prefix away-notify draft/chathistory=limit=100\r\n",
                    )
                    .await
                    .unwrap();

                assert_eq!(
                    read_wire_line(&mut reader).await,
                    "CAP REQ :multi-prefix away-notify draft/chathistory"
                );
                write_half
                    .write_all(
                        b":srv CAP * ACK :multi-prefix away-notify draft/chathistory\r\n",
                    )
                    .await
                    .unwrap();

                assert_eq!(read_wire_line(&mut reader).await, "CAP END");

                write_half
                    .write_all(
                        b":srv 001 tester :Welcome\r\n:alice!a@h PRIVMSG #chan :hello  \r\n:alice!a@h KICK #chan tester :bye\r\n",
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    read_wire_line(&mut reader).await,
                    "PRIVMSG #chan :queued during connect"
                );
                // Hold the socket open long enough for the client to read.
                tokio::time::sleep(Duration::from_millis(500)).await;
            });

            let config = ServerConfig {
                host: "127.0.0.1".to_string(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".to_string(),
                password: Some("server-pass".to_string()),
                ..Default::default()
            };
            let (msg_tx, mut msg_rx) = mpsc::channel(100);
            // Named binding: dropping the sender would close the command channel.
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            cmd_tx
                .send(IrcCommand::Privmsg(
                    "#chan".to_string(),
                    "queued during connect".to_string(),
                ))
                .unwrap();
            let mut client = IrcClient::new(config);
            let client_task = tokio::spawn(async move {
                let _ = client.connect(msg_tx, cmd_rx).await;
            });

            let mut saw_welcome = false;
            let mut saw_spaced_message = false;
            let mut saw_kick = false;
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(msg) = msg_rx.recv().await {
                    match &msg.command {
                        IrcCommand::Numeric(1, _) => saw_welcome = true,
                        IrcCommand::Privmsg(_, content) => {
                            assert_eq!(content, "hello  ");
                            saw_spaced_message = true;
                        }
                        IrcCommand::Kick(chan, nick, reason) => {
                            assert_eq!(chan, "#chan");
                            assert_eq!(nick, "tester");
                            assert_eq!(reason.as_deref(), Some("bye"));
                            saw_kick = true;
                            break;
                        }
                        _ => {}
                    }
                }
            })
            .await;

            assert!(result.is_ok(), "timed out waiting for server messages");
            assert!(saw_welcome, "RPL_WELCOME was not forwarded");
            assert!(saw_spaced_message, "trailing message spaces were lost");
            assert!(saw_kick, "KICK was not forwarded");

            server.abort();
            client_task.abort();
        });
    }

    #[test]
    fn registration_extensions_reject_invalid_wire_values_before_connecting() {
        test_runtime().block_on(async {
            for config in [
                ServerConfig {
                    pre_away_message: Some("away\r\nOPER root password".into()),
                    ..Default::default()
                },
                ServerConfig {
                    persistence_profile: Some("bad profile".into()),
                    ..Default::default()
                },
            ] {
                let (msg_tx, _msg_rx) = mpsc::channel(1);
                let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
                let error = IrcClient::new(config)
                    .connect(msg_tx, cmd_rx)
                    .await
                    .expect_err("invalid registration extension must be rejected");
                assert!(error.to_string().contains("must be"));
            }
        });
    }

    #[test]
    fn bounded_reader_checks_the_chunk_that_contains_lf() {
        test_runtime().block_on(async {
            let exact = b"abc\n";
            let mut reader = BufReader::with_capacity(1, &exact[..]);
            let mut buf = Vec::new();
            assert_eq!(
                read_until_lf_bounded(&mut reader, &mut buf, exact.len())
                    .await
                    .unwrap(),
                exact.len()
            );
            assert_eq!(buf, exact);

            // Reproduces the vulnerable final-chunk case: the buffer is already
            // at the limit when the next fill contains a newline.
            let final_chunk = b"x\n";
            let mut reader = BufReader::new(&final_chunk[..]);
            let mut buf = vec![b'a'; 4];
            let error = read_until_lf_bounded(&mut reader, &mut buf, 4)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert_eq!(buf.len(), 4, "over-limit bytes must not be appended");

            let empty = b"";
            let mut reader = BufReader::new(&empty[..]);
            let mut buf = vec![b'a'; 5];
            assert_eq!(
                read_until_lf_bounded(&mut reader, &mut buf, 4)
                    .await
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::InvalidData
            );

            // The no-LF branch uses the same pre-allocation bound.
            let no_lf = b"abcde";
            let mut reader = BufReader::new(&no_lf[..]);
            let mut buf = Vec::new();
            assert_eq!(
                read_until_lf_bounded(&mut reader, &mut buf, 4)
                    .await
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::InvalidData
            );
        });
    }

    #[test]
    fn cap_budget_rejects_excess_bytes_tokens_and_lines() {
        let mut bytes = CapBudget::default();
        assert!(bytes.observe(&"x".repeat(MAX_CAP_BYTES + 1)).is_err());

        let mut tokens = CapBudget::default();
        assert!(tokens.observe(&"x ".repeat(MAX_CAP_TOKENS + 1)).is_err());

        let mut lines = CapBudget::default();
        for _ in 0..MAX_CAP_LINES {
            lines.observe("x").unwrap();
        }
        assert!(lines.observe("x").is_err());
    }

    #[test]
    fn cap_continuation_flood_is_rejected_promptly() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                assert_eq!(read_wire_line(&mut reader).await, "NICK tester");
                assert!(read_wire_line(&mut reader).await.starts_with("USER "));
                for _ in 0..=MAX_CAP_LINES {
                    write_half
                        .write_all(b":srv CAP * LS * :away-notify\r\n")
                        .await
                        .unwrap();
                }
                write_half.flush().await.unwrap();
            });

            let config = ServerConfig {
                host: "127.0.0.1".to_string(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".to_string(),
                ..Default::default()
            };
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            let result = timeout(Duration::from_secs(2), client.connect(msg_tx, cmd_rx))
                .await
                .expect("CAP flood did not terminate promptly")
                .unwrap_err();
            assert!(result.to_string().contains("CAP negotiation exceeded"));
            server.await.unwrap();
        });
    }

    #[test]
    fn registration_deadline_includes_waiting_for_welcome() {
        test_runtime().block_on(async {
            let (client_io, server_io) = tokio::io::duplex(4096);
            let (client_read, mut client_write) = tokio::io::split(client_io);
            let mut client_read = BufReader::new(client_read);
            let (server_read, mut server_write) = tokio::io::split(server_io);
            let mut server_read = BufReader::new(server_read);

            let server = tokio::spawn(async move {
                assert_eq!(read_wire_line(&mut server_read).await, "CAP LS 302");
                assert_eq!(read_wire_line(&mut server_read).await, "NICK tester");
                assert!(read_wire_line(&mut server_read).await.starts_with("USER "));
                server_write
                    .write_all(b":srv 421 tester CAP :Unknown command\r\n")
                    .await
                    .unwrap();
                loop {
                    if server_write
                        .write_all(b":srv NOTICE tester :still waiting\r\n")
                        .await
                        .is_err()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });

            let config = ServerConfig {
                nick: "tester".to_string(),
                ..Default::default()
            };
            let client = IrcClient::new(config);
            let (msg_tx, mut msg_rx) = mpsc::channel(100);
            let drain = tokio::spawn(async move { while msg_rx.recv().await.is_some() {} });
            let error = client
                .register_with_deadline(
                    &mut client_write,
                    &mut client_read,
                    &msg_tx,
                    Duration::from_millis(100),
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("never sent a welcome reply"));

            drop(msg_tx);
            server.abort();
            drain.await.unwrap();
        });
    }

    #[test]
    fn legacy_server_can_register_without_answering_cap() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                assert_eq!(read_wire_line(&mut reader).await, "NICK legacy");
                assert!(read_wire_line(&mut reader).await.starts_with("USER "));
                // A legacy server ignores CAP and registers immediately once it
                // has NICK/USER. This must not incur the CAP step timeout.
                write_half
                    .write_all(b":legacy 001 legacy :Welcome\r\n")
                    .await
                    .unwrap();
            });

            let config = ServerConfig {
                host: "127.0.0.1".to_string(),
                port: addr.port(),
                use_tls: false,
                nick: "legacy".to_string(),
                ..Default::default()
            };
            let (msg_tx, mut msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            timeout(Duration::from_secs(1), client.connect(msg_tx, cmd_rx))
                .await
                .expect("legacy registration waited for the CAP timeout")
                .unwrap();
            assert!(
                std::iter::from_fn(|| msg_rx.try_recv().ok())
                    .any(|msg| matches!(msg.command, IrcCommand::Numeric(RPL_WELCOME, _)))
            );
            server.await.unwrap();
        });
    }

    #[test]
    fn required_sasl_rejects_a_legacy_welcome() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                assert_eq!(read_wire_line(&mut reader).await, "NICK tester");
                assert!(read_wire_line(&mut reader).await.starts_with("USER "));
                write_half
                    .write_all(b":legacy 001 tester :Welcome\r\n")
                    .await
                    .unwrap();
            });

            let config = ServerConfig {
                host: "127.0.0.1".into(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".into(),
                sasl_username: Some("account".into()),
                sasl_password: Some("secret".into()),
                sasl_required: true,
                ..Default::default()
            };
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let error = timeout(
                Duration::from_secs(1),
                IrcClient::new(config).connect(msg_tx, cmd_rx),
            )
            .await
            .expect("required-SASL registration timed out")
            .unwrap_err();
            assert!(error.to_string().contains("required SASL"));
            server.await.unwrap();
        });
    }

    #[test]
    fn handshake_answers_pings_without_stalling_cap_negotiation() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                assert_eq!(read_wire_line(&mut reader).await, "NICK tester");
                assert!(read_wire_line(&mut reader).await.starts_with("USER "));

                // A bouncer-style server PINGs unregistered clients while its
                // CAP LS reply is delayed. Each PING must be answered without
                // consuming the per-step budget meant for the CAP reply.
                for ping in ["h1", "h2", "h3"] {
                    write_half
                        .write_all(format!(":srv PING :{ping}\r\n").as_bytes())
                        .await
                        .unwrap();
                    assert_eq!(read_wire_line(&mut reader).await, format!("PONG :{ping}"));
                }

                write_half
                    .write_all(b":srv CAP * LS :multi-prefix\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_wire_line(&mut reader).await, "CAP REQ :multi-prefix");
                write_half
                    .write_all(b":srv CAP * ACK :multi-prefix\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_wire_line(&mut reader).await, "CAP END");
                write_half
                    .write_all(b":srv 001 tester :Welcome\r\n")
                    .await
                    .unwrap();
                // Hold the socket open long enough for the client to read.
                tokio::time::sleep(Duration::from_millis(100)).await;
            });

            let config = ServerConfig {
                host: "127.0.0.1".to_string(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".to_string(),
                ..Default::default()
            };
            let (msg_tx, mut msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            timeout(Duration::from_secs(5), client.connect(msg_tx, cmd_rx))
                .await
                .expect("registration completes despite handshake PINGs")
                .unwrap();
            assert!(
                std::iter::from_fn(|| msg_rx.try_recv().ok())
                    .any(|msg| matches!(msg.command, IrcCommand::Numeric(RPL_WELCOME, _)))
            );
            server.await.unwrap();
        });
    }

    #[test]
    fn sasl_registration_still_completes_before_welcome() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                assert_eq!(read_wire_line(&mut reader).await, "NICK tester");
                assert!(read_wire_line(&mut reader).await.starts_with("USER "));
                write_half
                    .write_all(b":srv CAP * LS :sasl=PLAIN\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_wire_line(&mut reader).await, "CAP REQ :sasl");
                write_half
                    .write_all(b":srv CAP * ACK :sasl\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_wire_line(&mut reader).await, "AUTHENTICATE PLAIN");
                write_half.write_all(b"AUTHENTICATE +\r\n").await.unwrap();
                let expected = BASE64_STANDARD.encode(b"\0user\0password");
                assert_eq!(
                    read_wire_line(&mut reader).await,
                    format!("AUTHENTICATE {expected}")
                );
                write_half
                    .write_all(b":srv 903 tester :SASL successful\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_wire_line(&mut reader).await, "CAP END");
                write_half
                    .write_all(b":srv 001 tester :Welcome\r\n")
                    .await
                    .unwrap();
            });

            let config = ServerConfig {
                host: "127.0.0.1".to_string(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".to_string(),
                sasl_username: Some("user".to_string()),
                sasl_password: Some("password".to_string()),
                ..Default::default()
            };
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            timeout(Duration::from_secs(1), client.connect(msg_tx, cmd_rx))
                .await
                .expect("SASL registration timed out")
                .unwrap();
            server.await.unwrap();
        });
    }

    #[test]
    fn nefarious_registration_only_commands_precede_cap_end() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                assert_eq!(read_wire_line(&mut reader).await, "NICK tester");
                assert!(read_wire_line(&mut reader).await.starts_with("USER "));
                write_half
                    .write_all(b":srv CAP * LS :draft/pre-away draft/persistence sasl=PLAIN\r\n")
                    .await
                    .unwrap();
                assert_eq!(
                    read_wire_line(&mut reader).await,
                    "CAP REQ :draft/pre-away draft/persistence sasl"
                );
                write_half
                    .write_all(b":srv CAP * ACK :draft/pre-away draft/persistence sasl\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_wire_line(&mut reader).await, "AUTHENTICATE PLAIN");
                write_half.write_all(b"AUTHENTICATE +\r\n").await.unwrap();
                let expected = BASE64_STANDARD.encode(b"\0user\0password");
                assert_eq!(
                    read_wire_line(&mut reader).await,
                    format!("AUTHENTICATE {expected}")
                );
                write_half
                    .write_all(b":srv 903 tester :SASL successful\r\n")
                    .await
                    .unwrap();
                assert_eq!(
                    read_wire_line(&mut reader).await,
                    "PERSISTENCE ATTACH mobile"
                );
                assert_eq!(read_wire_line(&mut reader).await, "AWAY :syncing");
                assert_eq!(read_wire_line(&mut reader).await, "CAP END");
                write_half
                    .write_all(b":srv 001 tester :Welcome\r\n")
                    .await
                    .unwrap();
            });

            let config = ServerConfig {
                host: "127.0.0.1".into(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".into(),
                sasl_username: Some("user".into()),
                sasl_password: Some("password".into()),
                pre_away_message: Some("syncing".into()),
                persistence_profile: Some("mobile".into()),
                ..Default::default()
            };
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            timeout(Duration::from_secs(1), client.connect(msg_tx, cmd_rx))
                .await
                .expect("registration-only commands timed out")
                .unwrap();
            server.await.unwrap();
        });
    }

    #[test]
    fn quit_cancels_stalled_tls_handshake() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (accepted_tx, mut accepted_rx) = mpsc::channel(1);
            let server = tokio::spawn(async move {
                let (_stream, _) = listener.accept().await.unwrap();
                accepted_tx.send(()).await.unwrap();
                tokio::time::sleep(Duration::from_secs(5)).await;
            });

            let config = ServerConfig {
                host: "127.0.0.1".to_string(),
                port: addr.port(),
                use_tls: true,
                nick: "tester".to_string(),
                ..Default::default()
            };
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            let client_task = tokio::spawn(async move {
                client
                    .connect(msg_tx, cmd_rx)
                    .await
                    .map_err(|error| error.to_string())
            });
            accepted_rx.recv().await.unwrap();
            cmd_tx.send(IrcCommand::Quit(None)).unwrap();
            let error = timeout(Duration::from_secs(1), client_task)
                .await
                .expect("TLS cancellation did not stop the connection")
                .unwrap()
                .unwrap_err();
            assert!(error.contains("cancelled during TLS handshake"));
            server.abort();
        });
    }

    #[test]
    fn prequeued_quit_cancels_tcp_connect() {
        test_runtime().block_on(async {
            let config = ServerConfig {
                host: "203.0.113.1".to_string(),
                port: 9,
                use_tls: false,
                ..Default::default()
            };
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            cmd_tx.send(IrcCommand::Quit(None)).unwrap();
            let mut client = IrcClient::new(config);
            let error = timeout(Duration::from_secs(1), client.connect(msg_tx, cmd_rx))
                .await
                .expect("prequeued QUIT did not cancel TCP connect")
                .unwrap_err();
            assert!(error.to_string().contains("cancelled during TCP connect"));
        });
    }

    #[test]
    fn quit_cancels_stalled_registration_read() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (preamble_tx, mut preamble_rx) = mpsc::channel(1);
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, _write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                assert_eq!(read_wire_line(&mut reader).await, "NICK tester");
                assert!(read_wire_line(&mut reader).await.starts_with("USER "));
                preamble_tx.send(()).await.unwrap();
                tokio::time::sleep(Duration::from_secs(5)).await;
            });

            let config = ServerConfig {
                host: "127.0.0.1".to_string(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".to_string(),
                ..Default::default()
            };
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            let client_task = tokio::spawn(async move {
                client
                    .connect(msg_tx, cmd_rx)
                    .await
                    .map_err(|error| error.to_string())
            });
            preamble_rx.recv().await.unwrap();
            cmd_tx.send(IrcCommand::Quit(None)).unwrap();
            let error = timeout(Duration::from_secs(1), client_task)
                .await
                .expect("QUIT did not cancel registration")
                .unwrap()
                .unwrap_err();
            assert!(error.contains("cancelled during registration"));
            server.abort();
        });
    }

    #[test]
    fn quit_cancels_reader_when_server_keeps_socket_open() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (quit_tx, mut quit_rx) = mpsc::channel(1);
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                assert_eq!(read_wire_line(&mut reader).await, "NICK tester");
                assert!(read_wire_line(&mut reader).await.starts_with("USER "));
                write_half.write_all(b":srv CAP * LS :\r\n").await.unwrap();
                assert_eq!(read_wire_line(&mut reader).await, "CAP END");
                write_half
                    .write_all(b":srv 001 tester :Welcome\r\n")
                    .await
                    .unwrap();
                let quit = read_wire_line(&mut reader).await;
                assert_eq!(quit, "QUIT :bye");
                quit_tx.send(()).await.unwrap();
                // Deliberately ignore QUIT and leave the socket open.
                tokio::time::sleep(Duration::from_secs(5)).await;
            });

            let config = ServerConfig {
                host: "127.0.0.1".to_string(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".to_string(),
                ..Default::default()
            };
            let (msg_tx, mut msg_rx) = mpsc::channel(100);
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            let client_task = tokio::spawn(async move {
                client
                    .connect(msg_tx, cmd_rx)
                    .await
                    .map_err(|error| error.to_string())
            });
            timeout(Duration::from_secs(1), async {
                while let Some(msg) = msg_rx.recv().await {
                    if matches!(msg.command, IrcCommand::Numeric(RPL_WELCOME, _)) {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            cmd_tx
                .send(IrcCommand::Quit(Some("bye".to_string())))
                .unwrap();
            quit_rx.recv().await.unwrap();
            timeout(Duration::from_secs(1), client_task)
                .await
                .expect("reader stayed blocked after QUIT")
                .unwrap()
                .unwrap();
            server.abort();
        });
    }

    #[test]
    fn dropping_gui_receiver_cancels_established_socket_read() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                assert_eq!(read_wire_line(&mut reader).await, "NICK tester");
                assert!(read_wire_line(&mut reader).await.starts_with("USER "));
                write_half.write_all(b":srv CAP * LS :\r\n").await.unwrap();
                assert_eq!(read_wire_line(&mut reader).await, "CAP END");
                write_half
                    .write_all(b":srv 001 tester :Welcome\r\n")
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_secs(5)).await;
            });

            let config = ServerConfig {
                host: "127.0.0.1".to_string(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".to_string(),
                ..Default::default()
            };
            let (msg_tx, mut msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            let client_task = tokio::spawn(async move {
                client
                    .connect(msg_tx, cmd_rx)
                    .await
                    .map_err(|error| error.to_string())
            });
            timeout(Duration::from_secs(1), async {
                while let Some(msg) = msg_rx.recv().await {
                    if matches!(msg.command, IrcCommand::Numeric(RPL_WELCOME, _)) {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            drop(msg_rx);
            timeout(Duration::from_secs(1), client_task)
                .await
                .expect("closed GUI receiver did not cancel socket read")
                .unwrap()
                .unwrap();
            server.abort();
        });
    }

    #[test]
    fn ipv6_literal_reaches_loopback_listener() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                assert_eq!(read_wire_line(&mut reader).await, "NICK ipv6");
                assert!(read_wire_line(&mut reader).await.starts_with("USER "));
                write_half
                    .write_all(b":ipv6 001 ipv6 :Welcome\r\n")
                    .await
                    .unwrap();
            });

            let config = ServerConfig {
                host: "::1".to_string(),
                port: addr.port(),
                use_tls: false,
                nick: "ipv6".to_string(),
                ..Default::default()
            };
            assert_eq!(
                format_server_addr(&config.host, config.port),
                format!("[::1]:{}", addr.port())
            );
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            timeout(Duration::from_secs(1), client.connect(msg_tx, cmd_rx))
                .await
                .expect("IPv6 connect timed out")
                .unwrap();
            server.await.unwrap();
        });
    }
}
