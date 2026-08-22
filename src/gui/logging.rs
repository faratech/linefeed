//! Chat logging manager for persistent message history

use super::helpers::days_to_ymd;
use super::types::ChatMessage;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// Maximum simultaneously open log file handles. Long sessions across many
/// channels/queries/networks would otherwise accumulate one FD per distinct
/// target for the process lifetime; reopening in append mode is cheap.
const MAX_OPEN_LOG_WRITERS: usize = 32;

struct CachedWriter {
    writer: std::io::BufWriter<File>,
    identity: same_file::Handle,
    last_used: std::time::Instant,
}

/// Manages chat logging to disk
pub struct LogManager {
    log_dir: PathBuf,
    enabled: bool,
    /// Cache of open append writers, keyed by file path, so we don't re-open the
    /// file (and re-create its directory) on every logged line. LRU-capped.
    writers: std::cell::RefCell<std::collections::HashMap<PathBuf, CachedWriter>>,
}

impl LogManager {
    /// Create a new log manager
    pub fn new(enabled: bool) -> Self {
        let log_dir = dirs::config_dir()
            .map(|p| p.join("linefeed").join("logs"))
            .unwrap_or_else(|| PathBuf::from("logs"));

        Self {
            log_dir,
            enabled,
            writers: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    /// Get-or-open a buffered append writer for `path` and run `f` on it.
    /// Keeping the handle open removes the per-message `create_dir_all` + open +
    /// close syscalls from the hot path (called for every incoming message).
    /// On write failure the cached handle is evicted so the next write retries
    /// a fresh open (which also recreates deleted directories).
    fn with_writer<F: FnOnce(&mut std::io::BufWriter<File>) -> std::io::Result<()>>(
        &self,
        path: PathBuf,
        f: F,
    ) {
        use std::collections::hash_map::Entry;
        let mut writers = self.writers.borrow_mut();

        // Revalidate the cached descriptor against the file currently at the
        // path. Existence alone is insufficient: logrotate commonly renames
        // the old inode and creates a replacement at the same path.
        if let Entry::Occupied(entry) = writers.entry(path.clone()) {
            let replaced = same_file::Handle::from_path(&path)
                .map(|current| current != entry.get().identity)
                .unwrap_or(true);
            if replaced {
                tracing::warn!("Log file {:?} was removed or replaced; reopening", path);
                entry.remove();
            }
        }

        // LRU-evict before inserting a new handle at the cap.
        if !writers.contains_key(&path)
            && writers.len() >= MAX_OPEN_LOG_WRITERS
            && let Some(oldest) = writers
                .iter()
                .min_by_key(|(_, w)| w.last_used)
                .map(|(p, _)| p.clone())
            && let Some(mut evicted) = writers.remove(&oldest)
            && let Err(e) = evicted.writer.flush()
        {
            tracing::error!("Failed to flush evicted log file {:?}: {}", oldest, e);
        }

        let cached = match writers.entry(path.clone()) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(v) => {
                if let Some(parent) = path.parent()
                    && let Err(e) = fs::create_dir_all(parent)
                {
                    tracing::error!("Failed to create log directory: {}", e);
                    return;
                }
                match OpenOptions::new().create(true).append(true).open(&path) {
                    Ok(file) => {
                        let identity = match file.try_clone().and_then(same_file::Handle::from_file)
                        {
                            Ok(identity) => identity,
                            Err(e) => {
                                tracing::error!(
                                    "Failed to identify log file {:?}; not caching it: {}",
                                    path,
                                    e
                                );
                                return;
                            }
                        };
                        v.insert(CachedWriter {
                            writer: std::io::BufWriter::new(file),
                            identity,
                            last_used: std::time::Instant::now(),
                        })
                    }
                    Err(e) => {
                        tracing::error!("Failed to open log file {:?}: {}", path, e);
                        return;
                    }
                }
            }
        };
        cached.last_used = std::time::Instant::now();
        if let Err(e) = f(&mut cached.writer) {
            tracing::error!("Failed to write to log {:?}: {}", path, e);
            writers.remove(&path);
        }
        // Flushing is deferred to flush_all(), called once per GUI frame, so a
        // message flood does not pay one flush syscall per logged line.
    }

    /// Flush all cached writers. Called once per frame (and on shutdown via
    /// BufWriter's Drop) instead of after every logged line. Writers whose
    /// flush fails (e.g. disk full) are evicted so the next write retries a
    /// fresh open instead of silently dropping data forever.
    pub fn flush_all(&self) {
        let mut writers = self.writers.borrow_mut();
        let mut failed: Vec<PathBuf> = Vec::new();
        for (path, cached) in writers.iter_mut() {
            if let Err(e) = cached.writer.flush() {
                tracing::error!("Failed to flush log file {:?}: {}", path, e);
                failed.push(path.clone());
            }
        }
        for path in failed {
            writers.remove(&path);
        }
    }

    /// Get the log file path for a channel/query
    fn log_path(&self, network: &str, channel: &str) -> PathBuf {
        // Keep a readable prefix, then append a stable hash of the complete raw
        // identifier. The hash prevents collisions from lossy filesystem
        // sanitization and from case-insensitive filesystems.
        let safe_network = encoded_path_component(network);
        let safe_channel = encoded_path_component(channel);
        self.log_dir
            .join(&safe_network)
            .join(format!("{}.log", safe_channel))
    }

    fn legacy_log_path(&self, network: &str, channel: &str) -> PathBuf {
        self.log_dir
            .join(sanitize_filename(network))
            .join(format!("{}.log", sanitize_filename(channel)))
    }

    /// Claim and move one legacy host-only log into a validated endpoint path.
    /// Migration is deliberately conservative: lossy legacy components are
    /// never read, and an atomic per-network claim prevents two endpoints from
    /// importing the same old host-only history.
    fn migrate_legacy_history(
        &self,
        network: &str,
        legacy_network: &str,
        channel: &str,
        allow_migration: bool,
    ) {
        if !allow_migration
            || sanitize_filename(legacy_network) != legacy_network
            || sanitize_filename(channel) != channel
        {
            return;
        }

        let destination = self.log_path(network, channel);
        if destination.exists() {
            return;
        }
        let legacy = self.legacy_log_path(legacy_network, channel);
        if !legacy.is_file() {
            return;
        }

        let Some(legacy_dir) = legacy.parent() else {
            return;
        };
        let claim_path = legacy_dir.join(".linefeed-endpoint-migration");
        let claimed = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&claim_path)
        {
            Ok(mut claim) => claim
                .write_all(network.as_bytes())
                .and_then(|_| claim.flush())
                .is_ok(),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                fs::read_to_string(&claim_path).is_ok_and(|value| value == network)
            }
            Err(_) => false,
        };
        if !claimed {
            return;
        }

