//! IRC command processing and help system

use super::helpers::{is_channel, nick_to_mask};
use super::{ChatMessage, IrcApp};
use crate::irc::IrcCommand;

impl IrcApp {
    pub fn process_command(&mut self, input: &str) {
        let parts: Vec<&str> = input[1..].splitn(2, ' ').collect();
        let cmd = parts[0].to_uppercase();
        // trim_start: a doubled space after the command ("/msg  alice hi") must
        // not produce an empty first argument (which silently sent messages to
        // an empty target and opened a phantom blank-named tab).
        let args = parts.get(1).copied().unwrap_or("").trim_start();

        match cmd.as_str() {
            "JOIN" | "J" => {
                let join_parts: Vec<&str> = args.splitn(2, ' ').collect();
                let channel_arg = join_parts.first().copied().unwrap_or("");
                let key = join_parts.get(1).map(|k| k.to_string());
                if channel_arg.is_empty() {
                    self.add_message_to_current(ChatMessage::system_fmt(
                        "Usage: /join <#channel> [key]",
                        &self.timestamp_format,
                    ));
                    return;
                }
                let channel = self.normalize_channel_name(channel_arg);
                // Store key for use when channel is created
                if let Some(ref k) = key {
                    self.pending_channel_keys
                        .insert(channel.to_lowercase(), k.clone());
                }
                self.send_command(IrcCommand::Join(channel, key, None, None));
            }

            "PART" | "LEAVE" => {
                // Forms: "/part", "/part #chan", "/part #chan reason", "/part reason".
                // Split off a leading channel token; otherwise treat args as a reason
                // for the current channel (so the channel field never contains spaces).
                let (channel, inline_reason) = if args.is_empty() {
                    (self.current_channel.clone(), None)
                } else {
                    let mut it = args.splitn(2, ' ');
                    let first = it.next().unwrap_or("");
                    if is_channel(first) {
                        (Some(first.to_string()), it.next().map(|r| r.to_string()))
                    } else {
                        (self.current_channel.clone(), Some(args.to_string()))
                    }
                };
                if let Some(ch) = channel {
                    // Inline reason wins, else the configured part message (if any).
                    let reason = inline_reason.or_else(|| {
                        if self.part_message.is_empty() {
                            None
                        } else {
                            Some(self.part_message.clone())
                        }
                    });
                    if !self.send_command(IrcCommand::Part(ch.clone(), reason)) {
                        // Disconnected: the server can't confirm the part, so
                        // close the stale channel window locally instead of
                        // leaving a tab that cannot be removed.
                        self.channels.remove(&ch);
                        if self.current_channel.as_ref() == Some(&ch) {
                            self.current_channel = self.channels.keys().next().cloned();
                        }
                    }
                }
            }

            "MSG" | "PRIVMSG" => {
                let msg_parts: Vec<&str> = args.splitn(2, ' ').collect();
                if let (Some(target), Some(message)) = (msg_parts.first(), msg_parts.get(1)) {
                    if self.connected && self.send_privmsg_text(target, message) {
                        let chat_msg =
                            ChatMessage::new_fmt(&self.my_nick, message, &self.timestamp_format);
                        self.add_message_to_channel(target, chat_msg);
                    } else {
                        self.add_message_to_current(ChatMessage::system_fmt(
                            "Not connected - message not sent",
                            &self.timestamp_format,
                        ));
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /msg <target> <message>",
                    ));
                }
            }

            "QUERY" | "Q" => {
                let msg_parts: Vec<&str> = args.splitn(2, ' ').collect();
                if let Some(&target) = msg_parts.first() {
                    if target.is_empty() {
                        self.add_server_message(ChatMessage::system(
                            "Usage: /query <nick> [message]",
                        ));
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
                    if let Some(&message) = msg_parts.get(1)
                        && !message.is_empty()
                    {
                        if self.connected && self.send_privmsg_text(target, message) {
                            let chat_msg = ChatMessage::new_fmt(
                                &self.my_nick,
                                message,
                                &self.timestamp_format,
                            );
                            self.add_message_to_channel(target, chat_msg);
                        } else {
                            self.add_message_to_current(ChatMessage::system_fmt(
                                "Not connected - message not sent",
                                &self.timestamp_format,
                            ));
                        }
                    }
                }
            }

            "ME" => {
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /me <action>"));
                } else if let Some(channel) = &self.current_channel.clone() {
                    if self.connected && self.send_action_text(channel, args) {
                        let msg =
                            ChatMessage::action_fmt(&self.my_nick, args, &self.timestamp_format);
                        self.add_message_to_channel(channel, msg);
                    } else {
                        self.add_message_to_current(ChatMessage::system_fmt(
                            "Not connected - action not sent",
                            &self.timestamp_format,
                        ));
                    }
                }
            }

            "SLAP" => {
                // Classic IRC slap action
                if let Some(channel) = &self.current_channel.clone() {
                    let target = if args.is_empty() { "everyone" } else { args };
                    let action_text = format!("slaps {} around a bit with a large trout", target);
                    if self.connected && self.send_action_text(channel, &action_text) {
                        let msg = ChatMessage::action_fmt(
                            &self.my_nick,
                            &action_text,
                            &self.timestamp_format,
                        );
                        self.add_message_to_channel(channel, msg);
                    } else {
                        self.add_message_to_current(ChatMessage::system_fmt(
                            "Not connected - action not sent",
                            &self.timestamp_format,
                        ));
                    }
                }
            }

            "NICK" => {
                // NICK takes a single token; reject empty and ignore trailing words.
                let new_nick = args.split_whitespace().next().unwrap_or("");
                if new_nick.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /nick <newnick>"));
                } else {
                    self.send_command(IrcCommand::Nick(new_nick.to_string()));
                }
            }

