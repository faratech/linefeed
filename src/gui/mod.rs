use std::collections::HashMap;
use egui::{Color32, Label, RichText, ScrollArea, Sense, TextEdit, Vec2};
use tokio::sync::mpsc;

use crate::irc::{IrcCommand, IrcMessage};
use crate::irc::client::ServerConfig;

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

fn render_irc_text(ui: &mut egui::Ui, text: &str, default_color: Color32) {
    let spans = parse_irc_colors(text);

    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for span in spans {
            let color = span.fg_color.unwrap_or(default_color);
            let mut rich_text = RichText::new(&span.text).color(color);

            if span.bold {
                rich_text = rich_text.strong();
            }
            if span.underline {
                rich_text = rich_text.underline();
            }
            if span.italic {
                rich_text = rich_text.italics();
            }

            // For background colors, we use a different approach
            if let Some(bg) = span.bg_color {
                rich_text = rich_text.background_color(bg);
            }

            ui.label(rich_text);
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
}

impl ChatMessage {
    pub fn new(sender: &str, content: &str) -> Self {
        Self {
            timestamp: current_time_hhmm(),
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: false,
            is_system: false,
        }
    }

    pub fn system(content: &str) -> Self {
        Self {
            timestamp: current_time_hhmm(),
            sender: "*".to_string(),
            content: content.to_string(),
            is_action: false,
            is_system: true,
        }
    }

    pub fn action(sender: &str, content: &str) -> Self {
        Self {
            timestamp: current_time_hhmm(),
            sender: sender.to_string(),
            content: content.to_string(),
            is_action: true,
            is_system: false,
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

    // Tab completion
    pub tab_completion: Option<TabCompletion>,
}

impl Default for IrcApp {
    fn default() -> Self {
        Self {
            connected: false,
            connecting: false,
            my_nick: String::new(),

            server_host: "irc.afternet.org".to_string(),
            server_port: "6697".to_string(),
            use_tls: true,
            accept_invalid_certs: false,
            nickname: format!("fmIRC_{}", rand_suffix()),
            username: "fmirc".to_string(),
            realname: "fmIRC".to_string(),
            password: String::new(),

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

            tab_completion: None,
        }
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

                let chat_msg = if is_action {
                    let action_text = content
                        .strip_prefix("\x01ACTION ")
                        .and_then(|s| s.strip_suffix('\x01'))
                        .unwrap_or(content);
                    ChatMessage::action(&sender, action_text)
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
                if let Some(nick) = params.get(0) {
                    self.my_nick = nick.clone();
                }
                let msg = params.get(1).cloned().unwrap_or_else(|| "Welcome!".to_string());
                self.add_server_message(ChatMessage::system(&msg));
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

    fn add_server_message(&mut self, msg: ChatMessage) {
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
                            ScrollArea::vertical().show(ui, |ui| {
                                for (nick, mode) in &channel.users {
                                    let prefix = mode.prefix();
                                    let color = match mode {
                                        UserMode::Owner => Color32::from_rgb(255, 100, 100),
                                        UserMode::Admin => Color32::from_rgb(255, 150, 100),
                                        UserMode::Op => Color32::from_rgb(100, 200, 100),
                                        UserMode::HalfOp => Color32::from_rgb(100, 200, 200),
                                        UserMode::Voice => Color32::from_rgb(200, 200, 100),
                                        UserMode::Normal => Color32::WHITE,
                                    };
                                    let response = ui.add(
                                        Label::new(RichText::new(format!("{}{}", prefix, nick)).color(color))
                                            .sense(Sense::click())
                                    );
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
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(&msg.timestamp)
                                        .color(Color32::GRAY)
                                        .monospace()
                                );

                                if msg.is_system {
                                    // System messages with IRC color support
                                    render_irc_text(ui, &msg.content, Color32::GRAY);
                                } else if msg.is_action {
                                    // Action messages with IRC color support
                                    ui.label(RichText::new(format!("* {} ", msg.sender))
                                        .color(Color32::from_rgb(150, 100, 200)));
                                    render_irc_text(ui, &msg.content, Color32::from_rgb(150, 100, 200));
                                } else {
                                    // Regular messages with IRC color support
                                    ui.label(RichText::new(format!("<{}>", msg.sender))
                                        .color(nick_color(&msg.sender)));
                                    render_irc_text(ui, &msg.content, Color32::WHITE);
                                }
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

                ui.horizontal(|ui| {
                    if ui.button("Connect").clicked() {
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
                ui.heading("About");
                ui.separator();
                ui.label("fmIRC v0.0.1");
                ui.label("A cross-platform IRC client");

                ui.separator();
                if ui.button("Close").clicked() {
                    self.show_settings = false;
                }
            });
    }

    fn show_channel_list_window(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut join_channel: Option<String> = None;

        egui::Window::new("Channel List")
            .resizable(true)
            .default_size([600.0, 400.0])
            .show(ctx, |ui| {
                // Filter input and status
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    ui.add(TextEdit::singleline(&mut self.channel_list_filter).desired_width(200.0));
                    ui.label(format!("{} channels", self.channel_list.len()));
                    if self.channel_list_loading {
                        ui.spinner();
                    }
                });
                ui.separator();

                // Table header
                egui::Grid::new("channel_list_header")
                    .num_columns(3)
                    .spacing([20.0, 4.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("Channel").strong());
                        ui.label(RichText::new("Users").strong());
                        ui.label(RichText::new("Topic").strong());
                        ui.end_row();
                    });
                ui.separator();

                // Scrollable channel list
                ScrollArea::vertical()
                    .max_height(300.0)
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

                            let response = ui.horizontal(|ui| {
                                ui.set_min_width(580.0);
                                ui.label(RichText::new(&entry.name).color(Color32::LIGHT_BLUE));
                                ui.add_space(20.0);
                                ui.label(entry.user_count.to_string());
                                ui.add_space(20.0);
                                // Truncate long topics
                                let topic_display = if entry.topic.len() > 60 {
                                    format!("{}...", &entry.topic[..60])
                                } else {
                                    entry.topic.clone()
                                };
                                ui.label(RichText::new(topic_display).color(Color32::GRAY));
                            }).response;

                            if response.double_clicked() {
                                join_channel = Some(entry.name.clone());
                            }
                        }
                    });

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                    if ui.button("Refresh").clicked() {
                        self.send_command(IrcCommand::List(None));
                    }
                });
            });

        if close {
            self.show_channel_list = false;
        }
        if let Some(channel) = join_channel {
            self.send_command(IrcCommand::Join(channel));
            self.show_channel_list = false;
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
