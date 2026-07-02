//! Core types for the IRC GUI

use super::formatting::{RenderSegment, layout_irc_text, strip_irc_formatting};
use super::helpers::current_time_formatted;
use serde::{Deserialize, Serialize};
use std::cell::OnceCell;
use std::collections::VecDeque;
use std::path::PathBuf;

/// Sort column for channel list
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ChannelListSort {
    #[default]
    Channel,
    Users,
    Topic,
}

/// Sort direction
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum SortDirection {
    #[default]
    Ascending,
    Descending,
}

/// Server favorite for quick connect
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerFavorite {
    pub name: String,         // Display name (e.g., "Libera Chat")
    pub host: String,         // Server host
    pub port: String,         // Server port
    pub use_tls: bool,        // Use TLS
    pub password: String,     // Server password (if any)
    pub nickname: String,     // Nick to use (empty = use default)
    pub auto_join: String,    // Channels to auto-join (comma-separated)
    pub auto_perform: String, // Commands to run on connect (newline-separated)
    // SASL authentication
    #[serde(default)]
    pub sasl_username: String,
    #[serde(default)]
    pub sasl_password: String,
    // Connection details that were previously dropped when saving a favorite
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub realname: String,
    #[serde(default)]
    pub accept_invalid_certs: bool,
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
    pub timestamp_format: String, // "short" = HH:MM, "long" = HH:MM:SS, "full" = MM-DD HH:MM
    pub hide_join_part: bool,
    pub font_size: f32,
    pub max_scrollback: usize,
    // Privacy
    pub ctcp_replies_enabled: bool,
    // Highlights
    pub highlight_words: String, // comma-separated
    // Connection
    pub reconnect_delay_secs: u32,
    pub max_reconnect_attempts: u32,
    // Custom messages
    pub quit_message: String,
    pub part_message: String,
    // Channel list
    pub list_min_users: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_host: "irc.afternet.org".to_string(),
            server_port: "6697".to_string(),
            use_tls: true,
            accept_invalid_certs: false,
            nickname: String::new(),
            username: "linefeed".to_string(),
            realname: "Linefeed".to_string(),
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
            // Custom messages
            quit_message: "Linefeed".to_string(),
            part_message: String::new(),
            // Channel list
            list_min_users: 0,
        }
    }
}

