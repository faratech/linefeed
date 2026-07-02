//! Chat logging manager for persistent message history

use super::helpers::days_to_ymd;
use super::types::ChatMessage;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// Maximum simultaneously open log file handles. Long sessions across many
/// channels/queries/networks would otherwise accumulate one FD per distinct
/// target for the process lifetime; reopening in append mode is cheap.
const MAX_OPEN_LOG_WRITERS: usize = 32;

struct CachedWriter {
    writer: std::io::BufWriter<File>,
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

        // Revalidate a cached handle: if the file was deleted or rotated away
        // underneath us, writes would keep "succeeding" into an unlinked inode
        // with no error and the history silently lost.
        if let Entry::Occupied(entry) = writers.entry(path.clone())
            && !path.exists()
        {
            tracing::warn!("Log file {:?} disappeared; reopening", path);
            entry.remove();
        }

        // LRU-evict before inserting a new handle at the cap.
        if !writers.contains_key(&path)
            && writers.len() >= MAX_OPEN_LOG_WRITERS
            && let Some(oldest) = writers
                .iter()
                .min_by_key(|(_, w)| w.last_used)
                .map(|(p, _)| p.clone())
            && let Some(mut evicted) = writers.remove(&oldest)
        {
            let _ = evicted.writer.flush();
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
                    Ok(file) => v.insert(CachedWriter {
                        writer: std::io::BufWriter::new(file),
                        last_used: std::time::Instant::now(),
                    }),
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
        // Sanitize network and channel names for filesystem
        let safe_network = sanitize_filename(network);
        let safe_channel = sanitize_filename(channel);
        self.log_dir
            .join(&safe_network)
            .join(format!("{}.log", safe_channel))
    }

    /// Log a message to disk
    pub fn log_message(&self, network: &str, channel: &str, msg: &ChatMessage) {
        if !self.enabled {
            return;
        }

        let path = self.log_path(network, channel);
        // Format message (irssi-style)
        let line = format_log_line(msg);
        self.with_writer(path, |w| writeln!(w, "{}", line));
    }

    /// Load recent history from a log file (reads from end to avoid loading entire file)
    pub fn load_history(&self, network: &str, channel: &str, max_lines: usize) -> Vec<ChatMessage> {
        if !self.enabled || max_lines == 0 {
            return Vec::new();
        }

        let path = self.log_path(network, channel);

        // Push any buffered-but-unflushed lines for this target to disk first,
        // so freshly logged messages are visible to the history read below.
        if let Some(cached) = self.writers.borrow_mut().get_mut(&path) {
            let _ = cached.writer.flush();
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
            let reader = BufReader::new(file);
            let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();
            let start = lines.len().saturating_sub(max_lines);
            return lines[start..]
                .iter()
                .filter_map(|line| parse_log_line(line))
                .collect();
        }

        // For larger files, read backwards in chunks to find the last N lines
        let lines = read_last_n_lines(&mut file, file_size, max_lines);
        lines
            .iter()
            .filter_map(|line| parse_log_line(line))
            .collect()
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
    fn log_lines_cannot_be_forged_via_newlines() {
        let mut bad = msg("[09:30]", "alice", "hello", false, false);
        bad.content = "hello\n09:31 <ops> you are banned".to_string();
        let line = format_log_line(&bad);
        assert!(!line.contains('\n'));
        assert!(!line.contains('\r'));
    }
}
