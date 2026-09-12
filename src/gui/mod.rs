mod commands;
mod dialogs;
mod formatting;
mod helpers;
mod logging;
mod types;

use egui::{Color32, RichText, ScrollArea, TextEdit};
use std::collections::{HashMap, VecDeque};
use tokio::sync::mpsc;

use crate::irc::client::{DESIRED_CAPS, ServerConfig};
use crate::irc::numerics::*;
use crate::irc::{IrcCommand, IrcMessage};
use formatting::{nick_color, render_irc_text, render_segments};
use helpers::{epoch_time_formatted, format_timestamp, mask_matches, truncate_chars};
pub use types::{
    BanEntry, CaseMapping, Channel, ChannelListEntry, ChannelListSort, ChatMessage, NetworkSupport,
    RowHeightEntry, ServerFavorite, Settings, SortDirection, TabCompletion, UserMode,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionIntent {
    None,
    ManualDisconnect,
    ReconnectAfterClose,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EndpointKey {
    host: String,
    port: u16,
    use_tls: bool,
}

/// Immutable configuration for one connection/reconnection lifecycle. The
/// editable Settings/Connect fields may change while a socket is active; those
/// changes must not relabel the session or alter what an automatic reconnect
/// authenticates with and executes.
#[derive(Clone)]
struct SessionConfig {
    server: ServerConfig,
    auto_join_channels: String,
    auto_perform: String,
    set_invisible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryPlacement {
    Prepend,
    Append,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryDirection {
    Before,
    After,
}

#[derive(Debug, Clone, Copy)]
struct PendingHistoryRequest {
    placement: HistoryPlacement,
    direction: HistoryDirection,
    remaining: usize,
    loaded: usize,
    page_limit: usize,
}

struct ChatHistoryBatch {
    target: String,
    messages: Vec<ChatMessage>,
    request: PendingHistoryRequest,
    complete: bool,
    partial: bool,
}

struct MultilineBatch {
    target: String,
    sender: String,
    content: String,
    has_lines: bool,
    is_notice: bool,
    parent_history: Option<String>,
    server_time: Option<String>,
    msgid: Option<String>,
    account: Option<String>,
    oper: Option<String>,
    reply_to: Option<String>,
}

struct AuthtokenBatch {
    service: String,
    token: String,
}

impl SessionConfig {
    fn endpoint_key(&self) -> EndpointKey {
        EndpointKey {
            host: self.server.host.to_lowercase(),
            port: self.server.port,
            use_tls: self.server.use_tls,
        }
    }

    fn endpoint_label(&self) -> String {
        let host = if self.server.host.contains(':') {
            format!("[{}]", self.server.host)
        } else {
            self.server.host.clone()
        };
        format!("{}:{}", host, self.server.port)
    }

    fn log_identity(&self) -> String {
        let scheme = if self.server.use_tls { "tls" } else { "plain" };
        format!("{scheme}://{}", self.endpoint_label().to_lowercase())
    }

    fn allows_legacy_log_migration(&self) -> bool {
        (self.server.use_tls && self.server.port == 6697)
            || (!self.server.use_tls && self.server.port == 6667)
    }
}

const IRC_MAX_LINE_BYTES: usize = 510;
pub(crate) const PROJECT_SOURCE_URL: &str = "https://github.com/faratech/linefeed";
const COMMAND_COMPLETIONS: &[&str] = &[
    "/accept",
    "/admin",
    "/ame",
    "/amsg",
    "/away",
    "/back",
    "/ban",
    "/bouncer",
    "/botserv",
    "/chanserv",
    "/clear",
    "/close",
    "/ctcp",
    "/cycle",
    "/dehalfop",
    "/deop",
    "/devoice",
    "/describe",
    "/disconnect",
    "/echo",
    "/grep",
    "/halfop",
    "/history",
    "/hop",
    "/hostserv",
    "/ignore",
    "/info",
    "/invite",
    "/ison",
    "/join",
    "/kick",
    "/kickban",
    "/label",
    "/knock",
    "/lastlog",
    "/links",
    "/list",
    "/lusers",
    "/me",
    "/markread",
    "/metadata",
    "/memoserv",
    "/mode",
    "/monitor",
    "/motd",
    "/msg",
    "/names",
    "/nick",
    "/nickserv",
    "/notice",
    "/onotice",
    "/op",
    "/operserv",
    "/part",
    "/persistence",
    "/perform",
    "/ping",
    "/query",
    "/quote",
    "/quit",
    "/raw",
    "/react",
    "/redact",
    "/register",
    "/relocate",
    "/rename",
    "/reconnect",
    "/rejoin",
    "/reply",
    "/say",
    "/search",
    "/server",
    "/setname",
    "/settings",
    "/silence",
    "/slap",
    "/stats",
    "/sversion",
    "/time",
    "/topic",
    "/token",
    "/trace",
    "/typing",
    "/umode",
    "/unban",
    "/unignore",
    "/userhost",
    "/version",
    "/verify",
    "/webpush",
    "/voice",
    "/wallops",
    "/who",
    "/whois",
    "/whowas",
];

fn style_with_font_size(mut style: egui::Style, font_size: f32) -> egui::Style {
    let font_size = font_size.clamp(10.0, 24.0);
    for (text_style, size) in [
        (egui::TextStyle::Small, font_size * 0.85),
        (egui::TextStyle::Body, font_size),
        (egui::TextStyle::Button, font_size),
        (egui::TextStyle::Monospace, font_size),
        (egui::TextStyle::Heading, font_size * 1.35),
    ] {
        if let Some(font_id) = style.text_styles.get_mut(&text_style) {
            font_id.size = size;
        }
    }
    style
}

pub(crate) fn apply_font_size(ctx: &egui::Context, font_size: f32) {
    let style = style_with_font_size((*ctx.global_style()).clone(), font_size);
    ctx.set_global_style(style);
}

pub struct IrcApp {
    // Connection state
    pub connected: bool,
    pub connecting: bool,
    pub my_nick: String,
    pub connection_intent: ConnectionIntent,

    // Server config
    pub server_host: String,
    pub server_port: String,
    pub use_tls: bool,
    pub accept_invalid_certs: bool,
    pub nickname: String,
    pub username: String,
    pub realname: String,
    pub password: String,

    // Immutable active/pending connection snapshots. `pending_session` is used
    // while an old socket closes so its final messages retain the old identity.
    active_session: Option<SessionConfig>,
    pending_session: Option<SessionConfig>,
    connection_error: Option<String>,

    // Channels and messages
    pub channels: HashMap<String, Channel>,
    pub current_channel: Option<String>,
    pub server_messages: VecDeque<ChatMessage>,
    pub server_unread: usize,

    // Input
    pub input_text: String,
    pub join_channel: String,

    // Communication channels
    pub cmd_tx: Option<mpsc::UnboundedSender<IrcCommand>>,
    pub msg_rx: Option<mpsc::Receiver<IrcMessage>>,

    // UI state
    pub show_settings: bool,
    pub scroll_to_bottom: bool,
    pub show_connect_dialog: bool,

    // Command history
    pub command_history: Vec<String>,
    pub history_index: Option<usize>,
    pub history_temp: String,

    // Channel list popup
    pub channel_list: Vec<ChannelListEntry>,
    pub channel_list_loading: bool,
    pub show_channel_list: bool,
    pub channel_list_filter: String,
    pub channel_list_selected: Option<String>,
    pub channel_list_sort: ChannelListSort,
    pub channel_list_sort_dir: SortDirection,

    // Channel list view cache: filtered+sorted indices into channel_list,
    // recomputed only when the list, filter, or sort changes (re-sorting up to
    // 50k rows every frame would otherwise burn a core while the dialog is open).
    pub(crate) channel_list_cache: Vec<usize>,
    pub(crate) channel_list_dirty: bool,
    pub(crate) channel_list_cache_filter: String,
    pub(crate) channel_list_cache_sort: (ChannelListSort, SortDirection),
    pub(crate) channel_list_last_refilter: Option<std::time::Instant>,

    // Tab completion
    pub tab_completion: Option<TabCompletion>,

    // User list selection
    pub selected_user: Option<String>,

    // Connection options
    pub auto_join_channels: String,
    pub set_invisible: bool,
    pub minimize_to_tray: bool,
    pub auto_reconnect: bool,
    pub notifications_enabled: bool,

    // Window focus tracking (for notifications)
    pub window_focused: bool,

    // Auto-reconnect state
    pub reconnect_attempts: u32,
    pub last_disconnect_time: Option<std::time::Instant>,
    pub connection_lost: bool,

    // Ignore list
    pub ignore_list: Vec<String>,

    // Server favorites
    pub server_favorites: Vec<ServerFavorite>,

    // Auto-perform commands (newline-separated)
    pub auto_perform: String,
    pub pre_away_message: String,
    pub persistence_profile: String,

    // Pending auto-perform (to execute after connect)
    pub pending_auto_perform: Option<Vec<String>>,

    // Server favorites UI state
    pub selected_favorite: Option<usize>,
    pub new_favorite_name: String,
    pub show_save_favorite_dialog: bool,

    // Pending channel keys (for storing key when joining +k channels)
    pub pending_channel_keys: HashMap<String, String>,

    // Pending invites (from_nick, channel)
    pub pending_invites: Vec<(String, String)>,

    // Away status tracking
    pub away_status: Option<String>,
    pub last_activity: std::time::Instant,
    pub auto_away_triggered: bool,

    // Auto-away settings
    pub auto_away_enabled: bool,
    pub auto_away_minutes: u32,
    pub auto_away_message: String,

    // SASL authentication
    pub sasl_username: String,
    pub sasl_password: String,

    // Logging
    pub log_manager: logging::LogManager,
    pub logging_enabled: bool,
    pub logging_load_history: bool,
    pub logging_history_lines: usize,

    // Display settings
    pub timestamp_format: String,
    pub hide_join_part: bool,
    pub font_size: f32,
    pub max_scrollback: usize,

    // Privacy
    pub ctcp_replies_enabled: bool,

    // Highlights
    pub highlight_words: String,

    // Server-advertised protocol support
    pub network_support: NetworkSupport,
    enabled_caps: std::collections::HashSet<String>,
    chathistory_enabled: bool,
    chathistory_limit: Option<usize>,
    chathistory_batches: HashMap<String, ChatHistoryBatch>,
    batch_parents: HashMap<String, String>,
    multiline_batches: HashMap<String, MultilineBatch>,
    authtoken_batches: HashMap<String, AuthtokenBatch>,
    pending_history_requests: HashMap<String, VecDeque<PendingHistoryRequest>>,

    // Cached lowercase versions for efficient comparison (updated when source changes)
    my_nick_lower: String,
    ignore_list_lower: Vec<String>,
    highlight_words_lower: Vec<String>,

    // Reconnect settings
    pub reconnect_delay_secs: u32,
    pub max_reconnect_attempts: u32,

    // Custom messages
    pub quit_message: String,
    pub part_message: String,

    // Channel list
    pub list_min_users: u32,

    // Channel info dialog
    pub show_channel_info: bool,
    pub channel_info_target: Option<String>,

    // Settings dialog tab
    pub settings_tab: usize,

    // Lag meter
    pub lag_ms: Option<u32>,
    pub ping_sent_time: Option<std::time::Instant>,
    pub last_lag_check: std::time::Instant,

    // Color picker state
    pub show_color_picker: bool,
    pub color_picker_fg: bool, // true = selecting foreground, false = selecting background

    // CPU optimization state
    pub had_messages_this_frame: bool,

    // Rate limiting for desktop notifications (each one spawns a thread and,
    // on Linux/macOS, a subprocess - a flood must not spawn one per message).
    last_notification_time: Option<std::time::Instant>,

    nick_retry_attempts: u8,
}

impl Default for IrcApp {
    fn default() -> Self {
        Self::with_settings(Settings::load())
    }
}

impl IrcApp {
    fn with_settings(settings: Settings) -> Self {
        ChatMessage::configure_default_timestamp_format(&settings.timestamp_format);
        let nickname = if settings.nickname.is_empty() {
            format!("Linefeed_{}", rand_suffix())
        } else {
            settings.nickname.clone()
        };
        let network_support = NetworkSupport::default();
        let my_nick_lower = network_support.canonicalize(&nickname);

        Self {
            connected: false,
            connecting: false,
            my_nick: nickname.clone(),
            connection_intent: ConnectionIntent::None,

            server_host: settings.server_host,
            server_port: settings.server_port,
            use_tls: settings.use_tls,
            accept_invalid_certs: settings.accept_invalid_certs,
            nickname,
            username: settings.username,
            realname: settings.realname,
            password: settings.password,
            active_session: None,
            pending_session: None,
            connection_error: None,

            channels: HashMap::new(),
            current_channel: None,
            server_messages: VecDeque::new(),
            server_unread: 0,

            input_text: String::new(),
            join_channel: String::new(),

            cmd_tx: None,
            msg_rx: None,

            show_settings: false,
            scroll_to_bottom: true,
            show_connect_dialog: true,

            command_history: Vec::new(),
            history_index: None,
            history_temp: String::new(),

            channel_list: Vec::new(),
            channel_list_loading: false,
            show_channel_list: false,
            channel_list_filter: String::new(),
            channel_list_selected: None,
            channel_list_sort: ChannelListSort::Users,
            channel_list_sort_dir: SortDirection::Descending,

            channel_list_cache: Vec::new(),
            channel_list_dirty: false,
            channel_list_cache_filter: String::new(),
            channel_list_cache_sort: (ChannelListSort::Users, SortDirection::Descending),
            channel_list_last_refilter: None,

            tab_completion: None,

            selected_user: None,

            auto_join_channels: settings.auto_join_channels,
            set_invisible: settings.set_invisible,
            minimize_to_tray: settings.minimize_to_tray,
            auto_reconnect: settings.auto_reconnect,
            notifications_enabled: settings.notifications_enabled,

            window_focused: true,

            reconnect_attempts: 0,
            last_disconnect_time: None,
            connection_lost: false,

            // Cached lowercase versions (computed before moving owned values)
            my_nick_lower,
            ignore_list_lower: settings
                .ignore_list
                .iter()
                .map(|s| network_support.canonicalize(s))
                .collect(),
            highlight_words_lower: settings
                .highlight_words
                .split(',')
                .map(|s| network_support.canonicalize(s.trim()))
                .filter(|s| !s.is_empty())
                .collect(),

            ignore_list: settings.ignore_list,
            server_favorites: settings.server_favorites,
            auto_perform: settings.auto_perform,
            pre_away_message: settings.pre_away_message,
            persistence_profile: settings.persistence_profile,
            pending_auto_perform: None,

            // Server favorites UI state
            selected_favorite: None,
            new_favorite_name: String::new(),
            show_save_favorite_dialog: false,

            pending_channel_keys: HashMap::new(),
            pending_invites: Vec::new(),

            // Away status
            away_status: None,
            last_activity: std::time::Instant::now(),
            auto_away_triggered: false,
            auto_away_enabled: settings.auto_away_enabled,
            auto_away_minutes: settings.auto_away_minutes,
            auto_away_message: settings.auto_away_message,
            sasl_username: settings.sasl_username,
            sasl_password: settings.sasl_password,
            log_manager: logging::LogManager::new(settings.logging_enabled),
            logging_enabled: settings.logging_enabled,
            logging_load_history: settings.logging_load_history,
            logging_history_lines: settings.logging_history_lines,

            // Display settings
            timestamp_format: settings.timestamp_format,
            hide_join_part: settings.hide_join_part,
            font_size: settings.font_size,
            max_scrollback: settings.max_scrollback,

            // Privacy
            ctcp_replies_enabled: settings.ctcp_replies_enabled,

            // Highlights
            highlight_words: settings.highlight_words.clone(),
            network_support,
            enabled_caps: std::collections::HashSet::new(),
            chathistory_enabled: false,
            chathistory_limit: None,
            chathistory_batches: HashMap::new(),
            batch_parents: HashMap::new(),
            multiline_batches: HashMap::new(),
            authtoken_batches: HashMap::new(),
            pending_history_requests: HashMap::new(),

            // Reconnect settings
            reconnect_delay_secs: settings.reconnect_delay_secs,
            max_reconnect_attempts: settings.max_reconnect_attempts,

            // Custom messages
            quit_message: settings.quit_message,
            part_message: settings.part_message,

            // Channel list
            list_min_users: settings.list_min_users,

            show_channel_info: false,
            channel_info_target: None,

            // Settings dialog tab
            settings_tab: 0,

            // Lag meter
            lag_ms: None,
            ping_sent_time: None,
            last_lag_check: std::time::Instant::now(),

            // Color picker
            show_color_picker: false,
            color_picker_fg: true,

            // CPU optimization
            had_messages_this_frame: false,
            last_notification_time: None,
            nick_retry_attempts: 0,
        }
    }

    fn get_settings(&self) -> Settings {
        Settings {
            server_host: self.server_host.clone(),
            server_port: self.server_port.clone(),
            use_tls: self.use_tls,
            accept_invalid_certs: self.accept_invalid_certs,
            nickname: self.nickname.clone(),
            username: self.username.clone(),
            realname: self.realname.clone(),
            password: self.password.clone(),
            auto_join_channels: self.auto_join_channels.clone(),
            set_invisible: self.set_invisible,
            minimize_to_tray: self.minimize_to_tray,
            auto_reconnect: self.auto_reconnect,
            notifications_enabled: self.notifications_enabled,
            ignore_list: self.ignore_list.clone(),
            server_favorites: self.server_favorites.clone(),
            auto_perform: self.auto_perform.clone(),
            pre_away_message: self.pre_away_message.clone(),
            persistence_profile: self.persistence_profile.clone(),
            auto_away_enabled: self.auto_away_enabled,
            auto_away_minutes: self.auto_away_minutes,
            auto_away_message: self.auto_away_message.clone(),
            sasl_username: self.sasl_username.clone(),
            sasl_password: self.sasl_password.clone(),
            logging_enabled: self.logging_enabled,
            logging_load_history: self.logging_load_history,
            logging_history_lines: self.logging_history_lines,
            // Display
            timestamp_format: self.timestamp_format.clone(),
            hide_join_part: self.hide_join_part,
            font_size: self.font_size,
            max_scrollback: self.max_scrollback,
            // Privacy
            ctcp_replies_enabled: self.ctcp_replies_enabled,
            // Highlights
            highlight_words: self.highlight_words.clone(),
            // Connection
            reconnect_delay_secs: self.reconnect_delay_secs,
            max_reconnect_attempts: self.max_reconnect_attempts,
            // Custom messages
            quit_message: self.quit_message.clone(),
            part_message: self.part_message.clone(),
            // Channel list
            list_min_users: self.list_min_users,
        }
    }

    /// Check if a sender is ignored (by nick or mask)
    /// Uses cached ignore_list_lower to avoid allocation per message
    fn is_ignored(&self, sender: &str, prefix: Option<&str>) -> bool {
        let sender_key = self.network_support.canonicalize(sender);
        for (pattern, pattern_lower) in self.ignore_list.iter().zip(self.ignore_list_lower.iter()) {
            if pattern_lower == &sender_key {
                return true;
            }

            let has_wildcard = pattern.contains('*') || pattern.contains('?');
            if has_wildcard && !pattern.contains('!') && !pattern.contains('@') {
                if mask_matches(pattern_lower, &sender_key) {
                    return true;
                }
            } else if (has_wildcard || pattern.contains('!') || pattern.contains('@'))
                && let Some(full_prefix) = prefix
                // RFC casemapping applies to nicknames, while user/host masks
                // are conventionally ASCII case-insensitive. Canonicalizing the
                // whole mask still provides both properties without allocation
                // inside the matcher.
                && mask_matches(
                    pattern_lower,
                    &self.network_support.canonicalize(full_prefix),
                )
            {
                return true;
            }
        }
        false
    }

    /// Calculate reconnect delay with exponential backoff (1s, 2s, 4s, 8s, max 60s)
    pub fn get_reconnect_delay(&self) -> std::time::Duration {
        let base_delay = self.reconnect_delay_secs as u64;
        let max_delay = 120u64;
        // Exponential backoff: delay * 2^attempt, capped at max_delay
        let delay = base_delay.saturating_mul(2u64.saturating_pow(self.reconnect_attempts.min(6)));
        std::time::Duration::from_secs(delay.min(max_delay))
    }

    /// Check if we should attempt reconnection now
    pub fn should_reconnect(&self) -> bool {
        if !self.awaiting_reconnect() {
            return false;
        }

        if let Some(disconnect_time) = self.last_disconnect_time {
            let elapsed = disconnect_time.elapsed();
            let delay = self.get_reconnect_delay();
            elapsed >= delay
        } else {
            true
        }
    }

    /// Whether an auto-reconnect retry is still pending (regardless of whether
    /// its backoff deadline has passed). The event loop must keep polling while
    /// this is true: on an idle visible window nothing else wakes the frame to
    /// observe the deadline.
    pub fn awaiting_reconnect(&self) -> bool {
        self.auto_reconnect
            && self.connection_lost
            && !self.connecting
            && self.reconnect_attempts < self.max_reconnect_attempts
    }

    /// Reset reconnection state (called on successful connect)
    pub fn reset_reconnect_state(&mut self) {
        self.reconnect_attempts = 0;
        self.last_disconnect_time = None;
        self.connection_lost = false;
    }

    /// Mark connection as lost (called on disconnect)
    pub fn mark_connection_lost(&mut self) {
        // Drain any still-buffered incoming messages before dropping the receiver,
        // so the last lines the server sent before the disconnect are not lost.
        if let Some(mut rx) = self.msg_rx.take() {
            while let Ok(msg) = rx.try_recv() {
                self.handle_incoming_message(msg);
            }
        }
        self.connected = false;
        self.connection_lost = true;
        self.last_disconnect_time = Some(std::time::Instant::now());
        self.cmd_tx = None;
        self.msg_rx = None;
        self.lag_ms = None;
        self.ping_sent_time = None;
        self.mark_channel_tabs_unjoined();
    }

    /// Prepare for reconnection attempt
    pub fn start_reconnect(&mut self) {
        self.reconnect_attempts += 1;
        self.connecting = true;
        self.connection_lost = false;
        let delay = self.get_reconnect_delay();
        self.add_server_message(ChatMessage::system(&format!(
            "Reconnecting... (attempt {}, next retry in {:?})",
            self.reconnect_attempts, delay
        )));
    }

    /// Minimal update when window is minimized - drains messages without rendering UI
    /// This prevents the message buffer from filling up while keeping CPU usage near zero
    pub fn update_minimal(&mut self) {
        // Drain all pending messages to prevent buffer overflow
        let messages: Vec<_> = if let Some(rx) = &mut self.msg_rx {
            std::iter::from_fn(|| rx.try_recv().ok()).collect()
        } else {
            Vec::new()
        };

        // Process messages so notifications, state updates, etc. still work
        for msg in messages {
            self.handle_incoming_message(msg);
        }
        self.log_manager.flush_all();
    }

    /// Send a desktop notification
    pub fn send_notification(&mut self, title: &str, body: &str, force: bool) {
        // Always log the notification
        tracing::info!("Notification: {} - {}", title, body);

        if !self.notifications_enabled {
            tracing::debug!("Notifications disabled, skipping");
            return;
        }

        // Skip if window is focused (unless force=true for PMs)
        if self.window_focused && !force {
            tracing::debug!("Window focused, skipping notification");
            return;
        }

        // Rate-limit: each notification spawns an OS thread (and on Linux/macOS a
        // subprocess), so a netsplit replay or highlight flood must not spawn one
        // per message.
        const MIN_NOTIFICATION_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
        if let Some(last) = self.last_notification_time
            && last.elapsed() < MIN_NOTIFICATION_INTERVAL
        {
            tracing::debug!("Notification rate-limited");
            return;
        }
        self.last_notification_time = Some(std::time::Instant::now());

        // Use notify-send on Linux/BSD (usually pre-installed on Linux desktops)
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let title = title.to_string();
            let body = body.to_string();
            std::thread::spawn(move || {
                let mut command = std::process::Command::new("notify-send");
                command.args(["-a", "Linefeed", "-t", "5000", &title, &body]);
                match wait_for_notification_helper(&mut command) {
                    Ok(status) if !status.success() => {
                        tracing::warn!("notify-send exited with status {}", status)
                    }
                    Err(error) => tracing::warn!("Failed to run notify-send: {}", error),
                    Ok(_) => {}
                }
            });
        }

        #[cfg(target_os = "macos")]
        {
            let title = title.replace('"', "\\\"");
            let body = body.replace('"', "\\\"");
            std::thread::spawn(move || {
                let script = format!("display notification \"{}\" with title \"{}\"", body, title);
                let mut command = std::process::Command::new("osascript");
                command.args(["-e", &script]);
                match wait_for_notification_helper(&mut command) {
                    Ok(status) if !status.success() => {
                        tracing::warn!("osascript exited with status {}", status)
                    }
                    Err(error) => tracing::warn!("Failed to run osascript: {}", error),
                    Ok(_) => {}
                }
            });
        }

        // On Windows, flash the taskbar - or, when hidden to the tray (no
        // taskbar button to flash), show a tray balloon instead.
        #[cfg(windows)]
        {
            crate::systray::notify(title, body);
        }
    }

    fn save_settings(&mut self) {
        self.update_cached_lowercase();
        ChatMessage::configure_default_timestamp_format(&self.timestamp_format);
        self.get_settings().save();
    }

    /// Update cached lowercase versions of strings for efficient comparison
    fn update_cached_lowercase(&mut self) {
        self.highlight_words_lower = self
            .highlight_words
            .split(',')
            .map(|s| self.network_support.canonicalize(s.trim()))
            .filter(|s| !s.is_empty())
            .collect();
        self.ignore_list_lower = self
            .ignore_list
            .iter()
            .map(|s| self.network_support.canonicalize(s))
            .collect();
    }

    fn set_my_nick(&mut self, nick: String) {
        self.my_nick = nick;
        self.my_nick_lower = self.network_support.canonicalize(&self.my_nick);
    }

    pub fn is_channel_name(&self, name: &str) -> bool {
        self.network_support.is_channel(name)
    }

    fn strip_status_prefix<'a>(&self, target: &'a str) -> &'a str {
        if self.is_channel_name(target) {
            return target;
        }
        let mut chars = target.char_indices();
        let Some((_, prefix)) = chars.next() else {
            return target;
        };
        let rest = &target[chars.next().map_or(target.len(), |(index, _)| index)..];
        if self.network_support.status_prefixes.contains(prefix) && self.is_channel_name(rest) {
            rest
        } else {
            target
        }
    }

    pub fn normalize_channel_name(&self, name: &str) -> String {
        if self.is_channel_name(name) {
            name.to_string()
        } else {
            format!("#{}", name)
        }
    }

    fn identifiers_equal(&self, left: &str, right: &str) -> bool {
        self.network_support.identifiers_equal(left, right)
    }

    /// Return the display-preserving key already used for an IRC target.
    /// `HashMap<String, _>` itself is byte-sensitive, so every lookup must pass
    /// through negotiated CASEMAPPING to avoid duplicate channel/query tabs.
    fn channel_key(&self, name: &str) -> Option<String> {
        let wanted = self.network_support.canonicalize(name);
        self.channels
            .keys()
            .find(|key| self.network_support.canonicalize(key) == wanted)
            .cloned()
    }

    fn channel_mut(&mut self, name: &str) -> Option<&mut Channel> {
        let key = self.channel_key(name)?;
        self.channels.get_mut(&key)
    }

    fn remove_channel(&mut self, name: &str) -> Option<Channel> {
        let key = self.channel_key(name)?;
        self.channels.remove(&key)
    }

    /// Open a query through the same initialization path used by incoming
    /// private messages: casemapped reuse, query-window cap, history loading,
    /// and session markers all stay consistent across UI entry points.
    pub(crate) fn open_query(&mut self, target: &str) -> bool {
        if target.is_empty() || self.is_channel_name(target) {
            return false;
        }
        if let Some(existing) = self.channel_key(target) {
            self.current_channel = Some(existing);
            return true;
        }

        self.add_message_to_channel(
            target,
            ChatMessage::system_fmt(
                &format!("Conversation with {}", target),
                &self.timestamp_format,
            ),
        );
        if let Some(created) = self.channel_key(target) {
            self.current_channel = Some(created);
            true
        } else {
            false
        }
    }

    fn current_target_is(&self, target: &str) -> bool {
        self.current_channel
            .as_deref()
            .is_some_and(|current| self.identifiers_equal(current, target))
    }

    fn mark_target_read(&mut self, target: &str) {
        if !self.connected || !self.enabled_caps.contains("draft/read-marker") {
            return;
        }
        let Some(key) = self.channel_key(target) else {
            return;
        };
        let timestamp = self.channels.get(&key).and_then(|channel| {
            channel
                .messages
                .iter()
                .rev()
                .find_map(|message| message.server_time.clone())
        });
        let Some(timestamp) = timestamp else {
            return;
        };
        let marker = format!("timestamp={timestamp}");
        if self.channels[&key].read_marker.as_deref() == Some(marker.as_str()) {
            return;
        }
        if self.send_command(IrcCommand::Markread(key.clone(), Some(marker.clone())))
            && let Some(channel) = self.channels.get_mut(&key)
        {
            channel.read_marker = Some(marker);
        }
    }

    fn mark_channel_tabs_unjoined(&mut self) {
        for channel in self.channels.values_mut() {
            channel.joined = false;
        }
        self.channel_list_loading = false;
    }

    fn can_send_to_target(&self, target: &str) -> bool {
        if !self.is_channel_name(target) {
            return true;
        }
        self.channel_key(target)
            .and_then(|key| self.channels.get(&key))
            // An explicit target without an open tab may be a channel that
            // permits external messages. Only a retained, known-stale tab is
            // blocked locally.
            .is_none_or(|channel| channel.joined)
    }

    fn refresh_case_mapping_state(&mut self) {
        let mapping = self.network_support.case_mapping;
        for channel in self.channels.values_mut() {
            channel.set_case_mapping(mapping);
        }
        self.my_nick_lower = self.network_support.canonicalize(&self.my_nick);
        self.update_cached_lowercase();

        // Pending JOIN keys are internal lookup state, so normalize them rather
        // than keeping the spelling used before CASEMAPPING arrived.
        self.pending_channel_keys = self
            .pending_channel_keys
            .drain()
            .map(|(channel, key)| (self.network_support.canonicalize(&channel), key))
            .collect();
    }

    fn reset_connection_support(&mut self) {
        self.network_support = NetworkSupport::default();
        self.enabled_caps.clear();
        self.chathistory_enabled = false;
        self.chathistory_limit = None;
        self.chathistory_batches.clear();
        self.batch_parents.clear();
        self.multiline_batches.clear();
        self.authtoken_batches.clear();
        self.pending_history_requests.clear();
        self.nick_retry_attempts = 0;
    }

    fn request_capabilities(&mut self, caps: Vec<String>) {
        const CAP_REQ_OVERHEAD: usize = "CAP REQ :".len();
        let mut chunk: Vec<String> = Vec::new();
        let mut chunk_len = CAP_REQ_OVERHEAD;
        for cap in caps {
            let added = usize::from(!chunk.is_empty()) + cap.len();
            if chunk_len + added > IRC_MAX_LINE_BYTES && !chunk.is_empty() {
                self.send_command(IrcCommand::Cap(
                    String::new(),
                    "REQ".to_string(),
                    vec![chunk.join(" ")],
                ));
                chunk.clear();
                chunk_len = CAP_REQ_OVERHEAD;
            }
            chunk_len += usize::from(!chunk.is_empty()) + cap.len();
            chunk.push(cap);
        }
        if !chunk.is_empty() {
            self.send_command(IrcCommand::Cap(
                String::new(),
                "REQ".to_string(),
                vec![chunk.join(" ")],
            ));
        }
    }

    fn handle_cap_message(&mut self, subcommand: &str, params: &[String]) {
        let Some(caps) = params.last() else {
            return;
        };
        let mut newly_available = Vec::new();
        for raw_token in caps.split_whitespace() {
            let removing = raw_token.starts_with('-');
            let token = raw_token.trim_start_matches(['-', '~', '=']);
            let (name, value) = token
                .split_once('=')
                .map_or((token, None), |(name, value)| (name, Some(value)));
            let canonical_name = name.to_ascii_lowercase();

            if subcommand.eq_ignore_ascii_case("NEW")
                && DESIRED_CAPS.contains(&canonical_name.as_str())
                && !self.enabled_caps.contains(&canonical_name)
            {
                newly_available.push(canonical_name.clone());
            }

            if subcommand.eq_ignore_ascii_case("ACK") {
                if removing {
                    self.enabled_caps.remove(&canonical_name);
                } else {
                    self.enabled_caps.insert(canonical_name.clone());
                }
            } else if subcommand.eq_ignore_ascii_case("DEL")
                || subcommand.eq_ignore_ascii_case("NAK")
            {
                self.enabled_caps.remove(&canonical_name);
            }

            if name.eq_ignore_ascii_case("draft/chathistory") {
                if let Some(value) = value {
                    self.chathistory_limit = value
                        .parse::<usize>()
                        .ok()
                        .filter(|limit| *limit > 0)
                        .or_else(|| {
                            value.split(',').find_map(|item| {
                                item.strip_prefix("limit=")
                                    .and_then(|limit| limit.parse::<usize>().ok())
                                    .filter(|limit| *limit > 0)
                            })
                        });
                }

                if subcommand.eq_ignore_ascii_case("ACK") {
                    self.chathistory_enabled = !removing;
                } else if subcommand.eq_ignore_ascii_case("DEL")
                    || subcommand.eq_ignore_ascii_case("NAK")
                {
                    self.chathistory_enabled = false;
                }
            } else if name.eq_ignore_ascii_case("draft/multiline")
                && let Some(value) = value
            {
                for item in value.split(',') {
                    if let Some(value) = item.strip_prefix("max-bytes=") {
                        self.network_support.multiline_max_bytes = value.parse().ok();
                    } else if let Some(value) = item.strip_prefix("max-lines=") {
                        self.network_support.multiline_max_lines = value.parse().ok();
                    }
                }
            }
        }
        if !newly_available.is_empty() {
            self.request_capabilities(newly_available);
        }
    }

    fn total_history_limit(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.logging_history_lines)
            .min(self.max_scrollback)
            .max(1)
    }

    fn history_page_limit(&self, remaining: usize) -> usize {
        remaining.min(self.chathistory_limit.unwrap_or(100)).max(1)
    }

    fn message_reference(message: &ChatMessage) -> Option<String> {
        message
            .msgid
            .as_ref()
            .map(|msgid| format!("msgid={msgid}"))
            .or_else(|| {
                message
                    .server_time
                    .as_ref()
                    .map(|time| format!("timestamp={time}"))
            })
    }

    fn queue_history_request(
        &mut self,
        target: &str,
        command: IrcCommand,
        request: PendingHistoryRequest,
    ) -> bool {
        if !self.send_command(command) {
            return false;
        }
        self.pending_history_requests
            .entry(self.network_support.canonicalize(target))
            .or_default()
            .push_back(request);
        true
    }

    fn request_latest_history(
        &mut self,
        target: &str,
        requested: Option<usize>,
        placement: HistoryPlacement,
    ) -> bool {
        if !self.chathistory_enabled {
            return false;
        }
        let total = self.total_history_limit(requested);
        let limit = self.history_page_limit(total);
        let reference = if placement == HistoryPlacement::Append {
            self.channel_key(target)
                .and_then(|key| self.channels.get(&key))
                .and_then(|channel| {
                    channel
                        .messages
                        .iter()
                        .rev()
                        .find_map(Self::message_reference)
                })
                .unwrap_or_else(|| "*".to_string())
        } else {
            "*".to_string()
        };
        let (direction, command) = if placement == HistoryPlacement::Append && reference != "*" {
            (
                HistoryDirection::After,
                IrcCommand::ChathistoryAfter(target.to_string(), reference, limit),
            )
        } else {
            (
                HistoryDirection::Before,
                IrcCommand::ChathistoryLatest(target.to_string(), reference, limit),
            )
        };
        self.queue_history_request(
            target,
            command,
            PendingHistoryRequest {
                placement,
                direction,
                remaining: total,
                loaded: 0,
                page_limit: limit,
            },
        )
    }

    fn prepare_history_target(&mut self, target: &str) {
        let key = if let Some(key) = self.channel_key(target) {
            key
        } else {
            let mut channel = Channel::new();
            channel.set_case_mapping(self.network_support.case_mapping);
            self.channels.insert(target.to_string(), channel);
            target.to_string()
        };
        self.current_channel = Some(key);
    }

    fn queue_chathistory_message(&mut self, batch_id: &str, message: ChatMessage) {
        if let Some(batch) = self.chathistory_batches.get_mut(batch_id)
            && batch.messages.len() < self.max_scrollback
        {
            batch.messages.push(message);
        }
    }

    fn containing_history_batch(&self, batch_id: &str) -> Option<String> {
        let mut current = batch_id;
        for _ in 0..32 {
            if self.chathistory_batches.contains_key(current) {
                return Some(current.to_string());
            }
            current = self.batch_parents.get(current)?;
        }
        None
    }

    fn collect_multiline_line(&mut self, msg: &IrcMessage) -> bool {
        let Some(batch_id) = msg.get_batch() else {
            return false;
        };
        let Some(batch) = self.multiline_batches.get_mut(&batch_id) else {
            return false;
        };
        let (content, is_notice) = match &msg.command {
            IrcCommand::Privmsg(_, content) => (content, false),
            IrcCommand::Notice(_, content) => (content, true),
            _ => return false,
        };
        let concat = msg.get_tag("draft/multiline-concat").is_some();
        if batch.has_lines && !concat {
            batch.content.push('\n');
        }
        batch.content.push_str(content);
        batch.has_lines = true;
        batch.is_notice = is_notice;
        true
    }

    fn collect_authtoken_line(&mut self, msg: &IrcMessage) -> bool {
        let Some(batch_id) = msg.get_batch() else {
            return false;
        };
        let Some(batch) = self.authtoken_batches.get_mut(&batch_id) else {
            return false;
        };
        let IrcCommand::Extension(name, params) = &msg.command else {
            return false;
        };
        if !name.eq_ignore_ascii_case("TOKEN")
            || !params
                .first()
                .is_some_and(|subcommand| subcommand.eq_ignore_ascii_case("GENERATE"))
        {
            return false;
        }
        if let Some(chunk) = params.get(2) {
            batch.token.push_str(chunk);
        }
        true
    }

    fn finish_authtoken_batch(&mut self, batch_id: &str) {
        let Some(batch) = self.authtoken_batches.remove(batch_id) else {
            return;
        };
        if batch.token.is_empty() {
            return;
        }
        self.add_server_message(
            ChatMessage::system_fmt(
                &format!("[TOKEN GENERATE {}] {}", batch.service, batch.token),
                &self.timestamp_format,
            )
            .without_logging(),
        );
    }

    fn finish_multiline_batch(&mut self, batch_id: &str) {
        let Some(batch) = self.multiline_batches.remove(batch_id) else {
            return;
        };
        let is_action = batch.content.starts_with("\x01ACTION ") && batch.content.ends_with('\x01');
        let content = if is_action {
            batch
                .content
                .strip_prefix("\x01ACTION ")
                .and_then(|content| content.strip_suffix('\x01'))
                .unwrap_or(&batch.content)
        } else {
            &batch.content
        };
        let message = if batch.is_notice {
            ChatMessage::system_fmt(
                &format!("-{}- {}", batch.sender, content),
                &self.timestamp_format,
            )
        } else if is_action {
            ChatMessage::action_fmt(&batch.sender, content, &self.timestamp_format)
        } else {
            ChatMessage::new_fmt(&batch.sender, content, &self.timestamp_format)
        }
        .with_irc_metadata(batch.server_time, batch.msgid, batch.account)
        .with_oper(batch.oper)
        .with_reply_to(batch.reply_to);

        if let Some(history_batch) = batch.parent_history {
            self.queue_chathistory_message(&history_batch, message.without_logging());
            return;
        }

        let target = self.strip_status_prefix(&batch.target).to_string();
        let target =
            if !self.is_channel_name(&target) && self.identifiers_equal(&target, &self.my_nick) {
                batch.sender
            } else {
                target
            };
        if !self.merge_server_echo(&target, &message) {
            self.add_message_to_channel(&target, message);
        }
    }

    /// Reconcile an `echo-message` response with the optimistic local row.
    /// The server supplies the authoritative timestamp, msgid, and account;
    /// retaining one row avoids duplicate messages while making that identity
    /// available to REDACT, reconnect history cursors, and MARKREAD.
    fn merge_server_echo(&mut self, target: &str, echoed: &ChatMessage) -> bool {
        if !self.enabled_caps.contains("echo-message")
            || !self.identifiers_equal(&echoed.sender, &self.my_nick)
            || echoed.is_system
        {
            return false;
        }
        let Some(channel) = self.channel_mut(target) else {
            return false;
        };
        let Some(local) = channel
            .messages
            .iter_mut()
            .rev()
            .take(32)
            .find(|candidate| {
                !candidate.is_system
                    && candidate.msgid.is_none()
                    && candidate.server_time.is_none()
                    && candidate.sender.eq_ignore_ascii_case(&echoed.sender)
                    && candidate.content == echoed.content
                    && candidate.is_action == echoed.is_action
            })
        else {
            return false;
        };
        local.timestamp.clone_from(&echoed.timestamp);
        local.server_time.clone_from(&echoed.server_time);
        local.msgid.clone_from(&echoed.msgid);
        local.account.clone_from(&echoed.account);
        local.oper.clone_from(&echoed.oper);
        local.reply_to.clone_from(&echoed.reply_to);
        true
    }

    fn history_event_message(&self, msg: &IrcMessage) -> Option<ChatMessage> {
        let sender = msg
            .get_sender_nick()
            .unwrap_or_else(|| "Server".to_string());
        let content = match &msg.command {
            IrcCommand::Join(channel, _, account, _) => account.as_ref().map_or_else(
                || format!("{sender} joined {channel}"),
                |account| format!("{sender} [{account}] joined {channel}"),
            ),
            IrcCommand::Part(channel, reason) => format!(
                "{sender} left {channel}{}",
                reason
                    .as_ref()
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            ),
            IrcCommand::Quit(reason) => format!(
                "{sender} quit{}",
                reason
                    .as_ref()
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            ),
            IrcCommand::Kick(channel, nick, reason) => format!(
                "{nick} was kicked from {channel} by {sender}{}",
                reason
                    .as_ref()
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            ),
            IrcCommand::Mode(target, modes, params) => format!(
                "{sender} set mode {} {} {}",
                target,
                modes.as_deref().unwrap_or_default(),
                params.join(" ")
            ),
            IrcCommand::Topic(channel, topic) => format!(
                "{sender} changed the topic in {channel} to: {}",
                topic.as_deref().unwrap_or_default()
            ),
            IrcCommand::Nick(new_nick) => format!("{sender} is now known as {new_nick}"),
            IrcCommand::Setname(realname) => {
                format!("{sender} changed their realname to {realname}")
            }
            IrcCommand::Redact(target, _, reason) => format!(
                "A message in {target} was redacted{}",
                reason
                    .as_deref()
                    .filter(|reason| !reason.is_empty())
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            ),
            _ => return None,
        };
        Some(
            ChatMessage::system_fmt(&content, &self.timestamp_format)
                .with_irc_metadata(
                    msg.get_server_time_raw(),
                    msg.get_msgid(),
                    msg.get_account(),
                )
                .without_logging(),
        )
    }

    fn finish_chathistory_batch(&mut self, batch_id: &str) {
        let Some(batch) = self.chathistory_batches.remove(batch_id) else {
            return;
        };
        let key = if let Some(key) = self.channel_key(&batch.target) {
            key
        } else {
            let mut channel = Channel::new();
            channel.set_case_mapping(self.network_support.case_mapping);
            self.channels.insert(batch.target.clone(), channel);
            batch.target.clone()
        };
        let mut messages = batch.messages;
        let page_count = messages.len();
        let remaining = batch.request.remaining.saturating_sub(page_count);
        let loaded = batch.request.loaded.saturating_add(page_count);
        let complete = batch.complete || page_count < batch.request.page_limit;
        let next_reference = match batch.request.direction {
            HistoryDirection::Before => messages.first().and_then(Self::message_reference),
            HistoryDirection::After => messages.last().and_then(Self::message_reference),
        };
        let next = if !batch.partial && !complete && remaining > 0 {
            next_reference.map(|reference| {
                let limit = self.history_page_limit(remaining);
                let command = match batch.request.direction {
                    HistoryDirection::Before => {
                        IrcCommand::ChathistoryBefore(batch.target.clone(), reference, limit)
                    }
                    HistoryDirection::After => {
                        IrcCommand::ChathistoryAfter(batch.target.clone(), reference, limit)
                    }
                };
                (
                    command,
                    PendingHistoryRequest {
                        remaining,
                        loaded,
                        page_limit: limit,
                        ..batch.request
                    },
                )
            })
        } else {
            None
        };
        let mut history = VecDeque::with_capacity(page_count.saturating_add(1));
        if next.is_none() {
            let mut status = match loaded {
                0 => "--- No server history available ---".to_string(),
                1 => "--- 1 line of server history loaded ---".to_string(),
                count => format!("--- {count} lines of server history loaded ---"),
            };
            if batch.partial {
                status.push_str(" (server reports a partial result)");
            } else if !complete && remaining > 0 {
                status.push_str(" (more history is available)");
            }
            history.push_back(
                ChatMessage::system_fmt(&status, &self.timestamp_format).without_logging(),
            );
        }
        history.extend(messages.drain(..));

        if let Some(channel) = self.channels.get_mut(&key) {
            let existing_ids: std::collections::HashSet<String> = channel
                .messages
                .iter()
                .filter_map(|message| message.msgid.clone())
                .collect();
            history.retain(|message| {
                message
                    .msgid
                    .as_ref()
                    .is_none_or(|msgid| !existing_ids.contains(msgid))
            });
            if batch.request.placement == HistoryPlacement::Prepend {
                history.append(&mut channel.messages);
                channel.messages = history;
            } else {
                channel.messages.append(&mut history);
            }
            while channel.messages.len() > self.max_scrollback {
                channel.messages.pop_front();
            }
        }
        if let Some((command, request)) = next {
            self.queue_history_request(&batch.target, command, request);
        } else if self.current_target_is(&batch.target) && self.window_focused {
            self.mark_target_read(&batch.target);
        }
    }

    /// Clamp a QUIT reason so the resulting line always fits in one IRC line.
    /// An overlong reason must never cause the QUIT itself to be dropped by the
    /// send_command length check: that would orphan the connection thread.
    fn clamp_quit_reason(reason: Option<String>) -> Option<String> {
        const MAX_REASON_BYTES: usize = IRC_MAX_LINE_BYTES - "QUIT :".len();
        reason.map(|r| {
            if r.len() <= MAX_REASON_BYTES {
                r
            } else {
                let mut end = MAX_REASON_BYTES;
                while end > 0 && !r.is_char_boundary(end) {
                    end -= 1;
                }
                r[..end].to_string()
            }
        })
    }

    pub fn request_manual_disconnect(&mut self, reason: Option<String>) {
        self.connection_intent = ConnectionIntent::ManualDisconnect;
        // A manual disconnect cancels an in-flight explicit server switch. If
        // the old worker exits later it must not unexpectedly activate the
        // staged endpoint.
        self.pending_session = None;
        if self.cmd_tx.is_some() {
            let _ = self.send_command(IrcCommand::Quit(Self::clamp_quit_reason(reason)));
        }
        self.connected = false;
        self.connecting = false;
        self.connection_lost = false;
        self.cmd_tx = None;
        // Dropping the receiver signals the connection thread (the handshake and
        // read loop both watch for a closed channel), so a wedged connect attempt
        // can be aborted instead of blocking future reconnects.
        self.msg_rx = None;
        self.lag_ms = None;
        self.ping_sent_time = None;
        self.mark_channel_tabs_unjoined();
    }

    pub fn request_reconnect_after_close(&mut self, reason: &str) {
        if self.cmd_tx.is_some() {
            self.connection_intent = ConnectionIntent::ReconnectAfterClose;
            let _ = self.send_command(IrcCommand::Quit(Self::clamp_quit_reason(Some(
                reason.to_string(),
            ))));
            self.connected = false;
            self.connecting = false;
            self.connection_lost = false;
            self.cmd_tx = None;
        } else {
            self.connection_intent = ConnectionIntent::None;
            self.connected = false;
            self.connecting = true;
            self.connection_lost = false;
        }
        self.lag_ms = None;
        self.ping_sent_time = None;
        self.mark_channel_tabs_unjoined();
    }

    fn fail_registration(&mut self, message: String) {
        self.add_server_message(ChatMessage::system_fmt(&message, &self.timestamp_format));
        self.request_manual_disconnect(Some("Registration failed".to_string()));
    }

    fn reset_session_state(&mut self) {
        self.channels.clear();
        self.current_channel = None;
        self.server_messages.clear();
        self.server_unread = 0;
        self.selected_user = None;
        self.pending_invites.clear();
        self.pending_channel_keys.clear();
        self.pending_auto_perform = None;
        self.channel_list.clear();
        self.channel_list_dirty = true;
        self.channel_list_loading = false;
        self.away_status = None;
        self.auto_away_triggered = false;
        self.reset_connection_support();
    }

    /// Load a server favorite into the connection form
    fn load_favorite(&mut self, fav: &ServerFavorite) {
        self.server_host = fav.host.clone();
        self.server_port = fav.port.clone();
        self.use_tls = fav.use_tls;
        self.password = fav.password.clone();
        if !fav.nickname.is_empty() {
            self.nickname = fav.nickname.clone();
        }
        self.auto_join_channels = fav.auto_join.clone();
        self.auto_perform = fav.auto_perform.clone();
        self.pre_away_message = fav.pre_away_message.clone();
        self.persistence_profile = fav.persistence_profile.clone();
        self.sasl_username = fav.sasl_username.clone();
        self.sasl_password = fav.sasl_password.clone();
        // Restore the previously-dropped connection details. username/realname are
        // only applied when non-empty so loading an older favorite (serde default)
        // doesn't wipe the current values.
        if !fav.username.is_empty() {
            self.username = fav.username.clone();
        }
        if !fav.realname.is_empty() {
            self.realname = fav.realname.clone();
        }
        self.accept_invalid_certs = fav.accept_invalid_certs;
    }
}

