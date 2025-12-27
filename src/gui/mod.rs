use std::collections::HashMap;
use std::path::PathBuf;
use egui::{Color32, RichText, ScrollArea, TextEdit, Vec2};
use tokio::sync::mpsc;
use serde::{Deserialize, Serialize};

use crate::irc::{IrcCommand, IrcMessage};
use crate::irc::client::ServerConfig;

// Persistent settings
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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_host: "irc.afternet.org".to_string(),
            server_port: "6697".to_string(),
            use_tls: true,
            accept_invalid_certs: false,
            nickname: String::new(), // Will be generated if empty
            username: "fmirc".to_string(),
            realname: "fmIRC".to_string(),
            password: String::new(),
            auto_join_channels: String::new(),
            set_invisible: true,
            minimize_to_tray: false,
            auto_reconnect: true,
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

// IRC color codes (mIRC standard)
const IRC_COLORS: [Color32; 16] = [
    Color32::WHITE,                      // 0: White
    Color32::BLACK,                      // 1: Black
    Color32::from_rgb(0, 0, 127),        // 2: Blue (navy)
    Color32::from_rgb(0, 147, 0),        // 3: Green
    Color32::from_rgb(255, 0, 0),        // 4: Red
    Color32::from_rgb(127, 0, 0),        // 5: Brown (maroon)
    Color32::from_rgb(156, 0, 156),      // 6: Purple
    Color32::from_rgb(252, 127, 0),      // 7: Orange
    Color32::from_rgb(255, 255, 0),      // 8: Yellow
    Color32::from_rgb(0, 252, 0),        // 9: Light Green
    Color32::from_rgb(0, 147, 147),      // 10: Cyan (teal)
    Color32::from_rgb(0, 255, 255),      // 11: Light Cyan
    Color32::from_rgb(0, 0, 252),        // 12: Light Blue
    Color32::from_rgb(255, 0, 255),      // 13: Pink
    Color32::from_rgb(127, 127, 127),    // 14: Grey
    Color32::from_rgb(210, 210, 210),    // 15: Light Grey
];

#[derive(Clone, Debug)]
struct TextSpan {
    text: String,
    fg_color: Option<Color32>,
    bg_color: Option<Color32>,
    bold: bool,
    underline: bool,
    italic: bool,
}

fn parse_irc_colors(input: &str) -> Vec<TextSpan> {
    let mut spans = Vec::new();
    let mut current_text = String::new();
    let mut fg_color: Option<Color32> = None;
    let mut bg_color: Option<Color32> = None;
    let mut bold = false;
    let mut underline = false;
    let mut italic = false;

    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\x03' => {
                // Color code - push current span if not empty
                if !current_text.is_empty() {
                    spans.push(TextSpan {
                        text: current_text.clone(),
                        fg_color,
                        bg_color,
                        bold,
                        underline,
                        italic,
                    });
                    current_text.clear();
                }

                // Parse foreground color (1-2 digits)
                let mut fg_str = String::new();
                while fg_str.len() < 2 {
                    if let Some(&next) = chars.peek() {
                        if next.is_ascii_digit() {
                            fg_str.push(chars.next().unwrap());
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }

                if let Ok(fg) = fg_str.parse::<usize>() {
                    fg_color = Some(IRC_COLORS[fg % 16]);
                } else {
                    // \x03 with no number resets colors
                    fg_color = None;
                    bg_color = None;
                }

                // Check for background color (comma followed by 1-2 digits)
                if chars.peek() == Some(&',') {
                    chars.next(); // consume comma
                    let mut bg_str = String::new();
                    while bg_str.len() < 2 {
                        if let Some(&next) = chars.peek() {
                            if next.is_ascii_digit() {
                                bg_str.push(chars.next().unwrap());
                            } else {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    if let Ok(bg) = bg_str.parse::<usize>() {
                        bg_color = Some(IRC_COLORS[bg % 16]);
                    }
                }
            }
            '\x02' => {
                // Bold toggle
                if !current_text.is_empty() {
                    spans.push(TextSpan {
                        text: current_text.clone(),
                        fg_color,
                        bg_color,
                        bold,
                        underline,
                        italic,
                    });
                    current_text.clear();
                }
                bold = !bold;
            }
            '\x1F' => {
                // Underline toggle
                if !current_text.is_empty() {
                    spans.push(TextSpan {
                        text: current_text.clone(),
                        fg_color,
                        bg_color,
                        bold,
                        underline,
                        italic,
                    });
                    current_text.clear();
                }
                underline = !underline;
            }
            '\x1D' | '\x16' => {
                // Italic toggle (\x1D is proper italic, \x16 is reverse/italic)
                if !current_text.is_empty() {
                    spans.push(TextSpan {
                        text: current_text.clone(),
                        fg_color,
                        bg_color,
                        bold,
                        underline,
                        italic,
                    });
                    current_text.clear();
                }
                italic = !italic;
            }
            '\x0F' => {
                // Reset all formatting
                if !current_text.is_empty() {
                    spans.push(TextSpan {
                        text: current_text.clone(),
                        fg_color,
                        bg_color,
                        bold,
                        underline,
                        italic,
                    });
                    current_text.clear();
                }
                fg_color = None;
                bg_color = None;
                bold = false;
                underline = false;
                italic = false;
            }
            _ => {
                current_text.push(c);
            }
        }
    }

    // Push remaining text
    if !current_text.is_empty() {
        spans.push(TextSpan {
            text: current_text,
            fg_color,
            bg_color,
            bold,
            underline,
            italic,
        });
    }

    spans
}

/// Split text into URL and non-URL segments
fn split_urls(text: &str) -> Vec<(String, bool)> {
    let mut result = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        // Find the start of a URL
        let url_starts = ["http://", "https://", "www."];
        let mut earliest_url: Option<(usize, &str)> = None;

        for prefix in &url_starts {
            if let Some(pos) = remaining.find(prefix) {
                if earliest_url.is_none() || pos < earliest_url.unwrap().0 {
                    earliest_url = Some((pos, prefix));
                }
            }
        }

        match earliest_url {
            Some((pos, _)) => {
                // Add text before the URL
                if pos > 0 {
                    result.push((remaining[..pos].to_string(), false));
                }

                // Find the end of the URL (first whitespace or end of string)
                let url_start = pos;
                let after_url = &remaining[url_start..];
                let url_end = after_url
                    .find(|c: char| c.is_whitespace() || c == '>' || c == ')' || c == ']' || c == '"' || c == '\'')
                    .unwrap_or(after_url.len());

                let url = &after_url[..url_end];
                result.push((url.to_string(), true));

                remaining = &remaining[url_start + url_end..];
            }
            None => {
                // No more URLs, add the rest as plain text
                result.push((remaining.to_string(), false));
                break;
            }
        }
    }

    result
}

fn render_irc_text(ui: &mut egui::Ui, text: &str, default_color: Color32) {
    let spans = parse_irc_colors(text);

    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for span in spans {
            let color = span.fg_color.unwrap_or(default_color);

            // Split the span text into URL and non-URL parts
            let segments = split_urls(&span.text);

            for (segment, is_url) in segments {
                if is_url {
                    // Render as clickable hyperlink
                    let url = if segment.starts_with("www.") {
                        format!("https://{}", segment)
                    } else {
                        segment.clone()
                    };
                    ui.hyperlink_to(
                        RichText::new(&segment).color(Color32::from_rgb(100, 150, 255)).underline(),
                        &url
                    );
                } else {
                    // Render as regular text with formatting
                    let mut rich_text = RichText::new(&segment).color(color);

                    if span.bold {
                        rich_text = rich_text.strong();
                    }
                    if span.underline {
                        rich_text = rich_text.underline();
                    }
                    if span.italic {
                        rich_text = rich_text.italics();
                    }

                    if let Some(bg) = span.bg_color {
                        rich_text = rich_text.background_color(bg);
                    }

                    ui.label(rich_text);
                }
            }
        }
    });
}

