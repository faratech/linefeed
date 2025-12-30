//! Chat logging manager for persistent message history

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use super::types::ChatMessage;
use super::helpers::days_to_ymd;

/// Manages chat logging to disk
pub struct LogManager {
    log_dir: PathBuf,
    enabled: bool,
}

impl LogManager {
    /// Create a new log manager
    pub fn new(enabled: bool) -> Self {
        let log_dir = dirs::config_dir()
            .map(|p| p.join("linefeed").join("logs"))
            .unwrap_or_else(|| PathBuf::from("logs"));

        Self { log_dir, enabled }
    }

    /// Get the log file path for a channel/query
    fn log_path(&self, network: &str, channel: &str) -> PathBuf {
        // Sanitize network and channel names for filesystem
        let safe_network = sanitize_filename(network);
        let safe_channel = sanitize_filename(channel);
        self.log_dir.join(&safe_network).join(format!("{}.log", safe_channel))
    }

    /// Log a message to disk
    pub fn log_message(&self, network: &str, channel: &str, msg: &ChatMessage) {
        if !self.enabled {
            return;
        }

        let path = self.log_path(network, channel);

        // Ensure directory exists
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                tracing::error!("Failed to create log directory: {}", e);
                return;
            }
        }

        // Open file in append mode
        let mut file = match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("Failed to open log file {:?}: {}", path, e);
                return;
            }
        };

        // Format message (irssi-style)
        let line = format_log_line(msg);
        if let Err(e) = writeln!(file, "{}", line) {
            tracing::error!("Failed to write to log: {}", e);
        }
    }

    /// Load recent history from a log file (reads from end to avoid loading entire file)
    pub fn load_history(&self, network: &str, channel: &str, max_lines: usize) -> Vec<ChatMessage> {
        if !self.enabled || max_lines == 0 {
            return Vec::new();
        }

        let path = self.log_path(network, channel);

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
            let lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
            let start = lines.len().saturating_sub(max_lines);
            return lines[start..]
                .iter()
                .filter_map(|line| parse_log_line(line))
                .collect();
        }

        // For larger files, read backwards in chunks to find the last N lines
        let lines = read_last_n_lines(&mut file, file_size, max_lines);
        lines.iter().filter_map(|line| parse_log_line(line)).collect()
    }

    /// Write a session marker (log opened/closed)
    pub fn log_session_start(&self, network: &str, channel: &str) {
        if !self.enabled {
            return;
        }

        let path = self.log_path(network, channel);

        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let now = current_datetime_string();
            let _ = writeln!(file, "--- Log opened {}", now);
        }
    }
}

/// Sanitize a string for use as a filename
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

/// Format a ChatMessage as a log line
fn format_log_line(msg: &ChatMessage) -> String {
    // Remove brackets from timestamp for cleaner logs
    let time = msg.timestamp.trim_start_matches('[').trim_end_matches(']');

    if msg.is_system {
        format!("{} -!- {}", time, msg.content)
    } else if msg.is_action {
        format!("{} * {} {}", time, msg.sender, msg.content)
    } else {
        format!("{} <{}> {}", time, msg.sender, msg.content)
    }
}

/// Parse a log line back into a ChatMessage
fn parse_log_line(line: &str) -> Option<ChatMessage> {
    // Skip session markers
    if line.starts_with("---") {
        return None;
    }

    // Parse format: "HH:MM <nick> message" or "HH:MM * nick action" or "HH:MM -!- system"
    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    if parts.len() < 2 {
        return None;
    }

    let timestamp = format!("[{}]", parts[0]);
    let rest = parts[1];

    if rest.starts_with("-!- ") {
        // System message
        Some(ChatMessage {
            timestamp,
            sender: "*".to_string(),
            content: rest[4..].to_string(),
            is_action: false,
            is_system: true,
            is_highlight: false,
        })
    } else if rest.starts_with("* ") {
        // Action message: "* nick does something"
        let action_rest = &rest[2..];
        let action_parts: Vec<&str> = action_rest.splitn(2, ' ').collect();
        if action_parts.len() >= 2 {
            Some(ChatMessage {
                timestamp,
                sender: action_parts[0].to_string(),
                content: action_parts[1].to_string(),
                is_action: true,
                is_system: false,
                is_highlight: false,
            })
        } else {
            None
        }
    } else if rest.starts_with('<') {
        // Regular message: "<nick> message"
        if let Some(end) = rest.find('>') {
            let nick = &rest[1..end];
            let content = rest[end+1..].trim_start();
            Some(ChatMessage {
                timestamp,
                sender: nick.to_string(),
                content: content.to_string(),
                is_action: false,
                is_system: false,
                is_highlight: false,
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
    let mut lines = Vec::new();
    let mut remaining_bytes = Vec::new();
    let mut pos = file_size;

    // Read backwards in chunks
    while pos > 0 && lines.len() < max_lines + 1 {
        let chunk_start = pos.saturating_sub(CHUNK_SIZE);
        let chunk_len = (pos - chunk_start) as usize;

        if file.seek(SeekFrom::Start(chunk_start)).is_err() {
            break;
        }

        let mut chunk = vec![0u8; chunk_len];
        if file.read_exact(&mut chunk).is_err() {
            break;
        }

        // Prepend to remaining bytes
        chunk.extend(remaining_bytes);
        remaining_bytes = chunk;

        // Count newlines from the end
        let mut newline_count = 0;
        for &b in remaining_bytes.iter().rev() {
            if b == b'\n' {
                newline_count += 1;
                if newline_count > max_lines {
                    break;
                }
            }
        }

        // Convert to string and split into lines
        if let Ok(text) = String::from_utf8(remaining_bytes.clone()) {
            lines = text.lines().map(|s| s.to_string()).collect();
            if lines.len() > max_lines {
                break;
            }
        }

        pos = chunk_start;
    }

    // Return only the last max_lines
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].to_vec()
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
    let month_name = ["", "Jan", "Feb", "Mar", "Apr", "May", "Jun",
                      "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][month as usize];

    format!("{} {} {:2} {:02}:{:02}:{:02} {}",
            weekday_name, month_name, day, hours, minutes, seconds, year)
}