fn rand_suffix() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        % 10000) as u32
}

fn is_irc_nick_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '_' | '[' | ']' | '\\' | '`' | '^' | '{' | '}' | '|' | '~'
        )
}

/// True when `needle_key` occurs in `haystack_key` at IRC-identifier
/// boundaries: the characters immediately before and after each occurrence
/// must not themselves be nick characters. Both arguments must already be
/// casemapped consistently (see `NetworkSupport::canonicalize`).
fn ident_contains(haystack_key: &str, needle_key: &str) -> bool {
    if needle_key.is_empty() {
        return false;
    }
    haystack_key.match_indices(needle_key).any(|(start, _)| {
        let end = start + needle_key.len();
        let before = haystack_key[..start].chars().next_back();
        let after = haystack_key[end..].chars().next();
        !before.is_some_and(is_irc_nick_char) && !after.is_some_and(is_irc_nick_char)
    })
}

/// Hash of every input that shapes a scrollback row's height: the content
/// width and the two font sizes used by message rows. A change in any of them
/// invalidates all cached row heights.
fn row_layout_key(ui: &egui::Ui, content_width: f32) -> u64 {
    let body = ui.style().text_styles[&egui::TextStyle::Body]
        .size
        .to_bits();
    let mono = ui.style().text_styles[&egui::TextStyle::Monospace]
        .size
        .to_bits();
    (content_width.to_bits() as u64) ^ (body as u64).rotate_left(32) ^ (mono as u64).rotate_left(17)
}

/// How far below the scroll area's clip rect scratch measurement rows are
/// parked: far enough that nothing they paint can ever be visible.
const SCRATCH_MEASURE_OFFSET: f32 = 100_000.0;

/// The row's cached advance, measuring it first if the cache is cold or was
/// invalidated by a layout change (width/font size).
///
/// Measurement lays the row out for real inside a scratch child Ui far below
/// the clip rect: nothing paints, but egui runs the full widget layout, so
/// the cached height is exactly what drawing will occupy - including egui's
/// trailing item_spacing inside the row's horizontal layout. Metric-based
/// estimation cannot stay in lockstep with widget rendering; this can.
///
/// The scratch Ui is created with [`Ui::new_child`] rather than
/// `scope_builder` on purpose: scope folds the child's min_rect back into
/// the parent cursor (`advance_cursor_after_rect`), which would balloon the
/// scroll content to ~100k px for every frame that measured a new row -
/// visibly flashing the whole message area blank.
fn row_height(
    ui: &mut egui::Ui,
    msg: &ChatMessage,
    my_nick: &str,
    content_width: f32,
    layout_key: u64,
    stats: &mut ScrollbackStats,
) -> f32 {
    let cached = msg.row_height.get();
    if cached.key == layout_key && cached.height > 0.0 {
        return cached.height;
    }
    stats.measured += 1;
    let scratch_origin = egui::pos2(
        ui.max_rect().left(),
        ui.max_rect().bottom() + SCRATCH_MEASURE_OFFSET,
    );
    let scratch_rect = egui::Rect::from_min_size(scratch_origin, egui::vec2(content_width, 0.0));
    let mut scratch = ui.new_child(egui::UiBuilder::new().max_rect(scratch_rect));
    draw_chat_row(&mut scratch, msg, my_nick);
    let h = scratch.min_rect().height();
    msg.row_height.set(RowHeightEntry {
        key: layout_key,
        height: h,
    });
    h
}

