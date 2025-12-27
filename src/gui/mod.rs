use std::collections::HashMap;
use egui::{Color32, RichText, ScrollArea, TextEdit, Vec2};
use tokio::sync::mpsc;

use crate::irc::{IrcCommand, IrcMessage};
use crate::irc::client::ServerConfig;

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

            input_text: String::new(),
            join_channel: String::new(),

            cmd_tx: None,
            msg_rx: None,

            show_settings: false,
            scroll_to_bottom: true,
            show_connect_dialog: true,
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
                    self.server_messages.push(chat_msg);
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
                self.server_messages.push(sys_msg);
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
                self.server_messages.push(ChatMessage::system(&msg));
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
                    self.server_messages.push(ChatMessage::system(text));
                }
            }

            433 => {
                // ERR_NICKNAMEINUSE
                let new_nick = format!("{}_", self.my_nick);
                self.my_nick = new_nick.clone();
                if let Some(tx) = &self.cmd_tx {
                    let _ = tx.try_send(IrcCommand::Nick(new_nick));
                }
                self.server_messages.push(ChatMessage::system("Nickname in use, trying alternative..."));
            }

            _ => {
                // Show other numerics in server buffer
                let text = params.join(" ");
                self.server_messages.push(ChatMessage::system(&format!("[{}] {}", num, text)));
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
                    self.server_messages.push(ChatMessage::system(msg));
                }
            }

            _ => {
                self.server_messages.push(ChatMessage::system(&format!("Unknown command: {}", cmd)));
            }
        }
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

                // Server buffer
                let server_selected = self.current_channel.is_none();
                if ui.selectable_label(server_selected, "Server").clicked() {
                    self.current_channel = None;
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
                ScrollArea::vertical().show(ui, |ui| {
                    let channels: Vec<_> = self.channels.keys().cloned().collect();
                    for channel_name in channels {
                        let is_selected = self.current_channel.as_ref() == Some(&channel_name);
                        let unread = self.channels.get(&channel_name).map(|c| c.unread).unwrap_or(0);

                        let label = if unread > 0 {
                            format!("{} ({})", channel_name, unread)
                        } else {
                            channel_name.clone()
                        };

                        let text = if unread > 0 {
                            RichText::new(label).strong()
                        } else {
                            RichText::new(label)
                        };

                        if ui.selectable_label(is_selected, text).clicked() {
                            self.current_channel = Some(channel_name.clone());
                            if let Some(ch) = self.channels.get_mut(&channel_name) {
                                ch.unread = 0;
                            }
                        }
                    }
                });
            });

        // Right panel - user list (only for channels)
        if let Some(channel_name) = &self.current_channel {
            if channel_name.starts_with('#') || channel_name.starts_with('&') {
                egui::SidePanel::right("users")
                    .resizable(true)
                    .default_width(140.0)
                    .show(ctx, |ui| {
                        if let Some(channel) = self.channels.get(channel_name) {
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
                                    ui.label(RichText::new(format!("{}{}", prefix, nick)).color(color));
                                }
                            });
                        }
                    });
            }
        }

        // Central panel - chat area
        egui::CentralPanel::default().show(ctx, |ui| {
            // Topic bar
            if let Some(channel_name) = &self.current_channel {
                if let Some(channel) = self.channels.get(channel_name) {
                    if let Some(topic) = &channel.topic {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Topic:").strong());
                            ui.label(topic);
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
                                    ui.label(RichText::new(&msg.content).color(Color32::GRAY).italics());
                                } else if msg.is_action {
                                    ui.label(RichText::new(format!("* {} {}", msg.sender, msg.content))
                                        .color(Color32::from_rgb(150, 100, 200)));
                                } else {
                                    ui.label(RichText::new(format!("<{}>", msg.sender))
                                        .color(nick_color(&msg.sender)));
                                    ui.label(&msg.content);
                                }
                            });
                        }
                    }
                });

            // Input area
            ui.separator();
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

                if ui.button("Send").clicked() {
                    self.process_input();
                }
            });
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