impl Settings {
    fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("linefeed").join("settings.json"))
    }

    pub fn load() -> Self {
        if let Some(path) = Self::config_path()
            && path.exists()
            && let Ok(data) = std::fs::read_to_string(&path)
            && let Ok(settings) = serde_json::from_str(&data)
        {
            tracing::info!("Loaded settings from {:?}", path);
            return settings;
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
    /// Parsed formatting + URL segments, computed lazily on first render.
    /// Content never changes after construction, so the cache never invalidates.
    pub render_cache: OnceCell<Vec<RenderSegment>>,
}

impl ChatMessage {
    /// Render-ready segments for this message, parsed once and cached.
    pub fn render_segments(&self) -> &[RenderSegment] {
        self.render_cache
            .get_or_init(|| layout_irc_text(&self.content))
    }

    pub fn new_fmt(sender: &str, content: &str, format: &str) -> Self {
        Self {
            timestamp: current_time_formatted(format),
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: false,
            is_system: false,
            is_highlight: false,
            render_cache: OnceCell::new(),
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
            render_cache: OnceCell::new(),
        }
    }

    pub fn action_fmt(sender: &str, content: &str, format: &str) -> Self {
        Self {
            timestamp: current_time_formatted(format),
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: true,
            is_system: false,
            is_highlight: false,
            render_cache: OnceCell::new(),
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
            render_cache: OnceCell::new(),
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
            render_cache: OnceCell::new(),
        }
    }

    /// Override timestamp with server-provided time (IRCv3 server-time)
    /// If server_time is Some, use it; otherwise keep existing timestamp
    pub fn with_server_time(mut self, server_time: Option<String>) -> Self {
        if let Some(time) = server_time {
            self.timestamp = time;
        }
        self
    }
}

/// User mode in a channel (determines prefix and sort order)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UserMode {
    Owner,  // ~
    Admin,  // &
    Op,     // @
    HalfOp, // %
    Voice,  // +
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

/// Server-advertised IRC support that affects routing and user prefixes.
#[derive(Debug, Clone)]
pub struct NetworkSupport {
    pub channel_types: String,
    pub user_prefixes: String,
    /// Mode letters corresponding position-wise to `user_prefixes`
    /// (from ISUPPORT `PREFIX=(modes)symbols`).
    pub prefix_modes: String,
    /// ISUPPORT CHANMODES groups. They decide which channel modes consume a
    /// parameter, which is essential to keep MODE parameters aligned:
    /// type A (lists; parameter in both directions), type B (parameter in both
    /// directions), type C (parameter only when set), type D (never).
    pub chanmodes_a: String,
    pub chanmodes_b: String,
    pub chanmodes_c: String,
}

impl Default for NetworkSupport {
    fn default() -> Self {
        Self {
            channel_types: "#&+!".to_string(),
            user_prefixes: "~&@%+".to_string(),
            prefix_modes: "qaohv".to_string(),
            chanmodes_a: "beI".to_string(),
            chanmodes_b: "k".to_string(),
            chanmodes_c: "l".to_string(),
        }
    }
}

impl NetworkSupport {
    pub fn is_channel(&self, name: &str) -> bool {
        name.chars()
            .next()
            .is_some_and(|c| self.channel_types.contains(c))
    }

    /// Map a PREFIX mode letter (e.g. 'o') to its user mode via the
    /// position-matched prefix symbol (e.g. '@').
    pub fn user_mode_for_letter(&self, letter: char) -> Option<UserMode> {
        let idx = self.prefix_modes.chars().position(|c| c == letter)?;
        self.user_prefixes
            .chars()
            .nth(idx)
            .and_then(UserMode::from_prefix)
    }
}

/// Ban list entry
#[derive(Debug, Clone)]
pub struct BanEntry {
    pub mask: String,
    pub set_by: String,
    pub set_time: u64,
}

/// A user in a channel with mode and away status
#[derive(Debug, Clone)]
pub struct ChannelUser {
    pub nick: String,
    pub mode: UserMode,
    pub away: Option<String>, // None = not away, Some(msg) = away with message
    pub account: Option<String>, // IRCv3 account name
}

impl ChannelUser {
    pub fn new(nick: String, mode: UserMode) -> Self {
        Self {
            nick,
            mode,
            away: None,
            account: None,
        }
    }

    pub fn is_away(&self) -> bool {
        self.away.is_some()
    }
}

/// An IRC channel with users and messages
#[derive(Debug, Clone)]
pub struct Channel {
    pub topic: Option<String>,
    pub topic_set_by: Option<String>,
    pub topic_set_time: Option<u64>,
    pub modes: String,            // Channel modes like "+nt"
    pub mode_params: Vec<String>, // Mode parameters (limit, key, etc.)
    pub created: Option<u64>,     // Channel creation timestamp
    pub users: Vec<ChannelUser>,
    pub messages: VecDeque<ChatMessage>,
    pub unread: usize,
    pub key: Option<String>, // Channel key for auto-rejoin
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
            messages: VecDeque::new(),
            unread: 0,
            key: None,
            bans: Vec::new(),
            ban_list_complete: false,
        }
    }

    pub fn add_user(&mut self, nick: &str, mode: UserMode) {
        self.add_user_with_prefixes(nick, mode, "~&@%+");
    }

    pub fn add_user_with_prefixes(&mut self, nick: &str, mode: UserMode, prefixes: &str) {
        let clean_nick = nick.trim_start_matches(|c| prefixes.contains(c));
        // Ignore tokens that are only mode prefixes (e.g. a stray "@"): they would
        // otherwise insert a blank-nick, unremovable user into the list.
        if clean_nick.is_empty() {
            return;
        }
        if !self
            .users
            .iter()
            .any(|u| u.nick.eq_ignore_ascii_case(clean_nick))
        {
            self.users
                .push(ChannelUser::new(clean_nick.to_string(), mode));
            self.sort_users();
        }
    }

    /// Insert a user without sorting or duplicate checking. Used while streaming
    /// a NAMES burst, where a per-insert sort would make a large join O(N²·logN);
    /// the caller MUST invoke `finish_user_burst` when the burst completes
    /// (RPL_ENDOFNAMES) to dedup and sort once.
    pub fn add_user_burst(&mut self, nick: &str, mode: UserMode, prefixes: &str) {
        let clean_nick = nick.trim_start_matches(|c| prefixes.contains(c));
        if clean_nick.is_empty() {
            return;
        }
        self.users
            .push(ChannelUser::new(clean_nick.to_string(), mode));
    }

    /// Dedup (keeping the first, state-carrying entry) and sort after a NAMES
    /// burst. Safe to call even if no burst inserts happened.
    pub fn finish_user_burst(&mut self) {
        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(self.users.len());
        self.users.retain(|u| seen.insert(u.nick.to_lowercase()));
        self.sort_users();
    }

    pub fn set_user_mode(&mut self, nick: &str, mode: UserMode) {
        if let Some(user) = self.get_user_mut(nick) {
            user.mode = mode;
            self.sort_users();
        }
    }

    pub fn apply_channel_mode_flag(&mut self, sign: char, mode: char) {
        if sign == '+' {
            if !self.modes.contains(mode) {
                if self.modes.starts_with('+') {
                    self.modes.push(mode);
                } else {
                    self.modes = format!("+{}", mode);
                }
            }
        } else if sign == '-' {
            self.modes.retain(|c| c != mode);
            if self.modes == "+" {
                self.modes.clear();
            }
        }
    }

    pub fn remove_user(&mut self, nick: &str) {
        self.users.retain(|u| !u.nick.eq_ignore_ascii_case(nick));
    }

    pub fn rename_user(&mut self, old_nick: &str, new_nick: &str) {
        // If the target nick already exists as a different user, drop the old entry
        // rather than creating a duplicate (can happen with out-of-order NICK/JOIN).
        // A pure case change of the same nick still falls through to the rename.
        if !old_nick.eq_ignore_ascii_case(new_nick)
            && self
                .users
                .iter()
                .any(|u| u.nick.eq_ignore_ascii_case(new_nick))
        {
            self.users
                .retain(|u| !u.nick.eq_ignore_ascii_case(old_nick));
            self.sort_users();
            return;
        }
        if let Some(user) = self
            .users
            .iter_mut()
            .find(|u| u.nick.eq_ignore_ascii_case(old_nick))
        {
            user.nick = new_nick.to_string();
            self.sort_users();
        }
    }

    /// Append a message, trimming the scrollback to at most `max` messages.
    /// Used by callers that push directly to `messages` (e.g. Quit/Nick loops
    /// over all channels) so they respect the same cap as add_message_to_channel.
    pub fn push_trimmed(&mut self, msg: ChatMessage, max: usize) {
        self.messages.push_back(msg);
        if max > 0 {
            while self.messages.len() > max {
                self.messages.pop_front();
            }
        }
    }

    pub fn has_user(&self, nick: &str) -> bool {
        self.get_user(nick).is_some()
    }

    pub fn get_user(&self, nick: &str) -> Option<&ChannelUser> {
        self.users
            .iter()
            .find(|u| u.nick.eq_ignore_ascii_case(nick))
    }

    pub fn get_user_mut(&mut self, nick: &str) -> Option<&mut ChannelUser> {
        self.users
            .iter_mut()
            .find(|u| u.nick.eq_ignore_ascii_case(nick))
    }

    fn sort_users(&mut self) {
        // Case-insensitive comparison without allocating per comparison (nick
        // casing on IRC is ASCII-folded; full Unicode folding is unnecessary).
        self.users.sort_by(|a, b| {
            a.mode.cmp(&b.mode).then_with(|| {
                let a_bytes = a.nick.as_bytes().iter().map(u8::to_ascii_lowercase);
                let b_bytes = b.nick.as_bytes().iter().map(u8::to_ascii_lowercase);
                a_bytes.cmp(b_bytes)
            })
        });
    }

    /// IRCv3 away-notify: update user's away status
    pub fn set_user_away(&mut self, nick: &str, away_msg: Option<String>) {
        if let Some(user) = self.get_user_mut(nick) {
            user.away = away_msg;
        }
    }

    /// IRCv3 account-notify: update user's logged-in account
    pub fn set_user_account(&mut self, nick: &str, account: Option<String>) {
        if let Some(user) = self.get_user_mut(nick) {
            user.account = account;
        }
    }

    /// Get count of away users
    pub fn away_count(&self) -> usize {
        self.users.iter().filter(|u| u.is_away()).count()
    }
}