        if let Some(parent) = destination.parent()
            && fs::create_dir_all(parent).is_ok()
        {
            // Link-then-unlink rather than rename: hard_link fails atomically
            // when the destination already exists, so a log file a concurrent
            // instance just created cannot be clobbered by the migration
            // (single-instance enforcement is Windows-only).
            match fs::hard_link(&legacy, &destination) {
                Ok(()) => {
                    let _ = fs::remove_file(&legacy);
                    tracing::info!(
                        "Migrated legacy chat history {:?} to endpoint-scoped path {:?}",
                        legacy,
                        destination
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => match fs::rename(&legacy, &destination) {
                    Ok(()) => tracing::info!(
                        "Migrated legacy chat history {:?} to endpoint-scoped path {:?}",
                        legacy,
                        destination
                    ),
                    Err(error) => tracing::warn!(
                        "Could not migrate legacy chat history {:?}: {}",
                        legacy,
                        error
                    ),
                },
            }
        }
    }

    /// Log a message to disk
    pub fn log_message(&self, network: &str, channel: &str, msg: &ChatMessage) {
        if !self.enabled
            || super::commands::service_message_contains_credentials(channel, &msg.content)
        {
            return;
        }

        let path = self.log_path(network, channel);
        // Format message (irssi-style)
        let line = format_log_line(msg);
        self.with_writer(path, |w| writeln!(w, "{}", line));
    }

    pub fn load_history_with_legacy(
        &self,
        network: &str,
        legacy_network: &str,
        channel: &str,
        max_lines: usize,
        allow_legacy_migration: bool,
    ) -> Vec<ChatMessage> {
        if !self.enabled || max_lines == 0 {
            return Vec::new();
        }

        self.migrate_legacy_history(network, legacy_network, channel, allow_legacy_migration);
        let path = self.log_path(network, channel);

        // Push any buffered-but-unflushed lines for this target to disk first,
        // so freshly logged messages are visible to the history read below.
        let flush_error = {
            let mut writers = self.writers.borrow_mut();
            writers
                .get_mut(&path)
                .and_then(|cached| cached.writer.flush().err())
        };
        if let Some(e) = flush_error {
            tracing::error!("Failed to flush log file {:?} before reading: {}", path, e);
            self.writers.borrow_mut().remove(&path);
            return Vec::new();
        }

        let mut file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => return Vec::new(), // File doesn't exist yet
        };

        // Get file size
        let file_size = match file.seek(SeekFrom::End(0)) {
            Ok(size) => size,
            Err(_) => return Vec::new(),
        };

        // For small files, just read the whole thing
        if file_size < 65536 {
            if file.seek(SeekFrom::Start(0)).is_err() {
                return Vec::new();
            }
            let mut raw = String::with_capacity(file_size as usize);
            // Decode lossily like the large-file branch below: BufRead::lines
            // yields Err on the first non-UTF-8 byte and would silently
            // truncate history at that point (irssi logs carry raw wire bytes).
            if file.read_to_string(&mut raw).is_err() {
                let mut bytes = Vec::with_capacity(file_size as usize);
                if file.seek(SeekFrom::Start(0)).is_err() || file.read_to_end(&mut bytes).is_err()
                {
                    return Vec::new();
                }
                raw = String::from_utf8_lossy(&bytes).into_owned();
            }
            let lines: Vec<&str> = raw.lines().collect();
            let start = lines.len().saturating_sub(max_lines);
            let mut messages: Vec<ChatMessage> = lines[start..]
                .iter()
                .filter_map(|line| parse_log_line(line))
                .collect();
            redact_sensitive_history(channel, &mut messages);
            return messages;
        }

        // For larger files, read backwards in chunks to find the last N lines
        let lines = read_last_n_lines(&mut file, file_size, max_lines);
        let mut messages: Vec<ChatMessage> = lines
            .iter()
            .filter_map(|line| parse_log_line(line))
            .collect();
        redact_sensitive_history(channel, &mut messages);
        messages
    }