fn current_time_hhmm() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    #[cfg(unix)]
    let (hours, minutes) = {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&t, &mut tm) };
        (tm.tm_hour as u64, tm.tm_min as u64)
    };

    #[cfg(not(unix))]
    let (hours, minutes) = {
        // Fallback to UTC on non-unix (Windows handled by winapi if needed)
        let hours = (secs % 86400) / 3600;
        let minutes = (secs % 3600) / 60;
        (hours, minutes)
    };

    format!("{:02}:{:02}", hours, minutes)
}

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
        Self {
            timestamp: current_time_hhmm(),
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: false,
            is_system: false,
            is_highlight: false,
        }
    }

    pub fn system(content: &str) -> Self {
        Self {
            timestamp: current_time_hhmm(),
            sender: "*".to_string(),
            content: content.to_string(),
            is_action: false,
            is_system: true,
            is_highlight: false,
        }
    }

    pub fn action(sender: &str, content: &str) -> Self {
        Self {
            timestamp: current_time_hhmm(),
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: true,
            is_system: false,
            is_highlight: false,
        }
    }

    pub fn highlighted(sender: &str, content: &str) -> Self {
        Self {
            timestamp: current_time_hhmm(),
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: false,
            is_system: false,
            is_highlight: true,
        }
    }

    pub fn action_highlighted(sender: &str, content: &str) -> Self {
        Self {
            timestamp: current_time_hhmm(),
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: true,
            is_system: false,
            is_highlight: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Channel {
    pub topic: Option<String>,
    pub users: Vec<(String, UserMode)>,  // (nick, mode)
    pub messages: Vec<ChatMessage>,
    pub unread: usize,
}

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

impl Channel {
    pub fn new() -> Self {
        Self {
            topic: None,
            users: Vec::new(),
            messages: Vec::new(),
            unread: 0,
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
        // Sort by mode (ops first) then alphabetically
        self.users.sort_by(|(a_nick, a_mode), (b_nick, b_mode)| {
            a_mode.cmp(b_mode).then_with(|| a_nick.to_lowercase().cmp(&b_nick.to_lowercase()))
        });
    }
}

#[derive(Debug, Clone)]
pub struct ChannelListEntry {
    pub name: String,
    pub user_count: usize,
    pub topic: String,
}

#[derive(Debug, Clone)]
pub struct TabCompletion {
    pub prefix: String,
    pub matches: Vec<String>,
    pub index: usize,
}

pub struct IrcApp {
    // Connection state
    pub connected: bool,
    pub connecting: bool,
    pub my_nick: String,

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
    pub cmd_tx: Option<mpsc::Sender<IrcCommand>>,
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

    // Tab completion
    pub tab_completion: Option<TabCompletion>,

    // User list selection
    pub selected_user: Option<String>,

    // Connection options
    pub auto_join_channels: String,
    pub set_invisible: bool,
    pub minimize_to_tray: bool,
    pub auto_reconnect: bool,

    // Auto-reconnect state
    pub reconnect_attempts: u32,
    pub last_disconnect_time: Option<std::time::Instant>,
    pub connection_lost: bool,
}

impl Default for IrcApp {
    fn default() -> Self {
        Self::with_settings(Settings::load())
    }
}

impl IrcApp {
    fn with_settings(settings: Settings) -> Self {
        let nickname = if settings.nickname.is_empty() {
            format!("fmIRC_{}", rand_suffix())
        } else {
            settings.nickname.clone()
        };

        Self {
            connected: false,
            connecting: false,
            my_nick: String::new(),

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

            tab_completion: None,

            selected_user: None,

            auto_join_channels: settings.auto_join_channels,
            set_invisible: settings.set_invisible,
            minimize_to_tray: settings.minimize_to_tray,
            auto_reconnect: settings.auto_reconnect,

            reconnect_attempts: 0,
            last_disconnect_time: None,
            connection_lost: false,
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
        }
    }

    /// Calculate reconnect delay with exponential backoff (1s, 2s, 4s, 8s, max 60s)
    pub fn get_reconnect_delay(&self) -> std::time::Duration {
        let base_delay = 1u64;
        let max_delay = 60u64;
        let delay = base_delay.saturating_mul(2u64.saturating_pow(self.reconnect_attempts.min(6)));
        std::time::Duration::from_secs(delay.min(max_delay))
    }

    /// Check if we should attempt reconnection now
    pub fn should_reconnect(&self) -> bool {
        if !self.auto_reconnect || !self.connection_lost || self.connecting {
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
        self.connected = false;
        self.connection_lost = true;
        self.last_disconnect_time = Some(std::time::Instant::now());
        self.cmd_tx = None;
        self.msg_rx = None;
    }

    /// Prepare for reconnection attempt
    pub fn start_reconnect(&mut self) {
        self.reconnect_attempts += 1;
        self.connecting = true;
        self.connection_lost = false;
        let delay = self.get_reconnect_delay();
        self.add_server_message(ChatMessage::system(&format!(
            "Reconnecting... (attempt {}, next retry in {:?})",
            self.reconnect_attempts,
            delay
        )));
    }

    fn save_settings(&self) {
        self.get_settings().save();
    }
}

fn rand_suffix() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() % 10000) as u32
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
            password: if self.password.is_empty() { None } else { Some(self.password.clone()) },
        }
    }

    pub fn handle_incoming_message(&mut self, msg: IrcMessage) {
        match &msg.command {
            IrcCommand::Privmsg(target, content) => {
                let sender = msg.get_sender_nick().unwrap_or_else(|| "???".to_string());
                let is_action = content.starts_with("\x01ACTION ") && content.ends_with('\x01');

                // Check if the message mentions our nick (case-insensitive word boundary check)
                let is_highlight = self.check_nick_mention(content);

                let chat_msg = if is_action {
                    let action_text = content
                        .strip_prefix("\x01ACTION ")
                        .and_then(|s| s.strip_suffix('\x01'))
                        .unwrap_or(content);
                    if is_highlight {
                        ChatMessage::action_highlighted(&sender, action_text)
                    } else {
                        ChatMessage::action(&sender, action_text)
                    }
                } else if is_highlight {
                    ChatMessage::highlighted(&sender, content)
                } else {
                    ChatMessage::new(&sender, content)
                };

                // Determine target channel/query
                let target_name = if target.starts_with('#') || target.starts_with('&') {
                    target.clone()
                } else if target.eq_ignore_ascii_case(&self.my_nick) {
                    // Private message to us - use sender as channel
                    sender.clone()
                } else {
                    target.clone()
                };

                self.add_message_to_channel(&target_name, chat_msg);
            }

            IrcCommand::Notice(target, content) => {
                let sender = msg.get_sender_nick().unwrap_or_else(|| "Server".to_string());
                let chat_msg = ChatMessage::system(&format!("-{}- {}", sender, content));

                if target == "*" || !self.connected {
                    self.add_server_message(chat_msg);
                } else {
                    self.add_message_to_channel(target, chat_msg);
                }
            }

            IrcCommand::Join(channel) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                if sender.eq_ignore_ascii_case(&self.my_nick) {
                    // We joined a channel
                    if !self.channels.contains_key(channel) {
                        self.channels.insert(channel.clone(), Channel::new());
                    }
                    self.current_channel = Some(channel.clone());
                    let sys_msg = ChatMessage::system(&format!("Now talking in {}", channel));
                    self.add_message_to_channel(channel, sys_msg);
                } else {
                    let sys_msg = ChatMessage::system(&format!("{} has joined {}", sender, channel));
                    self.add_message_to_channel(channel, sys_msg);
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
                    let sys_msg = ChatMessage::system(&format!(
                        "{} has left {} ({})", sender, channel, reason_str
                    ));
                    self.add_message_to_channel(channel, sys_msg);
                    if let Some(ch) = self.channels.get_mut(channel) {
                        ch.remove_user(&sender);
                    }
                }
            }

            IrcCommand::Quit(reason) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                let reason_str = reason.as_deref().unwrap_or("Quit");
                let sys_msg = ChatMessage::system(&format!("{} has quit ({})", sender, reason_str));

                // Add quit message to all channels the user was in
                for (_, channel) in self.channels.iter_mut() {
                    if channel.has_user(&sender) {
                        channel.messages.push(sys_msg.clone());
                        channel.remove_user(&sender);
                    }
                }
            }

            IrcCommand::Nick(new_nick) => {
                let old_nick = msg.get_sender_nick().unwrap_or_default();
                if old_nick.eq_ignore_ascii_case(&self.my_nick) {
                    self.my_nick = new_nick.clone();
                }
                let sys_msg = ChatMessage::system(&format!(
                    "{} is now known as {}", old_nick, new_nick
                ));
                for (_, channel) in self.channels.iter_mut() {
                    if channel.has_user(&old_nick) {
                        channel.messages.push(sys_msg.clone());
                        channel.rename_user(&old_nick, new_nick);
                    }
                }
            }

            IrcCommand::Topic(channel, topic) => {
                if let Some(ch) = self.channels.get_mut(channel) {
                    ch.topic = topic.clone();
                    let sender = msg.get_sender_nick();
                    let sys_msg = if let Some(s) = sender {
                        ChatMessage::system(&format!(
                            "{} changed the topic to: {}", s, topic.as_deref().unwrap_or("")
                        ))
                    } else {
                        ChatMessage::system(&format!(
                            "Topic: {}", topic.as_deref().unwrap_or("")
                        ))
                    };
                    ch.messages.push(sys_msg);
                }
            }

            IrcCommand::Numeric(num, params) => {
                self.handle_numeric(*num, params);
            }

            IrcCommand::Ping(_) => {
                // Handled automatically in client
            }

            _ => {
                // Log unknown messages
                let sys_msg = ChatMessage::system(&msg.raw);
                self.add_server_message(sys_msg);
            }
        }

        self.scroll_to_bottom = true;
    }

    fn handle_numeric(&mut self, num: u16, params: &[String]) {
        match num {
            1 => {
                // RPL_WELCOME
                self.connected = true;
                self.connecting = false;
                self.reset_reconnect_state(); // Reset reconnect attempts on successful connection
                if let Some(nick) = params.get(0) {
                    self.my_nick = nick.clone();
                }
                let msg = params.get(1).cloned().unwrap_or_else(|| "Welcome!".to_string());
                self.add_server_message(ChatMessage::system(&msg));

                // Set invisible mode if requested
                if self.set_invisible {
                    self.send_command(IrcCommand::Mode(self.my_nick.clone(), Some("+i".to_string()), None));
                    self.add_server_message(ChatMessage::system("Setting user mode +i (invisible)"));
                }

                // Auto-join channels
                let auto_join = self.auto_join_channels.clone();
                if !auto_join.is_empty() {
                    for chan in auto_join.split(',') {
                        let chan = chan.trim();
                        if !chan.is_empty() {
                            let channel = if chan.starts_with('#') || chan.starts_with('&') {
                                chan.to_string()
                            } else {
                                format!("#{}", chan)
                            };
                            self.send_command(IrcCommand::Join(channel.clone()));
                            self.add_server_message(ChatMessage::system(&format!("Auto-joining {}", channel)));
                        }
                    }
                }
            }

            332 => {
                // RPL_TOPIC
                if let (Some(channel), Some(topic)) = (params.get(1), params.get(2)) {
                    if let Some(ch) = self.channels.get_mut(channel) {
                        ch.topic = Some(topic.clone());
                        ch.messages.push(ChatMessage::system(&format!("Topic: {}", topic)));
                    }
                }
            }

            353 => {
                // RPL_NAMREPLY
                if let Some(channel) = params.get(2) {
                    if let Some(names) = params.get(3) {
                        if let Some(ch) = self.channels.get_mut(channel) {
                            for name in names.split_whitespace() {
                                // Parse mode prefix
                                let mode = name.chars().next()
                                    .and_then(UserMode::from_prefix)
                                    .unwrap_or(UserMode::Normal);
                                ch.add_user(name, mode);
                            }
                        }
                    }
                }
            }

            366 => {
                // RPL_ENDOFNAMES
            }

            372 | 375 | 376 => {
                // MOTD
                if let Some(text) = params.last() {
                    self.add_server_message(ChatMessage::system(text));
                }
            }

            433 => {
                // ERR_NICKNAMEINUSE
                let new_nick = format!("{}_", self.my_nick);
                self.my_nick = new_nick.clone();
                if let Some(tx) = &self.cmd_tx {
                    let _ = tx.try_send(IrcCommand::Nick(new_nick));
                }
                self.add_server_message(ChatMessage::system("Nickname in use, trying alternative..."));
            }

            321 => {
                // RPL_LISTSTART - clear old list, start collecting
                self.channel_list.clear();
                self.channel_list_loading = true;
                self.show_channel_list = true;
            }

            322 => {
                // RPL_LIST - params: [client, channel, visible_count, topic]
                if let (Some(channel), Some(count_str)) = (params.get(1), params.get(2)) {
                    let user_count = count_str.parse().unwrap_or(0);
                    let topic = params.get(3).cloned().unwrap_or_default();
                    self.channel_list.push(ChannelListEntry {
                        name: channel.clone(),
                        user_count,
                        topic,
                    });
                }
            }

            323 => {
                // RPL_LISTEND
                self.channel_list_loading = false;
            }

            305 => {
                // RPL_UNAWAY - You are no longer marked as being away
                if let Some(text) = params.get(1) {
                    self.add_server_message(ChatMessage::system(text));
                }
            }

            306 => {
                // RPL_NOWAWAY - You have been marked as being away
                if let Some(text) = params.get(1) {
                    self.add_server_message(ChatMessage::system(text));
                }
            }

            _ => {
                // Show other numerics in server buffer
                let text = params.join(" ");
                self.add_server_message(ChatMessage::system(&format!("[{}] {}", num, text)));
            }
        }
    }

    fn add_message_to_channel(&mut self, channel: &str, msg: ChatMessage) {
        if !self.channels.contains_key(channel) {
            self.channels.insert(channel.to_string(), Channel::new());
        }
        if let Some(ch) = self.channels.get_mut(channel) {
            ch.messages.push(msg);
            if self.current_channel.as_ref() != Some(&channel.to_string()) {
                ch.unread += 1;
            }
        }
    }

    pub fn add_server_message(&mut self, msg: ChatMessage) {
        self.server_messages.push(msg);
        // Increment unread if not viewing server buffer
        if self.current_channel.is_some() {
            self.server_unread += 1;
        }
    }

    pub fn send_command(&mut self, cmd: IrcCommand) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.try_send(cmd);
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
            // Send message to current channel
            let msg = ChatMessage::new(&self.my_nick, &input);
            self.add_message_to_channel(channel, msg);
            self.send_command(IrcCommand::Privmsg(channel.clone(), input));
        }
    }

    fn process_command(&mut self, input: &str) {
        let parts: Vec<&str> = input[1..].splitn(2, ' ').collect();
        let cmd = parts[0].to_uppercase();
        let args = parts.get(1).copied().unwrap_or("");

        match cmd.as_str() {
            "JOIN" | "J" => {
                let channel = if args.starts_with('#') {
                    args.to_string()
                } else {
                    format!("#{}", args)
                };
                self.send_command(IrcCommand::Join(channel));
            }

            "PART" | "LEAVE" => {
                let channel = if args.is_empty() {
                    self.current_channel.clone()
                } else {
                    Some(args.to_string())
                };
                if let Some(ch) = channel {
                    self.send_command(IrcCommand::Part(ch, None));
                }
            }

            "MSG" | "PRIVMSG" | "QUERY" => {
                let msg_parts: Vec<&str> = args.splitn(2, ' ').collect();
                if let (Some(target), Some(message)) = (msg_parts.get(0), msg_parts.get(1)) {
                    self.send_command(IrcCommand::Privmsg(target.to_string(), message.to_string()));
                    let chat_msg = ChatMessage::new(&self.my_nick, message);
                    self.add_message_to_channel(target, chat_msg);
                }
            }

            "ME" => {
                if let Some(channel) = &self.current_channel.clone() {
                    let action = format!("\x01ACTION {}\x01", args);
                    self.send_command(IrcCommand::Privmsg(channel.clone(), action));
                    let msg = ChatMessage::action(&self.my_nick, args);
                    self.add_message_to_channel(channel, msg);
                }
            }

            "NICK" => {
                self.send_command(IrcCommand::Nick(args.to_string()));
            }

            "TOPIC" => {
                if let Some(channel) = &self.current_channel.clone() {
                    if args.is_empty() {
                        self.send_command(IrcCommand::Topic(channel.clone(), None));
                    } else {
                        self.send_command(IrcCommand::Topic(channel.clone(), Some(args.to_string())));
                    }
                }
            }

            "QUIT" => {
                let reason = if args.is_empty() { None } else { Some(args.to_string()) };
                self.send_command(IrcCommand::Quit(reason));
                self.connected = false;
            }

            "AWAY" => {
                if args.is_empty() {
                    // Clear away status
                    self.send_command(IrcCommand::Away(None));
                    self.add_server_message(ChatMessage::system("You are no longer marked as away"));
                } else {
                    self.send_command(IrcCommand::Away(Some(args.to_string())));
                    self.add_server_message(ChatMessage::system(&format!("You are now marked as away: {}", args)));
                }
            }

            "RAW" | "QUOTE" => {
                self.send_command(IrcCommand::Raw(args.to_string()));
            }

            "CLEAR" => {
                if let Some(channel) = &self.current_channel {
                    if let Some(ch) = self.channels.get_mut(channel) {
                        ch.messages.clear();
                    }
                }
            }

            "WHO" => {
                if !args.is_empty() {
                    self.send_command(IrcCommand::Who(args.to_string()));
                } else if let Some(channel) = &self.current_channel.clone() {
                    self.send_command(IrcCommand::Who(channel.clone()));
                }
            }

            "WHOIS" => {
                if !args.is_empty() {
                    self.send_command(IrcCommand::Whois(args.to_string()));
                }
            }

            "NAMES" => {
                if !args.is_empty() {
                    self.send_command(IrcCommand::Names(Some(args.to_string())));
                } else if let Some(channel) = &self.current_channel.clone() {
                    self.send_command(IrcCommand::Names(Some(channel.clone())));
                }
            }

            "LIST" => {
                if args.is_empty() {
                    self.send_command(IrcCommand::List(None));
                } else {
                    self.send_command(IrcCommand::List(Some(args.to_string())));
                }
            }

            "SETTINGS" => {
                self.show_settings = true;
            }

            "HELP" => {
                let help_msgs = vec![
                    "Available commands:",
                    "/join #channel - Join a channel",
                    "/part [#channel] - Leave current or specified channel",
                    "/msg <nick> <message> - Send private message",
                    "/me <action> - Send action message",
                    "/nick <newnick> - Change nickname",
                    "/topic [new topic] - View or set channel topic",
                    "/who [channel/nick] - Query user info",
                    "/whois <nick> - Get detailed user info",
                    "/names [#channel] - List users in channel",
                    "/list [pattern] - List channels",
                    "/settings - Open settings",
                    "/quit [message] - Disconnect from server",
                    "/away [message] - Set away status, or clear if no message",
                    "/clear - Clear current channel messages",
                    "/raw <command> - Send raw IRC command",
                ];
                for msg in help_msgs {
                    self.add_server_message(ChatMessage::system(msg));
                }
            }

            _ => {
                self.add_server_message(ChatMessage::system(&format!("Unknown command: {}", cmd)));
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
        match self.history_index {
            Some(i) => {
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
            None => {} // Not browsing, do nothing
        }
    }

    fn find_nick_completions(&self, prefix: &str) -> Vec<String> {
        if let Some(channel_name) = &self.current_channel {
            if let Some(channel) = self.channels.get(channel_name) {
                let prefix_lower = prefix.to_lowercase();
                let mut matches: Vec<_> = channel
                    .users
                    .iter()
                    .filter(|(nick, _)| nick.to_lowercase().starts_with(&prefix_lower))
                    .map(|(nick, _)| nick.clone())
                    .collect();
                matches.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
                return matches;
            }
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

    /// Check if a message contains a mention of our nick
    fn check_nick_mention(&self, content: &str) -> bool {
        if self.my_nick.is_empty() {
            return false;
        }

        let content_lower = content.to_lowercase();
        let nick_lower = self.my_nick.to_lowercase();

        // Check for nick as a word (with word boundaries)
        for word in content_lower.split(|c: char| !c.is_alphanumeric() && c != '_') {
            if word == nick_lower {
                return true;
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
            channel_names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
            for name in channel_names {
                tabs.push(Some(name));
            }

            // Check Alt+1 through Alt+9
            let alt = i.modifiers.alt;
            if alt {
                for (idx, key) in [
                    egui::Key::Num1, egui::Key::Num2, egui::Key::Num3,
                    egui::Key::Num4, egui::Key::Num5, egui::Key::Num6,
                    egui::Key::Num7, egui::Key::Num8, egui::Key::Num9,
                ].iter().enumerate() {
                    if i.key_pressed(*key) {
                        if idx < tabs.len() {
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
            }

            // Ctrl+W to close current tab
            if i.modifiers.ctrl && i.key_pressed(egui::Key::W) {
                if let Some(channel_name) = self.current_channel.clone() {
                    if channel_name.starts_with('#') || channel_name.starts_with('&') {
                        // Part the channel
                        self.send_command(IrcCommand::Part(channel_name, None));
                    } else {
                        // Close query window
                        self.channels.remove(&channel_name);
                        self.current_channel = self.channels.keys().next().cloned();
                    }
                }
                // If on server buffer, do nothing (can't close it)
            }
        });
    }
}

impl eframe::App for IrcApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Process incoming messages
        let messages: Vec<_> = if let Some(rx) = &mut self.msg_rx {
            std::iter::from_fn(|| rx.try_recv().ok()).collect()
        } else {
            Vec::new()
        };
        for msg in messages {
            self.handle_incoming_message(msg);
        }

        // Handle keyboard shortcuts
        self.handle_keyboard_shortcuts(ctx);

        // Request repaint for real-time updates
        ctx.request_repaint();

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

        // Top panel - toolbar
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("fmIRC");
                ui.separator();

                if self.connected {
                    ui.label(RichText::new("●").color(Color32::GREEN));
                    ui.label(&self.my_nick);
                    ui.label("@");
                    ui.label(&self.server_host);
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
                    } else if self.connected {
                        if ui.button("Disconnect").clicked() {
                            self.send_command(IrcCommand::Quit(Some("fmIRC".to_string())));
                            self.connected = false;
                        }
                    }
                });
            });
        });

        // Left panel - channel list
        egui::SidePanel::left("channels")
            .resizable(true)
            .default_width(150.0)
            .show(ctx, |ui| {
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
                    RichText::new(server_label).strong().color(Color32::from_rgb(255, 255, 100))
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
                                .desired_width(100.0)
                        );
                        if (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                            || ui.button("+").clicked()
                        {
                            if !self.join_channel.is_empty() {
                                let channel = if self.join_channel.starts_with('#') {
                                    self.join_channel.clone()
                                } else {
                                    format!("#{}", self.join_channel)
                                };
                                self.send_command(IrcCommand::Join(channel));
                                self.join_channel.clear();
                            }
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
                        let unread = self.channels.get(&channel_name).map(|c| c.unread).unwrap_or(0);
                        let is_pm = !channel_name.starts_with('#') && !channel_name.starts_with('&');

                        let label = if unread > 0 {
                            format!("{} ({})", channel_name, unread)
                        } else {
                            channel_name.clone()
                        };

                        // Color based on type and unread status
                        let text = if unread > 0 {
                            if is_pm {
                                // PM with unread - orange/red for attention
                                RichText::new(label).strong().color(Color32::from_rgb(255, 150, 50))
                            } else {
                                // Channel with unread - light green
                                RichText::new(label).strong().color(Color32::from_rgb(100, 255, 100))
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

                        // Context menu for channels
                        let chan_for_menu = channel_name.clone();
                        response.context_menu(|ui| {
                            if chan_for_menu.starts_with('#') || chan_for_menu.starts_with('&') {
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
        let mut op_nick: Option<(String, String)> = None;  // (channel, nick)
        let mut voice_nick: Option<(String, String)> = None;
        let mut kick_nick: Option<(String, String)> = None;
        if let Some(channel_name) = &self.current_channel {
            if channel_name.starts_with('#') || channel_name.starts_with('&') {
                let chan_for_context = channel_name.clone();
                egui::SidePanel::right("users")
                    .resizable(true)
                    .default_width(140.0)
                    .show(ctx, |ui| {
                        if let Some(channel) = self.channels.get(&chan_for_context) {
                            ui.heading(format!("Users ({})", channel.users.len()));
                            ui.separator();
                            // Get selected user for this render
                            let current_selected = self.selected_user.clone();
                            let mut new_selected: Option<String> = current_selected.clone();

                            ScrollArea::vertical().show(ui, |ui| {
                                for (nick, mode) in &channel.users {
                                    let prefix = mode.prefix();
                                    let is_selected = current_selected.as_ref() == Some(nick);

                                    // Color based on mode
                                    let color = match mode {
                                        UserMode::Owner => Color32::from_rgb(255, 100, 100),
                                        UserMode::Admin => Color32::from_rgb(255, 150, 100),
                                        UserMode::Op => Color32::from_rgb(100, 200, 100),
                                        UserMode::HalfOp => Color32::from_rgb(100, 200, 200),
                                        UserMode::Voice => Color32::from_rgb(200, 200, 100),
                                        UserMode::Normal => Color32::WHITE,
                                    };

                                    let text = RichText::new(format!("{}{}", prefix, nick)).color(color);
                                    let response = ui.selectable_label(is_selected, text);

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
            self.send_command(IrcCommand::Mode(channel, Some("+o".to_string()), Some(nick)));
        }
        if let Some((channel, nick)) = voice_nick {
            self.send_command(IrcCommand::Mode(channel, Some("+v".to_string()), Some(nick)));
        }
        if let Some((channel, nick)) = kick_nick {
            self.send_command(IrcCommand::Kick(channel, nick, None));
        }

        // Central panel - chat area
        egui::CentralPanel::default().show(ctx, |ui| {
            // Topic bar (with IRC color support)
            if let Some(channel_name) = &self.current_channel {
                if let Some(channel) = self.channels.get(channel_name) {
                    if let Some(topic) = &channel.topic {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Topic:").strong());
                            render_irc_text(ui, topic, Color32::WHITE);
                        });
                        ui.separator();
                    }
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
                            // Use a frame for highlighted messages to show background
                            let frame = if msg.is_highlight {
                                egui::Frame::NONE.fill(Color32::from_rgb(60, 40, 20))
                            } else {
                                egui::Frame::NONE
                            };

                            frame.show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    // Highlight indicator
                                    if msg.is_highlight {
                                        ui.label(RichText::new("*").color(Color32::YELLOW).strong());
                                    }

                                    ui.label(
                                        RichText::new(&msg.timestamp)
                                            .color(Color32::GRAY)
                                            .monospace()
                                    );

                                    if msg.is_system {
                                        // System messages with IRC color support
                                        render_irc_text(ui, &msg.content, Color32::GRAY);
                                    } else if msg.is_action {
                                        // Action messages - highlighted actions use yellow
                                        let action_color = if msg.is_highlight {
                                            Color32::YELLOW
                                        } else {
                                            Color32::from_rgb(150, 100, 200)
                                        };
                                        ui.label(RichText::new(format!("* {} ", msg.sender))
                                            .color(action_color));
                                        render_irc_text(ui, &msg.content, action_color);
                                    } else {
                                        // Regular messages - highlighted messages use yellow text
                                        ui.label(RichText::new(format!("<{}>", msg.sender))
                                            .color(nick_color(&msg.sender)));
                                        let text_color = if msg.is_highlight {
                                            Color32::YELLOW
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
            let input_before = self.input_text.clone();
            ui.horizontal(|ui| {
                let response = ui.add(
                    TextEdit::singleline(&mut self.input_text)
                        .hint_text("Type a message...")
                        .desired_width(ui.available_width() - 60.0)
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
        });
    }
}

impl IrcApp {
    fn show_connect_window(&mut self, ctx: &egui::Context) {
        egui::Window::new("Connect to Server")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Server:");
                    ui.add(TextEdit::singleline(&mut self.server_host).desired_width(200.0));
                });

                ui.horizontal(|ui| {
                    ui.label("Port:");
                    ui.add(TextEdit::singleline(&mut self.server_port).desired_width(80.0));
                    ui.checkbox(&mut self.use_tls, "Use TLS");
                });

                if self.use_tls {
                    ui.horizontal(|ui| {
                        ui.add_space(50.0);
                        ui.checkbox(&mut self.accept_invalid_certs, "Accept invalid certs (insecure)");
                    });
                }

                ui.separator();

                ui.horizontal(|ui| {
                    ui.label("Nickname:");
                    ui.add(TextEdit::singleline(&mut self.nickname).desired_width(150.0));
                });

                ui.horizontal(|ui| {
                    ui.label("Username:");
                    ui.add(TextEdit::singleline(&mut self.username).desired_width(150.0));
                });

                ui.horizontal(|ui| {
                    ui.label("Real name:");
                    ui.add(TextEdit::singleline(&mut self.realname).desired_width(200.0));
                });

                ui.horizontal(|ui| {
                    ui.label("Password:");
                    ui.add(TextEdit::singleline(&mut self.password).password(true).desired_width(150.0));
                    ui.label("(optional)");
                });

                ui.separator();
                ui.heading("On Connect");
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    ui.label("Auto-join:");
                    ui.add(
                        TextEdit::singleline(&mut self.auto_join_channels)
                            .desired_width(200.0)
                            .hint_text("#chan1, #chan2")
                    );
                });

                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.set_invisible, "Set invisible (+i)");
                });
                ui.checkbox(&mut self.auto_reconnect, "Auto-reconnect on disconnect");

                ui.separator();

                ui.horizontal(|ui| {
                    if ui.button("Connect").clicked() {
                        self.save_settings();
                        self.show_connect_dialog = false;
                        self.connecting = true;
                        self.my_nick = self.nickname.clone();
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_connect_dialog = false;
                    }
                });
            });
    }

    fn show_settings_window(&mut self, ctx: &egui::Context) {
        egui::Window::new("Settings")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.heading("Connection Defaults");
                ui.separator();

                ui.horizontal(|ui| {
                    ui.label("Default Server:");
                    ui.add(TextEdit::singleline(&mut self.server_host).desired_width(200.0));
                });

                ui.horizontal(|ui| {
                    ui.label("Default Port:");
                    ui.add(TextEdit::singleline(&mut self.server_port).desired_width(80.0));
                });

                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.use_tls, "Use TLS by default");
                });

                ui.separator();
                ui.heading("Identity");
                ui.separator();

                ui.horizontal(|ui| {
                    ui.label("Nickname:");
                    ui.add(TextEdit::singleline(&mut self.nickname).desired_width(150.0));
                });

                ui.horizontal(|ui| {
                    ui.label("Username:");
                    ui.add(TextEdit::singleline(&mut self.username).desired_width(150.0));
                });

                ui.horizontal(|ui| {
                    ui.label("Real name:");
                    ui.add(TextEdit::singleline(&mut self.realname).desired_width(200.0));
                });

                ui.separator();
                ui.heading("On Connect");
                ui.separator();

                ui.horizontal(|ui| {
                    ui.label("Auto-join:");
                    ui.add(
                        TextEdit::singleline(&mut self.auto_join_channels)
                            .desired_width(200.0)
                            .hint_text("#chan1, #chan2")
                    );
                });

                ui.checkbox(&mut self.set_invisible, "Set invisible (+i)");

                ui.separator();
                ui.heading("Behavior");
                ui.separator();

                ui.checkbox(&mut self.auto_reconnect, "Auto-reconnect on disconnect");

                let tray_response = ui.checkbox(&mut self.minimize_to_tray, "Minimize to system tray");
                if self.minimize_to_tray {
                    tray_response.on_hover_text("System tray support requires platform-specific setup");
                }

                ui.separator();
                ui.heading("About");
                ui.separator();
                ui.label("fmIRC v0.0.1");
                ui.label("A cross-platform IRC client");

                ui.separator();
                if ui.button("Close").clicked() {
                    self.save_settings();
                    self.show_settings = false;
                }
            });
    }

    fn show_channel_list_window(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut join_channel: Option<String> = None;

        egui::Window::new("Channel List")
            .resizable(true)
            .default_size([700.0, 450.0])
            .show(ctx, |ui| {
                // Filter input and status
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    ui.add(TextEdit::singleline(&mut self.channel_list_filter).desired_width(200.0));
                    ui.add_space(10.0);

                    // Count filtered channels
                    let filter = self.channel_list_filter.to_lowercase();
                    let filtered_count = if filter.is_empty() {
                        self.channel_list.len()
                    } else {
                        self.channel_list.iter()
                            .filter(|e| e.name.to_lowercase().contains(&filter)
                                || e.topic.to_lowercase().contains(&filter))
                            .count()
                    };
                    ui.label(format!("{} of {} channels", filtered_count, self.channel_list.len()));

                    if self.channel_list_loading {
                        ui.spinner();
                        ui.label("Loading...");
                    }
                });
                ui.separator();

                // Column widths
                let channel_width = 150.0;
                let users_width = 60.0;
                let topic_width = ui.available_width() - channel_width - users_width - 40.0;

                // Table header
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(channel_width, 20.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| { ui.label(RichText::new("Channel").strong()); }
                    );
                    ui.allocate_ui_with_layout(
                        Vec2::new(users_width, 20.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| { ui.label(RichText::new("Users").strong()); }
                    );
                    ui.add_space(10.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(topic_width, 20.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| { ui.label(RichText::new("Topic").strong()); }
                    );
                });
                ui.separator();

                // Scrollable channel list
                let current_selected = self.channel_list_selected.clone();
                let mut new_selected = current_selected.clone();

                ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .max_height(350.0)
                    .show(ui, |ui| {
                        let filter = self.channel_list_filter.to_lowercase();

                        for entry in &self.channel_list {
                            // Filter by channel name or topic
                            if !filter.is_empty()
                                && !entry.name.to_lowercase().contains(&filter)
                                && !entry.topic.to_lowercase().contains(&filter)
                            {
                                continue;
                            }

                            let is_selected = current_selected.as_ref() == Some(&entry.name);

                            // Build the row text
                            let row_text = format!(
                                "{:<width_c$} {:>width_u$}  {}",
                                entry.name,
                                entry.user_count,
                                if entry.topic.len() > 80 {
                                    format!("{}...", &entry.topic.chars().take(80).collect::<String>())
                                } else {
                                    entry.topic.clone()
                                },
                                width_c = 20,
                                width_u = 5
                            );

                            let response = ui.selectable_label(
                                is_selected,
                                RichText::new(&row_text).color(if is_selected {
                                    Color32::WHITE
                                } else {
                                    Color32::LIGHT_BLUE
                                })
                            );

                            // Single click to select
                            if response.clicked() {
                                new_selected = Some(entry.name.clone());
                            }

                            // Double click to join
                            if response.double_clicked() {
                                join_channel = Some(entry.name.clone());
                            }

                            // Right-click context menu
                            let channel_name = entry.name.clone();
                            let channel_topic = entry.topic.clone();
                            let channel_users = entry.user_count;
                            response.context_menu(|ui| {
                                ui.label(RichText::new(&channel_name).strong());
                                ui.label(format!("{} users", channel_users));
                                if !channel_topic.is_empty() {
                                    ui.separator();
                                    ui.label(RichText::new("Topic:").small());
                                    // Word wrap long topics
                                    ui.label(RichText::new(&channel_topic).small().color(Color32::GRAY));
                                }
                                ui.separator();
                                if ui.button("Join Channel").clicked() {
                                    join_channel = Some(channel_name.clone());
                                    ui.close();
                                }
                                if ui.button("Copy Channel Name").clicked() {
                                    ui.ctx().copy_text(channel_name.clone());
                                    ui.close();
                                }
                            });
                        }
                    });

                // Update selection
                self.channel_list_selected = new_selected;

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Join").clicked() {
                        if let Some(channel) = &self.channel_list_selected {
                            join_channel = Some(channel.clone());
                        }
                    }
                    if ui.button("Refresh").clicked() {
                        self.channel_list.clear();
                        self.channel_list_selected = None;
                        self.send_command(IrcCommand::List(None));
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });

        if close {
            self.show_channel_list = false;
            self.channel_list_selected = None;
        }
        if let Some(channel) = join_channel {
            self.send_command(IrcCommand::Join(channel));
            self.show_channel_list = false;
            self.channel_list_selected = None;
        }
    }
}

fn nick_color(nick: &str) -> Color32 {
    let hash: u32 = nick.bytes().fold(0, |acc, b| acc.wrapping_add(b as u32).wrapping_mul(31));
    let hue = (hash % 360) as f32;

    // Convert HSL to RGB (simplified)
    let c = 0.6;
    let x = c * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let m = 0.3;

    let (r, g, b) = match (hue / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };

    Color32::from_rgb(
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}
