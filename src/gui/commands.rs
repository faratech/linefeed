//! IRC command processing and help system

use super::helpers::{is_extended_ban, mask_matches, nick_to_mask};
use super::{ChatMessage, IrcApp};
use crate::irc::IrcCommand;

impl IrcApp {
    pub fn process_command(&mut self, input: &str) {
        // "//text" is an escaped slash: it sends the literal message "/text"
        // instead of running a command (the same as "/say /text", #263).
        if input.starts_with("//") {
            let text = &input[1..];
            // A mistyped "//ns identify pw" must not go out as the public
            // message "/ns identify pw" (or be echoed and logged in a
            // NickServ tab, where the leading slash hides the keyword).
            // /say remains the deliberate way to send such text.
            if command_line_contains_credentials(text) {
                self.add_message_to_current(
                    ChatMessage::system(
                        "Not sent: this looks like a credential command typed with a doubled \
                         slash. Use a single slash to run it, or /say to send it as text.",
                    )
                    .without_logging(),
                );
                return;
            }
            self.process_command(&format!("/say {text}"));
            return;
        }
        // Whitespace separator (not just ' '): the credential filter splits
        // on any whitespace, so the command token must too — a tab after the
        // command word would otherwise become an unknown command (#148).
        let parts: Vec<&str> = input[1..].splitn(2, char::is_whitespace).collect();
        let word = parts[0];
        // ASCII only: full Unicode upper-casing maps the dotless i and the
        // long s to I and S, so "/ıdentify pw" would dispatch as the raw
        // IDENTIFY service command behind the credential filter's back.
        if !word.is_ascii() {
            self.add_message_to_current(ChatMessage::system(&format!(
                "Invalid command: {word}. Type /help for list."
            )));
            return;
        }
        let cmd = word.to_ascii_uppercase();
        // trim_start: a doubled space after the command ("/msg  alice hi") must
        // not produce an empty first argument (which silently sent messages to
        // an empty target and opened a phantom blank-named tab).
        let args = parts.get(1).copied().unwrap_or("").trim_start();

        match cmd.as_str() {
            "JOIN" | "J" => {
                // Whitespace separators throughout the arg-splitting arms:
                // the command token already splits on any whitespace (#148),
                // so a tab-pasted argument must not be glued into a single
                // mangled token here (#159).
                // split_word drops the whole run of separators, so a doubled
                // space never becomes part of the stored key.
                let (channel_arg, key) = split_word(args);
                let key = Some(key).filter(|k| !k.is_empty()).map(str::to_string);
                if channel_arg.is_empty() {
                    self.add_message_to_current(ChatMessage::system_fmt(
                        "Usage: /join <#channel> [key]",
                        &self.timestamp_format,
                    ));
                    return;
                }
                let channel = self.normalize_channel_name(channel_arg);
                // send_join records the key for the tab the server confirms.
                let sent = self.send_join(channel.clone(), key);
                self.report_send_failure(sent, &format!("Not connected - join {channel} not sent"));
            }

            "PART" | "LEAVE" => {
                // Forms: "/part", "/part #chan", "/part #chan reason", "/part reason".
                // Split off a leading channel token; otherwise treat args as a reason
                // for the current channel (so the channel field never contains spaces).
                let (channel, inline_reason) = if args.is_empty() {
                    (self.current_channel.clone(), None)
                } else {
                    let (first, rest) = split_word(args);
                    if self.is_channel_name(first) {
                        (
                            Some(first.to_string()),
                            Some(rest).filter(|r| !r.is_empty()).map(str::to_string),
                        )
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
                    if !self.send_command(IrcCommand::Part(ch.clone(), reason))
                        && self.cmd_tx.is_none()
                    {
                        // Disconnected: the server can't confirm the part, so
                        // close the stale channel window locally instead of
                        // leaving a tab that cannot be removed. A send
                        // failure on a live channel (e.g. a line-too-long
                        // reason) must never destroy a joined tab (#162) —
                        // send_command already reported the real cause.
                        self.remove_channel(&ch);
                        if self.current_target_is(&ch) {
                            self.current_channel = self.channels.keys().next().cloned();
                        }
                    }
                }
            }

            "MSG" | "PRIVMSG" => {
                // Whitespace separator (not just ' '): the credential filter
                // splits on any whitespace, so dispatch must agree or a
                // tab-pasted target dodges the service redaction (#148).
                // split_word also drops the whole run of separators.
                let (target, message) = split_word(args);
                if !target.is_empty() && !message.is_empty() {
                    if self.connected {
                        if self.send_privmsg_text(target, message) {
                            let chat_msg = outgoing_local_echo(
                                target,
                                &self.my_nick,
                                message,
                                &self.timestamp_format,
                            );
                            self.add_message_to_channel(target, chat_msg);
                        } else {
                            self.add_message_to_current(
                                ChatMessage::system_fmt("Message not sent", &self.timestamp_format)
                                    .without_logging(),
                            );
                        }
                    } else {
                        self.add_message_to_current(
                            ChatMessage::system_fmt(
                                "Not connected - message not sent",
                                &self.timestamp_format,
                            )
                            .without_logging(),
                        );
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /msg <target> <message>",
                    ));
                }
            }

            "QUERY" | "Q" => {
                // Whitespace separator: see the MSG arm (#148).
                let (target, message) = split_word(args);
                if target.is_empty() {
                    self.add_server_message(ChatMessage::system("Usage: /query <nick> [message]"));
                    return;
                }
                if !self.open_query(target) {
                    self.add_server_message(ChatMessage::system_fmt(
                        "Unable to open query window",
                        &self.timestamp_format,
                    ));
                    return;
                }
                // If there's a message, send it
                if !message.is_empty() {
                    if self.connected {
                        if self.send_privmsg_text(target, message) {
                            let chat_msg = outgoing_local_echo(
                                target,
                                &self.my_nick,
                                message,
                                &self.timestamp_format,
                            );
                            self.add_message_to_channel(target, chat_msg);
                        } else {
                            self.add_message_to_current(
                                ChatMessage::system_fmt("Message not sent", &self.timestamp_format)
                                    .without_logging(),
                            );
                        }
                    } else {
                        self.add_message_to_current(
                            ChatMessage::system_fmt(
                                "Not connected - message not sent",
                                &self.timestamp_format,
                            )
                            .without_logging(),
                        );
                    }
                }
            }

            "ME" => {
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /me <action>"));
                } else if let Some(channel) = &self.current_channel.clone() {
                    if self.connected {
                        if self.send_action_text(channel, args) {
                            let msg = ChatMessage::action_fmt(
                                &self.my_nick,
                                args,
                                &self.timestamp_format,
                            );
                            self.add_message_to_channel(channel, msg);
                        } else {
                            self.add_message_to_current(
                                ChatMessage::system_fmt("Action not sent", &self.timestamp_format)
                                    .without_logging(),
                            );
                        }
                    } else {
                        self.add_message_to_current(
                            ChatMessage::system_fmt(
                                "Not connected - action not sent",
                                &self.timestamp_format,
                            )
                            .without_logging(),
                        );
                    }
                }
            }

            "SLAP" => {
                // Classic IRC slap action, with an optional custom phrase:
                // "/slap <nick> [text...]" — a bare slap keeps the trout.
                const DEFAULT_TROUT: &str = "around a bit with a large trout";
                if let Some(channel) = &self.current_channel.clone() {
                    let (target, custom) = args
                        .split_once(char::is_whitespace)
                        .map_or((args, ""), |(target, custom)| {
                            (target.trim(), custom.trim())
                        });
                    let action_text = match target {
                        "" => format!("slaps everyone {DEFAULT_TROUT}"),
                        nick if custom.is_empty() => format!("slaps {nick} {DEFAULT_TROUT}"),
                        nick => format!("slaps {nick} {custom}"),
                    };
                    if self.connected {
                        if self.send_action_text(channel, &action_text) {
                            let msg = ChatMessage::action_fmt(
                                &self.my_nick,
                                &action_text,
                                &self.timestamp_format,
                            );
                            self.add_message_to_channel(channel, msg);
                        } else {
                            self.add_message_to_current(
                                ChatMessage::system_fmt("Action not sent", &self.timestamp_format)
                                    .without_logging(),
                            );
                        }
                    } else {
                        self.add_message_to_current(
                            ChatMessage::system_fmt(
                                "Not connected - action not sent",
                                &self.timestamp_format,
                            )
                            .without_logging(),
                        );
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
                    let sent = self.send_command(IrcCommand::Away(None));
                    if sent {
                        self.away_status = None;
                        self.auto_away_triggered = false;
                        // Update our own status in all channels
                        let my_nick = self.my_nick.clone();
                        self.update_user_away_status(&my_nick, None);
                        self.add_server_message(ChatMessage::system(
                            "You are no longer marked as away",
                        ));
                    } else {
                        self.report_send_failure(sent, "Not connected - away status not changed");
                    }
                } else {
                    let reason = args.to_string();
                    let sent = self.send_command(IrcCommand::Away(Some(reason.clone())));
                    if sent {
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
                        self.report_send_failure(sent, "Not connected - away status not changed");
                    }
                }
            }

            "BACK" => {
                let sent = self.send_command(IrcCommand::Away(None));
                if sent {
                    self.away_status = None;
                    self.auto_away_triggered = false;
                    // Update our own status in all channels
                    let my_nick = self.my_nick.clone();
                    self.update_user_away_status(&my_nick, None);
                    self.add_server_message(ChatMessage::system(
                        "You are no longer marked as away",
                    ));
                } else {
                    self.report_send_failure(sent, "Not connected - away status not changed");
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

            "SETNAME" => {
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /setname <realname>"));
                } else {
                    self.send_command(IrcCommand::Setname(args.to_string()));
                }
            }

            "REPLY" => {
                let (msgid, content) = split_word(args);
                if let Some(target) = self.current_channel.clone()
                    && self.enabled_caps.contains("message-tags")
                    && valid_message_reference(msgid)
                    && !content.is_empty()
                {
                    let tag = escape_message_tag_value(msgid);
                    let command =
                        IrcCommand::Raw(format!("@+reply={tag} PRIVMSG {target} :{content}"));
                    if self.send_command(command) {
                        self.add_message_to_channel(
                            &target,
                            ChatMessage::new_fmt(&self.my_nick, content, &self.timestamp_format)
                                .with_reply_to(Some(msgid.to_string())),
                        );
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /reply <msgid> <message>",
                    ));
                }
            }

            "REACT" => {
                let (msgid, reaction) = split_word(args);
                if let Some(target) = self.current_channel.clone()
                    && self.enabled_caps.contains("message-tags")
                    && valid_message_reference(msgid)
                    && !reaction.is_empty()
                {
                    let msgid = escape_message_tag_value(msgid);
                    let reaction = escape_message_tag_value(reaction);
                    self.send_command(IrcCommand::Raw(format!(
                        "@+draft/react={reaction};+reply={msgid} TAGMSG {target}"
                    )));
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /react <msgid> <reaction>",
                    ));
                }
            }