            "TOPIC" => {
                if let Some(channel) = &self.current_channel.clone() {
                    if args.is_empty() {
                        self.send_command(IrcCommand::Topic(channel.clone(), None));
                    } else {
                        self.send_command(IrcCommand::Topic(
                            channel.clone(),
                            Some(args.to_string()),
                        ));
                    }
                }
            }

            "QUIT" => {
                // Use explicit message, or custom quit message, or default
                let reason = if !args.is_empty() {
                    Some(args.to_string())
                } else if !self.quit_message.is_empty() {
                    Some(self.quit_message.clone())
                } else {
                    Some("Linefeed".to_string())
                };
                self.request_manual_disconnect(reason);
            }

            "AWAY" => {
                if args.is_empty() {
                    // Clear away status - only track the change if it was sent,
                    // otherwise the client claims a state the server never saw.
                    if self.send_command(IrcCommand::Away(None)) {
                        self.away_status = None;
                        self.auto_away_triggered = false;
                        // Update our own status in all channels
                        let my_nick = self.my_nick.clone();
                        self.update_user_away_status(&my_nick, None);
                        self.add_server_message(ChatMessage::system(
                            "You are no longer marked as away",
                        ));
                    } else {
                        self.add_server_message(ChatMessage::system(
                            "Not connected - away status not changed",
                        ));
                    }
                } else {
                    let reason = args.to_string();
                    if self.send_command(IrcCommand::Away(Some(reason.clone()))) {
                        self.away_status = Some(reason.clone());
                        self.auto_away_triggered = false;
                        // Update our own status in all channels
                        let my_nick = self.my_nick.clone();
                        self.update_user_away_status(&my_nick, Some(reason.clone()));
                        self.add_server_message(ChatMessage::system(&format!(
                            "You are now marked as away: {}",
                            reason
                        )));
                    } else {
                        self.add_server_message(ChatMessage::system(
                            "Not connected - away status not changed",
                        ));
                    }
                }
            }

            "BACK" => {
                if self.send_command(IrcCommand::Away(None)) {
                    self.away_status = None;
                    self.auto_away_triggered = false;
                    // Update our own status in all channels
                    let my_nick = self.my_nick.clone();
                    self.update_user_away_status(&my_nick, None);
                    self.add_server_message(ChatMessage::system(
                        "You are no longer marked as away",
                    ));
                } else {
                    self.add_server_message(ChatMessage::system(
                        "Not connected - away status not changed",
                    ));
                }
            }

