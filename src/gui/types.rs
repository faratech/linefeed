//! Core types for the IRC GUI

use super::formatting::{RenderSegment, layout_irc_text, strip_irc_formatting};
use super::helpers::current_time_formatted;
use crate::irc::client::SaslMechanism;
use precis_profiles::{UsernameCaseMapped, precis_core::profile::Profile};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, OnceCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

thread_local! {
    /// `ChatMessage::system` is used throughout GUI, command, and top-level
    /// lifecycle code. Keep its configured format on the GUI thread so every
    /// local/system message follows the user's setting without global test
    /// races or threading a format argument through every call site.
    static DEFAULT_SYSTEM_TIMESTAMP_FORMAT: Cell<u8> = const { Cell::new(0) };
}

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
    #[serde(default)]
    pub sasl_mechanism: SaslMechanism,
    #[serde(default)]
    pub sasl_required: bool,
    #[serde(default)]
    pub tls_client_cert_path: String,
    #[serde(default)]
    pub tls_client_key_path: String,
    // Connection details that were previously dropped when saving a favorite
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub realname: String,
    #[serde(default)]
    pub accept_invalid_certs: bool,
    /// IRCv3 draft/pre-away value sent before registration completes.
    #[serde(default)]
    pub pre_away_message: String,
    /// Nefarious draft/persistence profile attached during registration.
    #[serde(default)]
    pub persistence_profile: String,
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
    pub pre_away_message: String,
    pub persistence_profile: String,
    // Auto-away settings
    pub auto_away_enabled: bool,
    pub auto_away_minutes: u32,
    pub auto_away_message: String,
    // SASL authentication
    pub sasl_username: String,
    pub sasl_password: String,
    pub sasl_mechanism: SaslMechanism,
    pub sasl_required: bool,
    pub tls_client_cert_path: String,
    pub tls_client_key_path: String,
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
            pre_away_message: String::new(),
            persistence_profile: String::new(),
            auto_away_enabled: false,
            auto_away_minutes: 10,
            auto_away_message: "Auto-away".to_string(),
            sasl_username: String::new(),
            sasl_password: String::new(),
            sasl_mechanism: SaslMechanism::Auto,
            sasl_required: false,
            tls_client_cert_path: String::new(),
            tls_client_key_path: String::new(),
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
        {
            match std::fs::read_to_string(&path) {
                Ok(data) => match serde_json::from_str(&data) {
                    Ok(settings) => {
                        tracing::info!("Loaded settings from {:?}", path);
                        return settings;
                    }
                    Err(e) => {
                        // Preserve the unreadable file instead of leaving it to be
                        // overwritten: starting with defaults and then saving would
                        // permanently destroy every server favorite (and stored
                        // password) after e.g. a torn write.
                        tracing::error!("Failed to parse settings file {:?}: {}", path, e);
                        let backup = path.with_extension("json.bak");
                        match std::fs::rename(&path, &backup) {
                            Ok(_) => {
                                tracing::warn!("Unreadable settings file backed up to {:?}", backup)
                            }
                            Err(e) => tracing::error!("Failed to back up settings: {}", e),
                        }
                    }
                },
                Err(e) => tracing::error!("Failed to read settings file {:?}: {}", path, e),
            }
        }
        tracing::info!("Using default settings");
        Self::default()
    }

    pub fn save(&self) {
        let Some(path) = Self::config_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
            // The directory holds plaintext credentials (settings and logs);
            // keep it private on multi-user systems.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        let data = match serde_json::to_string_pretty(self) {
            Ok(data) => data,
            Err(e) => {
                tracing::error!("Failed to serialize settings: {}", e);
                return;
            }
        };
        // Write to a temp file and rename over the original: a crash or power
        // loss mid-write can then never leave settings.json truncated.
        let tmp = path.with_extension("json.tmp");
        let write_result = write_settings_file(&tmp, &data);
        match write_result.and_then(|_| std::fs::rename(&tmp, &path)) {
            Ok(_) => tracing::debug!("Saved settings to {:?}", path),
            Err(e) => {
                tracing::error!("Failed to save settings: {}", e);
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}

/// Write the settings payload to `tmp`, private (0600) on Unix since it
/// contains server and SASL passwords, and fsynced so the atomic rename that
/// follows lands on durable bytes.
#[cfg(unix)]
fn write_settings_file(tmp: &std::path::Path, data: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(tmp)?;
    file.write_all(data.as_bytes())?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_settings_file(tmp: &std::path::Path, data: &str) -> std::io::Result<()> {
    std::fs::write(tmp, data)
}

/// Cached scrollback row height for one message, valid for one layout key
/// (a hash of the content width and font sizes). A zero height means "never
/// measured".
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RowHeightEntry {
    pub key: u64,
    pub height: f32,
}

/// A chat message with metadata
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub timestamp: String,
    /// Canonical IRCv3 `time` tag retained for ordering and MARKREAD.
    pub server_time: Option<String>,
    /// Stable IRCv3 message identity retained for replay deduplication and REDACT.
    pub msgid: Option<String>,
    /// Authenticated sender identity from the IRCv3 `account` tag.
    pub account: Option<String>,
    /// Operator name from Nefarious `draft/oper-tag`.
    pub oper: Option<String>,
    /// Referenced message ID from `+reply` / legacy `+draft/reply`.
    pub reply_to: Option<String>,
    pub sender: String,
    pub content: String,
    pub is_action: bool,
    pub is_system: bool,
    pub is_highlight: bool,
    /// Display-only message (e.g. /lastlog results, /help): excluded from the
    /// on-disk chat log and from /lastlog searches so search output does not
    /// pollute history or match itself on repeated searches.
    pub no_log: bool,
    /// Parsed formatting + URL segments, computed lazily on first render.
    /// Content never changes after construction, so the cache never invalidates.
    pub render_cache: OnceCell<Vec<RenderSegment>>,
    /// Measured scrollback row height, lazily filled by the virtualized
    /// message list and invalidated whenever the layout key changes.
    pub row_height: Cell<RowHeightEntry>,
}

impl ChatMessage {
    pub fn configure_default_timestamp_format(format: &str) {
        let code = match format {
            "long" => 1,
            "full" => 2,
            _ => 0,
        };
        DEFAULT_SYSTEM_TIMESTAMP_FORMAT.set(code);
    }

    fn default_timestamp_format() -> &'static str {
        DEFAULT_SYSTEM_TIMESTAMP_FORMAT.with(|format| match format.get() {
            1 => "long",
            2 => "full",
            _ => "short",
        })
    }

    /// Render-ready segments for this message, parsed once and cached.
    pub fn render_segments(&self) -> &[RenderSegment] {
        self.render_cache
            .get_or_init(|| layout_irc_text(&self.content))
    }

    /// Mark this message as display-only (not logged, not searchable).
    pub fn without_logging(mut self) -> Self {
        self.no_log = true;
        self
    }

    pub fn new_fmt(sender: &str, content: &str, format: &str) -> Self {
        Self {
            timestamp: current_time_formatted(format),
            server_time: None,
            msgid: None,
            account: None,
            oper: None,
            reply_to: None,
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: false,
            is_system: false,
            is_highlight: false,
            no_log: false,
            render_cache: OnceCell::new(),
            row_height: Cell::default(),
        }
    }

    pub fn system(content: &str) -> Self {
        Self::system_fmt(content, Self::default_timestamp_format())
    }

    pub fn system_fmt(content: &str, format: &str) -> Self {
        Self {
            timestamp: current_time_formatted(format),
            server_time: None,
            msgid: None,
            account: None,
            oper: None,
            reply_to: None,
            sender: "*".to_string(),
            content: content.to_string(),
            is_action: false,
            is_system: true,
            is_highlight: false,
            no_log: false,
            render_cache: OnceCell::new(),
            row_height: Cell::default(),
        }
    }

    pub fn action_fmt(sender: &str, content: &str, format: &str) -> Self {
        Self {
            timestamp: current_time_formatted(format),
            server_time: None,
            msgid: None,
            account: None,
            oper: None,
            reply_to: None,
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: true,
            is_system: false,
            is_highlight: false,
            no_log: false,
            render_cache: OnceCell::new(),
            row_height: Cell::default(),
        }
    }

    pub fn highlighted_fmt(sender: &str, content: &str, format: &str) -> Self {
        Self {
            timestamp: current_time_formatted(format),
            server_time: None,
            msgid: None,
            account: None,
            oper: None,
            reply_to: None,
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: false,
            is_system: false,
            is_highlight: true,
            no_log: false,
            render_cache: OnceCell::new(),
            row_height: Cell::default(),
        }
    }

    pub fn action_highlighted_fmt(sender: &str, content: &str, format: &str) -> Self {
        Self {
            timestamp: current_time_formatted(format),
            server_time: None,
            msgid: None,
            account: None,
            oper: None,
            reply_to: None,
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: true,
            is_system: false,
            is_highlight: true,
            no_log: false,
            render_cache: OnceCell::new(),
            row_height: Cell::default(),
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

    pub fn with_irc_metadata(
        mut self,
        server_time: Option<String>,
        msgid: Option<String>,
        account: Option<String>,
    ) -> Self {
        self.server_time = server_time;
        self.msgid = msgid;
        self.account = account;
        self
    }

    pub fn with_oper(mut self, oper: Option<String>) -> Self {
        self.oper = oper;
        self
    }

    pub fn with_reply_to(mut self, reply_to: Option<String>) -> Self {
        self.reply_to = reply_to;
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
    fn mode_letter(self) -> Option<char> {
        match self {
            UserMode::Owner => Some('q'),
            UserMode::Admin => Some('a'),
            UserMode::Op => Some('o'),
            UserMode::HalfOp => Some('h'),
            UserMode::Voice => Some('v'),
            UserMode::Normal => None,
        }
    }

    pub fn from_mode_letter(c: char) -> Option<Self> {
        match c {
            'q' => Some(UserMode::Owner),
            'a' => Some(UserMode::Admin),
            'o' => Some(UserMode::Op),
            'h' => Some(UserMode::HalfOp),
            'v' => Some(UserMode::Voice),
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

/// IRC identifier case folding advertised by ISUPPORT CASEMAPPING.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseMapping {
    Ascii,
    StrictRfc1459,
    #[default]
    Rfc1459,
    /// Unicode identifier comparison requested by `UTF8MAPPING=rfc8265`.
    Utf8Rfc8265,
}

impl CaseMapping {
    /// Produce a stable key for an IRC nickname or channel. IRC casemapping is
    /// deliberately ASCII-only; Unicode text outside the IRC identifier range
    /// must not acquire locale- or Unicode-dependent equivalences.
    pub fn canonicalize(self, value: &str) -> String {
        if self == CaseMapping::Utf8Rfc8265 {
            // Invalid PRECIS input must remain distinct instead of being folded
            // into another identifier. Servers should not emit it, but ASCII
            // folding is a deterministic and conservative fallback.
            return UsernameCaseMapped::new()
                .enforce(value)
                .map(|value| value.into_owned())
                .unwrap_or_else(|_| value.to_ascii_lowercase());
        }
        value
            .chars()
            .map(|c| match c {
                'A'..='Z' => c.to_ascii_lowercase(),
                '[' if self != CaseMapping::Ascii => '{',
                ']' if self != CaseMapping::Ascii => '}',
                '\\' if self != CaseMapping::Ascii => '|',
                '^' if self == CaseMapping::Rfc1459 => '~',
                other => other,
            })
            .collect()
    }

    pub fn eq(self, left: &str, right: &str) -> bool {
        self.canonicalize(left) == self.canonicalize(right)
    }
}

/// Server-advertised IRC support that affects routing and user prefixes.
#[derive(Debug, Clone)]
pub struct NetworkSupport {
    /// Complete, case-normalized ISUPPORT state. `None` represents a flag
    /// token and `Some` contains the value after `=`.
    pub isupport: HashMap<String, Option<String>>,
    pub case_mapping: CaseMapping,
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
    pub status_prefixes: String,
    pub history_reference_types: String,
    pub history_limit: Option<usize>,
    pub history_retention_secs: Option<u64>,
    pub nick_len: Option<usize>,
    pub channel_len: Option<usize>,
    pub topic_len: Option<usize>,
    pub away_len: Option<usize>,
    pub kick_len: Option<usize>,
    pub monitor_limit: Option<usize>,
    pub watch_limit: Option<usize>,
    pub silence_limit: Option<usize>,
    pub whox: bool,
    pub elist: String,
    pub safelist: bool,
    pub line_len: Option<usize>,
    pub client_tag_deny: Option<String>,
    pub bot_mode: Option<char>,
    pub except_mode: Option<char>,
    pub invex_mode: Option<char>,
    pub extban: Option<String>,
    pub network_name: Option<String>,
    pub utf8_only: bool,
    pub account_required: bool,
    pub multiline_max_bytes: Option<usize>,
    pub multiline_max_lines: Option<usize>,
}

impl Default for NetworkSupport {
    fn default() -> Self {
        Self {
            isupport: HashMap::new(),
            case_mapping: CaseMapping::Rfc1459,
            channel_types: "#&+!".to_string(),
            user_prefixes: "~&@%+".to_string(),
            prefix_modes: "qaohv".to_string(),
            chanmodes_a: "beI".to_string(),
            chanmodes_b: "k".to_string(),
            chanmodes_c: "l".to_string(),
            status_prefixes: "@%+".to_string(),
            history_reference_types: String::new(),
            history_limit: None,
            history_retention_secs: None,
            nick_len: None,
            channel_len: None,
            topic_len: None,
            away_len: None,
            kick_len: None,
            monitor_limit: None,
            watch_limit: None,
            silence_limit: None,
            whox: false,
            elist: String::new(),
            safelist: false,
            line_len: None,
            client_tag_deny: None,
            bot_mode: None,
            except_mode: None,
            invex_mode: None,
            extban: None,
            network_name: None,
            utf8_only: false,
            account_required: false,
            multiline_max_bytes: None,
            multiline_max_lines: None,
        }
    }
}

impl NetworkSupport {
    pub fn has_isupport(&self, name: &str) -> bool {
        self.isupport.contains_key(&name.to_ascii_uppercase())
    }

    pub fn canonicalize(&self, value: &str) -> String {
        if self.case_mapping == CaseMapping::Utf8Rfc8265
            && let Some((first, rest)) = value
                .char_indices()
                .next()
                .filter(|(_, first)| self.channel_types.contains(*first))
                .map(|(index, first)| (first, &value[index + first.len_utf8()..]))
        {
            return format!("{first}{}", self.case_mapping.canonicalize(rest));
        }
        self.case_mapping.canonicalize(value)
    }

    pub fn identifiers_equal(&self, left: &str, right: &str) -> bool {
        self.canonicalize(left) == self.canonicalize(right)
    }

    pub fn is_channel(&self, name: &str) -> bool {
        name.chars()
            .next()
            .is_some_and(|c| self.channel_types.contains(c))
    }

    /// Split all advertised status prefixes from a NAMES token. With the
    /// IRCv3 multi-prefix capability a user can carry several prefixes (for
    /// example `@+alice`), all of which must survive later MODE changes.
    pub fn parse_prefixed_nick<'a>(&self, token: &'a str) -> (&'a str, Vec<char>) {
        let mut end = 0;
        let mut modes = Vec::new();
        for (idx, c) in token.char_indices() {
            let Some(prefix_idx) = self.user_prefixes.chars().position(|p| p == c) else {
                break;
            };
            end = idx + c.len_utf8();
            if let Some(letter) = self.prefix_modes.chars().nth(prefix_idx)
                && !modes.contains(&letter)
            {
                modes.push(letter);
            }
        }
        (&token[end..], modes)
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
    mode_letters: Vec<char>,
    display_prefix: String,
    rank: usize,
    pub away: Option<String>, // None = not away, Some(msg) = away with message
    pub account: Option<String>, // IRCv3 account name
    pub realname: Option<String>, // IRCv3 extended-join / setname value
    pub username: Option<String>,
    pub hostname: Option<String>,
    pub bot: bool,
}

/// Identity and presence data shared across every channel membership and
/// retained for MONITOR/WATCH users who do not share a channel with us.
#[derive(Debug, Clone, Default)]
pub struct KnownUser {
    pub nick: String,
    pub username: Option<String>,
    pub hostname: Option<String>,
    pub account: Option<String>,
    pub realname: Option<String>,
    pub away: Option<String>,
    pub bot: bool,
    pub online: bool,
}

impl ChannelUser {
    pub fn new(nick: String, mode: UserMode) -> Self {
        let mode_letter = mode.mode_letter();
        Self {
            nick,
            mode,
            mode_letters: mode_letter.into_iter().collect(),
            display_prefix: mode.prefix().to_string(),
            rank: match mode {
                UserMode::Owner => 0,
                UserMode::Admin => 1,
                UserMode::Op => 2,
                UserMode::HalfOp => 3,
                UserMode::Voice => 4,
                UserMode::Normal => usize::MAX,
            },
            away: None,
            account: None,
            realname: None,
            username: None,
            hostname: None,
            bot: false,
        }
    }

    pub fn is_away(&self) -> bool {
        self.away.is_some()
    }

    #[cfg(test)]
    pub fn has_mode(&self, mode: UserMode) -> bool {
        if mode == UserMode::Normal {
            self.mode_letters.is_empty()
        } else {
            mode.mode_letter()
                .is_some_and(|letter| self.mode_letters.contains(&letter))
        }
    }

    pub fn prefix(&self) -> &str {
        &self.display_prefix
    }

    fn add_status_mode(&mut self, letter: char, modes: &str, prefixes: &str) {
        if !self.mode_letters.contains(&letter) {
            self.mode_letters.push(letter);
        }
        self.refresh_display_mode(modes, prefixes);
    }

    fn remove_status_mode(&mut self, letter: char, modes: &str, prefixes: &str) {
        self.mode_letters.retain(|existing| *existing != letter);
        self.refresh_display_mode(modes, prefixes);
    }

    fn refresh_display_mode(&mut self, modes: &str, prefixes: &str) {
        let selected = modes
            .chars()
            .enumerate()
            .find(|(_, letter)| self.mode_letters.contains(letter));
        if let Some((rank, letter)) = selected {
            self.rank = rank;
            self.display_prefix = prefixes
                .chars()
                .nth(rank)
                .map(String::from)
                .unwrap_or_default();
            self.mode = UserMode::from_mode_letter(letter).unwrap_or(UserMode::Normal);
        } else {
            self.rank = usize::MAX;
            self.display_prefix.clear();
            self.mode = UserMode::Normal;
        }
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
    /// Server-defined CHANMODES type-A lists, keyed by mode letter. The
    /// dedicated ban fields remain for settings compatibility and common UI.
    pub mode_lists: HashMap<char, Vec<BanEntry>>,
    pub mode_lists_complete: HashSet<char>,
    pub read_marker: Option<String>,
    /// Whether the local user is currently a member. Query windows are not
    /// channels and therefore leave this false without affecting sends.
    pub joined: bool,
    case_mapping: CaseMapping,
    prefix_modes: String,
    prefix_symbols: String,
    names_snapshot: Option<Vec<ChannelUser>>,
    /// When the newest 353 of the pending burst arrived; a 353 arriving long
    /// after it means the previous burst was abandoned and must not be
    /// appended to.
    burst_activity: Option<std::time::Instant>,
    mode_parameters: HashMap<char, String>,
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
            mode_lists: HashMap::new(),
            mode_lists_complete: HashSet::new(),
            read_marker: None,
            joined: false,
            case_mapping: CaseMapping::Rfc1459,
            prefix_modes: "qaohv".to_string(),
            prefix_symbols: "~&@%+".to_string(),
            names_snapshot: None,
            burst_activity: None,
            mode_parameters: HashMap::new(),
        }
    }

    pub fn set_case_mapping(&mut self, case_mapping: CaseMapping) {
        self.case_mapping = case_mapping;
    }

    pub fn set_prefix_schema(&mut self, modes: &str, symbols: &str) {
        if modes.chars().count() != symbols.chars().count() || symbols.is_empty() {
            return;
        }
        self.prefix_modes = modes.to_string();
        self.prefix_symbols = symbols.to_string();
        for user in &mut self.users {
            user.refresh_display_mode(&self.prefix_modes, &self.prefix_symbols);
        }
        if let Some(snapshot) = &mut self.names_snapshot {
            for user in snapshot {
                user.refresh_display_mode(&self.prefix_modes, &self.prefix_symbols);
            }
        }
        self.sort_users();
    }

    pub fn upsert_mode_list_entry(
        &mut self,
        mode: char,
        mask: String,
        set_by: String,
        set_time: u64,
    ) {
        let entries = self.mode_lists.entry(mode).or_default();
        match entries
            .iter()
            .position(|entry| entry.mask.eq_ignore_ascii_case(&mask))
        {
            Some(slot) => {
                entries[slot].set_by = set_by;
                entries[slot].set_time = set_time;
            }
            None => entries.push(BanEntry {
                mask,
                set_by,
                set_time,
            }),
        }
    }

    pub fn complete_mode_list(&mut self, mode: char) {
        self.mode_lists_complete.insert(mode);
    }

    pub fn apply_mode_list_change(&mut self, sign: char, mode: char, mask: &str, setter: &str) {
        if sign == '+' {
            self.upsert_mode_list_entry(mode, mask.to_string(), setter.to_string(), 0);
            if mode == 'b'
                && !self
                    .bans
                    .iter()
                    .any(|entry| entry.mask.eq_ignore_ascii_case(mask))
            {
                self.bans.push(BanEntry {
                    mask: mask.to_string(),
                    set_by: setter.to_string(),
                    set_time: 0,
                });
            }
        } else {
            if let Some(entries) = self.mode_lists.get_mut(&mode) {
                entries.retain(|entry| !entry.mask.eq_ignore_ascii_case(mask));
            }
            if mode == 'b' {
                self.bans
                    .retain(|entry| !entry.mask.eq_ignore_ascii_case(mask));
            }
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
        if let Some(user) = self
            .users
            .iter_mut()
            .find(|u| self.case_mapping.eq(&u.nick, clean_nick))
        {
            if let Some(letter) = mode.mode_letter() {
                user.add_status_mode(letter, &self.prefix_modes, &self.prefix_symbols);
            }
        } else {
            let mut user = ChannelUser::new(clean_nick.to_string(), UserMode::Normal);
            if let Some(letter) = mode.mode_letter() {
                user.add_status_mode(letter, &self.prefix_modes, &self.prefix_symbols);
            }
            self.users.push(user);
            self.sort_users();
        }

        // A JOIN can interleave with a multi-line NAMES response. Mirror that
        // membership into the pending snapshot so it is not lost at 366.
        if let Some(snapshot) = &mut self.names_snapshot {
            if let Some(user) = snapshot
                .iter_mut()
                .find(|u| self.case_mapping.eq(&u.nick, clean_nick))
            {
                if let Some(letter) = mode.mode_letter() {
                    user.add_status_mode(letter, &self.prefix_modes, &self.prefix_symbols);
                }
            } else {
                let mut user = ChannelUser::new(clean_nick.to_string(), UserMode::Normal);
                if let Some(letter) = mode.mode_letter() {
                    user.add_status_mode(letter, &self.prefix_modes, &self.prefix_symbols);
                }
                snapshot.push(user);
            }
        }
    }

    /// Start an authoritative NAMES snapshot. The visible membership is left
    /// untouched until RPL_ENDOFNAMES, avoiding partial-list flicker.
    pub fn begin_user_burst(&mut self) {
        // A burst that never reached RPL_ENDOFNAMES leaves a partial snapshot
        // behind. NAMES lines stream back-to-back, so a 353 arriving long
        // after the last one means the previous burst was abandoned: start
        // fresh rather than appending to it (which would resurrect ghost
        // users when the new burst completes).
        const BURST_GAP: std::time::Duration = std::time::Duration::from_secs(5);
        let stale = self.burst_activity.is_some_and(|t| t.elapsed() > BURST_GAP);
        if self.names_snapshot.is_none() || stale {
            self.names_snapshot = Some(Vec::new());
        }
    }

    /// Insert a user into the pending NAMES snapshot. Used while streaming a
    /// burst, where a per-insert visible-list sort would make a large join
    /// O(N²·logN). The caller must invoke `finish_user_burst` at 366.
    #[cfg(test)]
    pub fn add_user_burst_modes(&mut self, clean_nick: &str, modes: &[UserMode]) {
        let letters: Vec<char> = modes.iter().filter_map(|mode| mode.mode_letter()).collect();
        self.add_user_burst_status_modes(clean_nick, &letters);
    }

    pub fn add_user_burst_status_modes(&mut self, clean_nick: &str, modes: &[char]) {
        if clean_nick.is_empty() {
            return;
        }
        self.begin_user_burst();
        self.burst_activity = Some(std::time::Instant::now());
        let snapshot = self.names_snapshot.as_mut().expect("burst just started");
        if let Some(existing) = snapshot
            .iter_mut()
            .find(|u| self.case_mapping.eq(&u.nick, clean_nick))
        {
            for mode in modes {
                existing.add_status_mode(*mode, &self.prefix_modes, &self.prefix_symbols);
            }
        } else {
            let mut user = ChannelUser::new(clean_nick.to_string(), UserMode::Normal);
            for mode in modes {
                user.add_status_mode(*mode, &self.prefix_modes, &self.prefix_symbols);
            }
            snapshot.push(user);
        }
    }

    /// Atomically replace membership with the authoritative NAMES snapshot,
    /// preserving IRCv3 metadata for users who are still present.
    pub fn finish_user_burst(&mut self) {
        self.burst_activity = None;
        let Some(mut snapshot) = self.names_snapshot.take() else {
            return;
        };
        for fresh in &mut snapshot {
            if let Some(previous) = self
                .users
                .iter()
                .find(|old| self.case_mapping.eq(&old.nick, &fresh.nick))
            {
                fresh.away = previous.away.clone();
                fresh.account = previous.account.clone();
                fresh.realname = previous.realname.clone();
                fresh.username = previous.username.clone();
                fresh.hostname = previous.hostname.clone();
                fresh.bot = previous.bot;
            }
        }
        self.users = snapshot;
        self.sort_users();
    }

    pub fn add_user_status_mode(&mut self, nick: &str, mode: char) {
        let modes = self.prefix_modes.clone();
        let symbols = self.prefix_symbols.clone();
        if let Some(user) = self.get_user_mut(nick) {
            user.add_status_mode(mode, &modes, &symbols);
        }
        if let Some(snapshot) = &mut self.names_snapshot
            && let Some(user) = snapshot
                .iter_mut()
                .find(|u| self.case_mapping.eq(&u.nick, nick))
        {
            user.add_status_mode(mode, &modes, &symbols);
        }
        self.sort_users();
    }

    #[cfg(test)]
    pub fn remove_user_mode(&mut self, nick: &str, mode: UserMode) {
        let Some(mode) = mode.mode_letter() else {
            return;
        };
        self.remove_user_status_mode(nick, mode);
    }

    pub fn remove_user_status_mode(&mut self, nick: &str, mode: char) {
        let modes = self.prefix_modes.clone();
        let symbols = self.prefix_symbols.clone();
        if let Some(user) = self.get_user_mut(nick) {
            user.remove_status_mode(mode, &modes, &symbols);
        }
        if let Some(snapshot) = &mut self.names_snapshot
            && let Some(user) = snapshot
                .iter_mut()
                .find(|u| self.case_mapping.eq(&u.nick, nick))
        {
            user.remove_status_mode(mode, &modes, &symbols);
        }
        self.sort_users();
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
        if sign == '-' {
            self.mode_parameters.remove(&mode);
        }
        self.rebuild_mode_params();
    }

    pub fn apply_channel_mode(&mut self, sign: char, mode: char, parameter: Option<&str>) {
        if sign == '+' {
            if let Some(parameter) = parameter {
                self.mode_parameters.insert(mode, parameter.to_string());
            }
        } else if sign == '-' {
            self.mode_parameters.remove(&mode);
        }
        self.apply_channel_mode_flag(sign, mode);
    }

    pub fn set_channel_modes_from_reply(
        &mut self,
        modes: &str,
        params: &[String],
        support: &NetworkSupport,
    ) {
        self.modes.clear();
        self.mode_params.clear();
        self.mode_parameters.clear();
        let mut sign = '+';
        let mut param_idx = 0;
        for mode in modes.chars() {
            match mode {
                '+' | '-' => sign = mode,
                mode => {
                    // CHANMODES list modes (type A: bans, exceptions, invite
                    // masks) always take a parameter, in either direction.
                    let takes_parameter = support.chanmodes_a.contains(mode)
                        || support.chanmodes_b.contains(mode)
                        || (sign == '+' && support.chanmodes_c.contains(mode));
                    let parameter = if takes_parameter {
                        let value = params.get(param_idx).map(String::as_str);
                        if value.is_some() {
                            param_idx += 1;
                        }
                        value
                    } else {
                        None
                    };
                    self.apply_channel_mode(sign, mode, parameter);
                }
            }
        }
        self.rebuild_mode_params();
    }

    fn rebuild_mode_params(&mut self) {
        self.mode_params = self
            .modes
            .chars()
            .filter(|c| *c != '+' && *c != '-')
            .filter_map(|mode| self.mode_parameters.get(&mode).cloned())
            .collect();
    }

    pub fn remove_user(&mut self, nick: &str) {
        let mapping = self.case_mapping;
        self.users.retain(|u| !mapping.eq(&u.nick, nick));
        if let Some(snapshot) = &mut self.names_snapshot {
            snapshot.retain(|u| !mapping.eq(&u.nick, nick));
        }
    }

    pub fn clear_users(&mut self) {
        self.users.clear();
        self.names_snapshot = None;
    }

    pub fn rename_user(&mut self, old_nick: &str, new_nick: &str) {
        // If the target nick already exists as a different user, drop the old entry
        // rather than creating a duplicate (can happen with out-of-order NICK/JOIN).
        // A pure case change of the same nick still falls through to the rename.
        if !self.case_mapping.eq(old_nick, new_nick)
            && self
                .users
                .iter()
                .any(|u| self.case_mapping.eq(&u.nick, new_nick))
        {
            let mapping = self.case_mapping;
            self.users.retain(|u| !mapping.eq(&u.nick, old_nick));
            self.sort_users();
            return;
        }
        if let Some(user) = self
            .users
            .iter_mut()
            .find(|u| self.case_mapping.eq(&u.nick, old_nick))
        {
            user.nick = new_nick.to_string();
            self.sort_users();
        }
        if let Some(snapshot) = &mut self.names_snapshot
            && let Some(user) = snapshot
                .iter_mut()
                .find(|u| self.case_mapping.eq(&u.nick, old_nick))
        {
            user.nick = new_nick.to_string();
        }
    }

    /// Append a message, trimming the scrollback to at most `max` messages.
    /// Used by callers that push directly to `messages` (e.g. Quit/Nick loops
    /// over all channels) so they respect the same cap as add_message_to_channel.
    pub fn push_trimmed(&mut self, msg: ChatMessage, max: usize) {
        if msg.msgid.as_ref().is_some_and(|msgid| {
            self.messages
                .iter()
                .any(|existing| existing.msgid.as_ref() == Some(msgid))
        }) {
            return;
        }
        self.messages.push_back(msg);
        if max > 0 {
            while self.messages.len() > max {
                self.messages.pop_front();
            }
        }
    }

    pub fn redact_message(&mut self, msgid: &str) -> bool {
        let previous_len = self.messages.len();
        self.messages
            .retain(|message| message.msgid.as_deref() != Some(msgid));
        self.messages.len() != previous_len
    }

    pub fn has_user(&self, nick: &str) -> bool {
        self.get_user(nick).is_some()
    }

    pub fn get_user(&self, nick: &str) -> Option<&ChannelUser> {
        self.users
            .iter()
            .find(|u| self.case_mapping.eq(&u.nick, nick))
    }

    pub fn get_user_mut(&mut self, nick: &str) -> Option<&mut ChannelUser> {
        let mapping = self.case_mapping;
        self.users.iter_mut().find(|u| mapping.eq(&u.nick, nick))
    }

    fn sort_users(&mut self) {
        // Decorate-sort-undecorate: canonicalizing inside the comparator would
        // allocate two Strings per comparison; computing each key once makes
        // this one allocation per user regardless of list size.
        let mapping = self.case_mapping;
        let users = std::mem::take(&mut self.users);
        let mut keyed: Vec<(usize, String, ChannelUser)> = users
            .into_iter()
            .map(|user| {
                let key = mapping.canonicalize(&user.nick);
                (user.rank, key, user)
            })
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        self.users = keyed.into_iter().map(|(_, _, user)| user).collect();
    }

    /// IRCv3 away-notify: update user's away status
    #[cfg(test)]
    pub fn set_user_away(&mut self, nick: &str, away_msg: Option<String>) {
        if let Some(user) = self.get_user_mut(nick) {
            user.away = away_msg.clone();
        }
        let mapping = self.case_mapping;
        if let Some(snapshot) = &mut self.names_snapshot
            && let Some(user) = snapshot.iter_mut().find(|u| mapping.eq(&u.nick, nick))
        {
            user.away = away_msg;
        }
    }

    /// IRCv3 account-notify: update user's logged-in account
    pub fn set_user_account(&mut self, nick: &str, account: Option<String>) {
        if let Some(user) = self.get_user_mut(nick) {
            user.account = account.clone();
        }
        let mapping = self.case_mapping;
        if let Some(snapshot) = &mut self.names_snapshot
            && let Some(user) = snapshot.iter_mut().find(|u| mapping.eq(&u.nick, nick))
        {
            user.account = account;
        }
    }

    /// IRCv3 extended-join / setname: retain the user's current real name.
    pub fn set_user_realname(&mut self, nick: &str, realname: Option<String>) {
        if let Some(user) = self.get_user_mut(nick) {
            user.realname = realname.clone();
        }
        let mapping = self.case_mapping;
        if let Some(snapshot) = &mut self.names_snapshot
            && let Some(user) = snapshot.iter_mut().find(|u| mapping.eq(&u.nick, nick))
        {
            user.realname = realname;
        }
    }

    pub fn apply_user_identity(&mut self, identity: &KnownUser) {
        if let Some(user) = self.get_user_mut(&identity.nick) {
            user.username = identity.username.clone();
            user.hostname = identity.hostname.clone();
            user.account = identity.account.clone();
            user.realname = identity.realname.clone();
            user.away = identity.away.clone();
            user.bot = identity.bot;
        }
        let mapping = self.case_mapping;
        if let Some(snapshot) = &mut self.names_snapshot
            && let Some(user) = snapshot
                .iter_mut()
                .find(|user| mapping.eq(&user.nick, &identity.nick))
        {
            user.username = identity.username.clone();
            user.hostname = identity.hostname.clone();
            user.account = identity.account.clone();
            user.realname = identity.realname.clone();
            user.away = identity.away.clone();
            user.bot = identity.bot;
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
    pub matches: Vec<String>,
    pub index: usize,
    /// Input before the original token being completed.
    pub base: String,
    /// Text appended after each candidate (`": "` for a leading nick, a
    /// single space for commands and other nick positions).
    pub suffix: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_system_timestamp_uses_configured_format() {
        ChatMessage::configure_default_timestamp_format("long");
        let long = ChatMessage::system("long");
        assert_eq!(long.timestamp.len(), "[00:00:00]".len());

        ChatMessage::configure_default_timestamp_format("full");
        let full = ChatMessage::system("full");
        assert_eq!(full.timestamp.len(), "[00-00 00:00]".len());

        ChatMessage::configure_default_timestamp_format("short");
    }

    #[test]
    fn user_burst_dedups_and_sorts_once() {
        let mut ch = Channel::new();
        ch.add_user("zed", UserMode::Normal);
        ch.set_user_away("zed", Some("afk".to_string()));
        // A NAMES refresh streams in, including a duplicate of the existing user.
        ch.add_user_burst_modes("alice", &[UserMode::Op]);
        ch.add_user_burst_modes("ZED", &[UserMode::Normal]);
        ch.add_user_burst_modes("bob", &[UserMode::Normal]);
        ch.finish_user_burst();

        let nicks: Vec<&str> = ch.users.iter().map(|u| u.nick.as_str()).collect();
        assert_eq!(nicks, vec!["alice", "bob", "ZED"]);
        // Fresh NAMES spelling/modes win, while compatible metadata survives.
        assert!(ch.get_user("zed").unwrap().is_away());
    }

    #[test]
    fn names_snapshot_replaces_stale_membership() {
        let mut ch = Channel::new();
        ch.add_user("alice", UserMode::Op);
        ch.add_user("bob", UserMode::Voice);
        ch.set_user_account("alice", Some("account".to_string()));
        ch.set_user_realname("alice", Some("Alice Example".to_string()));

        ch.begin_user_burst();
        ch.add_user_burst_modes("Alice", &[UserMode::Voice]);
        ch.finish_user_burst();

        assert_eq!(ch.users.len(), 1);
        let alice = ch.get_user("ALICE").unwrap();
        assert_eq!(alice.mode, UserMode::Voice);
        assert_eq!(alice.account.as_deref(), Some("account"));
        assert_eq!(alice.realname.as_deref(), Some("Alice Example"));
        assert!(!ch.has_user("bob"));
    }

    #[test]
    fn names_snapshot_reconciles_interleaved_membership_events() {
        let mut ch = Channel::new();
        ch.add_user("stale", UserMode::Normal);
        ch.begin_user_burst();
        ch.add_user_burst_modes("alice", &[UserMode::Normal]);
        ch.add_user_burst_modes("bob", &[UserMode::Normal]);

        ch.remove_user("bob");
        ch.add_user("carol", UserMode::Voice);
        ch.finish_user_burst();

        assert!(ch.has_user("alice"));
        assert!(ch.has_user("carol"));
        assert!(!ch.has_user("bob"));
        assert!(!ch.has_user("stale"));
    }

    #[test]
    fn simultaneous_membership_modes_survive_individual_changes() {
        let support = NetworkSupport::default();
        let (nick, modes) = support.parse_prefixed_nick("@+alice");
        let mut ch = Channel::new();
        ch.add_user_burst_status_modes(nick, &modes);
        ch.finish_user_burst();

        let alice = ch.get_user("alice").unwrap();
        assert!(alice.has_mode(UserMode::Op));
        assert!(alice.has_mode(UserMode::Voice));
        assert_eq!(alice.mode, UserMode::Op);

        ch.remove_user_mode("alice", UserMode::Voice);
        let alice = ch.get_user("alice").unwrap();
        assert!(alice.has_mode(UserMode::Op));
        assert!(!alice.has_mode(UserMode::Voice));
        assert_eq!(alice.mode, UserMode::Op);
    }

    #[test]
    fn live_parameter_modes_stay_aligned() {
        let support = NetworkSupport::default();
        let mut ch = Channel::new();
        ch.set_channel_modes_from_reply("+ntk", &["oldkey".to_string()], &support);
        assert_eq!(ch.modes, "+ntk");
        assert_eq!(ch.mode_params, ["oldkey"]);

        ch.apply_channel_mode('-', 'k', Some("oldkey"));
        assert_eq!(ch.modes, "+nt");
        assert!(ch.mode_params.is_empty());

        ch.apply_channel_mode('+', 'l', Some("50"));
        assert_eq!(ch.modes, "+ntl");
        assert_eq!(ch.mode_params, ["50"]);
        ch.apply_channel_mode('-', 'l', None);
        assert_eq!(ch.modes, "+nt");
        assert!(ch.mode_params.is_empty());
    }

    #[test]
    fn channel_mode_reply_consumes_type_a_list_parameters() {
        let support = NetworkSupport::default();
        let mut ch = Channel::new();

        // 324 <me> #chan +tnk <key> — type B k takes a param, then rebuild.
        ch.set_channel_modes_from_reply("+ntk", &["secret".to_string()], &support);
        assert_eq!(ch.modes, "+ntk");
        assert_eq!(ch.mode_params, ["secret"]);

        // Type A modes (b: bans) take a parameter in either direction; a
        // following type D mode must not steal the ban mask as its own param.
        ch.set_channel_modes_from_reply(
            "+bk-m",
            &["*!bad@host".to_string(), "secret2".to_string()],
            &support,
        );
        // ("+bkm": the -m half is not representable in the positive-modes
        // string, matching how live MODE changes are tracked.)
        assert_eq!(ch.modes, "+bk");
        assert_eq!(ch.mode_params, ["*!bad@host", "secret2"]);

        // Removing a ban consumes its parameter too; without type-A handling
        // the mask would be misattributed to a later mode or dropped.
        ch.set_channel_modes_from_reply(
            "-b+k",
            &["*!gone@host".to_string(), "newkey".to_string()],
            &support,
        );
        assert_eq!(ch.modes, "+k");
        assert_eq!(
            ch.mode_parameters.get(&'k').map(String::as_str),
            Some("newkey")
        );
    }

    #[test]
    fn irc_case_mappings_fold_only_their_advertised_equivalences() {
        assert!(CaseMapping::Rfc1459.eq("[Nick]\\^", "{nick}|~"));
        assert!(CaseMapping::StrictRfc1459.eq("[Nick]\\", "{nick}|"));
        assert!(!CaseMapping::StrictRfc1459.eq("nick^", "nick~"));
        assert!(!CaseMapping::Ascii.eq("[nick]", "{nick}"));
        assert!(CaseMapping::Ascii.eq("Alice", "alice"));
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
