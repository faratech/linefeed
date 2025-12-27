//! Core types for the IRC GUI

use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use super::helpers::current_time_formatted;

/// Server favorite for quick connect
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerFavorite {
    pub name: String,           // Display name (e.g., "Libera Chat")
    pub host: String,           // Server host
    pub port: String,           // Server port
    pub use_tls: bool,          // Use TLS
    pub password: String,       // Server password (if any)
    pub nickname: String,       // Nick to use (empty = use default)
    pub auto_join: String,      // Channels to auto-join (comma-separated)
    pub auto_perform: String,   // Commands to run on connect (newline-separated)
    // SASL authentication
    #[serde(default)]
    pub sasl_username: String,
    #[serde(default)]
    pub sasl_password: String,
}

/// Persistent settings
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub server_host: String,
    pub server_port: String,
    pub use_tls: bool,
    pub accept_invalid_certs: bool,
    pub nickname: String,
    pub username: String,
    pub realname: String,
    pub password: String,
    pub auto_join_channels: String,
    pub set_invisible: bool,
    pub minimize_to_tray: bool,
    pub auto_reconnect: bool,
    pub notifications_enabled: bool,
    pub ignore_list: Vec<String>,
    pub server_favorites: Vec<ServerFavorite>,
    pub auto_perform: String,
    // Auto-away settings
    pub auto_away_enabled: bool,
    pub auto_away_minutes: u32,
    pub auto_away_message: String,
    // SASL authentication
    pub sasl_username: String,
    pub sasl_password: String,
    // Logging
    pub logging_enabled: bool,
    pub logging_load_history: bool,
    pub logging_history_lines: usize,
    // Display
    pub timestamp_format: String,  // "short" = HH:MM, "long" = HH:MM:SS, "full" = MM-DD HH:MM
    pub hide_join_part: bool,
    pub font_size: f32,
    pub max_scrollback: usize,
    // Privacy
    pub ctcp_replies_enabled: bool,
    // Highlights
    pub highlight_words: String,  // comma-separated
    // Connection
    pub reconnect_delay_secs: u32,
    pub max_reconnect_attempts: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_host: "irc.afternet.org".to_string(),
            server_port: "6697".to_string(),
            use_tls: true,
            accept_invalid_certs: false,
            nickname: String::new(),
            username: "fmirc".to_string(),
            realname: "fmIRC".to_string(),
            password: String::new(),
            auto_join_channels: String::new(),
            set_invisible: true,
            minimize_to_tray: false,
            auto_reconnect: true,
            notifications_enabled: true,
            ignore_list: Vec::new(),
            server_favorites: Vec::new(),
            auto_perform: String::new(),
            auto_away_enabled: false,
            auto_away_minutes: 10,
            auto_away_message: "Auto-away".to_string(),
            sasl_username: String::new(),
            sasl_password: String::new(),
            logging_enabled: true,
            logging_load_history: true,
            logging_history_lines: 1000,
            // Display
            timestamp_format: "short".to_string(),
            hide_join_part: false,
            font_size: 14.0,
            max_scrollback: 1000,
            // Privacy
            ctcp_replies_enabled: true,
            // Highlights
            highlight_words: String::new(),
            // Connection
            reconnect_delay_secs: 5,
            max_reconnect_attempts: 10,
        }
    }
}

impl Settings {
    fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("fmirc").join("settings.json"))
    }

    pub fn load() -> Self {
        if let Some(path) = Self::config_path() {
            if path.exists() {
                if let Ok(data) = std::fs::read_to_string(&path) {
                    if let Ok(settings) = serde_json::from_str(&data) {
                        tracing::info!("Loaded settings from {:?}", path);
                        return settings;
                    }
                }
            }
        }
        tracing::info!("Using default settings");
        Self::default()
    }

    pub fn save(&self) {
        if let Some(path) = Self::config_path() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match serde_json::to_string_pretty(self) {
                Ok(data) => {
                    if let Err(e) = std::fs::write(&path, data) {
                        tracing::error!("Failed to save settings: {}", e);
                    } else {
                        tracing::debug!("Saved settings to {:?}", path);
                    }
                }
                Err(e) => tracing::error!("Failed to serialize settings: {}", e),
            }
        }
    }
}

/// A chat message with metadata
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub timestamp: String,
    pub sender: String,
    pub content: String,
    pub is_action: bool,
    pub is_system: bool,
    pub is_highlight: bool,
}

impl ChatMessage {
    pub fn new(sender: &str, content: &str) -> Self {
        Self::new_fmt(sender, content, "short")
    }

    pub fn new_fmt(sender: &str, content: &str, format: &str) -> Self {
        Self {
            timestamp: current_time_formatted(format),
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: false,
            is_system: false,
            is_highlight: false,
        }
    }

    pub fn system(content: &str) -> Self {
        Self::system_fmt(content, "short")
    }

