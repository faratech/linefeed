mod commands;
mod dialogs;
mod formatting;
mod helpers;
mod logging;
mod types;

use egui::{Color32, RichText, ScrollArea, TextEdit};
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::irc::client::ServerConfig;
use crate::irc::numerics::*;
use crate::irc::{IrcCommand, IrcMessage};
use formatting::{nick_color, render_irc_text};
use helpers::{format_timestamp, mask_matches, truncate_chars};
pub use types::{
    BanEntry, Channel, ChannelListEntry, ChannelListSort, ChatMessage, NetworkSupport,
    ServerFavorite, Settings, SortDirection, TabCompletion, UserMode,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionIntent {
    None,
    ManualDisconnect,
    ReconnectAfterClose,
}

const IRC_MAX_LINE_BYTES: usize = 510;

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

    // Channels and messages
    pub channels: HashMap<String, Channel>,
    pub current_channel: Option<String>,
    pub server_messages: Vec<ChatMessage>,
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

    nick_retry_attempts: u8,
}

impl Default for IrcApp {
    fn default() -> Self {
        Self::with_settings(Settings::load())
    }
}

impl IrcApp {
    fn with_settings(settings: Settings) -> Self {
        let nickname = if settings.nickname.is_empty() {
            format!("Linefeed_{}", rand_suffix())
        } else {
            settings.nickname.clone()
        };
        let my_nick_lower = nickname.to_lowercase();

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

            channels: HashMap::new(),
            current_channel: None,
            server_messages: Vec::new(),
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
                .map(|s| s.to_lowercase())
                .collect(),
            highlight_words_lower: settings
                .highlight_words
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),

            ignore_list: settings.ignore_list,
            server_favorites: settings.server_favorites,
            auto_perform: settings.auto_perform,
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
            network_support: NetworkSupport::default(),

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
        let sender_lower = sender.to_lowercase();
        for (pattern, pattern_lower) in self.ignore_list.iter().zip(self.ignore_list_lower.iter()) {
            // Check for exact nick match using cached lowercase
            if pattern_lower == &sender_lower {
                return true;
            }
            // Check for wildcard mask match (e.g., *!*@*.spammer.net)
            if (pattern.contains('!') || pattern.contains('@') || pattern.contains('*'))
                && let Some(full_prefix) = prefix
                && mask_matches(pattern_lower, &full_prefix.to_lowercase())
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
        if !self.auto_reconnect || !self.connection_lost || self.connecting {
            return false;
        }

        // Check if we've exceeded max attempts
        if self.reconnect_attempts >= self.max_reconnect_attempts {
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
    }

    /// Send a desktop notification
    pub fn send_notification(&self, title: &str, body: &str, force: bool) {
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

        // Use notify-send on Linux/BSD (usually pre-installed on Linux desktops)
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let title = title.to_string();
            let body = body.to_string();
            std::thread::spawn(move || {
                let _ = std::process::Command::new("notify-send")
                    .args(["-a", "Linefeed", "-t", "5000", &title, &body])
                    .spawn();
            });
        }

        #[cfg(target_os = "macos")]
        {
            let title = title.replace('"', "\\\"");
            let body = body.replace('"', "\\\"");
            std::thread::spawn(move || {
                let script = format!("display notification \"{}\" with title \"{}\"", body, title);
                let _ = std::process::Command::new("osascript")
                    .args(["-e", &script])
                    .spawn();
            });
        }

        // On Windows, flash the taskbar to alert the user
        #[cfg(windows)]
        {
            let _ = (title, body); // Used for logging above
            // Flash taskbar via the tray module
            crate::systray::flash_window();
        }
    }

    fn save_settings(&mut self) {
        self.update_cached_lowercase();
        self.get_settings().save();
    }

    /// Update cached lowercase versions of strings for efficient comparison
    fn update_cached_lowercase(&mut self) {
        self.highlight_words_lower = self
            .highlight_words
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        self.ignore_list_lower = self.ignore_list.iter().map(|s| s.to_lowercase()).collect();
    }

