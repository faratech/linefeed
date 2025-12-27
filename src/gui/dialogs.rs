//! Dialog windows for connect, settings, and channel list

use egui::{Color32, RichText, ScrollArea, TextEdit, Vec2};
use crate::irc::IrcCommand;
use super::{IrcApp, ServerFavorite};
use super::helpers::format_timestamp;

impl IrcApp {
    pub fn show_connect_window(&mut self, ctx: &egui::Context) {
        egui::Window::new("Connect to Server")
            .collapsible(false)
            .resizable(true)
            .default_width(450.0)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                // ===== Server Favorites Section =====
                egui::CollapsingHeader::new("Server Favorites")
                    .default_open(!self.server_favorites.is_empty())
                    .show(ui, |ui| {
                        if self.server_favorites.is_empty() {
                            ui.label("No saved favorites. Fill in connection details and click 'Save as Favorite'.");
                        } else {
                            // Favorites list with selection
                            ScrollArea::vertical()
                                .max_height(120.0)
                                .show(ui, |ui| {
                                    let mut clicked_idx = None;
                                    for (idx, fav) in self.server_favorites.iter().enumerate() {
                                        let is_selected = self.selected_favorite == Some(idx);
                                        let text = format!("{} ({}:{})", fav.name, fav.host, fav.port);
                                        let response = ui.selectable_label(is_selected, &text);
                                        if response.clicked() {
                                            clicked_idx = Some(idx);
                                        }
                                        if response.double_clicked() {
                                            // Load and connect on double-click
                                            clicked_idx = Some(idx);
                                        }
                                    }
                                    if let Some(idx) = clicked_idx {
                                        self.selected_favorite = Some(idx);
                                    }
                                });

                            ui.horizontal(|ui| {
                                // Load button
                                let has_selection = self.selected_favorite.is_some();
                                if ui.add_enabled(has_selection, egui::Button::new("Load")).clicked() {
                                    if let Some(idx) = self.selected_favorite {
                                        if let Some(fav) = self.server_favorites.get(idx).cloned() {
                                            self.load_favorite(&fav);
                                        }
                                    }
                                }

                                // Delete button
                                if ui.add_enabled(has_selection, egui::Button::new("Delete")).clicked() {
                                    if let Some(idx) = self.selected_favorite {
                                        self.server_favorites.remove(idx);
                                        self.selected_favorite = None;
                                        self.save_settings();
                                    }
                                }

                                // Connect button
                                if ui.add_enabled(has_selection, egui::Button::new("Connect")).clicked() {
                                    if let Some(idx) = self.selected_favorite {
                                        if let Some(fav) = self.server_favorites.get(idx).cloned() {
                                            self.load_favorite(&fav);
                                            self.save_settings();
                                            self.show_connect_dialog = false;
                                            self.connecting = true;
                                            self.my_nick = self.nickname.clone();
                                        }
                                    }
                                }
                            });
                        }
                    });

                ui.separator();

                // ===== Server Details =====
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
                ui.heading("SASL Authentication");
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    ui.label("SASL User:");
                    ui.add(TextEdit::singleline(&mut self.sasl_username).desired_width(150.0));
                });

                ui.horizontal(|ui| {
                    ui.label("SASL Pass:");
                    ui.add(TextEdit::singleline(&mut self.sasl_password).password(true).desired_width(150.0));
                });

                if !self.sasl_username.is_empty() || !self.sasl_password.is_empty() {
                    ui.label(RichText::new("For Libera Chat, OFTC, etc. Uses SASL PLAIN.").small().color(Color32::GRAY));
                }

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

                // ===== Save as Favorite =====
                if self.show_save_favorite_dialog {
                    ui.horizontal(|ui| {
                        ui.label("Name:");
                        ui.add(TextEdit::singleline(&mut self.new_favorite_name).desired_width(150.0));
                        if ui.button("Save").clicked() && !self.new_favorite_name.is_empty() {
                            let new_fav = ServerFavorite {
                                name: self.new_favorite_name.clone(),
                                host: self.server_host.clone(),
                                port: self.server_port.clone(),
                                use_tls: self.use_tls,
                                password: self.password.clone(),
                                nickname: self.nickname.clone(),
                                auto_join: self.auto_join_channels.clone(),
                                auto_perform: self.auto_perform.clone(),
                                sasl_username: self.sasl_username.clone(),
                                sasl_password: self.sasl_password.clone(),
                            };
                            self.server_favorites.push(new_fav);
                            self.selected_favorite = Some(self.server_favorites.len() - 1);
                            self.save_settings();
                            self.new_favorite_name.clear();
                            self.show_save_favorite_dialog = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.new_favorite_name.clear();
                            self.show_save_favorite_dialog = false;
                        }
                    });
                } else {
                    if ui.button("Save as Favorite").clicked() {
                        // Pre-fill with server name if empty
                        if self.new_favorite_name.is_empty() {
                            self.new_favorite_name = self.server_host.clone();
                        }
                        self.show_save_favorite_dialog = true;
                    }
                }

                ui.separator();

                // ===== Connect/Cancel Buttons =====
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

    pub fn show_settings_window(&mut self, ctx: &egui::Context) {
        egui::Window::new("Settings")
            .collapsible(false)
            .resizable(true)
            .default_size([380.0, 320.0])
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                // Tab bar
                ui.horizontal(|ui| {
                    if ui.selectable_label(self.settings_tab == 0, "Connection").clicked() {
                        self.settings_tab = 0;
                    }
                    if ui.selectable_label(self.settings_tab == 1, "Display").clicked() {
                        self.settings_tab = 1;
                    }
                    if ui.selectable_label(self.settings_tab == 2, "Automation").clicked() {
                        self.settings_tab = 2;
                    }
                    if ui.selectable_label(self.settings_tab == 3, "Behavior").clicked() {
                        self.settings_tab = 3;
                    }
                    if ui.selectable_label(self.settings_tab == 4, "About").clicked() {
                        self.settings_tab = 4;
                    }
                });
                ui.separator();

                // Tab content
                match self.settings_tab {
                    0 => self.settings_tab_connection(ui),
                    1 => self.settings_tab_display(ui),
                    2 => self.settings_tab_automation(ui),
                    3 => self.settings_tab_behavior(ui),
                    4 => self.settings_tab_about(ui),
                    _ => {}
                }

                ui.separator();
                if ui.button("Close").clicked() {
                    self.save_settings();
                    self.show_settings = false;
                }
            });
    }

    fn settings_tab_connection(&mut self, ui: &mut egui::Ui) {
        ui.heading("Server Defaults");
        ui.add_space(4.0);

        egui::Grid::new("connection_grid")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Server:");
                ui.add(TextEdit::singleline(&mut self.server_host).desired_width(200.0));
                ui.end_row();

                ui.label("Port:");
                ui.add(TextEdit::singleline(&mut self.server_port).desired_width(80.0));
                ui.end_row();
            });

        ui.checkbox(&mut self.use_tls, "Use TLS by default");

        ui.add_space(8.0);
        ui.heading("Identity");
        ui.add_space(4.0);

        egui::Grid::new("identity_grid")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Nickname:");
                ui.add(TextEdit::singleline(&mut self.nickname).desired_width(150.0));
                ui.end_row();

                ui.label("Username:");
                ui.add(TextEdit::singleline(&mut self.username).desired_width(150.0));
                ui.end_row();

                ui.label("Real name:");
                ui.add(TextEdit::singleline(&mut self.realname).desired_width(200.0));
                ui.end_row();
            });
    }

    fn settings_tab_display(&mut self, ui: &mut egui::Ui) {
        ui.heading("Timestamps");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label("Format:");
            egui::ComboBox::from_id_salt("timestamp_format")
                .selected_text(match self.timestamp_format.as_str() {
                    "short" => "HH:MM",
                    "long" => "HH:MM:SS",
                    "full" => "MM-DD HH:MM",
                    _ => "HH:MM",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.timestamp_format, "short".to_string(), "HH:MM");
                    ui.selectable_value(&mut self.timestamp_format, "long".to_string(), "HH:MM:SS");
                    ui.selectable_value(&mut self.timestamp_format, "full".to_string(), "MM-DD HH:MM");
                });
        });

        ui.add_space(8.0);
        ui.heading("Messages");
        ui.add_space(4.0);

        ui.checkbox(&mut self.hide_join_part, "Hide join/part/quit messages");

        ui.horizontal(|ui| {
            ui.label("Max scrollback:");
            let mut lines = self.max_scrollback as i32;
            ui.add(egui::DragValue::new(&mut lines).speed(100).range(100..=10000));
            self.max_scrollback = lines.max(100) as usize;
            ui.label("lines");
        });

        ui.add_space(8.0);
        ui.heading("Font");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label("Size:");
            ui.add(egui::DragValue::new(&mut self.font_size).speed(0.5).range(10.0..=24.0));
            ui.label("px");
        });
        ui.label(RichText::new("Restart required for font changes").small().color(Color32::GRAY));

        ui.add_space(8.0);
        ui.heading("Highlights");
        ui.add_space(4.0);

        ui.label("Extra highlight words (comma-separated):");
        ui.add(
            TextEdit::singleline(&mut self.highlight_words)
                .desired_width(300.0)
                .hint_text("urgent, alert, your-other-nick")
        );
        ui.label(RichText::new("Your nick is always highlighted").small().color(Color32::GRAY));
    }

    fn settings_tab_automation(&mut self, ui: &mut egui::Ui) {
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

        ui.checkbox(&mut self.set_invisible, "Set invisible (+i)");

        ui.add_space(8.0);
        ui.label("Auto-perform (one command per line):");
        ui.add(
            TextEdit::multiline(&mut self.auto_perform)
                .desired_width(340.0)
                .desired_rows(4)
                .hint_text("/msg NickServ identify pass\n/join #secret key")
        );

        ui.add_space(8.0);
        ui.heading("Ignore List");
        ui.add_space(4.0);

        if self.ignore_list.is_empty() {
            ui.label(RichText::new("No users ignored").italics().color(Color32::GRAY));
            ui.label("Use /ignore <nick|mask> to add.");
        } else {
            let ignore_display = self.ignore_list.join(", ");
            ui.horizontal_wrapped(|ui| {
                ui.label("Ignored:");
                ui.label(RichText::new(&ignore_display).color(Color32::GRAY));
            });
            ui.label("Use /unignore <nick> to remove.");
        }
    }

    fn settings_tab_behavior(&mut self, ui: &mut egui::Ui) {
        ui.heading("Connection");
        ui.add_space(4.0);

        ui.checkbox(&mut self.auto_reconnect, "Auto-reconnect on disconnect");
        if self.auto_reconnect {
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.label("Delay:");
                let mut secs = self.reconnect_delay_secs as i32;
                ui.add(egui::DragValue::new(&mut secs).speed(1).range(1..=60));
                self.reconnect_delay_secs = secs.max(1) as u32;
                ui.label("sec");
            });
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.label("Max attempts:");
                let mut attempts = self.max_reconnect_attempts as i32;
                ui.add(egui::DragValue::new(&mut attempts).speed(1).range(1..=100));
                self.max_reconnect_attempts = attempts.max(1) as u32;
            });
        }

        ui.checkbox(&mut self.notifications_enabled, "Desktop notifications for highlights/PMs");

        let tray_response = ui.checkbox(&mut self.minimize_to_tray, "Minimize to system tray");
        if self.minimize_to_tray {
            tray_response.on_hover_text("System tray support requires platform-specific setup");
        }

        ui.add_space(8.0);
        ui.heading("Privacy");
        ui.add_space(4.0);

        ui.checkbox(&mut self.ctcp_replies_enabled, "Reply to CTCP requests (VERSION, TIME, etc.)");

        ui.add_space(8.0);
        ui.heading("Auto-Away");
        ui.add_space(4.0);

        ui.checkbox(&mut self.auto_away_enabled, "Auto-away when idle");
        if self.auto_away_enabled {
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.label("After");
                let mut minutes = self.auto_away_minutes as i32;
                ui.add(egui::DragValue::new(&mut minutes).speed(1).range(1..=120));
                self.auto_away_minutes = minutes.max(1) as u32;
                ui.label("minutes");
            });
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.label("Message:");
                ui.add(TextEdit::singleline(&mut self.auto_away_message).desired_width(180.0));
            });
        }

        ui.add_space(8.0);
        ui.heading("Logging");
        ui.add_space(4.0);

        ui.checkbox(&mut self.logging_load_history, "Load chat history on join");
        if self.logging_load_history {
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.label("Load last");
                let mut lines = self.logging_history_lines as i32;
                ui.add(egui::DragValue::new(&mut lines).speed(10).range(10..=10000));
                self.logging_history_lines = lines.max(10) as usize;
                ui.label("lines");
            });
        }
        ui.label(RichText::new("Logs: ~/.config/fmirc/logs/").small().color(Color32::GRAY));
    }

    fn settings_tab_about(&mut self, ui: &mut egui::Ui) {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.heading("fmIRC");
            ui.label("Version 0.0.1");
            ui.add_space(10.0);
            ui.label("A cross-platform IRC client");
            ui.label(RichText::new("Built with Rust + egui").small().color(Color32::GRAY));
            ui.add_space(20.0);
            ui.label(RichText::new("Windows ARM64 / x86 / Linux").small());
        });
    }

    pub fn show_channel_list_window(&mut self, ctx: &egui::Context) {
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

                // Scrollable channel list with proper column layout
                let current_selected = self.channel_list_selected.clone();
                let mut new_selected = current_selected.clone();

                ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .max_height(350.0)
                    .show(ui, |ui| {
                        let filter = self.channel_list_filter.to_lowercase();

                        // Use Grid for proper column alignment
                        egui::Grid::new("channel_list_grid")
                            .num_columns(3)
                            .min_col_width(0.0)
                            .spacing([8.0, 2.0])
                            .striped(true)
                            .show(ui, |ui| {
                                for entry in &self.channel_list {
                                    // Filter by channel name or topic
                                    if !filter.is_empty()
                                        && !entry.name.to_lowercase().contains(&filter)
                                        && !entry.topic.to_lowercase().contains(&filter)
                                    {
                                        continue;
                                    }

                                    let is_selected = current_selected.as_ref() == Some(&entry.name);
                                    let row_color = if is_selected {
                                        Color32::WHITE
                                    } else {
                                        Color32::from_rgb(180, 210, 255)
                                    };

                                    // Column 1: Channel name (clickable)
                                    let response = ui.add_sized(
                                        [channel_width, 18.0],
                                        egui::Button::new(RichText::new(&entry.name).color(row_color))
                                            .selected(is_selected)
                                            .frame(false)
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
                                            ui.add(egui::Label::new(
                                                RichText::new(&channel_topic).small().color(Color32::GRAY)
                                            ).wrap_mode(egui::TextWrapMode::Wrap));
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

                                    // Column 2: User count (right-aligned)
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        ui.set_min_width(users_width);
                                        ui.label(RichText::new(format!("{}", entry.user_count)).color(Color32::LIGHT_GREEN));
                                    });

                                    // Column 3: Topic (truncated)
                                    let topic_display = if entry.topic.len() > 70 {
                                        format!("{}...", entry.topic.chars().take(70).collect::<String>())
                                    } else {
                                        entry.topic.clone()
                                    };
                                    ui.add_sized(
                                        [topic_width, 18.0],
                                        egui::Label::new(RichText::new(&topic_display).color(Color32::GRAY))
                                    );

                                    ui.end_row();
                                }
                            });
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
            self.send_command(IrcCommand::Join(channel, None));
            self.show_channel_list = false;
            self.channel_list_selected = None;
        }
    }

    pub fn show_channel_info_window(&mut self, ctx: &egui::Context) {
        let channel_name = match &self.channel_info_target {
            Some(name) => name.clone(),
            None => return,
        };

        let channel_data = self.channels.get(&channel_name).cloned();

        let mut close = false;

        egui::Window::new(format!("Channel Info: {}", channel_name))
            .collapsible(false)
            .resizable(true)
            .default_size([500.0, 400.0])
            .show(ctx, |ui| {
                if let Some(ch) = channel_data {
                    // Channel name and modes
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&channel_name).strong().size(16.0));
                        if !ch.modes.is_empty() {
                            ui.label(RichText::new(format!("({})", ch.modes)).color(Color32::GRAY));
                        }
                    });
                    ui.separator();

                    egui::Grid::new("channel_info_grid")
                        .num_columns(2)
                        .spacing([10.0, 6.0])
                        .show(ui, |ui| {
                            // User count
                            ui.label("Users:");
                            ui.label(format!("{}", ch.users.len()));
                            ui.end_row();

                            // Created
                            ui.label("Created:");
                            if let Some(ts) = ch.created {
                                ui.label(format_timestamp(ts));
                            } else {
                                ui.label("Unknown");
                            }
                            ui.end_row();

                            // Modes with params
                            if !ch.modes.is_empty() {
                                ui.label("Modes:");
                                let mode_str = if ch.mode_params.is_empty() {
                                    ch.modes.clone()
                                } else {
                                    format!("{} {}", ch.modes, ch.mode_params.join(" "))
                                };
                                ui.label(mode_str);
                                ui.end_row();
                            }
                        });

                    ui.separator();

                    // Topic section
                    ui.label(RichText::new("Topic").strong());
                    if let Some(topic) = &ch.topic {
                        ui.add(egui::Label::new(topic).wrap());
                        if let Some(setter) = &ch.topic_set_by {
                            let time_str = ch.topic_set_time
                                .map(|ts| format_timestamp(ts))
                                .unwrap_or_else(|| "Unknown".to_string());
                            ui.label(RichText::new(format!("Set by {} on {}", setter, time_str))
                                .small()
                                .color(Color32::GRAY));
                        }
                    } else {
                        ui.label(RichText::new("No topic set").italics().color(Color32::GRAY));
                    }

                    ui.separator();

                    // Ban list section
                    ui.collapsing(format!("Ban List ({})", ch.bans.len()), |ui| {
                        if ch.bans.is_empty() {
                            if ch.ban_list_complete {
                                ui.label(RichText::new("No bans").italics().color(Color32::GRAY));
                            } else {
                                if ui.button("Load Ban List").clicked() {
                                    self.send_command(IrcCommand::Mode(
                                        channel_name.clone(),
                                        Some("+b".to_string()),
                                        None,
                                    ));
                                }
                            }
                        } else {
                            ScrollArea::vertical()
                                .max_height(150.0)
                                .show(ui, |ui| {
                                    for ban in &ch.bans {
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(&ban.mask).monospace());
                                            ui.label(RichText::new(format!("by {} on {}",
                                                ban.set_by, format_timestamp(ban.set_time)))
                                                .small()
                                                .color(Color32::GRAY));
                                        });
                                    }
                                });
                        }
                    });

                    ui.separator();

                    // User list section
                    ui.collapsing(format!("Users ({})", ch.users.len()), |ui| {
                        ScrollArea::vertical()
                            .max_height(150.0)
                            .show(ui, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    for (nick, mode) in &ch.users {
                                        let prefix = mode.prefix();
                                        let display = format!("{}{}", prefix, nick);
                                        ui.label(&display);
                                    }
                                });
                            });
                    });
                } else {
                    ui.label("Channel not found");
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Refresh").clicked() {
                        // Request fresh channel info
                        self.send_command(IrcCommand::Mode(channel_name.clone(), None, None));
                        // Clear and re-request ban list
                        if let Some(ch) = self.channels.get_mut(&channel_name) {
                            ch.bans.clear();
                            ch.ban_list_complete = false;
                        }
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });

        if close {
            self.show_channel_info = false;
            self.channel_info_target = None;
        }
    }
}
