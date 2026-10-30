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
/// Bounds on the phases before registration, whose own deadline only starts
/// once TLS completes. Without them a blackholed address or a middlebox that
/// accepts TCP and then goes silent stalls the connection thread forever, so
/// the GUI's auto-reconnect never gets a connection error to act on (#233).
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
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

/// How one connect deadline is divided between the addresses a host resolves
/// to. A blackholed address (preferred IPv6 on a broken route, say) must not
/// starve the fallbacks behind it, but halving what is left for each address
/// in turn would starve the later ones exponentially, so the half is held to
/// a range: no more than `cap` and, budget permitting, no less than `floor`.
#[derive(Clone, Copy)]
struct AttemptShares {
    cap: Duration,
    floor: Duration,
}

/// Half of what is left of the connect budget, between 2 and 8 seconds.
const TCP_ATTEMPT_SHARES: AttemptShares = AttemptShares {
    cap: Duration::from_secs(8),
    floor: Duration::from_secs(2),
};

impl AttemptShares {
    /// How long to wait on the next address, given the time `remaining`. The
    /// last address gets everything left; no address ever gets more than
    /// that.
    fn share(self, remaining: Duration, last: bool) -> Duration {
        if last {
            remaining
        } else {
            (remaining / 2).clamp(self.floor, self.cap).min(remaining)
        }
    }
}

/// Try each resolved address in turn, all within one `deadline`, giving each
/// but the last the `shares` of the time still left (#233).
async fn connect_first_reachable<T, F, Fut>(
    addrs: &[std::net::SocketAddr],
    deadline: tokio::time::Instant,
    shares: AttemptShares,
    mut connect: F,
) -> std::io::Result<T>
where
    F: FnMut(std::net::SocketAddr) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<T>>,
{
    let mut last_error = None;
    for (index, addr) in addrs.iter().enumerate() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let share = shares.share(remaining, index + 1 == addrs.len());
        last_error = Some(match timeout(share, connect(*addr)).await {
            Ok(Ok(connected)) => return Ok(connected),
            Ok(Err(error)) => error,
            Err(_) => std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out"),
        });
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "could not resolve to any address",
        )
    }))
}

/// Resolve `host` and connect TCP, bounded overall by `budget`.
async fn connect_tcp(host: &str, port: u16, budget: Duration) -> std::io::Result<TcpStream> {
    connect_tcp_via(host, port, budget, TCP_ATTEMPT_SHARES, TcpStream::connect).await
}

/// `connect_tcp` with the per-address connect (and its time shares) supplied.
async fn connect_tcp_via<T, F, Fut>(
    host: &str,
    port: u16,
    budget: Duration,
    shares: AttemptShares,
    connect: F,
) -> std::io::Result<T>
where
    F: FnMut(std::net::SocketAddr) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<T>>,
{
    let deadline = tokio::time::Instant::now() + budget;
    let addrs: Vec<_> = tokio::time::timeout_at(deadline, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "name lookup timed out"))??
        .collect();
    connect_first_reachable(&addrs, deadline, shares, connect).await
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

/// Registration-phase variant of wait_for_disconnect: a queued NICK (the
/// GUI's 433 walkaway retry — registration state that must reach the wire
/// at once, not held until 001; #173/#174) is forwarded to the registration
/// read loops, which write it through beside their socket reads; everything
/// else stays buffered in order. The future completes only on disconnect,
/// so it can never drop or restart the in-flight registration.
async fn wait_for_registration_events(
    outgoing_rx: &mut mpsc::UnboundedReceiver<IrcCommand>,
    pending: &mut VecDeque<IrcCommand>,
    nick_tx: &mpsc::UnboundedSender<String>,
) {
    loop {
        match outgoing_rx.recv().await {
            Some(IrcCommand::Quit(_)) | None => return,
            Some(IrcCommand::Nick(nick)) => {
                let _ = nick_tx.send(nick);
            }
            Some(command) => pending.push_back(command),
        }
    }
}

/// Whether a message target names a network service (NickServ and friends,
/// the QuakeNet Q / Undernet X shorthands), optionally with a status prefix
/// or an `@network` suffix. Any element of a comma list counts. A local
/// predicate on purpose: the connection layer must not depend on GUI code,
/// and for logging over-redaction is the safe direction.
fn is_service_target(targets: &str) -> bool {
    targets.split(',').any(|target| {
        let name = target
            .trim_start_matches(['~', '&', '@', '%', '+'])
            .split('@')
            .next()
            .unwrap_or("");
        [
            "nickserv", "chanserv", "authserv", "hostserv", "memoserv", "botserv", "operserv",
            "saslserv", "ns", "cs", "as", "hs", "ms", "bs", "os", "q", "x",
        ]
        .iter()
        .any(|service| name.eq_ignore_ascii_case(service))
    })
}

/// Redact credentials from an outgoing line before it reaches the log output.
/// Whole-line verbs (PASS, AUTHENTICATE, OPER, REGISTER, bare service
/// commands, ...) carry passwords or tokens; any PRIVMSG/NOTICE/SQUERY to a
/// service-shaped target is redacted whatever its text, since the target
/// alone says the text is a service command.
fn redact_for_log(line: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = line.trim_end();
    // A tagged line ("@label=1 PRIVMSG ...") carries its command after the tags.
    let untagged = if trimmed.starts_with('@') {
        trimmed
            .split_once(char::is_whitespace)
            .map_or("", |(_, rest)| rest.trim_start())
    } else {
        trimmed
    };
    let mut words = untagged.split_whitespace();
    let verb = words.next().unwrap_or("");
    match verb.to_ascii_uppercase().as_str() {
        "PASS" | "AUTH" | "LOGIN" | "OPER" | "WEBPUSH" | "REGISTER" | "VERIFY" | "AUTHTOKEN"
        | "BOUNCER" | "IDENTIFY" | "ID" | "SIDENTIFY" | "GHOST" | "RECOVER" | "RELEASE"
        | "REGAIN" | "GROUP" | "SETPASS" | "NS" | "NICKSERV" | "CS" | "CHANSERV" | "AS"
        | "AUTHSERV" | "HS" | "HOSTSERV" | "MS" | "MEMOSERV" | "BS" | "BOTSERV"
            if words.next().is_some() =>
        {
            format!("{verb} <redacted>").into()
        }
        "TOKEN"
            if words
                .next()
                .is_some_and(|sub| sub.eq_ignore_ascii_case("VALIDATE")) =>
        {
            format!("{verb} <redacted>").into()
        }
        "AUTHENTICATE" => match words.next() {
            Some(arg) if arg != "+" && !arg.eq_ignore_ascii_case("PLAIN") => {
                format!("{verb} <redacted>").into()
            }
            _ => trimmed.into(),
        },
        "PRIVMSG" | "NOTICE" | "SQUERY" => match words.next() {
            Some(target) if is_service_target(target) => {
                format!("{verb} {target} :<redacted>").into()
            }
            _ => trimmed.into(),
        },
        _ => trimmed.into(),
    }
}

/// Wait for one complete line from the server and leave it in `buf`.
/// Cancellation-safe: callers race this against NICK write-through and the
/// step timeout, and dropping it loses nothing because partial bytes stay in
/// `buf` for the next call to resume (see `read_until_lf_bounded`).
async fn read_handshake_line<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    incoming_tx: &mpsc::Sender<IrcMessage>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
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
    Ok(())
}

/// Serialise an outbound command as one CRLF-terminated wire line. A command
/// whose fields contain CR, LF or NUL would put extra, attacker-chosen
/// protocol lines on the wire, so it is refused (logged without its content,
/// which may be a credential) and an empty string is returned, which the
/// writers send as a no-op (#285).
fn outbound_line(cmd: &IrcCommand) -> String {
    match cmd.to_wire_line() {
        Some(line) => line + "\r\n",
        None => {
            tracing::warn!("Refusing to send a command containing CR, LF or NUL");
            String::new()
        }
    }
}

/// Build the PONG answering a server PING. The token is server-controlled and
/// echoed verbatim, so CR, LF and NUL are dropped: a bare CR is a line
/// terminator to some peers (bouncers, lax parsers), which would turn the tail
/// of the token into a second command sent as this client (#281).
fn pong_line(token: &str) -> String {
    let token: String = token
        .chars()
        .filter(|c| !matches!(c, '\r' | '\n' | '\0'))
        .collect();
    format!("PONG :{token}\r\n")
}