/// Entry in the channel list dialog. Derived keys (lowercase name, stripped
/// topic) are computed once at insert so filtering and sorting up to 50k rows
/// is allocation-free.
#[derive(Debug, Clone)]
pub struct ChannelListEntry {
    pub name: String,
    pub user_count: usize,
    /// Lowercased name, for filtering and sorting.
    pub name_lower: String,
    /// Topic with IRC formatting stripped, for display.
    pub topic_clean: String,
    /// Lowercased stripped topic, for filtering and sorting.
    pub topic_lower: String,
}

impl ChannelListEntry {
    pub fn new(name: String, user_count: usize, topic: String) -> Self {
        let name_lower = name.to_lowercase();
        let topic_clean = strip_irc_formatting(&topic);
        let topic_lower = topic_clean.to_lowercase();
        Self {
            name,
            user_count,
            name_lower,
            topic_clean,
            topic_lower,
        }
    }
}

/// Tab completion state
#[derive(Debug, Clone)]
pub struct TabCompletion {
    pub prefix: String,
    pub matches: Vec<String>,
    pub index: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_burst_dedups_and_sorts_once() {
        let mut ch = Channel::new();
        ch.add_user("zed", UserMode::Normal);
        ch.set_user_away("zed", Some("afk".to_string()));
        // A NAMES refresh streams in, including a duplicate of the existing user.
        ch.add_user_burst("@alice", UserMode::Op, "~&@%+");
        ch.add_user_burst("ZED", UserMode::Normal, "~&@%+");
        ch.add_user_burst("bob", UserMode::Normal, "~&@%+");
        ch.finish_user_burst();

        let nicks: Vec<&str> = ch.users.iter().map(|u| u.nick.as_str()).collect();
        assert_eq!(nicks, vec!["alice", "bob", "zed"]);
        // The original, state-carrying entry survived the dedup.
        assert!(ch.get_user("zed").unwrap().is_away());
    }

    #[test]
    fn push_trimmed_caps_scrollback() {
        let mut ch = Channel::new();
        for i in 0..10 {
            ch.push_trimmed(ChatMessage::system(&format!("m{}", i)), 5);
        }
        assert_eq!(ch.messages.len(), 5);
        assert_eq!(ch.messages.front().unwrap().content, "m5");
        assert_eq!(ch.messages.back().unwrap().content, "m9");
    }

    #[test]
    fn channel_list_entry_precomputes_keys() {
        let e = ChannelListEntry::new("#Rust".into(), 42, "\x0304Hot\x03 topic".into());
        assert_eq!(e.name_lower, "#rust");
        assert_eq!(e.topic_clean, "Hot topic");
        assert_eq!(e.topic_lower, "hot topic");
    }
}