            "TYPING" => {
                let state = args.to_ascii_lowercase();
                if let Some(target) = self.current_channel.clone()
                    && self.enabled_caps.contains("message-tags")
                    && matches!(state.as_str(), "active" | "paused" | "done")
                {
                    self.send_command(IrcCommand::Raw(format!("@+typing={state} TAGMSG {target}")));
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /typing active|paused|done",
                    ));
                }
            }

            "LABEL" => {
                let (label, command) = split_word(args);
                if self.enabled_caps.contains("labeled-response")
                    && valid_message_reference(label)
                    && !command.is_empty()
                {
                    self.send_command(IrcCommand::Raw(format!(
                        "@label={} {command}",
                        escape_message_tag_value(label)
                    )));
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /label <label> <IRC command>",
                    ));
                }
            }

            "MARKREAD" => {
                let mut parts = args.split_whitespace();
                let explicit_target = parts.next();
                let (target, timestamp) = match explicit_target {
                    Some(target)
                        if self.is_channel_name(target) || self.channel_key(target).is_some() =>
                    {
                        (Some(target.to_string()), parts.next().map(str::to_string))
                    }
                    Some(timestamp) if timestamp.starts_with("timestamp=") => {
                        (self.current_channel.clone(), Some(timestamp.to_string()))
                    }
                    Some("*") => (self.current_channel.clone(), Some("*".to_string())),
                    Some(_) => (None, None),
                    None => {
                        let target = self.current_channel.clone();
                        let timestamp = target
                            .as_deref()
                            .and_then(|target| self.channel_key(target))
                            .and_then(|key| self.channels.get(&key))
                            .and_then(|channel| {
                                channel.messages.iter().rev().find_map(|message| {
                                    message
                                        .server_time
                                        .as_ref()
                                        .map(|time| format!("timestamp={time}"))
                                })
                            });
                        (target, timestamp)
                    }
                };
                if let Some(target) = target {
                    self.send_command(IrcCommand::Markread(target, timestamp));
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /markread [target] [timestamp=<ISO8601>]",
                    ));
                }
            }

            "REDACT" => {
                // The reason is everything after the message id, however many
                // words it has (#260).
                let (first, rest) = split_word(args);
                let (target, msgid, reason) = if self.is_channel_name(first) {
                    let (msgid, reason) = split_word(rest);
                    (
                        Some(first.to_string()),
                        msgid.to_string(),
                        Some(reason).filter(|reason| !reason.is_empty()),
                    )
                } else {
                    (
                        self.current_channel.clone(),
                        first.to_string(),
                        Some(rest).filter(|reason| !reason.is_empty()),
                    )
                };
                let reason = reason.map(str::to_string);
                if let Some(target) = target
                    && !msgid.is_empty()
                {
                    self.send_command(IrcCommand::Redact(target, msgid, reason));
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /redact [#channel] <msgid> [reason]",
                    ));
                }
            }

            "RENAME" => {
                let (new_name, reason) = split_word(args);
                if let Some(old_name) = self.current_channel.clone()
                    && self.is_channel_name(&old_name)
                    && self.is_channel_name(new_name)
                {
                    let sent = self.send_command(IrcCommand::Rename(
                        old_name,
                        new_name.to_string(),
                        Some(reason).filter(|r| !r.is_empty()).map(str::to_string),
                    ));
                    self.report_send_failure(sent, "Not connected - server command not sent");
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /rename <#new-channel> [reason]",
                    ));
                }
            }

            "RELOCATE" => {
                let (new_name, reason) = split_word(args);
                if let Some(old_name) = self.current_channel.clone()
                    && self.is_channel_name(&old_name)
                    && self.is_channel_name(new_name)
                {
                    let sent = self.send_command(IrcCommand::Relocate(
                        old_name,
                        new_name.to_string(),
                        Some(reason).filter(|r| !r.is_empty()).map(str::to_string),
                    ));
                    self.report_send_failure(sent, "Not connected - server command not sent");
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /relocate <#new-channel> [reason]",
                    ));
                }
            }

            "BOUNCER" | "PERSISTENCE" | "METADATA" | "WEBPUSH" | "REGISTER" | "VERIFY"
            | "TOKEN" => {
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Usage: /{} <subcommand or arguments>",
                        cmd.to_ascii_lowercase()
                    )));
                } else {
                    let sent = self.send_command(IrcCommand::Raw(format!("{cmd} {args}")));
                    self.report_send_failure(sent, "Not connected - server command not sent");
                }
            }

            "RAW" | "QUOTE" => {
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /raw <command>"));
                } else {
                    // Send even during CAP/SASL negotiation: cmd_tx is live
                    // before RPL_WELCOME and manual negotiation lines are
                    // legitimate (#156). While the channel is live,
                    // send_command reports its own accurate row for
                    // too-long / denied-tag failures.
                    let sent = self.send_command(IrcCommand::Raw(args.to_string()));
                    self.report_send_failure(sent, "Not connected - server command not sent");
                }
            }

            "HISTORY" | "CHATHISTORY" => {
                let parts: Vec<&str> = args.split_whitespace().collect();
                let subcommand = parts.first().map(|part| part.to_ascii_uppercase());
                if matches!(
                    subcommand.as_deref(),
                    Some("BEFORE" | "AFTER" | "AROUND" | "BETWEEN" | "TARGETS")
                ) {
                    let parsed_limit = |value: Option<&&str>, fallback: usize| {
                        value
                            .and_then(|value| value.parse::<usize>().ok())
                            .filter(|limit| *limit > 0)
                            .unwrap_or(fallback)
                    };
                    let fallback = self.total_history_limit(None);
                    let command = match (subcommand.as_deref(), parts.as_slice()) {
                        (Some("BEFORE"), [_, target, reference, rest @ ..]) => {
                            let total = self
                                .total_history_limit(Some(parsed_limit(rest.first(), fallback)));
                            let page = self.history_page_limit(total);
                            Some((
                                Some(*target),
                                IrcCommand::ChathistoryBefore(
                                    (*target).to_string(),
                                    (*reference).to_string(),
                                    page,
                                ),
                                Some(super::PendingHistoryRequest {
                                    placement: super::HistoryPlacement::Prepend,
                                    direction: super::HistoryDirection::Before,
                                    remaining: total,
                                    loaded: 0,
                                    shown: 0,
                                    page_limit: page,
                                }),
                            ))
                        }
                        (Some("AFTER"), [_, target, reference, rest @ ..]) => {
                            let total = self
                                .total_history_limit(Some(parsed_limit(rest.first(), fallback)));
                            let page = self.history_page_limit(total);
                            Some((
                                Some(*target),
                                IrcCommand::ChathistoryAfter(
                                    (*target).to_string(),
                                    (*reference).to_string(),
                                    page,
                                ),
                                Some(super::PendingHistoryRequest {
                                    placement: super::HistoryPlacement::Append,
                                    direction: super::HistoryDirection::After,
                                    remaining: total,
                                    loaded: 0,
                                    shown: 0,
                                    page_limit: page,
                                }),
                            ))
                        }
                        (Some("AROUND"), [_, target, reference, rest @ ..]) => Some((
                            Some(*target),
                            IrcCommand::ChathistoryAround(
                                (*target).to_string(),
                                (*reference).to_string(),
                                self.history_page_limit(parsed_limit(rest.first(), fallback)),
                            ),
                            None,
                        )),
                        (Some("BETWEEN"), [_, target, first, second, rest @ ..]) => Some((
                            Some(*target),
                            IrcCommand::ChathistoryBetween(
                                (*target).to_string(),
                                (*first).to_string(),
                                (*second).to_string(),
                                self.history_page_limit(parsed_limit(rest.first(), fallback)),
                            ),
                            None,
                        )),
                        (Some("TARGETS"), [_, first, second, rest @ ..]) => Some((
                            None,
                            IrcCommand::ChathistoryTargets(
                                (*first).to_string(),
                                (*second).to_string(),
                                self.history_page_limit(parsed_limit(rest.first(), fallback)),
                            ),
                            None,
                        )),
                        _ => None,
                    };
                    if let Some((target, command, pending)) = command {
                        if self.chathistory_enabled {
                            if let Some(target) = target {
                                // Send first: switching tabs for a request that
                                // was never queued would silently strand the
                                // user in a tab with no history in flight.
                                let sent = if let Some(pending) = pending {
                                    self.queue_history_request(target, command, pending)
                                } else {
                                    self.send_command(command)
                                };
                                if sent {
                                    self.prepare_history_target(target);
                                } else {
                                    // While the channel is live, send_command
                                    // already reported its own accurate row.
                                    self.report_send_failure(
                                        sent,
                                        "Not connected - history request not sent",
                                    );
                                }
                            } else {
                                let sent = self.send_command(command);
                                self.report_send_failure(
                                    sent,
                                    "Not connected - history request not sent",
                                );
                            }
                        } else {
                            self.add_message_to_current(ChatMessage::system(
                                "Server history is unavailable on this connection",
                            ));
                        }
                    } else {
                        self.add_message_to_current(ChatMessage::system(
                            "Usage: /history before|after|around <target> <reference> [limit]; /history between <target> <first> <second> [limit]; /history targets <first> <second> [limit]",
                        ));
                    }
                    return;
                }
                let (target, requested_limit) = match parts.as_slice() {
                    [] => (self.current_channel.clone(), None),
                    [limit] if limit.chars().all(|c| c.is_ascii_digit()) => {
                        match limit.parse::<usize>() {
                            Ok(limit) if limit > 0 => (self.current_channel.clone(), Some(limit)),
                            _ => {
                                self.add_message_to_current(ChatMessage::system(
                                    "Usage: /history [#channel] [limit]",
                                ));
                                return;
                            }
                        }
                    }
                    [target] => (Some(self.normalize_channel_name(target)), None),
                    [target, limit] => match limit.parse::<usize>() {
                        Ok(limit) if limit > 0 => {
                            (Some(self.normalize_channel_name(target)), Some(limit))
                        }
                        _ => {
                            self.add_message_to_current(ChatMessage::system(
                                "Usage: /history [#channel] [limit]",
                            ));
                            return;
                        }
                    },
                    _ => {
                        self.add_message_to_current(ChatMessage::system(
                            "Usage: /history [#channel] [limit]",
                        ));
                        return;
                    }
                };
                let Some(target) = target.filter(|target| self.is_channel_name(target)) else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /history [#channel] [limit]",
                    ));
                    return;
                };
                if self.request_latest_history(
                    &target,
                    requested_limit,
                    super::HistoryPlacement::Prepend,
                ) {
                    self.prepare_history_target(&target);
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Server history is unavailable on this connection",
                    ));
                }
            }

            "CTCP" => {
                // /ctcp <nick> <command> [args]
                let (target, rest) = split_word(args);
                let (ctcp_command, ctcp_args) = split_word(rest);
                if !target.is_empty() && !ctcp_command.is_empty() {
                    let ctcp_cmd = ctcp_command.to_uppercase();
                    let ctcp_msg = if ctcp_args.is_empty() {
                        format!("\x01{}\x01", ctcp_cmd)
                    } else {
                        format!("\x01{} {}\x01", ctcp_cmd, ctcp_args)
                    };
                    if self.send_command(IrcCommand::Privmsg(target.to_string(), ctcp_msg)) {
                        self.add_message_to_current(ChatMessage::system(&format!(
                            "[CTCP] Sent {} to {}",
                            ctcp_cmd, target
                        )));
                    } else {
                        self.add_message_to_current(
                            ChatMessage::system(
                                "CTCP request not sent (not connected or message too long)",
                            )
                            .without_logging(),
                        );
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /ctcp <nick> <command> [args]",
                    ));
                }
            }

            "VERSION" => {
                // Convenience: /version <nick> is shorthand for /ctcp <nick> VERSION.
                // Only the first word is the target; trailing words would
                // otherwise become the message text of the PRIVMSG (#261).
                let (target, _) = split_word(args);
                if !target.is_empty() {
                    let ctcp_msg = "\x01VERSION\x01".to_string();
                    if self.send_command(IrcCommand::Privmsg(target.to_string(), ctcp_msg)) {
                        self.add_message_to_current(ChatMessage::system(&format!(
                            "[CTCP] Sent VERSION to {}",
                            target
                        )));
                    } else {
                        self.add_message_to_current(
                            ChatMessage::system(
                                "CTCP VERSION not sent (not connected or message too long)",
                            )
                            .without_logging(),
                        );
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /version <nick>"));
                }
            }

            "PING" if !args.is_empty() && !args.starts_with('#') => {
                // /ping <nick> - Send CTCP PING to measure latency (first word
                // only is the target, as for /version).
                use std::time::{SystemTime, UNIX_EPOCH};
                let (target, _) = split_word(args);
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let ctcp_msg = format!("\x01PING {}\x01", timestamp);
                if self.send_command(IrcCommand::Privmsg(target.to_string(), ctcp_msg)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[CTCP] Sent PING to {}",
                        target
                    )));
                } else {
                    self.add_message_to_current(
                        ChatMessage::system(
                            "CTCP PING not sent (not connected or message too long)",
                        )
                        .without_logging(),
                    );
                }
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
                    self.send_command(IrcCommand::Whois(whois_query_target(args)));
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
                self.show_channel_list = true;
                self.channel_list_apply_min_filter = args.is_empty()
                    && self.list_min_users >= 1
                    && !self
                        .network_support
                        .elist
                        .to_ascii_uppercase()
                        .contains('U');

                // Use configurable min users filter to avoid flooding on large networks
                // Users can override with /list * or /list <filter>
                let request = if args.is_empty() {
                    // >N means "more than N users", so for min 5, send >4
                    if self.list_min_users >= 1
                        && self
                            .network_support
                            .elist
                            .to_ascii_uppercase()
                            .contains('U')
                    {
                        IrcCommand::List(Some(format!(
                            ">{}",
                            self.list_min_users.saturating_sub(1)
                        )))
                    } else {
                        IrcCommand::List(None)
                    }
                } else if args == "*" {
                    // Allow /list * to get all channels (use at your own risk)
                    IrcCommand::List(None)
                } else {
                    IrcCommand::List(Some(args.to_string()))
                };
                let sent = self.send_command(request);
                if sent {
                    self.channel_list.clear();
                    self.channel_list_dirty = true;
                    self.channel_list_loading = true;
                } else {
                    self.channel_list_loading = false;
                    self.report_send_failure(
                        sent,
                        "Not connected - channel list was not refreshed",
                    );
                }
            }

            // === Channel Operator Commands ===
            "KICK" | "K" => {
                let (nick, reason) = split_word(args);
                if nick.is_empty() {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /kick <nick> [reason]",
                    ));
                } else if let Some(channel) = &self.current_channel.clone()
                    && self.is_channel_name(channel)
                {
                    let reason = Some(reason).filter(|r| !r.is_empty()).map(str::to_string);
                    self.send_command(IrcCommand::Kick(channel.clone(), nick.to_string(), reason));
                }
            }

            "BAN" => {
                if !args.is_empty() {
                    if let Some(channel) = &self.current_channel.clone()
                        && self.is_channel_name(channel)
                    {
                        // Convert nick to ban mask if it's just a nick
                        let mask = self.ban_mask_for(args);
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
                        && self.is_channel_name(channel)
                    {
                        let plan = self.unban_masks_for(args);
                        if let Some(note) = plan.note {
                            self.add_message_to_current(ChatMessage::system(&note));
                        }
                        // MODE -bbb m1 m2 m3, at most the server's MODES= per line.
                        let per_line = self.modes_per_line();
                        for chunk in plan.masks.chunks(per_line) {
                            self.send_command(IrcCommand::Mode(
                                channel.clone(),
                                Some(format!("-{}", "b".repeat(chunk.len()))),
                                chunk.to_vec(),
                            ));
                        }
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /unban <nick|mask>"));
                }
            }

            "KICKBAN" | "KB" => {
                let (target, reason) = split_word(args);
                if target.is_empty() {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /kickban <nick> [reason]",
                    ));
                } else if let Some(channel) = &self.current_channel.clone()
                    && self.is_channel_name(channel)
                {
                    // Ban first, then kick. nick_to_mask passes a full/partial
                    // hostmask through and completes a bare nick to nick!*@*
                    // (blindly appending "!*@*" to a mask built a garbage ban).
                    let is_mask = is_extended_ban(target)
                        || target.contains('!')
                        || target.contains('@')
                        || target.contains('*');
                    let mask = self.ban_mask_for(target);
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
                        let reason = Some(reason).filter(|r| !r.is_empty()).map(str::to_string);
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
                        && self.is_channel_name(channel)
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
                        && self.is_channel_name(channel)
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
                        && self.is_channel_name(channel)
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
                        && self.is_channel_name(channel)
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

            "HALFOP" => {
                if !args.is_empty()
                    && let Some(channel) = &self.current_channel.clone()
                    && self.is_channel_name(channel)
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
                    && self.is_channel_name(channel)
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
                    // The "/mode +m" shorthand applies to the current channel;
                    // a query tab's target is a peer nick, not a channel.
                    let shorthand_target = self
                        .current_channel
                        .as_deref()
                        .filter(|name| self.network_support.is_channel(name));
                    if let Some((target, mode, params)) = mode_command_args(args, shorthand_target)
                    {
                        self.send_command(IrcCommand::Mode(target, mode, params));
                    } else {
                        self.add_message_to_current(ChatMessage::system(
                            "Usage: /mode [target] [+/-modes] [params]",
                        ));
                    }
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
                // Server-side ignore (supported by some networks). The argument
                // goes out as typed: +mask/-mask edit the list, and Bahamut/ircu
                // define a bare nick as querying that user's silence list.
                if self.isupport_lacks(&["SILENCE"]) {
                    self.add_message_to_current(ChatMessage::system(
                        "Server does not advertise SILENCE support",
                    ));
                } else if args.is_empty() {
                    // List silence entries
                    self.send_command(IrcCommand::Raw("SILENCE".to_string()));
                } else {
                    self.send_command(IrcCommand::Raw(format!("SILENCE {}", args)));
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
                // Whitespace separator: see the MSG arm (#148).
                let (target, message) = split_word(args);
                if !target.is_empty() && !message.is_empty() {
                    if self.connected {
                        if self.send_notice_text(target, message) {
                            let confirmation =
                                if service_message_contains_credentials(target, message) {
                                    ChatMessage::system_fmt(
                                        &format!("-> -{}- <credential command sent>", target),
                                        &self.timestamp_format,
                                    )
                                    .without_logging()
                                } else {
                                    ChatMessage::system_fmt(
                                        &format!("-> -{}- {}", target, message),
                                        &self.timestamp_format,
                                    )
                                };
                            self.add_message_to_current(confirmation);
                        } else {
                            self.add_message_to_current(
                                ChatMessage::system_fmt("Notice not sent", &self.timestamp_format)
                                    .without_logging(),
                            );
                        }
                    } else {
                        self.add_message_to_current(
                            ChatMessage::system_fmt(
                                "Not connected - notice not sent",
                                &self.timestamp_format,
                            )
                            .without_logging(),
                        );
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
                        && self.is_channel_name(channel)
                    {
                        let target = format!("@{}", channel);
                        if self.connected {
                            if self.send_notice_text(&target, message) {
                                self.add_message_to_current(ChatMessage::system_fmt(
                                    &format!("-> -{}- {}", target, message),
                                    &self.timestamp_format,
                                ));
                            } else {
                                self.add_message_to_current(
                                    ChatMessage::system_fmt(
                                        "Notice not sent",
                                        &self.timestamp_format,
                                    )
                                    .without_logging(),
                                );
                            }
                        } else {
                            self.add_message_to_current(
                                ChatMessage::system_fmt(
                                    "Not connected - notice not sent",
                                    &self.timestamp_format,
                                )
                                .without_logging(),
                            );
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
                    self.add_message_to_current(
                        ChatMessage::system_fmt(
                            "Not connected - message not sent",
                            &self.timestamp_format,
                        )
                        .without_logging(),
                    );
                } else {
                    for channel_name in self.channels.keys().cloned().collect::<Vec<_>>() {
                        if !self.is_channel_name(&channel_name) {
                            continue;
                        }
                        if self.send_privmsg_text(&channel_name, args) {
                            let msg =
                                ChatMessage::new_fmt(&self.my_nick, args, &self.timestamp_format);
                            self.add_message_to_channel(&channel_name, msg);
                        } else {
                            // A stale (kicked-from) tab must not swallow the
                            // broadcast silently.
                            self.add_message_to_channel(
                                &channel_name,
                                ChatMessage::system_fmt("Message not sent", &self.timestamp_format),
                            );
                        }
                    }
                }
            }

            "AME" => {
                // Send action to all channels
                if args.is_empty() {
                    self.add_message_to_current(ChatMessage::system("Usage: /ame <action>"));
                } else if !self.connected {
                    self.add_message_to_current(
                        ChatMessage::system_fmt(
                            "Not connected - action not sent",
                            &self.timestamp_format,
                        )
                        .without_logging(),
                    );
                } else {
                    for channel_name in self.channels.keys().cloned().collect::<Vec<_>>() {
                        if !self.is_channel_name(&channel_name) {
                            continue;
                        }
                        if self.send_action_text(&channel_name, args) {
                            let msg = ChatMessage::action_fmt(
                                &self.my_nick,
                                args,
                                &self.timestamp_format,
                            );
                            self.add_message_to_channel(&channel_name, msg);
                        } else {
                            self.add_message_to_channel(
                                &channel_name,
                                ChatMessage::system_fmt("Action not sent", &self.timestamp_format),
                            );
                        }
                    }
                }
            }

            "SAY" => {
                // Force send as message (even if starts with /)
                if args.is_empty() {
                    self.add_message_to_current(
                        ChatMessage::system("Usage: /say <message>").without_logging(),
                    );
                } else if let Some(channel) = &self.current_channel.clone() {
                    if self.connected {
                        if self.send_privmsg_text(channel, args) {
                            let msg =
                                ChatMessage::new_fmt(&self.my_nick, args, &self.timestamp_format);
                            self.add_message_to_channel(channel, msg);
                        } else {
                            self.add_message_to_current(
                                ChatMessage::system_fmt("Message not sent", &self.timestamp_format)
                                    .without_logging(),
                            );
                        }
                    } else {
                        self.add_message_to_current(
                            ChatMessage::system_fmt(
                                "Not connected - message not sent",
                                &self.timestamp_format,
                            )
                            .without_logging(),
                        );
                    }
                } else {
                    // The server tab has no target: say so rather than
                    // silently dropping the text (also reached via "//text").
                    self.add_message_to_current(
                        ChatMessage::system("No channel or query selected - message not sent")
                            .without_logging(),
                    );
                }
            }

            "DESCRIBE" => {
                // Send action to specific target: /describe <target> <action>
                // Whitespace separator: see the MSG arm (#148).
                let (target, action_text) = split_word(args);
                if !target.is_empty() && !action_text.is_empty() {
                    if self.connected {
                        if self.send_action_text(target, action_text) {
                            let msg = ChatMessage::action_fmt(
                                &self.my_nick,
                                action_text,
                                &self.timestamp_format,
                            );
                            self.add_message_to_channel(target, msg);
                        } else {
                            self.add_message_to_current(
                                ChatMessage::system_fmt("Action not sent", &self.timestamp_format)
                                    .without_logging(),
                            );
                        }
                    } else {
                        self.add_message_to_current(
                            ChatMessage::system_fmt(
                                "Not connected - action not sent",
                                &self.timestamp_format,
                            )
                            .without_logging(),
                        );
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /describe <target> <action>",
                    ));
                }
            }

            // === Channel Management ===
            "INVITE" => {
                let (nick, channel) = split_word(args);
                let channel = Some(channel)
                    .filter(|channel| !channel.is_empty())
                    .map(str::to_string)
                    .or_else(|| self.current_channel.clone())
                    .unwrap_or_default();
                if nick.is_empty() || channel.is_empty() {
                    self.add_message_to_current(ChatMessage::system(
                        "Usage: /invite <nick> [#channel]",
                    ));
                } else {
                    if self.send_command(IrcCommand::Invite(nick.to_string(), channel.clone())) {
                        self.add_message_to_current(ChatMessage::system(&format!(
                            "Inviting {} to {}",
                            nick, channel
                        )));
                    } else {
                        self.add_message_to_current(
                            ChatMessage::system(
                                "Invite not sent (not connected or command too long)",
                            )
                            .without_logging(),
                        );
                    }
                }
            }

            "CYCLE" | "REJOIN" | "HOP" => {
                let channel = if args.is_empty() {
                    self.current_channel.clone()
                } else {
                    Some(args.to_string())
                };
                if let Some(ch) = channel
                    && self.is_channel_name(&ch)
                {
                    // Get the stored key for auto-rejoin if available
                    let key = self
                        .channel_key(&ch)
                        .and_then(|stored| self.channels.get(&stored))
                        .and_then(|c| c.key.clone());
                    self.send_command(IrcCommand::Part(ch.clone(), Some("Cycling".to_string())));
                    // The own-PART handler drops the tab, so the key must
                    // travel with this JOIN to reach the replacement tab (#250).
                    self.send_join(ch, key);
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
                    if self.send_command(IrcCommand::Raw(format!("KNOCK {}", channel))) {
                        self.add_message_to_current(ChatMessage::system(&format!(
                            "Knocking on {}",
                            channel
                        )));
                    } else {
                        self.add_message_to_current(
                            ChatMessage::system(
                                "Knock request not sent (not connected or command too long)",
                            )
                            .without_logging(),
                        );
                    }
                } else {
                    self.add_message_to_current(ChatMessage::system("Usage: /knock <#channel>"));
                }
            }

            "CLOSE" | "WC" => {
                self.close_current_tab();
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
                } else if self.isupport_lacks(&["MONITOR", "WATCH"]) {
                    // Nothing is sent, so nothing is tracked locally either.
                    self.add_message_to_current(ChatMessage::system(
                        "Server supports neither MONITOR nor WATCH",
                    ));
                } else {
                    let (subcmd, targets) = monitor_args(args);
                    match subcmd.as_str() {
                        "+" => {
                            if let Some(targets) = &targets {
                                for nick in targets.split(',').filter(|nick| !nick.is_empty()) {
                                    self.monitored_users
                                        .insert(self.network_support.canonicalize(nick));
                                }
                            }
                        }
                        "-" => {
                            if let Some(targets) = &targets {
                                for nick in targets.split(',') {
                                    self.monitored_users
                                        .remove(&self.network_support.canonicalize(nick));
                                }
                            }
                        }
                        "C" => self.monitored_users.clear(),
                        _ => {}
                    }
                    // MONITOR wins when both are advertised; WATCH is the
                    // legacy fallback. With no ISUPPORT yet, MONITOR is assumed.
                    if self.network_support.has_isupport("MONITOR")
                        || !self.network_support.has_isupport("WATCH")
                    {
                        self.send_command(IrcCommand::Monitor(subcmd, targets));
                    } else {
                        let wire = match targets {
                            Some(targets) if subcmd == "+" || subcmd == "-" => targets
                                .split(',')
                                .filter(|nick| !nick.is_empty())
                                .map(|nick| format!("{subcmd}{nick}"))
                                .collect::<Vec<_>>()
                                .join(" "),
                            _ => subcmd,
                        };
                        self.send_command(IrcCommand::Raw(format!("WATCH {wire}")));
                    }
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
                    // Handles "host", "host [+]port", "host:[+]port",
                    // "[v6::1]:[+]port", and bare IPv6 literals (which contain
                    // multiple colons and must not be split on the first one).
                    let ServerArg { host, port, tls } = match parse_server_arg(args) {
                        Ok(endpoint) => endpoint,
                        Err(error) => {
                            self.add_server_message(ChatMessage::system_fmt(
                                &error,
                                &self.timestamp_format,
                            ));
                            return;
                        }
                    };
                    let selected_port = port.as_deref().unwrap_or(&self.server_port);
                    if host.trim().is_empty() {
                        self.add_server_message(ChatMessage::system_fmt(
                            "Server host cannot be empty",
                            &self.timestamp_format,
                        ));
                        return;
                    }
                    if let Err(error) = parse_server_port(selected_port) {
                        self.add_server_message(ChatMessage::system_fmt(
                            &error,
                            &self.timestamp_format,
                        ));
                        return;
                    }
                    // /server is deliberately unprofiled. /reconnect preserves
                    // the active profile; /server never inherits its secrets,
                    // even if the user names the same host and port again.
                    // The new session is snapshotted from the form, so the
                    // form is only borrowed for the switch: afterwards it
                    // goes back to describing the saved profile, or the next
                    // save would persist the cleared secrets (and the new
                    // host next to the old network's credentials) (#234).
                    let saved_profile = self.take_endpoint_form();
                    self.clear_endpoint_credentials();
                    self.server_host = host;
                    if let Some(port) = port {
                        self.server_port = port;
                    }
                    if let Some(tls) = tls {
                        self.use_tls = tls;
                    }
                    let target = format!("{}:{}", self.server_host, self.server_port);
                    let switched = self.switch_session_from_form("Changing servers");
                    self.restore_endpoint_form(saved_profile);
                    if !switched {
                        return;
                    }
                    self.add_server_message(ChatMessage::system(&format!(
                        "Connecting to {target}..."
                    )));
                } else {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Current server: {}",
                        self.active_endpoint_label()
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
                    let mask_key = self.network_support.canonicalize(&mask);
                    if !self
                        .ignore_list
                        .iter()
                        .any(|m| self.network_support.canonicalize(m) == mask_key)
                    {
                        self.ignore_list_lower.push(mask_key);
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
                    let mask_key = self.network_support.canonicalize(args);
                    let case_mapping = self.network_support.case_mapping;
                    let before_len = self.ignore_list.len();
                    self.ignore_list
                        .retain(|m| case_mapping.canonicalize(m) != mask_key);
                    if self.ignore_list.len() < before_len {
                        self.ignore_list_lower = self
                            .ignore_list
                            .iter()
                            .map(|s| case_mapping.canonicalize(s))
                            .collect();
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
                        self.add_message_to_current(
                            ChatMessage::system(match_line).without_logging(),
                        );
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
                            .map(|(i, line)| {
                                let shown = if command_line_contains_credentials(line) {
                                    "<credential command redacted>"
                                } else {
                                    line
                                };
                                format!("  {}. {}", i + 1, shown)
                            })
                            .collect();
                        self.add_message_to_current(
                            ChatMessage::system("Auto-perform commands:").without_logging(),
                        );
                        for line in lines {
                            self.add_message_to_current(
                                ChatMessage::system(&line).without_logging(),
                            );
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
                    self.add_message_to_current(
                        ChatMessage::system("Added command to auto-perform.").without_logging(),
                    );
                }
            }

            "SETTINGS" => {
                self.show_settings = true;
            }

            "CPRIVMSG" | "CNOTICE" => {
                let (target, rest) = split_word(args);
                let (channel, text) = split_word(rest);
                if target.is_empty() || channel.is_empty() || text.is_empty() {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Usage: /{} <nick> <channel> <message>",
                        cmd.to_ascii_lowercase()
                    )));
                } else {
                    let sent = self
                        .send_command(IrcCommand::Raw(format!("{cmd} {target} {channel} :{text}")));
                    self.report_send_failure(sent, "Not connected - server command not sent");
                }
            }

            "WALLCHOPS" | "WALLVOICES" => {
                let (channel, text) = split_word(args);
                if channel.is_empty() || text.is_empty() {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Usage: /{} <channel> <message>",
                        cmd.to_ascii_lowercase()
                    )));
                } else {
                    let sent =
                        self.send_command(IrcCommand::Raw(format!("{cmd} {channel} :{text}")));
                    self.report_send_failure(sent, "Not connected - server command not sent");
                }
            }

            "SHELP" => {
                let line = if args.is_empty() {
                    "HELP".to_string()
                } else {
                    format!("HELP {args}")
                };
                let sent = self.send_command(IrcCommand::Raw(line));
                self.report_send_failure(sent, "Not connected - server command not sent");
            }

            "HELP" | "H" | "?" => {
                self.show_help();
            }

            _ => {
                if !cmd.is_empty()
                    && cmd.bytes().all(|byte| byte.is_ascii_alphabetic())
                    && !args.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | 0))
                {
                    let line = if args.is_empty() {
                        cmd.to_string()
                    } else {
                        format!("{cmd} {args}")
                    };
                    let sent = self.send_command(IrcCommand::Raw(line));
                    self.report_send_failure(sent, "Not connected - server command not sent");
                } else {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Invalid command: {}. Type /help for list.",
                        cmd
                    )));
                }
            }
        }
    }

    /// Send a message to a network service (NickServ, ChanServ, ...) into its
    /// query window, with explicit feedback when disconnected: silently
    /// dropping e.g. "/ns identify <pass>" would leave the user believing they
    /// are identified.
    fn send_service_message(&mut self, service: &str, args: &str) {
        if self.connected {
            if self.send_privmsg_text(service, args) {
                self.add_message_to_channel(
                    service,
                    outgoing_local_echo(service, &self.my_nick, args, &self.timestamp_format),
                );
            } else {
                self.add_message_to_current(
                    ChatMessage::system_fmt("Message not sent", &self.timestamp_format)
                        .without_logging(),
                );
            }
        } else {
            self.add_message_to_current(
                ChatMessage::system_fmt("Not connected - message not sent", &self.timestamp_format)
                    .without_logging(),
            );
        }
    }

    /// True once the server's ISUPPORT has arrived and advertises none of
    /// `tokens`. Before that nothing is known (an early /perform line can run
    /// ahead of the 005 burst), so callers keep sending rather than claim the
    /// feature is missing.
    fn isupport_lacks(&self, tokens: &[&str]) -> bool {
        !self.network_support.isupport.is_empty()
            && !tokens
                .iter()
                .any(|token| self.network_support.has_isupport(token))
    }

    fn ban_mask_for(&self, value: &str) -> String {
        if is_extended_ban(value) || value.contains(['!', '@', '*']) {
            return nick_to_mask(value);
        }
        self.current_channel
            .as_deref()
            .and_then(|channel| self.channel_key(channel))
            .and_then(|channel| self.channels.get(&channel))
            .and_then(|channel| channel.get_user(value))
            .and_then(|user| match (&user.username, &user.hostname) {
                (Some(username), Some(hostname)) => Some(format!("*!{username}@{hostname}")),
                _ => None,
            })
            .unwrap_or_else(|| nick_to_mask(value))
    }

    /// How many mode changes one MODE line may carry: the server's MODES=
    /// limit (capped), or one when the server never advertised it.
    fn modes_per_line(&self) -> usize {
        match self.network_support.isupport.get("MODES") {
            Some(Some(value)) if !value.is_empty() => value
                .parse::<usize>()
                .ok()
                .filter(|limit| *limit > 0)
                .map_or(1, |limit| limit.min(MAX_UNBAN_MASKS)),
            // A bare MODES (or MODES=) means the server sets no limit.
            Some(_) => MAX_UNBAN_MASKS,
            None => 1,
        }
    }

    /// Resolve `/unban <value>` to the masks to remove. A hostmask or extended
    /// ban is used as typed. A bare nick is looked up in the channel's ban
    /// list, because `/ban nick` sets a mask built from the user's host, which
    /// `nick!*@*` would never undo (#235). Exact entries win: bans naming the
    /// nick, or exactly the mask `/ban` builds. Only when there are none does
    /// it fall back to the single narrowest ban that merely covers the user,
    /// and never a catch-all like `*!*@*` that someone else set on purpose.
    fn unban_masks_for(&self, value: &str) -> UnbanPlan {
        if is_extended_ban(value) || value.contains(['!', '@', '*']) {
            return UnbanPlan {
                masks: vec![nick_to_mask(value)],
                note: None,
            };
        }
        let built = self.ban_mask_for(value);
        let channel = self
            .current_channel
            .as_deref()
            .and_then(|channel| self.channel_key(channel))
            .and_then(|channel| self.channels.get(&channel));
        let mut broad_only: Option<String> = None;
        if let Some(channel) = channel {
            let nick_key = self.network_support.canonicalize(value);
            let built_key = self.network_support.canonicalize(&built);
            let identity =
                channel
                    .get_user(value)
                    .and_then(|user| match (&user.username, &user.hostname) {
                        (Some(username), Some(hostname)) => Some(
                            self.network_support
                                .canonicalize(&format!("{}!{username}@{hostname}", user.nick)),
                        ),
                        _ => None,
                    });
            let mut exact: Vec<String> = Vec::new();
            let mut covering: Vec<&str> = Vec::new();
            for ban in &channel.bans {
                let mask = self.network_support.canonicalize(&ban.mask);
                // A ban naming this nick (also once the user has left), the
                // mask /ban builds, or the user's full identity.
                if mask
                    .split_once('!')
                    .is_some_and(|(nick, _)| nick == nick_key)
                    || mask == built_key
                    || identity.as_deref() == Some(mask.as_str())
                {
                    exact.push(ban.mask.clone());
                } else if identity
                    .as_deref()
                    .is_some_and(|identity| mask_matches(&mask, identity))
                {
                    covering.push(&ban.mask);
                }
            }
            if !exact.is_empty() {
                let extra = exact.len().saturating_sub(MAX_UNBAN_MASKS);
                exact.truncate(MAX_UNBAN_MASKS);
                let mut note = format!("Unbanning {value}: {}", exact.join(", "));
                if extra > 0 {
                    note.push_str(&format!(" (and {extra} more - run /unban again)"));
                }
                return UnbanPlan {
                    masks: exact,
                    note: Some(note),
                };
            }
            // min_by_key keeps the first of equally specific entries.
            if let Some(narrowest) = covering
                .iter()
                .copied()
                .filter(|mask| ban_specificity(mask) > 0)
                .min_by_key(|mask| std::cmp::Reverse(ban_specificity(mask)))
            {
                return UnbanPlan {
                    masks: vec![narrowest.to_string()],
                    note: Some(format!(
                        "No ban names {value} exactly; removing the narrowest ban that covers \
                         them: {narrowest}"
                    )),
                };
            }
            broad_only = covering.first().map(|mask| mask.to_string());
        }
        let note = match broad_only {
            Some(broad) => format!(
                "Only the catch-all ban {broad} covers {value}, so it was left alone. \
                 Use /unban {broad} to remove it deliberately."
            ),
            None => format!("No matching ban in the list; sending MODE -b {built}"),
        };
        UnbanPlan {
            masks: vec![built],
            note: Some(note),
        }
    }

    /// Secrets and automation belong to one configured network profile. An
    /// unprofiled endpoint change must never inherit them from the old server.
    pub(crate) fn clear_endpoint_credentials(&mut self) {
        self.password.clear();
        self.sasl_username.clear();
        self.sasl_password.clear();
        self.sasl_mechanism = crate::irc::client::SaslMechanism::Auto;
        self.sasl_required = false;
        self.tls_client_cert_path.clear();
        self.tls_client_key_path.clear();
        self.auto_join_channels.clear();
        self.auto_perform.clear();
        self.pre_away_message.clear();
        self.persistence_profile.clear();
        self.pending_auto_perform = None;
        self.accept_invalid_certs = false;
    }

    /// Snapshot the connection-form fields that make up the saved network
    /// profile (endpoint, secrets, automation). Deliberately not `Debug`.
    fn take_endpoint_form(&self) -> EndpointForm {
        EndpointForm {
            server_host: self.server_host.clone(),
            server_port: self.server_port.clone(),
            use_tls: self.use_tls,
            accept_invalid_certs: self.accept_invalid_certs,
            password: self.password.clone(),
            sasl_username: self.sasl_username.clone(),
            sasl_password: self.sasl_password.clone(),
            sasl_mechanism: self.sasl_mechanism,
            sasl_required: self.sasl_required,
            tls_client_cert_path: self.tls_client_cert_path.clone(),
            tls_client_key_path: self.tls_client_key_path.clone(),
            auto_join_channels: self.auto_join_channels.clone(),
            auto_perform: self.auto_perform.clone(),
            pre_away_message: self.pre_away_message.clone(),
            persistence_profile: self.persistence_profile.clone(),
        }
    }

    fn restore_endpoint_form(&mut self, form: EndpointForm) {
        self.server_host = form.server_host;
        self.server_port = form.server_port;
        self.use_tls = form.use_tls;
        self.accept_invalid_certs = form.accept_invalid_certs;
        self.password = form.password;
        self.sasl_username = form.sasl_username;
        self.sasl_password = form.sasl_password;
        self.sasl_mechanism = form.sasl_mechanism;
        self.sasl_required = form.sasl_required;
        self.tls_client_cert_path = form.tls_client_cert_path;
        self.tls_client_key_path = form.tls_client_key_path;
        self.auto_join_channels = form.auto_join_channels;
        self.auto_perform = form.auto_perform;
        self.pre_away_message = form.pre_away_message;
        self.persistence_profile = form.persistence_profile;
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
                    "/ban <nick|mask>    - Ban user or mask/extban (+b)",
                    "/unban <nick|mask>  - Remove ban (-b)",
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
                    "/say <text>         - Send as message (even if /; also //text)",
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
                    "/server <host> [[+]port] - Connect to a server (saved settings unchanged)",
                    "  Port 6697 or +port (e.g. +7000) uses TLS, other ports plain text;",
                    "  with no port the current port and TLS setting are kept",
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
                    "/history [#c] [n]  - Load/paginate server history",
                    "/ignore [mask]      - List or add to ignore list",
                    "/unignore <mask>    - Remove from ignore list",
                    "/perform [cmd]      - View/add auto-perform",
                    "/echo <text>        - Echo to window",
                    "/raw <command>      - Send raw IRC",
                    "/settings           - Open settings",
                    "/slap <nick> [text] - Slap (default: large trout)",
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
                    "/cprivmsg <n> <c> <text> - Context private message",
                    "/cnotice <n> <c> <text>  - Context notice",
                    "/wallchops <c> <text>    - Message channel ops",
                    "/wallvoices <c> <text>   - Message voiced users",
                    "/shelp [topic]      - Request server help",
                ],
            ),
            (
                "=== IRCv3 / Nefarious ===",
                vec![
                    "/history before|after|around <target> <ref> [n]",
                    "/history between <target> <ref1> <ref2> [n]",
                    "/history targets <ref1> <ref2> [n]",
                    "/markread [target] [timestamp=<time>|*]",
                    "/redact [target] <msgid> [reason]",
                    "/rename <#new-channel> [reason]",
                    "/relocate <#new-channel> [reason]",
                    "/setname <real name>",
                    "/reply <msgid> <message>",
                    "/react <msgid> <reaction>",
                    "/typing active|paused|done",
                    "/label <label> <IRC command>",
                    "/bouncer <subcommand> ...",
                    "/persistence <subcommand> ...",
                    "/metadata <subcommand> ...",
                    "/webpush <subcommand> ...",
                    "/register <account> <email|*> <password>",
                    "/verify <account> <code>",
                    "/token servicelist|generate|validate ...",
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

/// The outcome of resolving `/unban <nick>`: the masks to remove and a note
/// telling the user what was chosen (or deliberately left alone).
struct UnbanPlan {
    masks: Vec<String>,
    note: Option<String>,
}

/// The most masks one `/unban` removes, and the most mode changes it packs
/// into a single MODE line, whatever the ban list or the server's MODES= says.
const MAX_UNBAN_MASKS: usize = 12;

/// How many literal characters a ban mask pins down. A mask made only of
/// wildcards and separators (`*!*@*`, `*!*@*.*`) scores 0: a catch-all.
fn ban_specificity(mask: &str) -> usize {
    mask.chars()
        .filter(|c| !matches!(c, '*' | '?' | '!' | '@' | '.'))
        .count()
}

/// Split off the first whitespace-delimited word. The remainder has its
/// leading whitespace removed, so a run of spaces or tabs between arguments
/// never produces an empty argument (#263).
fn split_word(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    match text.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim_start()),
        None => (text, ""),
    }
}