    fn set_my_nick(&mut self, nick: String) {
        self.my_nick = nick;
        self.my_nick_lower = self.my_nick.to_lowercase();
    }

    pub fn is_channel_name(&self, name: &str) -> bool {
        self.network_support.is_channel(name)
    }

    pub fn normalize_channel_name(&self, name: &str) -> String {
        if self.is_channel_name(name) {
            name.to_string()
        } else {
            format!("#{}", name)
        }
    }

    fn reset_connection_support(&mut self) {
        self.network_support = NetworkSupport::default();
        self.nick_retry_attempts = 0;
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
        self.channel_list.clear();
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

impl IrcApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self::default()
    }

    pub fn get_server_config(&self) -> ServerConfig {
        ServerConfig {
            host: self.server_host.clone(),
            port: self.server_port.parse().unwrap_or(6697),
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
        }
    }

    pub fn handle_incoming_message(&mut self, msg: IrcMessage) {
        let _ = msg.get_account();
        let _ = msg.get_batch();
        match &msg.command {
            IrcCommand::Privmsg(target, content) => {
                let sender = msg.get_sender_nick().unwrap_or_else(|| "???".to_string());

                // Check if sender is ignored
                if self.is_ignored(&sender, msg.prefix.as_deref()) {
                    tracing::debug!("Ignoring message from {}", sender);
                    return;
                }

                // Handle CTCP requests (except ACTION which is displayed as a message)
                if self.handle_ctcp_request(&sender, content) {
                    return; // CTCP was handled, don't display as regular message
                }

                let is_action = content.starts_with("\x01ACTION ") && content.ends_with('\x01');

                // Check if the message mentions our nick (case-insensitive word boundary check)
                let is_highlight = self.check_nick_mention(content);

                // Get server-time from IRCv3 tags if available
                let server_time = msg.get_server_time();

                let fmt = &self.timestamp_format;
                let chat_msg = if is_action {
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
                };

                // Determine target channel/query
                let is_pm = !target.starts_with('#')
                    && !target.starts_with('&')
                    && target.eq_ignore_ascii_case(&self.my_nick);
                let target_name = if self.is_channel_name(target) {
                    target.clone()
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
                let is_from_self = sender.eq_ignore_ascii_case(&self.my_nick);
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

                self.add_message_to_channel(&target_name, chat_msg);
            }

            IrcCommand::Notice(target, content) => {
                let sender = msg
                    .get_sender_nick()
                    .unwrap_or_else(|| "Server".to_string());

                // Check if sender is ignored (but not server notices)
                if msg.prefix.is_some() && self.is_ignored(&sender, msg.prefix.as_deref()) {
                    tracing::debug!("Ignoring notice from {}", sender);
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

                let chat_msg = ChatMessage::system(&format!("-{}- {}", sender, content));

                if target == "*" || !self.connected {
                    self.add_server_message(chat_msg);
                } else {
                    // For private notices (target is our nick), route to sender's window
                    // This handles NickServ, X3, and other service responses
                    let is_private = !target.starts_with('#')
                        && !target.starts_with('&')
                        && target.eq_ignore_ascii_case(&self.my_nick);
                    let target_name = if is_private {
                        sender.clone() // Route to sender's query window
                    } else {
                        target.clone()
                    };
                    self.add_message_to_channel(&target_name, chat_msg);
                }
            }

            IrcCommand::Join(channel, _key, account, realname) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                if sender.eq_ignore_ascii_case(&self.my_nick) {
                    // We joined a channel
                    if !self.channels.contains_key(channel) {
                        let mut new_channel = Channel::new();
                        // Check for pending key and store it
                        if let Some(key) = self.pending_channel_keys.remove(&channel.to_lowercase())
                        {
                            new_channel.key = Some(key);
                        }
                        // Load chat history from log file
                        if self.logging_load_history {
                            let history = self.log_manager.load_history(
                                &self.server_host,
                                channel,
                                self.logging_history_lines,
                            );
                            if !history.is_empty() {
                                new_channel.messages.push(ChatMessage::system(&format!(
                                    "--- {} lines of history loaded ---",
                                    history.len()
                                )));
                                new_channel.messages.extend(history);
                            }
                            // Log session start
                            self.log_manager
                                .log_session_start(&self.server_host, channel);
                        }
                        self.channels.insert(channel.clone(), new_channel);
                    }
                    self.current_channel = Some(channel.clone());
                    let sys_msg = ChatMessage::system(&format!("Now talking in {}", channel));
                    self.add_message_to_channel(channel, sys_msg);
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
                    if let Some(ch) = self.channels.get_mut(channel) {
                        ch.add_user(&sender, UserMode::Normal);
                    }
                }
            }

            IrcCommand::Part(channel, reason) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                let reason_str = reason.as_deref().unwrap_or("");
                if sender.eq_ignore_ascii_case(&self.my_nick) {
                    self.channels.remove(channel);
                    if self.current_channel.as_ref() == Some(channel) {
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
                    if let Some(ch) = self.channels.get_mut(channel) {
                        ch.remove_user(&sender);
                    }
                }
            }

            IrcCommand::Kick(channel, kicked, reason) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                let reason_str = reason.as_deref().unwrap_or("");
                let max = self.max_scrollback;
                if kicked.eq_ignore_ascii_case(&self.my_nick) {
                    // We were kicked: keep the tab visible with a notice, but clear
                    // membership state since the server has removed us.
                    if let Some(ch) = self.channels.get_mut(channel) {
                        ch.users.clear();
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
                    if let Some(ch) = self.channels.get_mut(channel) {
                        ch.remove_user(kicked);
                    }
                }
            }

            IrcCommand::Quit(reason) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                let reason_str = reason.as_deref().unwrap_or("Quit");

                // Remove user from all channels, optionally show quit message
                let max = self.max_scrollback;
                for (_, channel) in self.channels.iter_mut() {
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
                if old_nick.eq_ignore_ascii_case(&self.my_nick) {
                    self.set_my_nick(new_nick.clone());
                }
                let sys_msg =
                    ChatMessage::system(&format!("{} is now known as {}", old_nick, new_nick));
                let max = self.max_scrollback;
                for (_, channel) in self.channels.iter_mut() {
                    if channel.has_user(&old_nick) {
                        channel.push_trimmed(sys_msg.clone(), max);
                        channel.rename_user(&old_nick, new_nick);
                    }
                }
            }

            IrcCommand::Topic(channel, topic) => {
                let max = self.max_scrollback;
                if let Some(ch) = self.channels.get_mut(channel) {
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

            IrcCommand::Invite(_target, channel) => {
                let sender = msg
                    .get_sender_nick()
                    .unwrap_or_else(|| "Someone".to_string());
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
                    for (_, channel) in self.channels.iter_mut() {
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
                for (_, channel) in self.channels.iter_mut() {
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
                    for (_, channel) in self.channels.iter_mut() {
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
                    for (_, channel) in self.channels.iter_mut() {
                        if channel.has_user(&sender) {
                            channel.push_trimmed(sys_msg.clone(), max);
                            break; // Only show once
                        }
                    }
                }
            }

            // IRCv3 batch: grouped messages (we just log start/end for now)
            IrcCommand::Batch(reference, batch_type, _params) => {
                if reference.starts_with('+') {
                    // Batch start
                    tracing::debug!("Batch started: {} type={:?}", reference, batch_type);
                } else if reference.starts_with('-') {
                    // Batch end
                    tracing::debug!("Batch ended: {}", reference);
                }
                // Messages within batches have @batch=reference tag, handled normally
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
        for token in params.iter().skip(1) {
            if token.starts_with(':') {
                break;
            }
            if let Some(value) = token.strip_prefix("CHANTYPES=") {
                if !value.is_empty() {
                    self.network_support.channel_types = value.to_string();
                }
            } else if let Some(value) = token.strip_prefix("PREFIX=")
                && let Some((_, prefixes)) = value.rsplit_once(')')
                && !prefixes.is_empty()
            {
                self.network_support.user_prefixes = prefixes.to_string();
            }
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

        if is_channel {
            if let Some(channel) = self.channels.get_mut(target) {
                for c in mode.chars() {
                    match c {
                        '+' | '-' => sign = c,
                        'q' | 'a' | 'o' | 'h' | 'v' => {
                            if let Some(nick) = params.get(param_idx) {
                                param_idx += 1;
                                let new_mode = if sign == '+' {
                                    match c {
                                        'q' => UserMode::Owner,
                                        'a' => UserMode::Admin,
                                        'o' => UserMode::Op,
                                        'h' => UserMode::HalfOp,
                                        'v' => UserMode::Voice,
                                        _ => UserMode::Normal,
                                    }
                                } else {
                                    UserMode::Normal
                                };
                                channel.set_user_mode(nick, new_mode);
                            }
                        }
                        'b' | 'e' | 'I' => {
                            // List modes carry a parameter but are tracked through
                            // their numeric list replies, not as persistent channel modes.
                            if params.get(param_idx).is_some() {
                                param_idx += 1;
                            }
                        }
                        other => {
                            if sign == '+' && matches!(other, 'k' | 'l') {
                                if params.get(param_idx).is_some() {
                                    param_idx += 1;
                                }
                            } else if sign == '-' && other == 'k' && params.get(param_idx).is_some()
                            {
                                param_idx += 1;
                            }
                            channel.apply_channel_mode_flag(sign, other);
                        }
                    }
                }
                channel.mode_params = params.to_vec();
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
        } else if target.eq_ignore_ascii_case(&self.my_nick) {
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

                // Set invisible mode if requested
                if self.set_invisible {
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
                let auto_join = self.auto_join_channels.clone();
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

                // Queue auto-perform commands for execution
                let auto_perform = self.auto_perform.clone();
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
                if let (Some(channel), Some(topic)) = (params.get(1), params.get(2))
                    && let Some(ch) = self.channels.get_mut(channel)
                {
                    ch.topic = Some(topic.clone());
                    ch.messages
                        .push(ChatMessage::system(&format!("Topic: {}", topic)));
                }
            }

            RPL_CHANNELMODEIS => {
                // Format: 324 <nick> <channel> <modes> [<mode params>...]
                if let (Some(channel), Some(modes)) = (params.get(1), params.get(2))
                    && let Some(ch) = self.channels.get_mut(channel)
                {
                    ch.modes = modes.clone();
                    ch.mode_params = params[3..].to_vec();
                }
            }

            RPL_CREATIONTIME => {
                // Format: 329 <nick> <channel> <timestamp>
                if let (Some(channel), Some(ts_str)) = (params.get(1), params.get(2)) {
                    let ts: u64 = ts_str.parse().unwrap_or(0);
                    if let Some(ch) = self.channels.get_mut(channel) {
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
                    if let Some(ch) = self.channels.get_mut(channel) {
                        ch.topic_set_by = Some(setter.clone());
                        ch.topic_set_time = Some(ts);
                    }

                    let time_str = format_timestamp(ts);
                    if let Some(ch) = self.channels.get_mut(channel) {
                        ch.messages.push(ChatMessage::system(&format!(
                            "Topic set by {} on {}",
                            setter, time_str
                        )));
                    }
                }
            }

            RPL_NAMREPLY => {
                if let Some(channel) = params.get(2)
                    && let Some(names) = params.get(3)
                {
                    let prefixes = self.network_support.user_prefixes.clone();
                    if let Some(ch) = self.channels.get_mut(channel) {
                        for name in names.split_whitespace() {
                            // Parse mode prefix
                            let mode = name
                                .chars()
                                .next()
                                .and_then(|c| {
                                    if prefixes.contains(c) {
                                        UserMode::from_prefix(c)
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or(UserMode::Normal);
                            ch.add_user_with_prefixes(name, mode, &prefixes);
                        }
                    }
                }
            }

            RPL_ENDOFNAMES => {
                // After getting the user list, send WHO to get away status
                if let Some(channel) = params.get(1)
                    && self.is_channel_name(channel)
                {
                    self.send_command(IrcCommand::Who(channel.clone()));
                }
            }

            RPL_BANLIST => {
                // Format: 367 <nick> <channel> <banmask> <setter> <timestamp>
                if let (Some(channel), Some(mask), Some(setter), Some(ts_str)) =
                    (params.get(1), params.get(2), params.get(3), params.get(4))
                {
                    let ts: u64 = ts_str.parse().unwrap_or(0);
                    if let Some(ch) = self.channels.get_mut(channel) {
                        ch.bans.push(BanEntry {
                            mask: mask.clone(),
                            set_by: setter.clone(),
                            set_time: ts,
                        });
                    }
                }
            }

            RPL_ENDOFBANLIST => {
                // Format: 368 <nick> <channel> :End of channel ban list
                if let Some(channel) = params.get(1)
                    && let Some(ch) = self.channels.get_mut(channel)
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

                let mut channel = None;
                let mut user_count = 0;
                let mut topic = String::new();

                for (i, p) in params.iter().enumerate() {
                    // Channel name: first param starting with #, &, or just looks like a channel
                    // Relaxed check: if it's not a number and channel is none, take it? No, unsafe.
                    if (p.starts_with('#') || p.starts_with('&')) && channel.is_none() {
                        channel = Some(p.clone());
                        if let Some(last_p) = params.last()
                            && last_p != p
                            && !last_p.chars().all(|c| c.is_ascii_digit())
                        {
                            topic = last_p.clone();
                        }
                    }

                    // User count: any numeric param. Handle '1000' with commas.
                    let clean_p = p.replace(',', "");
                    if i > 0
                        && clean_p.chars().all(|c| c.is_ascii_digit() || c == '+')
                        && !clean_p.is_empty()
                        && let Ok(cnt) = clean_p.trim_start_matches('+').parse::<usize>()
                    {
                        user_count = cnt;
                    }
                }

                if let Some(name) = channel {
                    self.channel_list.push(ChannelListEntry {
                        name,
                        user_count,
                        topic,
                    });
                } else {
                    // Fallback: if we have at least 3 params, assume standard format
                    // [client, channel, count, topic]
                    if params.len() >= 3 {
                        self.channel_list.push(ChannelListEntry {
                            name: params[1].clone(),
                            user_count: params[2].replace(',', "").parse().unwrap_or(0),
                            topic: params.get(3).cloned().unwrap_or_default(),
                        });
                    }
                }
            }

            RPL_LISTEND => {
                self.channel_list_loading = false;
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
                    if let Some(ch) = self.channels.get_mut(channel) {
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
                    self.pending_channel_keys.remove(&channel.to_lowercase());
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

    fn add_message_to_channel(&mut self, channel: &str, msg: ChatMessage) {
        let is_new = !self.channels.contains_key(channel);
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
            // Load chat history for new query windows (non-channels)
            if self.logging_load_history && !self.is_channel_name(channel) {
                let history = self.log_manager.load_history(
                    &self.server_host,
                    channel,
                    self.logging_history_lines,
                );
                if !history.is_empty() {
                    new_channel.messages.push(ChatMessage::system(&format!(
                        "--- {} lines of history loaded ---",
                        history.len()
                    )));
                    new_channel.messages.extend(history);
                }
                self.log_manager
                    .log_session_start(&self.server_host, channel);
            }
            self.channels.insert(channel.to_string(), new_channel);
        }
        // Log message to disk
        self.log_manager
            .log_message(&self.server_host, channel, &msg);

        if let Some(ch) = self.channels.get_mut(channel) {
            ch.messages.push(msg);
            // Limit scrollback
            if ch.messages.len() > self.max_scrollback {
                let excess = ch.messages.len() - self.max_scrollback;
                ch.messages.drain(0..excess);
            }
            if self.current_channel.as_ref() != Some(&channel.to_string()) {
                ch.unread += 1;
            }
        }
    }

    pub fn add_server_message(&mut self, msg: ChatMessage) {
        self.server_messages.push(msg);
        // Limit scrollback
        if self.server_messages.len() > self.max_scrollback {
            let excess = self.server_messages.len() - self.max_scrollback;
            self.server_messages.drain(0..excess);
        }
        // Increment unread if not viewing server buffer
        if self.current_channel.is_some() {
            self.server_unread += 1;
        }
    }

    /// Update a user's away status in all channels they're in
    pub fn update_user_away_status(&mut self, nick: &str, away_msg: Option<String>) {
        for (_, channel) in self.channels.iter_mut() {
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
        format!("{}", cmd).as_bytes().len() <= IRC_MAX_LINE_BYTES
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

    pub fn send_privmsg_text(&mut self, target: &str, text: &str) -> bool {
        let Some(chunks) = Self::split_irc_text_payload("PRIVMSG", target, text, "", "") else {
            self.add_message_to_current(ChatMessage::system_fmt(
                "Message not sent - target name leaves no room for text",
                &self.timestamp_format,
            ));
            return false;
        };
        chunks
            .into_iter()
            .all(|chunk| self.send_command(IrcCommand::Privmsg(target.to_string(), chunk)))
    }

    pub fn send_notice_text(&mut self, target: &str, text: &str) -> bool {
        let Some(chunks) = Self::split_irc_text_payload("NOTICE", target, text, "", "") else {
            self.add_message_to_current(ChatMessage::system_fmt(
                "Notice not sent - target name leaves no room for text",
                &self.timestamp_format,
            ));
            return false;
        };
        chunks
            .into_iter()
            .all(|chunk| self.send_command(IrcCommand::Notice(target.to_string(), chunk)))
    }

    pub fn send_action_text(&mut self, target: &str, text: &str) -> bool {
        let Some(chunks) =
            Self::split_irc_text_payload("PRIVMSG", target, text, "\x01ACTION ", "\x01")
        else {
            self.add_message_to_current(ChatMessage::system_fmt(
                "Action not sent - target name leaves no room for text",
                &self.timestamp_format,
            ));
            return false;
        };
        chunks
            .into_iter()
            .all(|chunk| self.send_command(IrcCommand::Privmsg(target.to_string(), chunk)))
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

        // Save to history (avoid duplicates of last entry)
        if self.command_history.last() != Some(&input) {
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

    fn handle_tab_completion(&mut self) {
        // Find the word being typed at the end of input
        let input = self.input_text.clone();
        let last_space = input.rfind(' ').map(|i| i + 1).unwrap_or(0);
        let prefix = input[last_space..].to_string();

        if prefix.is_empty() {
            return;
        }

        if let Some(ref mut completion) = self.tab_completion {
            // Already completing - check if prefix still matches
            if completion.prefix == prefix && !completion.matches.is_empty() {
                // Cycle to next match
                completion.index = (completion.index + 1) % completion.matches.len();
                let nick = completion.matches[completion.index].clone();
                // Replace the prefix with the nick
                let suffix = if last_space == 0 { ": " } else { " " };
                self.input_text = format!("{}{}{}", &input[..last_space], nick, suffix);
            } else {
                // Prefix changed, start fresh
                self.tab_completion = None;
                self.handle_tab_completion();
            }
        } else {
            // Start new completion
            let matches = self.find_nick_completions(&prefix);
            if !matches.is_empty() {
                let nick = matches[0].clone();
                let suffix = if last_space == 0 { ": " } else { " " };
                self.input_text = format!("{}{}{}", &input[..last_space], nick, suffix);
                self.tab_completion = Some(TabCompletion {
                    prefix,
                    matches,
                    index: 0,
                });
            }
        }
    }

    fn reset_tab_completion(&mut self) {
        self.tab_completion = None;
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
                "\x01VERSION Linefeed v0.0.1 - Rust/egui cross-platform IRC client\x01".to_string(),
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
            "SOURCE" => Some("\x01SOURCE https://github.com/user/linefeed\x01".to_string()),
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
        let content_lower = content.to_lowercase();

        // Check for nick as a word (with word boundaries)
        // Uses cached my_nick_lower to avoid allocation per message
        if !self.my_nick_lower.is_empty() {
            for word in content_lower.split(|c: char| !c.is_alphanumeric() && c != '_') {
                if word == self.my_nick_lower {
                    return true;
                }
            }
        }

        // Check for highlight words using cached lowercase versions
        for highlight in &self.highlight_words_lower {
            for word in content_lower.split(|c: char| !c.is_alphanumeric() && c != '_') {
                if word == highlight {
                    return true;
                }
            }
        }

        false
    }

    /// Handle keyboard shortcuts
    fn handle_keyboard_shortcuts(&mut self, ctx: &egui::Context) {
        // Don't process shortcuts if a dialog is open
        if self.show_connect_dialog || self.show_settings || self.show_channel_list {
            return;
        }

        ctx.input(|i| {
            // Alt+1-9 to switch channels
            // Build ordered list: Server (0), then channels sorted alphabetically
            let mut tabs: Vec<Option<String>> = vec![None]; // Server buffer is index 0 (Alt+1)
            let mut channel_names: Vec<_> = self.channels.keys().cloned().collect();
            channel_names.sort_by_key(|a| a.to_lowercase());
            for name in channel_names {
                tabs.push(Some(name));
            }

            // Check Alt+1 through Alt+9
            let alt = i.modifiers.alt;
            if alt {
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
                        self.current_channel = tabs[idx].clone();
                        self.selected_user = None;
                        // Clear unread for the switched-to channel
                        if let Some(ref channel_name) = self.current_channel {
                            if let Some(ch) = self.channels.get_mut(channel_name) {
                                ch.unread = 0;
                            }
                        } else {
                            self.server_unread = 0;
                        }
                    }
                }
            }

            // Ctrl+W to close current tab
            if i.modifiers.ctrl
                && i.key_pressed(egui::Key::W)
                && let Some(channel_name) = self.current_channel.clone()
            {
                if self.is_channel_name(&channel_name) {
                    // Part the channel
                    self.send_command(IrcCommand::Part(channel_name, None));
                } else {
                    // Close query window
                    self.channels.remove(&channel_name);
                    self.current_channel = self.channels.keys().next().cloned();
                }
            }
            // If on server buffer, do nothing (can't close it)
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
        egui::Panel::top("toolbar").show_inside(root_ui, |ui| {
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
                    ui.label(&self.server_host);

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
            .show_inside(root_ui, |ui| {
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
                            self.current_channel = Some(channel_name.clone());
                            self.selected_user = None; // Clear user selection when switching channels
                            if let Some(ch) = self.channels.get_mut(&channel_name) {
                                ch.unread = 0;
                            }
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
                .show_inside(root_ui, |ui| {
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

                        ScrollArea::vertical().show(ui, |ui| {
                            for user in &channel.users {
                                let nick = &user.nick;
                                let mode = &user.mode;
                                let prefix = mode.prefix();
                                let is_selected = current_selected.as_ref() == Some(nick);
                                let is_away = channel.is_user_away(nick);

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
                                if is_away
                                    && let Some(user) = channel.get_user(nick)
                                    && let Some(away_msg) = &user.away
                                {
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
                                        op_nick = Some((chan_clone.clone(), nick_clone.clone()));
                                        ui.close();
                                    }
                                    if ui.button("Voice (+v)").clicked() {
                                        voice_nick = Some((chan_clone.clone(), nick_clone.clone()));
                                        ui.close();
                                    }
                                    ui.separator();
                                    if ui.button("Kick").clicked() {
                                        kick_nick = Some((chan_clone.clone(), nick_clone.clone()));
                                        ui.close();
                                    }
                                });
                            }
                        });

                        // Update selected user
                        self.selected_user = new_selected;
                    }
                });
        }

        // Handle user list actions
        if let Some(nick) = pm_to_open {
            // Create query window if it doesn't exist
            if !self.channels.contains_key(&nick) {
                self.channels.insert(nick.clone(), Channel::new());
            }
            self.current_channel = Some(nick);
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
        egui::CentralPanel::default().show_inside(root_ui, |ui| {
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
            ScrollArea::vertical()
                .auto_shrink([false; 2])
                .max_height(available_height)
                .stick_to_bottom(self.scroll_to_bottom)
                .show(ui, |ui| {
                    let messages = if let Some(channel_name) = &self.current_channel {
                        self.channels.get(channel_name).map(|c| &c.messages)
                    } else {
                        Some(&self.server_messages)
                    };

                    if let Some(msgs) = messages {
                        for msg in msgs {
                            // Check if message is from self
                            let is_own_msg =
                                !msg.is_system && msg.sender.eq_ignore_ascii_case(&self.my_nick);

                            // Use a frame for highlighted or own messages
                            let frame = if msg.is_highlight {
                                egui::Frame::NONE.fill(Color32::from_rgb(60, 40, 20))
                            } else if is_own_msg {
                                egui::Frame::NONE.fill(Color32::from_rgb(25, 35, 45))
                            } else {
                                egui::Frame::NONE
                            };

                            frame.show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    // Highlight indicator
                                    if msg.is_highlight {
                                        ui.label(
                                            RichText::new("*").color(Color32::YELLOW).strong(),
                                        );
                                    }

                                    ui.label(
                                        RichText::new(&msg.timestamp)
                                            .color(Color32::GRAY)
                                            .monospace(),
                                    );

                                    if msg.is_system {
                                        // System messages with IRC color support
                                        render_irc_text(ui, &msg.content, Color32::GRAY);
                                    } else if msg.is_action {
                                        // Action messages - highlighted actions use yellow, own use cyan
                                        let action_color = if msg.is_highlight {
                                            Color32::YELLOW
                                        } else if is_own_msg {
                                            Color32::from_rgb(100, 180, 220)
                                        } else {
                                            Color32::from_rgb(150, 100, 200)
                                        };
                                        ui.label(
                                            RichText::new(format!("* {} ", msg.sender))
                                                .color(action_color),
                                        );
                                        render_irc_text(ui, &msg.content, action_color);
                                    } else {
                                        // Regular messages
                                        // Own messages use cyan nick, others use computed color
                                        let nick_style = if is_own_msg {
                                            RichText::new(format!("<{}>", msg.sender))
                                                .color(Color32::from_rgb(100, 200, 255))
                                        } else {
                                            RichText::new(format!("<{}>", msg.sender))
                                                .color(nick_color(&msg.sender))
                                        };
                                        ui.label(nick_style);

                                        // Text color: yellow for highlights, light gray for own, white for others
                                        let text_color = if msg.is_highlight {
                                            Color32::YELLOW
                                        } else if is_own_msg {
                                            Color32::from_rgb(200, 210, 220)
                                        } else {
                                            Color32::WHITE
                                        };
                                        render_irc_text(ui, &msg.content, text_color);
                                    }
                                });
                            });
                        }
                    }
                });

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
                    if ui.input(|i| i.key_pressed(egui::Key::Tab)) {
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

    fn test_app() -> IrcApp {
        IrcApp::with_settings(Settings {
            // Keep unit tests from writing chat logs to the real config dir.
            logging_enabled: false,
            ..Settings::default()
        })
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
                "are supported".to_string(),
            ],
        );
        assert!(app.is_channel_name("+ops"));
        assert!(app.is_channel_name("!safe"));
        assert!(!app.is_channel_name("nick"));
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
                .last()
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
    fn incoming_kick_removes_user_and_notes_channel() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.add_user("bob", UserMode::Normal);
        app.channels.insert("#chan".to_string(), channel);

        let msg = IrcMessage::parse(":alice!a@h KICK #chan bob :spamming").unwrap();
        app.handle_incoming_message(msg);

        let ch = &app.channels["#chan"];
        assert!(!ch.has_user("bob"));
        assert!(
            ch.messages
                .iter()
                .any(|m| m.content.contains("bob was kicked from #chan by alice (spamming)"))
        );
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
        assert!(
            ch.messages
                .iter()
                .any(|m| m.content.contains("You were kicked from #chan by alice (bye)"))
        );
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
                .last()
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

        app.process_command("/disconnect");

        assert!(!app.connection_lost);
        assert!(!app.should_reconnect());
        assert!(app.auto_reconnect, "persisted setting must not be flipped");
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
}