    #[cfg(test)]
    pub(super) fn with_test_dir(log_dir: PathBuf) -> Self {
        Self {
            log_dir,
            enabled: true,
            writers: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    #[cfg(test)]
    pub(super) fn test_log_path(&self, network: &str, channel: &str) -> PathBuf {
        self.log_path(network, channel)
    }

    /// Write a session marker (log opened/closed)
    pub fn log_session_start(&self, network: &str, channel: &str) {
        if !self.enabled {
            return;
        }

        let path = self.log_path(network, channel);
        let now = current_datetime_string();
        self.with_writer(path, |w| writeln!(w, "--- Log opened {}", now));
    }
}

/// Sanitize a string for use as a filename
fn sanitize_filename(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            _ => c,
        })
        .collect();
    let trimmed = mapped.trim();
    // Neutralize path-significant components so a network/channel name like ".."
    // or "." (or all dots) cannot escape the logs directory via path traversal.
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '.') {
        return "_".to_string();
    }
    // Strip leading dots to avoid hidden files / ".." remnants.
    let cleaned = trimmed.trim_start_matches('.');
    if cleaned.is_empty() {
        return "_".to_string();
    }
    // Windows reserved device names (CON, PRN, AUX, NUL, COM1-9, LPT1-9) refer
    // to the device even with an extension appended ("nul.log" is still NUL),
    // so a PM from a nick like "nul" would silently log into the void. Prefix
    // them on every platform to keep log directories portable.
    let stem = cleaned.split('.').next().unwrap_or(cleaned);
    let stem_upper = stem.to_ascii_uppercase();
    let reserved = matches!(stem_upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem_upper.len() == 4
            && (stem_upper.starts_with("COM") || stem_upper.starts_with("LPT"))
            && stem_upper.as_bytes()[3].is_ascii_digit());
    if reserved {
        format!("_{}", cleaned)
    } else {
        cleaned.to_string()
    }
}