            // === Network Service Shortcuts ===
            "NS" | "NICKSERV" => {
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /ns <command> [args]"));
                } else {
                    self.send_service_message("NickServ", args);
                }
            }

            "CS" | "CHANSERV" => {
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /cs <command> [args]"));
                } else {
                    self.send_service_message("ChanServ", args);
                }
            }

            "MS" | "MEMOSERV" => {
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /ms <command> [args]"));
                } else {
                    self.send_service_message("MemoServ", args);
                }
            }

            "HS" | "HOSTSERV" => {
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /hs <command> [args]"));
                } else {
                    self.send_service_message("HostServ", args);
                }
            }

            "OS" | "OPERSERV" => {
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /os <command> [args]"));
                } else {
                    self.send_service_message("OperServ", args);
                }
            }

            "BS" | "BOTSERV" => {
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /bs <command> [args]"));
                } else {
                    self.send_service_message("BotServ", args);
                }
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
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[CTCP] Sent {} to {}",
                        ctcp_cmd, target
                    )));
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /ctcp <nick> <command> [args]",
                    ));
                }
            }

            "VERSION" => {
                // Convenience: /version <nick> is shorthand for /ctcp <nick> VERSION
                if !args.is_empty() {
                    let ctcp_msg = "\x01VERSION\x01".to_string();
                    self.send_command(IrcCommand::Privmsg(args.to_string(), ctcp_msg));
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[CTCP] Sent VERSION to {}",
                        args
                    )));
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /version <nick>"));
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
                self.add_message_to_current(ChatMessage::system(&format!(
                    "[CTCP] Sent PING to {}",
                    args
                )));
            }

            // Catch the cases the guarded PING arm above rejects (empty target, or a
            // channel-like target) instead of falling through to "Unknown command".
            "PING" => {
                self.add_message_to_current(ChatMessage::system("Usage: /ping <nick>"));
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
                // Prepare the channel list dialog
                self.channel_list.clear();
                self.channel_list_dirty = true;
                self.channel_list_loading = true;
                self.show_channel_list = true;

                // Use configurable min users filter to avoid flooding on large networks
                // Users can override with /list * or /list <filter>
                if args.is_empty() {
                    // >N means "more than N users", so for min 5, send >4
                    if self.list_min_users >= 1 {
                        self.send_command(IrcCommand::List(Some(format!(
                            ">{}",
                            self.list_min_users.saturating_sub(1)
                        ))));
                    } else {
                        self.send_command(IrcCommand::List(None));
                    }
                } else if args == "*" {
                    // Allow /list * to get all channels (use at your own risk)
                    self.send_command(IrcCommand::List(None));
                } else {
                    self.send_command(IrcCommand::List(Some(args.to_string())));
                }
            }

            // === Channel Operator Commands ===
            "KICK" | "K" => {
                let parts: Vec<&str> = args.splitn(2, ' ').collect();
                let nick = parts.first().copied().unwrap_or("");
                if nick.is_empty() {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /kick <nick> [reason]",
                    ));
                } else if let Some(channel) = &self.current_channel.clone()
                    && is_channel(channel)
                {
                    let reason = parts.get(1).map(|s| s.to_string());
                    self.send_command(IrcCommand::Kick(channel.clone(), nick.to_string(), reason));
                }
            }

            "BAN" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone()
                        && is_channel(channel)
                    {
                        // Convert nick to ban mask if it's just a nick
                        let mask = nick_to_mask(args);
                        self.send_command(IrcCommand::Mode(
                            channel.clone(),
                            Some("+b".to_string()),
                            vec![mask],
                        ));
                    }
                } else {
                    // Show ban list
                    if let Some(channel) = &self.current_channel.clone() {
                        self.send_command(IrcCommand::Mode(
                            channel.clone(),
                            Some("+b".to_string()),
                            Vec::new(),
                        ));
                    }
                }
            }

            "UNBAN" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone()
                        && is_channel(channel)
                    {
                        let mask = nick_to_mask(args);
                        self.send_command(IrcCommand::Mode(
                            channel.clone(),
                            Some("-b".to_string()),
                            vec![mask],
                        ));
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /unban <nick|mask>"));
                }
            }

            "KICKBAN" | "KB" => {
                let parts: Vec<&str> = args.splitn(2, ' ').collect();
                let target = parts.first().copied().unwrap_or("");
                if target.is_empty() {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /kickban <nick> [reason]",
                    ));
                } else if let Some(channel) = &self.current_channel.clone()
                    && is_channel(channel)
                {
                    // Ban first, then kick. nick_to_mask passes a full/partial
                    // hostmask through and completes a bare nick to nick!*@*
                    // (blindly appending "!*@*" to a mask built a garbage ban).
                    let is_mask =
                        target.contains('!') || target.contains('@') || target.contains('*');
                    let mask = nick_to_mask(target);
                    self.send_command(IrcCommand::Mode(
                        channel.clone(),
                        Some("+b".to_string()),
                        vec![mask],
                    ));
                    if is_mask {
                        // KICK takes a nick; a hostmask has none to kick.
                        self.add_message_to_current(ChatMessage::system(
                            "Ban set. /kickban with a hostmask cannot kick - use /kick <nick>.",
                        ));
                    } else {
                        let reason = parts.get(1).map(|s| s.to_string());
                        self.send_command(IrcCommand::Kick(
                            channel.clone(),
                            target.to_string(),
                            reason,
                        ));
                    }
                }
            }

            "OP" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone()
                        && is_channel(channel)
                    {
                        for nick in args.split_whitespace() {
                            self.send_command(IrcCommand::Mode(
                                channel.clone(),
                                Some("+o".to_string()),
                                vec![nick.to_string()],
                            ));
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /op <nick> [nick2] ...",
                    ));
                }
            }

            "DEOP" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone()
                        && is_channel(channel)
                    {
                        for nick in args.split_whitespace() {
                            self.send_command(IrcCommand::Mode(
                                channel.clone(),
                                Some("-o".to_string()),
                                vec![nick.to_string()],
                            ));
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /deop <nick> [nick2] ...",
                    ));
                }
            }

            "VOICE" | "V" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone()
                        && is_channel(channel)
                    {
                        for nick in args.split_whitespace() {
                            self.send_command(IrcCommand::Mode(
                                channel.clone(),
                                Some("+v".to_string()),
                                vec![nick.to_string()],
                            ));
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /voice <nick> [nick2] ...",
                    ));
                }
            }

            "DEVOICE" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone()
                        && is_channel(channel)
                    {
                        for nick in args.split_whitespace() {
                            self.send_command(IrcCommand::Mode(
                                channel.clone(),
                                Some("-v".to_string()),
                                vec![nick.to_string()],
                            ));
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /devoice <nick> [nick2] ...",
                    ));
                }
            }

            "HALFOP" | "HOP" => {
                if !args.is_empty()
                    && let Some(channel) = &self.current_channel.clone()
                    && is_channel(channel)
                {
                    for nick in args.split_whitespace() {
                        self.send_command(IrcCommand::Mode(
                            channel.clone(),
                            Some("+h".to_string()),
                            vec![nick.to_string()],
                        ));
                    }
                }
            }

            "DEHALFOP" | "DEHOP" => {
                if !args.is_empty()
                    && let Some(channel) = &self.current_channel.clone()
                    && is_channel(channel)
                {
                    for nick in args.split_whitespace() {
                        self.send_command(IrcCommand::Mode(
                            channel.clone(),
                            Some("-h".to_string()),
                            vec![nick.to_string()],
                        ));
                    }
                }
            }

            "MODE" | "M" => {
                if !args.is_empty() {
                    let parts: Vec<&str> = args.splitn(3, ' ').collect();
                    let target = parts[0];
                    let mode = parts.get(1).map(|s| s.to_string());
                    let params = parts
                        .get(2)
                        .map(|s| s.split_whitespace().map(str::to_string).collect())
                        .unwrap_or_default();
                    self.send_command(IrcCommand::Mode(target.to_string(), mode, params));
                } else if let Some(channel) = &self.current_channel.clone() {
                    // Query channel modes
                    self.send_command(IrcCommand::Mode(channel.clone(), None, Vec::new()));
                }
            }

            "UMODE" => {
                // Set or query user modes
                if args.is_empty() {
                    // Query own modes
                    self.send_command(IrcCommand::Mode(self.my_nick.clone(), None, Vec::new()));
                } else {
                    // Set modes on self
                    self.send_command(IrcCommand::Mode(
                        self.my_nick.clone(),
                        Some(args.to_string()),
                        Vec::new(),
                    ));
                }
            }

            "SILENCE" => {
                // Server-side ignore (supported by some networks)
                if args.is_empty() {
                    // List silence entries
                    self.send_command(IrcCommand::Raw("SILENCE".to_string()));
                } else if args.starts_with('-') || args.starts_with('+') {
                    // Add/remove silence entry
                    self.send_command(IrcCommand::Raw(format!("SILENCE {}", args)));
                } else {
                    // Assume adding
                    self.send_command(IrcCommand::Raw(format!("SILENCE +{}", args)));
                }
            }

            "ACCEPT" => {
                // Manage callerid accept list (mode +g)
                if args.is_empty() {
                    // List accepted nicks
                    self.send_command(IrcCommand::Raw("ACCEPT *".to_string()));
                } else {
                    self.send_command(IrcCommand::Raw(format!("ACCEPT {}", args)));
                }
            }

            // === Messaging Commands ===
            "NOTICE" | "N" => {
                let parts: Vec<&str> = args.splitn(2, ' ').collect();
                if let (Some(target), Some(message)) = (parts.first(), parts.get(1)) {
                    if self.connected && self.send_notice_text(target, message) {
                        self.add_message_to_current(ChatMessage::system_fmt(
                            &format!("-> -{}- {}", target, message),
                            &self.timestamp_format,
                        ));
                    } else {
                        self.add_message_to_current(ChatMessage::system_fmt(
                            "Not connected - notice not sent",
                            &self.timestamp_format,
                        ));
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /notice <target> <message>",
                    ));
                }
            }

            "ONOTICE" => {
                // Send notice to channel ops
                let message = args;
                if !message.is_empty() {
                    if let Some(channel) = &self.current_channel.clone()
                        && is_channel(channel)
                    {
                        let target = format!("@{}", channel);
                        if self.connected && self.send_notice_text(&target, message) {
                            self.add_message_to_current(ChatMessage::system_fmt(
                                &format!("-> -{}- {}", target, message),
                                &self.timestamp_format,
                            ));
                        } else {
                            self.add_message_to_current(ChatMessage::system_fmt(
                                "Not connected - notice not sent",
                                &self.timestamp_format,
                            ));
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /onotice <message>"));
                }
            }

            "AMSG" => {
                // Send message to all channels
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /amsg <message>"));
                } else if !self.connected {
                    self.add_message_to_current(ChatMessage::system_fmt(
                        "Not connected - message not sent",
                        &self.timestamp_format,
                    ));
                } else {
                    for channel_name in self.channels.keys().cloned().collect::<Vec<_>>() {
                        if is_channel(&channel_name) && self.send_privmsg_text(&channel_name, args)
                        {
                            let msg =
                                ChatMessage::new_fmt(&self.my_nick, args, &self.timestamp_format);
                            self.add_message_to_channel(&channel_name, msg);
                        }
                    }
                }
            }

            "AME" => {
                // Send action to all channels
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /ame <action>"));
                } else if !self.connected {
                    self.add_message_to_current(ChatMessage::system_fmt(
                        "Not connected - action not sent",
                        &self.timestamp_format,
                    ));
                } else {
                    for channel_name in self.channels.keys().cloned().collect::<Vec<_>>() {
                        if is_channel(&channel_name) && self.send_action_text(&channel_name, args)
                        {
                            let msg = ChatMessage::action_fmt(
                                &self.my_nick,
                                args,
                                &self.timestamp_format,
                            );
                            self.add_message_to_channel(&channel_name, msg);
                        }
                    }
                }
            }

            "SAY" => {
                // Force send as message (even if starts with /)
                if let Some(channel) = &self.current_channel.clone()
                    && !args.is_empty()
                {
                    if self.connected && self.send_privmsg_text(channel, args) {
                        let msg = ChatMessage::new_fmt(&self.my_nick, args, &self.timestamp_format);
                        self.add_message_to_channel(channel, msg);
                    } else {
                        self.add_message_to_current(ChatMessage::system_fmt(
                            "Not connected - message not sent",
                            &self.timestamp_format,
                        ));
                    }
                }
            }

            "DESCRIBE" => {
                // Send action to specific target: /describe <target> <action>
                let parts: Vec<&str> = args.splitn(2, ' ').collect();
                if let (Some(target), Some(action_text)) = (parts.first(), parts.get(1)) {
                    if self.connected && self.send_action_text(target, action_text) {
                        let msg = ChatMessage::action_fmt(
                            &self.my_nick,
                            action_text,
                            &self.timestamp_format,
                        );
                        self.add_message_to_channel(target, msg);
                    } else {
                        self.add_message_to_current(ChatMessage::system_fmt(
                            "Not connected - action not sent",
                            &self.timestamp_format,
                        ));
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /describe <target> <action>",
                    ));
                }
            }

            // === Channel Management ===
            "INVITE" => {
                let parts: Vec<&str> = args.splitn(2, ' ').collect();
                let nick = parts.first().copied().unwrap_or("");
                let channel = parts
                    .get(1)
                    .map(|s| s.to_string())
                    .or_else(|| self.current_channel.clone())
                    .unwrap_or_default();
                if nick.is_empty() || channel.is_empty() {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /invite <nick> [#channel]",
                    ));
                } else {
                    self.send_command(IrcCommand::Invite(nick.to_string(), channel.clone()));
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Inviting {} to {}",
                        nick, channel
                    )));
                }
            }

            "CYCLE" | "REJOIN" => {
                let channel = if args.is_empty() {
                    self.current_channel.clone()
                } else {
                    Some(args.to_string())
                };
                if let Some(ch) = channel
                    && is_channel(&ch)
                {
                    // Get the stored key for auto-rejoin if available
                    let key = self.channels.get(&ch).and_then(|c| c.key.clone());
                    self.send_command(IrcCommand::Part(ch.clone(), Some("Cycling".to_string())));
                    self.send_command(IrcCommand::Join(ch, key, None, None));
                }
            }

            "KNOCK" => {
                // Request invite to a channel
                if !args.is_empty() {
                    let channel = if args.starts_with('#') {
                        args.to_string()
                    } else {
                        format!("#{}", args)
                    };
                    self.send_command(IrcCommand::Raw(format!("KNOCK {}", channel)));
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Knocking on {}",
                        channel
                    )));
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /knock <#channel>"));
                }
            }

            "CLOSE" | "WC" => {
                // Close current window
                if let Some(channel_name) = self.current_channel.clone() {
                    // Live channels are PARTed (the Part echo removes the window);
                    // query windows and stale channel tabs from a dead connection
                    // are closed locally.
                    if is_channel(&channel_name)
                        && self.send_command(IrcCommand::Part(channel_name.clone(), None))
                    {
                        // Window is removed when the server confirms the PART.
                    } else {
                        self.channels.remove(&channel_name);
                        self.current_channel = self.channels.keys().next().cloned();
                    }
                }
            }

            // === Server Info Commands ===
            "MOTD" => {
                let server = if args.is_empty() {
                    None
                } else {
                    Some(args.to_string())
                };
                self.send_command(IrcCommand::Motd(server));
            }

            "TIME" => {
                let server = if args.is_empty() {
                    None
                } else {
                    Some(args.to_string())
                };
                self.send_command(IrcCommand::Time(server));
            }

            "ADMIN" => {
                let server = if args.is_empty() {
                    None
                } else {
                    Some(args.to_string())
                };
                self.send_command(IrcCommand::Admin(server));
            }

            "INFO" => {
                let server = if args.is_empty() {
                    None
                } else {
                    Some(args.to_string())
                };
                self.send_command(IrcCommand::Info(server));
            }

            "LUSERS" => {
                self.send_command(IrcCommand::Lusers);
            }

            "LINKS" => {
                let mask = if args.is_empty() {
                    None
                } else {
                    Some(args.to_string())
                };
                self.send_command(IrcCommand::Links(mask));
            }

            "STATS" => {
                if !args.is_empty() {
                    self.send_command(IrcCommand::Stats(args.to_string()));
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /stats <query> (c=servers, m=commands, o=opers, u=uptime)",
                    ));
                }
            }

            "TRACE" => {
                let target = if args.is_empty() {
                    None
                } else {
                    Some(args.to_string())
                };
                self.send_command(IrcCommand::Trace(target));
            }

            "USERHOST" => {
                if !args.is_empty() {
                    self.send_command(IrcCommand::Userhost(args.to_string()));
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /userhost <nick> [nick2] ...",
                    ));
                }
            }

            "ISON" => {
                if !args.is_empty() {
                    self.send_command(IrcCommand::Ison(args.to_string()));
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /ison <nick> [nick2] ...",
                    ));
                }
            }

            // IRCv3 Monitor - friend list online tracking
            "MONITOR" | "MON" => {
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /monitor + nick1,nick2 - Add to monitor list",
                    ));
                    self.add_message_to_current(ChatMessage::system(
                        "       /monitor - nick1,nick2 - Remove from list",
                    ));
                    self.add_message_to_current(ChatMessage::system(
                        "       /monitor L - List monitored nicks",
                    ));
                    self.add_message_to_current(ChatMessage::system(
                        "       /monitor C - Clear monitor list",
                    ));
                    self.add_message_to_current(ChatMessage::system(
                        "       /monitor S - Show status of monitored nicks",
                    ));
                } else {
                    let (subcmd, targets) = monitor_args(args);
                    self.send_command(IrcCommand::Monitor(subcmd, targets));
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
                    // Handles "host", "host port", "host:port", "[v6::1]:port",
                    // and bare IPv6 literals (which contain multiple colons and
                    // must not be split on the first one).
                    let (host, port) = parse_server_arg(args);
                    self.server_host = host;
                    if let Some(port) = port {
                        self.server_port = port;
                    }
                    self.reset_session_state();
                    // Disconnect from current server if connected
                    if self.connected || self.connecting || self.cmd_tx.is_some() {
                        self.request_reconnect_after_close("Changing servers");
                    } else {
                        self.connecting = true;
                    }
                    self.add_server_message(ChatMessage::system(&format!(
                        "Connecting to {}:{}...",
                        self.server_host, self.server_port
                    )));
                } else {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Current server: {}:{}",
                        self.server_host, self.server_port
                    )));
                }
            }

            "DISCONNECT" => {
                // Works in every connection state: connected, mid-connect, or in a
                // reconnect backoff loop (this is the only way to stop the latter).
                // request_manual_disconnect clears connecting/connection_lost, which
                // suppresses further reconnect attempts without touching the
                // persisted auto_reconnect setting.
                if self.connected
                    || self.connecting
                    || self.connection_lost
                    || self.cmd_tx.is_some()
                {
                    let reason = if args.is_empty() {
                        None
                    } else {
                        Some(args.to_string())
                    };
                    self.request_manual_disconnect(reason);
                    self.add_server_message(ChatMessage::system("Disconnected from server"));
                } else {
                    self.add_server_message(ChatMessage::system("Not connected"));
                }
            }

            "RECONNECT" => {
                self.request_reconnect_after_close("Reconnecting");
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
                        let list: Vec<String> = self
                            .ignore_list
                            .iter()
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
                    if !self
                        .ignore_list
                        .iter()
                        .any(|m| m.eq_ignore_ascii_case(&mask))
                    {
                        self.ignore_list_lower.push(mask.to_lowercase());
                        self.ignore_list.push(mask.clone());
                        self.add_message_to_current(ChatMessage::system(&format!(
                            "Now ignoring: {}",
                            mask
                        )));
                        // Save settings
                        self.get_settings().save();
                    } else {
                        self.add_message_to_current(ChatMessage::system(&format!(
                            "Already ignoring: {}",
                            mask
                        )));
                    }
                }
            }

            "UNIGNORE" => {
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /unignore <nick|mask>",
                    ));
                } else {
                    let mask_lower = args.to_lowercase();
                    let before_len = self.ignore_list.len();
                    self.ignore_list
                        .retain(|m| !m.to_lowercase().eq(&mask_lower));
                    if self.ignore_list.len() < before_len {
                        // Rebuild cached lowercase list
                        self.ignore_list_lower =
                            self.ignore_list.iter().map(|s| s.to_lowercase()).collect();
                        self.add_message_to_current(ChatMessage::system(&format!(
                            "No longer ignoring: {}",
                            args
                        )));
                        self.get_settings().save();
                    } else {
                        self.add_message_to_current(ChatMessage::system(&format!(
                            "Not in ignore list: {}",
                            args
                        )));
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
                                // Skip display-only output (previous search
                                // results, /help text) so a repeated search
                                // does not match its own banner and results.
                                .filter(|msg| !msg.no_log)
                                .filter(|msg| {
                                    msg.content.to_lowercase().contains(&pattern)
                                        || msg.sender.to_lowercase().contains(&pattern)
                                })
                                .take(50)
                                .map(|msg| {
                                    format!("[{}] <{}> {}", msg.timestamp, msg.sender, msg.content)
                                })
                                .collect()
                        } else {
                            Vec::new()
                        }
                    };

                    // Now display results (display-only: kept out of the log file)
                    self.add_message_to_current(
                        ChatMessage::system(&format!("Searching for: {}", args)).without_logging(),
                    );
                    for match_line in &matches {
                        self.add_message_to_current(ChatMessage::system(match_line).without_logging());
                    }
                    self.add_message_to_current(
                        ChatMessage::system(&format!("Found {} matches", matches.len()))
                            .without_logging(),
                    );
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /lastlog <pattern>"));
                }
            }

            "SVERSION" => {
                // Server VERSION (not CTCP)
                let server = if args.is_empty() {
                    None
                } else {
                    Some(args.to_string())
                };
                self.send_command(IrcCommand::Version(server));
            }

            "PERFORM" => {
                if args.is_empty() {
                    // Show current auto-perform
                    if self.auto_perform.is_empty() {
                        self.add_message_to_current(ChatMessage::system(
                            "No auto-perform commands set. Use /perform <command> to add.",
                        ));
                    } else {
                        let lines: Vec<String> = self
                            .auto_perform
                            .lines()
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
                    self.add_message_to_current(ChatMessage::system(
                        "Auto-perform commands cleared.",
                    ));
                } else {
                    // Add to auto-perform
                    if !self.auto_perform.is_empty() {
                        self.auto_perform.push('\n');
                    }
                    self.auto_perform.push_str(args);
                    self.get_settings().save();
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Added to auto-perform: {}",
                        args
                    )));
                }
            }

            "SETTINGS" => {
                self.show_settings = true;
            }

            "HELP" | "H" | "?" => {
                self.show_help();
            }

            _ => {
                self.add_message_to_current(ChatMessage::system(&format!(
                    "Unknown command: {}. Type /help for list.",
                    cmd
                )));
            }
        }
    }

    /// Send a message to a network service (NickServ, ChanServ, ...) into its
    /// query window, with explicit feedback when disconnected: silently
    /// dropping e.g. "/ns identify <pass>" would leave the user believing they
    /// are identified.
    fn send_service_message(&mut self, service: &str, args: &str) {
        if self.connected && self.send_privmsg_text(service, args) {
            self.add_message_to_channel(
                service,
                ChatMessage::new_fmt(&self.my_nick, args, &self.timestamp_format),
            );
        } else {
            self.add_message_to_current(ChatMessage::system_fmt(
                "Not connected - message not sent",
                &self.timestamp_format,
            ));
        }
    }

    pub fn show_help(&mut self) {
        let help_sections = vec![
            (
                "=== Basic Commands ===",
                vec![
                    "/join #channel      - Join a channel (alias: /j)",
                    "/part [#channel]    - Leave channel (alias: /leave)",
                    "/msg <nick> <text>  - Send private message (alias: /query)",
                    "/me <action>        - Send action message",
                    "/nick <newnick>     - Change nickname",
                    "/quit [message]     - Disconnect from server",
                ],
            ),
            (
                "=== Channel Commands ===",
                vec![
                    "/topic [text]       - View or set topic",
                    "/names [#channel]   - List users in channel",
                    "/list [pattern]     - List channels",
                    "/cycle              - Part and rejoin (alias: /hop, /rejoin)",
                    "/invite <nick>      - Invite user to channel",
                    "/knock <#channel>   - Request invite",
                ],
            ),
            (
                "=== Operator Commands ===",
                vec![
                    "/kick <nick> [why]  - Kick user (alias: /k)",
                    "/ban <mask>         - Ban user (+b)",
                    "/unban <mask>       - Remove ban (-b)",
                    "/kickban <nick>     - Ban and kick (alias: /kb)",
                    "/op <nick>          - Give ops (+o)",
                    "/deop <nick>        - Remove ops (-o)",
                    "/voice <nick>       - Give voice (+v)",
                    "/devoice <nick>     - Remove voice (-v)",
                    "/mode <+/-modes>    - Set channel/user modes",
                ],
            ),
            (
                "=== Messaging ===",
                vec![
                    "/notice <tgt> <msg> - Send notice (alias: /n)",
                    "/onotice <message>  - Notice to channel ops",
                    "/amsg <message>     - Message all channels",
                    "/ame <action>       - Action to all channels",
                    "/say <text>         - Send as message (even if /)",
                    "/describe <t> <act> - Send action to target",
                ],
            ),
            (
                "=== Info Commands ===",
                vec![
                    "/whois <nick>       - User info",
                    "/whowas <nick>      - Offline user info",
                    "/who <mask>         - Query users",
                    "/userhost <nick>    - Get user@host",
                    "/ison <nicks>       - Check if online",
                    "/motd               - Show MOTD",
                    "/lusers             - Network stats",
                    "/time               - Server time",
                ],
            ),
            (
                "=== CTCP ===",
                vec![
                    "/ctcp <n> <cmd>     - Send CTCP request",
                    "/version <nick>     - Query version",
                    "/ping <nick>        - Measure latency",
                ],
            ),
            (
                "=== Connection ===",
                vec![
                    "/server <host:port> - Connect to server",
                    "/disconnect         - Disconnect",
                    "/reconnect          - Reconnect",
                    "/away [message]     - Set away message",
                    "/back               - Clear away status",
                ],
            ),
            (
                "=== Utility ===",
                vec![
                    "/clear              - Clear window",
                    "/lastlog <pattern>  - Search messages",
                    "/ignore [mask]      - List or add to ignore list",
                    "/unignore <mask>    - Remove from ignore list",
                    "/perform [cmd]      - View/add auto-perform",
                    "/echo <text>        - Echo to window",
                    "/raw <command>      - Send raw IRC",
                    "/settings           - Open settings",
                    "/slap <nick>        - Slap with trout",
                ],
            ),
            (
                "=== Services ===",
                vec![
                    "/ns <command>       - NickServ message",
                    "/cs <command>       - ChanServ message",
                    "/ms <command>       - MemoServ message",
                    "/hs <command>       - HostServ message",
                    "/os <command>       - OperServ message",
                    "/bs <command>       - BotServ message",
                ],
            ),
            (
                "=== Advanced ===",
                vec![
                    "/umode [modes]      - Set/view user modes",
                    "/silence [+/-mask]  - Server-side ignore",
                    "/accept [nick]      - Callerid accept list",
                    "/monitor + nicks    - Track online status",
                ],
            ),
            (
                "=== Shortcuts ===",
                vec![
                    "Alt+1-9             - Switch tabs",
                    "Ctrl+W              - Close tab",
                    "Up/Down             - Command history",
                    "Tab                 - Nick completion",
                ],
            ),
        ];

        for (section, commands) in help_sections {
            // Display-only: help text does not belong in the persistent chat log.
            self.add_message_to_current(ChatMessage::system(section).without_logging());
            for cmd in commands {
                self.add_message_to_current(ChatMessage::system(cmd).without_logging());
            }
        }
    }
}