/// Draw one scrollback row: timestamp, optional indicator, nick, body.
fn draw_chat_row(ui: &mut egui::Ui, msg: &ChatMessage, my_nick: &str) {
    let is_own_msg = !msg.is_system && msg.sender.eq_ignore_ascii_case(my_nick);

    let render_row = |ui: &mut egui::Ui| {
        ui.horizontal(|ui| {
            if msg.is_highlight {
                ui.label(RichText::new("*").color(Color32::YELLOW).strong());
            }

            ui.label(
                RichText::new(&msg.timestamp)
                    .color(Color32::GRAY)
                    .monospace(),
            );

            if msg.is_system {
                render_segments(ui, msg.render_segments(), Color32::GRAY);
            } else if msg.is_action {
                let action_color = if msg.is_highlight {
                    Color32::YELLOW
                } else if is_own_msg {
                    Color32::from_rgb(100, 180, 220)
                } else {
                    Color32::from_rgb(150, 100, 200)
                };
                let sender = msg.oper.as_ref().map_or_else(
                    || msg.sender.clone(),
                    |oper| format!("{} [{}]", msg.sender, oper),
                );
                ui.label(RichText::new(format!("* {sender} ")).color(action_color));
                if let Some(reply_to) = &msg.reply_to {
                    ui.label(
                        RichText::new(format!("↪ {reply_to}"))
                            .small()
                            .color(Color32::GRAY),
                    );
                }
                render_segments(ui, msg.render_segments(), action_color);
            } else {
                let sender = msg.oper.as_ref().map_or_else(
                    || msg.sender.clone(),
                    |oper| format!("{} [{}]", msg.sender, oper),
                );
                let nick_style = if is_own_msg {
                    RichText::new(format!("<{sender}>")).color(Color32::from_rgb(100, 200, 255))
                } else {
                    RichText::new(format!("<{sender}>")).color(nick_color(&msg.sender))
                };
                ui.label(nick_style);

                if let Some(reply_to) = &msg.reply_to {
                    ui.label(
                        RichText::new(format!("↪ {reply_to}"))
                            .small()
                            .color(Color32::GRAY),
                    );
                }

                let text_color = if msg.is_highlight {
                    Color32::YELLOW
                } else if is_own_msg {
                    Color32::from_rgb(200, 210, 220)
                } else {
                    Color32::WHITE
                };
                render_segments(ui, msg.render_segments(), text_color);
            }
        });
    };

    // Only highlighted/own rows pay for a Frame (a nested Ui with its own
    // layout pass); plain rows - the overwhelming majority - are laid out
    // directly. Frame::NONE adds no margin, so heights match measurement.
    if msg.is_highlight {
        egui::Frame::NONE
            .fill(Color32::from_rgb(60, 40, 20))
            .show(ui, render_row);
    } else if is_own_msg {
        egui::Frame::NONE
            .fill(Color32::from_rgb(25, 35, 45))
            .show(ui, render_row);
    } else {
        render_row(ui);
    }
}

/// Statistics from one virtualized scrollback pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollbackStats {
    /// Rows actually laid out this frame (visible band plus straddlers).
    pub drawn: usize,
    /// Heights that had to be measured fresh (cold cache or layout change).
    pub measured: usize,
    pub total_rows: usize,
}

#[cfg(unix)]
fn wait_for_notification_helper(
    command: &mut std::process::Command,
) -> std::io::Result<std::process::ExitStatus> {
    // `status` waits for and reaps the child. Dropping a handle returned by
    // `spawn` would leave an exited helper as a zombie on Unix.
    command.status()
}

