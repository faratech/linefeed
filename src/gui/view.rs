//! Drawing one session's pieces for the window manager in `app.rs`: the
//! toolbar status summary and join box, and a window's contents (topic,
//! user list, scrollback and input line). A window draws with its session's
//! `current_conversation` pointed at the window's target and the window's input
//! state swapped in (see `app::with_window_view`).

use egui::{Color32, RichText, ScrollArea, TextEdit};

use super::formatting::render_irc_text;
use super::{Session, UserMode, commands};
use crate::irc::IrcCommand;

impl Session {
    /// Connection status for the toolbar: state dot, nick, network, lag and
    /// away message.
    pub(crate) fn draw_status_summary(&self, ui: &mut egui::Ui, network_label: &str) {
        if self.is_connected() {
            if self.away_status.is_some() {
                ui.label(RichText::new("●").color(Color32::from_rgb(255, 200, 0)));
            } else {
                ui.label(RichText::new("●").color(Color32::GREEN));
            }
            ui.label(&self.my_nick);
            ui.label("@");
            ui.label(network_label);
            self.draw_account_security_status(ui);

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

            if let Some(away_msg) = &self.away_status {
                ui.separator();
                ui.label(
                    RichText::new(format!("Away: {}", away_msg))
                        .color(Color32::from_rgb(255, 200, 0)),
                );
            }
        } else if self.is_connecting() {
            ui.label(RichText::new("●").color(Color32::YELLOW));
            ui.label(format!("Connecting to {network_label}..."));
        } else {
            ui.label(RichText::new("●").color(Color32::RED));
            ui.label("Disconnected");
        }
    }

    pub(crate) fn draw_account_security_status(&self, ui: &mut egui::Ui) {
        if !self.is_connected() {
            return;
        }
        use crate::irc::client::{AuthenticationStatus, TlsStatus};
        let auth = match &self.connection_status.authentication {
            AuthenticationStatus::LoggedIn { account } => format!("Logged in as {account}"),
            AuthenticationStatus::Authenticated => "Authenticated (account not reported)".into(),
            AuthenticationStatus::Authenticating => "Authenticating…".into(),
            _ => "Connected · not authenticated".into(),
        };
        ui.label(RichText::new(auth).small()).on_hover_text(self.connection_status.sasl_mechanism.map_or_else(||"Account status is confirmed by server account events or a correlated self-WHOIS.".into(),|mechanism|format!("SASL used on this connection: {mechanism:?}\nAccount status is confirmed by the server.")));
        let (tls, color) = match self.connection_status.tls {
            TlsStatus::Verified => ("TLS verified", Color32::GREEN),
            TlsStatus::Unverified => ("TLS unverified", Color32::YELLOW),
            TlsStatus::Negotiating => ("TLS negotiating", Color32::YELLOW),
            TlsStatus::Plaintext => ("Plaintext", Color32::YELLOW),
        };
        let endpoint = self
            .connection_status
            .effective_host
            .as_ref()
            .zip(self.connection_status.effective_port)
            .map(|(host, port)| {
                super::identity::TransportEndpointKey::new(
                    host,
                    port,
                    self.connection_status.effective_use_tls.unwrap_or(false),
                )
                .label()
            })
            .unwrap_or_else(|| "Awaiting transport confirmation".into());
        ui.label(RichText::new(tls).small().color(color)).on_hover_text(format!("Effective endpoint: {endpoint}\nUnverified TLS means certificate validation was explicitly bypassed."));
    }

    /// The "#channel [+]" join box.
    pub(crate) fn draw_join_box(&mut self, ui: &mut egui::Ui) {
        // Laid out right to left in the toolbar: button first.
        let clicked = ui.button("Join").clicked();
        let response = ui.add(
            TextEdit::singleline(&mut self.join_channel)
                .hint_text("#channel")
                .desired_width(110.0),
        );
        let entered = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (clicked || entered) && !self.join_channel.trim().is_empty() {
            let channel = self.normalize_channel_name(self.join_channel.trim());
            self.send_join(channel, None);
            self.join_channel.clear();
        }
    }

    /// Open the Channel Info dialog for `channel`, asking for fresh modes.
    pub(crate) fn open_channel_info(&mut self, channel: &str) {
        self.channel_info_target = self.id_for_target(channel);
        self.show_channel_info = true;
        self.send_command(IrcCommand::Mode(channel.to_string(), None, Vec::new()));
    }