/// Return a portable, collision-resistant path component. FNV-1a is used only
/// as a stable identifier suffix (not for security); the readable prefix keeps
/// log directories usable by humans while distinct raw IRC identifiers no
/// longer collapse to the same sanitized name.
fn encoded_path_component(name: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }

    let readable = sanitize_filename(name);
    let prefix: String = readable.chars().take(80).collect();
    format!("{prefix}~{hash:016x}")
}

/// Replace CR/LF so no field can inject forged lines into the log
/// (defense in depth: the server-time tag is now strictly parsed, but any
/// future field source gets the same guarantee).
fn sanitize_log_field(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains(['\r', '\n']) {
        s.replace(['\r', '\n'], " ").into()
    } else {
        s.into()
    }
}

fn redact_sensitive_history(channel: &str, messages: &mut [ChatMessage]) {
    for message in messages {
        if super::commands::service_message_contains_credentials(channel, &message.content) {
            message.content = "<credential command redacted>".to_string();
            message.no_log = true;
            message.render_cache = Default::default();
        }
    }
}

/// Format a ChatMessage as a log line
fn format_log_line(msg: &ChatMessage) -> String {
    // Remove brackets from timestamp for cleaner logs
    let time = msg.timestamp.trim_start_matches('[').trim_end_matches(']');
    let time = sanitize_log_field(time);
    let sender = sanitize_log_field(&msg.sender);
    let content = sanitize_log_field(&msg.content);

    if msg.is_system {
        format!("{} -!- {}", time, content)
    } else if msg.is_action {
        format!("{} * {} {}", time, sender, content)
    } else {
        format!("{} <{}> {}", time, sender, content)
    }
}

/// Split a log line into (timestamp, rest-of-line), tolerating a timestamp that
/// contains a space (the "full" MM-DD HH:MM format). The timestamp is either a
/// single time token (contains ':') or a "MM-DD HH:MM" pair whose first token is
/// a date (no ':' but a '-'). Returns None if there is no separating space.
fn split_timestamp(line: &str) -> Option<(String, &str)> {
    let (first, after) = line.split_once(' ')?;
    if !first.contains(':') && first.contains('-') {
        // Date token; the next whitespace-separated token is the time.
        let (second, rest) = after.split_once(' ')?;
        Some((format!("{} {}", first, second), rest.trim_start()))
    } else {
        Some((first.to_string(), after))
    }
}

/// Parse a log line back into a ChatMessage
fn parse_log_line(line: &str) -> Option<ChatMessage> {
    // Skip session markers
    if line.starts_with("---") {
        return None;
    }

    // Parse format: "HH:MM <nick> message" / "HH:MM * nick action" / "HH:MM -!- system".
    // The timestamp may itself contain a space when the "full" (MM-DD HH:MM) format
    // is used, so detect a leading date token rather than blindly splitting once.
    let (ts_inner, rest) = split_timestamp(line)?;
    let timestamp = format!("[{}]", ts_inner);

    if let Some(system_rest) = rest.strip_prefix("-!- ") {
        // System message
        Some(ChatMessage {
            timestamp,
            sender: "*".to_string(),
            content: system_rest.to_string(),
            is_action: false,
            is_system: true,
            is_highlight: false,
            no_log: false,
            render_cache: Default::default(),
        })
    } else if let Some(action_rest) = rest.strip_prefix("* ") {
        // Action message: "* nick does something"
        let action_parts: Vec<&str> = action_rest.splitn(2, ' ').collect();
        if action_parts.len() >= 2 {
            Some(ChatMessage {
                timestamp,
                sender: action_parts[0].to_string(),
                content: action_parts[1].to_string(),
                is_action: true,
                is_system: false,
                is_highlight: false,
                no_log: false,
                render_cache: Default::default(),
            })
        } else {
            None
        }
    } else if rest.starts_with('<') {
        // Regular message: "<nick> message"
        if let Some(end) = rest.find('>') {
            let nick = &rest[1..end];
            let content = rest[end + 1..].trim_start();
            Some(ChatMessage {
                timestamp,
                sender: nick.to_string(),
                content: content.to_string(),
                is_action: false,
                is_system: false,
                is_highlight: false,
                no_log: false,
                render_cache: Default::default(),
            })
        } else {
            None
        }
    } else {
        None
    }
}