impl IrcApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self::default()
    }

    fn session_from_form(&self) -> Result<SessionConfig, String> {
        let mut host = self.server_host.trim();
        if let Some(inner) = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
        {
            host = inner;
        }
        if host.is_empty() {
            return Err("Server host cannot be empty".to_string());
        }
        let port = commands::parse_server_port(&self.server_port)?;
        if self.pre_away_message.contains(['\r', '\n']) || self.pre_away_message.len() > 300 {
            return Err("Pre-away message must be one IRC line of at most 300 bytes".to_string());
        }
        let persistence_profile = self.persistence_profile.trim();
        if !persistence_profile.is_empty()
            && (persistence_profile.len() > 32
                || !persistence_profile
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
        {
            return Err("Persistence profile must be 1-32 letters, digits, '_' or '-'".to_string());
        }

        Ok(SessionConfig {
            server: ServerConfig {
                host: host.to_string(),
                port,
                use_tls: self.use_tls,
                accept_invalid_certs: self.accept_invalid_certs,
                nick: self.nickname.clone(),
                username: self.username.clone(),
                realname: self.realname.clone(),
                password: if self.password.is_empty() {
                    None
                } else {
                    Some(self.password.clone())
                },
                sasl_username: if self.sasl_username.is_empty() {
                    None
                } else {
                    Some(self.sasl_username.clone())
                },
                sasl_password: if self.sasl_password.is_empty() {
                    None
                } else {
                    Some(self.sasl_password.clone())
                },
                pre_away_message: if self.pre_away_message.is_empty() {
                    None
                } else {
                    Some(self.pre_away_message.clone())
                },
                persistence_profile: if persistence_profile.is_empty() {
                    None
                } else {
                    Some(persistence_profile.to_string())
                },
            },
            auto_join_channels: self.auto_join_channels.clone(),
            auto_perform: self.auto_perform.clone(),
            set_invisible: self.set_invisible,
        })
    }

    fn activate_session(&mut self, session: SessionConfig) {
        let endpoint_changed = self
            .active_session
            .as_ref()
            .is_some_and(|active| active.endpoint_key() != session.endpoint_key());
        if endpoint_changed {
            // Do not let buffered messages from the old socket recreate tabs or
            // logs after the new endpoint identity becomes active.
            self.msg_rx = None;
            self.cmd_tx = None;
            self.reset_session_state();
        }
        self.set_my_nick(session.server.nick.clone());
        self.active_session = Some(session);
        self.pending_session = None;
        self.connection_error = None;
    }

    pub(crate) fn start_session_from_form(&mut self) -> bool {
        match self.session_from_form() {
            Ok(session) => {
                self.activate_session(session);
                self.connected = false;
                self.connecting = true;
                self.connection_lost = false;
                true
            }
            Err(error) => {
                self.connection_error = Some(error.clone());
                self.connecting = false;
                self.show_connect_dialog = true;
                self.add_server_message(ChatMessage::system_fmt(&error, &self.timestamp_format));
                false
            }
        }
    }

    pub(crate) fn validate_connection_form(&mut self) -> bool {
        match self.session_from_form() {
            Ok(_) => {
                self.connection_error = None;
                true
            }
            Err(error) => {
                self.connection_error = Some(error);
                false
            }
        }
    }

    pub(crate) fn switch_session_from_form(&mut self, reason: &str) -> bool {
        let session = match self.session_from_form() {
            Ok(session) => session,
            Err(error) => {
                self.connection_error = Some(error.clone());
                self.add_server_message(ChatMessage::system_fmt(&error, &self.timestamp_format));
                return false;
            }
        };

        // `connecting` can be set one UI frame before the lifecycle creates a
        // worker/channel. In that state there is no old socket to close, so the
        // fresh snapshot can be activated immediately. Staging is only needed
        // while an actual transport (or registered connection) exists.
        if self.connected || self.cmd_tx.is_some() || self.msg_rx.is_some() {
            self.pending_session = Some(session);
            self.request_reconnect_after_close(reason);
        } else {
            self.activate_session(session);
            self.connected = false;
            self.connecting = true;
            self.connection_lost = false;
        }
        true
    }

    pub(crate) fn promote_pending_session(&mut self) {
        if let Some(session) = self.pending_session.take() {
            self.activate_session(session);
        }
    }

    pub(crate) fn ensure_active_session(&mut self) -> bool {
        if self.active_session.is_some() {
            true
        } else {
            self.start_session_from_form()
        }
    }

    pub(crate) fn active_server_config(&self) -> Option<ServerConfig> {
        self.active_session
            .as_ref()
            .map(|session| session.server.clone())
    }

    fn active_log_context(&self) -> Option<(String, String, bool)> {
        self.active_session.as_ref().map(|session| {
            (
                session.log_identity(),
                session.server.host.clone(),
                session.allows_legacy_log_migration(),
            )
        })
    }

    fn active_endpoint_label(&self) -> String {
        self.active_session
            .as_ref()
            .map(SessionConfig::endpoint_label)
            .unwrap_or_else(|| format!("{}:{}", self.server_host, self.server_port))
    }

    pub(crate) fn note_unprofiled_endpoint_edit(&mut self) {
        self.clear_endpoint_credentials();
        self.connection_error = None;
    }

    pub(crate) fn apply_unprofiled_endpoint(&mut self, host: &str, port: &str, use_tls: bool) {
        // Selecting a preset is an unprofiled action even when it happens to
        // name the same endpoint. Only loading an explicit favorite/profile is
        // allowed to carry endpoint-scoped credentials and automation.
        self.clear_endpoint_credentials();
        self.server_host = host.to_string();
        self.server_port = port.to_string();
        self.use_tls = use_tls;
        self.connection_error = None;
    }

    pub(crate) fn connection_error(&self) -> Option<&str> {
        self.connection_error.as_deref()
    }

    fn current_session_automation(&self) -> (bool, String, String) {
        self.active_session
            .as_ref()
            .map(|session| {
                (
                    session.set_invisible,
                    session.auto_join_channels.clone(),
                    session.auto_perform.clone(),
                )
            })
            .unwrap_or_else(|| {
                (
                    self.set_invisible,
                    self.auto_join_channels.clone(),
                    self.auto_perform.clone(),
                )
            })
    }

    #[cfg(test)]
    fn active_endpoint_key(&self) -> Option<EndpointKey> {
        self.active_session
            .as_ref()
            .map(SessionConfig::endpoint_key)
    }

    #[cfg(test)]
    fn form_server_config_for_tests(&self) -> Result<ServerConfig, String> {
        self.session_from_form().map(|session| session.server)
    }

    pub fn handle_incoming_message(&mut self, msg: IrcMessage) {
        if self.collect_multiline_line(&msg) {
            self.scroll_to_bottom = true;
            return;
        }
        if self.collect_authtoken_line(&msg) {
            self.scroll_to_bottom = true;
            return;
        }
        let history_batch = msg
            .get_batch()
            .and_then(|batch_id| self.containing_history_batch(&batch_id));
        if let Some(batch_id) = history_batch.as_deref()
            && let Some(event) = self.history_event_message(&msg)
        {
            self.queue_chathistory_message(batch_id, event);
            self.scroll_to_bottom = true;
            return;
        }
        match &msg.command {
            IrcCommand::Privmsg(target, content) => {
                let sender = msg.get_sender_nick().unwrap_or_else(|| "???".to_string());
                let routed_target = self.strip_status_prefix(target).to_string();

                // Check if sender is ignored
                if self.is_ignored(&sender, msg.prefix.as_deref()) {
                    tracing::debug!("Ignoring message from {}", sender);
                    return;
                }

                // Handle CTCP requests (except ACTION which is displayed as a message)
                if history_batch.is_none() && self.handle_ctcp_request(&sender, content) {
                    return; // CTCP was handled, don't display as regular message
                }

                let is_action = content.starts_with("\x01ACTION ") && content.ends_with('\x01');

                // Check if the message mentions our nick (case-insensitive word boundary check)
                let is_highlight = self.check_nick_mention(content);

                // Get server-time from IRCv3 tags if available, converted to
                // local time and rendered in the user's timestamp format.
                let server_time = msg
                    .get_server_time()
                    .map(|epoch| epoch_time_formatted(epoch, &self.timestamp_format));
                let server_time_raw = msg.get_server_time_raw();
                let msgid = msg.get_msgid();
                let account = msg.get_account();

                let fmt = &self.timestamp_format;
                let chat_msg = if msg.get_tag("+evilnet.github.io/chathistory-gap").is_some() {
                    ChatMessage::system_fmt(content, fmt).with_server_time(server_time)
                } else if is_action {
                    let action_text = content
                        .strip_prefix("\x01ACTION ")
                        .and_then(|s| s.strip_suffix('\x01'))
                        .unwrap_or(content);
                    if is_highlight {
                        ChatMessage::action_highlighted_fmt(&sender, action_text, fmt)
                            .with_server_time(server_time)
                    } else {
                        ChatMessage::action_fmt(&sender, action_text, fmt)
                            .with_server_time(server_time)
                    }
                } else if is_highlight {
                    ChatMessage::highlighted_fmt(&sender, content, fmt)
                        .with_server_time(server_time)
                } else {
                    ChatMessage::new_fmt(&sender, content, fmt).with_server_time(server_time)
                }
                .with_irc_metadata(server_time_raw, msgid, account)
                .with_oper(msg.get_oper())
                .with_reply_to(msg.get_reply());

                if let Some(batch_id) = &history_batch {
                    self.queue_chathistory_message(batch_id, chat_msg.without_logging());
                    self.scroll_to_bottom = true;
                    return;
                }

                // Determine target channel/query
                let is_pm = !self.is_channel_name(&routed_target)
                    && self.identifiers_equal(&routed_target, &self.my_nick);
                let target_name = if self.is_channel_name(&routed_target) {
                    routed_target
                } else if is_pm {
                    // Private message to us - use sender as channel
                    sender.clone()
                } else {
                    target.clone()
                };
                tracing::debug!(
                    "PRIVMSG: target={:?} sender={:?} is_pm={} target_name={:?}",
                    target,
                    sender,
                    is_pm,
                    target_name
                );

                // Send desktop notification for highlights and PMs
                // Skip if we sent it ourselves
                let is_from_self = self.identifiers_equal(&sender, &self.my_nick);
                if is_pm && !is_from_self {
                    // PMs always notify (force=true)
                    self.send_notification(
                        &format!("PM from {}", sender),
                        &truncate_chars(content, 50),
                        true,
                    );
                } else if is_highlight && !is_from_self {
                    // Highlights only notify when not focused
                    self.send_notification(
                        &format!("{} mentioned you in {}", sender, target_name),
                        &truncate_chars(content, 50),
                        false,
                    );
                }

                if !is_from_self || !self.merge_server_echo(&target_name, &chat_msg) {
                    self.add_message_to_channel(&target_name, chat_msg);
                }
            }

            IrcCommand::Notice(target, content) => {
                let sender = msg
                    .get_sender_nick()
                    .unwrap_or_else(|| "Server".to_string());
                let routed_target = self.strip_status_prefix(target).to_string();

                // NOTICE uses a decorated local confirmation row rather than a
                // normal chat row, so its server echo cannot be merged by
                // identity. Suppress only our own negotiated echo.
                if self.enabled_caps.contains("echo-message")
                    && self.identifiers_equal(&sender, &self.my_nick)
                {
                    return;
                }

                // Check if sender is ignored (but not server notices)
                if msg.prefix.is_some() && self.is_ignored(&sender, msg.prefix.as_deref()) {
                    tracing::debug!("Ignoring notice from {}", sender);
                    return;
                }

                if let Some(batch_id) = &history_batch {
                    let server_time = msg
                        .get_server_time()
                        .map(|epoch| epoch_time_formatted(epoch, &self.timestamp_format));
                    let replayed = ChatMessage::system_fmt(
                        &format!("-{sender}- {content}"),
                        &self.timestamp_format,
                    )
                    .with_server_time(server_time)
                    .with_irc_metadata(
                        msg.get_server_time_raw(),
                        msg.get_msgid(),
                        msg.get_account(),
                    )
                    .without_logging();
                    self.queue_chathistory_message(batch_id, replayed);
                    self.scroll_to_bottom = true;
                    return;
                }

                // Check for CTCP reply (starts and ends with \x01).
                // Require >= 2 bytes so the opening and closing \x01 are distinct;
                // a lone "\x01" would otherwise slice as content[1..0] and panic.
                if content.len() >= 2 && content.starts_with('\x01') && content.ends_with('\x01') {
                    let ctcp_content = &content[1..content.len() - 1];
                    let parts: Vec<&str> = ctcp_content.splitn(2, ' ').collect();
                    let ctcp_cmd = parts[0];
                    let ctcp_reply = parts.get(1).copied().unwrap_or("");

                    // Handle CTCP PING reply to show latency
                    if ctcp_cmd.eq_ignore_ascii_case("PING") {
                        if let Ok(sent_time) = ctcp_reply.parse::<u128>() {
                            use std::time::{SystemTime, UNIX_EPOCH};
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis();
                            let latency = now.saturating_sub(sent_time);
                            self.add_message_to_current(ChatMessage::system(&format!(
                                "[CTCP PING reply] {} - {}ms",
                                sender, latency
                            )));
                        } else {
                            self.add_message_to_current(ChatMessage::system(&format!(
                                "[CTCP PING reply] {} - {}",
                                sender, ctcp_reply
                            )));
                        }
                    } else {
                        // Other CTCP replies (VERSION, TIME, etc.)
                        self.add_message_to_current(ChatMessage::system(&format!(
                            "[CTCP {} reply] {} - {}",
                            ctcp_cmd, sender, ctcp_reply
                        )));
                    }
                    return;
                }

                let chat_msg = ChatMessage::system(&format!("-{}- {}", sender, content))
                    .with_irc_metadata(
                        msg.get_server_time_raw(),
                        msg.get_msgid(),
                        msg.get_account(),
                    );

                if routed_target == "*" || !self.connected {
                    self.add_server_message(chat_msg);
                } else {
                    // For private notices (target is our nick), route to sender's window
                    // This handles NickServ, X3, and other service responses
                    let is_private = !self.is_channel_name(&routed_target)
                        && self.identifiers_equal(&routed_target, &self.my_nick);
                    let target_name = if is_private {
                        sender.clone() // Route to sender's query window
                    } else {
                        routed_target
                    };
                    self.add_message_to_channel(&target_name, chat_msg);
                }
            }

            IrcCommand::StatusMessage(status, target, content) => {
                let sender = msg
                    .get_sender_nick()
                    .unwrap_or_else(|| "Server".to_string());
                let target = self.strip_status_prefix(target).to_string();
                let label = match status {
                    '@' => "ops",
                    '%' => "halfops",
                    '+' => "voices",
                    _ => "members",
                };
                let chat_msg = ChatMessage::system_fmt(
                    &format!("-{sender}/{label}- {content}"),
                    &self.timestamp_format,
                )
                .with_irc_metadata(
                    msg.get_server_time_raw(),
                    msg.get_msgid(),
                    msg.get_account(),
                );
                self.add_message_to_channel(&target, chat_msg);
            }

            IrcCommand::Tagmsg(target) => {
                // Typing notifications are transient UI state; until a typing
                // indicator is visible, consuming them silently is preferable
                // to printing protocol noise into the conversation.
                if let Some(reaction) = msg.get_tag("+draft/react") {
                    let sender = msg.get_sender_nick().unwrap_or_else(|| "???".to_string());
                    let reply_to = msg
                        .get_reply()
                        .map(|id| format!(" to {id}"))
                        .unwrap_or_default();
                    let target = self.strip_status_prefix(target).to_string();
                    let target = if !self.is_channel_name(&target)
                        && self.identifiers_equal(&target, &self.my_nick)
                    {
                        sender.clone()
                    } else {
                        target
                    };
                    let reaction_message = ChatMessage::system_fmt(
                        &format!("{sender} reacted {reaction}{reply_to}"),
                        &self.timestamp_format,
                    )
                    .with_irc_metadata(
                        msg.get_server_time_raw(),
                        msg.get_msgid(),
                        msg.get_account(),
                    );
                    if let Some(batch_id) = history_batch.as_deref() {
                        self.queue_chathistory_message(
                            batch_id,
                            reaction_message.without_logging(),
                        );
                    } else {
                        self.add_message_to_channel(&target, reaction_message);
                    }
                }
            }

            IrcCommand::StandardReply {
                kind,
                command,
                code,
                context,
                description,
            } => {
                let context_text = context.join(" ");
                let text = if context_text.is_empty() {
                    format!("[{kind} {command}/{code}] {description}")
                } else {
                    format!("[{kind} {command}/{code}] {context_text}: {description}")
                };
                let target = context
                    .iter()
                    .flat_map(|value| value.split_whitespace())
                    .find(|value| self.is_channel_name(value))
                    .map(str::to_string);
                let message = ChatMessage::system_fmt(&text, &self.timestamp_format)
                    .with_irc_metadata(msg.get_server_time_raw(), None, None);
                if let Some(target) = target {
                    self.add_message_to_channel(&target, message);
                } else {
                    self.add_server_message(message);
                }
            }

            IrcCommand::Markread(target, timestamp) => {
                if let Some(key) = self.channel_key(target)
                    && let Some(channel) = self.channels.get_mut(&key)
                {
                    channel.read_marker = timestamp.clone();
                }
            }

            IrcCommand::Redact(target, msgid, reason) => {
                if let Some(key) = self.channel_key(target)
                    && let Some(channel) = self.channels.get_mut(&key)
                {
                    let removed = channel.redact_message(msgid);
                    let reason = reason
                        .as_deref()
                        .filter(|reason| !reason.is_empty())
                        .map(|reason| format!(": {reason}"))
                        .unwrap_or_default();
                    channel.push_trimmed(
                        ChatMessage::system_fmt(
                            &if removed {
                                format!("A message was redacted{reason}")
                            } else {
                                format!("Message {msgid} was redacted{reason}")
                            },
                            &self.timestamp_format,
                        )
                        .without_logging(),
                        self.max_scrollback,
                    );
                }
            }

            IrcCommand::Rename(old, new, reason) => {
                if let Some(old_key) = self.channel_key(old)
                    && let Some(mut channel) = self.channels.remove(&old_key)
                {
                    let destination_was_current = self.current_target_is(new);
                    if let Some(existing_key) = self.channel_key(new)
                        && let Some(mut existing) = self.channels.remove(&existing_key)
                    {
                        // A public-history tab for the destination may already
                        // exist. Preserve it and deduplicate any overlap before
                        // turning the renamed live channel into that tab.
                        while let Some(message) = existing.messages.pop_back() {
                            let duplicate = message.msgid.as_ref().is_some_and(|msgid| {
                                channel
                                    .messages
                                    .iter()
                                    .any(|row| row.msgid.as_deref() == Some(msgid.as_str()))
                            });
                            if !duplicate {
                                channel.messages.push_front(message);
                            }
                        }
                        channel.unread = channel.unread.saturating_add(existing.unread);
                        while channel.messages.len() > self.max_scrollback {
                            channel.messages.pop_front();
                        }
                    }
                    channel.push_trimmed(
                        ChatMessage::system_fmt(
                            &format!(
                                "Channel renamed from {old} to {new}{}",
                                reason
                                    .as_deref()
                                    .filter(|reason| !reason.is_empty())
                                    .map(|reason| format!(": {reason}"))
                                    .unwrap_or_default()
                            ),
                            &self.timestamp_format,
                        ),
                        self.max_scrollback,
                    );
                    if self.current_target_is(&old_key) || destination_was_current {
                        self.current_channel = Some(new.clone());
                    }
                    self.channels.insert(new.clone(), channel);
                    let old_pending = self.network_support.canonicalize(old);
                    let new_pending = self.network_support.canonicalize(new);
                    if let Some(requests) = self.pending_history_requests.remove(&old_pending) {
                        self.pending_history_requests
                            .entry(new_pending)
                            .or_default()
                            .extend(requests);
                    }
                }
            }

            IrcCommand::Relocate(old, new, reason) => {
                let reason = reason
                    .as_deref()
                    .filter(|reason| !reason.is_empty())
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default();
                self.add_message_to_channel(
                    old,
                    ChatMessage::system_fmt(
                        &format!("Channel has moved to {new}{reason}; use /join {new} to follow"),
                        &self.timestamp_format,
                    ),
                );
            }

            IrcCommand::Setname(realname) => {
                let sender = msg.get_sender_nick().unwrap_or_else(|| "???".to_string());
                let notice = ChatMessage::system_fmt(
                    &format!("{sender} changed their realname to {realname}"),
                    &self.timestamp_format,
                );
                let max = self.max_scrollback;
                for channel in self.channels.values_mut() {
                    if channel.has_user(&sender) {
                        channel.set_user_realname(&sender, Some(realname.clone()));
                        channel.push_trimmed(notice.clone(), max);
                    }
                }
            }

            IrcCommand::Extension(name, params) => {
                let text = if name.eq_ignore_ascii_case("TOKEN")
                    && params
                        .first()
                        .is_some_and(|subcommand| subcommand.eq_ignore_ascii_case("SERVICE"))
                {
                    let key = params.get(1).map(String::as_str).unwrap_or("?");
                    let url = params.get(2).map(String::as_str).unwrap_or("");
                    let description = params.get(3).map(String::as_str).unwrap_or("");
                    format!("Token service {key} ({url}): {description}")
                } else if name.eq_ignore_ascii_case("TOKEN")
                    && params
                        .first()
                        .is_some_and(|subcommand| subcommand.eq_ignore_ascii_case("GENERATE"))
                {
                    let service = params.get(1).map(String::as_str).unwrap_or("?");
                    let token = params.get(2).map(String::as_str).unwrap_or("");
                    format!("[TOKEN GENERATE {service}] {token}")
                } else {
                    format!("[{name}] {}", params.join(" "))
                };
                self.add_server_message(
                    ChatMessage::system_fmt(&text, &self.timestamp_format).without_logging(),
                );
            }

            IrcCommand::ChathistoryTarget(target, timestamp) => {
                self.add_server_message(
                    ChatMessage::system_fmt(
                        &format!("History target {target}: last activity {timestamp}"),
                        &self.timestamp_format,
                    )
                    .without_logging(),
                );
            }

            IrcCommand::Join(channel, _key, account, realname) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                if self.identifiers_equal(&sender, &self.my_nick) {
                    // We joined a channel
                    let existing_key = self.channel_key(channel);
                    let is_new_channel = existing_key.is_none();
                    if existing_key.is_none() {
                        let mut new_channel = Channel::new();
                        new_channel.set_case_mapping(self.network_support.case_mapping);
                        // Check for pending key and store it
                        if let Some(key) = self
                            .pending_channel_keys
                            .remove(&self.network_support.canonicalize(channel))
                        {
                            new_channel.key = Some(key);
                        }
                        // Prefer server-backed history when negotiated. It has
                        // authoritative server-time and, on Nefarious +H
                        // channels, is also available without membership.
                        if !self.chathistory_enabled
                            && self.logging_load_history
                            && let Some((network, legacy_host, allow_legacy)) =
                                self.active_log_context()
                        {
                            let history = self.log_manager.load_history_with_legacy(
                                &network,
                                &legacy_host,
                                channel,
                                self.logging_history_lines,
                                allow_legacy,
                            );
                            if !history.is_empty() {
                                new_channel.messages.push_back(ChatMessage::system(&format!(
                                    "--- {} lines of history loaded ---",
                                    history.len()
                                )));
                                new_channel.messages.extend(history);
                            }
                            // Log session start
                            self.log_manager.log_session_start(&network, channel);
                        }
                        self.channels.insert(channel.clone(), new_channel);
                    }
                    if let Some(joined_channel) = self.channel_mut(channel) {
                        joined_channel.joined = true;
                    }
                    if self.enabled_caps.contains("no-implicit-names") {
                        self.send_command(IrcCommand::Names(Some(channel.clone())));
                    }
                    self.current_channel = Some(existing_key.unwrap_or_else(|| channel.clone()));
                    let sys_msg = ChatMessage::system(&format!("Now talking in {}", channel));
                    self.add_message_to_channel(channel, sys_msg);
                    if self.chathistory_enabled && self.logging_load_history {
                        self.request_latest_history(
                            channel,
                            None,
                            if is_new_channel {
                                HistoryPlacement::Prepend
                            } else {
                                HistoryPlacement::Append
                            },
                        );
                    }
                } else {
                    if !self.hide_join_part {
                        // Include account info if available (IRCv3 extended-join)
                        let sys_msg = match (account, realname) {
                            (Some(acct), Some(real)) => ChatMessage::system(&format!(
                                "{} ({}) [{}] has joined {}",
                                sender, real, acct, channel
                            )),
                            (Some(acct), None) => ChatMessage::system(&format!(
                                "{} [{}] has joined {}",
                                sender, acct, channel
                            )),
                            (None, Some(real)) => ChatMessage::system(&format!(
                                "{} ({}) has joined {}",
                                sender, real, channel
                            )),
                            (None, None) => {
                                ChatMessage::system(&format!("{} has joined {}", sender, channel))
                            }
                        };
                        self.add_message_to_channel(channel, sys_msg);
                    }
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.add_user(&sender, UserMode::Normal);
                        ch.set_user_account(&sender, account.clone());
                        ch.set_user_realname(&sender, realname.clone());
                    }
                }
            }

            IrcCommand::Part(channel, reason) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                let reason_str = reason.as_deref().unwrap_or("");
                if self.identifiers_equal(&sender, &self.my_nick) {
                    self.remove_channel(channel);
                    if self.current_target_is(channel) {
                        self.current_channel = self.channels.keys().next().cloned();
                    }
                } else {
                    if !self.hide_join_part {
                        let sys_msg = ChatMessage::system(&format!(
                            "{} has left {} ({})",
                            sender, channel, reason_str
                        ));
                        self.add_message_to_channel(channel, sys_msg);
                    }
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.remove_user(&sender);
                    }
                }
            }

            IrcCommand::Kick(channel, kicked, reason) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                let reason_str = reason.as_deref().unwrap_or("");
                let max = self.max_scrollback;
                if self.identifiers_equal(kicked, &self.my_nick) {
                    // We were kicked: keep the tab visible with a notice, but clear
                    // membership state since the server has removed us.
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.joined = false;
                        ch.clear_users();
                        ch.push_trimmed(
                            ChatMessage::system(&format!(
                                "You were kicked from {} by {} ({})",
                                channel, sender, reason_str
                            )),
                            max,
                        );
                    }
                    self.send_notification(
                        &format!("Kicked from {}", channel),
                        &format!("by {} ({})", sender, reason_str),
                        false,
                    );
                } else {
                    // Kicks are moderation events, shown even when join/part is hidden.
                    let sys_msg = ChatMessage::system(&format!(
                        "{} was kicked from {} by {} ({})",
                        kicked, channel, sender, reason_str
                    ));
                    self.add_message_to_channel(channel, sys_msg);
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.remove_user(kicked);
                    }
                }
            }

            IrcCommand::Quit(reason) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                let reason_str = reason.as_deref().unwrap_or("Quit");

                // Remove user from all channels, optionally show quit message
                let max = self.max_scrollback;
                for channel in self.channels.values_mut() {
                    if channel.has_user(&sender) {
                        if !self.hide_join_part {
                            let sys_msg = ChatMessage::system(&format!(
                                "{} has quit ({})",
                                sender, reason_str
                            ));
                            channel.push_trimmed(sys_msg, max);
                        }
                        channel.remove_user(&sender);
                    }
                }
            }

            IrcCommand::Nick(new_nick) => {
                let old_nick = msg.get_sender_nick().unwrap_or_default();
                if self.identifiers_equal(&old_nick, &self.my_nick) {
                    self.set_my_nick(new_nick.clone());
                }
                let sys_msg =
                    ChatMessage::system(&format!("{} is now known as {}", old_nick, new_nick));
                let max = self.max_scrollback;
                for channel in self.channels.values_mut() {
                    if channel.has_user(&old_nick) {
                        channel.push_trimmed(sys_msg.clone(), max);
                        channel.rename_user(&old_nick, new_nick);
                    }
                }
                // Re-key an open query (PM) window so the conversation follows
                // the rename: otherwise new messages open a second tab under
                // the new nick and replies in the stale tab go to the old nick.
                let query_key = self
                    .channels
                    .keys()
                    .find(|k| !self.is_channel_name(k) && self.identifiers_equal(k, &old_nick))
                    .cloned();
                if let Some(key) = query_key
                    && !self
                        .channels
                        .keys()
                        .any(|k| self.identifiers_equal(k, new_nick))
                    && let Some(mut ch) = self.channels.remove(&key)
                {
                    ch.push_trimmed(sys_msg, max);
                    let was_current = self.current_channel.as_deref() == Some(key.as_str());
                    self.channels.insert(new_nick.clone(), ch);
                    if was_current {
                        self.current_channel = Some(new_nick.clone());
                    }
                }
            }

            IrcCommand::Topic(channel, topic) => {
                let max = self.max_scrollback;
                if let Some(ch) = self.channel_mut(channel) {
                    ch.topic = topic.clone();
                    let sender = msg.get_sender_nick();
                    let sys_msg = if let Some(s) = sender {
                        ChatMessage::system(&format!(
                            "{} changed the topic to: {}",
                            s,
                            topic.as_deref().unwrap_or("")
                        ))
                    } else {
                        ChatMessage::system(&format!("Topic: {}", topic.as_deref().unwrap_or("")))
                    };
                    ch.push_trimmed(sys_msg, max);
                }
            }

            IrcCommand::Invite(target, channel) => {
                let sender = msg
                    .get_sender_nick()
                    .unwrap_or_else(|| "Someone".to_string());
                if !self.identifiers_equal(target, &self.my_nick) {
                    self.add_message_to_channel(
                        channel,
                        ChatMessage::system_fmt(
                            &format!("{sender} invited {target} to {channel}"),
                            &self.timestamp_format,
                        ),
                    );
                    return;
                }
                // Store the invite, bounding the queue so unsolicited INVITE spam
                // cannot grow it without limit (drop the oldest when full).
                const MAX_PENDING_INVITES: usize = 64;
                if self.pending_invites.len() >= MAX_PENDING_INVITES {
                    self.pending_invites.remove(0);
                }
                self.pending_invites.push((sender.clone(), channel.clone()));
                // Show prominent message in server buffer
                self.add_server_message(ChatMessage::system(&format!(
                    "*** {} has invited you to {} - type /join {} to accept",
                    sender, channel, channel
                )));
                // Send notification
                self.send_notification(
                    "Channel Invite",
                    &format!("{} invited you to {}", sender, channel),
                    false,
                );
            }

            IrcCommand::Mode(target, mode, params) => {
                let sender = msg
                    .get_sender_nick()
                    .unwrap_or_else(|| "Server".to_string());
                self.handle_mode_message(&sender, target, mode.as_deref(), params);
            }

            IrcCommand::Numeric(num, params) => {
                self.handle_numeric(*num, params);
            }

            IrcCommand::Ping(_) => {
                // Handled automatically in client
            }

            IrcCommand::Pong(_) => {
                // Calculate lag from our ping
                if let Some(sent_time) = self.ping_sent_time.take() {
                    let elapsed = sent_time.elapsed();
                    self.lag_ms = Some(elapsed.as_millis() as u32);
                }
            }

            // IRCv3 away-notify: user changed away status
            IrcCommand::Away(away_msg) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                // Update away status for user in ALL channels they're in
                self.update_user_away_status(&sender, away_msg.clone());

                // Show message once (in first shared channel)
                if !self.hide_join_part {
                    let sys_msg = if let Some(away_text) = away_msg {
                        ChatMessage::system(&format!("{} is now away: {}", sender, away_text))
                    } else {
                        ChatMessage::system(&format!("{} is back", sender))
                    };
                    // Find first channel where user is present and show message there
                    let max = self.max_scrollback;
                    for channel in self.channels.values_mut() {
                        if channel.has_user(&sender) {
                            channel.push_trimmed(sys_msg, max);
                            break;
                        }
                    }
                }
            }

            // IRCv3 account-notify: user logged in/out of account
            IrcCommand::Account(account) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                // Update account for user in all channels they're in
                for channel in self.channels.values_mut() {
                    if channel.has_user(&sender) {
                        channel.set_user_account(
                            &sender,
                            if account == "*" {
                                None
                            } else {
                                Some(account.clone())
                            },
                        );
                    }
                }
                // Optionally show message (only if not hiding join/part)
                if !self.hide_join_part {
                    let sys_msg = if account == "*" {
                        ChatMessage::system(&format!("{} has logged out", sender))
                    } else {
                        ChatMessage::system(&format!("{} has logged in as {}", sender, account))
                    };
                    // Show in channels where user is present
                    let max = self.max_scrollback;
                    for channel in self.channels.values_mut() {
                        if channel.has_user(&sender) {
                            channel.push_trimmed(sys_msg.clone(), max);
                            break; // Only show once
                        }
                    }
                }
            }

            // IRCv3 chghost: user changed their username/hostname
            IrcCommand::Chghost(new_user, new_host) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                // Update host for user in all channels (informational, we don't track hosts)
                if !self.hide_join_part {
                    let sys_msg = ChatMessage::system(&format!(
                        "{} changed host to {}@{}",
                        sender, new_user, new_host
                    ));
                    let max = self.max_scrollback;
                    for channel in self.channels.values_mut() {
                        if channel.has_user(&sender) {
                            channel.push_trimmed(sys_msg.clone(), max);
                            break; // Only show once
                        }
                    }
                }
            }

            IrcCommand::Cap(_target, subcommand, params) => {
                self.handle_cap_message(subcommand, params);
            }

            // Collect chathistory rows until the batch closes so they can be
            // inserted before live traffic as one chronological block.
            IrcCommand::Batch(reference, batch_type, params) => {
                if let Some(batch_id) = reference.strip_prefix('+') {
                    tracing::debug!("Batch started: {} type={:?}", reference, batch_type);
                    if let Some(parent) = msg.get_batch() {
                        self.batch_parents.insert(batch_id.to_string(), parent);
                    }
                    if batch_type
                        .as_deref()
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("draft/multiline"))
                        && let Some(target) = params
                        && self.multiline_batches.len() < 32
                    {
                        let parent_history = msg
                            .get_batch()
                            .and_then(|parent| self.containing_history_batch(&parent));
                        self.multiline_batches.insert(
                            batch_id.to_string(),
                            MultilineBatch {
                                target: target.clone(),
                                sender: msg.get_sender_nick().unwrap_or_else(|| "???".to_string()),
                                content: String::new(),
                                has_lines: false,
                                is_notice: false,
                                parent_history,
                                server_time: msg.get_server_time_raw(),
                                msgid: msg.get_msgid(),
                                account: msg.get_account(),
                                oper: msg.get_oper(),
                                reply_to: msg.get_reply(),
                            },
                        );
                    }
                    if batch_type
                        .as_deref()
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("draft/authtoken"))
                        && self.authtoken_batches.len() < 32
                    {
                        self.authtoken_batches.insert(
                            batch_id.to_string(),
                            AuthtokenBatch {
                                service: params.clone().unwrap_or_else(|| "*".to_string()),
                                token: String::new(),
                            },
                        );
                    }
                    if batch_type
                        .as_deref()
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("chathistory"))
                        && let Some(target) = params
                        && self.chathistory_batches.len() < 32
                    {
                        let target_key = self.network_support.canonicalize(target);
                        let request = self
                            .pending_history_requests
                            .get_mut(&target_key)
                            .and_then(VecDeque::pop_front)
                            .unwrap_or(PendingHistoryRequest {
                                placement: HistoryPlacement::Prepend,
                                direction: HistoryDirection::Before,
                                remaining: 0,
                                loaded: 0,
                                page_limit: self.max_scrollback.max(1),
                            });
                        if self
                            .pending_history_requests
                            .get(&target_key)
                            .is_some_and(VecDeque::is_empty)
                        {
                            self.pending_history_requests.remove(&target_key);
                        }
                        self.chathistory_batches.insert(
                            batch_id.to_string(),
                            ChatHistoryBatch {
                                target: target.clone(),
                                messages: Vec::new(),
                                request,
                                complete: msg.get_tag("draft/chathistory-end").is_some(),
                                partial: msg
                                    .get_tag("evilnet.github.io/chathistory-partial")
                                    .is_some(),
                            },
                        );
                    }
                } else if let Some(batch_id) = reference.strip_prefix('-') {
                    tracing::debug!("Batch ended: {}", reference);
                    self.finish_multiline_batch(batch_id);
                    self.finish_authtoken_batch(batch_id);
                    self.finish_chathistory_batch(batch_id);
                    self.batch_parents.remove(batch_id);
                }
            }

            _ => {
                // Log unknown messages
                let sys_msg = ChatMessage::system(&msg.raw);
                self.add_server_message(sys_msg);
            }
        }

        self.scroll_to_bottom = true;
    }

    fn apply_isupport_tokens(&mut self, params: &[String]) {
        let previous_case_mapping = self.network_support.case_mapping;
        for token in params.iter().skip(1) {
            if token.starts_with(':') {
                break;
            }
            if let Some(value) = token.strip_prefix("CHANTYPES=") {
                if !value.is_empty() {
                    self.network_support.channel_types = value.to_string();
                }
            } else if let Some(value) = token.strip_prefix("CASEMAPPING=") {
                if value.eq_ignore_ascii_case("ascii") {
                    self.network_support.case_mapping = CaseMapping::Ascii;
                } else if value.eq_ignore_ascii_case("strict-rfc1459") {
                    self.network_support.case_mapping = CaseMapping::StrictRfc1459;
                } else if value.eq_ignore_ascii_case("rfc1459") {
                    self.network_support.case_mapping = CaseMapping::Rfc1459;
                }
            } else if let Some(value) = token.strip_prefix("PREFIX=") {
                // PREFIX=(modes)symbols - keep the mode letters too, so MODE
                // parsing knows which modes take a nick parameter.
                if let Some(rest) = value.strip_prefix('(')
                    && let Some((letters, symbols)) = rest.split_once(')')
                    && !symbols.is_empty()
                    && letters.chars().count() == symbols.chars().count()
                {
                    self.network_support.prefix_modes = letters.to_string();
                    self.network_support.user_prefixes = symbols.to_string();
                }
            } else if let Some(value) = token.strip_prefix("CHANMODES=") {
                // CHANMODES=A,B,C,D - which channel modes consume a parameter.
                let mut groups = value.split(',');
                if let (Some(a), Some(b), Some(c), Some(_d)) =
                    (groups.next(), groups.next(), groups.next(), groups.next())
                {
                    self.network_support.chanmodes_a = a.to_string();
                    self.network_support.chanmodes_b = b.to_string();
                    self.network_support.chanmodes_c = c.to_string();
                }
            } else if let Some(value) = token.strip_prefix("STATUSMSG=") {
                self.network_support.status_prefixes = value.to_string();
            } else if let Some(value) = token.strip_prefix("CHATHISTORY=") {
                if let Ok(limit) = value.parse::<usize>()
                    && limit > 0
                {
                    self.network_support.history_limit = Some(limit);
                    self.chathistory_limit = Some(limit);
                }
            } else if let Some(value) = token.strip_prefix("MSGREFTYPES=") {
                self.network_support.history_reference_types = value.to_string();
            } else if let Some(value) = token.strip_prefix("evilnet/CHATHISTORYRETENTION=") {
                self.network_support.history_retention_secs = value.parse().ok();
            } else if let Some(value) = token.strip_prefix("NICKLEN=") {
                self.network_support.nick_len = value.parse().ok();
            } else if let Some(value) = token.strip_prefix("CHANNELLEN=") {
                self.network_support.channel_len = value.parse().ok();
            } else if let Some(value) = token.strip_prefix("TOPICLEN=") {
                self.network_support.topic_len = value.parse().ok();
            } else if let Some(value) = token.strip_prefix("AWAYLEN=") {
                self.network_support.away_len = value.parse().ok();
            } else if let Some(value) = token.strip_prefix("KICKLEN=") {
                self.network_support.kick_len = value.parse().ok();
            } else if let Some(value) = token.strip_prefix("MONITOR=") {
                self.network_support.monitor_limit = value.parse().ok();
            } else if token.eq_ignore_ascii_case("UTF8ONLY") {
                self.network_support.utf8_only = true;
            } else if token.eq_ignore_ascii_case("draft/ACCOUNTREQUIRED") {
                self.network_support.account_required = true;
            }
        }
        if self.network_support.case_mapping != previous_case_mapping {
            self.refresh_case_mapping_state();
        }
    }

    fn handle_mode_message(
        &mut self,
        sender: &str,
        target: &str,
        mode: Option<&str>,
        params: &[String],
    ) {
        let Some(mode) = mode else {
            return;
        };
        let is_channel = self.is_channel_name(target);
        let mut param_idx = 0usize;
        let mut sign = '+';
        let max = self.max_scrollback;
        let timestamp_format = self.timestamp_format.clone();
        // Which modes consume a parameter comes from ISUPPORT (PREFIX mode
        // letters + CHANMODES groups); hardcoded sets misalign parameters on
        // networks with extra parameterized modes (e.g. `MODE #c +fo [4:5] nick`
        // would read "[4:5]" as the nick to op).
        let ns = self.network_support.clone();

        if is_channel {
            if let Some(channel) = self.channel_mut(target) {
                for c in mode.chars() {
                    match c {
                        '+' | '-' => sign = c,
                        // Prefix (user) modes: always take a nick parameter.
                        c if ns.prefix_modes.contains(c) => {
                            if let Some(nick) = params.get(param_idx) {
                                param_idx += 1;
                                if let Some(user_mode) = ns.user_mode_for_letter(c) {
                                    if sign == '+' {
                                        channel.add_user_mode(nick, user_mode);
                                    } else {
                                        channel.remove_user_mode(nick, user_mode);
                                    }
                                }
                            }
                        }
                        // Type A list modes (bans etc.): parameter in both
                        // directions, tracked through their numeric list replies
                        // rather than as persistent channel modes.
                        c if ns.chanmodes_a.contains(c) => {
                            if params.get(param_idx).is_some() {
                                param_idx += 1;
                            }
                        }
                        // Type B: parameter in both directions.
                        c if ns.chanmodes_b.contains(c) => {
                            let parameter = params.get(param_idx).map(String::as_str);
                            if parameter.is_some() {
                                param_idx += 1;
                            }
                            channel.apply_channel_mode(sign, c, parameter);
                        }
                        // Type C: parameter only when set.
                        c if ns.chanmodes_c.contains(c) => {
                            let parameter = if sign == '+' {
                                let value = params.get(param_idx).map(String::as_str);
                                if value.is_some() {
                                    param_idx += 1;
                                }
                                value
                            } else {
                                None
                            };
                            channel.apply_channel_mode(sign, c, parameter);
                        }
                        // Type D (and unknown): never takes a parameter.
                        other => {
                            channel.apply_channel_mode_flag(sign, other);
                        }
                    }
                }
                let rendered = if params.is_empty() {
                    mode.to_string()
                } else {
                    format!("{} {}", mode, params.join(" "))
                };
                channel.push_trimmed(
                    ChatMessage::system_fmt(
                        &format!("{} set mode {}", sender, rendered),
                        &timestamp_format,
                    ),
                    max,
                );
            }
        } else if self.identifiers_equal(target, &self.my_nick) {
            let rendered = if params.is_empty() {
                mode.to_string()
            } else {
                format!("{} {}", mode, params.join(" "))
            };
            self.add_server_message(ChatMessage::system_fmt(
                &format!("{} set user mode {}", sender, rendered),
                &self.timestamp_format,
            ));
        }
    }

    fn handle_numeric(&mut self, num: u16, params: &[String]) {
        match num {
            RPL_WELCOME => {
                self.connected = true;
                self.connecting = false;
                self.reset_reconnect_state(); // Reset reconnect attempts on successful connection
                self.connection_intent = ConnectionIntent::None;
                self.nick_retry_attempts = 0;
                // Clear any lag reading left over from a previous connection.
                self.lag_ms = None;
                if let Some(nick) = params.first() {
                    self.set_my_nick(nick.clone());
                }
                let msg = params
                    .get(1)
                    .cloned()
                    .unwrap_or_else(|| "Welcome!".to_string());
                self.add_server_message(ChatMessage::system(&msg));

                let (set_invisible, auto_join, auto_perform) = self.current_session_automation();

                // Set invisible mode if requested for this session snapshot.
                if set_invisible {
                    self.send_command(IrcCommand::Mode(
                        self.my_nick.clone(),
                        Some("+i".to_string()),
                        Vec::new(),
                    ));
                    self.add_server_message(ChatMessage::system(
                        "Setting user mode +i (invisible)",
                    ));
                }

                // Auto-join channels
                if !auto_join.is_empty() {
                    for chan in auto_join.split(',') {
                        let chan = chan.trim();
                        if !chan.is_empty() {
                            let channel = self.normalize_channel_name(chan);
                            self.send_command(IrcCommand::Join(channel.clone(), None, None, None));
                            self.add_server_message(ChatMessage::system(&format!(
                                "Auto-joining {}",
                                channel
                            )));
                        }
                    }
                }

                // Nefarious suppresses its legacy bouncer replay for clients
                // that negotiated CHATHISTORY. Channel tabs catch up after
                // their JOIN acknowledgement; retained query tabs have no
                // JOIN event, so request their missed messages here.
                if self.chathistory_enabled && self.logging_load_history {
                    let query_targets: Vec<String> = self
                        .channels
                        .iter()
                        .filter(|(target, channel)| {
                            !self.is_channel_name(target)
                                && channel
                                    .messages
                                    .iter()
                                    .rev()
                                    .any(|message| Self::message_reference(message).is_some())
                        })
                        .map(|(target, _)| target.clone())
                        .collect();
                    for target in query_targets {
                        self.request_latest_history(&target, None, HistoryPlacement::Append);
                    }
                }

                // Queue auto-perform commands for execution
                if !auto_perform.is_empty() {
                    let commands: Vec<String> = auto_perform
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(|line| line.to_string())
                        .collect();
                    if !commands.is_empty() {
                        self.pending_auto_perform = Some(commands);
                        self.add_server_message(ChatMessage::system(
                            "Running auto-perform commands...",
                        ));
                    }
                }

                // Initial lag check
                self.ping_sent_time = Some(std::time::Instant::now());
                self.send_command(IrcCommand::Ping("LAG".to_string()));
            }

            RPL_ISUPPORT => {
                self.apply_isupport_tokens(params);
            }

            RPL_TOPIC => {
                let max = self.max_scrollback;
                if let (Some(channel), Some(topic)) = (params.get(1), params.get(2))
                    && let Some(ch) = self.channel_mut(channel)
                {
                    ch.topic = Some(topic.clone());
                    ch.push_trimmed(ChatMessage::system(&format!("Topic: {}", topic)), max);
                }
            }

            RPL_CHANNELMODEIS => {
                // Format: 324 <nick> <channel> <modes> [<mode params>...]
                let support = self.network_support.clone();
                if let (Some(channel), Some(modes)) = (params.get(1), params.get(2))
                    && let Some(ch) = self.channel_mut(channel)
                {
                    ch.set_channel_modes_from_reply(modes, &params[3..], &support);
                }
            }

            RPL_CREATIONTIME => {
                // Format: 329 <nick> <channel> <timestamp>
                if let (Some(channel), Some(ts_str)) = (params.get(1), params.get(2)) {
                    let ts: u64 = ts_str.parse().unwrap_or(0);
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.created = Some(ts);
                    }
                }
            }

            RPL_TOPICWHOTIME => {
                // Format: 333 <nick> <channel> <setter> <timestamp>
                if let (Some(channel), Some(setter), Some(ts_str)) =
                    (params.get(1), params.get(2), params.get(3))
                {
                    let ts: u64 = ts_str.parse().unwrap_or(0);

                    // Store topic setter info
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.topic_set_by = Some(setter.clone());
                        ch.topic_set_time = Some(ts);
                    }

                    let time_str = format_timestamp(ts);
                    let max = self.max_scrollback;
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.push_trimmed(
                            ChatMessage::system(&format!(
                                "Topic set by {} on {}",
                                setter, time_str
                            )),
                            max,
                        );
                    }
                }
            }

            RPL_NAMREPLY => {
                if let Some(channel) = params.get(2)
                    && let Some(names) = params.get(3)
                {
                    let support = self.network_support.clone();
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.begin_user_burst();
                        for name in names.split_whitespace() {
                            // `userhost-in-names` appends `!user@host` after the
                            // nickname. Membership identity remains the nick;
                            // WHO supplies richer user state separately.
                            let nick_token = name.split_once('!').map_or(name, |(nick, _)| nick);
                            let (nick, modes) = support.parse_prefixed_nick(nick_token);
                            ch.add_user_burst_modes(nick, &modes);
                        }
                    }
                }
            }

            RPL_ENDOFNAMES => {
                if let Some(channel) = params.get(1) {
                    // NAMES burst complete: dedup and sort the user list once.
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.finish_user_burst();
                    }
                    // After getting the user list, send WHO to get away status
                    if self.is_channel_name(channel) {
                        self.send_command(IrcCommand::Who(channel.clone()));
                    }
                }
            }

            RPL_BANLIST => {
                // Format: 367 <nick> <channel> <banmask> <setter> <timestamp>
                if let (Some(channel), Some(mask), Some(setter), Some(ts_str)) =
                    (params.get(1), params.get(2), params.get(3), params.get(4))
                {
                    let ts: u64 = ts_str.parse().unwrap_or(0);
                    if let Some(ch) = self.channel_mut(channel) {
                        // Overlapping MODE +b requests can list the same mask
                        // twice; a re-listed ban replaces its earlier entry
                        // instead of accumulating duplicates.
                        match ch
                            .bans
                            .iter()
                            .position(|b| b.mask.eq_ignore_ascii_case(mask))
                        {
                            Some(slot) => {
                                let existing = &mut ch.bans[slot];
                                existing.set_by = setter.clone();
                                existing.set_time = ts;
                            }
                            None => {
                                ch.bans.push(BanEntry {
                                    mask: mask.clone(),
                                    set_by: setter.clone(),
                                    set_time: ts,
                                });
                            }
                        }
                    }
                }
            }

            RPL_ENDOFBANLIST => {
                // Format: 368 <nick> <channel> :End of channel ban list
                if let Some(channel) = params.get(1)
                    && let Some(ch) = self.channel_mut(channel)
                {
                    ch.ban_list_complete = true;
                }
            }

            RPL_MOTD | RPL_MOTDSTART | RPL_ENDOFMOTD => {
                if let Some(text) = params.last() {
                    self.add_server_message(ChatMessage::system(text));
                }
            }

            ERR_NICKNAMEINUSE => {
                // After registration this is a failed /nick attempt: report it and
                // keep our current nick. Auto-retrying here would silently rename
                // the user to "<current>_" and desync my_nick from the server.
                if self.connected {
                    let rejected = params.get(1).cloned().unwrap_or_default();
                    let text = params
                        .last()
                        .cloned()
                        .unwrap_or_else(|| "Nickname is already in use".to_string());
                    self.add_server_message(ChatMessage::system_fmt(
                        &format!("{}: {}", rejected, text),
                        &self.timestamp_format,
                    ));
                    return;
                }
                if self.nick_retry_attempts >= 3 {
                    let text = params
                        .last()
                        .cloned()
                        .unwrap_or_else(|| "Nickname is already in use".to_string());
                    self.fail_registration(format!("Registration failed: {}", text));
                    return;
                }
                self.nick_retry_attempts = self.nick_retry_attempts.saturating_add(1);
                let new_nick = format!("{}_", self.my_nick);
                self.set_my_nick(new_nick.clone());
                if let Some(tx) = &self.cmd_tx {
                    let _ = tx.send(IrcCommand::Nick(new_nick));
                }
                self.add_server_message(ChatMessage::system(
                    "Nickname in use, trying alternative...",
                ));
            }

            ERR_NONICKNAMEGIVEN | ERR_ERRONEUSNICKNAME | ERR_NICKCOLLISION | ERR_PASSWDMISMATCH
            | ERR_YOUREBANNEDCREEP => {
                let text = params
                    .last()
                    .cloned()
                    .unwrap_or_else(|| format!("registration error {}", num));
                if !self.connected {
                    self.fail_registration(format!("Registration failed: {}", text));
                } else {
                    self.add_server_message(ChatMessage::system_fmt(
                        &format!("[{}] {}", num, text),
                        &self.timestamp_format,
                    ));
                }
            }

            RPL_LISTSTART => {
                // Clear old list, start collecting
                self.channel_list.clear();
                self.channel_list_dirty = true;
                self.channel_list_loading = true;
                self.show_channel_list = true;
            }

            RPL_LIST => {
                // Bound the in-memory channel list so an enormous (or malicious)
                // LIST reply cannot grow it without limit.
                const MAX_LIST_ENTRIES: usize = 50_000;
                if self.channel_list.len() >= MAX_LIST_ENTRIES {
                    return;
                }

                let channel_idx = params
                    .iter()
                    .position(|p| p.starts_with('#') || p.starts_with('&'));

                let parsed = channel_idx.and_then(|ci| {
                    if ci + 1 >= params.len() {
                        return None;
                    }
                    fn parse_count(p: &str) -> Option<usize> {
                        let clean = p.replace(',', "");
                        if !clean.is_empty()
                            && clean.chars().all(|c| c.is_ascii_digit() || c == '+')
                        {
                            clean.trim_start_matches('+').parse().ok()
                        } else {
                            None
                        }
                    }

                    let last = params.last()?;
                    let (user_count, topic) = if ci + 1 == params.len() - 1 {
                        // One parameter after the channel: per the standard
                        // format it is the count ("322 me #chan 42"); only a
                        // non-numeric value there means the server skipped the
                        // count and sent the topic directly.
                        match parse_count(last) {
                            Some(count) => (count, String::new()),
                            None => (0, last.clone()),
                        }
                    } else {
                        // Standard form: counts sit between the channel name
                        // and the trailing topic. The trailing parameter is the
                        // topic even when purely numeric ("322 me #a 12 :2024").
                        let count = params[ci + 1..params.len() - 1]
                            .iter()
                            .find_map(|p| parse_count(p))
                            .unwrap_or(0);
                        (count, last.clone())
                    };
                    Some((params[ci].clone(), user_count, topic))
                });

                match parsed {
                    Some((name, user_count, topic)) => {
                        self.channel_list
                            .push(ChannelListEntry::new(name, user_count, topic));
                        self.channel_list_dirty = true;
                    }
                    None => {
                        // Fallback: if we have at least 3 params, assume standard format
                        // [client, channel, count, topic]
                        if params.len() >= 3 {
                            self.channel_list.push(ChannelListEntry::new(
                                params[1].clone(),
                                params[2].replace(',', "").parse().unwrap_or(0),
                                params.get(3).cloned().unwrap_or_default(),
                            ));
                            self.channel_list_dirty = true;
                        }
                    }
                }
            }

            RPL_LISTEND => {
                self.channel_list_loading = false;
                // Force a final view refresh in case the streaming throttle
                // skipped the last appends.
                self.channel_list_dirty = true;
            }

            RPL_AWAY => {
                // <nick> :<away message> - shown during WHOIS or when messaging away user
                if let (Some(nick), Some(message)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is away: {}",
                        nick, message
                    )));
                }
            }

            RPL_UNAWAY => {
                if let Some(text) = params.get(1) {
                    self.add_server_message(ChatMessage::system(text));
                }
            }

            RPL_NOWAWAY => {
                if let Some(text) = params.get(1) {
                    self.add_server_message(ChatMessage::system(text));
                }
            }

            // WHOIS responses
            RPL_WHOISUSER => {
                // <nick> <user> <host> * :<realname>
                if let (Some(nick), Some(user), Some(host)) =
                    (params.get(1), params.get(2), params.get(3))
                {
                    let realname = params.get(5).cloned().unwrap_or_default();
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} ({}@{}) - {}",
                        nick, user, host, realname
                    )));
                }
            }

            RPL_WHOISSERVER => {
                // <nick> <server> :<serverinfo>
                if let (Some(nick), Some(server)) = (params.get(1), params.get(2)) {
                    let info = params.get(3).cloned().unwrap_or_default();
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is on server {} ({})",
                        nick, server, info
                    )));
                }
            }

            RPL_WHOISOPERATOR => {
                if let Some(nick) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is an IRC operator",
                        nick
                    )));
                }
            }

            RPL_WHOISIDLE => {
                // <nick> <seconds> <signon> :seconds idle, signon time
                if let (Some(nick), Some(idle_secs)) = (params.get(1), params.get(2)) {
                    let idle: u64 = idle_secs.parse().unwrap_or(0);
                    let idle_str = if idle >= 3600 {
                        format!("{}h {}m", idle / 3600, (idle % 3600) / 60)
                    } else if idle >= 60 {
                        format!("{}m {}s", idle / 60, idle % 60)
                    } else {
                        format!("{}s", idle)
                    };
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} has been idle for {}",
                        nick, idle_str
                    )));
                }
            }

            RPL_ENDOFWHOIS => {
                if let Some(nick) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] End of WHOIS for {}",
                        nick
                    )));
                }
            }

            RPL_WHOISCHANNELS => {
                if let (Some(nick), Some(channels)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is on: {}",
                        nick, channels
                    )));
                }
            }

            RPL_WHOISACCOUNT => {
                if let (Some(nick), Some(account)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is logged in as {}",
                        nick, account
                    )));
                }
            }

            RPL_WHOISACTUALLY => {
                if let (Some(nick), Some(host)) = (params.get(1), params.get(2)) {
                    let ip = params.get(3).cloned().unwrap_or_default();
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is actually using host {} ({})",
                        nick, host, ip
                    )));
                }
            }

            RPL_WHOISMARKS => {
                // Format: 339 <me> <nick> :is marked: <mark>
                if let (Some(nick), Some(text)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} {}",
                        nick, text
                    )));
                }
            }

            RPL_WHOISSECURE => {
                if let Some(nick) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is using a secure connection",
                        nick
                    )));
                }
            }

            RPL_WHOISSPECIAL => {
                // <nick> :<special info> - used by some networks for custom titles/info
                if let (Some(nick), Some(info)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} - {}",
                        nick, info
                    )));
                }
            }

            RPL_WHOISCERTFP => {
                // <nick> :has client certificate fingerprint <fingerprint>
                if let (Some(nick), Some(fp)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} has TLS fingerprint: {}",
                        nick, fp
                    )));
                }
            }

            RPL_WHOISREGNICK => {
                // <nick> :is a registered nick
                if let Some(nick) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is a registered nick",
                        nick
                    )));
                }
            }

            RPL_WHOISBOT => {
                // <nick> :is a Bot
                if let Some(nick) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is a bot",
                        nick
                    )));
                }
            }

            RPL_WHOISHOST => {
                // <nick> :is connecting from <host> <ip>
                if let (Some(nick), Some(info)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} - {}",
                        nick, info
                    )));
                }
            }

            RPL_WHOISMODES => {
                // <nick> :is using modes <modes>
                if let (Some(nick), Some(modes)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is using modes {}",
                        nick, modes
                    )));
                }
            }

            // WHO responses
            RPL_WHOREPLY => {
                // <channel> <user> <host> <server> <nick> <H|G>[*][@|+] :<hopcount> <realname>
                if let (
                    Some(channel),
                    Some(_user),
                    Some(_host),
                    Some(_server),
                    Some(nick),
                    Some(flags),
                ) = (
                    params.get(1),
                    params.get(2),
                    params.get(3),
                    params.get(4),
                    params.get(5),
                    params.get(6),
                ) {
                    // Update away status based on H (Here) or G (Gone/away) flag
                    let is_away = flags.starts_with('G');
                    if let Some(ch) = self.channel_mut(channel) {
                        if is_away {
                            // Mark user as away (we don't have the away message from WHO)
                            ch.set_user_away(nick, Some("Away".to_string()));
                        } else {
                            ch.set_user_away(nick, None);
                        }
                    }
                }
            }

            RPL_ENDOFWHO => {
                // Silently handled - WHO is automatically sent to get away status
            }

            // Error numerics
            ERR_NOSUCHNICK => {
                if let Some(nick) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "No such nick/channel: {}",
                        nick
                    )));
                }
            }

            ERR_NOSUCHCHANNEL => {
                if let Some(channel) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "No such channel: {}",
                        channel
                    )));
                }
            }

            ERR_CANNOTSENDTOCHAN => {
                if let Some(channel) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Cannot send to channel: {}",
                        channel
                    )));
                }
            }

            ERR_NOTONCHANNEL => {
                if let Some(channel) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "You're not on that channel: {}",
                        channel
                    )));
                }
            }

            ERR_INVITEONLYCHAN => {
                if let Some(channel) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Cannot join {} (invite only)",
                        channel
                    )));
                }
            }

            ERR_BANNEDFROMCHAN => {
                if let Some(channel) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Cannot join {} (banned)",
                        channel
                    )));
                }
            }

            ERR_BADCHANNELKEY => {
                if let Some(channel) = params.get(1) {
                    // Remove any pending key since it was wrong
                    self.pending_channel_keys
                        .remove(&self.network_support.canonicalize(channel));
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Cannot join {} (bad or missing channel key). Use: /join {} <key>",
                        channel, channel
                    )));
                }
            }

            // Quiet list (mode +q)
            RPL_QUIETLIST => {
                // <channel> <mode> <mask> <setter> <timestamp>
                if let (Some(channel), Some(mask)) = (params.get(1), params.get(3)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[QUIET] {} - {}",
                        channel, mask
                    )));
                }
            }

            RPL_ENDOFQUIETLIST => {
                if let Some(channel) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[QUIET] End of quiet list for {}",
                        channel
                    )));
                }
            }

            // IRCv3 Monitor
            RPL_MONONLINE => {
                // :server 730 <nick> :target1,target2,...
                if let Some(targets) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[MONITOR] Online: {}",
                        targets
                    )));
                }
            }

            RPL_MONOFFLINE => {
                // :server 731 <nick> :target1,target2,...
                if let Some(targets) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[MONITOR] Offline: {}",
                        targets
                    )));
                }
            }

            RPL_MONLIST => {
                // :server 732 <nick> :target1,target2,...
                if let Some(targets) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[MONITOR] List: {}",
                        targets
                    )));
                }
            }

            RPL_ENDOFMONLIST => {
                self.add_message_to_current(ChatMessage::system("[MONITOR] End of monitor list"));
            }

            ERR_MONLISTFULL => {
                if let Some(limit) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[MONITOR] List is full (limit: {})",
                        limit
                    )));
                }
            }

            RPL_WHOISKEYVALUE
            | RPL_KEYVALUE
            | RPL_METADATAEND
            | RPL_KEYNOTSET
            | RPL_METADATASUBOK
            | RPL_METADATAUNSUBOK
            | RPL_METADATASUBS
            | RPL_METADATASYNCLATER => {
                let text = params.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
                self.add_server_message(ChatMessage::system_fmt(
                    &format!("[METADATA {num}] {text}"),
                    &self.timestamp_format,
                ));
            }

            RPL_BOUNCERSESSION | RPL_BOUNCETOKEN | RPL_BOUNCERSETTINGS => {
                let text = params.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
                self.add_server_message(ChatMessage::system_fmt(
                    &format!("[BOUNCER {num}] {text}"),
                    &self.timestamp_format,
                ));
            }

            _ => {
                // Show other numerics in current window for visibility
                let text = params.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
                if !text.is_empty() {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[{}] {}",
                        num, text
                    )));
                }
            }
        }
    }

    fn add_message_to_channel(&mut self, channel: &str, mut msg: ChatMessage) {
        let existing_key = self.channel_key(channel);
        let key = existing_key.clone().unwrap_or_else(|| channel.to_string());

        // Redact at the shared display/log boundary as defense in depth. This
        // covers ordinary query input, /say, service shortcuts, server echo-
        // message, and future command paths that might otherwise forget to use
        // outgoing_local_echo().
        if self.identifiers_equal(&msg.sender, &self.my_nick)
            && commands::service_message_contains_credentials(&key, &msg.content)
        {
            msg.content = "<credential command sent>".to_string();
            msg.no_log = true;
            msg.render_cache = Default::default();
        }

        let is_new = existing_key.is_none();
        if is_new {
            // Cap auto-created query (non-channel) windows so a flood of messages
            // from many distinct senders cannot grow `channels` without bound.
            // Real channels you joined are exempt; over the cap we fall back to the
            // server buffer instead of opening a new window.
            if !self.is_channel_name(channel) {
                const MAX_QUERY_WINDOWS: usize = 200;
                let query_count = self
                    .channels
                    .keys()
                    .filter(|k| !self.is_channel_name(k))
                    .count();
                if query_count >= MAX_QUERY_WINDOWS {
                    self.add_server_message(msg);
                    return;
                }
            }
            let mut new_channel = Channel::new();
            new_channel.set_case_mapping(self.network_support.case_mapping);
            // Load chat history for new query windows (non-channels)
            if self.logging_load_history
                && !self.is_channel_name(&key)
                && let Some((network, legacy_host, allow_legacy)) = self.active_log_context()
            {
                let history = self.log_manager.load_history_with_legacy(
                    &network,
                    &legacy_host,
                    &key,
                    self.logging_history_lines,
                    allow_legacy,
                );
                if !history.is_empty() {
                    new_channel.messages.push_back(ChatMessage::system(&format!(
                        "--- {} lines of history loaded ---",
                        history.len()
                    )));
                    new_channel.messages.extend(history);
                }
                self.log_manager.log_session_start(&network, &key);
            }
            self.channels.insert(key.clone(), new_channel);
        }
        if msg.msgid.as_ref().is_some_and(|msgid| {
            self.channels.get(&key).is_some_and(|channel| {
                channel
                    .messages
                    .iter()
                    .any(|existing| existing.msgid.as_ref() == Some(msgid))
            })
        }) {
            return;
        }
        // Log message to disk (display-only messages like /lastlog output are
        // excluded so search results don't pollute the persistent history).
        // Always use the display-preserving resolved key: CASEMAPPING-equivalent
        // spellings must never fork one tab into multiple log files.
        if !msg.no_log
            && let Some((network, _, _)) = self.active_log_context()
        {
            self.log_manager.log_message(&network, &key, &msg);
        }

        let is_current = self.current_target_is(&key);
        if let Some(ch) = self.channels.get_mut(&key) {
            ch.push_trimmed(msg, self.max_scrollback);
            if !is_current {
                ch.unread += 1;
            }
        }
        if is_current && self.window_focused {
            self.mark_target_read(&key);
        }
    }

    pub fn add_server_message(&mut self, msg: ChatMessage) {
        self.server_messages.push_back(msg);
        // Limit scrollback
        while self.server_messages.len() > self.max_scrollback {
            self.server_messages.pop_front();
        }
        // Increment unread if not viewing server buffer
        if self.current_channel.is_some() {
            self.server_unread += 1;
        }
    }

    /// Update a user's away status in all channels they're in
    pub fn update_user_away_status(&mut self, nick: &str, away_msg: Option<String>) {
        for channel in self.channels.values_mut() {
            channel.set_user_away(nick, away_msg.clone());
        }
    }

    /// Add a message to the current window (channel or server buffer)
    fn add_message_to_current(&mut self, msg: ChatMessage) {
        if let Some(channel_name) = &self.current_channel.clone() {
            self.add_message_to_channel(channel_name, msg);
        } else {
            self.add_server_message(msg);
        }
    }

    fn command_fits_irc_line(cmd: &IrcCommand) -> bool {
        let line = format!("{}", cmd);
        line.len() <= IRC_MAX_LINE_BYTES
            && !line.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | 0))
    }

    fn split_irc_text_payload(
        command: &str,
        target: &str,
        text: &str,
        wrapper_prefix: &str,
        wrapper_suffix: &str,
    ) -> Option<Vec<String>> {
        let overhead =
            format!("{} {} :", command, target).len() + wrapper_prefix.len() + wrapper_suffix.len();
        if overhead >= IRC_MAX_LINE_BYTES {
            return None;
        }
        let max_payload_bytes = IRC_MAX_LINE_BYTES - overhead;
        if text.len() <= max_payload_bytes {
            return Some(vec![format!(
                "{}{}{}",
                wrapper_prefix, text, wrapper_suffix
            )]);
        }

        let mut chunks = Vec::new();
        let mut remaining = text;
        while !remaining.is_empty() {
            let mut end = remaining.len().min(max_payload_bytes);
            while end > 0 && !remaining.is_char_boundary(end) {
                end -= 1;
            }
            if end == 0 {
                return None;
            }
            chunks.push(format!(
                "{}{}{}",
                wrapper_prefix,
                &remaining[..end],
                wrapper_suffix
            ));
            remaining = &remaining[end..];
        }
        Some(chunks)
    }

    fn send_text_command(
        &mut self,
        command: &str,
        target: &str,
        text: &str,
        wrapper_prefix: &str,
        wrapper_suffix: &str,
    ) -> bool {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut chunks = Vec::new();
        for logical_line in normalized.split('\n') {
            let Some(mut line_chunks) = Self::split_irc_text_payload(
                command,
                target,
                logical_line,
                wrapper_prefix,
                wrapper_suffix,
            ) else {
                self.add_message_to_current(ChatMessage::system_fmt(
                    "Message not sent - target name leaves no room for text",
                    &self.timestamp_format,
                ));
                return false;
            };
            chunks.append(&mut line_chunks);
        }

        let full_text = format!("{wrapper_prefix}{normalized}{wrapper_suffix}");
        let use_multiline = self.enabled_caps.contains("draft/multiline")
            && (normalized.contains('\n') || chunks.len() > 1)
            && self
                .network_support
                .multiline_max_bytes
                .is_none_or(|max| full_text.len() <= max);
        if !use_multiline {
            return chunks.into_iter().all(|chunk| match command {
                "NOTICE" => self.send_command(IrcCommand::Notice(target.to_string(), chunk)),
                _ => self.send_command(IrcCommand::Privmsg(target.to_string(), chunk)),
            });
        }

        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_BATCH: AtomicU64 = AtomicU64::new(1);
        let batch_id = format!("lf{}", NEXT_BATCH.fetch_add(1, Ordering::Relaxed));
        let concat_overhead =
            format!("@batch={batch_id};draft/multiline-concat {command} {target} :").len();
        if concat_overhead >= IRC_MAX_LINE_BYTES {
            return false;
        }
        let max_payload = IRC_MAX_LINE_BYTES - concat_overhead;
        let mut lines: Vec<(String, bool)> = Vec::new();
        for logical_line in full_text.split('\n') {
            if logical_line.is_empty() {
                lines.push((String::new(), false));
                continue;
            }
            let mut rest = logical_line;
            let mut first = true;
            while !rest.is_empty() {
                let mut end = rest.len().min(max_payload);
                while end > 0 && !rest.is_char_boundary(end) {
                    end -= 1;
                }
                if end == 0 {
                    return false;
                }
                lines.push((rest[..end].to_string(), !first));
                first = false;
                rest = &rest[end..];
            }
        }
        if self
            .network_support
            .multiline_max_lines
            .is_some_and(|max| lines.len() > max)
        {
            return chunks.into_iter().all(|chunk| match command {
                "NOTICE" => self.send_command(IrcCommand::Notice(target.to_string(), chunk)),
                _ => self.send_command(IrcCommand::Privmsg(target.to_string(), chunk)),
            });
        }

        if !self.send_command(IrcCommand::Batch(
            format!("+{batch_id}"),
            Some("draft/multiline".to_string()),
            Some(target.to_string()),
        )) {
            return false;
        }
        let mut success = true;
        for (line, concat) in lines {
            let concat_tag = if concat {
                ";draft/multiline-concat"
            } else {
                ""
            };
            success &= self.send_command(IrcCommand::Raw(format!(
                "@batch={batch_id}{concat_tag} {command} {target} :{line}"
            )));
        }
        success &= self.send_command(IrcCommand::Batch(format!("-{batch_id}"), None, None));
        success
    }

    pub fn send_privmsg_text(&mut self, target: &str, text: &str) -> bool {
        if !self.can_send_to_target(target) {
            return false;
        }
        self.send_text_command("PRIVMSG", target, text, "", "")
    }

    pub fn send_notice_text(&mut self, target: &str, text: &str) -> bool {
        if !self.can_send_to_target(target) {
            return false;
        }
        self.send_text_command("NOTICE", target, text, "", "")
    }

    pub fn send_action_text(&mut self, target: &str, text: &str) -> bool {
        if !self.can_send_to_target(target) {
            return false;
        }
        self.send_text_command("PRIVMSG", target, text, "\x01ACTION ", "\x01")
    }

    pub fn send_command(&mut self, cmd: IrcCommand) -> bool {
        if !Self::command_fits_irc_line(&cmd) {
            tracing::warn!("Outgoing command not sent (line too long): {:?}", cmd);
            self.add_message_to_current(ChatMessage::system_fmt(
                "Command not sent - IRC line is too long",
                &self.timestamp_format,
            ));
            return false;
        }
        if let Some(tx) = &self.cmd_tx {
            if let Err(e) = tx.send(cmd) {
                tracing::warn!("Outgoing command not sent (disconnected): {:?}", e);
                return false;
            }
            true
        } else {
            false
        }
    }

    pub fn process_input(&mut self) {
        let input = self.input_text.trim().to_string();
        if input.is_empty() {
            return;
        }

        // Credentials must never enter the Up/Down command history. Besides
        // explicit /msg and /query forms, account for ordinary input and /say
        // while the active query is NickServ/AuthServ.
        let contains_credentials = commands::command_line_contains_credentials(&input)
            || self.current_channel.as_deref().is_some_and(|target| {
                let candidate = if let Some(command_line) = input.strip_prefix('/') {
                    let mut parts = command_line.splitn(2, char::is_whitespace);
                    let command = parts.next().unwrap_or("");
                    if command.eq_ignore_ascii_case("SAY") {
                        parts.next().unwrap_or("").trim_start()
                    } else {
                        ""
                    }
                } else {
                    input.as_str()
                };
                !candidate.is_empty()
                    && commands::service_message_contains_credentials(target, candidate)
            });

        // Save non-sensitive input to history (avoid duplicates of last entry).
        if !contains_credentials && self.command_history.last() != Some(&input) {
            self.command_history.push(input.clone());
            if self.command_history.len() > 100 {
                self.command_history.remove(0);
            }
        }

        // Reset history browsing state
        self.history_index = None;
        self.history_temp.clear();

        self.input_text.clear();

        if input.starts_with('/') {
            self.process_command(&input);
        } else if let Some(channel) = &self.current_channel.clone() {
            // Don't echo a message we can't actually send: without a live
            // connection the command channel is gone and the line would be lost.
            if !self.connected || self.cmd_tx.is_none() {
                self.add_message_to_current(ChatMessage::system(
                    "Not connected - message not sent",
                ));
                return;
            }

            if !self.can_send_to_target(channel) {
                self.add_message_to_current(ChatMessage::system_fmt(
                    "You are not joined to this channel - message not sent",
                    &self.timestamp_format,
                ));
                return;
            }

            // Auto-back: clear away status when user sends a message
            if self.away_status.is_some() {
                let _ = self.send_command(IrcCommand::Away(None));
                self.away_status = None;
                self.auto_away_triggered = false;
                // Update our own status in all channels
                let my_nick = self.my_nick.clone();
                self.update_user_away_status(&my_nick, None);
                self.add_server_message(ChatMessage::system(
                    "You are no longer marked as away (auto-back)",
                ));
            }

            // Send message to current channel
            if self.send_privmsg_text(channel, &input) {
                let msg = ChatMessage::new_fmt(&self.my_nick, &input, &self.timestamp_format);
                self.add_message_to_channel(channel, msg);
            } else {
                self.add_message_to_current(ChatMessage::system_fmt(
                    "Message not sent",
                    &self.timestamp_format,
                ));
            }
        }
    }

    fn history_up(&mut self) {
        if self.command_history.is_empty() {
            return;
        }

        match self.history_index {
            None => {
                // Start browsing - save current input
                self.history_temp = self.input_text.clone();
                self.history_index = Some(self.command_history.len() - 1);
                self.input_text = self.command_history.last().unwrap().clone();
            }
            Some(i) if i > 0 => {
                self.history_index = Some(i - 1);
                self.input_text = self.command_history[i - 1].clone();
            }
            _ => {} // At beginning, do nothing
        }
    }

    fn history_down(&mut self) {
        if let Some(i) = self.history_index {
            if i + 1 < self.command_history.len() {
                self.history_index = Some(i + 1);
                self.input_text = self.command_history[i + 1].clone();
            } else {
                // Past end of history - restore temp
                self.history_index = None;
                self.input_text = self.history_temp.clone();
                self.history_temp.clear();
            }
        }
    }

    fn find_nick_completions(&self, prefix: &str) -> Vec<String> {
        if let Some(channel_name) = &self.current_channel
            && let Some(channel) = self.channels.get(channel_name)
        {
            let prefix_lower = prefix.to_lowercase();
            let mut matches: Vec<String> = channel
                .users
                .iter()
                .filter(|user| user.nick.to_lowercase().starts_with(&prefix_lower))
                .map(|user| user.nick.clone())
                .collect();
            matches.sort_by_key(|a| a.to_lowercase());
            return matches;
        }
        Vec::new()
    }

    fn find_command_completions(prefix: &str) -> Vec<String> {
        let prefix = prefix.to_ascii_lowercase();
        COMMAND_COMPLETIONS
            .iter()
            .filter(|command| command.starts_with(&prefix))
            .map(|command| (*command).to_string())
            .collect()
    }

    fn handle_tab_completion(&mut self) {
        // Once completion starts, cycle using the original replacement span;
        // the inserted trailing space must not become the next prefix.
        if let Some(completion) = &mut self.tab_completion {
            if completion.matches.is_empty() {
                return;
            }
            completion.index = (completion.index + 1) % completion.matches.len();
            self.input_text = format!(
                "{}{}{}",
                completion.base, completion.matches[completion.index], completion.suffix
            );
            return;
        }

        let input = self.input_text.clone();
        let (prefix, matches, base, suffix) =
            if input.starts_with('/') && !input.chars().any(char::is_whitespace) {
                (
                    input.clone(),
                    Self::find_command_completions(&input),
                    String::new(),
                    " ".to_string(),
                )
            } else {
                // Advance past the whole whitespace character: multi-byte
                // whitespace (U+00A0, U+3000, ...) would otherwise leave `idx + 1`
                // mid-character and the slicing below would panic.
                let start = input.rfind(char::is_whitespace).map_or(0, |idx| {
                    idx + input[idx..].chars().next().map_or(1, char::len_utf8)
                });
                let prefix = input[start..].to_string();
                let suffix = if start == 0 { ": " } else { " " };
                (
                    prefix.clone(),
                    self.find_nick_completions(&prefix),
                    input[..start].to_string(),
                    suffix.to_string(),
                )
            };

        if prefix.is_empty() || matches.is_empty() {
            return;
        }

        self.input_text = format!("{}{}{}", base, matches[0], suffix);
        self.tab_completion = Some(TabCompletion {
            matches,
            index: 0,
            base,
            suffix,
        });
    }

    fn reset_tab_completion(&mut self) {
        self.tab_completion = None;
    }

    /// Draw the active view's message scrollback, virtualized: only the rows
    /// intersecting the viewport (plus one straddler below) are laid out
    /// visibly each frame. Row heights are cached per message and invalidated
    /// as a whole whenever the content width or font size changes.
    ///
    /// Heights are not estimated from font metrics: a never-measured row is
    /// laid out once inside a scratch Ui parked far below the clip rect (so it
    /// paints nothing) and its real laid-out advance is cached. Measurement
    /// and drawing therefore cannot disagree, which keeps cumulative offsets,
    /// the scrollbar range, and stick-to-bottom exact.
    pub fn draw_scrollback(&self, ui: &mut egui::Ui, available_height: f32) -> ScrollbackStats {
        let messages = if let Some(channel_name) = &self.current_channel {
            self.channels.get(channel_name).map(|c| &c.messages)
        } else {
            Some(&self.server_messages)
        };
        let Some(msgs) = messages else {
            return ScrollbackStats {
                drawn: 0,
                measured: 0,
                total_rows: 0,
            };
        };
        let my_nick = self.my_nick.clone();
        let stick = self.scroll_to_bottom;
        let mut stats = ScrollbackStats {
            drawn: 0,
            measured: 0,
            total_rows: msgs.len(),
        };

        ScrollArea::vertical()
            .auto_shrink([false; 2])
            .max_height(available_height)
            .stick_to_bottom(stick)
            .show_viewport(ui, |ui, viewport| {
                if msgs.is_empty() {
                    return;
                }
                let content_width = ui.available_width();
                let spacing = ui.spacing().item_spacing;
                let layout_key = row_layout_key(ui, content_width);

                // Pass 1 (arithmetic on warm cache): cumulative offsets to
                // find the first row intersecting the viewport top and the
                // total content height. Rows above the viewport are never
                // drawn. Offsets are in content coordinates (0 = top of the
                // scrolled content), matching `viewport`.
                let mut y = 0.0f32;
                let mut first_idx = msgs.len();
                let mut first_y = 0.0f32;
                for (i, msg) in msgs.iter().enumerate() {
                    let bottom =
                        y + row_height(ui, msg, &my_nick, content_width, layout_key, &mut stats);
                    if first_idx == msgs.len() && bottom >= viewport.min.y {
                        first_idx = i;
                        first_y = y;
                    }
                    y = bottom + spacing.y;
                }
                ui.set_height((y - spacing.y).max(0.0));
                if first_idx == msgs.len() {
                    // The viewport sits past the end of the content (a
                    // transient state mid-scroll, e.g. right after scrollback
                    // trimming shrank the history). Pin the buffer's tail to
                    // the viewport bottom for this frame rather than flashing
                    // an empty area; egui re-clamps the offset next frame.
                    let mut yy = viewport.max.y;
                    for msg in msgs.iter().rev() {
                        let h =
                            row_height(ui, msg, &my_nick, content_width, layout_key, &mut stats);
                        let top = yy - h;
                        first_idx -= 1;
                        first_y = top;
                        if top <= viewport.min.y {
                            break;
                        }
                        yy = top - spacing.y;
                    }
                }

                // Pass 2: lay out only the visible band, flowing naturally
                // from the first visible row - the same trick egui's own
                // show_rows uses for uniform rows, adapted to variable ones.
                // The row straddling the viewport's bottom edge is included,
                // so partial rows never flicker in from below.
                //
                // `viewport` is content space while UiBuilder rects are in
                // the parent's screen space (shifted up by the scroll
                // offset), so the band origin must be translated by
                // ui.max_rect().top() - exactly what egui's show_rows does.
                let top = ui.max_rect().top();
                let x_range = ui.max_rect().x_range();
                let band_top = top + first_y;
                if band_top > ui.max_rect().bottom() {
                    return; // degenerate mid-scroll frame; nothing to draw
                }
                let band = egui::Rect::from_x_y_ranges(x_range, band_top..=ui.max_rect().bottom());
                let mut drawn = 0usize;
                // Track the band cursor in content space ourselves: the child
                // Ui's cursor is in screen space and must not be compared to
                // the content-space viewport.
                let mut cy = first_y;
                ui.scope_builder(egui::UiBuilder::new().max_rect(band), |rows_ui| {
                    for msg in msgs.iter().skip(first_idx) {
                        if cy > viewport.max.y {
                            break;
                        }
                        // Warm cache at this point: pass 1 measured everything.
                        cy += msg.row_height.get().height + spacing.y;
                        draw_chat_row(rows_ui, msg, &my_nick);
                        drawn += 1;
                    }
                });
                stats.drawn = drawn;
            });
        stats
    }

    fn ordered_tabs(&self) -> Vec<Option<String>> {
        let mut tabs = vec![None];
        let mut names: Vec<_> = self.channels.keys().cloned().collect();
        names.sort_by_key(|name| self.network_support.canonicalize(name));
        tabs.extend(names.into_iter().map(Some));
        tabs
    }

    fn select_tab(&mut self, target: Option<String>) {
        self.current_channel = target;
        self.selected_user = None;
        if let Some(channel_name) = &self.current_channel {
            if let Some(channel) = self.channels.get_mut(channel_name) {
                channel.unread = 0;
            }
        } else {
            self.server_unread = 0;
        }
        if let Some(target) = self.current_channel.clone() {
            self.mark_target_read(&target);
        }
    }

    fn cycle_tab(&mut self, reverse: bool) {
        let tabs = self.ordered_tabs();
        if tabs.is_empty() {
            return;
        }
        let current = tabs
            .iter()
            .position(
                |tab| match (tab.as_deref(), self.current_channel.as_deref()) {
                    (None, None) => true,
                    (Some(left), Some(right)) => self.identifiers_equal(left, right),
                    _ => false,
                },
            )
            .unwrap_or(0);
        let next = if reverse {
            current.checked_sub(1).unwrap_or(tabs.len() - 1)
        } else {
            (current + 1) % tabs.len()
        };
        self.select_tab(tabs[next].clone());
    }

    pub(crate) fn close_current_tab(&mut self) {
        let Some(channel_name) = self.current_channel.clone() else {
            return;
        };
        let should_part = self.is_channel_name(&channel_name)
            && self
                .channel_key(&channel_name)
                .and_then(|key| self.channels.get(&key))
                .is_some_and(|channel| channel.joined);
        if should_part && self.send_command(IrcCommand::Part(channel_name.clone(), None)) {
            // Keep the tab until the server confirms our PART.
            return;
        }
        self.remove_channel(&channel_name);
        self.current_channel = self.channels.keys().next().cloned();
    }

    /// Handle CTCP request and send reply if applicable
    /// Returns true if the message was a CTCP request that was handled
    fn handle_ctcp_request(&mut self, sender: &str, content: &str) -> bool {
        // CTCP messages start and end with \x01. Require >= 2 bytes so the opening
        // and closing delimiters are distinct; a lone "\x01" (first byte == last
        // byte) would otherwise slice as content[1..0] and panic.
        if content.len() < 2 || !content.starts_with('\x01') || !content.ends_with('\x01') {
            return false;
        }

        // Extract CTCP command and args
        let ctcp_content = &content[1..content.len() - 1];
        let parts: Vec<&str> = ctcp_content.splitn(2, ' ').collect();
        let ctcp_cmd = parts[0].to_uppercase();
        let ctcp_args = parts.get(1).copied().unwrap_or("");

        // ACTION is not a request, it's a message type - don't reply
        if ctcp_cmd == "ACTION" {
            return false;
        }

        // Build the reply based on the CTCP command
        let reply = match ctcp_cmd.as_str() {
            "VERSION" => Some(
                concat!(
                    "\x01VERSION Linefeed v",
                    env!("CARGO_PKG_VERSION"),
                    " - Rust/egui cross-platform IRC client\x01"
                )
                .to_string(),
            ),
            "TIME" => {
                // Get current local time
                use std::time::{SystemTime, UNIX_EPOCH};
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                #[cfg(unix)]
                let time_str = {
                    let t = secs as libc::time_t;
                    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
                    unsafe { libc::localtime_r(&t, &mut tm) };
                    format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                        tm.tm_year + 1900,
                        tm.tm_mon + 1,
                        tm.tm_mday,
                        tm.tm_hour,
                        tm.tm_min,
                        tm.tm_sec
                    )
                };

                #[cfg(not(unix))]
                let time_str = {
                    let days = secs / 86400;
                    let hours = (secs % 86400) / 3600;
                    let mins = (secs % 3600) / 60;
                    let s = secs % 60;
                    format!("Day {} {:02}:{:02}:{:02} UTC", days, hours, mins, s)
                };

                Some(format!("\x01TIME {}\x01", time_str))
            }
            "PING" => {
                // Echo back the ping argument
                Some(format!("\x01PING {}\x01", ctcp_args))
            }
            "CLIENTINFO" => Some(
                "\x01CLIENTINFO ACTION PING VERSION TIME CLIENTINFO SOURCE USERINFO\x01"
                    .to_string(),
            ),
            "SOURCE" => Some(format!("\x01SOURCE {}\x01", PROJECT_SOURCE_URL)),
            "USERINFO" => Some(format!("\x01USERINFO {}\x01", self.realname)),
            _ => None,
        };

        // Send the reply if we have one and CTCP replies are enabled
        if let Some(reply_msg) = reply {
            if self.ctcp_replies_enabled {
                self.send_command(IrcCommand::Notice(sender.to_string(), reply_msg));
                self.add_message_to_current(ChatMessage::system(&format!(
                    "[CTCP] {} from {} - replied",
                    ctcp_cmd, sender
                )));
            } else {
                self.add_message_to_current(ChatMessage::system(&format!(
                    "[CTCP] {} from {} - ignored (replies disabled)",
                    ctcp_cmd, sender
                )));
            }
            true
        } else {
            // Unknown CTCP, log it but don't reply
            self.add_message_to_current(ChatMessage::system(&format!(
                "[CTCP] Unknown {} from {}",
                ctcp_cmd, sender
            )));
            true
        }
    }

    /// Check if a message contains a mention of our nick or highlight words
    fn check_nick_mention(&self, content: &str) -> bool {
        // IRC nicknames and configured highlight words admit punctuation that
        // generic "word" tokenizers split (`foo-bar`, `[alice]`, `node.js`,
        // ...). Search for each complete casemapped needle and validate
        // IRC-identifier boundaries instead of token equality.
        let content_key = self.network_support.canonicalize(content);

        if !self.my_nick_lower.is_empty() && ident_contains(&content_key, &self.my_nick_lower) {
            return true;
        }

        // Highlight words are cached casemapped (see update_cached_lowercase)
        self.highlight_words_lower
            .iter()
            .any(|highlight| ident_contains(&content_key, highlight))
    }

    /// Handle keyboard shortcuts
    fn handle_keyboard_shortcuts(&mut self, ctx: &egui::Context) {
        // Don't process shortcuts if a dialog is open
        if self.show_connect_dialog || self.show_settings || self.show_channel_list {
            return;
        }

        let (cycle_tabs, reverse_tabs, close_tab) = ctx.input(|input| {
            (
                input.modifiers.ctrl && input.key_pressed(egui::Key::Tab),
                input.modifiers.shift,
                input.modifiers.ctrl && input.key_pressed(egui::Key::W),
            )
        });
        if cycle_tabs {
            self.cycle_tab(reverse_tabs);
        }
        if close_tab {
            self.close_current_tab();
        }

        ctx.input(|i| {
            // Check Alt+1 through Alt+9
            let alt = i.modifiers.alt;
            if alt {
                // Alt+1-9 to switch channels. Build the ordered tab list (Server
                // is index 0, then channels sorted alphabetically) only while Alt
                // is actually held - building and sorting it every frame is
                // wasted work on nearly all frames.
                let tabs = self.ordered_tabs(); // Server buffer is index 0 (Alt+1)

                for (idx, key) in [
                    egui::Key::Num1,
                    egui::Key::Num2,
                    egui::Key::Num3,
                    egui::Key::Num4,
                    egui::Key::Num5,
                    egui::Key::Num6,
                    egui::Key::Num7,
                    egui::Key::Num8,
                    egui::Key::Num9,
                ]
                .iter()
                .enumerate()
                {
                    if i.key_pressed(*key) && idx < tabs.len() {
                        self.select_tab(tabs[idx].clone());
                    }
                }
            }
        });
    }
}

