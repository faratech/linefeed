//! Drawing one session's pieces for the window manager in `app.rs`: the
//! toolbar status summary and join box, and a window's contents (topic,
//! user list, scrollback and input line). A window draws with its session's
//! `current_conversation` pointed at the window's target and the window's input
//! state swapped in (see `app::with_window_view`).

use egui::{RichText, ScrollArea, TextEdit};

use super::{Session, commands};
use crate::irc::IrcCommand;

/// The people button's label: "1 person", "12 people".
fn people_label(count: usize) -> String {
    if count == 1 {
        "1 person".to_owned()
    } else {
        format!("{count} people")
    }
}

impl Session {
    /// Connection status for the toolbar: state dot, nick, network, lag and
    /// away message.
    /// The one-line connection summary for the top bar, and whether it
    /// reports something the user should notice (an unencrypted or
    /// unverified link).
    pub(crate) fn status_line(&self, network_label: &str) -> (String, bool) {
        use crate::irc::client::{AuthenticationStatus, TlsStatus};
        if !self.is_connected() {
            let text = if self.is_connecting() {
                format!("Connecting to {network_label}...")
            } else {
                "Disconnected".to_owned()
            };
            return (text, false);
        }
        let (security, warning) = match self.connection_status.tls {
            TlsStatus::Verified => ("Encrypted", false),
            TlsStatus::Unverified => ("Encrypted, certificate not checked", true),
            TlsStatus::Negotiating => ("Securing…", false),
            TlsStatus::Plaintext => ("Not encrypted", true),
        };
        let mut parts = vec![security.to_owned()];
        parts.push(match &self.connection_status.authentication {
            AuthenticationStatus::LoggedIn { account } => format!("signed in as {account}"),
            AuthenticationStatus::Authenticated => "signed in".to_owned(),
            AuthenticationStatus::Authenticating => "signing in…".to_owned(),
            _ => "not signed in".to_owned(),
        });
        if let Some(lag) = self.lag_ms {
            parts.push(format!("{lag} ms"));
        }
        if let Some(away) = &self.away_status {
            parts.push(format!("away: {away}"));
        }
        (parts.join(" · "), warning)
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
        let palette = super::theme::palette(ui.ctx());
        let (tls, color) = match self.connection_status.tls {
            TlsStatus::Verified => ("TLS verified", palette.online),
            TlsStatus::Unverified => ("TLS unverified", palette.mention),
            TlsStatus::Negotiating => ("TLS negotiating", palette.mention),
            TlsStatus::Plaintext => ("Plaintext", palette.mention),
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

    /// Join the channel typed into the sidebar's join row.
    pub(crate) fn join_typed_channel(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let channel = self.normalize_channel_name(text.trim());
        self.send_join(channel, None);
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
    pub(crate) fn draw_target(
        &mut self,
        ui: &mut egui::Ui,
        network_label: &str,
        narrow: bool,
        controls: &mut dyn FnMut(&mut egui::Ui),
    ) {
        let salt = (self.id, self.current_conversation);
        let palette = super::theme::palette(ui.ctx());
        self.capture_upload_drop(ui);
        self.draw_target_header(ui, network_label, narrow, controls);
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
            && self.people_visible(narrow)
        {
            egui::Panel::right(("users", salt))
                .frame(egui::Frame::side_top_panel(ui.style()).fill(palette.panel))
                .resizable(true)
                .default_size(208.0)
                .min_size(140.0)
                .max_size((ui.available_width() * 0.35).max(140.0))
                .show(ui, |ui| self.draw_user_list(ui, &channel_name));
        }
        // Messages and the composer sit on the chat ground; the people
        // list is a panel beside them.
        egui::Panel::bottom(("input", salt))
            .frame(egui::Frame::side_top_panel(ui.style()).fill(palette.bg))
            .show_separator_line(true)
            .show(ui, |ui| self.draw_input_row(ui));
        let chat_ground = egui::Frame::central_panel(ui.style()).fill(palette.bg);
        egui::CentralPanel::default()
            .frame(chat_ground)
            .show(ui, |ui| {
                let available_height = ui.available_height();
                self.draw_scrollback(ui, available_height);
            });
    }

    /// Whether this window shows its people list: the user's own choice,
    /// or by default everywhere but in a narrow window.
    pub(crate) fn people_visible(&self, narrow: bool) -> bool {
        !self.people_hidden.unwrap_or(narrow)
    }

    /// One row heading the window: its name, the topic on a single line, a
    /// button that shows or hides the people list, and the window's own
    /// controls at the right.
    fn draw_target_header(
        &mut self,
        ui: &mut egui::Ui,
        network_label: &str,
        narrow: bool,
        controls: &mut dyn FnMut(&mut egui::Ui),
    ) {
        let palette = super::theme::palette(ui.ctx());
        let target = self.current_target_name().map(str::to_owned);
        let channel = target
            .as_deref()
            .and_then(|name| self.conversations.get(name));
        let is_channel = target
            .as_deref()
            .is_some_and(|name| self.is_channel_name(name));
        let name = target.as_deref().unwrap_or("Status").to_owned();
        // What follows the name: the topic, or the network for the status
        // window.
        let detail = match (&target, channel) {
            (None, _) => Some(network_label.to_owned()),
            (Some(_), Some(channel)) => channel
                .topic
                .as_deref()
                .map(super::formatting::strip_irc_formatting),
            _ => None,
        };
        let modes = channel
            .filter(|channel| is_channel && !channel.modes.is_empty())
            .map(|channel| format!("Modes: {}", channel.modes));
        let people = channel.filter(|_| is_channel).map(|channel| {
            let away = channel.away_count();
            let hover = if away > 0 {
                format!("{away} away. Show or hide the people list")
            } else {
                "Show or hide the people list".to_owned()
            };
            (people_label(channel.users().len()), hover)
        });
        let mut toggle_people = false;
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                controls(ui);
                if let Some((label, hover)) = &people
                    && ui.small_button(label).on_hover_text(hover).clicked()
                {
                    toggle_people = true;
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let heading = egui::TextStyle::Heading.resolve(ui.style()).size;
                    // A long name is cut rather than run over the buttons;
                    // the topic gets whatever width is left.
                    let response = super::widgets::truncated(
                        ui,
                        super::widgets::strong(&name)
                            .size(heading)
                            .color(palette.text),
                    );
                    if let Some(modes) = &modes {
                        response.on_hover_text(modes);
                    }
                    if let Some(detail) = detail {
                        super::widgets::truncated(ui, RichText::new(detail).color(palette.muted));
                    }
                });
            });
        });
        if toggle_people {
            self.people_hidden = Some(self.people_visible(narrow));
        }
        ui.separator();
    }

    fn draw_user_list(&mut self, ui: &mut egui::Ui, channel_name: &str) {
        let mut pm_to_open: Option<String> = None;
        let mut whois_nick: Option<String> = None;
        let mut op_nick: Option<(String, String)> = None;
        let mut voice_nick: Option<(String, String)> = None;
        let mut kick_nick: Option<(String, String)> = None;

        if let Some(channel) = self.conversations.get(channel_name) {
            let palette = super::theme::palette(ui.ctx());
            let rows = channel.people_rows();
            let current_selected = self.selected_user.clone();
            let mut new_selected: Option<String> = current_selected.clone();

            // Row virtualization: only the visible slice is built each frame,
            // which matters for channels with thousands of users. Headings
            // are rows of the same height as people, so every row is uniform.
            let row_height = super::theme::control_height(ui);
            ScrollArea::vertical()
                .id_salt(("user_list", self.id, self.current_conversation))
                .auto_shrink([false, false])
                .show_rows(ui, row_height, rows.len(), |ui, row_range| {
                    for row in &rows[row_range] {
                        let user = match row {
                            super::types::PeopleRow::Group { label, count } => {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), row_height),
                                    egui::Sense::hover(),
                                );
                                let mut heading = ui.new_child(
                                    egui::UiBuilder::new()
                                        .max_rect(rect.shrink2(egui::vec2(10.0, 0.0)))
                                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                                );
                                super::widgets::section_label(
                                    &mut heading,
                                    &format!("{label} · {count}"),
                                );
                                continue;
                            }
                            super::types::PeopleRow::Person(position) => {
                                let Some(user) = channel.users().get(*position) else {
                                    continue;
                                };
                                user
                            }
                        };
                        let nick = &user.nick;
                        let is_selected = current_selected.as_ref() == Some(nick);
                        let is_away = user.is_away();
                        let note = match (is_away, user.bot) {
                            (true, true) => "away, bot",
                            (true, false) => "away",
                            (false, true) => "bot",
                            (false, false) => "",
                        };
                        let response = super::widgets::list_row(ui, is_selected, nick, |ui| {
                            // Away is a ring, present a filled dot: told
                            // apart by shape, not by colour alone.
                            let dot = if is_away {
                                palette.faint
                            } else {
                                palette.online
                            };
                            super::widgets::dot(ui, dot, !is_away);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if !note.is_empty() {
                                        ui.label(RichText::new(note).small().color(palette.faint));
                                    }
                                    ui.with_layout(
                                        egui::Layout::left_to_right(egui::Align::Center),
                                        |ui| {
                                            let color =
                                                if is_away { palette.faint } else { palette.text };
                                            super::widgets::truncated(
                                                ui,
                                                RichText::new(nick).color(color),
                                            );
                                        },
                                    );
                                },
                            );
                        });
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
        let palette = super::theme::palette(ui.ctx());
        let metrics = super::theme::metrics(ui.ctx());
        if self.attention.show_typing {
            // The line keeps its height when nobody types, so the composer
            // does not jump as people start and stop.
            let (line, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::hover());
            let label = self.typing_label();
            if !label.is_empty() {
                ui.painter().text(
                    line.left_center() + egui::vec2(4.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    label,
                    egui::TextStyle::Small.resolve(ui.style()),
                    palette.muted,
                );
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
        let input_font = match metrics.message_font {
            super::theme::MessageFont::Proportional => egui::TextStyle::Body,
            super::theme::MessageFont::Monospace => egui::TextStyle::Monospace,
        }
        .resolve(ui.style());
        let line_height = ui.fonts_mut(|fonts| fonts.row_height(&input_font))
            + ui.spacing().extra_text_line_spacing;
        let control_height = super::theme::control_height(ui);
        let max_height = (line_height * 4.0 + 8.0).max(control_height);
        let hint = self.current_target_name().map_or_else(
            || "Type a message…".to_owned(),
            |target| format!("Message {target}"),
        );
        let narrow = super::app::narrow(ui.ctx());
        let output = ui
            .scope_builder(
                egui::UiBuilder::new().scope_id(input_id.with("row")),
                |ui| {
                    ui.horizontal_top(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                            let send_button = egui::Button::new(
                                super::widgets::strong("Send").color(palette.on_accent),
                            )
                            .fill(palette.accent);
                            if ui
                                .add(send_button)
                                .on_hover_text("Enter sends; Shift+Enter adds a line")
                                .clicked()
                            {
                                send = true;
                            }
                            let format_label = if narrow { "Aa" } else { "Format" };
                            let format = ui.menu_button(format_label, |ui| {
                                for (label, code) in [
                                    ("Bold", '\x02'),
                                    ("Underline", '\x1f'),
                                    ("Italic", '\x1d'),
                                    ("Reset formatting", '\x0f'),
                                ] {
                                    if ui.button(label).clicked() {
                                        insert = Some(code);
                                        ui.close();
                                    }
                                }
                                if ui.button("Color…").clicked() {
                                    self.show_color_picker = true;
                                    self.color_picker_fg = true;
                                    ui.close();
                                }
                            });
                            if narrow {
                                format.response.on_hover_text("Text formatting");
                            }
                            // The panel starts at its previous height. Allow the
                            // draft to grow, then shrink the viewport to its text.
                            ui.set_max_height(max_height);
                            ScrollArea::vertical()
                                .id_salt(("composer_scroll", self.id, self.current_conversation))
                                .auto_shrink([false, true])
                                .min_scrolled_height(0.0)
                                .max_height(max_height)
                                .show(ui, |ui| {
                                    TextEdit::multiline(&mut self.input_text)
                                        .id(input_id)
                                        .font(input_font.clone())
                                        .hint_text(hint.as_str())
                                        .background_color(palette.panel)
                                        .min_size(egui::vec2(0.0, control_height))
                                        .align(egui::Align2::LEFT_CENTER)
                                        .desired_rows(1)
                                        .desired_width(ui.available_width())
                                        .show(ui)
                                })
                                .inner
                        })
                        .inner
                    })
                    .inner
                },
            )
            .inner;
        if metrics.density == super::theme::Density::Comfortable {
            ui.label(
                RichText::new("Enter sends. Shift+Enter starts a new line.")
                    .small()
                    .color(palette.faint),
            );
        }
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
                .map(|range| range.primary.index.into())
                .or(self.composer.cursor)
                .unwrap_or_else(|| self.input_text.chars().count())
                .min(self.input_text.chars().count());
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
    use egui::Color32;

    #[test]
    fn the_people_button_counts_one_person_and_several_people() {
        assert_eq!(people_label(1), "1 person");
        assert_eq!(people_label(0), "0 people");
        assert_eq!(people_label(12), "12 people");
    }
    use crate::gui::{Channel, Settings};
    use tokio::sync::mpsc;
    fn frame(
        session: &mut Session,
        ctx: &egui::Context,
        size: egui::Vec2,
        events: Vec<egui::Event>,
        _modifiers: egui::Modifiers,
    ) -> egui::Vec2 {
        let mut used = egui::Vec2::ZERO;
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ui| {
                egui::Panel::bottom("test_composer").show(ui, |ui| {
                    used = ui
                        .scope(|ui| session.draw_input_row(ui))
                        .response
                        .rect
                        .size();
                });
            },
        );
        output.textures_delta.clear();
        used
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

    /// One frame of the composer at this size: its height, the texts it
    /// painted with their colours, and its filled rectangles.
    #[allow(clippy::type_complexity)]
    fn painted(
        session: &mut Session,
        ctx: &egui::Context,
        size: egui::Vec2,
    ) -> (
        f32,
        Vec<(String, egui::Rect, Color32)>,
        Vec<(egui::Rect, Color32)>,
    ) {
        fn collect(
            shape: &egui::Shape,
            texts: &mut Vec<(String, egui::Rect, Color32)>,
            fills: &mut Vec<(egui::Rect, Color32)>,
        ) {
            match shape {
                egui::Shape::Text(text) => {
                    let color = text
                        .galley
                        .job
                        .sections
                        .first()
                        .map(|section| section.format.color)
                        .filter(|color| *color != Color32::PLACEHOLDER)
                        .unwrap_or(text.fallback_color);
                    texts.push((
                        text.galley.text().to_string(),
                        text.galley.rect.translate(text.pos.to_vec2()),
                        color,
                    ));
                }
                egui::Shape::Rect(rect) => fills.push((rect.rect, rect.fill)),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect(s, texts, fills)),
                _ => {}
            }
        }
        let mut height = 0.0;
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                ..Default::default()
            },
            |ui| {
                egui::Panel::bottom("test_composer").show(ui, |ui| {
                    height = ui
                        .scope(|ui| session.draw_input_row(ui))
                        .response
                        .rect
                        .height();
                });
            },
        );
        output.textures_delta.clear();
        let (mut texts, mut fills) = (Vec::new(), Vec::new());
        for clipped in &output.shapes {
            collect(&clipped.shape, &mut texts, &mut fills);
        }
        (height, texts, fills)
    }

    fn room_session() -> Session {
        let mut session = Session::with_settings(Settings {
            nickname: "me".into(),
            logging_enabled: false,
            logging_load_history: false,
            ..Settings::default()
        });
        let mut channel = Channel::new();
        channel.joined = true;
        session.conversations.insert("#room".into(), channel);
        session.current_conversation = session.id_for_target("#room");
        session
    }

    #[test]
    fn the_composer_names_its_target_in_the_hint() {
        let ctx = egui::Context::default();
        let size = egui::vec2(640.0, 240.0);
        let mut session = room_session();
        let (_, texts, _) = painted(&mut session, &ctx, size);
        assert!(
            texts.iter().any(|(text, _, _)| text == "Message #room"),
            "{texts:?}"
        );
        session.current_conversation = None;
        let (_, texts, _) = painted(&mut session, &ctx, size);
        assert!(texts.iter().any(|(text, _, _)| text == "Type a message…"));
    }

    #[test]
    fn send_is_the_accent_button_as_tall_as_a_control() {
        use crate::gui::theme::{self, DisplayPrefs};
        let ctx = egui::Context::default();
        theme::apply(&ctx, &DisplayPrefs::default());
        let palette = theme::palette(&ctx);
        let mut session = room_session();
        let size = egui::vec2(640.0, 240.0);
        painted(&mut session, &ctx, size);
        let (_, texts, fills) = painted(&mut session, &ctx, size);
        let (_, send, color) = texts
            .iter()
            .find(|(text, _, _)| text == "Send")
            .expect("a Send button")
            .clone();
        assert_eq!(color, palette.on_accent);
        let button = fills
            .iter()
            .find(|(rect, fill)| *fill == palette.accent && rect.contains_rect(send))
            .expect("Send is filled with the accent")
            .0;
        assert_eq!(button.height(), 44.0);
        // The last one painted: the panel behind the composer has the same
        // colour in this test.
        let field = fills
            .iter()
            .rev()
            .find(|(rect, fill)| *fill == palette.panel && rect.width() > 300.0)
            .expect("the message field is filled with the panel colour")
            .0;
        assert!(field.height() >= 44.0, "{field:?}");
        assert!(
            (field.center().y - button.center().y).abs() < 2.0,
            "field and Send line up: {field:?} vs {button:?}"
        );
        assert!(texts.iter().any(|(text, _, color)| text
            == "Enter sends. Shift+Enter starts a new line."
            && *color == palette.faint));
    }

    #[test]
    fn the_hint_line_is_left_out_of_the_compact_spacing() {
        use crate::gui::theme::{self, Density, DisplayPrefs};
        let ctx = egui::Context::default();
        theme::apply(
            &ctx,
            &DisplayPrefs {
                density: Density::Compact,
                ..DisplayPrefs::default()
            },
        );
        let mut session = room_session();
        let (_, texts, _) = painted(&mut session, &ctx, egui::vec2(640.0, 240.0));
        assert!(
            !texts
                .iter()
                .any(|(text, _, _)| text.starts_with("Enter sends."))
        );
    }

    #[test]
    fn the_typing_line_reserves_its_height_when_nobody_types() {
        let ctx = egui::Context::default();
        let size = egui::vec2(640.0, 240.0);
        let mut session = room_session();
        painted(&mut session, &ctx, size);
        let (quiet, texts, _) = painted(&mut session, &ctx, size);
        assert!(!texts.iter().any(|(text, _, _)| text.contains("typing")));
        let typing =
            crate::irc::IrcMessage::parse("@+typing=active :alice!u@h TAGMSG #room").unwrap();
        session.observe_tagmsg(&typing, "#room", false);
        let (typed, texts, _) = painted(&mut session, &ctx, size);
        assert!(
            texts.iter().any(|(text, _, _)| text == "alice is typing…"),
            "{texts:?}"
        );
        assert_eq!(
            typed, quiet,
            "the composer does not jump when someone types"
        );

        // With typing indicators off there is no line to reserve.
        session.attention.show_typing = false;
        let (off, _, _) = painted(&mut session, &ctx, size);
        assert!(off < quiet - 15.0, "{off} vs {quiet}");
    }

    #[test]
    fn composer_starts_on_one_row_and_bounds_long_drafts() {
        for width in [320.0, 640.0] {
            for target in [None, Some("#room")] {
                let mut session = Session::with_settings(Settings::default());
                if let Some(target) = target {
                    session.conversations.insert(target.into(), Channel::new());
                    session.current_conversation = session.id_for_target(target);
                }
                // The compact composer on its own: no typing line, no hint.
                session.attention.show_typing = false;
                let ctx = egui::Context::default();
                crate::gui::theme::apply(
                    &ctx,
                    &crate::gui::theme::DisplayPrefs {
                        density: crate::gui::theme::Density::Compact,
                        font_size: 14.0,
                        ..Default::default()
                    },
                );
                let size = egui::vec2(width, 240.0);
                let mut measure = |text: &str| {
                    session.input_text = text.into();
                    frame(&mut session, &ctx, size, Vec::new(), egui::Modifiers::NONE);
                    frame(&mut session, &ctx, size, Vec::new(), egui::Modifiers::NONE)
                };
                let empty = measure("");
                let short = measure("Hello");
                let multiline = measure("First\nSecond\nThird");
                let many_lines = measure(&"Line\n".repeat(100));
                let wrapped = measure(&"A long wrapped draft ".repeat(100));
                let cleared = measure("");
                assert!(empty.y <= 36.0, "{target:?} {width}: {empty:?}");
                assert!((short.y - empty.y).abs() <= 1.0);
                assert!(multiline.y > short.y);
                assert!(many_lines.y <= 88.0, "{target:?} {width}: {many_lines:?}");
                assert!(wrapped.y <= 88.0, "{target:?} {width}: {wrapped:?}");
                assert!(wrapped.y > short.y);
                assert!((cleared.y - empty.y).abs() <= 1.0);
                for measured in [empty, short, multiline, many_lines, wrapped, cleared] {
                    assert!(measured.x <= width + 1.0, "{width}: {measured:?}");
                }
            }
        }
    }
}
