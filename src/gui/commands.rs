//! IRC command processing and help system

use crate::irc::IrcCommand;
use super::{IrcApp, ChatMessage};
use super::helpers::{is_channel, nick_to_mask};

impl IrcApp {
    pub fn process_command(&mut self, input: &str) {
        let parts: Vec<&str> = input[1..].splitn(2, ' ').collect();
        let cmd = parts[0].to_uppercase();
        let args = parts.get(1).copied().unwrap_or("");

        match cmd.as_str() {
            "JOIN" | "J" => {
                let join_parts: Vec<&str> = args.splitn(2, ' ').collect();
                let channel_arg = join_parts.get(0).copied().unwrap_or("");
                let key = join_parts.get(1).map(|k| k.to_string());
                let channel = if channel_arg.starts_with('#') || channel_arg.starts_with('&') {
                    channel_arg.to_string()
                } else {
                    format!("#{}", channel_arg)
                };
                // Store key for use when channel is created
                if let Some(ref k) = key {
                    self.pending_channel_keys.insert(channel.to_lowercase(), k.clone());
                }
                self.send_command(IrcCommand::Join(channel, key));
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

            "MSG" | "PRIVMSG" => {
                let msg_parts: Vec<&str> = args.splitn(2, ' ').collect();
                if let (Some(target), Some(message)) = (msg_parts.get(0), msg_parts.get(1)) {
                    self.send_command(IrcCommand::Privmsg(target.to_string(), message.to_string()));
                    let chat_msg = ChatMessage::new(&self.my_nick, message);
                    self.add_message_to_channel(target, chat_msg);
                }
            }

            "QUERY" | "Q" => {
                let msg_parts: Vec<&str> = args.splitn(2, ' ').collect();
                if let Some(&target) = msg_parts.get(0) {
                    if target.is_empty() {
                        self.add_server_message(ChatMessage::system("Usage: /query <nick> [message]"));
                        return;
                    }
                    // Open query window (loads history via add_message_to_channel)
                    if !self.channels.contains_key(target) {
                        // Create the query window with a system message to trigger history load
                        let sys_msg = ChatMessage::system(&format!("Conversation with {}", target));
                        self.add_message_to_channel(target, sys_msg);
                    }
                    self.current_channel = Some(target.to_string());
                    // If there's a message, send it
                    if let Some(&message) = msg_parts.get(1) {
                        if !message.is_empty() {
                            self.send_command(IrcCommand::Privmsg(target.to_string(), message.to_string()));
                            let chat_msg = ChatMessage::new(&self.my_nick, message);
                            self.add_message_to_channel(target, chat_msg);
                        }
                    }
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
                    self.away_status = None;
                    self.auto_away_triggered = false;
                    self.add_server_message(ChatMessage::system("You are no longer marked as away"));
                } else {
                    let reason = args.to_string();
                    self.send_command(IrcCommand::Away(Some(reason.clone())));
                    self.away_status = Some(reason.clone());
                    self.auto_away_triggered = false;
                    self.add_server_message(ChatMessage::system(&format!("You are now marked as away: {}", reason)));
                }
            }

            "BACK" => {
                self.send_command(IrcCommand::Away(None));
                self.away_status = None;
                self.auto_away_triggered = false;
                self.add_server_message(ChatMessage::system("You are no longer marked as away"));
            }

            "RAW" | "QUOTE" => {
                self.send_command(IrcCommand::Raw(args.to_string()));
            }

            "CTCP" => {
                // /ctcp <nick> <command> [args]
                let parts: Vec<&str> = args.splitn(3, ' ').collect();
                if parts.len() >= 2 {
                    let target = parts[0];
                    let ctcp_cmd = parts[1].to_uppercase();
                    let ctcp_args = parts.get(2).copied().unwrap_or("");
                    let ctcp_msg = if ctcp_args.is_empty() {
                        format!("\x01{}\x01", ctcp_cmd)
                    } else {
                        format!("\x01{} {}\x01", ctcp_cmd, ctcp_args)
                    };
                    self.send_command(IrcCommand::Privmsg(target.to_string(), ctcp_msg));
                    self.add_message_to_current(ChatMessage::system(
                        &format!("[CTCP] Sent {} to {}", ctcp_cmd, target)
                    ));
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /ctcp <nick> <command> [args]"
                    ));
                }
            }

            "VERSION" => {
                // Convenience: /version <nick> is shorthand for /ctcp <nick> VERSION
                if !args.is_empty() {
                    let ctcp_msg = "\x01VERSION\x01".to_string();
                    self.send_command(IrcCommand::Privmsg(args.to_string(), ctcp_msg));
                    self.add_message_to_current(ChatMessage::system(
                        &format!("[CTCP] Sent VERSION to {}", args)
                    ));
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /version <nick>"
                    ));
                }
            }

            "PING" if !args.is_empty() && !args.starts_with('#') => {
                // /ping <nick> - Send CTCP PING to measure latency
                use std::time::{SystemTime, UNIX_EPOCH};
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let ctcp_msg = format!("\x01PING {}\x01", timestamp);
                self.send_command(IrcCommand::Privmsg(args.to_string(), ctcp_msg));
                self.add_message_to_current(ChatMessage::system(
                    &format!("[CTCP] Sent PING to {}", args)
                ));
            }

            "CLEAR" => {
                if let Some(channel) = &self.current_channel {
                    if let Some(ch) = self.channels.get_mut(channel) {
                        ch.messages.clear();
                    }
                } else {
                    self.server_messages.clear();
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
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /whois <nick>"));
                }
            }

            "WHOWAS" => {
                if !args.is_empty() {
                    self.send_command(IrcCommand::Whowas(args.to_string()));
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /whowas <nick>"));
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

            // === Channel Operator Commands ===

            "KICK" | "K" => {
                let parts: Vec<&str> = args.splitn(2, ' ').collect();
                if let Some(nick) = parts.get(0) {
                    if !nick.is_empty() {
                        if let Some(channel) = &self.current_channel.clone() {
                            if is_channel(channel) {
                                let reason = parts.get(1).map(|s| s.to_string());
                                self.send_command(IrcCommand::Kick(channel.clone(), nick.to_string(), reason));
                            }
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /kick <nick> [reason]"));
                }
            }

            "BAN" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone() {
                        if is_channel(channel) {
                            // Convert nick to ban mask if it's just a nick
                            let mask = nick_to_mask(args);
                            self.send_command(IrcCommand::Mode(channel.clone(), Some("+b".to_string()), Some(mask)));
                        }
                    }
                } else {
                    // Show ban list
                    if let Some(channel) = &self.current_channel.clone() {
                        self.send_command(IrcCommand::Mode(channel.clone(), Some("+b".to_string()), None));
                    }
                }
            }

            "UNBAN" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone() {
                        if is_channel(channel) {
                            let mask = nick_to_mask(args);
                            self.send_command(IrcCommand::Mode(channel.clone(), Some("-b".to_string()), Some(mask)));
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /unban <nick|mask>"));
                }
            }

            "KICKBAN" | "KB" => {
                let parts: Vec<&str> = args.splitn(2, ' ').collect();
                if let Some(nick) = parts.get(0) {
                    if !nick.is_empty() {
                        if let Some(channel) = &self.current_channel.clone() {
                            if is_channel(channel) {
                                // Ban first, then kick
                                let mask = format!("{}!*@*", nick);
                                self.send_command(IrcCommand::Mode(channel.clone(), Some("+b".to_string()), Some(mask)));
                                let reason = parts.get(1).map(|s| s.to_string());
                                self.send_command(IrcCommand::Kick(channel.clone(), nick.to_string(), reason));
                            }
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /kickban <nick> [reason]"));
                }
            }

            "OP" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone() {
                        if is_channel(channel) {
                            for nick in args.split_whitespace() {
                                self.send_command(IrcCommand::Mode(channel.clone(), Some("+o".to_string()), Some(nick.to_string())));
                            }
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /op <nick> [nick2] ..."));
                }
            }

            "DEOP" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone() {
                        if is_channel(channel) {
                            for nick in args.split_whitespace() {
                                self.send_command(IrcCommand::Mode(channel.clone(), Some("-o".to_string()), Some(nick.to_string())));
                            }
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /deop <nick> [nick2] ..."));
                }
            }

            "VOICE" | "V" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone() {
                        if is_channel(channel) {
                            for nick in args.split_whitespace() {
                                self.send_command(IrcCommand::Mode(channel.clone(), Some("+v".to_string()), Some(nick.to_string())));
                            }
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /voice <nick> [nick2] ..."));
                }
            }

            "DEVOICE" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone() {
                        if is_channel(channel) {
                            for nick in args.split_whitespace() {
                                self.send_command(IrcCommand::Mode(channel.clone(), Some("-v".to_string()), Some(nick.to_string())));
                            }
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /devoice <nick> [nick2] ..."));
                }
            }

            "HALFOP" | "HOP" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone() {
                        if is_channel(channel) {
                            for nick in args.split_whitespace() {
                                self.send_command(IrcCommand::Mode(channel.clone(), Some("+h".to_string()), Some(nick.to_string())));
                            }
                        }
                    }
                }
            }

            "DEHALFOP" | "DEHOP" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone() {
                        if is_channel(channel) {
                            for nick in args.split_whitespace() {
                                self.send_command(IrcCommand::Mode(channel.clone(), Some("-h".to_string()), Some(nick.to_string())));
                            }
                        }
                    }
                }
            }

            "MODE" | "M" => {
                if !args.is_empty() {
                    let parts: Vec<&str> = args.splitn(3, ' ').collect();
                    let target = parts[0];
                    let mode = parts.get(1).map(|s| s.to_string());
                    let param = parts.get(2).map(|s| s.to_string());
                    self.send_command(IrcCommand::Mode(target.to_string(), mode, param));
                } else if let Some(channel) = &self.current_channel.clone() {
                    // Query channel modes
                    self.send_command(IrcCommand::Mode(channel.clone(), None, None));
                }
            }

            // === Messaging Commands ===

            "NOTICE" | "N" => {
                let parts: Vec<&str> = args.splitn(2, ' ').collect();
                if let (Some(target), Some(message)) = (parts.get(0), parts.get(1)) {
                    self.send_command(IrcCommand::Notice(target.to_string(), message.to_string()));
                    self.add_message_to_current(ChatMessage::system(&format!("-> -{}- {}", target, message)));
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /notice <target> <message>"));
                }
            }

            "ONOTICE" => {
                // Send notice to channel ops
                let message = args;
                if !message.is_empty() {
                    if let Some(channel) = &self.current_channel.clone() {
                        if is_channel(channel) {
                            let target = format!("@{}", channel);
                            self.send_command(IrcCommand::Notice(target.clone(), message.to_string()));
                            self.add_message_to_current(ChatMessage::system(&format!("-> -{}- {}", target, message)));
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /onotice <message>"));
                }
            }

            "AMSG" => {
                // Send message to all channels
                if !args.is_empty() {
                    for channel_name in self.channels.keys().cloned().collect::<Vec<_>>() {
                        if is_channel(&channel_name) {
                            self.send_command(IrcCommand::Privmsg(channel_name.clone(), args.to_string()));
                            let msg = ChatMessage::new(&self.my_nick, args);
                            self.add_message_to_channel(&channel_name, msg);
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /amsg <message>"));
                }
            }

            "AME" => {
                // Send action to all channels
                if !args.is_empty() {
                    let action = format!("\x01ACTION {}\x01", args);
                    for channel_name in self.channels.keys().cloned().collect::<Vec<_>>() {
                        if is_channel(&channel_name) {
                            self.send_command(IrcCommand::Privmsg(channel_name.clone(), action.clone()));
                            let msg = ChatMessage::action(&self.my_nick, args);
                            self.add_message_to_channel(&channel_name, msg);
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /ame <action>"));
                }
            }

            "SAY" => {
                // Force send as message (even if starts with /)
                if let Some(channel) = &self.current_channel.clone() {
                    if !args.is_empty() {
                        let msg = ChatMessage::new(&self.my_nick, args);
                        self.add_message_to_channel(channel, msg);
                        self.send_command(IrcCommand::Privmsg(channel.clone(), args.to_string()));
                    }
                }
            }

            "DESCRIBE" => {
                // Send action to specific target: /describe <target> <action>
                let parts: Vec<&str> = args.splitn(2, ' ').collect();
                if let (Some(target), Some(action_text)) = (parts.get(0), parts.get(1)) {
                    let action = format!("\x01ACTION {}\x01", action_text);
                    self.send_command(IrcCommand::Privmsg(target.to_string(), action));
                    let msg = ChatMessage::action(&self.my_nick, action_text);
                    self.add_message_to_channel(target, msg);
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /describe <target> <action>"));
                }
            }

            // === Channel Management ===

            "INVITE" => {
                let parts: Vec<&str> = args.splitn(2, ' ').collect();
                if let Some(nick) = parts.get(0) {
                    let channel = parts.get(1).map(|s| s.to_string())
                        .or_else(|| self.current_channel.clone())
                        .unwrap_or_default();
                    if !nick.is_empty() && !channel.is_empty() {
                        self.send_command(IrcCommand::Invite(nick.to_string(), channel.clone()));
                        self.add_message_to_current(ChatMessage::system(&format!("Inviting {} to {}", nick, channel)));
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /invite <nick> [#channel]"));
                }
            }

            "CYCLE" | "REJOIN" => {
                let channel = if args.is_empty() {
                    self.current_channel.clone()
                } else {
                    Some(args.to_string())
                };
                if let Some(ch) = channel {
                    if is_channel(&ch) {
                        // Get the stored key for auto-rejoin if available
                        let key = self.channels.get(&ch).and_then(|c| c.key.clone());
                        self.send_command(IrcCommand::Part(ch.clone(), Some("Cycling".to_string())));
                        self.send_command(IrcCommand::Join(ch, key));
                    }
                }
            }

            "KNOCK" => {
                // Request invite to a channel
                if !args.is_empty() {
                    let channel = if args.starts_with('#') { args.to_string() } else { format!("#{}", args) };
                    self.send_command(IrcCommand::Raw(format!("KNOCK {}", channel)));
                    self.add_message_to_current(ChatMessage::system(&format!("Knocking on {}", channel)));
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /knock <#channel>"));
                }
            }

            "CLOSE" | "WC" => {
                // Close current window
                if let Some(channel_name) = self.current_channel.clone() {
                    if is_channel(&channel_name) {
                        self.send_command(IrcCommand::Part(channel_name, None));
                    } else {
                        self.channels.remove(&channel_name);
                        self.current_channel = self.channels.keys().next().cloned();
                    }
                }
            }

            // === Server Info Commands ===

            "MOTD" => {
                let server = if args.is_empty() { None } else { Some(args.to_string()) };
                self.send_command(IrcCommand::Motd(server));
            }

            "TIME" => {
                let server = if args.is_empty() { None } else { Some(args.to_string()) };
                self.send_command(IrcCommand::Time(server));
            }

            "ADMIN" => {
                let server = if args.is_empty() { None } else { Some(args.to_string()) };
                self.send_command(IrcCommand::Admin(server));
            }

            "INFO" => {
                let server = if args.is_empty() { None } else { Some(args.to_string()) };
                self.send_command(IrcCommand::Info(server));
            }

            "LUSERS" => {
                self.send_command(IrcCommand::Lusers);
            }

            "LINKS" => {
                let mask = if args.is_empty() { None } else { Some(args.to_string()) };
                self.send_command(IrcCommand::Links(mask));
            }

            "STATS" => {
                if !args.is_empty() {
                    self.send_command(IrcCommand::Stats(args.to_string()));
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /stats <query> (c=servers, m=commands, o=opers, u=uptime)"));
                }
            }

            "TRACE" => {
                let target = if args.is_empty() { None } else { Some(args.to_string()) };
                self.send_command(IrcCommand::Trace(target));
            }

            "USERHOST" => {
                if !args.is_empty() {
                    self.send_command(IrcCommand::Userhost(args.to_string()));
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /userhost <nick> [nick2] ..."));
                }
            }

            "ISON" => {
                if !args.is_empty() {
                    self.send_command(IrcCommand::Ison(args.to_string()));
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /ison <nick> [nick2] ..."));
                }
            }

            "WALLOPS" => {
                if !args.is_empty() {
                    self.send_command(IrcCommand::Wallops(args.to_string()));
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /wallops <message>"));
                }
            }

            // === Connection Commands ===

            "SERVER" => {
                if !args.is_empty() {
                    // Parse server:port or server port format
                    let parts: Vec<&str> = if args.contains(':') {
                        args.splitn(2, ':').collect()
                    } else {
                        args.splitn(2, ' ').collect()
                    };
                    self.server_host = parts[0].to_string();
                    if let Some(port) = parts.get(1) {
                        self.server_port = port.to_string();
                    }
                    // Disconnect from current server if connected
                    if self.connected {
                        self.send_command(IrcCommand::Quit(Some("Changing servers".to_string())));
                        self.connected = false;
                    }
                    // Trigger reconnect
                    self.connecting = true;
                    self.add_server_message(ChatMessage::system(&format!("Connecting to {}:{}...", self.server_host, self.server_port)));
                } else {
                    self.add_message_to_current(ChatMessage::system(&format!("Current server: {}:{}", self.server_host, self.server_port)));
                }
            }

            "DISCONNECT" => {
                if self.connected {
                    let reason = if args.is_empty() { None } else { Some(args.to_string()) };
                    self.send_command(IrcCommand::Quit(reason));
                    self.connected = false;
                    self.auto_reconnect = false; // Disable auto-reconnect on manual disconnect
                    self.add_server_message(ChatMessage::system("Disconnected from server"));
                }
            }

            "RECONNECT" => {
                if self.connected {
                    self.send_command(IrcCommand::Quit(Some("Reconnecting".to_string())));
                    self.connected = false;
                }
                self.connecting = true;
                self.add_server_message(ChatMessage::system("Reconnecting..."));
            }

            // === Utility Commands ===

            "ECHO" => {
                if !args.is_empty() {
                    self.add_message_to_current(ChatMessage::system(args));
                }
            }

            "IGNORE" => {
                if args.is_empty() {
                    // List ignored users
                    if self.ignore_list.is_empty() {
                        self.add_message_to_current(ChatMessage::system("Ignore list is empty"));
                    } else {
                        let list: Vec<String> = self.ignore_list.iter()
                            .enumerate()
                            .map(|(i, mask)| format!("  {}. {}", i + 1, mask))
                            .collect();
                        self.add_message_to_current(ChatMessage::system("Ignored users/masks:"));
                        for line in list {
                            self.add_message_to_current(ChatMessage::system(&line));
                        }
                    }
                } else {
                    // Add to ignore list
                    let mask = args.to_string();
                    if !self.ignore_list.iter().any(|m| m.eq_ignore_ascii_case(&mask)) {
                        self.ignore_list.push(mask.clone());
                        self.add_message_to_current(ChatMessage::system(&format!("Now ignoring: {}", mask)));
                        // Save settings
                        self.get_settings().save();
                    } else {
                        self.add_message_to_current(ChatMessage::system(&format!("Already ignoring: {}", mask)));
                    }
                }
            }

            "UNIGNORE" => {
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /unignore <nick|mask>"));
                } else {
                    let mask_lower = args.to_lowercase();
                    let before_len = self.ignore_list.len();
                    self.ignore_list.retain(|m| !m.to_lowercase().eq(&mask_lower));
                    if self.ignore_list.len() < before_len {
                        self.add_message_to_current(ChatMessage::system(&format!("No longer ignoring: {}", args)));
                        self.get_settings().save();
                    } else {
                        self.add_message_to_current(ChatMessage::system(&format!("Not in ignore list: {}", args)));
                    }
                }
            }

            "LASTLOG" | "GREP" | "SEARCH" => {
                if !args.is_empty() {
                    let pattern = args.to_lowercase();

                    // Collect matching messages first to avoid borrow issues
                    let matches: Vec<String> = {
                        let messages = if let Some(channel_name) = &self.current_channel {
                            self.channels.get(channel_name).map(|c| &c.messages)
                        } else {
                            Some(&self.server_messages)
                        };

                        if let Some(msgs) = messages {
                            msgs.iter()
                                .rev()
                                .take(500)
                                .filter(|msg| {
                                    msg.content.to_lowercase().contains(&pattern) ||
                                    msg.sender.to_lowercase().contains(&pattern)
                                })
                                .take(50)
                                .map(|msg| format!("[{}] <{}> {}", msg.timestamp, msg.sender, msg.content))
                                .collect()
                        } else {
                            Vec::new()
                        }
                    };

                    // Now display results
                    self.add_message_to_current(ChatMessage::system(&format!("Searching for: {}", args)));
                    for match_line in &matches {
                        self.add_message_to_current(ChatMessage::system(match_line));
                    }
                    self.add_message_to_current(ChatMessage::system(&format!("Found {} matches", matches.len())));
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /lastlog <pattern>"));
                }
            }

            "SVERSION" => {
                // Server VERSION (not CTCP)
                let server = if args.is_empty() { None } else { Some(args.to_string()) };
                self.send_command(IrcCommand::Version(server));
            }

            "PERFORM" => {
                if args.is_empty() {
                    // Show current auto-perform
                    if self.auto_perform.is_empty() {
                        self.add_message_to_current(ChatMessage::system("No auto-perform commands set. Use /perform <command> to add."));
                    } else {
                        let lines: Vec<String> = self.auto_perform.lines()
                            .filter(|line| !line.trim().is_empty())
                            .enumerate()
                            .map(|(i, line)| format!("  {}. {}", i + 1, line))
                            .collect();
                        self.add_message_to_current(ChatMessage::system("Auto-perform commands:"));
                        for line in lines {
                            self.add_message_to_current(ChatMessage::system(&line));
                        }
                    }
                } else if args.eq_ignore_ascii_case("clear") {
                    self.auto_perform.clear();
                    self.get_settings().save();
                    self.add_message_to_current(ChatMessage::system("Auto-perform commands cleared."));
                } else {
                    // Add to auto-perform
                    if !self.auto_perform.is_empty() {
                        self.auto_perform.push('\n');
                    }
                    self.auto_perform.push_str(args);
                    self.get_settings().save();
                    self.add_message_to_current(ChatMessage::system(&format!("Added to auto-perform: {}", args)));
                }
            }

            "SETTINGS" => {
                self.show_settings = true;
            }

            "HELP" | "H" | "?" => {
                self.show_help();
            }

            _ => {
                self.add_message_to_current(ChatMessage::system(&format!("Unknown command: {}. Type /help for list.", cmd)));
            }
        }
    }

    pub fn show_help(&mut self) {
        let help_sections = vec![
            ("=== Basic Commands ===", vec![
                "/join #channel      - Join a channel (alias: /j)",
                "/part [#channel]    - Leave channel (alias: /leave)",
                "/msg <nick> <text>  - Send private message (alias: /query)",
                "/me <action>        - Send action message",
                "/nick <newnick>     - Change nickname",
                "/quit [message]     - Disconnect from server",
            ]),
            ("=== Channel Commands ===", vec![
                "/topic [text]       - View or set topic",
                "/names [#channel]   - List users in channel",
                "/list [pattern]     - List channels",
                "/cycle              - Part and rejoin (alias: /hop, /rejoin)",
                "/invite <nick>      - Invite user to channel",
                "/knock <#channel>   - Request invite",
            ]),
            ("=== Operator Commands ===", vec![
                "/kick <nick> [why]  - Kick user (alias: /k)",
                "/ban <mask>         - Ban user (+b)",
                "/unban <mask>       - Remove ban (-b)",
                "/kickban <nick>     - Ban and kick (alias: /kb)",
                "/op <nick>          - Give ops (+o)",
                "/deop <nick>        - Remove ops (-o)",
                "/voice <nick>       - Give voice (+v)",
                "/devoice <nick>     - Remove voice (-v)",
                "/mode <+/-modes>    - Set channel/user modes",
            ]),
            ("=== Messaging ===", vec![
                "/notice <tgt> <msg> - Send notice (alias: /n)",
                "/onotice <message>  - Notice to channel ops",
                "/amsg <message>     - Message all channels",
                "/ame <action>       - Action to all channels",
                "/say <text>         - Send as message (even if /)",
                "/describe <t> <act> - Send action to target",
            ]),
            ("=== Info Commands ===", vec![
                "/whois <nick>       - User info",
                "/whowas <nick>      - Offline user info",
                "/who <mask>         - Query users",
                "/userhost <nick>    - Get user@host",
                "/ison <nicks>       - Check if online",
                "/motd               - Show MOTD",
                "/lusers             - Network stats",
                "/time               - Server time",
            ]),
            ("=== CTCP ===", vec![
                "/ctcp <n> <cmd>     - Send CTCP request",
                "/version <nick>     - Query version",
                "/ping <nick>        - Measure latency",
            ]),
            ("=== Connection ===", vec![
                "/server <host:port> - Connect to server",
                "/disconnect         - Disconnect",
                "/reconnect          - Reconnect",
                "/away [message]     - Set away message",
                "/back               - Clear away status",
            ]),
            ("=== Utility ===", vec![
                "/clear              - Clear window",
                "/lastlog <pattern>  - Search messages",
                "/ignore [mask]      - List or add to ignore list",
                "/unignore <mask>    - Remove from ignore list",
                "/perform [cmd]      - View/add auto-perform",
                "/echo <text>        - Echo to window",
                "/raw <command>      - Send raw IRC",
                "/settings           - Open settings",
            ]),
            ("=== Shortcuts ===", vec![
                "Alt+1-9             - Switch tabs",
                "Ctrl+W              - Close tab",
                "Up/Down             - Command history",
                "Tab                 - Nick completion",
            ]),
        ];

        for (section, commands) in help_sections {
            self.add_message_to_current(ChatMessage::system(section));
            for cmd in commands {
                self.add_message_to_current(ChatMessage::system(cmd));
            }
        }
    }
}