    /// One window's contents for the current target: header, user list
    /// (channels only), scrollback and input line. Everything is laid out
    /// to the space the window gives it and nothing has a minimum width
    /// beyond it, so a floating window keeps the size it was tiled to or
    /// dragged to instead of growing to fit its content.
    pub(crate) fn draw_target(&mut self, ui: &mut egui::Ui) {
        let salt = (self.id, self.current_conversation);
        self.capture_upload_drop(ui);
        self.draw_target_header(ui);
        if self.timeline.borrow().restore_pending {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Restoring saved reading position…").small());
                if ui.small_button("Jump to latest").clicked() {
                    let mut timeline = self.timeline.borrow_mut();
                    timeline.restore_pending = false;
                    timeline.restored_hold = false;
                    timeline.hydrating = false;
                    timeline.anchor = Some(Default::default());
                    timeline.pending_offset = Some(f32::MAX);
                }
            });
        }
        if let Some(channel_name) = self.current_target_name().map(str::to_string)
            && self.is_channel_name(&channel_name)
        {
            egui::Panel::right(("users", salt))
                .resizable(true)
                .default_size(140.0)
                .min_size(80.0)
                .max_size((ui.available_width() * 0.35).max(80.0))
                .show(ui, |ui| self.draw_user_list(ui, &channel_name));
        }
        egui::Panel::bottom(("input", salt))
            .show_separator_line(true)
            .show(ui, |ui| self.draw_input_row(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            let available_height = ui.available_height();
            self.draw_scrollback(ui, available_height);
        });
    }

    fn draw_target_header(&self, ui: &mut egui::Ui) {
        let Some(channel_name) = self.current_target_name() else {
            return;
        };
        let Some(channel) = self.conversations.get(channel_name) else {
            return;
        };
        if self.is_channel_name(channel_name) && !channel.modes.is_empty() {
            ui.horizontal_wrapped(|ui| {
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
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Topic:").strong());
                render_irc_text(ui, topic, Color32::WHITE);
            });
        }
        if self.is_channel_name(channel_name) || channel.topic.is_some() {
            ui.separator();
        }
    }

    fn draw_user_list(&mut self, ui: &mut egui::Ui, channel_name: &str) {
        let mut pm_to_open: Option<String> = None;
        let mut whois_nick: Option<String> = None;
        let mut op_nick: Option<(String, String)> = None;
        let mut voice_nick: Option<(String, String)> = None;
        let mut kick_nick: Option<(String, String)> = None;

        if let Some(channel) = self.conversations.get(channel_name) {
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
            let current_selected = self.selected_user.clone();
            let mut new_selected: Option<String> = current_selected.clone();

            // Row virtualization: only the visible slice is built each frame,
            // which matters for channels with thousands of users.
            let row_height = ui.text_style_height(&egui::TextStyle::Body);
            ScrollArea::vertical()
                .id_salt(("user_list", self.id, self.current_conversation))
                .show_rows(ui, row_height, channel.users.len(), |ui, row_range| {
                    for user in &channel.users[row_range] {
                        let nick = &user.nick;
                        let is_selected = current_selected.as_ref() == Some(nick);
                        let is_away = user.is_away();
                        let base_color = match user.mode {
                            UserMode::Owner => Color32::from_rgb(255, 100, 100),
                            UserMode::Admin => Color32::from_rgb(255, 150, 100),
                            UserMode::Op => Color32::from_rgb(100, 200, 100),
                            UserMode::HalfOp => Color32::from_rgb(100, 200, 200),
                            UserMode::Voice => Color32::from_rgb(200, 200, 100),
                            UserMode::Normal => Color32::WHITE,
                        };
                        let color = if is_away {
                            Color32::from_rgb(
                                base_color.r() / 2,
                                base_color.g() / 2,
                                base_color.b() / 2,
                            )
                        } else {
                            base_color
                        };
                        let state = match (is_away, user.bot) {
                            (true, true) => " (away, bot)",
                            (true, false) => " (away)",
                            (false, true) => " (bot)",
                            (false, false) => "",
                        };
                        let display_text = format!("{}{nick}{state}", user.prefix());
                        let response = ui.selectable_label(
                            is_selected,
                            RichText::new(display_text).color(color),
                        );
                        if let Some(away_msg) = &user.away {
                            response
                                .clone()
                                .on_hover_text(format!("Away: {}", away_msg));
                        }
                        if response.clicked() {
                            new_selected = Some(nick.clone());
                        }
                        if response.double_clicked() {
                            pm_to_open = Some(nick.clone());
                        }
                        let nick_clone = nick.clone();
                        let chan_clone = channel_name.to_string();
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
            self.selected_user = new_selected;
        }

        if let Some(nick) = pm_to_open {
            self.open_query(&nick);
        }
        if let Some(nick) = whois_nick {
            // Double target: the reply comes from the user's own server,
            // revealing the real host behind a cloak plus idle/signon (#164).
            self.send_command(IrcCommand::Whois(commands::whois_query_target(&nick)));
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
    }

    fn consume_plain_composer_key(ui: &mut egui::Ui, key: egui::Key) -> bool {
        ui.input_mut(|input|{let mut pressed=false;input.events.retain(|event|{if matches!(event,egui::Event::Key{key:event_key,pressed:true,modifiers,..} if *event_key==key&&modifiers.is_none()){pressed=true;false}else{true}});pressed})
    }
    fn draw_input_row(&mut self, ui: &mut egui::Ui) {
        let input_before = self.input_text.clone();
        let mut send = false;
        let mut insert = None;
        let mut history_up = false;
        let mut history_down = false;
        let mut tab_complete = false;
        if let Some(reference) = self.composer.reply_to.clone() {
            let preview = self
                .current_target_name()
                .and_then(|target| self.conversations.get(target))
                .and_then(|channel| {
                    channel.messages.iter().find(|message| {
                        reference.archive_id.as_deref() == Some(message.message_key.as_str())
                            || reference
                                .server_msgid
                                .as_ref()
                                .is_some_and(|id| message.msgid.as_ref() == Some(id))
                    })
                })
                .map(|message| {
                    format!(
                        "{}: {}",
                        message.sender,
                        super::truncate_chars(&message.content, 100)
                    )
                })
                .unwrap_or_else(|| "Message unavailable locally".into());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("Replying to {preview}")).small());
                if ui.small_button("Cancel reply").clicked() {
                    self.composer.reply_to = None;
                }
            });
        }
        if self.attention.show_typing {
            let label = self.typing_label();
            if !label.is_empty() {
                ui.label(RichText::new(label).small().color(Color32::GRAY));
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_secs(1));
            }
        }
        let input_id = egui::Id::unique(("composer", self.id, self.current_conversation));
        let focused = ui.ctx().memory(|memory| memory.has_focus(input_id));
        if focused {
            self.capture_upload_input(ui.ctx());
            let ime_was_active = self.composer.ime_active;
            ui.input(|input| {
                for event in &input.events {
                    match event {
                        egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => {
                            self.composer.ime_active = !text.is_empty()
                        }
                        egui::Event::Ime(egui::ImeEvent::Commit(_)) => {
                            self.composer.ime_active = false
                        }
                        _ => {}
                    }
                }
            });
            if !ime_was_active && !self.composer.ime_active {
                send = Self::consume_plain_composer_key(ui, egui::Key::Enter);
            }
            tab_complete = Self::consume_plain_composer_key(ui, egui::Key::Tab);
            if !self.input_text.contains('\n') {
                history_up = Self::consume_plain_composer_key(ui, egui::Key::ArrowUp);
                history_down = Self::consume_plain_composer_key(ui, egui::Key::ArrowDown);
            }
        }
        ui.horizontal_wrapped(|ui| {
            for (label, code, help) in [
                ("B", '\x02', "Bold"),
                ("U", '\x1f', "Underline"),
                ("I", '\x1d', "Italic"),
                ("Reset", '\x0f', "Reset formatting"),
            ] {
                if ui.small_button(label).on_hover_text(help).clicked() {
                    insert = Some(code);
                }
            }
            if ui.small_button("Color").clicked() {
                self.show_color_picker = true;
                self.color_picker_fg = true;
            }
            self.draw_upload_controls(ui);
            if ui
                .button("Send")
                .on_hover_text("Enter sends; Shift+Enter adds a line")
                .clicked()
            {
                send = true;
            }
            ui.label(
                RichText::new("Enter sends · Shift+Enter new line")
                    .small()
                    .color(Color32::GRAY),
            );
        });
        let output = TextEdit::multiline(&mut self.input_text)
            .id(input_id)
            .hint_text("Type a message…")
            .desired_rows(3)
            .desired_width(ui.available_width())
            .show(ui);
        if std::mem::take(&mut self.input_focus_request) {
            output.response.request_focus();
            if let Some(cursor) = self.composer.cursor {
                let mut state = output.state.clone();
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(
                        egui::text::CCursor::new(cursor),
                    )));
                state.store(ui.ctx(), input_id);
            }
        }
        if output.response.lost_focus() {
            self.share_typing("done", true);
        }
        self.input_had_focus = output.response.has_focus();
        if let Some(range) = output.cursor_range {
            self.composer.cursor = Some(range.primary.index.into());
        }
        if let Some(code) = insert {
            let cursor = output
                .cursor_range
                .map_or(self.input_text.chars().count(), |range| {
                    range.primary.index.into()
                });
            let byte = self
                .input_text
                .char_indices()
                .nth(cursor)
                .map_or(self.input_text.len(), |(index, _)| index);
            self.input_text.insert(byte, code);
            self.composer.cursor = Some(cursor + 1);
            let mut state = output.state;
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(cursor + 1),
                )));
            state.store(ui.ctx(), input_id);
            output.response.request_focus();
        }
        if send {
            self.submit_composer();
            output.response.request_focus();
        }
        if history_up {
            self.history_up();
        }
        if history_down {
            self.history_down();
        }
        if tab_complete {
            self.handle_tab_completion();
        } else if self.input_text != input_before {
            self.reset_tab_completion();
        }
        if self.input_text != input_before && output.response.has_focus() && !send {
            self.share_typing(
                if self.input_text.is_empty() {
                    "done"
                } else {
                    "active"
                },
                self.input_text.is_empty(),
            );
        }
    }
}