/// Split /monitor arguments into (subcommand, targets), accepting both the
/// documented "+ nick1,nick2" form and the attached "+nick1,nick2" form
/// (which would otherwise send the sign as part of the nick: "MONITOR + +nick").
fn monitor_args(args: &str) -> (String, Option<String>) {
    let parts: Vec<&str> = args.splitn(2, ' ').collect();
    let first = parts[0];
    let rest = parts.get(1).map(|s| s.to_string());

    let subcmd = first.to_uppercase();
    match subcmd.as_str() {
        "+" | "-" | "C" | "L" | "S" => (subcmd, rest),
        _ => {
            if let Some(stripped) = first.strip_prefix(['+', '-']) {
                let sign = first[..1].to_string();
                let mut targets = stripped.to_string();
                if let Some(more) = rest {
                    // "/monitor +a b" -> targets are comma-separated on the wire.
                    targets.push(',');
                    targets.push_str(&more.replace(' ', ","));
                }
                (sign, Some(targets))
            } else {
                // Bare nick(s): assume add.
                ("+".to_string(), Some(args.to_string()))
            }
        }
    }
}

/// Parse the /server argument: "host", "host port", "host:port",
/// "[v6::addr]:port", or a bare IPv6 literal (multiple colons; must not be
/// split on the first one).
fn parse_server_arg(args: &str) -> (String, Option<String>) {
    let args = args.trim();
    // Space-separated form: "host 6697" (works for any host including IPv6).
    if let Some((host, port)) = args.split_once(' ') {
        let host = host.trim_matches(['[', ']']);
        return (host.to_string(), Some(port.trim().to_string()));
    }
    // Bracketed IPv6 with optional port: "[::1]:6697" or "[::1]".
    if let Some(rest) = args.strip_prefix('[') {
        if let Some((host, after)) = rest.split_once(']') {
            let port = after.strip_prefix(':').map(|p| p.to_string());
            return (host.to_string(), port);
        }
        return (rest.to_string(), None);
    }
    // "host:port" - only with exactly one colon and a numeric port; an IPv6
    // literal has several colons and falls through as a bare host.
    if args.chars().filter(|&c| c == ':').count() == 1
        && let Some((host, port)) = args.rsplit_once(':')
        && !host.is_empty()
        && !port.is_empty()
        && port.parse::<u16>().is_ok()
    {
        return (host.to_string(), Some(port.to_string()));
    }
    (args.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_args_accepts_attached_sign() {
        assert_eq!(monitor_args("+ friend"), ("+".into(), Some("friend".into())));
        assert_eq!(monitor_args("+friend"), ("+".into(), Some("friend".into())));
        assert_eq!(monitor_args("-friend"), ("-".into(), Some("friend".into())));
        assert_eq!(
            monitor_args("+a,b c"),
            ("+".into(), Some("a,b,c".into()))
        );
        assert_eq!(monitor_args("L"), ("L".into(), None));
        assert_eq!(monitor_args("friend"), ("+".into(), Some("friend".into())));
    }

    #[test]
    fn server_arg_handles_ipv6() {
        assert_eq!(
            parse_server_arg("irc.libera.chat:6697"),
            ("irc.libera.chat".into(), Some("6697".into()))
        );
        assert_eq!(
            parse_server_arg("irc.libera.chat 6697"),
            ("irc.libera.chat".into(), Some("6697".into()))
        );
        assert_eq!(
            parse_server_arg("2001:db8::1 6697"),
            ("2001:db8::1".into(), Some("6697".into()))
        );
        assert_eq!(parse_server_arg("2001:db8::1"), ("2001:db8::1".into(), None));
        assert_eq!(
            parse_server_arg("[::1]:7000"),
            ("::1".into(), Some("7000".into()))
        );
        assert_eq!(parse_server_arg("[2001:db8::1]"), ("2001:db8::1".into(), None));
        assert_eq!(parse_server_arg("irc.example.org"), ("irc.example.org".into(), None));
        // A non-numeric tail is not a port.
        assert_eq!(parse_server_arg("host:junk"), ("host:junk".into(), None));
    }
}