fn valid_message_reference(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b';' | b'\0'))
}

fn escape_message_tag_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            ';' => escaped.push_str("\\:"),
            ' ' => escaped.push_str("\\s"),
            '\\' => escaped.push_str("\\\\"),
            '\r' => escaped.push_str("\\r"),
            '\n' => escaped.push_str("\\n"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Recognize an authentication or channel service target: canonical names,
/// common shorthands (/msg NS ...), and network-qualified nicks
/// (NickServ@services.example). These services take account passwords and
/// channel keys, so anything sent to them is treated as sensitive; the
/// keyword gate below keeps false positives negligible for same-named users.
fn is_auth_service_target(raw_target: &str) -> bool {
    matches!(
        raw_target
            .trim_start_matches(['~', '&', '@', '%', '+'])
            .split('@')
            .next()
            .unwrap_or(raw_target)
            .to_ascii_lowercase()
            .as_str(),
        "nickserv" | "ns" | "authserv" | "as" | "chanserv" | "cs" | "q" | "x"
    )
}

/// The auth-command half of the credential test, without the service-target
/// gate. Targetless senders (/me, /say) use it alone — for those,
/// over-redaction (a `/me identify as the night` kept out of history) is the
/// safe direction.
pub(super) fn content_has_auth_command(message: &str) -> bool {
    let mut words = message.trim_start_matches(':').split_whitespace();
    let command = words.next().unwrap_or("").to_ascii_uppercase();
    // NOTE: keywords here also gate replay-row drops and log suppression for
    // peers on service-shaped nick aliases (q, x, ns, cs…) — bare PASS can
    // over-redact an ordinary "Pass me the file" from such a peer, but the
    // invariant is over-redaction (a leaked password cannot be recalled;
    // #167 weighed both and kept PASS).
    if matches!(
        command.as_str(),
        "IDENTIFY"
            | "ID"
            | "SIDENTIFY"
            | "AUTH"
            | "AUTHENTICATE"
            | "LOGIN"
            | "PASS"
            | "REGISTER"
            | "GHOST"
            | "REGAIN"
            | "RECOVER"
            | "RELEASE"
            | "GROUP"
            | "SETPASS"
            | "RESETPASS"
            | "NEWPASS"
    ) {
        return true;
    }
    match command.as_str() {
        // Atheme `DROP <nick> <password>` and Anope `DROP #chan`. DROP is an
        // ordinary word, so only the service shapes count: a channel
        // argument, or two or more arguments (one may be the password).
        "DROP" => match (words.next(), words.next()) {
            (Some(first), second) => second.is_some() || first.starts_with(['#', '&', '!']),
            (None, _) => false,
        },
        // `SET PASSWORD <pw>`, `SET #chan PASSWORD <pw>` and Anope's
        // `SASET <nick> PASSWORD <pw>`: the keyword is the first or second
        // word after the command.
        "SET" | "SASET" => words
            .take(2)
            .any(|word| word.eq_ignore_ascii_case("PASSWORD") || word.eq_ignore_ascii_case("PASS")),
        _ => false,
    }
}

pub(super) fn service_message_contains_credentials(target: &str, message: &str) -> bool {
    // IRC permits comma-separated message targets and network-qualified nicks
    // (NickServ@services.example). Treat a command as sensitive if any target
    // is an authentication or channel service (ChanServ IDENTIFY/REGISTER
    // carries channel keys and passwords too).
    if !target.split(',').any(is_auth_service_target) {
        return false;
    }

    content_has_auth_command(message)
}

pub(super) fn command_line_contains_credentials(line: &str) -> bool {
    let line = line.trim().trim_start_matches('/');
    let mut parts = line.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or("");
    let args = parts.next().unwrap_or("").trim_start();
    // A non-ASCII command word never dispatches (process_command refuses it),
    // but the unsent line still holds whatever the user typed: fold it fully
    // so the secret stays out of history.
    let command = if command.is_ascii() {
        command.to_ascii_uppercase()
    } else {
        command.to_uppercase()
    };
    match command.as_str() {
        "PASS" | "AUTH" | "LOGIN" | "AUTHENTICATE" | "OPER" | "REGISTER" | "VERIFY" | "WEBPUSH"
        | "AUTHTOKEN" | "BOUNCER" => true,
        // Bare service commands (/identify pw, /ghost nick pw, ...) fall
        // through to the catch-all arm and reach the wire as-is (#226).
        "IDENTIFY" | "ID" | "SIDENTIFY" | "GHOST" | "RECOVER" | "RELEASE" | "REGAIN" | "GROUP"
        | "SETPASS" => true,
        "TOKEN" => args
            .split_whitespace()
            .next()
            .is_some_and(|subcommand| subcommand.eq_ignore_ascii_case("VALIDATE")),
        "NS" | "NICKSERV" => service_message_contains_credentials("NickServ", args),
        "CS" | "CHANSERV" => service_message_contains_credentials("ChanServ", args),
        "AS" | "AUTHSERV" => service_message_contains_credentials("AuthServ", args),
        "MSG" | "PRIVMSG" | "QUERY" | "Q" | "NOTICE" | "N" | "DESCRIBE" | "SQUERY" => {
            // First word is the target, rest is the text (DESCRIBE sends it
            // as a CTCP ACTION; SQUERY addresses a service by name).
            let mut message = args.splitn(2, char::is_whitespace);
            let target = message.next().unwrap_or("");
            let content = message.next().unwrap_or("").trim_start();
            service_message_contains_credentials(target, content)
        }
        "ME" | "SAY" => content_has_auth_command(args),
        "RAW" | "QUOTE" => {
            // A raw line may lead with IRCv3 tags ("@label=1 PRIVMSG ..."):
            // without skipping them the tag token reads as the command.
            let untagged = if args.starts_with('@') {
                args.split_once(char::is_whitespace)
                    .map_or("", |(_, rest)| rest.trim_start())
            } else {
                args
            };
            command_line_contains_credentials(untagged)
        }
        "LABEL" => {
            let nested = args
                .split_once(char::is_whitespace)
                .map(|(_, command)| command)
                .unwrap_or("");
            command_line_contains_credentials(nested)
        }
        "PERFORM" if !args.eq_ignore_ascii_case("clear") => command_line_contains_credentials(args),
        "SERVER" => server_args_carry_secret(args),
        _ => false,
    }
}

fn outgoing_local_echo(target: &str, sender: &str, content: &str, format: &str) -> ChatMessage {
    if service_message_contains_credentials(target, content) {
        ChatMessage::new_fmt(sender, "<credential command sent>", format).without_logging()
    } else {
        ChatMessage::new_fmt(sender, content, format)
    }
}

/// Split /monitor arguments into (subcommand, targets), accepting both the
/// documented "+ nick1,nick2" form and the attached "+nick1,nick2" form
/// (which would otherwise send the sign as part of the nick: "MONITOR + +nick").
/// Nick lists are comma-separated on the wire; the user types them with any
/// whitespace (tabs from a paste included — #163).
fn whitespace_to_commas(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(",")
}

/// The wire target for a /whois query: a single nick is doubled so the reply
/// comes from the user's own server (real host behind a cloak, idle/signon —
/// #164). Separator-carrying input passes through unchanged: a manual double
/// target, a comma list, or a nick/mask with wildcards or !/@ — doubling a
/// mask makes solanum-family ircds parse the first parameter as a routing
/// target and fail with ERR_NOSUCHSERVER (#167).
pub(super) fn whois_query_target(args: &str) -> String {
    let args = args.trim();
    let single = args.split_whitespace().count() == 1 && !args.contains([',', '*', '?', '!', '@']);
    if single {
        format!("{args} {args}")
    } else {
        args.to_string()
    }
}

fn monitor_args(args: &str) -> (String, Option<String>) {
    // Whitespace separator: see process_command's command-token split (#159).
    let parts: Vec<&str> = args.splitn(2, char::is_whitespace).collect();
    let first = parts[0];
    let rest = parts
        .get(1)
        .map(|s| whitespace_to_commas(s))
        .filter(|value| !value.is_empty());

    let subcmd = first.to_uppercase();
    match subcmd.as_str() {
        // The separated-sign form carries a nick list in `rest` too: it must
        // reach the wire comma-separated (#163/#166).
        "+" | "-" | "C" | "L" | "S" => (subcmd, rest),
        _ => {
            if let Some(stripped) = first.strip_prefix(['+', '-']) {
                let sign = first[..1].to_string();
                let mut targets = stripped.to_string();
                if let Some(more) = rest {
                    // "/monitor +a b" -> targets are comma-separated on the wire.
                    targets.push(',');
                    targets.push_str(&more);
                }
                (sign, Some(targets))
            } else {
                // Bare nick(s): assume add.
                ("+".to_string(), Some(whitespace_to_commas(args)))
            }
        }
    }
}

/// Parse `/mode` while supporting both explicit targets and the documented
/// shorthand `/mode +m`, which applies to the current channel.
fn mode_command_args(
    args: &str,
    current_channel: Option<&str>,
) -> Option<(String, Option<String>, Vec<String>)> {
    let mut parts = args.split_whitespace();
    let first = parts.next()?;
    if first.starts_with(['+', '-']) {
        let target = current_channel?;
        return Some((
            target.to_string(),
            Some(first.to_string()),
            parts.map(str::to_string).collect(),
        ));
    }

    Some((
        first.to_string(),
        parts.next().map(str::to_string),
        parts.map(str::to_string).collect(),
    ))
}

/// The connection-form fields that make up one saved network profile, held
/// while `/server` borrows the form for a one-off session. Deliberately not
/// `Debug`: it carries credentials.
struct EndpointForm {
    server_host: String,
    server_port: String,
    use_tls: bool,
    accept_invalid_certs: bool,
    password: String,
    sasl_username: String,
    sasl_password: String,
    sasl_mechanism: crate::irc::client::SaslMechanism,
    sasl_required: bool,
    tls_client_cert_path: String,
    tls_client_key_path: String,
    auto_join_channels: String,
    auto_perform: String,
    pre_away_message: String,
    persistence_profile: String,
}

/// The standard TLS port: a `/server` that names it connects with TLS.
const TLS_PORT: u16 = 6697;

/// The endpoint named by `/server`. `tls` is `Some` only when a port was
/// typed: `+port` (mIRC's TLS marker) or the standard TLS port means TLS and
/// any other port means plain text. With no port the form's port and TLS
/// setting are kept together, so `/server host` cannot pair one with the
/// other's default (#234).
#[derive(Debug, PartialEq)]
struct ServerArg {
    host: String,
    port: Option<String>,
    tls: Option<bool>,
}

impl ServerArg {
    fn new(host: &str, port: Option<(u16, bool)>) -> Self {
        Self {
            host: host.to_string(),
            port: port.map(|(port, _)| port.to_string()),
            tls: port.map(|(_, tls)| tls),
        }
    }
}

/// Parse a port typed after `/server`, returning it with whether it selects
/// TLS. Errors never echo the input: in mIRC's `/server host port password`
/// order it can be a password (#226).
fn parse_server_arg_port(value: &str) -> Result<(u16, bool), String> {
    let value = value.trim();
    let (marked, digits) = match value.strip_prefix('+') {
        Some(digits) => (true, digits),
        None => (false, value),
    };
    match digits.parse::<u16>() {
        Ok(port) if port > 0 && digits.bytes().all(|byte| byte.is_ascii_digit()) => {
            Ok((port, marked || port == TLS_PORT))
        }
        _ => Err("Invalid server port; enter a number from 1 to 65535 \
                  (prefix it with + for TLS)"
            .to_string()),
    }
}

/// True when the text after `/server` carries more than an endpoint, such as
/// the password of mIRC's `/server host port password`: anything after the
/// host that is not a single (optionally `+`) port. Such a line must not be
/// echoed or kept in history (#226).
pub(super) fn server_args_carry_secret(args: &str) -> bool {
    let mut parts = args.split_whitespace();
    parts.next();
    match (parts.next(), parts.next()) {
        (None, _) => false,
        (Some(port), None) => parse_server_arg_port(port).is_err(),
        (Some(_), Some(_)) => true,
    }
}

/// Parse the /server argument: "host", "host [+]port", "host:[+]port",
/// "[v6::addr]:[+]port", or a bare IPv6 literal (multiple colons; must not be
/// split on the first one).
fn parse_server_arg(args: &str) -> Result<ServerArg, String> {
    let args = args.trim();
    // Space-separated form: "host 6697" (works for any host including IPv6;
    // any whitespace, per the command-token split — #159).
    if let Some((host, port)) = args.split_once(char::is_whitespace) {
        let host = host.trim_matches(['[', ']']);
        return Ok(ServerArg::new(host, Some(parse_server_arg_port(port)?)));
    }
    // Bracketed IPv6 with optional port: "[::1]:6697" or "[::1]".
    if let Some(rest) = args.strip_prefix('[') {
        if let Some((host, after)) = rest.split_once(']') {
            let port = match after.strip_prefix(':') {
                Some(port) => Some(parse_server_arg_port(port)?),
                None if after.is_empty() => None,
                None => {
                    return Err("Invalid server endpoint; use host, host port, host:port \
                                or [v6::addr]:port"
                        .to_string());
                }
            };
            return Ok(ServerArg::new(host, port));
        }
        return Ok(ServerArg::new(rest, None));
    }
    // "host:port" - only with exactly one colon and a numeric port; an IPv6
    // literal has several colons and falls through as a bare host.
    if args.chars().filter(|&c| c == ':').count() == 1
        && let Some((host, port)) = args.rsplit_once(':')
    {
        if host.is_empty() {
            return Err("Server host cannot be empty".to_string());
        }
        return Ok(ServerArg::new(host, Some(parse_server_arg_port(port)?)));
    }
    Ok(ServerArg::new(args, None))
}

pub(crate) fn parse_server_port(value: &str) -> Result<u16, String> {
    match value.trim().parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err(format!(
            "Invalid server port {:?}; enter a number from 1 to 65535",
            value.trim()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::types::Channel;
    use tokio::sync::mpsc;

    fn connected_command_app() -> (IrcApp, mpsc::UnboundedReceiver<IrcCommand>) {
        let mut app = IrcApp::with_settings(crate::gui::Settings::default());
        let (tx, rx) = mpsc::unbounded_channel();
        app.cmd_tx = Some(tx);
        app.connected = true;
        (app, rx)
    }

    #[test]
    fn monitor_args_accepts_attached_sign() {
        assert_eq!(
            monitor_args("+ friend"),
            ("+".into(), Some("friend".into()))
        );
        assert_eq!(monitor_args("+friend"), ("+".into(), Some("friend".into())));
        assert_eq!(monitor_args("-friend"), ("-".into(), Some("friend".into())));
        assert_eq!(monitor_args("+a,b c"), ("+".into(), Some("a,b,c".into())));
        assert_eq!(monitor_args("L"), ("L".into(), None));
        assert_eq!(monitor_args("friend"), ("+".into(), Some("friend".into())));
        // Separated-sign lists and whitespace-only tails (#166).
        assert_eq!(
            monitor_args("+ alice bob"),
            ("+".into(), Some("alice,bob".into()))
        );
        assert_eq!(monitor_args("+a\t"), ("+".into(), Some("a".into())));
    }

    #[test]
    fn mode_args_support_current_channel_shorthand() {
        assert_eq!(
            mode_command_args("+m", Some("#room")),
            Some(("#room".into(), Some("+m".into()), Vec::new()))
        );
        assert_eq!(
            mode_command_args("+kl secret 20", Some("#room")),
            Some((
                "#room".into(),
                Some("+kl".into()),
                vec!["secret".into(), "20".into()]
            ))
        );
        assert_eq!(
            mode_command_args("#other +m", Some("#room")),
            Some(("#other".into(), Some("+m".into()), Vec::new()))
        );
        assert_eq!(
            mode_command_args("#other", Some("#room")),
            Some(("#other".into(), None, Vec::new()))
        );
        assert_eq!(mode_command_args("+m", None), None);
    }

    #[test]
    fn hop_cycles_channel_while_halfop_remains_a_mode_command() {
        let (mut app, mut rx) = connected_command_app();
        let mut channel = Channel::new();
        channel.key = Some("secret-key".into());
        app.channels.insert("#Room".into(), channel);
        app.current_channel = Some("#Room".into());

        app.process_command("/hop");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Part("#Room".into(), Some("Cycling".into()))
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Join("#Room".into(), Some("secret-key".into()), None, None)
        );

        app.process_command("/halfop alice");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Mode("#Room".into(), Some("+h".into()), vec!["alice".into()])
        );
    }

    #[test]
    fn targetless_mode_queues_current_channel() {
        let (mut app, mut rx) = connected_command_app();
        app.current_channel = Some("#room".into());
        app.process_command("/mode +m");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Mode("#room".into(), Some("+m".into()), Vec::new())
        );
    }

    #[test]
    fn history_command_uses_negotiated_server_limit_and_opens_public_channel() {
        let (mut app, mut rx) = connected_command_app();
        app.handle_cap_message("LS", &["draft/chathistory=limit=50,retention=30d".into()]);
        app.handle_cap_message("ACK", &["draft/chathistory".into()]);

        app.process_command("/history #public 500");

        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryLatest("#public".into(), "*".into(), 50)
        );
        assert_eq!(app.current_channel.as_deref(), Some("#public"));
        assert!(!app.channels["#public"].joined);
    }

    #[test]
    fn reply_reaction_and_typing_use_client_only_tags() {
        let (mut app, mut rx) = connected_command_app();
        app.current_channel = Some("#room".into());
        app.channels.insert("#room".into(), Channel::new());
        app.handle_cap_message("ACK", &["message-tags".into()]);

        app.process_command("/reply abc hello there");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("@+reply=abc PRIVMSG #room :hello there".into())
        );
        app.process_command("/react abc 👍");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("@+draft/react=👍;+reply=abc TAGMSG #room".into())
        );
        app.process_command("/typing active");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("@+typing=active TAGMSG #room".into())
        );
    }

    #[test]
    fn tab_separated_args_parse_like_spaces_in_tag_and_channel_commands() {
        let (mut app, mut rx) = connected_command_app();
        app.current_channel = Some("#room".into());
        app.channels.insert("#room".into(), Channel::new());
        app.handle_cap_message("ACK", &["message-tags labeled-response".into()]);

        app.process_command("/reply abc\thello there");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("@+reply=abc PRIVMSG #room :hello there".into())
        );
        app.process_command("/react abc\t👍");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("@+draft/react=👍;+reply=abc TAGMSG #room".into())
        );
        app.process_command("/label lbl1\tWHOIS alice");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("@label=lbl1 WHOIS alice".into())
        );
        app.process_command("/redact abc123\tspam");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Redact("#room".into(), "abc123".into(), Some("spam".into()))
        );
        app.process_command("/rename #newroom\trenamed reason");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Rename(
                "#room".into(),
                "#newroom".into(),
                Some("renamed reason".into())
            )
        );
        app.process_command("/relocate #otherroom\tmoved reason");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Relocate(
                "#room".into(),
                "#otherroom".into(),
                Some("moved reason".into())
            )
        );
        app.process_command("/ctcp bob\tVERSION");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Privmsg("bob".into(), "\x01VERSION\x01".into())
        );
    }

    /// An app in `#chan` with `alice` (`~alice@host.example`) present.
    fn ban_command_app() -> (IrcApp, mpsc::UnboundedReceiver<IrcCommand>) {
        let (mut app, rx) = connected_command_app();
        let mut channel = Channel::new();
        channel.joined = true;
        channel.add_user("alice", crate::gui::types::UserMode::Normal);
        app.channels.insert("#chan".into(), channel);
        app.update_user_identity(
            "alice",
            Some(Some("~alice".into())),
            Some(Some("host.example".into())),
            None,
            None,
            None,
            None,
            Some(true),
        );
        app.current_channel = Some("#chan".into());
        (app, rx)
    }

    fn expect_mode(rx: &mut mpsc::UnboundedReceiver<IrcCommand>, mode: &str, mask: &str) {
        match rx.try_recv() {
            Ok(IrcCommand::Mode(channel, Some(sent_mode), params)) => {
                assert_eq!(channel, "#chan");
                assert_eq!(sent_mode, mode);
                assert_eq!(params, vec![mask.to_string()]);
            }
            other => panic!("expected MODE {mode} {mask}, got {other:?}"),
        }
    }

    #[test]
    fn unban_nick_undoes_the_mask_ban_set() {
        // /ban resolves alice's host, so /unban alice must remove that mask
        // rather than the never-set alice!*@* (#235).
        let (mut app, mut rx) = ban_command_app();
        app.process_command("/ban alice");
        expect_mode(&mut rx, "+b", "*!~alice@host.example");
        app.channels
            .get_mut("#chan")
            .unwrap()
            .apply_mode_list_change('+', 'b', "*!~alice@host.example", "me");

        app.process_command("/unban alice");
        expect_mode(&mut rx, "-b", "*!~alice@host.example");
        assert!(rx.try_recv().is_err());

        // Even before the ban list was fetched, the same host mask is used.
        app.channels.get_mut("#chan").unwrap().bans.clear();
        app.process_command("/unban alice");
        expect_mode(&mut rx, "-b", "*!~alice@host.example");
    }

    #[test]
    fn unban_nick_matches_broader_bans_and_departed_nicks() {
        let (mut app, mut rx) = ban_command_app();
        {
            let channel = app.channels.get_mut("#chan").unwrap();
            channel.apply_mode_list_change('+', 'b', "*!*@*.example", "op");
            channel.apply_mode_list_change('+', 'b', "*!*@other.test", "op");
            channel.apply_mode_list_change('+', 'b', "carol!*@*", "op");
        }
        // Only the ban that actually covers alice is lifted.
        app.process_command("/unban alice");
        expect_mode(&mut rx, "-b", "*!*@*.example");
        assert!(rx.try_recv().is_err());

        // A nick no longer in the channel resolves by the nick part of a ban.
        app.process_command("/unban CAROL");
        expect_mode(&mut rx, "-b", "carol!*@*");

        // Unknown nick with no matching entry: the plain nick mask.
        app.process_command("/unban dave");
        expect_mode(&mut rx, "-b", "dave!*@*");

        // Masks are used as typed.
        app.process_command("/unban *!*@other.test");
        expect_mode(&mut rx, "-b", "*!*@other.test");
        app.process_command("/unban bob@host");
        expect_mode(&mut rx, "-b", "bob!*@host");
    }

    fn set_bans(app: &mut IrcApp, masks: &[&str]) {
        let channel = app.channels.get_mut("#chan").unwrap();
        channel.bans.clear();
        for mask in masks {
            channel.apply_mode_list_change('+', 'b', mask, "op");
        }
    }

    fn last_note(app: &IrcApp) -> String {
        app.channels["#chan"]
            .messages
            .back()
            .map(|row| row.content.clone())
            .unwrap_or_default()
    }

    #[test]
    fn unban_nick_prefers_exact_bans_over_broad_ones_set_by_others() {
        let (mut app, mut rx) = ban_command_app();
        set_bans(
            &mut app,
            &[
                "*!*@*",
                "*!*@*.example",
                "*!*@host.example",
                "*!~alice@host.example",
            ],
        );
        // The mask /ban builds exists: only it goes, and the user is told.
        app.process_command("/unban alice");
        expect_mode(&mut rx, "-b", "*!~alice@host.example");
        assert!(rx.try_recv().is_err());
        assert!(
            last_note(&app).contains("*!~alice@host.example"),
            "{}",
            last_note(&app)
        );

        // A ban naming the nick is exact too; without a MODES limit each
        // removal is its own MODE.
        set_bans(&mut app, &["*!*@*", "alice!*@*", "*!~alice@host.example"]);
        app.process_command("/unban Alice");
        expect_mode(&mut rx, "-b", "alice!*@*");
        expect_mode(&mut rx, "-b", "*!~alice@host.example");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn unban_nick_falls_back_to_the_narrowest_covering_ban_never_a_catch_all() {
        let (mut app, mut rx) = ban_command_app();
        // No exact entry: exactly one covering ban goes, the narrowest.
        set_bans(&mut app, &["*!*@*", "*!*@*.example", "*!*@host.example"]);
        app.process_command("/unban alice");
        expect_mode(&mut rx, "-b", "*!*@host.example");
        assert!(rx.try_recv().is_err());
        assert!(last_note(&app).contains("*!*@host.example"));

        set_bans(&mut app, &["*!*@*", "*!*@*.example"]);
        app.process_command("/unban alice");
        expect_mode(&mut rx, "-b", "*!*@*.example");
        assert!(rx.try_recv().is_err());

        // Only a catch-all covers alice: it is left alone and the user is
        // pointed at the explicit mask form.
        set_bans(&mut app, &["*!*@*", "*!*@*.*"]);
        app.process_command("/unban alice");
        while let Ok(command) = rx.try_recv() {
            match command {
                IrcCommand::Mode(_, Some(mode), params) => {
                    assert_eq!(mode, "-b");
                    assert_eq!(params, vec!["*!~alice@host.example".to_string()]);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(
            last_note(&app).contains("/unban *!*@*"),
            "{}",
            last_note(&app)
        );
    }

    #[test]
    fn unban_batches_removals_up_to_the_modes_limit() {
        let (mut app, mut rx) = ban_command_app();
        set_bans(
            &mut app,
            &[
                "alice!*@*",
                "alice!~a@one",
                "alice!~b@two",
                "*!~alice@host.example",
            ],
        );
        app.network_support
            .isupport
            .insert("MODES".into(), Some("3".into()));
        app.process_command("/unban alice");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Mode(
                "#chan".into(),
                Some("-bbb".into()),
                vec![
                    "alice!*@*".into(),
                    "alice!~a@one".into(),
                    "alice!~b@two".into()
                ]
            )
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Mode(
                "#chan".into(),
                Some("-b".into()),
                vec!["*!~alice@host.example".into()]
            )
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn extended_bans_pass_through_ban_kickban_and_unban() {
        // $a:account, ~a:account and $j:#chan are not nicks: no !*@* suffix,
        // and /kickban has no nick to kick (#259).
        let (mut app, mut rx) = ban_command_app();
        for extban in [
            "$a:troll",
            "~a:troll",
            "$j:#other",
            "$a",
            "$~a",
            "~q:*!*@bad.host",
            "m:*!*@x",
            "~account:troll",
            "$account:troll",
            "account:troll",
        ] {
            app.process_command(&format!("/ban {extban}"));
            expect_mode(&mut rx, "+b", extban);
            app.process_command(&format!("/unban {extban}"));
            expect_mode(&mut rx, "-b", extban);
            app.process_command(&format!("/kickban {extban} go away"));
            expect_mode(&mut rx, "+b", extban);
            assert!(rx.try_recv().is_err(), "{extban}: no KICK for an extban");
        }
        // A wildcard inside the extban must not be completed either.
        app.process_command("/ban $a:tro*");
        expect_mode(&mut rx, "+b", "$a:tro*");

        // A colon in an IPv6 host is a hostmask, not an extban.
        app.process_command("/ban *@2001:db8::1");
        expect_mode(&mut rx, "+b", "*!*@2001:db8::1");
        app.process_command("/kickban bob@2001:db8::1 go");
        expect_mode(&mut rx, "+b", "bob!*@2001:db8::1");
    }

    #[test]
    fn double_slash_sends_a_literal_leading_slash() {
        let (mut app, mut rx) = connected_command_app();
        let mut channel = Channel::new();
        channel.joined = true;
        app.channels.insert("#room".into(), channel);
        app.current_channel = Some("#room".into());

        for (input, text) in [
            ("//me hi", "/me hi"),
            ("//shrug", "/shrug"),
            ("//", "/"),
            ("///etc/hosts", "//etc/hosts"),
        ] {
            app.process_command(input);
            assert_eq!(
                rx.try_recv().unwrap(),
                IrcCommand::Privmsg("#room".into(), text.into()),
                "{input}"
            );
        }
        assert!(rx.try_recv().is_err());
        assert!(
            app.server_messages
                .iter()
                .all(|row| !row.content.contains("Invalid command"))
        );
    }

    #[test]
    fn double_slash_refuses_a_mistyped_credential_command() {
        // "//ns identify hunter2" would otherwise go out as the public message
        // "/ns identify hunter2" (or be echoed in a NickServ query tab). /say
        // stays the deliberate way to send such text.
        let (mut app, mut rx) = connected_command_app();
        let mut channel = Channel::new();
        channel.joined = true;
        app.channels.insert("#room".into(), channel);
        app.current_channel = Some("#room".into());

        for input in [
            "//ns identify hunter2",
            "//NS IDENTIFY hunter2",
            "///ns identify hunter2",
            "//identify hunter2",
            "//msg NickServ identify hunter2",
            "//raw PASS hunter2",
        ] {
            app.process_command(input);
            assert!(rx.try_recv().is_err(), "{input} must not be sent");
        }
        let shown: Vec<&str> = app.channels["#room"]
            .messages
            .iter()
            .map(|row| row.content.as_str())
            .collect();
        assert!(
            shown.iter().any(|row| row.contains("/say")),
            "the refusal names /say: {shown:?}"
        );
        assert!(
            shown.iter().all(|row| !row.contains("hunter2")),
            "the secret is never echoed: {shown:?}"
        );

        // An ordinary escaped command still goes out, as does a deliberate /say.
        app.process_command("//nsfw is fine");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Privmsg("#room".into(), "/nsfw is fine".into())
        );
        app.process_command("/say /ns identify literal");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Privmsg("#room".into(), "/ns identify literal".into())
        );
    }

    #[test]
    fn join_and_part_treat_a_run_of_whitespace_as_one_separator() {
        let (mut app, mut rx) = connected_command_app();

        app.process_command("/join #chan  secret");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Join("#chan".into(), Some("secret".into()), None, None)
        );
        assert_eq!(
            app.pending_channel_keys
                .get(&app.network_support.canonicalize("#chan"))
                .map(String::as_str),
            Some("secret"),
            "the stored key has no leading whitespace"
        );
        app.process_command("/join #other \t key2");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Join("#other".into(), Some("key2".into()), None, None)
        );
        // No key stays no key.
        app.process_command("/join #plain");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Join("#plain".into(), None, None, None)
        );

        app.process_command("/part #chan  bye now");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Part("#chan".into(), Some("bye now".into()))
        );
        app.process_command("/part #chan");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Part("#chan".into(), None)
        );
        // Without a channel token the whole argument is the reason.
        app.channels.insert("#room".into(), Channel::new());
        app.current_channel = Some("#room".into());
        app.process_command("/part  gone   fishing");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Part("#room".into(), Some("gone   fishing".into()))
        );
    }

    #[test]
    fn double_slash_and_say_report_when_there_is_no_target() {
        let (mut app, mut rx) = connected_command_app();
        assert!(app.current_channel.is_none());
        for input in ["//shrug", "/say hello"] {
            let before = app.server_messages.len();
            app.process_command(input);
            assert!(rx.try_recv().is_err(), "{input}");
            assert_eq!(
                app.server_messages.len(),
                before + 1,
                "{input} must report an error instead of doing nothing"
            );
        }
    }

    #[test]
    fn runs_of_whitespace_between_arguments_are_one_separator() {
        let (mut app, mut rx) = connected_command_app();
        app.current_channel = Some("#room".into());
        app.channels.insert("#room".into(), Channel::new());
        app.handle_cap_message("ACK", &["message-tags labeled-response".into()]);

        app.process_command("/ctcp bob  VERSION");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Privmsg("bob".into(), "\x01VERSION\x01".into())
        );
        app.process_command("/ctcp bob \t ping   1  2");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Privmsg("bob".into(), "\x01PING 1  2\x01".into())
        );
        app.process_command("/cprivmsg bob  #room  hello there");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("CPRIVMSG bob #room :hello there".into())
        );
        app.process_command("/invite bob   #elsewhere");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Invite("bob".into(), "#elsewhere".into())
        );
        app.process_command("/react abc   👍");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("@+draft/react=👍;+reply=abc TAGMSG #room".into())
        );
        app.process_command("/reply abc   hello there");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("@+reply=abc PRIVMSG #room :hello there".into())
        );
        app.process_command("/label lbl1   WHOIS alice");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("@label=lbl1 WHOIS alice".into())
        );
        app.process_command("/msg alice   hi there");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Privmsg("alice".into(), "hi there".into())
        );
        app.process_command("/kick bob   too loud");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Kick("#room".into(), "bob".into(), Some("too loud".into()))
        );
        app.process_command("/rename #new   moved");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Rename("#room".into(), "#new".into(), Some("moved".into()))
        );
    }

    #[test]
    fn version_and_ping_target_only_the_first_word() {
        let (mut app, mut rx) = connected_command_app();

        app.process_command("/version alice hello");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Privmsg("alice".into(), "\x01VERSION\x01".into())
        );
        app.process_command("/version  alice\tignored words");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Privmsg("alice".into(), "\x01VERSION\x01".into())
        );
        app.process_command("/ping alice x");
        match rx.try_recv().unwrap() {
            IrcCommand::Privmsg(target, ctcp) => {
                assert_eq!(target, "alice");
                assert!(ctcp.starts_with("\x01PING ") && ctcp.ends_with('\x01'));
                assert!(!ctcp.contains('x'), "{ctcp:?}");
            }
            other => panic!("expected Privmsg, got {other:?}"),
        }
        let confirmations: Vec<&str> = app
            .server_messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert!(
            confirmations
                .iter()
                .all(|row| !row.contains("hello") && !row.contains("ignored")),
            "{confirmations:?}"
        );
    }

    #[test]
    fn redact_keeps_the_whole_multi_word_reason() {
        let (mut app, mut rx) = connected_command_app();
        app.current_channel = Some("#room".into());
        app.channels.insert("#room".into(), Channel::new());

        for (input, target, reason) in [
            (
                "/redact abc123 spam and junk",
                "#room",
                Some("spam and junk"),
            ),
            (
                "/redact #other abc123 spam and junk",
                "#other",
                Some("spam and junk"),
            ),
            ("/redact abc123", "#room", None),
            ("/redact #other abc123", "#other", None),
            (
                "/redact #other  abc123   spam   and junk",
                "#other",
                Some("spam   and junk"),
            ),
            (
                "/redact abc123\tspam\tand junk",
                "#room",
                Some("spam\tand junk"),
            ),
        ] {
            app.process_command(input);
            assert_eq!(
                rx.try_recv().unwrap(),
                IrcCommand::Redact(target.into(), "abc123".into(), reason.map(str::to_string)),
                "{input}"
            );
        }
    }

    #[test]
    fn unknown_valid_commands_pass_through_without_allowing_line_injection() {
        let (mut app, mut rx) = connected_command_app();
        app.process_command("/map");
        assert_eq!(rx.try_recv().unwrap(), IrcCommand::Raw("MAP".into()));
        app.process_command("/vendorcmd one two");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("VENDORCMD one two".into())
        );
        app.process_command("/vendorcmd one\r\nOPER root secret");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn monitor_uses_watch_on_legacy_networks() {
        let (mut app, mut rx) = connected_command_app();
        app.network_support
            .isupport
            .insert("WATCH".into(), Some("128".into()));
        app.process_command("/monitor +alice,bob");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("WATCH +alice +bob".into())
        );
    }

    #[test]
    fn monitor_and_silence_report_unsupported_instead_of_sending() {
        let (mut app, mut rx) = connected_command_app();
        // The server's ISUPPORT is known and advertises neither feature.
        app.network_support
            .isupport
            .insert("CHANTYPES".into(), Some("#".into()));

        app.process_command("/monitor +alice");
        app.process_command("/monitor L");
        app.process_command("/silence +*!*@spam.example");
        app.process_command("/silence");
        assert!(rx.try_recv().is_err(), "nothing may reach the wire");
        assert!(
            app.monitored_users.is_empty(),
            "an unsent add must not be tracked"
        );
        let rows: Vec<&str> = app
            .server_messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(rows.iter().filter(|row| row.contains("MONITOR")).count(), 2);
        assert_eq!(rows.iter().filter(|row| row.contains("SILENCE")).count(), 2);

        // Advertised support sends as before.
        app.network_support
            .isupport
            .insert("MONITOR".into(), Some("100".into()));
        app.network_support
            .isupport
            .insert("SILENCE".into(), Some("15".into()));
        app.process_command("/monitor +alice");
        assert!(matches!(
            rx.try_recv(),
            Ok(IrcCommand::Monitor(sub, Some(targets))) if sub == "+" && targets == "alice"
        ));
        app.process_command("/silence +*!*@spam.example");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("SILENCE +*!*@spam.example".into())
        );
        app.process_command("/silence");
        assert_eq!(rx.try_recv().unwrap(), IrcCommand::Raw("SILENCE".into()));
    }

    #[test]
    fn monitor_and_silence_still_send_before_isupport_arrives() {
        // An early /perform line can run before the 005 burst: with nothing
        // known yet the commands keep going to the server.
        let (mut app, mut rx) = connected_command_app();
        app.process_command("/monitor +alice");
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Monitor(..))));
        app.process_command("/silence -*!*@spam.example");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("SILENCE -*!*@spam.example".into())
        );
    }

    #[test]
    fn raw_command_reports_usage_and_disconnect_feedback() {
        let mut app = IrcApp::with_settings(crate::gui::Settings::default());
        app.process_command("/raw");
        assert!(
            app.server_messages
                .iter()
                .any(|message| message.content.contains("Usage: /raw"))
        );
        app.process_command("/raw JOIN #x");
        assert!(app.server_messages.iter().any(|message| {
            message
                .content
                .contains("Not connected - server command not sent")
        }));
    }

    #[test]
    fn history_subcommand_does_not_switch_tabs_when_the_request_cannot_be_sent() {
        let mut app = IrcApp::with_settings(crate::gui::Settings::default());
        app.handle_cap_message("ACK", &["draft/chathistory".into()]);
        app.process_command("/history before #chan *");
        assert!(
            !app.channels.contains_key("#chan"),
            "the tab must not be created for an unsent request"
        );
        assert!(
            app.server_messages
                .iter()
                .any(|message| message.content.contains("history request not sent"))
        );
    }

    #[test]
    fn stale_tab_send_failure_reports_the_real_reason() {
        let (mut app, mut rx) = connected_command_app();
        let mut stale = Channel::new();
        stale.joined = false;
        app.channels.insert("#stale".into(), stale);
        app.current_channel = Some("#stale".into());

        app.process_command("/msg #stale hello");
        assert!(rx.try_recv().is_err());
        assert!(
            app.channels["#stale"]
                .messages
                .iter()
                .any(|message| message.content == "Message not sent")
        );
        assert!(
            app.channels["#stale"]
                .messages
                .iter()
                .all(|message| !message.content.contains("Not connected"))
        );

        // While actually disconnected, the disconnection is still what's
        // reported.
        app.connected = false;
        app.cmd_tx = None;
        app.process_command("/msg #stale hello again");
        assert!(
            app.channels["#stale"]
                .messages
                .iter()
                .any(|message| message.content.contains("Not connected - message not sent"))
        );
    }

    #[test]
    fn disconnected_commands_never_claim_they_were_sent() {
        let mut app = IrcApp::with_settings(crate::gui::Settings::default());
        app.process_command("/ctcp bob VERSION");
        app.process_command("/invite bob #room");
        app.process_command("/knock #room");
        app.channel_list_loading = true;
        app.process_command("/list");

        let feedback: Vec<&str> = app
            .server_messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(feedback.len(), 4);
        assert!(
            feedback
                .iter()
                .all(|line| line.contains("not sent") || line.contains("not refreshed"))
        );
        assert!(feedback.iter().all(|line| !line.contains("Sent")));
        assert!(!app.channel_list_loading);
    }

    #[test]
    fn credential_commands_are_redacted_from_local_echoes_and_perform_output() {
        for message in [
            "IDENTIFY hunter2",
            "REGISTER hunter2 user@example.test",
            "SIDENTIFY hunter2",
            "GHOST oldnick hunter2",
            "REGAIN nick hunter2",
            "RECOVER nick hunter2",
            "RELEASE nick hunter2",
            "SET PASSWORD hunter2",
        ] {
            assert!(service_message_contains_credentials("NickServ", message));
            let echo = outgoing_local_echo("NickServ", "me", message, "short");
            assert!(echo.no_log);
            assert_eq!(echo.content, "<credential command sent>");
            assert!(!echo.content.contains("hunter2"));
        }

        for line in [
            "/ns identify hunter2",
            "/msg NickServ identify hunter2",
            "/query NickServ recover nick hunter2",
            "/notice NickServ identify hunter2",
            "/msg NickServ@services.example identify hunter2",
            "/msg NickServ,alice identify hunter2",
            // Service shorthands and ChanServ must be recognized too: a
            // channel key sent to ChanServ is as sensitive as a NickServ
            // password.
            "/msg NS identify hunter2",
            "/query NS id hunter2",
            "/notice NS identify hunter2",
            "/msg CS identify #chan key123",
            "/msg ChanServ identify #chan key123",
            "/cs identify #chan key123",
            "/as login account hunter2",
            "/perform /ns identify hunter2",
            "/raw PASS hunter2",
            "/quote AUTHENTICATE hunter2",
        ] {
            assert!(command_line_contains_credentials(line), "{line}");
        }

        let ordinary = outgoing_local_echo("NickServ", "me", "INFO alice", "short");
        assert!(!ordinary.no_log);
        assert_eq!(ordinary.content, "INFO alice");
        assert!(!command_line_contains_credentials("/join #rust"));

        // A user who happens to be named like a service is only treated as
        // sensitive when the payload looks like an auth command.
        assert!(!service_message_contains_credentials("NS", "hello there"));
        assert!(service_message_contains_credentials(
            "CS",
            "IDENTIFY #chan key123"
        ));
    }

    #[test]
    fn password_bearing_service_commands_are_redacted_everywhere() {
        // (target, text, credential?) — Atheme DROP, Undernet X / QuakeNet Q
        // NEWPASS, Anope SASET, and ChanServ-style `SET #chan PASSWORD` (#225).
        let cases: &[(&str, &str, bool)] = &[
            ("NickServ", "DROP mynick hunter2", true),
            ("NickServ", "drop mynick hunter2", true),
            ("ChanServ", "DROP #chan", true),
            ("NickServ", "DROP", false),
            ("Q@CServe.quakenet.org", "NEWPASS old new new", true),
            ("X", "newpass hunter2 hunter3 hunter3", true),
            ("NickServ", "SASET alice PASSWORD hunter2", true),
            ("NickServ", "SET PASSWORD hunter2", true),
            ("ChanServ", "SET #chan PASSWORD hunter2", true),
            ("ChanServ", "set #chan pass hunter2", true),
            ("ChanServ", "SET #chan TOPICLOCK ON", false),
            ("NickServ", "SET EMAIL me@example.test", false),
            ("NickServ", "SASET alice EMAIL me@example.test", false),
            ("NickServ", "INFO alice", false),
            // Not a service target: ordinary chat is never touched.
            ("alice", "DROP mynick hunter2", false),
        ];
        for &(target, text, expected) in cases {
            // Echo redaction.
            let echo = outgoing_local_echo(target, "me", text, "short");
            assert_eq!(
                echo.content == "<credential command sent>",
                expected,
                "{target} {text}"
            );
            assert_eq!(echo.no_log, expected, "{target} {text}");
            // Log suppression / replay filtering use the same predicate.
            assert_eq!(
                service_message_contains_credentials(target, text),
                expected,
                "{target} {text}"
            );
            // Input-history predicate, through the service shorthands and /msg.
            let command_line = format!("/msg {target} {text}");
            assert_eq!(
                command_line_contains_credentials(&command_line),
                expected,
                "{command_line}"
            );
        }

        // Targetless senders (/me, /say) and the service shorthands.
        assert!(content_has_auth_command("DROP mynick hunter2"));
        assert!(!content_has_auth_command("drop"));
        assert!(content_has_auth_command("NEWPASS old new new"));
        assert!(content_has_auth_command("SET #chan PASSWORD hunter2"));
        assert!(command_line_contains_credentials("/ns drop mynick hunter2"));
        assert!(command_line_contains_credentials("/ns newpass a b b"));
        assert!(command_line_contains_credentials(
            "/cs set #chan password hunter2"
        ));
        assert!(command_line_contains_credentials(
            "/ns saset alice password x"
        ));
        assert!(!command_line_contains_credentials("/ns set email a@b.test"));
    }

    #[test]
    fn drop_is_a_credential_command_only_with_a_channel_or_two_arguments() {
        // Atheme `DROP <nick> <pw>`, Anope `DROP #chan`; one plain word is
        // ordinary text ("drop it").
        for (text, expected) in [
            ("DROP", false),
            ("drop it", false),
            ("drop alice", false),
            ("drop #chan", true),
            ("DROP &chan", true),
            ("DROP mynick hunter2", true),
            ("drop me a line", true),
        ] {
            assert_eq!(content_has_auth_command(text), expected, "{text}");
        }
        assert!(!service_message_contains_credentials("NickServ", "drop it"));
        assert!(!command_line_contains_credentials("/say drop it"));
        assert!(!command_line_contains_credentials("/ns drop alice"));
        assert!(command_line_contains_credentials("/cs drop #chan"));
    }

    #[test]
    fn service_alias_squery_and_tagged_raw_credential_lines_never_enter_history() {
        // Bare service commands reach the wire through the catch-all arm, so
        // the history filter has to know them as top-level commands (#226).
        for line in [
            "/identify hunter2",
            "/IDENTIFY hunter2",
            "/id hunter2",
            "/sidentify hunter2",
            "/ghost oldnick hunter2",
            "/recover oldnick hunter2",
            "/release oldnick hunter2",
            "/regain oldnick hunter2",
            "/group hunter2",
            "/setpass hunter2",
            "/squery NickServ :identify hunter2",
            "/squery NickServ identify hunter2",
            "/squery ChanServ :identify #chan key123",
            "/quote @label=1 PRIVMSG NickServ :identify hunter2",
            "/raw @label=1;+draft/x=y NOTICE NickServ :identify hunter2",
            "/raw   @label=1   PRIVMSG   NS   :id hunter2",
            "/quote @label=1 PASS hunter2",
            "/perform /quote @label=1 PRIVMSG NickServ :identify hunter2",
        ] {
            assert!(command_line_contains_credentials(line), "{line}");
        }
        for line in [
            "/squery HostServ :help",
            "/squery NickServ :info alice",
            "/quote @label=1 PRIVMSG #chan :hello",
            "/quote @label=1 WHOIS alice",
            "/quote @label=1",
            "/raw @",
            "/version alice",
        ] {
            assert!(!command_line_contains_credentials(line), "{line}");
        }

        let (mut app, mut rx) = connected_command_app();
        app.set_session_nick("me".into(), true);
        for input in [
            "/identify hunter12",
            "/ghost oldnick hunter12",
            "/squery NickServ :identify hunter12",
            "/quote @label=1 PRIVMSG NickServ :identify hunter12",
        ] {
            app.input_text = input.into();
            app.process_input();
        }
        assert!(app.command_history.is_empty(), "{:?}", app.command_history);
        while rx.try_recv().is_ok() {}
    }

    #[test]
    fn non_ascii_command_words_never_dispatch_as_service_commands() {
        // Full Unicode upper-casing turns the dotless i and the long s into
        // plain I and S, so "/ıdentify" used to dispatch as the raw IDENTIFY
        // while the ASCII-only credential filter let it into history.
        let (mut app, mut rx) = connected_command_app();
        app.set_session_nick("me".into(), true);
        for input in [
            "/ıdentify hunter13",
            "/ıd hunter13",
            "/ghoſt oldnick hunter13",
            "/nſ identify hunter13",
            "/ſquery NickServ :identify hunter13",
        ] {
            app.input_text = input.into();
            app.process_input();
            assert!(rx.try_recv().is_err(), "{input} must not reach the wire");
        }
        assert!(
            app.server_messages
                .iter()
                .any(|row| row.content.contains("Invalid command")),
            "the user is told the command is unknown"
        );
        // The typed secret still stays out of Up-arrow history.
        assert!(app.command_history.is_empty(), "{:?}", app.command_history);
        // ASCII spellings dispatch as before, in any case.
        app.process_command("/IdEnTiFy hunter13");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("IDENTIFY hunter13".into())
        );
    }

    #[test]
    fn action_and_describe_credential_lines_never_enter_history() {
        let (mut app, mut rx) = connected_command_app();
        app.set_session_nick("me".into(), true);
        assert!(app.open_query("NickServ"));

        for input in [
            "/me IDENTIFY hunter8",
            "/describe NickServ IDENTIFY hunter9",
        ] {
            app.input_text = input.into();
            app.process_input();
        }
        assert!(app.command_history.is_empty(), "{:?}", app.command_history);
        // The scrollback redaction (defense in depth) still applies.
        let query = &app.channels["NickServ"];
        for secret in ["hunter8", "hunter9"] {
            assert!(
                query
                    .messages
                    .iter()
                    .all(|message| !message.content.contains(secret))
            );
        }
        while rx.try_recv().is_ok() {}
    }

    #[test]
    fn connected_send_failure_is_reported_once_with_the_accurate_reason() {
        let (mut app, _rx) = connected_command_app();
        // Connected, but the line is too long: send_command prints its own
        // accurate row; the command arm must not add a "Not connected" row.
        let long_arg = "x".repeat(600);
        app.process_command(&format!("/raw PING {long_arg}"));
        let rows: Vec<&str> = app
            .server_messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].contains("too long"), "{rows:?}");
        assert!(!rows[0].contains("Not connected"), "{rows:?}");
    }

    #[test]
    fn tab_separated_target_is_split_the_same_way_by_dispatch_and_filter() {
        let (mut app, mut rx) = connected_command_app();
        app.set_session_nick("me".into(), true);
        // A paste can carry a tab: dispatch must see target "NickServ" exactly
        // like the credential filter does (#148).
        app.input_text = "/msg NickServ\tIDENTIFY hunter10".into();
        app.process_input();

        assert!(
            !app.channels.keys().any(|key| key.contains('\t')),
            "no junk tab: {:?}",
            app.channels.keys().collect::<Vec<_>>()
        );
        let query = &app.channels["NickServ"];
        assert!(
            query
                .messages
                .iter()
                .all(|message| !message.content.contains("hunter10"))
        );
        assert!(app.command_history.is_empty());
        assert!(
            matches!(rx.try_recv(), Ok(IrcCommand::Privmsg(target, _)) if target == "NickServ")
        );
    }

    #[test]
    fn perform_listing_redacts_targetless_credential_lines() {
        // Hermetic by construction: IrcApp::default() reads the real user's
        // settings.json (and would rename a torn file), which is what #152
        // was about. Set auto_perform directly — the PERFORM add branch also
        // calls get_settings().save() and so cannot run against the real
        // config from a test.
        let mut app = IrcApp::with_settings(crate::gui::Settings::default());
        app.auto_perform = "/me IDENTIFY hunter11".into();
        app.process_command("/perform");
        let shown: Vec<&str> = app
            .server_messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert!(
            shown
                .iter()
                .any(|line| line.contains("<credential command redacted>")),
            "{shown:?}"
        );
        assert!(
            shown.iter().all(|line| !line.contains("hunter11")),
            "{shown:?}"
        );
    }

    #[test]
    fn single_nick_whois_queries_the_users_own_server() {
        let (mut app, mut rx) = connected_command_app();
        app.process_command("/whois alice");
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Whois(t)) if t == "alice alice"));
        // Separator-carrying forms pass through: masks, comma lists, and an
        // explicit manual double target.
        app.process_command("/whois alice,bob");
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Whois(t)) if t == "alice,bob"));
        app.process_command("/whois alice bob");
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Whois(t)) if t == "alice bob"));
        app.process_command("/whois *lur*");
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Whois(t)) if t == "*lur*"));
        // A hostmask must not be doubled: solanum-family ircds parse the
        // first parameter as a routing target and fail (#167).
        app.process_command("/whois bob!user@host");
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Whois(t)) if t == "bob!user@host"));
    }

    #[test]
    fn slap_supports_a_custom_action() {
        let (mut app, mut rx) = connected_command_app();
        app.set_session_nick("me".into(), true);
        app.current_channel = Some("#chan".into());
        let mut channel = Channel::new();
        channel.joined = true;
        app.channels.insert("#chan".into(), channel);

        app.process_command("/slap alice with a frozen herring");
        assert!(matches!(
            rx.try_recv(),
            Ok(IrcCommand::Privmsg(target, content))
                if target == "#chan"
                    && content == "\x01ACTION slaps alice with a frozen herring\x01"
        ));

        // Bare target keeps the classic trout.
        app.process_command("/slap bob");
        assert!(matches!(
            rx.try_recv(),
            Ok(IrcCommand::Privmsg(_, content))
                if content == "\x01ACTION slaps bob around a bit with a large trout\x01"
        ));

        // Targetless keeps the classic everyone/trout.
        app.process_command("/slap");
        assert!(matches!(
            rx.try_recv(),
            Ok(IrcCommand::Privmsg(_, content))
                if content == "\x01ACTION slaps everyone around a bit with a large trout\x01"
        ));
    }

    #[test]
    fn unprofiled_endpoint_changes_clear_server_scoped_secrets() {
        let mut app = IrcApp {
            password: "server-pass".into(),
            sasl_username: "account".into(),
            sasl_password: "sasl-pass".into(),
            auto_join_channels: "#secret key".into(),
            auto_perform: "/ns identify nickserv-pass".into(),
            pending_auto_perform: Some(vec!["/ns identify nickserv-pass".into()]),
            accept_invalid_certs: true,
            ..IrcApp::with_settings(crate::gui::Settings::default())
        };

        app.clear_endpoint_credentials();

        assert!(app.password.is_empty());
        assert!(app.sasl_username.is_empty());
        assert!(app.sasl_password.is_empty());
        assert!(app.auto_join_channels.is_empty());
        assert!(app.auto_perform.is_empty());
        assert!(app.pending_auto_perform.is_none());
        assert!(!app.accept_invalid_certs);
    }

    fn saved_libera_profile_app() -> IrcApp {
        IrcApp {
            server_host: "irc.libera.chat".into(),
            server_port: "6697".into(),
            use_tls: true,
            password: "server-pass".into(),
            sasl_username: "account".into(),
            sasl_password: "sasl-pass".into(),
            auto_join_channels: "#private key".into(),
            auto_perform: "/ns identify nickserv-pass".into(),
            accept_invalid_certs: true,
            ..IrcApp::with_settings(crate::gui::Settings {
                logging_enabled: false,
                ..crate::gui::Settings::default()
            })
        }
    }

    #[test]
    fn server_command_never_rewrites_the_saved_profile() {
        let mut app = saved_libera_profile_app();

        app.process_command("/server irc.oftc.net 6667");

        // The new session starts from nothing: no secret follows the switch.
        let session = app.active_server_config().unwrap();
        assert_eq!(session.host, "irc.oftc.net");
        assert!(session.password.is_none());
        assert!(session.sasl_username.is_none() && session.sasl_password.is_none());
        assert!(!session.accept_invalid_certs);
        // The persisted profile is untouched, so a later save (e.g. /ignore,
        // /perform, an update restart) cannot erase it or pair its secrets with
        // the switched-to host.
        let saved = app.get_settings();
        assert_eq!(saved.server_host, "irc.libera.chat");
        assert_eq!(saved.server_port, "6697");
        assert!(saved.use_tls);
        assert_eq!(saved.password, "server-pass");
        assert_eq!(saved.sasl_username, "account");
        assert_eq!(saved.sasl_password, "sasl-pass");
        assert_eq!(saved.auto_join_channels, "#private key");
        assert_eq!(saved.auto_perform, "/ns identify nickserv-pass");
        assert!(saved.accept_invalid_certs);
    }

    #[test]
    fn server_command_picks_tls_from_the_port_it_was_given() {
        let tls_of = |command: &str, start_tls: bool| {
            let mut app = saved_libera_profile_app();
            app.use_tls = start_tls;
            app.process_command(command);
            let session = app.active_server_config().unwrap();
            (session.port, session.use_tls)
        };
        // 6697 is TLS even after a plaintext session; other ports are plain.
        assert_eq!(tls_of("/server irc.x.net 6697", false), (6697, true));
        assert_eq!(tls_of("/server irc.x.net:6697", false), (6697, true));
        assert_eq!(tls_of("/server irc.x.net 6667", true), (6667, false));
        assert_eq!(tls_of("/server irc.x.net:7000", true), (7000, false));
        // mIRC's +port marker forces TLS on any port.
        assert_eq!(tls_of("/server irc.x.net +7000", false), (7000, true));
        assert_eq!(tls_of("/server irc.x.net:+7000", false), (7000, true));
        assert_eq!(tls_of("/server [2001:db8::1]:+7000", false), (7000, true));
        // Without a port the current port and TLS setting stay paired.
        assert_eq!(tls_of("/server irc.x.net", true), (6697, true));
        assert_eq!(tls_of("/server irc.x.net", false), (6697, false));
    }

    #[test]
    fn server_command_does_not_echo_or_remember_a_trailing_password() {
        let mut app = saved_libera_profile_app();
        app.input_text = "/server irc.x.net 6667 hunter2".into();
        app.process_input();

        assert!(!app.connecting);
        assert_eq!(app.server_host, "irc.libera.chat");
        assert!(
            app.server_messages
                .iter()
                .all(|message| !message.content.contains("hunter2")),
            "the password was echoed into the buffer"
        );
        assert!(
            app.server_messages
                .iter()
                .any(|message| message.content.contains("1 to 65535"))
        );
        assert!(
            app.command_history
                .iter()
                .all(|line| !line.contains("hunter2"))
        );

        // Ordinary endpoint forms still reach history.
        for line in [
            "/server irc.x.net",
            "/server irc.x.net 6697",
            "/server irc.x.net:6697",
            "/server irc.x.net +6697",
            "/server 2001:db8::1",
            "/server [2001:db8::1]:6697",
        ] {
            assert!(!command_line_contains_credentials(line), "{line}");
        }
        for line in [
            "/server irc.x.net 6667 hunter2",
            "/server irc.x.net hunter2",
            "/server irc.x.net:6667 hunter2",
            "/SERVER irc.x.net +6697 hunter2",
        ] {
            assert!(command_line_contains_credentials(line), "{line}");
        }
    }

    #[test]
    fn server_command_validates_before_mutation_and_never_inherits_a_profile() {
        let mut app = IrcApp {
            password: "server-pass".into(),
            sasl_username: "account".into(),
            sasl_password: "sasl-pass".into(),
            auto_join_channels: "#private key".into(),
            auto_perform: "/ns identify nickserv-pass".into(),
            accept_invalid_certs: true,
            ..IrcApp::with_settings(crate::gui::Settings::default())
        };
        let original_host = app.server_host.clone();
        app.process_command("/server new.example 0");
        assert_eq!(app.server_host, original_host);
        assert_eq!(app.password, "server-pass");
        assert!(!app.connecting);
        assert!(
            app.server_messages
                .back()
                .is_some_and(|message| message.content.contains("1 to 65535"))
        );

        // Even naming the current endpoint is an unprofiled /server action.
        let command = format!("/server {} {}", app.server_host, app.server_port);
        app.process_command(&command);
        assert!(app.connecting);
        let active = app.active_server_config().unwrap();
        assert!(active.password.is_none());
        assert!(active.sasl_username.is_none());
        assert!(active.sasl_password.is_none());
        assert!(!active.accept_invalid_certs);
        let session = app.active_session.as_ref().unwrap();
        assert!(session.auto_join_channels.is_empty());
        assert!(session.auto_perform.is_empty());
        // Only the session starts clean: the form keeps describing the saved
        // profile (see server_command_never_rewrites_the_saved_profile).
        assert_eq!(app.password, "server-pass");
        assert_eq!(app.auto_perform, "/ns identify nickserv-pass");
    }

    #[test]
    fn empty_nickname_is_rejected_by_the_connection_form() {
        let mut app = IrcApp::with_settings(crate::gui::Settings::default());
        app.nickname = "  ".into();
        app.server_host = "irc.example".into();
        app.server_port = "6697".into();
        assert!(app.session_from_form().is_err());
    }

    #[test]
    fn server_arg_handles_ipv6() {
        fn arg(host: &str, port: Option<&str>, tls: Option<bool>) -> Result<ServerArg, String> {
            Ok(ServerArg {
                host: host.into(),
                port: port.map(Into::into),
                tls,
            })
        }
        assert_eq!(
            parse_server_arg("irc.libera.chat:6697"),
            arg("irc.libera.chat", Some("6697"), Some(true))
        );
        assert_eq!(
            parse_server_arg("irc.libera.chat 6697"),
            arg("irc.libera.chat", Some("6697"), Some(true))
        );
        assert_eq!(
            parse_server_arg("2001:db8::1 6697"),
            arg("2001:db8::1", Some("6697"), Some(true))
        );
        assert_eq!(
            parse_server_arg("2001:db8::1"),
            arg("2001:db8::1", None, None)
        );
        assert_eq!(
            parse_server_arg("[::1]:7000"),
            arg("::1", Some("7000"), Some(false))
        );
        assert_eq!(
            parse_server_arg("[2001:db8::1]"),
            arg("2001:db8::1", None, None)
        );
        assert_eq!(
            parse_server_arg("irc.example.org"),
            arg("irc.example.org", None, None)
        );
        assert_eq!(
            parse_server_arg("irc.example.org 6667"),
            arg("irc.example.org", Some("6667"), Some(false))
        );
        assert_eq!(
            parse_server_arg("irc.example.org +7000"),
            arg("irc.example.org", Some("7000"), Some(true))
        );
        assert_eq!(
            parse_server_arg("[::1]:+7000"),
            arg("::1", Some("7000"), Some(true))
        );
        assert!(parse_server_arg("host:junk").is_err());
        assert!(parse_server_arg("host:").is_err());
        assert!(parse_server_arg(":6697").is_err());
        assert!(parse_server_arg("host 0").is_err());
        assert!(parse_server_arg("host 70000").is_err());
        assert!(parse_server_arg("host abc").is_err());
        assert!(parse_server_arg("host +").is_err());
        assert!(parse_server_arg("host ++6697").is_err());
        assert!(parse_server_arg("host -6697").is_err());
        // A rejected argument is never repeated back: it may be a password.
        for rejected in [
            "host 6667 hunter2",
            "host hunter2",
            "host:hunter2",
            "[::1]hunter2",
        ] {
            let error = parse_server_arg(rejected).unwrap_err();
            assert!(!error.contains("hunter2"), "{rejected}: {error}");
        }
        assert_eq!(parse_server_port("1"), Ok(1));
        assert_eq!(parse_server_port("65535"), Ok(65535));
    }
}
