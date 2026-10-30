//! Drawing one session's pieces for the window manager in `app.rs`: the
//! toolbar status summary and join box, and a window's contents (topic,
//! user list, scrollback and input line). A window draws with its session's
//! `current_channel` pointed at the window's target and the window's input
//! state swapped in (see `app::with_window_view`).

use egui::{Color32, RichText, ScrollArea, TextEdit};

use super::formatting::render_irc_text;
use super::{Session, UserMode, commands};
use crate::irc::IrcCommand;

impl Session {
    /// Connection status for the toolbar: state dot, nick, network, lag and
    /// away message.
    pub(crate) fn draw_status_summary(&self, ui: &mut egui::Ui, network_label: &str) {
        if self.connected {
            if self.away_status.is_some() {
                ui.label(RichText::new("●").color(Color32::from_rgb(255, 200, 0)));
            } else {
                ui.label(RichText::new("●").color(Color32::GREEN));
            }
            ui.label(&self.my_nick);
            ui.label("@");
            ui.label(network_label);

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
        } else if self.connecting {
            ui.label(RichText::new("●").color(Color32::YELLOW));
            ui.label(format!("Connecting to {network_label}..."));
        } else {
            ui.label(RichText::new("●").color(Color32::RED));
            ui.label("Disconnected");
        }
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
        self.channel_info_target = Some(channel.to_string());
        self.show_channel_info = true;
        self.send_command(IrcCommand::Mode(channel.to_string(), None, Vec::new()));
    }

    /// One window's contents for the current target: header, user list
    /// (channels only), scrollback and input line. Everything is laid out
    /// to the space the window gives it and nothing has a minimum width
    /// beyond it, so a floating window keeps the size it was tiled to or
    /// dragged to instead of growing to fit its content.
    pub(crate) fn draw_target(&mut self, ui: &mut egui::Ui) {
        let salt = (self.id, self.current_channel.clone());
        self.draw_target_header(ui);
        if let Some(channel_name) = self.current_channel.clone()
            && self.is_channel_name(&channel_name)
        {
            egui::Panel::right(("users", salt.clone()))
                .resizable(true)
                .default_size(140.0)
                .min_size(80.0)
                .max_size((ui.available_width() * 0.35).max(80.0))
                .show(ui, |ui| self.draw_user_list(ui, &channel_name));
        }
        egui::Panel::bottom(("input", salt.clone()))
            .show_separator_line(true)
            .show(ui, |ui| self.draw_input_row(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            let available_height = ui.available_height();
            self.draw_scrollback(ui, available_height);
        });
    }

    fn draw_target_header(&self, ui: &mut egui::Ui) {
        let Some(channel_name) = &self.current_channel else {
            return;
        };
        let Some(channel) = self.channels.get(channel_name) else {
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

        if let Some(channel) = self.channels.get(channel_name) {
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
                .id_salt(("user_list", self.id, channel_name))
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
                        let response =
                            ui.selectable_label(is_selected, RichText::new(display_text).color(color));
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
            self.send_command(IrcCommand::Mode(channel, Some("+o".to_string()), vec![nick]));
        }
        if let Some((channel, nick)) = voice_nick {
            self.send_command(IrcCommand::Mode(channel, Some("+v".to_string()), vec![nick]));
        }
        if let Some((channel, nick)) = kick_nick {
            self.send_command(IrcCommand::Kick(channel, nick, None));
        }
    }

    fn draw_input_row(&mut self, ui: &mut egui::Ui) {
        let mut history_up = false;
        let mut history_down = false;
        let mut tab_complete = false;
        let mut insert_bold = false;
        let mut insert_underline = false;
        let mut insert_italic = false;
        let mut insert_color = false;
        let mut insert_reset = false;
        let input_before = self.input_text.clone();

        let mut send = false;
        ui.horizontal(|ui| {
            // The formatting buttons give way in narrow windows; their
            // keyboard shortcuts still work.
            if ui.available_width() >= 300.0 {
                if ui.small_button("B").on_hover_text("Bold (Ctrl+B)").clicked() {
                    insert_bold = true;
                }
                if ui
                    .small_button("U")
                    .on_hover_text("Underline (Ctrl+U)")
                    .clicked()
                {
                    insert_underline = true;
                }
                if ui.small_button("I").on_hover_text("Italic (Ctrl+I)").clicked() {
                    insert_italic = true;
                }
                if ui.small_button("C").on_hover_text("Color (Ctrl+K)").clicked() {
                    insert_color = true;
                }
            }

            let has_formatting = self.input_text.contains('\x02')
                || self.input_text.contains('\x1F')
                || self.input_text.contains('\x1D')
                || self.input_text.contains('\x03');
            if has_formatting {
                ui.label(RichText::new("*").color(Color32::YELLOW).small());
            }

            // Send at the right edge, the text box filling what is left.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Send").clicked() {
                    send = true;
                }
                let response = ui.add(
                    TextEdit::singleline(&mut self.input_text)
                        .id_salt(("input", self.id, self.current_channel.clone()))
                        .hint_text("Type a message...")
                        .desired_width(ui.available_width()),
                );

                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    send = true;
                    response.request_focus();
                }

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
                    if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::B)) {
                        insert_bold = true;
                    }
                    if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::U)) {
                        insert_underline = true;
                    }
                    // Note: Ctrl+I may be captured by the system; use the button.
                    if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::K)) {
                        insert_color = true;
                    }
                    if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::O)) {
                        insert_reset = true;
                    }
                }
            });
        });

        if send {
            self.process_input();
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
    }
}