#[cfg(test)]
mod composer_tests {
    use super::*;
    use crate::gui::{Channel, Settings};
    use tokio::sync::mpsc;
    fn frame(
        session: &mut Session,
        ctx: &egui::Context,
        size: egui::Vec2,
        events: Vec<egui::Event>,
        _modifiers: egui::Modifiers,
    ) {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ui| session.draw_input_row(ui),
        );
        output.textures_delta.clear();
    }
    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }
    #[test]
    fn enter_sends_and_shift_enter_preserves_multiline_at_compact_sizes() {
        for size in [egui::vec2(640.0, 480.0), egui::vec2(320.0, 240.0)] {
            let mut session = Session::with_settings(Settings {
                nickname: "me".into(),
                logging_enabled: false,
                logging_load_history: false,
                ..Settings::default()
            });
            assert!(session.start_session_from_form());
            let (tx, mut rx) = mpsc::unbounded_channel();
            session.test_set_registered(tx);
            let mut channel = Channel::new();
            channel.joined = true;
            session.conversations.insert("#room".into(), channel);
            session.current_conversation = session.id_for_target("#room");
            session.input_text = "first".into();
            session.input_focus_request = true;
            let ctx = egui::Context::default();
            frame(&mut session, &ctx, size, Vec::new(), egui::Modifiers::NONE);
            // Move to the end explicitly, then Shift+Enter remains a text-edit event.
            frame(
                &mut session,
                &ctx,
                size,
                vec![key(egui::Key::End, egui::Modifiers::NONE)],
                egui::Modifiers::NONE,
            );
            frame(
                &mut session,
                &ctx,
                size,
                vec![key(egui::Key::Enter, egui::Modifiers::SHIFT)],
                egui::Modifiers::SHIFT,
            );
            assert!(
                session.input_text.contains('\n'),
                "{size:?}: {:?}",
                session.input_text
            );
            assert!(rx.try_recv().is_err());
            frame(
                &mut session,
                &ctx,
                size,
                vec![
                    egui::Event::Text("second".into()),
                    key(egui::Key::Enter, egui::Modifiers::NONE),
                ],
                egui::Modifiers::NONE,
            );
            assert!(session.input_text.is_empty(), "{size:?}");
            assert!(
                matches!(rx.try_recv(),Ok(IrcCommand::ClientChat{commands:lines,..})if lines.len()==2)
            );
            assert!(rx.try_recv().is_err());
        }
    }
}