/// Read the last N lines from a file by reading backwards in chunks
fn read_last_n_lines(file: &mut File, file_size: u64, max_lines: usize) -> Vec<String> {
    const CHUNK_SIZE: u64 = 8192;
    // Chunks are collected newest-first and concatenated once at the end;
    // prepending per chunk would re-copy the accumulated buffer every
    // iteration (O(k²) memcpy on large history loads).
    let mut chunks_rev: Vec<Vec<u8>> = Vec::new();
    let mut pos = file_size;
    let mut newline_count = 0usize;

    // Read backwards in chunks until we've accumulated more than `max_lines` line
    // breaks (so the last `max_lines` complete lines are guaranteed present).
    while pos > 0 && newline_count <= max_lines {
        let chunk_start = pos.saturating_sub(CHUNK_SIZE);
        let chunk_len = (pos - chunk_start) as usize;

        if file.seek(SeekFrom::Start(chunk_start)).is_err() {
            break;
        }

        let mut chunk = vec![0u8; chunk_len];
        if file.read_exact(&mut chunk).is_err() {
            break;
        }

        newline_count += chunk.iter().filter(|&&b| b == b'\n').count();
        chunks_rev.push(chunk);
        pos = chunk_start;
    }

    let total_len: usize = chunks_rev.iter().map(Vec::len).sum();
    let mut remaining_bytes: Vec<u8> = Vec::with_capacity(total_len);
    for chunk in chunks_rev.iter().rev() {
        remaining_bytes.extend_from_slice(chunk);
    }

    // Decode once (not per chunk), then return only the last `max_lines` lines.
    let text = String::from_utf8_lossy(&remaining_bytes);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].iter().map(|s| s.to_string()).collect()
}