    pub fn system_fmt(content: &str, format: &str) -> Self {
        Self {
            timestamp: current_time_formatted(format),
            sender: "*".to_string(),
            content: content.to_string(),
            is_action: false,
            is_system: true,
            is_highlight: false,
        }
    }

    pub fn action(sender: &str, content: &str) -> Self {
        Self::action_fmt(sender, content, "short")
    }

    pub fn action_fmt(sender: &str, content: &str, format: &str) -> Self {
        Self {
            timestamp: current_time_formatted(format),
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: true,
            is_system: false,
            is_highlight: false,
        }
    }

    pub fn highlighted_fmt(sender: &str, content: &str, format: &str) -> Self {
        Self {
            timestamp: current_time_formatted(format),
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: false,
            is_system: false,
            is_highlight: true,
        }
    }

    pub fn action_highlighted_fmt(sender: &str, content: &str, format: &str) -> Self {
        Self {
            timestamp: current_time_formatted(format),
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: true,
            is_system: false,
            is_highlight: true,
        }
    }
}

/// User mode in a channel (determines prefix and sort order)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UserMode {
    Owner,    // ~
    Admin,    // &
    Op,       // @
    HalfOp,   // %
    Voice,    // +
    Normal,
}

impl UserMode {
    pub fn from_prefix(c: char) -> Option<Self> {
        match c {
            '~' => Some(UserMode::Owner),
            '&' => Some(UserMode::Admin),
            '@' => Some(UserMode::Op),
            '%' => Some(UserMode::HalfOp),
            '+' => Some(UserMode::Voice),
            _ => None,
        }
    }

    pub fn prefix(&self) -> &'static str {
        match self {
            UserMode::Owner => "~",
            UserMode::Admin => "&",
            UserMode::Op => "@",
            UserMode::HalfOp => "%",
            UserMode::Voice => "+",
            UserMode::Normal => "",
        }
    }
}

/// Ban list entry
#[derive(Debug, Clone)]
pub struct BanEntry {
    pub mask: String,
    pub set_by: String,
    pub set_time: u64,
}

/// An IRC channel with users and messages
#[derive(Debug, Clone)]
pub struct Channel {
    pub topic: Option<String>,
    pub topic_set_by: Option<String>,
    pub topic_set_time: Option<u64>,
    pub modes: String,  // Channel modes like "+nt"
    pub mode_params: Vec<String>,  // Mode parameters (limit, key, etc.)
    pub created: Option<u64>,  // Channel creation timestamp
    pub users: Vec<(String, UserMode)>,
    pub messages: Vec<ChatMessage>,
    pub unread: usize,
    pub key: Option<String>,  // Channel key for auto-rejoin
    pub bans: Vec<BanEntry>,
    pub ban_list_complete: bool,
}

impl Channel {
    pub fn new() -> Self {
        Self {
            topic: None,
            topic_set_by: None,
            topic_set_time: None,
            modes: String::new(),
            mode_params: Vec::new(),
            created: None,
            users: Vec::new(),
            messages: Vec::new(),
            unread: 0,
            key: None,
            bans: Vec::new(),
            ban_list_complete: false,
        }
    }

    pub fn add_user(&mut self, nick: &str, mode: UserMode) {
        let clean_nick = nick.trim_start_matches(|c| c == '~' || c == '&' || c == '@' || c == '%' || c == '+');
        if !self.users.iter().any(|(n, _)| n.eq_ignore_ascii_case(clean_nick)) {
            self.users.push((clean_nick.to_string(), mode));
            self.sort_users();
        }
    }

    pub fn remove_user(&mut self, nick: &str) {
        self.users.retain(|(n, _)| !n.eq_ignore_ascii_case(nick));
    }

    pub fn rename_user(&mut self, old_nick: &str, new_nick: &str) {
        if let Some(pos) = self.users.iter().position(|(n, _)| n.eq_ignore_ascii_case(old_nick)) {
            let mode = self.users[pos].1;
            self.users[pos] = (new_nick.to_string(), mode);
            self.sort_users();
        }
    }

    pub fn has_user(&self, nick: &str) -> bool {
        self.users.iter().any(|(n, _)| n.eq_ignore_ascii_case(nick))
    }

    fn sort_users(&mut self) {
        self.users.sort_by(|(a_nick, a_mode), (b_nick, b_mode)| {
            a_mode.cmp(b_mode).then_with(|| a_nick.to_lowercase().cmp(&b_nick.to_lowercase()))
        });
    }
}

/// Entry in the channel list dialog
#[derive(Debug, Clone)]
pub struct ChannelListEntry {
    pub name: String,
    pub user_count: usize,
    pub topic: String,
}

/// Tab completion state
#[derive(Debug, Clone)]
pub struct TabCompletion {
    pub prefix: String,
    pub matches: Vec<String>,
    pub index: usize,
}