impl eframe::App for IrcApp {
    fn ui(&mut self, root_ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root_ui.ctx().clone();
        let ctx = &ctx;
        // Reset frame state
        self.had_messages_this_frame = false;

        // Process incoming messages with batch limiting for UI responsiveness
        const MAX_MESSAGES_PER_FRAME: usize = 100;

        let messages: Vec<_> = if let Some(rx) = &mut self.msg_rx {
            std::iter::from_fn(|| rx.try_recv().ok())
                .take(MAX_MESSAGES_PER_FRAME + 1) // Take one extra to detect if more pending
                .collect()
        } else {
            Vec::new()
        };

        // Check if we hit the batch limit (got more than MAX_MESSAGES_PER_FRAME)
        let has_more = messages.len() > MAX_MESSAGES_PER_FRAME;

        self.had_messages_this_frame = !messages.is_empty();
        for msg in messages {
            self.handle_incoming_message(msg);
        }

        // If we hit the batch limit, request immediate repaint to process remaining
        if has_more {
            ctx.request_repaint();
        }

        // Push this frame's logged lines to disk in one flush per file instead of
        // one flush syscall per message.
        self.log_manager.flush_all();

        // Execute pending auto-perform commands (one per frame to avoid flooding)
        if let Some(ref mut commands) = self.pending_auto_perform.take()
            && let Some(cmd) = commands.first().cloned()
        {
            // Process command (add / prefix if not present)
            let cmd = if cmd.starts_with('/') {
                cmd
            } else {
                format!("/{}", cmd)
            };
            self.process_command(&cmd);

            // Put remaining commands back
            let remaining: Vec<_> = commands.iter().skip(1).cloned().collect();
            if !remaining.is_empty() {
                self.pending_auto_perform = Some(remaining);
            }
        }

        // Handle keyboard shortcuts
        self.handle_keyboard_shortcuts(ctx);

        // Track window focus for notifications
        self.window_focused = ctx.input(|i| i.focused);

        // Track user activity for auto-away
        let has_activity = ctx
            .input(|i| !i.keys_down.is_empty() || i.pointer.any_click() || i.pointer.any_pressed());
        if has_activity {
            self.last_activity = std::time::Instant::now();

            // Clear auto-away on user activity
            if self.auto_away_triggered && self.away_status.is_some() {
                self.send_command(IrcCommand::Away(None));
                self.away_status = None;
                self.auto_away_triggered = false;
                self.add_server_message(ChatMessage::system(
                    "You are no longer away (auto-detected activity)",
                ));
            }
        }

        // Check for idle timeout and set auto-away
        if self.connected && self.away_status.is_none() && self.auto_away_enabled {
            let idle_duration = self.last_activity.elapsed();
            let timeout = std::time::Duration::from_secs(self.auto_away_minutes as u64 * 60);
            if idle_duration >= timeout {
                let msg = self.auto_away_message.clone();
                self.send_command(IrcCommand::Away(Some(msg.clone())));
                self.away_status = Some(msg.clone());
                self.auto_away_triggered = true;
                self.add_server_message(ChatMessage::system(&format!("Auto-away: {}", msg)));
            }
        }

        // Lag meter - send PING every 30 seconds when connected
        if self.connected && self.last_lag_check.elapsed().as_secs() >= 30 {
            self.last_lag_check = std::time::Instant::now();
            self.ping_sent_time = Some(std::time::Instant::now());
            self.send_command(IrcCommand::Ping("LAG".to_string()));
        }

        // Connect dialog
        if self.show_connect_dialog && !self.connected && !self.connecting {
            self.show_connect_window(ctx);
        }

        // Settings dialog
        if self.show_settings {
            self.show_settings_window(ctx);
        }

        // Channel list dialog
        if self.show_channel_list {
            self.show_channel_list_window(ctx);
        }

        // Channel info dialog
        if self.show_channel_info {
            self.show_channel_info_window(ctx);
        }

        // Color picker dialog
        if self.show_color_picker
            && let Some(color_code) = self.show_color_picker_window(ctx)
        {
            self.input_text.push_str(&color_code);
        }

        // Top panel - toolbar
        egui::Panel::top("toolbar").show(root_ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Linefeed");
                ui.separator();

                if self.connected {
                    // Show yellow if away, green if active
                    if self.away_status.is_some() {
                        ui.label(RichText::new("●").color(Color32::from_rgb(255, 200, 0)));
                    } else {
                        ui.label(RichText::new("●").color(Color32::GREEN));
                    }
                    ui.label(&self.my_nick);
                    ui.label("@");
                    ui.label(self.active_endpoint_label());

                    // Show lag meter
                    if let Some(lag) = self.lag_ms {
                        let lag_color = if lag < 100 {
                            Color32::GREEN
                        } else if lag < 300 {
                            Color32::YELLOW
                        } else {
                            Color32::from_rgb(255, 100, 100)
                        };
                        ui.label(
                            RichText::new(format!("({}ms)", lag))
                                .color(lag_color)
                                .small(),
                        );
                    }

                    // Show away status
                    if let Some(away_msg) = &self.away_status {
                        ui.separator();
                        ui.label(
                            RichText::new(format!("Away: {}", away_msg))
                                .color(Color32::from_rgb(255, 200, 0)),
                        );
                    }
                } else if self.connecting {
                    ui.label(RichText::new("●").color(Color32::YELLOW));
                    ui.label("Connecting...");
                } else {
                    ui.label(RichText::new("●").color(Color32::RED));
                    ui.label("Disconnected");
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Settings").clicked() {
                        self.show_settings = true;
                    }
                    ui.separator();
                    if !self.connected && !self.connecting {
                        if ui.button("Connect").clicked() {
                            self.show_connect_dialog = true;
                        }
                    } else if self.connected && ui.button("Disconnect").clicked() {
                        let reason = if self.quit_message.is_empty() {
                            "Linefeed".to_string()
                        } else {
                            self.quit_message.clone()
                        };
                        self.request_manual_disconnect(Some(reason));
                    }
                });
            });
        });

        // Left panel - channel list
        egui::Panel::left("channels")
            .resizable(true)
            .default_size(150.0)
            .show(root_ui, |ui| {
                ui.heading("Channels");
                ui.separator();

                // Server buffer with unread notification
                let server_selected = self.current_channel.is_none();
                let server_label = if self.server_unread > 0 {
                    format!("Server ({})", self.server_unread)
                } else {
                    "Server".to_string()
                };
                let server_text = if self.server_unread > 0 {
                    // Yellow for server notifications
                    RichText::new(server_label)
                        .strong()
                        .color(Color32::from_rgb(255, 255, 100))
                } else {
                    RichText::new(server_label)
                };
                if ui.selectable_label(server_selected, server_text).clicked() {
                    self.current_channel = None;
                    self.server_unread = 0;
                    self.selected_user = None; // Clear user selection when switching
                }

                ui.separator();

                // Join channel input
                if self.connected {
                    ui.horizontal(|ui| {
                        let response = ui.add(
                            TextEdit::singleline(&mut self.join_channel)
                                .hint_text("#channel")
                                .desired_width(100.0),
                        );
                        if ((response.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                            || ui.button("+").clicked())
                            && !self.join_channel.is_empty()
                        {
                            let channel = self.normalize_channel_name(&self.join_channel);
                            self.send_command(IrcCommand::Join(channel, None, None, None));
                            self.join_channel.clear();
                        }
                    });
                    ui.separator();
                }

                // Channel list
                let mut part_channel: Option<String> = None;
                let mut close_query: Option<String> = None;
                ScrollArea::vertical().show(ui, |ui| {
                    let channels: Vec<_> = self.channels.keys().cloned().collect();
                    for channel_name in channels {
                        let is_selected = self.current_channel.as_ref() == Some(&channel_name);
                        let unread = self
                            .channels
                            .get(&channel_name)
                            .map(|c| c.unread)
                            .unwrap_or(0);
                        let is_pm = !self.is_channel_name(&channel_name);

                        let label = if unread > 0 {
                            format!("{} ({})", channel_name, unread)
                        } else {
                            channel_name.clone()
                        };

                        // Color based on type and unread status
                        let text = if unread > 0 {
                            if is_pm {
                                // PM with unread - orange/red for attention
                                RichText::new(label)
                                    .strong()
                                    .color(Color32::from_rgb(255, 150, 50))
                            } else {
                                // Channel with unread - light green
                                RichText::new(label)
                                    .strong()
                                    .color(Color32::from_rgb(100, 255, 100))
                            }
                        } else if is_pm {
                            // PM without unread - light blue to distinguish from channels
                            RichText::new(label).color(Color32::from_rgb(150, 200, 255))
                        } else {
                            RichText::new(label)
                        };

                        let response = ui.selectable_label(is_selected, text);
                        if response.clicked() {
                            self.select_tab(Some(channel_name.clone()));
                        }

                        // Double-click to open channel info
                        if response.double_clicked() && self.is_channel_name(&channel_name) {
                            self.channel_info_target = Some(channel_name.clone());
                            self.show_channel_info = true;
                            // Request fresh mode info
                            self.send_command(IrcCommand::Mode(
                                channel_name.clone(),
                                None,
                                Vec::new(),
                            ));
                        }

                        // Context menu for channels
                        let chan_for_menu = channel_name.clone();
                        response.context_menu(|ui| {
                            if self.is_channel_name(&chan_for_menu) {
                                if ui.button("Channel Info").clicked() {
                                    self.channel_info_target = Some(chan_for_menu.clone());
                                    self.show_channel_info = true;
                                    self.send_command(IrcCommand::Mode(
                                        chan_for_menu.clone(),
                                        None,
                                        Vec::new(),
                                    ));
                                    ui.close();
                                }
                                if ui.button("Part Channel").clicked() {
                                    part_channel = Some(chan_for_menu.clone());
                                    ui.close();
                                }
                            } else {
                                // Query window (private message)
                                if ui.button("Close").clicked() {
                                    close_query = Some(chan_for_menu.clone());
                                    ui.close();
                                }
                            }
                        });
                    }
                });

                // Handle channel context menu actions
                if let Some(channel) = part_channel {
                    self.send_command(IrcCommand::Part(channel, None));
                }
                if let Some(query) = close_query {
                    self.channels.remove(&query);
                    if self.current_channel.as_ref() == Some(&query) {
                        self.current_channel = self.channels.keys().next().cloned();
                    }
                }
            });

        // Right panel - user list (only for channels)
        let mut pm_to_open: Option<String> = None;
        let mut whois_nick: Option<String> = None;
        let mut op_nick: Option<(String, String)> = None; // (channel, nick)
        let mut voice_nick: Option<(String, String)> = None;
        let mut kick_nick: Option<(String, String)> = None;
        if let Some(channel_name) = &self.current_channel
            && self.is_channel_name(channel_name)
        {
            let chan_for_context = channel_name.clone();
            egui::Panel::right("users")
                .resizable(true)
                .default_size(140.0)
                .show(root_ui, |ui| {
                    if let Some(channel) = self.channels.get(&chan_for_context) {
                        // Show user count with away count if any
                        let away_count = channel.away_count();
                        if away_count > 0 {
                            ui.heading(format!(
                                "Users ({}, {} away)",
                                channel.users.len(),
                                away_count
                            ));
                        } else {
                            ui.heading(format!("Users ({})", channel.users.len()));
                        }
                        ui.separator();
                        // Get selected user for this render
                        let current_selected = self.selected_user.clone();
                        let mut new_selected: Option<String> = current_selected.clone();

                        // Row virtualization: only the visible slice is built each
                        // frame, which matters for channels with thousands of users.
                        let row_height = ui.text_style_height(&egui::TextStyle::Body);
                        ScrollArea::vertical().show_rows(
                            ui,
                            row_height,
                            channel.users.len(),
                            |ui, row_range| {
                                for user in &channel.users[row_range] {
                                    let nick = &user.nick;
                                    let mode = &user.mode;
                                    let prefix = mode.prefix();
                                    let is_selected = current_selected.as_ref() == Some(nick);
                                    // The loop already holds the user entry; a
                                    // channel.is_user_away(nick) lookup here would be a
                                    // redundant O(n) scan per user (O(n²) per frame).
                                    let is_away = user.is_away();

                                    // Color based on mode, dimmed if away
                                    let base_color = match mode {
                                        UserMode::Owner => Color32::from_rgb(255, 100, 100),
                                        UserMode::Admin => Color32::from_rgb(255, 150, 100),
                                        UserMode::Op => Color32::from_rgb(100, 200, 100),
                                        UserMode::HalfOp => Color32::from_rgb(100, 200, 200),
                                        UserMode::Voice => Color32::from_rgb(200, 200, 100),
                                        UserMode::Normal => Color32::WHITE,
                                    };

                                    // Dim color for away users (reduce to ~50% brightness)
                                    let color = if is_away {
                                        Color32::from_rgb(
                                            base_color.r() / 2,
                                            base_color.g() / 2,
                                            base_color.b() / 2,
                                        )
                                    } else {
                                        base_color
                                    };

                                    // Add (away) indicator for away users
                                    let display_text = if is_away {
                                        format!("{}{} (away)", prefix, nick)
                                    } else {
                                        format!("{}{}", prefix, nick)
                                    };

                                    let text = RichText::new(display_text).color(color);
                                    let response = ui.selectable_label(is_selected, text);

                                    // Show away message on hover
                                    if let Some(away_msg) = &user.away {
                                        response
                                            .clone()
                                            .on_hover_text(format!("Away: {}", away_msg));
                                    }

                                    // Single click to select
                                    if response.clicked() {
                                        new_selected = Some(nick.clone());
                                    }

                                    // Double click to open PM
                                    if response.double_clicked() {
                                        pm_to_open = Some(nick.clone());
                                    }

                                    // Context menu for user
                                    let nick_clone = nick.clone();
                                    let chan_clone = chan_for_context.clone();
                                    response.context_menu(|ui| {
                                        if ui.button("Private Message").clicked() {
                                            pm_to_open = Some(nick_clone.clone());
                                            ui.close();
                                        }
                                        if ui.button("WHOIS").clicked() {
                                            whois_nick = Some(nick_clone.clone());
                                            ui.close();
                                        }
                                        ui.separator();
                                        if ui.button("Op (+o)").clicked() {
                                            op_nick =
                                                Some((chan_clone.clone(), nick_clone.clone()));
                                            ui.close();
                                        }
                                        if ui.button("Voice (+v)").clicked() {
                                            voice_nick =
                                                Some((chan_clone.clone(), nick_clone.clone()));
                                            ui.close();
                                        }
                                        ui.separator();
                                        if ui.button("Kick").clicked() {
                                            kick_nick =
                                                Some((chan_clone.clone(), nick_clone.clone()));
                                            ui.close();
                                        }
                                    });
                                }
                            },
                        );

                        // Update selected user
                        self.selected_user = new_selected;
                    }
                });
        }

        // Handle user list actions
        if let Some(nick) = pm_to_open {
            self.open_query(&nick);
        }
        if let Some(nick) = whois_nick {
            self.send_command(IrcCommand::Whois(nick));
        }
        if let Some((channel, nick)) = op_nick {
            self.send_command(IrcCommand::Mode(
                channel,
                Some("+o".to_string()),
                vec![nick],
            ));
        }
        if let Some((channel, nick)) = voice_nick {
            self.send_command(IrcCommand::Mode(
                channel,
                Some("+v".to_string()),
                vec![nick],
            ));
        }
        if let Some((channel, nick)) = kick_nick {
            self.send_command(IrcCommand::Kick(channel, nick, None));
        }

        // Central panel - chat area
        egui::CentralPanel::default().show(root_ui, |ui| {
            // Channel header with modes and topic
            if let Some(channel_name) = &self.current_channel
                && let Some(channel) = self.channels.get(channel_name)
            {
                // Show channel name and modes for channels
                if self.is_channel_name(channel_name) && !channel.modes.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(channel_name).strong());
                        ui.label(
                            RichText::new(format!("[{}]", channel.modes))
                                .color(Color32::from_rgb(150, 150, 150)),
                        );
                        ui.label(
                            RichText::new(format!("({} users)", channel.users.len()))
                                .small()
                                .color(Color32::GRAY),
                        );
                    });
                }
                if let Some(topic) = &channel.topic {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Topic:").strong());
                        render_irc_text(ui, topic, Color32::WHITE);
                    });
                }
                if self.is_channel_name(channel_name) || channel.topic.is_some() {
                    ui.separator();
                }
            }

            // Messages area
            let available_height = ui.available_height() - 30.0;
            self.draw_scrollback(ui, available_height);

            // Input area
            ui.separator();
            let mut history_up = false;
            let mut history_down = false;
            let mut tab_complete = false;
            let mut insert_bold = false;
            let mut insert_underline = false;
            let mut insert_italic = false;
            let mut insert_color = false;
            let mut insert_reset = false;
            let input_before = self.input_text.clone();

            ui.horizontal(|ui| {
                // Compact formatting buttons
                if ui
                    .small_button("B")
                    .on_hover_text("Bold (Ctrl+B)")
                    .clicked()
                {
                    insert_bold = true;
                }
                if ui
                    .small_button("U")
                    .on_hover_text("Underline (Ctrl+U)")
                    .clicked()
                {
                    insert_underline = true;
                }
                if ui
                    .small_button("I")
                    .on_hover_text("Italic (Ctrl+I)")
                    .clicked()
                {
                    insert_italic = true;
                }
                if ui
                    .small_button("C")
                    .on_hover_text("Color (Ctrl+K)")
                    .clicked()
                {
                    insert_color = true;
                }

                // Show formatting indicator
                let has_formatting = self.input_text.contains('\x02')
                    || self.input_text.contains('\x1F')
                    || self.input_text.contains('\x1D')
                    || self.input_text.contains('\x03');
                if has_formatting {
                    ui.label(RichText::new("*").color(Color32::YELLOW).small());
                }

                let response = ui.add(
                    TextEdit::singleline(&mut self.input_text)
                        .hint_text("Type a message...")
                        .desired_width(ui.available_width() - 50.0),
                );

                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.process_input();
                    response.request_focus();
                }

                // Check for history navigation and tab completion while input has focus
                if response.has_focus() {
                    if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                        history_up = true;
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                        history_down = true;
                    }
                    if ui.input(|i| !i.modifiers.ctrl && i.key_pressed(egui::Key::Tab)) {
                        tab_complete = true;
                    }

                    // Formatting shortcuts (Ctrl+B/U/I/K/O)
                    if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::B)) {
                        insert_bold = true;
                    }
                    if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::U)) {
                        insert_underline = true;
                    }
                    // Note: Ctrl+I may be captured by system, use button instead
                    if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::K)) {
                        insert_color = true;
                    }
                    if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::O)) {
                        insert_reset = true;
                    }
                }

                if ui.button("Send").clicked() {
                    self.process_input();
                }
            });

            // Handle history navigation outside the closure
            if history_up {
                self.history_up();
            }
            if history_down {
                self.history_down();
            }

            // Handle tab completion
            if tab_complete {
                self.handle_tab_completion();
            } else if self.input_text != input_before {
                // Reset tab completion if input changed (not by Tab)
                self.reset_tab_completion();
            }

            // Handle formatting insertions
            if insert_bold {
                self.input_text.push('\x02');
            }
            if insert_underline {
                self.input_text.push('\x1F');
            }
            if insert_italic {
                self.input_text.push('\x1D');
            }
            if insert_color {
                self.show_color_picker = true;
                self.color_picker_fg = true;
            }
            if insert_reset {
                self.input_text.push('\x0F');
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_log_dir(test_name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
        let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "linefeed-gui-{test_name}-{}-{serial}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_app() -> IrcApp {
        IrcApp::with_settings(Settings {
            // Keep unit tests from writing chat logs to the real config dir.
            logging_enabled: false,
            ..Settings::default()
        })
    }

    /// A connected app whose outgoing commands can be inspected.
    fn test_app_connected() -> (IrcApp, mpsc::UnboundedReceiver<IrcCommand>) {
        let mut app = test_app();
        let (tx, rx) = mpsc::unbounded_channel();
        app.cmd_tx = Some(tx);
        app.connected = true;
        (app, rx)
    }

    #[test]
    fn configured_font_size_updates_body_and_monospace_styles() {
        let style = style_with_font_size(egui::Style::default(), 20.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Body].size, 20.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Monospace].size, 20.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Small].size, 17.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Heading].size, 27.0);
    }

    #[cfg(unix)]
    #[test]
    fn notification_helper_waits_for_exit_status() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "exit 7"]);
        let status = wait_for_notification_helper(&mut command).unwrap();
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn minimal_update_drains_and_processes_background_messages() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel(2);
        app.msg_rx = Some(rx);
        tx.try_send(IrcMessage::parse(":server NOTICE * :background").unwrap())
            .unwrap();

        app.update_minimal();

        assert!(
            app.server_messages
                .iter()
                .any(|message| { message.content.contains("background") })
        );
    }

    #[test]
    fn self_kick_marks_channel_unjoined_and_blocks_send_and_echo() {
        let (mut app, mut rx) = test_app_connected();
        app.set_my_nick("me".to_string());
        let mut channel = Channel::new();
        channel.joined = true;
        channel.add_user("me", UserMode::Normal);
        app.channels.insert("#chan".to_string(), channel);
        app.current_channel = Some("#chan".to_string());

        app.handle_incoming_message(IrcMessage::parse(":oper!u@h KICK #chan me :bye").unwrap());
        assert!(!app.channels["#chan"].joined);

        app.input_text = "hello".to_string();
        app.process_input();
        app.process_command("/amsg broadcast");
        assert!(rx.try_recv().is_err());
        assert!(
            !app.channels["#chan"]
                .messages
                .iter()
                .any(|message| message.sender == "me" && message.content == "hello")
        );
        assert!(
            app.channels["#chan"]
                .messages
                .iter()
                .any(|message| { message.content.contains("not joined") })
        );
    }

    #[test]
    fn tab_completion_cycles_nicks_and_completes_commands() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.add_user("Alina", UserMode::Normal);
        channel.add_user("Alice", UserMode::Normal);
        app.channels.insert("#chan".to_string(), channel);
        app.current_channel = Some("#chan".to_string());

        app.input_text = "Al".to_string();
        app.handle_tab_completion();
        assert_eq!(app.input_text, "Alice: ");
        app.handle_tab_completion();
        assert_eq!(app.input_text, "Alina: ");
        app.handle_tab_completion();
        assert_eq!(app.input_text, "Alice: ");

        app.reset_tab_completion();
        app.input_text = "/jo".to_string();
        app.handle_tab_completion();
        assert_eq!(app.input_text, "/join ");
    }

    #[test]
    fn tab_completion_survives_multi_byte_whitespace() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.add_user("Alice", UserMode::Normal);
        app.channels.insert("#chan".to_string(), channel);
        app.current_channel = Some("#chan".to_string());

        // A multi-byte whitespace char before the word must not panic on
        // byte-offset slicing (rfind returns the char's start offset).
        for ws in ['\u{a0}', '\u{2003}', '\u{3000}'] {
            app.reset_tab_completion();
            app.input_text = format!("hi{ws}Al");
            app.handle_tab_completion();
            let expected_base = format!("hi{ws}");
            assert_eq!(app.input_text, format!("{expected_base}Alice "));
        }
    }

    #[test]
    fn scrollback_virtualizes_large_history() {
        let mut app = test_app();
        let mut channel = Channel::new();
        for i in 0..2000 {
            channel.messages.push_back(ChatMessage::system(&format!(
                "message {i}: some channel chatter"
            )));
        }
        app.channels.insert("#chan".to_string(), channel);
        app.current_channel = Some("#chan".to_string());

        // Headless egui pass: no renderer needed, real layout runs.
        let ctx = egui::Context::default();
        let mut first_stats = None;
        ctx.run_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                first_stats = Some(app.draw_scrollback(ui, 400.0));
            });
        })
        .textures_delta
        .clear();
        let stats = first_stats.expect("panel ran");
        assert_eq!(stats.total_rows, 2000);
        assert!(
            stats.drawn < 300,
            "virtualization failed: drew {} of 2000 rows",
            stats.drawn
        );
        // Cold cache: the first pass measures every height once so it can
        // compute cumulative offsets - that is the one-time cost, paid
        // instead of re-laying out every widget on every future frame.
        assert_eq!(stats.measured, 2000);

        // Second identical frame: everything cached, nothing re-measured,
        // and still only the visible band drawn.
        let mut second_stats = None;
        ctx.run_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                second_stats = Some(app.draw_scrollback(ui, 400.0));
            });
        })
        .textures_delta
        .clear();
        let stats2 = second_stats.expect("second panel ran");
        assert_eq!(stats2.measured, 0, "heights must stay cached across frames");
        assert!(
            stats2.drawn < 300,
            "virtualization failed on warm cache: drew {} of 2000",
            stats2.drawn
        );

        // Scrolling up into older history keeps the work bounded.
        let scrolled_input = egui::RawInput {
            events: vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                // Positive Y moves content down, revealing older rows above.
                delta: egui::vec2(0.0, 80.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::default(),
            }],
            ..Default::default()
        };
        let mut scrolled_stats = None;
        ctx.run_ui(scrolled_input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                scrolled_stats = Some(app.draw_scrollback(ui, 400.0));
            });
        })
        .textures_delta
        .clear();
        let stats3 = scrolled_stats.expect("scrolled panel ran");
        assert!(
            stats3.drawn < 300,
            "post-scroll draw count exploded: {}",
            stats3.drawn
        );
    }

    #[test]
    fn row_measurement_tracks_wrapping_heights() {
        let ctx = egui::Context::default();
        let short_msg = ChatMessage::system("hi");
        let long_msg = ChatMessage::system(
            "a very long system message that will certainly need to wrap across \
             several lines when the available width is small enough for wrapping",
        );
        let mut heights = None;
        ctx.run_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let width = 120.0;
                let key = row_layout_key(ui, width);
                let mut stats = ScrollbackStats {
                    drawn: 0,
                    measured: 0,
                    total_rows: 2,
                };
                // Narrow width forces the long message to wrap over lines;
                // the measured advance must reflect that.
                let h_short = row_height(ui, &short_msg, "me", width, key, &mut stats);
                let h_long = row_height(ui, &long_msg, "me", width, key, &mut stats);
                heights = Some((h_short, h_long));
            });
        })
        .textures_delta
        .clear();
        let (h_short, h_long) = heights.expect("panel ran");
        assert!(h_long > h_short * 1.5, "{h_long} vs {h_short}");
    }

    #[test]
    fn virtualized_rows_paint_inside_the_scroll_clip() {
        // Count-based assertions cannot see coordinate bugs: rows could be
        // laid out invisibly outside the clip rect. Inspect the painted
        // shapes instead - message text must be visible on EVERY frame,
        // including the frame right after a new message arrives (whose row
        // needs measuring). A frame with no visible text is what users see
        // as the whole message area flashing blank.
        let mut app = test_app();
        let mut channel = Channel::new();
        for i in 0..500 {
            channel
                .messages
                .push_back(ChatMessage::system(&format!("row {i} of history")));
        }
        app.channels.insert("#chan".to_string(), channel);
        app.current_channel = Some("#chan".to_string());

        let ctx = egui::Context::default();
        for frame in 0..6 {
            if frame > 0 {
                // Simulate a newly arrived message: its row has no cached
                // height yet, forcing a measurement this frame.
                let chan = app.channels.get_mut("#chan").unwrap();
                chan.messages
                    .push_back(ChatMessage::system(&format!("fresh {frame}")));
            }
            let mut output = ctx.run_ui(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    app.draw_scrollback(ui, ui.max_rect().height() - 30.0);
                });
            });
            let mut visible_text = 0usize;
            for clipped in &output.shapes {
                if let egui::Shape::Text(text) = &clipped.shape {
                    let bounds = egui::Rect::from_min_size(text.pos, text.galley.rect.size());
                    if clipped.clip_rect.intersects(bounds) && !text.galley.is_empty() {
                        visible_text += 1;
                    }
                }
            }
            assert!(
                visible_text >= 5,
                "frame {frame}: only {visible_text} visible text shapes - \
                 the message area would flash blank"
            );
            // Headless: no renderer consumes the font atlas uploads, so drop
            // them explicitly (egui debug-asserts on unapplied deltas).
            output.textures_delta.clear();
        }
    }

    #[test]
    fn ctrl_tab_navigation_wraps_in_both_directions() {
        let mut app = test_app();
        app.channels.insert("#b".to_string(), Channel::new());
        let mut a = Channel::new();
        a.unread = 2;
        app.channels.insert("#a".to_string(), a);

        app.cycle_tab(false);
        assert_eq!(app.current_channel.as_deref(), Some("#a"));
        assert_eq!(app.channels["#a"].unread, 0);
        app.cycle_tab(false);
        assert_eq!(app.current_channel.as_deref(), Some("#b"));
        app.cycle_tab(false);
        assert!(app.current_channel.is_none());
        app.cycle_tab(true);
        assert_eq!(app.current_channel.as_deref(), Some("#b"));
    }

    #[test]
    fn close_current_tab_parts_live_channels_and_removes_stale_ones() {
        let (mut connected, mut rx) = test_app_connected();
        let mut live = Channel::new();
        live.joined = true;
        connected.channels.insert("#live".to_string(), live);
        connected.current_channel = Some("#live".to_string());
        connected.close_current_tab();
        assert!(connected.channels.contains_key("#live"));
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Part(channel, None)) if channel == "#live"));

        let mut disconnected = test_app();
        let mut stale = Channel::new();
        stale.joined = true;
        disconnected.channels.insert("#stale".to_string(), stale);
        disconnected.current_channel = Some("#stale".to_string());
        disconnected.close_current_tab();
        assert!(!disconnected.channels.contains_key("#stale"));
    }

    #[test]
    fn user_list_query_opening_uses_normal_initialization_and_casemapped_reuse() {
        let mut app = test_app();
        app.logging_load_history = false;
        assert!(app.open_query("Alice"));
        assert_eq!(app.current_channel.as_deref(), Some("Alice"));
        assert!(
            app.channels["Alice"]
                .messages
                .iter()
                .any(|message| message.content == "Conversation with Alice")
        );

        assert!(app.open_query("alice"));
        assert_eq!(app.channels.len(), 1);
        assert_eq!(app.current_channel.as_deref(), Some("Alice"));
    }

    #[test]
    fn active_session_is_immutable_while_connection_form_is_edited() {
        let mut app = test_app();
        app.server_host = "old.example".into();
        app.server_port = "6697".into();
        app.use_tls = true;
        app.password = "old-server-pass".into();
        app.sasl_username = "old-account".into();
        app.sasl_password = "old-sasl-pass".into();
        app.accept_invalid_certs = true;
        app.auto_join_channels = "#old".into();
        app.auto_perform = "/ns identify old-nickserv-pass".into();
        app.set_invisible = true;
        assert!(app.start_session_from_form());

        // Manual/unprofiled endpoint edits clear the form's scoped secrets but
        // cannot mutate the active socket's reconnect/automation snapshot.
        app.apply_unprofiled_endpoint("new.example", "7000", false);
        assert!(app.password.is_empty());
        assert!(app.sasl_password.is_empty());
        assert_eq!(app.active_endpoint_label(), "old.example:6697");
        assert_eq!(
            app.active_log_context().map(|context| context.0),
            Some("tls://old.example:6697".into())
        );
        assert_eq!(
            app.active_endpoint_key(),
            Some(EndpointKey {
                host: "old.example".into(),
                port: 6697,
                use_tls: true,
            })
        );
        let active = app.active_server_config().unwrap();
        assert_eq!(active.password.as_deref(), Some("old-server-pass"));
        assert_eq!(active.sasl_password.as_deref(), Some("old-sasl-pass"));

        let (tx, mut rx) = mpsc::unbounded_channel();
        app.cmd_tx = Some(tx);
        app.handle_numeric(RPL_WELCOME, &["me".into(), "welcome".into()]);
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Mode(_, Some(mode), _)) if mode == "+i"));
        assert!(
            matches!(rx.try_recv(), Ok(IrcCommand::Join(channel, _, _, _)) if channel == "#old")
        );
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Ping(token)) if token == "LAG"));
        assert_eq!(
            app.pending_auto_perform.as_deref(),
            Some(["/ns identify old-nickserv-pass".to_string()].as_slice())
        );
    }

    #[test]
    fn explicit_endpoint_switch_promotes_fresh_config_after_old_socket_closes() {
        let mut app = test_app();
        app.server_host = "old.example".into();
        app.server_port = "6697".into();
        app.password = "old-server-pass".into();
        app.sasl_username = "old-account".into();
        app.sasl_password = "old-sasl-pass".into();
        app.accept_invalid_certs = true;
        app.auto_join_channels = "#old".into();
        app.auto_perform = "/ns identify old-nickserv-pass".into();
        assert!(app.start_session_from_form());
        app.channels.insert("#old".into(), Channel::new());

        let (tx, mut rx) = mpsc::unbounded_channel();
        app.cmd_tx = Some(tx);
        app.connected = true;
        app.connecting = false;
        app.apply_unprofiled_endpoint("new.example", "7000", false);
        assert!(app.switch_session_from_form("Changing servers"));

        // Until the old worker exits, display/log/reconnect identity remains A.
        assert_eq!(app.active_endpoint_label(), "old.example:6697");
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Quit(_))));
        assert!(app.pending_session.is_some());

        app.promote_pending_session();
        assert_eq!(app.active_endpoint_label(), "new.example:7000");
        assert_eq!(
            app.active_log_context().map(|context| context.0),
            Some("plain://new.example:7000".into())
        );
        assert!(app.channels.is_empty());
        assert!(app.pending_auto_perform.is_none());
        let session = app.active_session.as_ref().unwrap();
        assert!(session.server.password.is_none());
        assert!(session.server.sasl_password.is_none());
        assert!(session.auto_join_channels.is_empty());
        assert!(session.auto_perform.is_empty());
        assert!(!session.server.accept_invalid_certs);
        let config = app.active_server_config().unwrap();
        assert_eq!(config.host, "new.example");
        assert_eq!(config.port, 7000);
        assert!(!config.use_tls);
        assert!(config.sasl_username.is_none());
    }

    #[test]
    fn manual_disconnect_cancels_a_staged_endpoint_switch() {
        let mut app = test_app();
        assert!(app.start_session_from_form());
        app.connected = true;
        let (tx, _rx) = mpsc::unbounded_channel();
        app.cmd_tx = Some(tx);
        app.apply_unprofiled_endpoint("other.example", "6667", false);
        assert!(app.switch_session_from_form("Changing servers"));
        assert!(app.pending_session.is_some());

        app.request_manual_disconnect(None);
        assert!(app.pending_session.is_none());
    }

    #[test]
    fn connection_form_rejects_every_invalid_port_without_a_fallback() {
        let mut app = test_app();
        for invalid in ["", "0", "abc", "65536", "70000", "-1"] {
            app.server_port = invalid.into();
            assert!(
                app.form_server_config_for_tests().is_err(),
                "port {invalid:?} must be rejected"
            );
        }
        for valid in ["1", "65535"] {
            app.server_port = valid.into();
            assert_eq!(
                app.form_server_config_for_tests().unwrap().port.to_string(),
                valid
            );
        }

        app.server_host = "   ".into();
        app.server_port = "6697".into();
        app.show_connect_dialog = false;
        assert!(!app.start_session_from_form());
        assert!(app.show_connect_dialog);
        assert!(!app.connecting);
        assert!(app.connection_error().is_some());
        assert!(
            app.server_messages
                .back()
                .is_some_and(|message| message.content.contains("cannot be empty"))
        );
    }

    #[test]
    fn favorite_load_preserves_its_endpoint_scoped_profile() {
        let mut app = test_app();
        let favorite = ServerFavorite {
            name: "private".into(),
            host: "favorite.example".into(),
            port: "7443".into(),
            use_tls: true,
            password: "server-pass".into(),
            nickname: "favnick".into(),
            auto_join: "#private key".into(),
            auto_perform: "/ns identify nickserv-pass".into(),
            sasl_username: "account".into(),
            sasl_password: "sasl-pass".into(),
            username: "ident".into(),
            realname: "Favorite User".into(),
            accept_invalid_certs: true,
            pre_away_message: "syncing".into(),
            persistence_profile: "mobile".into(),
        };
        app.load_favorite(&favorite);
        assert!(app.start_session_from_form());

        let config = app.active_server_config().unwrap();
        assert_eq!(config.host, "favorite.example");
        assert_eq!(config.port, 7443);
        assert_eq!(config.password.as_deref(), Some("server-pass"));
        assert_eq!(config.sasl_username.as_deref(), Some("account"));
        assert_eq!(config.sasl_password.as_deref(), Some("sasl-pass"));
        assert_eq!(config.pre_away_message.as_deref(), Some("syncing"));
        assert_eq!(config.persistence_profile.as_deref(), Some("mobile"));
        assert!(config.accept_invalid_certs);
        assert_eq!(
            app.active_session.as_ref().unwrap().auto_join_channels,
            "#private key"
        );
    }

    #[test]
    fn credential_commands_never_enter_history_search_or_local_scrollback() {
        let (mut app, mut rx) = test_app_connected();
        app.set_my_nick("me".into());
        assert!(app.open_query("NickServ"));

        for input in [
            "IDENTIFY hunter2",
            "/say IDENTIFY hunter3",
            "/msg NickServ IDENTIFY hunter4",
            "/query NickServ IDENTIFY hunter5",
            "/ns IDENTIFY hunter6",
            "/notice NickServ IDENTIFY hunter7",
        ] {
            app.input_text = input.into();
            app.process_input();
        }
        while rx.try_recv().is_ok() {}

        assert!(app.command_history.is_empty());
        let query = &app.channels["NickServ"];
        for secret in ["hunter2", "hunter3", "hunter4", "hunter5", "hunter6"] {
            assert!(
                query
                    .messages
                    .iter()
                    .all(|message| !message.content.contains(secret))
            );
        }
        assert!(
            query.messages.iter().any(|message| {
                message.no_log && message.content == "<credential command sent>"
            })
        );
        assert!(
            query
                .messages
                .iter()
                .all(|message| { !message.content.contains("hunter7") })
        );

        app.input_text = "/lastlog IDENTIFY".into();
        app.process_input();
        assert!(
            app.channels["NickServ"]
                .messages
                .iter()
                .all(|message| !message.content.contains("hunter"))
        );
    }

    #[test]
    fn casemapped_channel_messages_share_one_endpoint_scoped_log() {
        let dir = temporary_log_dir("casemapped-log");
        let mut app = test_app();
        app.log_manager = logging::LogManager::with_test_dir(dir.clone());
        app.logging_enabled = true;
        app.server_host = "IRC.Example".into();
        app.server_port = "6697".into();
        app.use_tls = true;
        assert!(app.start_session_from_form());

        app.add_message_to_channel("#Te[st", ChatMessage::new_fmt("alice", "first", "short"));
        app.add_message_to_channel("#te{st", ChatMessage::new_fmt("bob", "second", "short"));
        app.log_manager.flush_all();

        assert_eq!(app.channels.len(), 1);
        assert!(app.channels.contains_key("#Te[st"));
        let path = app
            .log_manager
            .test_log_path("tls://irc.example:6697", "#Te[st");
        let alternate = app
            .log_manager
            .test_log_path("tls://irc.example:6697", "#te{st");
        let contents = std::fs::read_to_string(path).unwrap();
        assert!(contents.contains("first"));
        assert!(contents.contains("second"));
        assert!(!alternate.exists());

        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ctcp_source_and_clientinfo_advertise_supported_metadata() {
        let (mut app, mut rx) = test_app_connected();
        assert!(app.handle_ctcp_request("peer", "\x01SOURCE\x01"));
        assert!(matches!(
            rx.try_recv(),
            Ok(IrcCommand::Notice(target, reply))
                if target == "peer"
                    && reply == format!("\x01SOURCE {}\x01", PROJECT_SOURCE_URL)
        ));

        assert!(app.handle_ctcp_request("peer", "\x01CLIENTINFO\x01"));
        assert!(matches!(
            rx.try_recv(),
            Ok(IrcCommand::Notice(target, reply))
                if target == "peer" && reply.contains("SOURCE")
        ));
    }

    #[test]
    fn isupport_updates_channel_types() {
        let mut app = test_app();
        app.handle_numeric(
            RPL_ISUPPORT,
            &[
                "nick".to_string(),
                "CHANTYPES=#&+!".to_string(),
                "PREFIX=(qaohv)~&@%+".to_string(),
                "CASEMAPPING=strict-rfc1459".to_string(),
                "are supported".to_string(),
            ],
        );
        assert!(app.is_channel_name("+ops"));
        assert!(app.is_channel_name("!safe"));
        assert!(!app.is_channel_name("nick"));
        assert_eq!(app.network_support.case_mapping, CaseMapping::StrictRfc1459);
        assert!(app.identifiers_equal("[Nick]", "{nick}"));
        assert!(!app.identifiers_equal("Nick^", "nick~"));
    }

    #[test]
    fn casemapped_incoming_query_reuses_existing_tab() {
        let mut app = test_app();
        app.set_my_nick("me".to_string());
        app.add_message_to_channel("Alice", ChatMessage::system("opened"));

        let msg = IrcMessage::parse(":alice!u@h PRIVMSG me :hello").unwrap();
        app.handle_incoming_message(msg);

        assert_eq!(app.channels.len(), 1);
        assert!(app.channels.contains_key("Alice"));
        assert!(
            app.channels["Alice"]
                .messages
                .iter()
                .any(|message| message.content == "hello")
        );
    }

    #[test]
    fn question_mark_only_ignore_pattern_matches_nick() {
        let mut app = test_app();
        app.ignore_list = vec!["a?ice".to_string()];
        app.update_cached_lowercase();

        assert!(app.is_ignored("alice", Some("alice!u@host")));
        assert!(app.is_ignored("axice", Some("axice!u@host")));
        assert!(!app.is_ignored("allice", Some("allice!u@host")));
        assert!(!app.is_ignored("bob", Some("bob!u@host")));
    }

    #[test]
    fn special_character_nicks_highlight_with_identifier_boundaries() {
        let mut app = test_app();
        app.set_my_nick("foo-bar".to_string());
        assert!(app.check_nick_mention("hello foo-bar!"));
        assert!(app.check_nick_mention("(FOO-BAR), ping"));
        assert!(!app.check_nick_mention("xfoo-bar"));
        assert!(!app.check_nick_mention("foo-barry"));

        app.set_my_nick("[alice]".to_string());
        assert!(app.check_nick_mention("hi {ALICE},"));
        assert!(!app.check_nick_mention("x[alice]"));
    }

    #[test]
    fn highlight_words_match_with_punctuation_at_identifier_boundaries() {
        let mut app = test_app();
        app.highlight_words = "foo-bar, node.js".to_string();
        app.update_cached_lowercase();

        // Punctuation-bearing words (the dialog hint suggests alt nicks like
        // "your-other-nick") must fire; token-equality matching silently
        // dropped them.
        assert!(app.check_nick_mention("foo-bar: are you there"));
        assert!(app.check_nick_mention("pinging FOO-BAR again"));
        assert!(app.check_nick_mention("the node.js bot said hi"));
        assert!(!app.check_nick_mention("xfoo-bar"));
        assert!(!app.check_nick_mention("node.jss"));
        assert!(!app.check_nick_mention("no highlights here"));
    }

    #[test]
    fn rpl_list_keeps_numeric_topics_from_becoming_user_counts() {
        let mut app = test_app();

        // Standard: 322 <me> <#chan> <count> :<topic>
        app.handle_numeric(
            RPL_LIST,
            &[
                "me".to_string(),
                "#general".to_string(),
                "12".to_string(),
                "2024".to_string(),
            ],
        );
        let entry = &app.channel_list[0];
        assert_eq!(entry.name, "#general");
        assert_eq!(entry.user_count, 12, "topic must not overwrite the count");
        assert_eq!(entry.topic_clean, "2024", "numeric topic must be kept");

        // Ordinary topic still lands.
        app.handle_numeric(
            RPL_LIST,
            &[
                "me".to_string(),
                "#other".to_string(),
                "5".to_string(),
                "hello world".to_string(),
            ],
        );
        assert_eq!(app.channel_list[1].user_count, 5);
        assert_eq!(app.channel_list[1].topic_clean, "hello world");
    }

    #[test]
    fn names_numeric_atomically_replaces_stale_users_and_reads_multi_prefix() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.add_user("alice", UserMode::Normal);
        channel.add_user("stale", UserMode::Op);
        app.channels.insert("#chan".to_string(), channel);

        app.handle_numeric(
            RPL_NAMREPLY,
            &[
                "me".to_string(),
                "=".to_string(),
                "#CHAN".to_string(),
                "@+Alice bob".to_string(),
            ],
        );
        // The visible list is unchanged until the authoritative burst ends.
        assert!(app.channels["#chan"].has_user("stale"));
        app.handle_numeric(
            RPL_ENDOFNAMES,
            &["me".to_string(), "#CHAN".to_string(), "end".to_string()],
        );

        let channel = &app.channels["#chan"];
        assert!(!channel.has_user("stale"));
        let alice = channel.get_user("alice").unwrap();
        assert!(alice.has_mode(UserMode::Op));
        assert!(alice.has_mode(UserMode::Voice));
        assert_eq!(alice.mode, UserMode::Op);
    }

    #[test]
    fn userhost_in_names_keeps_only_the_membership_nickname() {
        let mut app = test_app();
        app.channels.insert("#chan".into(), Channel::new());
        app.handle_numeric(
            RPL_NAMREPLY,
            &[
                "me".into(),
                "=".into(),
                "#chan".into(),
                "@+Alice!user@example.test bob!ident@host".into(),
            ],
        );
        app.handle_numeric(RPL_ENDOFNAMES, &["me".into(), "#chan".into()]);

        let channel = &app.channels["#chan"];
        assert!(channel.has_user("Alice"));
        assert!(channel.has_user("bob"));
        assert!(!channel.has_user("Alice!user@example.test"));
    }

    #[test]
    fn extended_join_and_setname_retain_member_identity() {
        let mut app = test_app();
        app.set_my_nick("me".into());
        app.channels.insert("#room".into(), Channel::new());

        app.handle_incoming_message(
            IrcMessage::parse(":alice!u@h JOIN #room alice-account :Alice Example").unwrap(),
        );
        let alice = app.channels["#room"].get_user("alice").unwrap();
        assert_eq!(alice.account.as_deref(), Some("alice-account"));
        assert_eq!(alice.realname.as_deref(), Some("Alice Example"));

        app.handle_incoming_message(
            IrcMessage::parse(":alice!u@h SETNAME :Alice Updated").unwrap(),
        );
        assert_eq!(
            app.channels["#room"]
                .get_user("alice")
                .unwrap()
                .realname
                .as_deref(),
            Some("Alice Updated")
        );
    }

    #[test]
    fn private_reaction_routes_to_the_senders_query() {
        let mut app = test_app();
        app.set_my_nick("me".into());
        app.handle_incoming_message(
            IrcMessage::parse("@+draft/react=👍;+reply=abc :alice!u@h TAGMSG me").unwrap(),
        );

        assert!(app.channels.contains_key("alice"));
        assert!(!app.channels.contains_key("me"));
        assert!(
            app.channels["alice"].messages[0]
                .content
                .contains("reacted 👍 to abc")
        );
    }

    #[test]
    fn no_implicit_names_requests_membership_after_join() {
        let (mut app, mut rx) = test_app_connected();
        app.set_my_nick("me".into());
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :no-implicit-names").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #room").unwrap());

        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Names(Some("#room".into()))
        );
    }

    #[test]
    fn echo_message_enriches_the_local_row_without_duplication() {
        let (mut app, _rx) = test_app_connected();
        app.set_my_nick("me".into());
        app.handle_incoming_message(IrcMessage::parse(":srv CAP me ACK :echo-message").unwrap());
        app.add_message_to_channel("#room", ChatMessage::new_fmt("me", "hello", "short"));

        app.handle_incoming_message(
            IrcMessage::parse(
                "@time=2026-09-12T12:00:00.000Z;msgid=abc;account=acct;+reply=prior :me!u@h PRIVMSG #room :hello",
            )
            .unwrap(),
        );

        let channel = &app.channels["#room"];
        assert_eq!(channel.messages.len(), 1);
        assert_eq!(channel.messages[0].msgid.as_deref(), Some("abc"));
        assert_eq!(channel.messages[0].account.as_deref(), Some("acct"));
        assert_eq!(channel.messages[0].reply_to.as_deref(), Some("prior"));
    }

    #[test]
    fn selected_live_message_advances_the_server_read_marker() {
        let (mut app, mut rx) = test_app_connected();
        app.set_my_nick("me".into());
        app.current_channel = Some("#room".into());
        app.channels.insert("#room".into(), Channel::new());
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/read-marker").unwrap(),
        );

        app.handle_incoming_message(
            IrcMessage::parse(
                "@time=2026-09-12T12:00:00.000Z;msgid=abc :alice!u@h PRIVMSG #room :hello",
            )
            .unwrap(),
        );

        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Markread(
                "#room".into(),
                Some("timestamp=2026-09-12T12:00:00.000Z".into())
            )
        );
    }

    #[test]
    fn standard_reply_uses_its_channel_context() {
        let mut app = test_app();
        app.channels.insert("#room".into(), Channel::new());
        app.handle_incoming_message(
            IrcMessage::parse(":srv FAIL CHATHISTORY INVALID_TARGET #room :No history").unwrap(),
        );
        assert!(
            app.channels["#room"]
                .messages
                .iter()
                .any(|message| message.content.contains("INVALID_TARGET"))
        );
        assert!(app.server_messages.is_empty());
    }

    #[test]
    fn live_redaction_removes_the_identified_message() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.messages.push_back(
            ChatMessage::new_fmt("alice", "remove me", "short").with_irc_metadata(
                None,
                Some("gone".into()),
                None,
            ),
        );
        app.channels.insert("#room".into(), channel);

        app.handle_incoming_message(
            IrcMessage::parse(":alice!u@h REDACT #room gone :cleanup").unwrap(),
        );

        assert!(
            app.channels["#room"]
                .messages
                .iter()
                .all(|message| message.msgid.as_deref() != Some("gone"))
        );
        assert!(
            app.channels["#room"]
                .messages
                .iter()
                .any(|message| message.content == "A message was redacted: cleanup")
        );
    }

    #[test]
    fn cap_new_requests_newly_available_nefarious_features() {
        let (mut app, mut rx) = test_app_connected();
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me NEW :draft/chathistory=20 draft/webpush=vapid-key")
                .unwrap(),
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Cap(
                String::new(),
                "REQ".into(),
                vec!["draft/chathistory draft/webpush".into()]
            )
        );
        assert_eq!(app.chathistory_limit, Some(20));
    }

    #[test]
    fn cap_new_splits_the_full_supported_set_into_valid_lines() {
        let (mut app, mut rx) = test_app_connected();
        app.handle_cap_message("NEW", &[DESIRED_CAPS.join(" ")]);

        let mut requested = Vec::new();
        while let Ok(command) = rx.try_recv() {
            assert!(IrcApp::command_fits_irc_line(&command));
            let IrcCommand::Cap(_, subcommand, params) = command else {
                panic!("expected CAP REQ");
            };
            assert_eq!(subcommand, "REQ");
            requested.extend(params.join(" ").split_whitespace().map(str::to_string));
        }
        assert_eq!(requested, DESIRED_CAPS);
    }

    #[test]
    fn fatal_registration_error_clears_connecting() {
        let mut app = test_app();
        app.connecting = true;
        app.handle_numeric(
            ERR_PASSWDMISMATCH,
            &["nick".to_string(), "Password incorrect".to_string()],
        );
        assert!(!app.connecting);
        assert!(!app.connected);
        assert_eq!(app.connection_intent, ConnectionIntent::ManualDisconnect);
        assert!(
            app.server_messages
                .back()
                .is_some_and(|m| m.content.contains("Registration failed"))
        );
    }

    #[test]
    fn incoming_mode_updates_user_mode() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.add_user("alice", UserMode::Normal);
        app.channels.insert("#chan".to_string(), channel);

        app.handle_mode_message("oper", "#chan", Some("+o"), &["alice".to_string()]);

        let alice = app.channels["#chan"].get_user("alice").unwrap();
        assert_eq!(alice.mode, UserMode::Op);
    }

    #[test]
    fn incoming_membership_modes_preserve_other_prefixes() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.add_user("alice", UserMode::Op);
        app.channels.insert("#chan".to_string(), channel);

        app.handle_mode_message("oper", "#CHAN", Some("+v"), &["Alice".to_string()]);
        let alice = app.channels["#chan"].get_user("alice").unwrap();
        assert_eq!(alice.mode, UserMode::Op);
        assert!(alice.has_mode(UserMode::Voice));

        app.handle_mode_message("oper", "#CHAN", Some("-v"), &["Alice".to_string()]);
        let alice = app.channels["#chan"].get_user("alice").unwrap();
        assert_eq!(alice.mode, UserMode::Op);
        assert!(!alice.has_mode(UserMode::Voice));
    }

    #[test]
    fn live_channel_modes_update_parameter_values() {
        let mut app = test_app();
        app.channels.insert("#chan".to_string(), Channel::new());
        app.handle_numeric(
            RPL_CHANNELMODEIS,
            &[
                "me".to_string(),
                "#CHAN".to_string(),
                "+ntk".to_string(),
                "oldkey".to_string(),
            ],
        );

        app.handle_mode_message(
            "oper",
            "#CHAN",
            Some("-k+l"),
            &["oldkey".into(), "50".into()],
        );
        let channel = &app.channels["#chan"];
        assert_eq!(channel.modes, "+ntl");
        assert_eq!(channel.mode_params, ["50"]);

        app.handle_mode_message("oper", "#chan", Some("-l"), &[]);
        let channel = &app.channels["#chan"];
        assert_eq!(channel.modes, "+nt");
        assert!(channel.mode_params.is_empty());
    }

    #[test]
    fn mode_params_align_with_isupport_chanmodes() {
        let mut app = test_app();
        // InspIRCd-style network: +f (flood limit) is a type-C mode that takes
        // a parameter when set; PREFIX only has o/v.
        app.handle_numeric(
            RPL_ISUPPORT,
            &[
                "nick".to_string(),
                "CHANMODES=eIbq,k,flj,imnpst".to_string(),
                "PREFIX=(ov)@+".to_string(),
                "are supported".to_string(),
            ],
        );
        let mut channel = Channel::new();
        channel.add_user("alice", UserMode::Normal);
        app.channels.insert("#chan".to_string(), channel);

        // "+fo [4:5] alice": 'f' must consume "[4:5]" so 'o' reads "alice".
        app.handle_mode_message(
            "oper",
            "#chan",
            Some("+fo"),
            &["[4:5]".to_string(), "alice".to_string()],
        );
        let alice = app.channels["#chan"].get_user("alice").unwrap();
        assert_eq!(alice.mode, UserMode::Op);

        // Libera-style +q is a quiet LIST mode here, not owner: consuming the
        // mask must not create or promote any user.
        app.handle_mode_message("oper", "#chan", Some("+q"), &["*!*@spam".to_string()]);
        assert!(!app.channels["#chan"].has_user("*!*@spam"));
        assert_eq!(
            app.channels["#chan"].get_user("alice").unwrap().mode,
            UserMode::Op
        );
    }

    #[test]
    fn incoming_kick_removes_user_and_notes_channel() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.add_user("bob", UserMode::Normal);
        app.channels.insert("#chan".to_string(), channel);

        let msg = IrcMessage::parse(":alice!a@h KICK #chan bob :spamming").unwrap();
        app.handle_incoming_message(msg);

        let ch = &app.channels["#chan"];
        assert!(!ch.has_user("bob"));
        assert!(ch.messages.iter().any(|m| {
            m.content
                .contains("bob was kicked from #chan by alice (spamming)")
        }));
    }

    #[test]
    fn incoming_self_kick_clears_users_but_keeps_tab() {
        let mut app = test_app();
        app.set_my_nick("me".to_string());
        let mut channel = Channel::new();
        channel.add_user("me", UserMode::Normal);
        channel.add_user("bob", UserMode::Normal);
        app.channels.insert("#chan".to_string(), channel);
        app.current_channel = Some("#chan".to_string());

        let msg = IrcMessage::parse(":alice!a@h KICK #chan me :bye").unwrap();
        app.handle_incoming_message(msg);

        let ch = &app.channels["#chan"];
        assert!(ch.users.is_empty());
        assert!(ch.messages.iter().any(|m| {
            m.content
                .contains("You were kicked from #chan by alice (bye)")
        }));
    }

    #[test]
    fn nick_in_use_while_connected_reports_without_renaming() {
        let mut app = test_app();
        app.set_my_nick("mike".to_string());
        app.connected = true;
        app.handle_numeric(
            ERR_NICKNAMEINUSE,
            &[
                "mike".to_string(),
                "john".to_string(),
                "Nickname is already in use".to_string(),
            ],
        );
        assert_eq!(app.my_nick, "mike");
        assert!(
            app.server_messages
                .back()
                .is_some_and(|m| m.content.contains("john"))
        );
    }

    #[test]
    fn nick_in_use_during_registration_retries_with_suffix() {
        let mut app = test_app();
        app.set_my_nick("mike".to_string());
        app.connected = false;
        app.handle_numeric(
            ERR_NICKNAMEINUSE,
            &[
                "*".to_string(),
                "mike".to_string(),
                "Nickname is already in use".to_string(),
            ],
        );
        assert_eq!(app.my_nick, "mike_");
    }

    #[test]
    fn overlong_quit_reason_is_clamped_to_fit_one_line() {
        let clamped = IrcApp::clamp_quit_reason(Some("é".repeat(600))).unwrap();
        let line = format!("{}", IrcCommand::Quit(Some(clamped.clone())));
        assert!(line.len() <= IRC_MAX_LINE_BYTES);
        assert!(clamped.is_char_boundary(clamped.len()));
        // Short reasons pass through untouched.
        assert_eq!(
            IrcApp::clamp_quit_reason(Some("bye".to_string())).as_deref(),
            Some("bye")
        );
    }

    #[test]
    fn disconnect_command_stops_reconnect_loop_without_touching_setting() {
        let mut app = test_app();
        app.connection_lost = true;
        app.auto_reconnect = true;
        app.last_disconnect_time = None;
        app.channel_list_loading = true;

        app.process_command("/disconnect");

        assert!(!app.connection_lost);
        assert!(!app.should_reconnect());
        assert!(!app.channel_list_loading);
        assert!(app.auto_reconnect, "persisted setting must not be flipped");
    }

    #[test]
    fn unexpected_disconnect_stops_channel_list_loading() {
        let mut app = test_app();
        app.connected = true;
        app.channel_list_loading = true;

        app.mark_connection_lost();

        assert!(!app.channel_list_loading);
        assert!(app.connection_lost);
    }

    #[test]
    fn awaiting_reconnect_covers_the_whole_backoff_window() {
        let mut app = test_app();
        app.auto_reconnect = true;
        assert!(!app.awaiting_reconnect(), "nothing pending before a loss");

        app.mark_connection_lost();
        // The backoff deadline has not passed yet (should_reconnect is still
        // false), but the event loop must already keep polling or the frame
        // would never re-run to observe it on an idle window.
        assert!(app.awaiting_reconnect());
        assert!(!app.should_reconnect());

        app.reconnect_attempts = app.max_reconnect_attempts;
        assert!(!app.awaiting_reconnect(), "attempts exhausted");

        app.reconnect_attempts = 0;
        app.connecting = true;
        assert!(!app.awaiting_reconnect(), "a retry is already in flight");

        app.connecting = false;
        app.auto_reconnect = false;
        assert!(!app.awaiting_reconnect(), "setting disabled");
    }

    #[test]
    fn msg_with_doubled_space_still_reaches_target() {
        let (mut app, mut rx) = test_app_connected();
        app.process_command("/msg  alice hi");
        match rx.try_recv() {
            Ok(IrcCommand::Privmsg(target, content)) => {
                assert_eq!(target, "alice");
                assert_eq!(content, "hi");
            }
            other => panic!("expected Privmsg, got {:?}", other),
        }
        assert!(!app.channels.contains_key(""), "no phantom empty-name tab");
    }

    #[test]
    fn kickban_with_hostmask_bans_mask_and_skips_kick() {
        let (mut app, mut rx) = test_app_connected();
        app.channels.insert("#chan".to_string(), Channel::new());
        app.current_channel = Some("#chan".to_string());

        app.process_command("/kb *!*@spam.example.com flooding");
        match rx.try_recv() {
            Ok(IrcCommand::Mode(chan, mode, params)) => {
                assert_eq!(chan, "#chan");
                assert_eq!(mode.as_deref(), Some("+b"));
                assert_eq!(params, vec!["*!*@spam.example.com".to_string()]);
            }
            other => panic!("expected Mode, got {:?}", other),
        }
        assert!(rx.try_recv().is_err(), "no KICK for a hostmask");

        // A bare nick still bans nick!*@* and kicks.
        app.process_command("/kb spammer flooding");
        match rx.try_recv() {
            Ok(IrcCommand::Mode(_, _, params)) => {
                assert_eq!(params, vec!["spammer!*@*".to_string()]);
            }
            other => panic!("expected Mode, got {:?}", other),
        }
        match rx.try_recv() {
            Ok(IrcCommand::Kick(chan, nick, reason)) => {
                assert_eq!(chan, "#chan");
                assert_eq!(nick, "spammer");
                assert_eq!(reason.as_deref(), Some("flooding"));
            }
            other => panic!("expected Kick, got {:?}", other),
        }
    }

    #[test]
    fn monitor_attached_sign_is_parsed() {
        let (mut app, mut rx) = test_app_connected();
        app.process_command("/monitor +friend");
        match rx.try_recv() {
            Ok(IrcCommand::Monitor(sub, targets)) => {
                assert_eq!(sub, "+");
                assert_eq!(targets.as_deref(), Some("friend"));
            }
            other => panic!("expected Monitor, got {:?}", other),
        }
    }

    #[test]
    fn part_while_disconnected_closes_window_locally() {
        let mut app = test_app();
        app.channels.insert("#stale".to_string(), Channel::new());
        app.current_channel = Some("#stale".to_string());

        app.process_command("/part");

        assert!(!app.channels.contains_key("#stale"));
        assert_ne!(app.current_channel.as_deref(), Some("#stale"));
    }

    #[test]
    fn nick_change_rekeys_query_window() {
        let mut app = test_app();
        let mut query = Channel::new();
        query.push_trimmed(ChatMessage::system("old talk"), 100);
        app.channels.insert("alice".to_string(), query);
        app.current_channel = Some("alice".to_string());

        let msg = IrcMessage::parse(":alice!u@h NICK :alice2").unwrap();
        app.handle_incoming_message(msg);

        assert!(!app.channels.contains_key("alice"));
        let renamed = app.channels.get("alice2").expect("window re-keyed");
        assert!(
            renamed
                .messages
                .iter()
                .any(|m| m.content.contains("old talk"))
        );
        assert!(
            renamed
                .messages
                .iter()
                .any(|m| m.content.contains("alice is now known as alice2"))
        );
        assert_eq!(app.current_channel.as_deref(), Some("alice2"));
    }

    #[test]
    fn split_irc_text_payload_preserves_utf8_boundaries() {
        let text = "é".repeat(400);
        let chunks = IrcApp::split_irc_text_payload("PRIVMSG", "#chan", &text, "", "").unwrap();
        assert!(chunks.len() > 1);
        for chunk in chunks {
            assert!(format!("PRIVMSG #chan :{}", chunk).len() <= IRC_MAX_LINE_BYTES);
            assert!(chunk.is_char_boundary(chunk.len()));
        }
    }

    #[test]
    fn joining_with_chathistory_requests_server_backlog() {
        let (mut app, mut rx) = test_app_connected();
        app.set_my_nick("me".into());
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );

        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #room").unwrap());

        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryLatest("#room".into(), "*".into(), 100)
        );
        assert!(app.channels["#room"].joined);
    }

    #[test]
    fn reconnect_requests_history_after_last_seen_msgid() {
        let (mut app, mut rx) = test_app_connected();
        app.set_my_nick("me".into());
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );
        let mut channel = Channel::new();
        channel.messages.push_back(
            ChatMessage::new_fmt("alice", "last seen", "short").with_irc_metadata(
                Some("2026-09-12T12:00:00.000Z".into()),
                Some("42-last".into()),
                Some("alice".into()),
            ),
        );
        app.channels.insert("#room".into(), channel);

        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #room").unwrap());

        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryAfter("#room".into(), "msgid=42-last".into(), 100)
        );
    }

    #[test]
    fn reconnect_requests_history_for_retained_queries_after_welcome() {
        let (mut app, mut rx) = test_app_connected();
        app.logging_load_history = true;
        app.set_invisible = false;
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );
        let mut query = Channel::new();
        query.messages.push_back(
            ChatMessage::new_fmt("alice", "last message", "short").with_irc_metadata(
                Some("2026-09-12T12:00:00.000Z".into()),
                Some("query-last".into()),
                None,
            ),
        );
        app.channels.insert("alice".into(), query);

        app.handle_numeric(RPL_WELCOME, &["me".into(), "welcome".into()]);

        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryAfter("alice".into(), "msgid=query-last".into(), 100)
        );
        assert_eq!(rx.try_recv().unwrap(), IrcCommand::Ping("LAG".into()));
    }

    #[test]
    fn nefarious_bare_chathistory_cap_value_sets_page_limit() {
        let (mut app, _rx) = test_app_connected();
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me LS :draft/chathistory=37").unwrap(),
        );
        assert_eq!(app.chathistory_limit, Some(37));
    }

    #[test]
    fn chathistory_batch_is_prepended_without_unread_or_logging() {
        let mut app = test_app();
        app.max_scrollback = 20;
        app.prepare_history_target("#public");
        app.channels
            .get_mut("#public")
            .unwrap()
            .messages
            .push_back(ChatMessage::system("live row"));

        app.handle_incoming_message(
            IrcMessage::parse("@draft/chathistory-end :srv BATCH +hist chathistory #public")
                .unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=hist;time=2026-09-12T12:00:00.000Z :alice!u@h PRIVMSG #public :old row",
            )
            .unwrap(),
        );
        assert_eq!(app.channels["#public"].messages.len(), 1);

        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -hist").unwrap());

        let channel = &app.channels["#public"];
        let contents: Vec<&str> = channel
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(
            contents,
            [
                "--- 1 line of server history loaded ---",
                "old row",
                "live row"
            ]
        );
        assert!(channel.messages[1].no_log);
        assert_eq!(channel.unread, 0);
    }

    #[test]
    fn chathistory_pages_backward_until_requested_limit() {
        let (mut app, mut rx) = test_app_connected();
        app.set_my_nick("me".into());
        app.max_scrollback = 5;
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me LS :draft/chathistory=2").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #room").unwrap());
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryLatest("#room".into(), "*".into(), 2)
        );

        app.handle_incoming_message(
            IrcMessage::parse(":srv BATCH +page chathistory #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=page;time=2026-09-12T12:00:00.000Z;msgid=old :a PRIVMSG #room :old",
            )
            .unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=page;time=2026-09-12T12:01:00.000Z;msgid=new :b PRIVMSG #room :new",
            )
            .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -page").unwrap());

        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryBefore("#room".into(), "msgid=old".into(), 2)
        );
    }

    #[test]
    fn multiline_batches_preserve_newline_and_concat_semantics() {
        let mut app = test_app();
        app.handle_incoming_message(
            IrcMessage::parse(
                "@time=2026-09-12T12:00:00.000Z;msgid=ml1 :alice!u@h BATCH +ml draft/multiline #room",
            )
            .unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=ml :alice!u@h PRIVMSG #room :one").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=ml;draft/multiline-concat :alice!u@h PRIVMSG #room :two")
                .unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=ml :alice!u@h PRIVMSG #room :three").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":alice BATCH -ml").unwrap());

        let message = app.channels["#room"].messages.back().unwrap();
        assert_eq!(message.content, "onetwo\nthree");
        assert_eq!(message.msgid.as_deref(), Some("ml1"));
        assert_eq!(
            message.server_time.as_deref(),
            Some("2026-09-12T12:00:00.000Z")
        );
    }

    #[test]
    fn authtoken_chunks_are_joined_into_one_usable_token() {
        let mut app = test_app();
        app.handle_incoming_message(
            IrcMessage::parse(":srv BATCH +tok draft/authtoken web-api").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=tok :srv TOKEN GENERATE * :abc").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=tok :srv TOKEN GENERATE * :def").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -tok").unwrap());

        assert_eq!(app.server_messages.len(), 1);
        assert_eq!(
            app.server_messages[0].content,
            "[TOKEN GENERATE web-api] abcdef"
        );
        assert!(app.server_messages[0].no_log);
    }

    #[test]
    fn chathistory_gap_is_rendered_as_a_system_marker() {
        let mut app = test_app();
        app.prepare_history_target("#room");
        app.handle_incoming_message(
            IrcMessage::parse("@draft/chathistory-end :srv BATCH +hist chathistory #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=hist;+evilnet.github.io/chathistory-gap;msgid=gap1 :srv PRIVMSG #room :[2 messages not stored]",
            )
            .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -hist").unwrap());

        let gap = app.channels["#room"]
            .messages
            .iter()
            .find(|message| message.msgid.as_deref() == Some("gap1"))
            .unwrap();
        assert!(gap.is_system);
        assert_eq!(gap.content, "[2 messages not stored]");
    }

    #[test]
    fn historical_events_are_displayed_without_mutating_live_membership() {
        let mut app = test_app();
        app.prepare_history_target("#room");
        app.channels
            .get_mut("#room")
            .unwrap()
            .add_user("present", UserMode::Normal);
        app.handle_incoming_message(
            IrcMessage::parse("@draft/chathistory-end :srv BATCH +hist chathistory #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=hist;time=2026-09-12T12:00:00.000Z;msgid=j1 :gone!u@h JOIN #room",
            )
            .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -hist").unwrap());

        let channel = &app.channels["#room"];
        assert!(channel.has_user("present"));
        assert!(!channel.has_user("gone"));
        assert!(
            channel
                .messages
                .iter()
                .any(|message| message.content == "gone joined #room")
        );
    }

    #[test]
    fn outbound_multiline_uses_a_tagged_batch() {
        let (mut app, mut rx) = test_app_connected();
        app.handle_incoming_message(IrcMessage::parse(":srv CAP me ACK :draft/multiline").unwrap());
        assert!(app.send_privmsg_text("#room", "one\ntwo"));

        let IrcCommand::Batch(open, Some(kind), Some(target)) = rx.try_recv().unwrap() else {
            panic!("expected multiline batch opener");
        };
        assert!(open.starts_with("+lf"));
        assert_eq!(kind, "draft/multiline");
        assert_eq!(target, "#room");
        let batch_id = open.trim_start_matches('+');
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw(format!("@batch={batch_id} PRIVMSG #room :one"))
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw(format!("@batch={batch_id} PRIVMSG #room :two"))
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Batch(format!("-{batch_id}"), None, None)
        );
    }
}