/// Get current date/time as a string for session markers
fn current_datetime_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    let (year, month, day) = days_to_ymd(days);
    let weekday = ((days + 4) % 7) as usize; // Jan 1, 1970 was Thursday (4)
    let weekday_name = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][weekday];
    let month_name = [
        "", "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][month as usize];

    format!(
        "{} {} {:2} {:02}:{:02}:{:02} {}",
        weekday_name, month_name, day, hours, minutes, seconds, year
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::types::ChatMessage;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    fn test_manager(test_name: &str) -> (LogManager, PathBuf) {
        let unique = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "linefeed-logging-{test_name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temporary log directory");
        (
            LogManager {
                log_dir: dir.clone(),
                enabled: true,
                writers: std::cell::RefCell::new(std::collections::HashMap::new()),
            },
            dir,
        )
    }

    fn msg(ts: &str, sender: &str, content: &str, system: bool, action: bool) -> ChatMessage {
        ChatMessage {
            timestamp: ts.to_string(),
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: action,
            is_system: system,
            is_highlight: false,
            no_log: false,
            render_cache: Default::default(),
        }
    }

    fn round_trip(m: &ChatMessage) -> ChatMessage {
        parse_log_line(&format_log_line(m)).expect("log line should round-trip")
    }

    #[test]
    fn full_format_with_space_round_trips() {
        // Regression: the "full" timestamp [MM-DD HH:MM] contains a space, which
        // previously broke parsing and silently dropped all loaded history.
        let p = round_trip(&msg("[06-14 09:30]", "alice", "hello world", false, false));
        assert_eq!(p.timestamp, "[06-14 09:30]");
        assert_eq!(p.sender, "alice");
        assert_eq!(p.content, "hello world");
        assert!(!p.is_system && !p.is_action);
    }

    #[test]
    fn short_and_long_formats_round_trip() {
        let p = round_trip(&msg("[09:30]", "bob", "hi there", false, false));
        assert_eq!(p.timestamp, "[09:30]");
        assert_eq!(p.sender, "bob");
        assert_eq!(p.content, "hi there");

        let p = round_trip(&msg("[09:30:15]", "bob", "hi", false, false));
        assert_eq!(p.timestamp, "[09:30:15]");
    }

    #[test]
    fn system_and_action_round_trip() {
        let p = round_trip(&msg("[06-14 09:30]", "*", "x has joined", true, false));
        assert!(p.is_system);
        assert_eq!(p.content, "x has joined");

        let p = round_trip(&msg("[06-14 09:30]", "carol", "waves around", false, true));
        assert!(p.is_action);
        assert_eq!(p.sender, "carol");
        assert_eq!(p.content, "waves around");
    }

    #[test]
    fn sanitize_filename_blocks_traversal() {
        assert_eq!(sanitize_filename(".."), "_");
        assert_eq!(sanitize_filename("."), "_");
        assert_eq!(sanitize_filename("..."), "_");
        assert_eq!(sanitize_filename("a/b"), "a_b");
        // '/' maps to '_' and the leading dots are stripped -> safe single component.
        assert_eq!(sanitize_filename("../etc"), "_etc");
        assert_eq!(sanitize_filename("normal"), "normal");
        assert_eq!(sanitize_filename("#channel"), "#channel");
    }

    #[test]
    fn sanitize_filename_blocks_windows_device_names() {
        assert_eq!(sanitize_filename("nul"), "_nul");
        assert_eq!(sanitize_filename("NUL"), "_NUL");
        assert_eq!(sanitize_filename("CoN"), "_CoN");
        assert_eq!(sanitize_filename("com1"), "_com1");
        assert_eq!(sanitize_filename("LPT9"), "_LPT9");
        // The stem before the first dot is what Windows reserves.
        assert_eq!(sanitize_filename("nul.chan"), "_nul.chan");
        // Non-reserved lookalikes pass through.
        assert_eq!(sanitize_filename("nullable"), "nullable");
        assert_eq!(sanitize_filename("com10"), "com10");
        assert_eq!(sanitize_filename("console"), "console");
    }

    #[test]
    fn encoded_components_do_not_collapse_distinct_targets_or_endpoints() {
        let (manager, dir) = test_manager("collision");
        assert_ne!(
            manager.log_path("tls:irc.example:6697", "#a/b"),
            manager.log_path("tls:irc.example:6697", "#a?b")
        );
        assert_ne!(
            manager.log_path("tls:irc.example:6697", "#room"),
            manager.log_path("tls:irc.example:7000", "#room")
        );
        assert_ne!(
            manager.log_path("tls:IRC.example:6697", "#room"),
            manager.log_path("tls:irc.example:6697", "#room")
        );
        fs::remove_dir_all(dir).expect("remove temporary log directory");
    }

    #[test]
    fn lossless_legacy_history_migrates_once_to_endpoint_identity() {
        let (manager, dir) = test_manager("legacy-migration");
        let legacy = manager.legacy_log_path("irc.example", "#room");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "09:30 <alice> legacy hello\n").unwrap();

        let history = manager.load_history_with_legacy(
            "tls://irc.example:6697",
            "irc.example",
            "#room",
            20,
            true,
        );

        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "legacy hello");
        assert!(!legacy.exists());
        assert!(manager.log_path("tls://irc.example:6697", "#room").exists());
        assert_eq!(
            fs::read_to_string(
                legacy
                    .parent()
                    .unwrap()
                    .join(".linefeed-endpoint-migration")
            )
            .unwrap(),
            "tls://irc.example:6697"
        );

        drop(manager);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lossy_legacy_names_are_never_imported() {
        let (manager, dir) = test_manager("legacy-lossy");
        let colliding_legacy = manager.legacy_log_path("irc.example", "#a/b");
        fs::create_dir_all(colliding_legacy.parent().unwrap()).unwrap();
        fs::write(&colliding_legacy, "09:30 <alice> wrong channel\n").unwrap();

        let history = manager.load_history_with_legacy(
            "tls://irc.example:6697",
            "irc.example",
            "#a/b",
            20,
            true,
        );

        assert!(history.is_empty());
        assert!(colliding_legacy.exists());
        assert!(!manager.log_path("tls://irc.example:6697", "#a/b").exists());

        drop(manager);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_network_claim_prevents_cross_endpoint_import() {
        let (manager, dir) = test_manager("legacy-endpoint-claim");
        let first = manager.legacy_log_path("irc.example", "#one");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::write(&first, "09:30 <alice> endpoint one\n").unwrap();
        assert_eq!(
            manager
                .load_history_with_legacy(
                    "tls://irc.example:6697",
                    "irc.example",
                    "#one",
                    20,
                    true,
                )
                .len(),
            1
        );

        let second = manager.legacy_log_path("irc.example", "#two");
        fs::write(&second, "09:31 <bob> must stay legacy\n").unwrap();
        let other_endpoint = manager.load_history_with_legacy(
            "plain://irc.example:6667",
            "irc.example",
            "#two",
            20,
            true,
        );
        assert!(other_endpoint.is_empty());
        assert!(second.exists());
        assert!(
            !manager
                .log_path("plain://irc.example:6667", "#two")
                .exists()
        );

        drop(manager);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn credential_commands_are_suppressed_on_write_and_redacted_on_load() {
        let (manager, dir) = test_manager("credential-redaction");
        let identity = "tls://irc.example:6697";
        let sensitive = msg("[09:30]", "me", "IDENTIFY plaintext-secret", false, false);
        manager.log_message(identity, "NickServ", &sensitive);
        manager.flush_all();
        let path = manager.log_path(identity, "NickServ");
        assert!(!path.exists());

        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "09:30 <me> IDENTIFY legacy-secret\n").unwrap();
        let history =
            manager.load_history_with_legacy(identity, "irc.example", "NickServ", 20, false);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "<credential command redacted>");
        assert!(history[0].no_log);
        assert!(!history[0].content.contains("legacy-secret"));

        drop(manager);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn history_load_survives_invalid_utf8_bytes() {
        let (manager, dir) = test_manager("lossy-history");
        let path = manager.log_path("tls:irc.example:6697", "#room");
        // irssi-compatible logs may carry raw wire bytes: a non-UTF-8 byte
        // must not truncate everything after it (BufRead::lines semantics).
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut raw = Vec::new();
        raw.extend_from_slice(b"09:30 <alice> before the glitch\n");
        raw.push(0xFF);
        raw.extend_from_slice(b"\n");
        raw.extend_from_slice(b"09:31 <bob> after the glitch\n");
        fs::write(&path, &raw).unwrap();

        let messages =
            manager.load_history_with_legacy("tls:irc.example:6697", "", "#room", 100, false);
        let contents: Vec<&str> = messages.iter().map(|m| m.content.as_str()).collect();
        assert!(contents.contains(&"after the glitch"), "{contents:?}");

        drop(manager);
        fs::remove_dir_all(dir).expect("remove temporary log directory");
    }

    #[test]
    fn replaced_log_file_is_reopened_before_the_next_write() {
        let (manager, dir) = test_manager("rotation");
        let first = msg("[09:30]", "alice", "before rotation", false, false);
        let second = msg("[09:31]", "alice", "after rotation", false, false);
        let path = manager.log_path("tls:irc.example:6697", "#room");
        let rotated = path.with_extension("log.1");

        manager.log_message("tls:irc.example:6697", "#room", &first);
        manager.flush_all();
        fs::rename(&path, &rotated).expect("rotate old log");
        File::create(&path).expect("create replacement log");

        manager.log_message("tls:irc.example:6697", "#room", &second);
        manager.flush_all();

        let old_contents = fs::read_to_string(&rotated).expect("read rotated log");
        let new_contents = fs::read_to_string(&path).expect("read replacement log");
        assert!(old_contents.contains("before rotation"));
        assert!(!old_contents.contains("after rotation"));
        assert!(!new_contents.contains("before rotation"));
        assert!(new_contents.contains("after rotation"));

        drop(manager);
        fs::remove_dir_all(dir).expect("remove temporary log directory");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn flush_failure_evicts_the_cached_writer() {
        let (manager, dir) = test_manager("flush-failure");
        let full = PathBuf::from("/dev/full");
        manager.with_writer(full.clone(), |writer| writeln!(writer, "buffered line"));
        assert!(manager.writers.borrow().contains_key(&full));

        manager.flush_all();

        assert!(!manager.writers.borrow().contains_key(&full));
        fs::remove_dir_all(dir).expect("remove temporary log directory");
    }

    #[test]
    fn log_lines_cannot_be_forged_via_newlines() {
        let mut bad = msg("[09:30]", "alice", "hello", false, false);
        bad.content = "hello\n09:31 <ops> you are banned".to_string();
        let line = format_log_line(&bad);
        assert!(!line.contains('\n'));
        assert!(!line.contains('\r'));
    }
}