/// Consume the complete line in `buf`: parse it, forward a copy to the GUI,
/// and transparently answer a server PING (which can arrive mid-handshake,
/// before the writer task that normally handles them exists). PINGs and
/// unparseable lines return `None` so the caller re-arms its fresh per-step
/// budget; any other message is returned for the caller's state machine.
///
/// Deliberately NOT cancellation-safe: the line has already left the socket,
/// so dropping this future mid-PONG or mid-forward would lose it (#277).
/// Callers run it to completion, outside any select! arm or timeout.
async fn handle_handshake_line<W>(
    writer: &mut W,
    buf: &mut Vec<u8>,
    incoming_tx: &mpsc::Sender<IrcMessage>,
) -> Result<Option<IrcMessage>, Box<dyn std::error::Error + Send + Sync>>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let trimmed = bytes_to_string_lossy(buf);
    buf.clear();
    tracing::debug!("< {}", trimmed);
    match IrcMessage::parse(&trimmed) {
        Some(msg) => {
            if let IrcCommand::Ping(token) = &msg.command {
                writer.write_all(pong_line(token).as_bytes()).await?;
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

/// Read the next protocol message of interest during the registration
/// handshake (see `handle_handshake_line`). Returns `None` when the per-step
/// budget expires with nothing received.
async fn read_handshake_msg_timed<W, R>(
    writer: &mut W,
    reader: &mut R,
    buf: &mut Vec<u8>,
    incoming_tx: &mpsc::Sender<IrcMessage>,
    nick_rx: &mut mpsc::UnboundedReceiver<String>,
) -> Result<Option<IrcMessage>, Box<dyn std::error::Error + Send + Sync>>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncBufRead + Unpin,
{
    // The per-step budget covers a single line, not the whole wait for the
    // message we actually care about: a server that PINGs during registration
    // (ZNC-style bouncers do) must not consume the budget for its delayed CAP
    // or SASL reply. The overall registration deadline still bounds the total.
    // A queued NICK (the GUI's 433 walkaway retry) is written through on
    // arrival — registration state, so no budget is consumed for it (#173).
    loop {
        // Only the wait for a line races the NICK write-through and the step
        // budget; that wait is cancellation-safe. Handling the line (PONG,
        // forward to the GUI) runs to completion below: it has already been
        // consumed from the socket, so a cancel there would lose it (#277).
        tokio::select! {
            biased;
            Some(nick) = nick_rx.recv() => {
                let line = outbound_line(&IrcCommand::Nick(nick));
                tracing::debug!("> {}", line.trim_end());
                writer.write_all(line.as_bytes()).await?;
                writer.flush().await?;
                continue;
            }
            outcome = timeout(
                HANDSHAKE_STEP_TIMEOUT,
                read_handshake_line(reader, buf, incoming_tx),
            ) => match outcome {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(None),
            }
        }
        // PING answered (or junk line skipped): re-arm a fresh budget.
        if let Some(msg) = handle_handshake_line(writer, buf, incoming_tx).await? {
            return Ok(Some(msg));
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

/// Resolve the `(port, duration)` to persist for an advertised STS value:
/// the advertised port when present, otherwise the current connection port
/// (#186).
fn sts_policy_target(value: &str, current_port: u16) -> Option<(u16, u64)> {
    let (advertised_port, duration) = parse_sts_value(value);
    duration.map(|duration| (advertised_port.unwrap_or(current_port), duration))
}

/// Accumulates inbound `AUTHENTICATE` chunks per the IRCv3 SASL chunking
/// rule: a 400-byte chunk is always followed by another chunk, a shorter
/// chunk terminates the payload, and `+` terminates an exact multiple of
/// 400 bytes (or stands for an empty payload on its own) (#187).
#[derive(Default)]
struct SaslChallengeAccumulator {
    buffer: String,
}

impl SaslChallengeAccumulator {
    /// Feed one chunk; returns the complete payload once terminated.
    fn feed(&mut self, data: &str) -> Option<String> {
        if data == "+" {
            if self.buffer.is_empty() {
                return Some("+".to_string());
            }
            return Some(std::mem::take(&mut self.buffer));
        }
        self.buffer.push_str(data);
        if data.len() == 400 {
            None
        } else {
            Some(std::mem::take(&mut self.buffer))
        }
    }

    fn pending_len(&self) -> usize {
        self.buffer.len()
    }
}

/// Bound on a reassembled SASL challenge so a server drip-feeding 400-byte
/// chunks cannot grow client memory without limit.
const MAX_SASL_CHALLENGE_LEN: usize = 16 * 1024;

/// Most STS policy writes one established connection may trigger through
/// `CAP NEW`. A legitimate server advertises its policy once or twice; the
/// cap bounds the disk I/O a hostile one can force by churning values.
const MAX_LIVE_STS_UPDATES: usize = 8;

/// Admits STS policies advertised by `CAP NEW` on an established connection.
/// Each admitted policy costs a synchronous read-modify-write of the policy
/// file inside the read loop, so identical repeats and floods are dropped
/// (#280).
#[derive(Default)]
struct LiveStsGate {
    last: Option<(u16, u64)>,
    writes: usize,
}

impl LiveStsGate {
    fn admit(&mut self, port: u16, duration: u64) -> bool {
        if self.last == Some((port, duration)) || self.writes >= MAX_LIVE_STS_UPDATES {
            return false;
        }
        self.last = Some((port, duration));
        self.writes += 1;
        true
    }
}

#[cfg(test)]
thread_local! {
    /// Policy writes requested on this thread. Unit tests record them instead
    /// of touching the real policy store in the user's config directory.
    static STS_POLICY_WRITES: std::cell::RefCell<Vec<(String, u16, u64)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn update_sts_policy(host: &str, port: u16, duration: u64) {
    #[cfg(test)]
    STS_POLICY_WRITES.with_borrow_mut(|writes| writes.push((host.to_string(), port, duration)));
    #[cfg(not(test))]
    persist_sts_policy(host, port, duration);
}

#[cfg_attr(test, allow(dead_code))]
fn persist_sts_policy(host: &str, port: u16, duration: u64) {
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

/// Base64 decode tolerating missing padding: the client's own SCRAM nonce
/// is emitted unpadded, and some servers emit unpadded salt/proofs the same
/// way (#180).
fn decode_base64_forgiving(value: &str) -> Result<Vec<u8>, base64::DecodeError> {
    match BASE64_STANDARD.decode(value) {
        Ok(bytes) => Ok(bytes),
        Err(_) => {
            let mut padded = value.to_string();
            while !padded.len().is_multiple_of(4) {
                padded.push('=');
            }
            BASE64_STANDARD.decode(padded)
        }
    }
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
    let salt = decode_base64_forgiving(salt.ok_or("SCRAM challenge omitted salt")?)
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

/// Also embedded in the update-restart session snapshot, written by the
/// previous release. Fields added since default when absent (`Option`s
/// already do), but those naming the endpoint and the account the credentials
/// belong to are required: a snapshot missing them must fail to parse, not
/// restore to the built-in default server and send its passwords there.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub use_tls: bool,
    #[serde(default)]
    pub accept_invalid_certs: bool,
    pub nick: String,
    pub username: String,
    #[serde(default = "default_realname")]
    pub realname: String,
    pub password: Option<String>,
    // SASL authentication
    pub sasl_username: Option<String>,
    pub sasl_password: Option<String>,
    #[serde(default)]
    pub sasl_mechanism: SaslMechanism,
    #[serde(default)]
    pub sasl_required: bool,
    pub tls_client_cert_path: Option<String>,
    pub tls_client_key_path: Option<String>,
    pub pre_away_message: Option<String>,
    pub persistence_profile: Option<String>,
}

fn default_realname() -> String {
    "Linefeed".to_string()
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
            realname: default_realname(),
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
    /// Overall budget for resolving and connecting TCP; overridable in tests.
    connect_timeout: Duration,
    /// Budget for the TLS handshake after TCP connects; overridable in tests.
    tls_handshake_timeout: Duration,
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
        Self {
            config,
            tx: None,
            connect_timeout: TCP_CONNECT_TIMEOUT,
            tls_handshake_timeout: TLS_HANDSHAKE_TIMEOUT,
        }
    }

    /// Wire-value validation for the config-sourced registration fields:
    /// they are formatted verbatim into PASS/NICK/USER lines, so CR/LF/NUL
    /// (command injection), spaces or a leading ':' in the username
    /// (parameter shifting) and over-length values are rejected before any
    /// I/O (#177/#179). Shared by the connect form so the dialog rejects up
    /// front what the connection would fail on later.
    pub(crate) fn validate_registration_fields(
        nick: &str,
        username: &str,
        realname: &str,
        password: &str,
    ) -> Result<(), String> {
        if nick.is_empty() {
            return Err("Nickname must be non-empty".into());
        }
        if nick.len() > 500 {
            return Err("Nickname is too long".into());
        }
        if nick
            .bytes()
            .any(|byte| matches!(byte, b'\r' | b'\n' | b'\0' | b' ' | b'@'))
        {
            return Err("Nickname must be free of spaces, @, CR, LF and NUL".into());
        }
        // "NICK :alice" would be read as a trailing parameter: the server
        // registers "alice" while the client believes ":alice" (#303).
        if nick.starts_with(':') {
            return Err("Nickname must not start with ':'".into());
        }
        if username.is_empty() {
            return Err("Username must be non-empty".into());
        }
        if username.len() > 400 {
            return Err("Username is too long".into());
        }
        if username.starts_with(':') {
            return Err("Username must not start with ':'".into());
        }
        if username
            .bytes()
            .any(|byte| matches!(byte, b' ' | b'\r' | b'\n' | b'\0'))
        {
            return Err("Username must be free of spaces, CR, LF and NUL".into());
        }
        for (field, value) in [("Realname", realname), ("Password", password)] {
            if value
                .bytes()
                .any(|byte| matches!(byte, b'\r' | b'\n' | b'\0'))
            {
                return Err(format!("{field} must not contain CR, LF or NUL"));
            }
            if value.len() > 400 {
                return Err(format!("{field} is too long"));
            }
        }
        // Each field fits on its own, but the formatted USER line (plus CRLF)
        // must also fit the 512-byte wire limit or the server truncates it.
        let user_line = IrcCommand::User {
            username: username.to_string(),
            realname: realname.to_string(),
        }
        .to_string();
        if user_line.len() + 2 > IRC_WIRE_LINE_BYTES {
            return Err("Username and realname are too long together".into());
        }
        Ok(())
    }

    pub async fn connect(
        &mut self,
        incoming_tx: mpsc::Sender<IrcMessage>,
        mut outgoing_rx: mpsc::UnboundedReceiver<IrcCommand>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut pending_commands = VecDeque::new();
        Self::validate_registration_fields(
            &self.config.nick,
            &self.config.username,
            &self.config.realname,
            self.config.password.as_deref().unwrap_or_default(),
        )?;
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
            result = connect_tcp(
                self.config.host.as_str(),
                self.config.port,
                self.connect_timeout,
            ) => result,
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
        Self::probe_sts_upgrade_with_timeout(
            host,
            port,
            incoming_tx,
            outgoing_rx,
            pending_commands,
            STS_PROBE_TIMEOUT,
        )
        .await
    }

    async fn probe_sts_upgrade_with_timeout(
        host: &str,
        port: u16,
        incoming_tx: &mpsc::Sender<IrcMessage>,
        outgoing_rx: &mut mpsc::UnboundedReceiver<IrcCommand>,
        pending_commands: &mut VecDeque<IrcCommand>,
        probe_timeout: Duration,
    ) -> Result<Option<u16>, Box<dyn std::error::Error + Send + Sync>> {
        let stream = tokio::select! {
            biased;
            _ = incoming_tx.closed() => return Err("Connection cancelled during STS probe".into()),
            _ = wait_for_disconnect(outgoing_rx, pending_commands) => {
                return Err("Connection cancelled during STS probe".into());
            }
            result = timeout(probe_timeout, TcpStream::connect((host, port))) => {
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
        // One overall deadline for the whole probe: a drip-feeding endpoint
        // must not re-arm the budget per line (#189).
        let deadline = tokio::time::Instant::now() + probe_timeout;
        for _ in 0..MAX_CAP_LINES {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
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
                    remaining,
                    read_until_lf_bounded(&mut reader, &mut buf, MAX_LINE_LEN),
                ) => result,
            };
            let Ok(Ok(count)) = read else {
                return Ok(None);
            };
            if count == 0 {
                return Ok(None);
            }
            if !buf.ends_with(b"\n") {
                // EOF left a truncated fragment: never parse it as a complete
                // message (mirrors read_handshake_msg and the main read loop).
                return Ok(None);
            }
            let line = String::from_utf8_lossy(&buf);
            let Some(message) = IrcMessage::parse(&line) else {
                continue;
            };
            if let IrcCommand::Ping(token) = message.command {
                writer.write_all(pong_line(&token).as_bytes()).await?;
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
            result = timeout(
                self.tls_handshake_timeout,
                connector.connect(&self.config.host, stream),
            ) => result,
        };
        let tls_stream = match tls_result {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                let msg = format!(
                    "TLS handshake failed: {}. Try disabling TLS or enabling 'Accept invalid certs'",
                    e
                );
                tracing::error!("{}", msg);
                return Err(msg.into());
            }
            Err(_) => {
                let msg = format!(
                    "TLS handshake timed out after {:?}; the server accepted the connection but never completed the handshake",
                    self.tls_handshake_timeout
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
        // cancels blocked writes and reads immediately as well. A queued NICK
        // is forwarded to the registration read loops: it is registration
        // state (the GUI's 433 walkaway retry), and buffering it until 001
        // would strand the retry on conforming servers (#173). Pre-connect
        // phases have no writer and buffer everything, so the queue is swept
        // into the forwarder first; the read loops write it through after
        // send_registration's own NICK, so the retry is not clobbered
        // (#175).
        let (nick_tx, mut nick_rx) = mpsc::unbounded_channel::<String>();
        let mut index = 0;
        while index < pending_commands.len() {
            if let IrcCommand::Nick(nick) = &pending_commands[index] {
                let nick = nick.clone();
                pending_commands.remove(index);
                let _ = nick_tx.send(nick);
            } else {
                index += 1;
            }
        }
        let registration_result = tokio::select! {
            biased;
            _ = wait_for_registration_events(
                &mut outgoing_rx,
                &mut pending_commands,
                &nick_tx,
            ) => {
                return Err("Connection cancelled during registration".into());
            }
            result = self.register_with_deadline(
                &mut writer,
                &mut reader,
                &incoming_tx,
                &mut nick_rx,
                REGISTRATION_TIMEOUT,
            ) => result,
        };
        // NOTE: a NICK forwarded after the registration read loops have
        // returned (the CAP-END/001 tail) is dropped with nick_rx — the
        // window is sub-millisecond and the post-001 writer handles
        // everything still in outgoing_rx (#169 residual race, accepted).
        registration_result?;

        // Create channel for sending
        let (send_tx, mut send_rx) = mpsc::channel::<String>(100);
        self.tx = Some(send_tx);

        // Spawn writer task
        let mut writer_handle = tokio::spawn(async move {
            use tokio::time::{Duration, timeout};
            while let Some(cmd) = pending_commands.pop_front() {
                let should_close = matches!(cmd, IrcCommand::Quit(_));
                let line = outbound_line(&cmd);
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
                        let line = outbound_line(&cmd);
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
                            let line = outbound_line(&cmd);
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
        nick_rx: &mut mpsc::UnboundedReceiver<String>,
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
                self.send_registration(writer, reader, incoming_tx, nick_rx),
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
        nick_rx: &mut mpsc::UnboundedReceiver<String>,
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

        // One handshake buffer for the whole registration: a line dribbled
        // across the CAP→welcome phase boundary must not lose its head when
        // do_cap_negotiation returns (#179).
        let mut buf = Vec::with_capacity(512);
        let welcome_received = self
            .do_cap_negotiation(writer, reader, incoming_tx, nick_rx, &mut buf)
            .await?;
        if !welcome_received {
            self.wait_for_welcome(writer, reader, incoming_tx, nick_rx, &mut buf)
                .await?;
        }

        Ok(())
    }

    async fn wait_for_welcome<W, R>(
        &self,
        writer: &mut W,
        reader: &mut R,
        incoming_tx: &mpsc::Sender<IrcMessage>,
        nick_rx: &mut mpsc::UnboundedReceiver<String>,
        buf: &mut Vec<u8>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        W: tokio::io::AsyncWrite + Unpin,
        R: tokio::io::AsyncBufRead + Unpin,
    {
        loop {
            // read_handshake_msg_timed returns None for a PING (answered
            // inline) and for an unparseable line. Both are legal
            // pre-welcome traffic — bouncers PING unregistered clients — so
            // keep waiting instead of panicking. The outer
            // REGISTRATION_TIMEOUT bounds the total wait.
            let Some(msg) =
                read_handshake_msg_timed(writer, reader, buf, incoming_tx, nick_rx).await?
            else {
                continue;
            };
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
        nick_rx: &mut mpsc::UnboundedReceiver<String>,
        buf: &mut Vec<u8>,
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
        let mut available_caps = String::new();
        let mut cap_budget = CapBudget::default();

        loop {
            let Some(msg) =
                read_handshake_msg_timed(writer, reader, buf, incoming_tx, nick_rx).await?
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
                if subcmd.eq_ignore_ascii_case("LS") {
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
            && let Some((port, duration)) = sts_policy_target(sts, config.port)
        {
            update_sts_policy(&config.host, port, duration);
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
        // Caps already requested on a line of their own. A NAK rejects every
        // cap on its REQ line (IRCv3 is all-or-nothing), so a NAK of a
        // combined line retries each of its caps once, individually, and a
        // NAK of a lone cap is final. This bounds the retries to one REQ per
        // requested cap however a hostile server answers (#244).
        let mut requested_alone: std::collections::HashSet<String> = cap_reqs
            .iter()
            .filter_map(|line| {
                let mut caps = line
                    .trim_end()
                    .strip_prefix("CAP REQ :")?
                    .split_whitespace();
                let cap = caps.next()?;
                caps.next().is_none().then(|| cap.to_string())
            })
            .collect();
        loop {
            let Some(msg) =
                read_handshake_msg_timed(writer, reader, buf, incoming_tx, nick_rx).await?
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
                // Registration already completed: return at once (the
                // pre-001 state is exactly what the whole registration
                // machine above expects). SASL was left unattempted — say so
                // rather than failing silently (#182).
                if want_sasl {
                    let _ = incoming_tx.try_send(IrcMessage {
                        tags: None,
                        prefix: None,
                        command: IrcCommand::Notice(
                            "*".to_string(),
                            "SASL skipped: registration completed before SASL \
                             negotiation finished"
                                .to_string(),
                        ),
                        raw: String::new(),
                    });
                }
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
                        let mut retries: Vec<&str> = Vec::new();
                        for cap in caps.split_whitespace() {
                            let name = cap_token_name(cap);
                            // Only caps we asked for and still await a reply on.
                            if !pending_caps.contains(&name) {
                                continue;
                            }
                            if requested_alone.contains(&name) {
                                pending_caps.remove(&name);
                                rejected_caps.push(name);
                            } else if let Some(cap) =
                                caps_to_request.iter().copied().find(|cap| *cap == name)
                            {
                                requested_alone.insert(name);
                                retries.push(cap);
                            }
                        }
                        if !retries.is_empty() {
                            // SASL first: it gates authentication, so ask for it
                            // ahead of the optional caps.
                            retries.sort_by_key(|cap| *cap != "sasl");
                            tracing::info!(
                                "Server NAKed a combined CAP REQ; retrying individually: {}",
                                retries.join(" ")
                            );
                            for cap in retries {
                                let line = format!("CAP REQ :{cap}\r\n");
                                tracing::debug!("> {}", line.trim());
                                writer.write_all(line.as_bytes()).await?;
                            }
                            writer.flush().await?;
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
                self.do_sasl_auth_inner(writer, reader, incoming_tx, nick_rx, buf, &sasl_mechanisms)
                    .await?
            } else if want_sasl {
                let message = if !sasl_available {
                    "Server does not support SASL"
                } else if rejected_caps.iter().any(|cap| cap == "sasl") {
                    "Server rejected the SASL capability"
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
        // Registration tail, unified for both exits: the capability notice
        // and the pre-away AWAY always run (AWAY is an ordinary command and
        // stays valid post-001); PERSISTENCE needs the SASL account
        // established pre-001 and is skipped — with a Notice — when a raced
        // Welcome completed registration first or SASL did not authenticate;
        // CAP END is skipped post-001 (no-op or error depending on ircd)
        // (#179/#182).
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
        let skip_reason = "SASL authentication required";
        if acked_tokens.contains("draft/persistence")
            && let Some(profile) = config
                .persistence_profile
                .as_deref()
                .filter(|profile| !profile.is_empty())
        {
            if sasl_authenticated {
                let line = format!("PERSISTENCE ATTACH {}\r\n", profile);
                tracing::debug!("> PERSISTENCE ATTACH {}", profile);
                writer.write_all(line.as_bytes()).await?;
            } else {
                let _ = incoming_tx.try_send(IrcMessage {
                    tags: None,
                    prefix: None,
                    command: IrcCommand::Notice(
                        "*".to_string(),
                        format!("PERSISTENCE profile '{profile}' not attached: {skip_reason}"),
                    ),
                    raw: String::new(),
                });
            }
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
        if !welcome_received {
            let cap_end = "CAP END\r\n";
            tracing::debug!("> {}", cap_end.trim());
            writer.write_all(cap_end.as_bytes()).await?;
            writer.flush().await?;
        }

        Ok(welcome_received)
    }

    /// Try configured SASL mechanisms in preference order. A failed
    /// mechanism may be followed by another attempt without ending CAP.
    async fn do_sasl_auth_inner<W, R>(
        &self,
        writer: &mut W,
        reader: &mut R,
        incoming_tx: &mpsc::Sender<IrcMessage>,
        nick_rx: &mut mpsc::UnboundedReceiver<String>,
        buf: &mut Vec<u8>,
        mechanisms: &[SaslMechanism],
    ) -> Result<(bool, bool), Box<dyn std::error::Error + Send + Sync>>
    where
        W: tokio::io::AsyncWrite + Unpin,
        R: tokio::io::AsyncBufRead + Unpin,
    {
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
                .do_sasl_attempt(writer, reader, incoming_tx, nick_rx, buf, *mechanism)
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
        nick_rx: &mut mpsc::UnboundedReceiver<String>,
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
            .read_sasl_event(writer, reader, incoming_tx, nick_rx, buf)
            .await?
        {
            SaslServerEvent::Challenge(challenge) if challenge == "+" => {}
            SaslServerEvent::Success => return Ok(SaslAttemptOutcome::Success),
            SaslServerEvent::Welcome => return Ok(SaslAttemptOutcome::Welcome),
            SaslServerEvent::Challenge(_) => {
                // A malformed/unexpected challenge leaves the server-side
                // exchange live: abort it (consuming the acknowledgment so
                // the next mechanism's read does not mistake the stale 906
                // for its own) and fall through to the next mechanism
                // (#180).
                self.abort_sasl_exchange(writer).await?;
                return Ok(SaslAttemptOutcome::Failed);
            }
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

                let server_first: String = match self
                    .read_sasl_event(writer, reader, incoming_tx, nick_rx, buf)
                    .await?
                {
                    SaslServerEvent::Challenge(challenge) => {
                        // A malformed challenge (bad base64/UTF-8 or a
                        // server-first that fails validation) must not kill
                        // the whole connect: abort the live exchange and
                        // fall back to the next mechanism (#180). Base64 is
                        // decoded forgivingly — the client's own nonce is
                        // unpadded, so tolerate missing padding too.
                        let decoded = decode_base64_forgiving(&challenge)
                            .ok()
                            .and_then(|bytes| String::from_utf8(bytes).ok());
                        match decoded {
                            Some(server_first) => server_first,
                            None => {
                                self.abort_sasl_exchange(writer).await?;
                                return Ok(SaslAttemptOutcome::Failed);
                            }
                        }
                    }
                    SaslServerEvent::Welcome => {
                        return Ok(SaslAttemptOutcome::Welcome);
                    }
                    _ => return Ok(SaslAttemptOutcome::Failed),
                };
                let (client_final, signature) = match scram_sha256_response(
                    password,
                    &nonce,
                    &client_first_bare,
                    &server_first,
                ) {
                    Ok(result) => result,
                    Err(error) => {
                        tracing::warn!(
                            "SCRAM server-first rejected: {error}; aborting and \
                             trying the next mechanism"
                        );
                        self.abort_sasl_exchange(writer).await?;
                        return Ok(SaslAttemptOutcome::Failed);
                    }
                };
                expected_server_signature = Some(signature);
                Self::write_sasl_payload(writer, client_final.as_bytes()).await?;
            }
            SaslMechanism::Auto => unreachable!(),
        }

        let mut server_verified = expected_server_signature.is_none();
        loop {
            match self
                .read_sasl_event(writer, reader, incoming_tx, nick_rx, buf)
                .await?
            {
                SaslServerEvent::Challenge(challenge) => {
                    let Some(expected) = expected_server_signature
                        .as_deref()
                        .filter(|_| !server_verified)
                    else {
                        // Out-of-protocol extra challenge (a second one, or
                        // any after the server signature verified): abort
                        // the live exchange before falling back (#176/#181).
                        self.abort_sasl_exchange(writer).await?;
                        return Ok(SaslAttemptOutcome::Failed);
                    };
                    let decoded = decode_base64_forgiving(&challenge)
                        .ok()
                        .and_then(|bytes| String::from_utf8(bytes).ok());
                    let supplied = decoded
                        .as_deref()
                        .and_then(|value| value.split(',').find_map(|part| part.strip_prefix("v=")))
                        .and_then(|value| decode_base64_forgiving(value).ok());
                    server_verified = supplied
                        .as_deref()
                        .is_some_and(|actual| constant_time_eq(actual, expected));
                    if !server_verified {
                        self.abort_sasl_exchange(writer).await?;
                        return Ok(SaslAttemptOutcome::Failed);
                    }
                    // IRCv3 SASL 3.1: when the mechanism ends with a non-empty
                    // server challenge (SCRAM's server-final message) the
                    // client MUST still send an empty response; spec-following
                    // servers hold the 903 until it arrives (#230).
                    Self::write_sasl_payload(writer, b"").await?;
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

    /// Abort a live SASL exchange (AUTHENTICATE *). The server's 906
    /// ERR_SASLABORTED acknowledgment is skipped by read_sasl_event — it can
    /// only acknowledge this client-initiated abort — so no blind ack read
    /// is needed here (one risked swallowing a late 903 success or a
    /// Welcome; #181).
    async fn abort_sasl_exchange<W>(
        &self,
        writer: &mut W,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        writer.write_all(b"AUTHENTICATE *\r\n").await?;
        writer.flush().await?;
        Ok(())
    }

    async fn read_sasl_event<W, R>(
        &self,
        writer: &mut W,
        reader: &mut R,
        incoming_tx: &mpsc::Sender<IrcMessage>,
        nick_rx: &mut mpsc::UnboundedReceiver<String>,
        buf: &mut Vec<u8>,
    ) -> Result<SaslServerEvent, Box<dyn std::error::Error + Send + Sync>>
    where
        W: tokio::io::AsyncWrite + Unpin,
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let mut challenge = SaslChallengeAccumulator::default();
        loop {
            let Some(msg) =
                read_handshake_msg_timed(writer, reader, buf, incoming_tx, nick_rx).await?
            else {
                // Step budget expired mid-exchange: abort it so the server
                // stops composing the late reply — otherwise the next
                // mechanism's exchange interleaves with this one's (#179).
                self.abort_sasl_exchange(writer).await?;
                return Ok(SaslServerEvent::Failed);
            };
            match &msg.command {
                IrcCommand::Authenticate(data) => {
                    if let Some(complete) = challenge.feed(data) {
                        return Ok(SaslServerEvent::Challenge(complete));
                    }
                    if challenge.pending_len() > MAX_SASL_CHALLENGE_LEN {
                        self.abort_sasl_exchange(writer).await?;
                        return Ok(SaslServerEvent::Failed);
                    }
                    continue;
                }
                IrcCommand::Numeric(RPL_WELCOME, _) => return Ok(SaslServerEvent::Welcome),
                IrcCommand::Numeric(RPL_SASLSUCCESS, _) => return Ok(SaslServerEvent::Success),
                // 906 can only acknowledge this client's own abort: skip it
                // so the stale ack cannot fail the next mechanism (#181).
                IrcCommand::Numeric(ERR_SASLABORTED, _) => continue,
                IrcCommand::Numeric(
                    RPL_NICKLOCKED | ERR_SASLFAIL | ERR_SASLTOOLONG | ERR_SASLALREADY,
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
        let mut sts_gate = LiveStsGate::default();
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
                                    && let Some((port, duration)) =
                                        sts_policy_target(sts, self.config.port)
                                    && sts_gate.admit(port, duration)
                                {
                                    update_sts_policy(&self.config.host, port, duration);
                                }
                                // Handle PING automatically. Awaiting the send
                                // (instead of try_send) means a saturated writer
                                // queue delays the PONG rather than silently
                                // dropping it and risking a server ping-timeout.
                                if let IrcCommand::Ping(server) = &msg.command
                                    && let Some(ref tx) = self.tx
                                    && tx.send(pong_line(server)).await.is_err()
                                {
                                    tracing::warn!("Writer task gone; cannot send PONG");
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
            "PRIVMSG Q@CServe.quakenet.org :<redacted>"
        );
    }

    #[test]
    fn debug_log_redaction_covers_credential_verbs_and_service_targets() {
        // Whole-line verbs.
        for (line, expected) in [
            ("PASS hunter2", "PASS <redacted>"),
            ("pass hunter2", "pass <redacted>"),
            ("OPER admin hunter2", "OPER <redacted>"),
            ("WEBPUSH REGISTER endpoint hunter2", "WEBPUSH <redacted>"),
            (
                "REGISTER acct me@example.test hunter2",
                "REGISTER <redacted>",
            ),
            ("VERIFY acct 123456", "VERIFY <redacted>"),
            ("AUTHENTICATE dGVzdA==", "AUTHENTICATE <redacted>"),
            ("IDENTIFY hunter2", "IDENTIFY <redacted>"),
            ("GHOST oldnick hunter2", "GHOST <redacted>"),
            ("NS identify hunter2", "NS <redacted>"),
            ("CHANSERV identify #chan hunter2", "CHANSERV <redacted>"),
            ("TOKEN VALIDATE abc.def", "TOKEN <redacted>"),
            ("@label=1 PASS hunter2", "PASS <redacted>"),
        ] {
            assert_eq!(redact_for_log(&format!("{line}\r\n")), expected, "{line}");
        }
        // Any message to something service-shaped, whatever the text.
        for (line, expected) in [
            (
                "PRIVMSG NickServ :identify hunter2",
                "PRIVMSG NickServ :<redacted>",
            ),
            (
                "privmsg nickserv :hunter2 hunter2",
                "privmsg nickserv :<redacted>",
            ),
            ("PRIVMSG NS :id hunter2", "PRIVMSG NS :<redacted>"),
            (
                "NOTICE ChanServ :set #c password x",
                "NOTICE ChanServ :<redacted>",
            ),
            (
                "SQUERY NickServ :identify hunter2",
                "SQUERY NickServ :<redacted>",
            ),
            (
                "PRIVMSG AuthServ@services.example :auth u hunter2",
                "PRIVMSG AuthServ@services.example :<redacted>",
            ),
            (
                "PRIVMSG X@channels.undernet.org :login u hunter2",
                "PRIVMSG X@channels.undernet.org :<redacted>",
            ),
            (
                "PRIVMSG @NickServ :identify hunter2",
                "PRIVMSG @NickServ :<redacted>",
            ),
            (
                "PRIVMSG #chan,NickServ :identify hunter2",
                "PRIVMSG #chan,NickServ :<redacted>",
            ),
            (
                "@label=1 PRIVMSG HostServ :request hunter2",
                "PRIVMSG HostServ :<redacted>",
            ),
            (
                "PRIVMSG MemoServ :send bob hunter2",
                "PRIVMSG MemoServ :<redacted>",
            ),
            (
                "PRIVMSG BotServ :say hunter2",
                "PRIVMSG BotServ :<redacted>",
            ),
        ] {
            assert_eq!(redact_for_log(&format!("{line}\r\n")), expected, "{line}");
        }
        // Ordinary traffic is logged as-is.
        for line in [
            "PRIVMSG #chan :identify the culprit",
            "PRIVMSG alice :hello",
            "NOTICE alice :pass the salt",
            "JOIN #chan",
            "AUTHENTICATE PLAIN",
            "AUTHENTICATE +",
            "TOKEN LIST",
            "PASSWORD",
            "@label=1 PRIVMSG #chan :hi",
        ] {
            assert_eq!(redact_for_log(&format!("{line}\r\n")), line, "{line}");
        }
    }

    #[test]
    fn sts_policy_target_uses_advertised_port_not_connection_port() {
        // #186: TLS CAP LS / CAP NEW advertising a different port must pin
        // that port, not the port already in use.
        assert_eq!(
            sts_policy_target("port=6698,duration=7776000", 6697),
            Some((6698, 7776000))
        );
        assert_eq!(
            sts_policy_target("duration=7776000", 6697),
            Some((6697, 7776000))
        );
        assert_eq!(sts_policy_target("port=6698", 6697), None);
    }

    /// Feed `wire` to `read_loop` as an established TLS connection to
    /// irc.example.test:6697 and return the STS policy writes it requested.
    fn live_sts_policy_writes(wire: &str) -> Vec<(String, u16, u64)> {
        STS_POLICY_WRITES.with_borrow_mut(Vec::clear);
        test_runtime().block_on(async {
            let mut client = IrcClient::new(ServerConfig {
                host: "irc.example.test".into(),
                port: 6697,
                use_tls: true,
                ..Default::default()
            });
            // The channel must outlive the loop (a dropped receiver stops it)
            // and hold every forwarded message so the loop never blocks.
            let (msg_tx, _msg_rx) = mpsc::channel(1024);
            client.read_loop(wire.as_bytes(), msg_tx).await;
        });
        STS_POLICY_WRITES.with_borrow(Clone::clone)
    }

    #[test]
    fn repeated_identical_cap_new_sts_persists_the_policy_once() {
        // #280: every repeat used to cost a policy-file read, write and
        // rename inside the read loop.
        let line = ":srv CAP tester NEW :sts=port=6697,duration=86400\r\n";
        let writes = live_sts_policy_writes(&line.repeat(200));
        assert_eq!(writes.len(), 1, "policy written once per repeat");
        assert_eq!(writes[0], ("irc.example.test".to_string(), 6697, 86400));
    }

    #[test]
    fn changed_cap_new_sts_policy_is_still_persisted() {
        // The dedup must not swallow a real change, including the duration=0
        // removal that clears the policy.
        let wire = [86400, 86400, 0, 0]
            .iter()
            .map(|duration| format!(":srv CAP tester NEW :sts=port=6697,duration={duration}\r\n"))
            .collect::<String>();
        assert_eq!(
            live_sts_policy_writes(&wire),
            [
                ("irc.example.test".to_string(), 6697, 86400),
                ("irc.example.test".to_string(), 6697, 0),
            ]
        );
    }

    #[test]
    fn cap_new_sts_churn_cannot_force_unbounded_policy_writes() {
        // #280: a hostile server varying the value defeats plain dedup.
        let wire = (0..500)
            .map(|i| {
                format!(
                    ":srv CAP tester NEW :sts=port=6697,duration={}\r\n",
                    1000 + i
                )
            })
            .collect::<String>();
        let writes = live_sts_policy_writes(&wire);
        assert!(!writes.is_empty());
        assert!(
            writes.len() <= MAX_LIVE_STS_UPDATES,
            "{} policy writes for 500 CAP NEW lines",
            writes.len()
        );
    }

    #[test]
    fn sasl_challenge_chunks_are_reassembled_before_decoding() {
        // #187: a 400-byte chunk is not complete; the short tail ends it.
        let mut acc = SaslChallengeAccumulator::default();
        assert_eq!(acc.feed(&"A".repeat(400)), None);
        let complete = acc.feed("xyz").unwrap();
        assert_eq!(complete.len(), 403);
        assert!(complete.ends_with("xyz"));
        // Exact multiple of 400 is terminated by `+`.
        let mut acc = SaslChallengeAccumulator::default();
        assert_eq!(acc.feed(&"B".repeat(400)), None);
        assert_eq!(acc.feed("+").unwrap(), "B".repeat(400));
        // A lone `+` keeps its empty-payload sentinel meaning, and a short
        // single chunk completes immediately.
        let mut acc = SaslChallengeAccumulator::default();
        assert_eq!(acc.feed("+").unwrap(), "+");
        assert_eq!(acc.feed("abc").unwrap(), "abc");
    }

    #[test]
    fn sasl_chunked_server_challenge_is_returned_whole() {
        test_runtime().block_on(async {
            // #187 end-to-end through read_sasl_event over a duplex stream.
            let payload = BASE64_STANDARD
                .encode(format!("r=nonce,s=c2FsdA==,i=4096,x={}", "a".repeat(500)).as_bytes());
            assert!(payload.len() > 400);
            let (first, rest) = payload.split_at(400);
            let wire = format!("AUTHENTICATE {first}\r\nAUTHENTICATE {rest}\r\n");
            let (client_io, server_io) = tokio::io::duplex(8192);
            let (client_read, mut client_write) = tokio::io::split(client_io);
            let mut client_read = BufReader::new(client_read);
            let (_server_read, mut server_write) = tokio::io::split(server_io);
            let server = tokio::spawn(async move {
                server_write.write_all(wire.as_bytes()).await.unwrap();
                server_write.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(200)).await;
            });
            let client = IrcClient::new(ServerConfig::default());
            let (msg_tx, mut msg_rx) = mpsc::channel(100);
            let drain = tokio::spawn(async move { while msg_rx.recv().await.is_some() {} });
            let (_nick_tx, mut nick_rx) = mpsc::unbounded_channel::<String>();
            let mut buf = Vec::new();
            let event = client
                .read_sasl_event(
                    &mut client_write,
                    &mut client_read,
                    &msg_tx,
                    &mut nick_rx,
                    &mut buf,
                )
                .await
                .unwrap();
            match event {
                SaslServerEvent::Challenge(challenge) => assert_eq!(challenge, payload),
                _ => panic!("expected reassembled challenge"),
            }
            drop(msg_tx);
            drain.await.unwrap();
            server.await.unwrap();
        });
    }

    #[test]
    fn handshake_line_survives_a_nick_arriving_while_it_is_forwarded() {
        test_runtime().block_on(async {
            // #277: the line is consumed from the socket before it is
            // forwarded to the GUI. If the forward is blocked (full channel)
            // and a queued NICK wakes the select, the forward must still
            // finish; dropping it would lose the line (CAP LS, 001, ...) and
            // stall the handshake.
            // The pipe holds exactly one line, so a second write completes
            // only once the reader has taken that line off it. Awaiting that
            // second write orders the test after the reader has consumed the
            // line and, in this single-threaded runtime, parked itself on the
            // full channel, with no sleep to guess how long that takes.
            const LINE: &[u8] = b":srv CAP * LS :multi-prefix\r\n";
            let (client_io, server_io) = tokio::io::duplex(LINE.len());
            let (client_read, mut client_write) = tokio::io::split(client_io);
            let mut client_read = BufReader::new(client_read);
            let (_server_read, mut server_write) = tokio::io::split(server_io);
            let (incoming_tx, mut incoming_rx) = mpsc::channel(1);
            incoming_tx
                .send(IrcMessage {
                    tags: None,
                    prefix: None,
                    command: IrcCommand::Notice("*".to_string(), "filler".to_string()),
                    raw: String::new(),
                })
                .await
                .unwrap();
            let (nick_tx, mut nick_rx) = mpsc::unbounded_channel::<String>();
            server_write.write_all(LINE).await.unwrap();

            let reader_task = tokio::spawn(async move {
                let mut buf = Vec::new();
                read_handshake_msg_timed(
                    &mut client_write,
                    &mut client_read,
                    &mut buf,
                    &incoming_tx,
                    &mut nick_rx,
                )
                .await
                .map_err(|error| error.to_string())
            });
            // Completes once the reader has consumed the line, which leaves
            // it blocked forwarding that line to the full channel. The byte
            // is never read as part of a line: the test ends first.
            server_write.write_all(b"x").await.unwrap();
            // Wake it with a NICK, and let every task that wakes run.
            nick_tx.send("tester_".to_string()).unwrap();
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert!(matches!(
                incoming_rx.recv().await.unwrap().command,
                IrcCommand::Notice(..)
            ));

            let message = timeout(Duration::from_secs(1), reader_task)
                .await
                .expect("handshake line lost: its forward was cancelled")
                .unwrap()
                .unwrap()
                .expect("expected the CAP LS message");
            assert!(matches!(message.command, IrcCommand::Cap(..)));
            let forwarded = timeout(Duration::from_secs(1), incoming_rx.recv())
                .await
                .expect("CAP LS was never forwarded")
                .unwrap();
            assert!(matches!(forwarded.command, IrcCommand::Cap(..)));
            drop(server_write);
        });
    }

    #[test]
    fn sts_probe_drip_feed_is_bounded_by_one_overall_deadline() {
        test_runtime().block_on(async {
            // #189: PINGs arriving faster than the per-line timeout must not
            // keep re-arming the probe past its overall budget.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                // Drip PINGs well inside any per-line budget, indefinitely;
                // also consume the client's PONGs so writes never block.
                let drip = tokio::spawn(async move {
                    loop {
                        if write_half.write_all(b":srv PING :drip\r\n").await.is_err() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                });
                let mut line = String::new();
                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    line.clear();
                }
                drip.abort();
            });
            let (incoming_tx, _incoming_rx) = mpsc::channel(100);
            let (_outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel();
            let mut pending = VecDeque::new();
            let started = tokio::time::Instant::now();
            let port = timeout(
                Duration::from_secs(5),
                IrcClient::probe_sts_upgrade_with_timeout(
                    "127.0.0.1",
                    address.port(),
                    &incoming_tx,
                    &mut outgoing_rx,
                    &mut pending,
                    Duration::from_millis(300),
                ),
            )
            .await
            .expect("STS probe was not bounded by its overall deadline")
            .unwrap();
            assert_eq!(port, None);
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "probe took {:?}",
                started.elapsed()
            );
            server.abort();
        });
    }

    #[test]
    fn lowercase_cap_ls_still_negotiates() {
        test_runtime().block_on(async {
            // #188: CAP subcommands are case-insensitive.
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
                    .write_all(b":srv CAP * ls :multi-prefix\r\n")
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
            timeout(Duration::from_secs(2), client.connect(msg_tx, cmd_rx))
                .await
                .expect("lowercase CAP ls stalled negotiation")
                .unwrap();
            server.await.unwrap();
        });
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

    /// A PING whose token smuggles a second command after a bare CR (#281).
    const CR_INJECTION_PING: &str = ":srv PING :token\rOPER attacker password\r\n";
    /// The only PONG that is safe to send for it: one line, no interior CR.
    const SANITIZED_PONG: &[u8] = b"PONG :tokenOPER attacker password\r\n";

    #[test]
    fn handshake_pong_does_not_reflect_a_carriage_return_from_the_ping_token() {
        test_runtime().block_on(async {
            let (incoming_tx, _incoming_rx) = mpsc::channel(10);
            let mut buf = CR_INJECTION_PING.as_bytes().to_vec();
            let mut written = Vec::new();
            handle_handshake_line(&mut written, &mut buf, &incoming_tx)
                .await
                .unwrap();
            assert_eq!(written, SANITIZED_PONG);
        });
    }

    #[test]
    fn read_loop_pong_does_not_reflect_a_carriage_return_from_the_ping_token() {
        test_runtime().block_on(async {
            let mut client = IrcClient::new(ServerConfig::default());
            let (send_tx, mut send_rx) = mpsc::channel(10);
            client.tx = Some(send_tx);
            let (msg_tx, _msg_rx) = mpsc::channel(10);
            client.read_loop(CR_INJECTION_PING.as_bytes(), msg_tx).await;
            let pong = send_rx.try_recv().expect("PING went unanswered");
            assert_eq!(pong.as_bytes(), SANITIZED_PONG);
        });
    }

    #[test]
    fn sts_probe_pong_does_not_reflect_a_carriage_return_from_the_ping_token() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                write_half
                    .write_all(CR_INJECTION_PING.as_bytes())
                    .await
                    .unwrap();
                let mut pong = Vec::new();
                reader.read_until(b'\n', &mut pong).await.unwrap();
                write_half
                    .write_all(b":srv CAP * LS :sts=port=6697\r\n")
                    .await
                    .unwrap();
                pong
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
            assert_eq!(server.await.unwrap(), SANITIZED_PONG);
        });
    }

    #[test]
    fn sts_probe_ignores_a_truncated_final_line() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (_, mut write_half) = stream.into_split();
                // No trailing CRLF: an EOF-truncated fragment must never be
                // parsed as a complete CAP LS reply.
                write_half
                    .write_all(b":srv CAP * LS :sts=port=6697")
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
            assert_eq!(port, None);
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
                assert_eq!(read_wire_line(&mut reader).await, "PASS :server-pass");
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
                // Hand the queued line back to the test body instead of
                // asserting here: the test joins this task, so a command that
                // never reaches the wire fails the test (#276) rather than
                // being masked by an aborted task.
                timeout(Duration::from_secs(2), read_wire_line(&mut reader))
                    .await
                    .ok()
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

            let queued = server.await.expect("fake server task failed");
            assert_eq!(
                queued.as_deref(),
                Some("PRIVMSG #chan :queued during connect"),
                "command queued during connect never reached the wire"
            );
            client_task.abort();
        });
    }

    #[test]
    fn server_password_reaches_the_wire_as_one_trailing_parameter() {
        test_runtime().block_on(async {
            // #231: spaces and a leading ':' must not be cut or eaten by the
            // server's parameter parsing.
            for password in ["my secret", ":abc"] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (read_half, mut write_half) = stream.into_split();
                    let mut reader = BufReader::new(read_half);
                    assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                    let pass = read_wire_line(&mut reader).await;
                    assert_eq!(read_wire_line(&mut reader).await, "NICK tester");
                    assert!(read_wire_line(&mut reader).await.starts_with("USER "));
                    write_half
                        .write_all(b":srv 001 tester :Welcome\r\n")
                        .await
                        .unwrap();
                    pass
                });
                let config = ServerConfig {
                    host: "127.0.0.1".into(),
                    port: addr.port(),
                    use_tls: false,
                    nick: "tester".into(),
                    password: Some(password.into()),
                    ..Default::default()
                };
                let (msg_tx, _msg_rx) = mpsc::channel(100);
                let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
                let mut client = IrcClient::new(config);
                timeout(Duration::from_secs(2), client.connect(msg_tx, cmd_rx))
                    .await
                    .expect("registration with a server password timed out")
                    .unwrap();
                assert_eq!(server.await.unwrap(), format!("PASS :{password}"));
            }
        });
    }

    #[test]
    fn invalid_nick_is_rejected_before_connecting() {
        test_runtime().block_on(async {
            // ":alice" is sent as "NICK :alice", which the server reads as a
            // trailing parameter and registers "alice" (#303).
            for nick in [
                "",
                "a b",
                "a\rb",
                "a@b",
                "a\nb",
                "a\0b",
                ":alice",
                ":",
                &"x".repeat(501),
            ] {
                let config = ServerConfig {
                    host: "127.0.0.1".into(),
                    port: 6697,
                    use_tls: false,
                    nick: nick.to_string(),
                    ..Default::default()
                };
                let (msg_tx, _msg_rx) = mpsc::channel(100);
                let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
                let error = IrcClient::new(config)
                    .connect(msg_tx, cmd_rx)
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("ickname"), "{nick}: {error}");
            }
            // Username/realname/password rules (#179/#180): each row
            // isolates one branch.
            for (username, realname, password) in [
                ("us er", "", ""),
                (":oper", "", ""),
                ("a\rb", "", ""),
                ("a\nb", "", ""),
                ("a\0b", "", ""),
                ("ok", &"r".repeat(401), ""),
                (&"u".repeat(300), &"r".repeat(300), ""),
                ("ok", "", "p\r\nOPER x y"),
            ] {
                let config = ServerConfig {
                    host: "127.0.0.1".into(),
                    port: 6697,
                    use_tls: false,
                    nick: "tester".into(),
                    username: username.to_string(),
                    realname: realname.to_string(),
                    password: Some(password.to_string()),
                    ..Default::default()
                };
                let (msg_tx, _msg_rx) = mpsc::channel(100);
                let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
                let error = IrcClient::new(config)
                    .connect(msg_tx, cmd_rx)
                    .await
                    .unwrap_err();
                assert!(
                    error.to_string().contains("must") || error.to_string().contains("too long"),
                    "{username}/{realname}/{password}: {error}"
                );
            }
        });
    }

    #[test]
    fn combined_user_line_must_fit_the_irc_line_limit() {
        // #232: each field is capped at 400 bytes on its own, but the
        // formatted "USER <u> 0 * :<r>\r\n" must also fit in 512 bytes, i.e.
        // username + realname <= 499 bytes.
        let check = |username_len: usize, realname_len: usize| {
            IrcClient::validate_registration_fields(
                "tester",
                &"u".repeat(username_len),
                &"r".repeat(realname_len),
                "",
            )
        };
        assert!(check(250, 249).is_ok());
        assert!(check(300, 199).is_ok());
        assert!(check(1, 400).is_ok());
        for (username_len, realname_len) in [(250, 250), (300, 300), (400, 400)] {
            let error = check(username_len, realname_len).unwrap_err();
            assert!(error.contains("too long"), "{error}");
        }
        // The accepted boundary really is a legal wire line.
        let user = IrcCommand::User {
            username: "u".repeat(250),
            realname: "r".repeat(249),
        };
        assert_eq!(format!("{user}\r\n").len(), IRC_WIRE_LINE_BYTES);
        // Multi-byte realnames count bytes, not characters.
        assert!(
            IrcClient::validate_registration_fields(
                "tester",
                &"u".repeat(100),
                &"é".repeat(200),
                ""
            )
            .is_err()
        );
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
            let (_nick_tx, mut nick_rx) = mpsc::unbounded_channel::<String>();
            let drain = tokio::spawn(async move { while msg_rx.recv().await.is_some() {} });
            let error = client
                .register_with_deadline(
                    &mut client_write,
                    &mut client_read,
                    &msg_tx,
                    &mut nick_rx,
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
    fn welcome_wait_survives_pings_and_junk_lines() {
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
                // Empty LS -> client sends CAP END and waits for welcome.
                write_half.write_all(b":srv CAP * LS :\r\n").await.unwrap();
                assert_eq!(read_wire_line(&mut reader).await, "CAP END");
                // Both legal pre-welcome lines used to reach the `.expect`:
                // a bouncer-style PING (answered inline, returns None) and a
                // line that parses to None.
                write_half.write_all(b":srv PING :ka\r\n").await.unwrap();
                assert_eq!(read_wire_line(&mut reader).await, "PONG :ka");
                write_half.write_all(b"   \r\n").await.unwrap();
                write_half
                    .write_all(b":legacy 001 legacy :Welcome\r\n")
                    .await
                    .unwrap();
                // Hold the socket open long enough for the client to read.
                tokio::time::sleep(Duration::from_millis(100)).await;
            });

            let config = ServerConfig {
                host: "127.0.0.1".into(),
                port: addr.port(),
                use_tls: false,
                nick: "legacy".into(),
                ..Default::default()
            };
            let (msg_tx, mut msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            timeout(Duration::from_secs(1), client.connect(msg_tx, cmd_rx))
                .await
                .expect("registration did not survive pre-welcome PING/junk")
                .unwrap();
            assert!(
                std::iter::from_fn(|| msg_rx.try_recv().ok())
                    .any(|msg| matches!(msg.command, IrcCommand::Numeric(RPL_WELCOME, _)))
            );
            server.await.unwrap();
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

    fn authenticate_payload(line: &str) -> String {
        let encoded = line
            .strip_prefix("AUTHENTICATE ")
            .unwrap_or_else(|| panic!("expected AUTHENTICATE, got {line:?}"));
        String::from_utf8(BASE64_STANDARD.decode(encoded).unwrap()).unwrap()
    }

    #[derive(Clone, Copy, PartialEq)]
    enum ScramServerMode {
        Normal,
        ForgedSignature,
        RepeatedFinal,
    }

    /// Scripted SCRAM-SHA-256 server: CAP/SASL negotiation, then the full
    /// RFC 5802/7677 exchange with the server side computed independently of
    /// `scram_sha256_response` (verifying the client proof and deriving the
    /// server signature). Like a spec-following IRCv3 server it waits for the
    /// client's empty `AUTHENTICATE +` after the server-final message before
    /// reporting 903. Returns the line the client sent in answer to the
    /// server-final message (or a marker if it sent nothing in time); in
    /// `RepeatedFinal` mode, the line it sent in answer to the repeat.
    async fn scram_fake_server(
        listener: tokio::net::TcpListener,
        password: String,
        mode: ScramServerMode,
    ) -> String {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
        assert_eq!(read_wire_line(&mut reader).await, "NICK tester");
        assert!(read_wire_line(&mut reader).await.starts_with("USER "));
        write_half
            .write_all(b":srv CAP * LS :sasl=PLAIN,SCRAM-SHA-256\r\n")
            .await
            .unwrap();
        assert_eq!(read_wire_line(&mut reader).await, "CAP REQ :sasl");
        write_half
            .write_all(b":srv CAP * ACK :sasl\r\n")
            .await
            .unwrap();
        assert_eq!(
            read_wire_line(&mut reader).await,
            "AUTHENTICATE SCRAM-SHA-256"
        );
        write_half.write_all(b"AUTHENTICATE +\r\n").await.unwrap();

        let client_first = authenticate_payload(&read_wire_line(&mut reader).await);
        let client_first_bare = client_first.strip_prefix("n,,").unwrap().to_string();
        assert!(client_first_bare.starts_with("n=user,r="));
        let client_nonce = client_first_bare.split_once(",r=").unwrap().1;
        let salt = b"0123456789abcdef";
        let server_first = format!(
            "r={client_nonce}srvnonce,s={},i=4096",
            BASE64_STANDARD.encode(salt)
        );
        let line = format!(
            "AUTHENTICATE {}\r\n",
            BASE64_STANDARD.encode(server_first.as_bytes())
        );
        write_half.write_all(line.as_bytes()).await.unwrap();

        let client_final = authenticate_payload(&read_wire_line(&mut reader).await);
        let (without_proof, proof) = client_final.rsplit_once(",p=").unwrap();
        assert_eq!(without_proof, format!("c=biws,r={client_nonce}srvnonce"));
        let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
        let mut salted = [0u8; 32];
        pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, 4096, &mut salted);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let client_signature = hmac_sha256(&Sha256::digest(&client_key), auth_message.as_bytes());
        let expected_proof: Vec<u8> = client_key
            .iter()
            .zip(client_signature)
            .map(|(key, signature)| key ^ signature)
            .collect();
        assert_eq!(proof, BASE64_STANDARD.encode(expected_proof));

        let mut server_signature = hmac_sha256(
            &hmac_sha256(&salted, b"Server Key"),
            auth_message.as_bytes(),
        );
        if mode == ScramServerMode::ForgedSignature {
            server_signature[0] ^= 0x01;
        }
        let server_final = format!("v={}", BASE64_STANDARD.encode(server_signature));
        let line = format!(
            "AUTHENTICATE {}\r\n",
            BASE64_STANDARD.encode(server_final.as_bytes())
        );
        write_half.write_all(line.as_bytes()).await.unwrap();
        if mode == ScramServerMode::RepeatedFinal {
            write_half.write_all(line.as_bytes()).await.unwrap();
        }

        // IRCv3 SASL 3.1: the client must answer the non-empty final
        // challenge with an empty response; only then does the server finish.
        let Ok(response) = timeout(Duration::from_secs(1), read_wire_line(&mut reader)).await
        else {
            return "<no response to the server-final message>".to_string();
        };
        if mode == ScramServerMode::RepeatedFinal {
            // Only the first server-final is acknowledged; the repeat is out
            // of protocol and must abort the exchange, not draw a second '+'.
            assert_eq!(response, "AUTHENTICATE +");
            return timeout(Duration::from_secs(1), read_wire_line(&mut reader))
                .await
                .unwrap_or_else(|_| "<no response to the repeated server-final>".to_string());
        }
        if response == "AUTHENTICATE +" {
            write_half
                .write_all(b":srv 903 tester :SASL authentication successful\r\n")
                .await
                .unwrap();
            assert_eq!(read_wire_line(&mut reader).await, "CAP END");
            write_half
                .write_all(b":srv 001 tester :Welcome\r\n")
                .await
                .unwrap();
        }
        response
    }

    fn scram_test_config(port: u16) -> ServerConfig {
        ServerConfig {
            host: "127.0.0.1".into(),
            port,
            use_tls: false,
            nick: "tester".into(),
            sasl_username: Some("user".into()),
            sasl_password: Some("pencil".into()),
            sasl_mechanism: SaslMechanism::ScramSha256,
            sasl_required: true,
            ..Default::default()
        }
    }

    #[test]
    fn scram_sha256_exchange_sends_the_empty_ack_before_903() {
        test_runtime().block_on(async {
            // #230: the full exchange against a server that holds 903 until
            // it receives the empty AUTHENTICATE + after server-final.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(scram_fake_server(
                listener,
                "pencil".into(),
                ScramServerMode::Normal,
            ));
            let (msg_tx, mut msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(scram_test_config(addr.port()));
            let result = timeout(Duration::from_secs(5), client.connect(msg_tx, cmd_rx))
                .await
                .expect("SCRAM registration timed out");
            let response = server.await.unwrap();
            assert_eq!(
                response, "AUTHENTICATE +",
                "client must send an empty AUTHENTICATE + after the SCRAM server-final message"
            );
            result.unwrap();
            assert!(
                std::iter::from_fn(|| msg_rx.try_recv().ok()).any(|msg| matches!(
                    msg.command,
                    IrcCommand::Notice(_, ref text) if text.contains("SCRAM-SHA-256 authentication successful")
                ))
            );
        });
    }

    #[test]
    fn scram_sha256_bad_server_signature_aborts_without_the_ack() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(scram_fake_server(
                listener,
                "pencil".into(),
                ScramServerMode::ForgedSignature,
            ));
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(scram_test_config(addr.port()));
            let result = timeout(Duration::from_secs(5), client.connect(msg_tx, cmd_rx))
                .await
                .expect("SCRAM registration timed out");
            // A forged server signature must abort the exchange, never be
            // acknowledged as a success.
            assert_eq!(server.await.unwrap(), "AUTHENTICATE *");
            let error = result.unwrap_err();
            assert!(
                error.to_string().contains("Required SASL"),
                "unexpected error: {error}"
            );
        });
    }

    #[test]
    fn scram_sha256_acknowledges_the_server_final_message_only_once() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(scram_fake_server(
                listener,
                "pencil".into(),
                ScramServerMode::RepeatedFinal,
            ));
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(scram_test_config(addr.port()));
            let result = timeout(Duration::from_secs(5), client.connect(msg_tx, cmd_rx))
                .await
                .expect("SCRAM registration timed out");
            assert_eq!(server.await.unwrap(), "AUTHENTICATE *");
            assert!(result.is_err());
        });
    }

    #[test]
    fn cap_nak_of_a_combined_request_retries_each_cap_so_sasl_still_runs() {
        test_runtime().block_on(async {
            // #244: this server cannot enable away-notify and NAKs every REQ
            // containing it. The combined REQ is NAKed as a whole, which must
            // not take multi-prefix and SASL down with it.
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
                    .write_all(b":srv CAP * LS :multi-prefix away-notify sasl=PLAIN\r\n")
                    .await
                    .unwrap();
                let mut requests = Vec::new();
                let next = loop {
                    let line = read_wire_line(&mut reader).await;
                    let Some(caps) = line.strip_prefix("CAP REQ :") else {
                        break line;
                    };
                    let verb = if caps.split_whitespace().any(|cap| cap == "away-notify") {
                        "NAK"
                    } else {
                        "ACK"
                    };
                    write_half
                        .write_all(format!(":srv CAP * {verb} :{caps}\r\n").as_bytes())
                        .await
                        .unwrap();
                    requests.push(caps.to_string());
                };
                if next == "AUTHENTICATE PLAIN" {
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
                }
                (requests, next)
            });

            let config = ServerConfig {
                host: "127.0.0.1".into(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".into(),
                sasl_username: Some("user".into()),
                sasl_password: Some("password".into()),
                sasl_required: true,
                ..Default::default()
            };
            let (msg_tx, mut msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            let result = timeout(Duration::from_secs(5), client.connect(msg_tx, cmd_rx))
                .await
                .expect("registration after a CAP NAK timed out");
            let (requests, next) = server.await.unwrap();
            assert_eq!(requests[0], "multi-prefix away-notify sasl");
            // SASL, the critical cap, is retried first.
            assert_eq!(requests[1..], ["sasl", "multi-prefix", "away-notify"]);
            assert_eq!(next, "AUTHENTICATE PLAIN");
            result.unwrap();
            let notices: Vec<String> = std::iter::from_fn(|| msg_rx.try_recv().ok())
                .filter_map(|message| match message.command {
                    IrcCommand::Notice(_, content) => Some(content),
                    _ => None,
                })
                .collect();
            assert!(
                notices
                    .iter()
                    .any(|content| content == "Server rejected capabilities: away-notify"),
                "notices: {notices:?}"
            );
            assert!(
                notices
                    .iter()
                    .any(|content| content == "IRCv3: multi-prefix"),
                "notices: {notices:?}"
            );
        });
    }

    #[test]
    fn hostile_cap_nak_retries_are_bounded_and_limited_to_requested_caps() {
        test_runtime().block_on(async {
            // A server that NAKs everything (and pads the first NAK with a cap
            // we never asked for) can draw each requested cap at most once
            // more, and no request for anything else.
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
                    .write_all(format!(":srv CAP * LS :{}\r\n", DESIRED_CAPS.join(" ")).as_bytes())
                    .await
                    .unwrap();
                let mut requests = Vec::new();
                loop {
                    let line = read_wire_line(&mut reader).await;
                    let Some(caps) = line.strip_prefix("CAP REQ :") else {
                        assert_eq!(line, "CAP END");
                        break;
                    };
                    let echoed = if requests.is_empty() {
                        format!("{caps} bogus-cap")
                    } else {
                        caps.to_string()
                    };
                    write_half
                        .write_all(format!(":srv CAP * NAK :{echoed}\r\n").as_bytes())
                        .await
                        .unwrap();
                    requests.push(caps.to_string());
                }
                write_half
                    .write_all(b":srv 001 tester :Welcome\r\n")
                    .await
                    .unwrap();
                requests
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
            timeout(Duration::from_secs(5), client.connect(msg_tx, cmd_rx))
                .await
                .expect("registration after blanket CAP NAKs timed out")
                .unwrap();
            let requests = server.await.unwrap();
            let initial = cap_request_lines(DESIRED_CAPS).len();
            let mut retried: Vec<&str> = requests[initial..].iter().map(String::as_str).collect();
            assert!(retried.iter().all(|cap| !cap.contains(' ')), "{retried:?}");
            retried.sort_unstable();
            let mut expected = DESIRED_CAPS.to_vec();
            expected.sort_unstable();
            assert_eq!(retried, expected);
        });
    }

    #[test]
    fn cap_nak_of_a_single_cap_request_is_final() {
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
                    .write_all(b":srv CAP * LS :multi-prefix\r\n")
                    .await
                    .unwrap();
                assert_eq!(read_wire_line(&mut reader).await, "CAP REQ :multi-prefix");
                write_half
                    .write_all(b":srv CAP * NAK :multi-prefix\r\n")
                    .await
                    .unwrap();
                // Re-requesting the identical single-cap REQ would be pointless.
                let next = read_wire_line(&mut reader).await;
                write_half
                    .write_all(b":srv 001 tester :Welcome\r\n")
                    .await
                    .unwrap();
                next
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
            timeout(Duration::from_secs(5), client.connect(msg_tx, cmd_rx))
                .await
                .expect("registration after a single-cap NAK timed out")
                .unwrap();
            assert_eq!(server.await.unwrap(), "CAP END");
        });
    }

    #[test]
    fn nak_of_the_sasl_cap_is_reported_as_a_rejection_not_a_missing_mechanism() {
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
                    .write_all(b":srv CAP * NAK :sasl\r\n")
                    .await
                    .unwrap();
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
                ..Default::default()
            };
            let (msg_tx, mut msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            timeout(Duration::from_secs(5), client.connect(msg_tx, cmd_rx))
                .await
                .expect("registration after a SASL NAK timed out")
                .unwrap();
            server.await.unwrap();
            let notices: Vec<String> = std::iter::from_fn(|| msg_rx.try_recv().ok())
                .filter_map(|message| match message.command {
                    IrcCommand::Notice(_, content) => Some(content),
                    _ => None,
                })
                .collect();
            assert!(
                notices
                    .iter()
                    .any(|content| content == "Server rejected the SASL capability"),
                "notices: {notices:?}"
            );
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
    fn raced_welcome_in_ack_loop_returns_immediately() {
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
                // Advertise sasl, then race 001 ahead of the CAP ACK: the
                // Welcome arm must return at once — no CAP END, no post-001
                // writes (#182).
                write_half
                    .write_all(
                        b":srv CAP * LS :sasl
",
                    )
                    .await
                    .unwrap();
                assert_eq!(read_wire_line(&mut reader).await, "CAP REQ :sasl");
                write_half
                    .write_all(
                        b":srv 001 tester :Welcome
",
                    )
                    .await
                    .unwrap();
                write_half
                    .write_all(b":srv 001 tester :Welcome\r\n")
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
            });

            let config = ServerConfig {
                host: "127.0.0.1".into(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".into(),
                sasl_username: Some("u".into()),
                sasl_password: Some("p".into()),
                ..Default::default()
            };
            let (msg_tx, mut msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            timeout(Duration::from_secs(2), client.connect(msg_tx, cmd_rx))
                .await
                .expect("registration timed out")
                .unwrap();
            server.await.unwrap();
            // The only SASL signal is the skip notice; nothing else was sent
            // post-001.
            let notices: Vec<String> = std::iter::from_fn(|| msg_rx.try_recv().ok())
                .filter_map(|message| match message.command {
                    IrcCommand::Notice(_, content) => Some(content),
                    _ => None,
                })
                .collect();
            assert!(
                notices
                    .iter()
                    .any(|content| content.contains("SASL skipped")),
                "notices: {notices:?}"
            );
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
    fn silent_tls_peer_times_out_with_a_connection_error() {
        test_runtime().block_on(async {
            // #233: a middlebox that accepts TCP and then says nothing must
            // end in an ordinary connection error (so auto-reconnect engages)
            // rather than leaving the connection thread stuck in the
            // handshake forever.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (accepted_tx, mut accepted_rx) = mpsc::channel(1);
            let server = tokio::spawn(async move {
                let (_stream, _) = listener.accept().await.unwrap();
                accepted_tx.send(()).await.unwrap();
                tokio::time::sleep(Duration::from_secs(30)).await;
            });

            let config = ServerConfig {
                host: "127.0.0.1".to_string(),
                port: addr.port(),
                use_tls: true,
                nick: "tester".to_string(),
                ..Default::default()
            };
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            client.tls_handshake_timeout = Duration::from_millis(200);
            let started = tokio::time::Instant::now();
            let error = timeout(Duration::from_secs(5), client.connect(msg_tx, cmd_rx))
                .await
                .expect("silent TLS peer stalled the connection forever")
                .unwrap_err();
            assert!(accepted_rx.recv().await.is_some());
            assert!(
                error.to_string().contains("TLS handshake timed out"),
                "{error}"
            );
            assert!(started.elapsed() < Duration::from_secs(3));
            server.abort();
        });
    }

    /// Shares scaled down so the tests that wait them out stay fast.
    const TEST_SHARES: AttemptShares = AttemptShares {
        cap: Duration::from_millis(400),
        floor: Duration::from_millis(100),
    };

    #[test]
    fn unresponsive_tcp_address_fails_within_the_connect_budget() {
        test_runtime().block_on(async {
            // #233: an address that never answers (a blackholed route) must
            // end in an ordinary connection error within the budget instead
            // of the OS default (up to minutes). The silent peer is simulated
            // by a connector that never completes: a real unroutable address
            // can be rejected at once, or accepted by a transparent proxy,
            // depending on the network the tests run on.
            let started = tokio::time::Instant::now();
            let error = timeout(
                Duration::from_secs(10),
                connect_tcp_via(
                    "127.0.0.1",
                    9,
                    Duration::from_millis(300),
                    TEST_SHARES,
                    |_| std::future::pending::<std::io::Result<()>>(),
                ),
            )
            .await
            .expect("TCP connect was not bounded by its budget")
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
            // The budget was really waited out, and not overshot by much.
            assert!(started.elapsed() >= Duration::from_millis(250));
            assert!(started.elapsed() < Duration::from_secs(5));
        });
    }

    #[test]
    fn connect_budget_falls_back_past_a_blackholed_address() {
        test_runtime().block_on(async {
            let first: std::net::SocketAddr = "192.0.2.1:6697".parse().unwrap();
            let second: std::net::SocketAddr = "192.0.2.2:6697".parse().unwrap();
            let started = tokio::time::Instant::now();
            // The first address never answers; it may use only its share of
            // the budget, leaving the second a chance to connect.
            let connected = connect_first_reachable(
                &[first, second],
                started + Duration::from_secs(1),
                TEST_SHARES,
                |addr| async move {
                    if addr == first {
                        std::future::pending().await
                    } else {
                        Ok(addr)
                    }
                },
            )
            .await
            .unwrap();
            assert_eq!(connected, second);
            // The silent address was waited on for its capped 400 ms share
            // before falling back to the second.
            assert!(started.elapsed() >= Duration::from_millis(350));
            assert!(started.elapsed() < Duration::from_secs(5));

            // Every address silent: the whole attempt ends at the deadline
            // with a TimedOut error.
            let started = tokio::time::Instant::now();
            let error = connect_first_reachable::<(), _, _>(
                &[first, second],
                started + Duration::from_millis(300),
                TEST_SHARES,
                |_| std::future::pending(),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
            assert!(started.elapsed() >= Duration::from_millis(250));
            assert!(started.elapsed() < Duration::from_secs(5));

            // A refused address falls through to the next one immediately.
            let connected = connect_first_reachable(
                &[first, second],
                tokio::time::Instant::now() + Duration::from_secs(5),
                TEST_SHARES,
                |addr| async move {
                    if addr == first {
                        Err(std::io::ErrorKind::ConnectionRefused.into())
                    } else {
                        Ok(addr)
                    }
                },
            )
            .await
            .unwrap();
            assert_eq!(connected, second);
        });
    }

    #[test]
    fn connect_shares_do_not_starve_later_addresses() {
        let shares = TCP_ATTEMPT_SHARES;
        let secs = Duration::from_secs;
        let millis = Duration::from_millis;
        // Half of what is left, but no more than 8 s...
        assert_eq!(shares.share(secs(20), false), secs(8));
        assert_eq!(shares.share(secs(12), false), secs(6));
        // ...and no less than 2 s while the budget lasts,
        assert_eq!(shares.share(secs(3), false), secs(2));
        // ...never more than what is left,
        assert_eq!(shares.share(millis(500), false), millis(500));
        // ...and the last address gets all of it.
        assert_eq!(shares.share(secs(3), true), secs(3));
        assert_eq!(shares.share(Duration::ZERO, true), Duration::ZERO);

        // Three addresses that all hang use up the 20 s default budget; with
        // plain halving the last of n would be left with budget / 2^(n-1).
        let mut remaining = TCP_CONNECT_TIMEOUT;
        let mut waits = Vec::new();
        for last in [false, false, true] {
            let share = shares.share(remaining, last);
            waits.push(share);
            remaining -= share;
        }
        assert_eq!(waits, [secs(8), secs(6), secs(6)]);
        assert_eq!(remaining, Duration::ZERO);
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
    fn pre_registration_nick_is_written_through_immediately() {
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
                // The write-through NICK must arrive before 001, not after
                // the registration buffer flushes (#173); and after the
                // config NICK, not clobbering it (#175).
                assert_eq!(read_wire_line(&mut reader).await, "NICK tester_");
                write_half
                    .write_all(b":srv 001 tester_ :Welcome\r\n")
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
            });

            let config = ServerConfig {
                host: "127.0.0.1".into(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".into(),
                ..Default::default()
            };
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            // Queued before the connection starts: the registration event
            // loop must write it through at once, not hold it until 001.
            cmd_tx
                .send(IrcCommand::Nick("tester_".to_string()))
                .unwrap();
            let mut client = IrcClient::new(config);
            let client_task = tokio::spawn(async move {
                let _ = client.connect(msg_tx, cmd_rx).await;
            });
            timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap();
            client_task.abort();
        });
    }

    #[test]
    fn mid_registration_nick_is_written_through() {
        test_runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (user_read_tx, mut user_read_rx) = mpsc::channel(1);
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                assert_eq!(read_wire_line(&mut reader).await, "CAP LS 302");
                assert_eq!(read_wire_line(&mut reader).await, "NICK tester");
                assert!(read_wire_line(&mut reader).await.starts_with("USER "));
                // Registration reads are in flight when the GUI queues the
                // walkaway retry: the write-through must reach the wire
                // before 001 (#175).
                user_read_tx.send(()).await.unwrap();
                assert_eq!(read_wire_line(&mut reader).await, "NICK late_");
                write_half
                    .write_all(b":srv 001 late_ :Welcome\r\n")
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
            });

            let config = ServerConfig {
                host: "127.0.0.1".into(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".into(),
                ..Default::default()
            };
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let mut client = IrcClient::new(config);
            let client_task = tokio::spawn(async move {
                let _ = client.connect(msg_tx, cmd_rx).await;
            });
            user_read_rx.recv().await.unwrap();
            cmd_tx.send(IrcCommand::Nick("late_".to_string())).unwrap();
            timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap();
            client_task.abort();
        });
    }

    #[test]
    fn outbound_commands_with_line_breaks_are_refused_instead_of_split_into_extra_lines() {
        // #285: the writers append CRLF to the formatted command, so a field
        // containing CR/LF/NUL would put attacker-chosen lines on the wire,
        // both on the registration NICK write-through and on the post-001
        // writer. The unsafe commands are dropped; the safe ones still flow.
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
                let registration_nick = read_wire_line(&mut reader).await;
                write_half
                    .write_all(b":srv 001 tester :Welcome\r\n")
                    .await
                    .unwrap();
                let first_after_welcome = read_wire_line(&mut reader).await;
                let extra = timeout(Duration::from_millis(300), read_wire_line(&mut reader))
                    .await
                    .ok();
                (registration_nick, first_after_welcome, extra)
            });

            let config = ServerConfig {
                host: "127.0.0.1".into(),
                port: addr.port(),
                use_tls: false,
                nick: "tester".into(),
                ..Default::default()
            };
            let (msg_tx, _msg_rx) = mpsc::channel(100);
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            for command in [
                IrcCommand::Nick("evil\r\nJOIN #evil".to_string()),
                IrcCommand::Privmsg("#c".to_string(), "hi\r\nPRIVMSG #evil :spam".to_string()),
                IrcCommand::Nick("safe_".to_string()),
                IrcCommand::Privmsg("#c".to_string(), "fine".to_string()),
            ] {
                cmd_tx.send(command).unwrap();
            }
            let mut client = IrcClient::new(config);
            let client_task = tokio::spawn(async move {
                let _ = client.connect(msg_tx, cmd_rx).await;
            });
            let (registration_nick, first_after_welcome, extra) =
                timeout(Duration::from_secs(5), server)
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(registration_nick, "NICK safe_");
            assert_eq!(first_after_welcome, "PRIVMSG #c :fine");
            assert_eq!(extra, None, "an injected line reached the wire");
            client_task.abort();
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
