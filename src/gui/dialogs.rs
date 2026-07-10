//! Dialog windows for connect, settings, and channel list

use super::helpers::format_timestamp;
use super::{ChatMessage, IrcApp, ServerFavorite};
use crate::irc::IrcCommand;
use egui::{Color32, RichText, ScrollArea, TextEdit, Vec2};

/// Popular IRC network presets (alphabetical)
const NETWORK_PRESETS: &[(&str, &str, &str, bool)] = &[
    // (Name, Host, Port, TLS)
    ("AfterNET", "irc.afternet.org", "6697", true),
    ("DALnet", "irc.dal.net", "6697", true),
    ("EFnet", "irc.efnet.org", "6697", true),
    ("Esper.net", "irc.esper.net", "6697", true),
    ("GeekShed", "irc.geekshed.net", "6697", true),
    ("hackint", "irc.hackint.org", "6697", true),
    ("IRCnet", "irc.ircnet.com", "6667", false),
    ("Libera Chat", "irc.libera.chat", "6697", true),
    ("OFTC", "irc.oftc.net", "6697", true),
    ("QuakeNet", "irc.quakenet.org", "6667", false),
    ("Rizon", "irc.rizon.net", "6697", true),
    ("Snoonet", "irc.snoonet.org", "6697", true),
    ("SwiftIRC", "irc.swiftirc.net", "6697", true),
    ("Undernet", "irc.undernet.org", "6697", true),
];

impl IrcApp {
    pub fn show_connect_window(&mut self, ctx: &egui::Context) {
        egui::Window::new("Connect to Server")
            .collapsible(false)
            .resizable(true)
            .default_width(450.0)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                // ===== Network Presets Section =====
                egui::CollapsingHeader::new("Quick Connect (Network Presets)")
                    .default_open(self.server_favorites.is_empty())
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            for (name, host, port, tls) in NETWORK_PRESETS {
                                if ui.small_button(*name).clicked() {
                                    self.apply_unprofiled_endpoint(host, port, *tls);
                                }
                            }
                        });
                        ui.add_space(4.0);
                        ui.label(RichText::new("Click a network to fill in server details, then click Connect below.").small().color(Color32::GRAY));
                    });

                ui.add_space(4.0);

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
                                    let mut connect_idx = None;
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
                                            connect_idx = Some(idx);
                                        }
                                    }
                                    if let Some(idx) = clicked_idx {
                                        self.selected_favorite = Some(idx);
                                    }
                                    if let Some(idx) = connect_idx
                                        && let Some(fav) = self.server_favorites.get(idx).cloned() {
                                            self.load_favorite(&fav);
                                            if self.start_session_from_form() {
                                                self.save_settings();
                                                self.show_connect_dialog = false;
                                            }
                                        }
                                });

                            ui.horizontal(|ui| {
                                // Load button
                                let has_selection = self.selected_favorite.is_some();
                                if ui.add_enabled(has_selection, egui::Button::new("Load")).clicked()
                                    && let Some(idx) = self.selected_favorite
                                        && let Some(fav) = self.server_favorites.get(idx).cloned() {
                                            self.load_favorite(&fav);
                                        }

                                // Delete button
                                if ui.add_enabled(has_selection, egui::Button::new("Delete")).clicked()
                                    && let Some(idx) = self.selected_favorite {
                                        self.server_favorites.remove(idx);
                                        self.selected_favorite = None;
                                        self.save_settings();
                                    }

                                // Connect button
                                if ui.add_enabled(has_selection, egui::Button::new("Connect")).clicked()
                                    && let Some(idx) = self.selected_favorite
                                                && let Some(fav) = self.server_favorites.get(idx).cloned() {
                                                    self.load_favorite(&fav);
                                                    if self.start_session_from_form() {
                                                        self.save_settings();
                                                        self.show_connect_dialog = false;
                                                    }
                                                }
                            });
                        }
                    });

                ui.separator();

                // ===== Server Details =====
                let mut endpoint_edited = false;
                ui.horizontal(|ui| {
                    ui.label("Server:");
                    endpoint_edited |= ui
                        .add(TextEdit::singleline(&mut self.server_host).desired_width(200.0))
                        .changed();
                });

                ui.horizontal(|ui| {
                    ui.label("Port:");
                    endpoint_edited |= ui
                        .add(TextEdit::singleline(&mut self.server_port).desired_width(80.0))
                        .changed();
                    endpoint_edited |= ui.checkbox(&mut self.use_tls, "Use TLS").changed();
                });
                if endpoint_edited {
                    self.note_unprofiled_endpoint_edit();
                }
                if let Some(error) = self.connection_error() {
                    ui.label(RichText::new(error).color(Color32::RED));
                }

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
                        if ui.button("Save").clicked()
                            && !self.new_favorite_name.is_empty()
                            && self.validate_connection_form()
                        {
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
                                username: self.username.clone(),
                                realname: self.realname.clone(),
                                accept_invalid_certs: self.accept_invalid_certs,
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
                    if ui.button("Connect").clicked() && self.start_session_from_form() {
                        self.save_settings();
                        self.show_connect_dialog = false;
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
                    if ui
                        .selectable_label(self.settings_tab == 0, "Connection")
                        .clicked()
                    {
                        self.settings_tab = 0;
                    }
                    if ui
                        .selectable_label(self.settings_tab == 1, "Display")
                        .clicked()
                    {
                        self.settings_tab = 1;
                    }
                    if ui
                        .selectable_label(self.settings_tab == 2, "Automation")
                        .clicked()
                    {
                        self.settings_tab = 2;
                    }
                    if ui
                        .selectable_label(self.settings_tab == 3, "Behavior")
                        .clicked()
                    {
                        self.settings_tab = 3;
                    }
                    if ui
                        .selectable_label(self.settings_tab == 4, "About")
                        .clicked()
                    {
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

        let old_endpoint = (
            self.server_host.clone(),
            self.server_port.clone(),
            self.use_tls,
        );
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
        if old_endpoint
            != (
                self.server_host.clone(),
                self.server_port.clone(),
                self.use_tls,
            )
        {
            self.note_unprofiled_endpoint_edit();
        }
        if let Some(error) = self.connection_error() {
            ui.label(RichText::new(error).color(Color32::RED));
        }

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
                    ui.selectable_value(
                        &mut self.timestamp_format,
                        "full".to_string(),
                        "MM-DD HH:MM",
                    );
                });
        });
        ChatMessage::configure_default_timestamp_format(&self.timestamp_format);

        ui.add_space(8.0);
        ui.heading("Messages");
        ui.add_space(4.0);

        ui.checkbox(&mut self.hide_join_part, "Hide join/part/quit messages");

        ui.horizontal(|ui| {
            ui.label("Max scrollback:");
            let mut lines = self.max_scrollback as i32;
            ui.add(
                egui::DragValue::new(&mut lines)
                    .speed(100)
                    .range(100..=10000),
            );
            self.max_scrollback = lines.max(100) as usize;
            ui.label("lines");
        });

        ui.add_space(8.0);
        ui.heading("Font");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label("Size:");
            let changed = ui
                .add(
                    egui::DragValue::new(&mut self.font_size)
                        .speed(0.5)
                        .range(10.0..=24.0),
                )
                .changed();
            if changed {
                super::apply_font_size(ui.ctx(), self.font_size);
            }
            ui.label("px");
        });
        ui.label(
            RichText::new("Applied immediately and saved for restart")
                .small()
                .color(Color32::GRAY),
        );

        ui.add_space(8.0);
        ui.heading("Highlights");
        ui.add_space(4.0);

        ui.label("Extra highlight words (comma-separated):");
        ui.add(
            TextEdit::singleline(&mut self.highlight_words)
                .desired_width(300.0)
                .hint_text("urgent, alert, your-other-nick"),
        );
        ui.label(
            RichText::new("Your nick is always highlighted")
                .small()
                .color(Color32::GRAY),
        );
    }

    fn settings_tab_automation(&mut self, ui: &mut egui::Ui) {
        ui.heading("On Connect");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label("Auto-join:");
            ui.add(
                TextEdit::singleline(&mut self.auto_join_channels)
                    .desired_width(200.0)
                    .hint_text("#chan1, #chan2"),
            );
        });

        ui.checkbox(&mut self.set_invisible, "Set invisible (+i)");

        ui.add_space(8.0);
        ui.label("Auto-perform (one command per line):");
        ui.add(
            TextEdit::multiline(&mut self.auto_perform)
                .desired_width(340.0)
                .desired_rows(4)
                .hint_text("/msg NickServ identify pass\n/join #secret key"),
        );

        ui.add_space(8.0);
        ui.heading("Ignore List");
        ui.add_space(4.0);

        if self.ignore_list.is_empty() {
            ui.label(
                RichText::new("No users ignored")
                    .italics()
                    .color(Color32::GRAY),
            );
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

        ui.checkbox(
            &mut self.notifications_enabled,
            "Desktop notifications for highlights/PMs",
        );

        let tray_response = ui.checkbox(&mut self.minimize_to_tray, "Minimize to system tray");
        if self.minimize_to_tray {
            tray_response.on_hover_text("System tray support requires platform-specific setup");
        }

        ui.add_space(8.0);
        ui.heading("Privacy");
        ui.add_space(4.0);

        ui.checkbox(
            &mut self.ctcp_replies_enabled,
            "Reply to CTCP requests (VERSION, TIME, etc.)",
        );

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
        ui.label(
            RichText::new("Logs: ~/.config/linefeed/logs/")
                .small()
                .color(Color32::GRAY),
        );

        ui.add_space(8.0);
        ui.heading("Custom Messages");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label("Quit:");
            ui.add(
                TextEdit::singleline(&mut self.quit_message)
                    .desired_width(200.0)
                    .hint_text("Linefeed"),
            );
        });
        ui.horizontal(|ui| {
            ui.label("Part:");
            ui.add(
                TextEdit::singleline(&mut self.part_message)
                    .desired_width(200.0)
                    .hint_text("Leaving"),
            );
        });

        ui.add_space(8.0);
        ui.heading("Channel List");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label("Min users:");
            let mut min_users = self.list_min_users as i32;
            // Range must match the Channel List window's DragValue (0..=1000) so the
            // shared list_min_users value is never silently clamped when switching UIs.
            ui.add(
                egui::DragValue::new(&mut min_users)
                    .speed(1)
                    .range(0..=1000),
            );
            self.list_min_users = min_users.max(0) as u32;
        });
        ui.label(
            RichText::new("Filter /list to channels with at least this many users (0 = all)")
                .small()
                .color(Color32::GRAY),
        );
    }

    fn settings_tab_about(&mut self, ui: &mut egui::Ui) {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.heading("Linefeed");
            ui.label("Version 0.0.1");
            ui.add_space(10.0);
            ui.label("A cross-platform IRC client");
            ui.label(
                RichText::new("Built with Rust + egui")
                    .small()
                    .color(Color32::GRAY),
            );
            ui.add_space(20.0);
            ui.label(RichText::new("Windows ARM64 / x86 / Linux").small());
        });
    }

    /// Recompute the channel-list view (filtered + sorted row indices) only when
    /// the underlying list, the filter text, or the sort order actually changed.
    /// While the list is still streaming in from the server, refresh at most a
    /// few times per second instead of re-sorting tens of thousands of rows on
    /// every frame.
    fn ensure_channel_list_cache(&mut self) {
        use super::types::{ChannelListSort, SortDirection};

        let sort_key = (self.channel_list_sort, self.channel_list_sort_dir);
        let filter_changed = self.channel_list_filter != self.channel_list_cache_filter;
        let sort_changed = sort_key != self.channel_list_cache_sort;
        if !self.channel_list_dirty && !filter_changed && !sort_changed {
            return;
        }
        if self.channel_list_loading
            && !filter_changed
            && !sort_changed
            && let Some(last) = self.channel_list_last_refilter
            && last.elapsed() < std::time::Duration::from_millis(250)
        {
            return; // throttled; channel_list_dirty stays set for the next pass
        }
        self.channel_list_last_refilter = Some(std::time::Instant::now());
        self.channel_list_dirty = false;
        self.channel_list_cache_filter = self.channel_list_filter.clone();
        self.channel_list_cache_sort = sort_key;

        let filter = self.channel_list_filter.to_lowercase();
        let list = &self.channel_list;
        let mut rows: Vec<usize> = (0..list.len())
            .filter(|&i| {
                filter.is_empty()
                    || list[i].name_lower.contains(&filter)
                    || list[i].topic_lower.contains(&filter)
            })
            .collect();
        rows.sort_by(|&a, &b| {
            let (a, b) = (&list[a], &list[b]);
            let cmp = match self.channel_list_sort {
                ChannelListSort::Channel => a.name_lower.cmp(&b.name_lower),
                ChannelListSort::Users => a.user_count.cmp(&b.user_count),
                ChannelListSort::Topic => a.topic_lower.cmp(&b.topic_lower),
            };
            match self.channel_list_sort_dir {
                SortDirection::Ascending => cmp,
                SortDirection::Descending => cmp.reverse(),
            }
        });
        self.channel_list_cache = rows;
    }

    pub fn show_channel_list_window(&mut self, ctx: &egui::Context) {
        use super::types::{ChannelListSort, SortDirection};

        let mut close = false;
        let mut join_channel: Option<String> = None;

        self.ensure_channel_list_cache();

        egui::Window::new("Channel List")
            .resizable(false)
            .fixed_size([600.0, 400.0])
            .show(ctx, |ui| {
                // Filter input and status
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    ui.add(
                        TextEdit::singleline(&mut self.channel_list_filter).desired_width(150.0),
                    );
                    ui.add_space(8.0);
                    ui.label(format!(
                        "{}/{} channels",
                        self.channel_list_cache.len(),
                        self.channel_list.len()
                    ));
                    if self.channel_list.is_empty() && !self.channel_list_loading {
                        ui.label(
                            RichText::new("(No channels found - server may be restricting /list)")
                                .color(Color32::GOLD)
                                .small(),
                        );
                    }

                    if self.channel_list_loading {
                        ui.spinner();
                    }
                });
                ui.separator();

                // Fixed column widths
                let channel_width = 130.0;
                let users_width = 45.0;
                let topic_width = 380.0;

                // Compute sort indicators
                let current_sort = self.channel_list_sort;
                let current_dir = self.channel_list_sort_dir;
                let arrow = |col: ChannelListSort| -> &'static str {
                    if current_sort == col {
                        match current_dir {
                            SortDirection::Ascending => " ^",
                            SortDirection::Descending => " v",
                        }
                    } else {
                        ""
                    }
                };

                // Table header row
                ui.horizontal(|ui| {
                    // Channel header
                    let chan_text = format!("Channel{}", arrow(ChannelListSort::Channel));
                    if ui
                        .add_sized(
                            [channel_width, 16.0],
                            egui::Button::new(
                                RichText::new(chan_text).strong().color(Color32::WHITE),
                            ),
                        )
                        .clicked()
                    {
                        if self.channel_list_sort == ChannelListSort::Channel {
                            self.channel_list_sort_dir = match self.channel_list_sort_dir {
                                SortDirection::Ascending => SortDirection::Descending,
                                SortDirection::Descending => SortDirection::Ascending,
                            };
                        } else {
                            self.channel_list_sort = ChannelListSort::Channel;
                            self.channel_list_sort_dir = SortDirection::Ascending;
                        }
                    }

                    // Users header
                    let users_text = format!("#{}", arrow(ChannelListSort::Users));
                    if ui
                        .add_sized(
                            [users_width, 16.0],
                            egui::Button::new(
                                RichText::new(users_text).strong().color(Color32::WHITE),
                            ),
                        )
                        .clicked()
                    {
                        if self.channel_list_sort == ChannelListSort::Users {
                            self.channel_list_sort_dir = match self.channel_list_sort_dir {
                                SortDirection::Ascending => SortDirection::Descending,
                                SortDirection::Descending => SortDirection::Ascending,
                            };
                        } else {
                            self.channel_list_sort = ChannelListSort::Users;
                            self.channel_list_sort_dir = SortDirection::Descending;
                        }
                    }

                    // Topic header
                    let topic_text = format!("Topic{}", arrow(ChannelListSort::Topic));
                    if ui
                        .add_sized(
                            [topic_width, 16.0],
                            egui::Button::new(
                                RichText::new(topic_text).strong().color(Color32::WHITE),
                            ),
                        )
                        .clicked()
                    {
                        if self.channel_list_sort == ChannelListSort::Topic {
                            self.channel_list_sort_dir = match self.channel_list_sort_dir {
                                SortDirection::Ascending => SortDirection::Descending,
                                SortDirection::Descending => SortDirection::Ascending,
                            };
                        } else {
                            self.channel_list_sort = ChannelListSort::Topic;
                            self.channel_list_sort_dir = SortDirection::Ascending;
                        }
                    }
                });
                ui.separator();

                // Virtual scrolling - only render visible rows
                let current_selected = self.channel_list_selected.clone();
                let mut new_selected = current_selected.clone();
                let row_height = 16.0;
                let total_rows = self.channel_list_cache.len();

                ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .max_height(280.0)
                    .show_rows(ui, row_height, total_rows, |ui, row_range| {
                        for row_idx in row_range {
                            let Some(entry) = self
                                .channel_list_cache
                                .get(row_idx)
                                .and_then(|&i| self.channel_list.get(i))
                            else {
                                continue;
                            };
                            let is_selected = current_selected.as_ref() == Some(&entry.name);

                            // Alternate row background
                            let bg_color = if is_selected {
                                Color32::from_rgb(60, 80, 120)
                            } else if row_idx % 2 == 0 {
                                Color32::TRANSPARENT
                            } else {
                                Color32::from_rgba_unmultiplied(255, 255, 255, 8)
                            };
                            let text_color = if is_selected {
                                Color32::WHITE
                            } else {
                                Color32::from_rgb(180, 210, 255)
                            };

                            ui.horizontal(|ui| {
                                ui.set_height(row_height);

                                // Draw background
                                let row_rect = ui.max_rect();
                                ui.painter().rect_filled(row_rect, 0.0, bg_color);

                                // Channel name (clickable)
                                let response = ui.add_sized(
                                    [channel_width, row_height],
                                    egui::Button::new(RichText::new(&entry.name).color(text_color))
                                        .frame(false),
                                );

                                if response.clicked() {
                                    new_selected = Some(entry.name.clone());
                                }
                                if response.double_clicked() {
                                    join_channel = Some(entry.name.clone());
                                }

                                // Context menu
                                let channel_name = entry.name.clone();
                                let channel_topic = entry.topic_clean.clone();
                                let channel_users = entry.user_count;
                                response.context_menu(|ui| {
                                    ui.set_max_width(300.0);
                                    ui.label(RichText::new(&channel_name).strong());
                                    ui.label(format!("{} users", channel_users));
                                    if !channel_topic.is_empty() {
                                        ui.separator();
                                        ui.label(RichText::new("Topic:").small());
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(&channel_topic)
                                                    .small()
                                                    .color(Color32::GRAY),
                                            )
                                            .wrap_mode(egui::TextWrapMode::Wrap),
                                        );
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

                                // User count
                                ui.add_sized(
                                    [users_width, row_height],
                                    egui::Label::new(
                                        RichText::new(format!("{}", entry.user_count))
                                            .color(Color32::LIGHT_GREEN),
                                    ),
                                );

                                // Topic (clipped, with IRC formatting pre-stripped)
                                ui.add_sized(
                                    [topic_width, row_height],
                                    egui::Label::new(
                                        RichText::new(&entry.topic_clean).color(Color32::GRAY),
                                    )
                                    .truncate(),
                                );
                            });
                        }
                    });

                // Update selection
                self.channel_list_selected = new_selected;

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Join").clicked()
                        && let Some(channel) = &self.channel_list_selected
                    {
                        join_channel = Some(channel.clone());
                    }

                    ui.label("Min:");
                    let mut min_users = self.list_min_users as i32;
                    ui.add(
                        egui::DragValue::new(&mut min_users)
                            .range(0..=1000)
                            .speed(1),
                    );
                    self.list_min_users = min_users.max(0) as u32;

                    if ui.button("Refresh").clicked() {
                        // >N means "more than N users", so for min 5, send >4
                        let request = if self.list_min_users >= 1 {
                            IrcCommand::List(Some(format!(
                                ">{}",
                                self.list_min_users.saturating_sub(1)
                            )))
                        } else {
                            IrcCommand::List(None)
                        };
                        if self.send_command(request) {
                            self.channel_list.clear();
                            self.channel_list_dirty = true;
                            self.channel_list_selected = None;
                            self.channel_list_loading = true;
                        } else {
                            self.channel_list_loading = false;
                            self.add_server_message(super::types::ChatMessage::system_fmt(
                                "Not connected - channel list was not refreshed",
                                &self.timestamp_format,
                            ));
                        }
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });

        if close {
            self.show_channel_list = false;
            self.channel_list_selected = None;
            self.save_settings();
        }
        if let Some(channel) = join_channel {
            self.send_command(IrcCommand::Join(channel, None, None, None));
            self.show_channel_list = false;
            self.channel_list_selected = None;
            self.save_settings();
        }
    }

    pub fn show_channel_info_window(&mut self, ctx: &egui::Context) {
        let channel_name = match &self.channel_info_target {
            Some(name) => name.clone(),
            None => return,
        };

        let mut close = false;
        // Button actions are deferred to after the window closure so the channel
        // can be borrowed instead of deep-cloned (cloning the full scrollback,
        // user list, and bans every frame is several MB of allocation traffic).
        let mut load_ban_list = false;
        let mut refresh = false;

        egui::Window::new(format!("Channel Info: {}", channel_name))
            .collapsible(false)
            .resizable(true)
            .default_size([500.0, 400.0])
            .show(ctx, |ui| {
                if let Some(ch) = self.channels.get(&channel_name) {
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
                            let time_str = ch
                                .topic_set_time
                                .map(format_timestamp)
                                .unwrap_or_else(|| "Unknown".to_string());
                            ui.label(
                                RichText::new(format!("Set by {} on {}", setter, time_str))
                                    .small()
                                    .color(Color32::GRAY),
                            );
                        }
                    } else {
                        ui.label(RichText::new("No topic set").italics().color(Color32::GRAY));
                    }

                    ui.separator();

                    // Ban list section. The stable id_salt keeps the section's
                    // open/closed state when the count in the title changes
                    // (a title-derived id would collapse it on every new entry).
                    egui::CollapsingHeader::new(format!("Ban List ({})", ch.bans.len()))
                        .id_salt("channel_info_bans")
                        .show(ui, |ui| {
                            if ch.bans.is_empty() {
                                if ch.ban_list_complete {
                                    ui.label(
                                        RichText::new("No bans").italics().color(Color32::GRAY),
                                    );
                                } else {
                                    if ui.button("Load Ban List").clicked() {
                                        load_ban_list = true;
                                    }
                                }
                            } else {
                                ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                                    for ban in &ch.bans {
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(&ban.mask).monospace());
                                            ui.label(
                                                RichText::new(format!(
                                                    "by {} on {}",
                                                    ban.set_by,
                                                    format_timestamp(ban.set_time)
                                                ))
                                                .small()
                                                .color(Color32::GRAY),
                                            );
                                        });
                                    }
                                });
                            }
                        });

                    ui.separator();

                    // User list section (stable id_salt: see ban list above).
                    egui::CollapsingHeader::new(format!("Users ({})", ch.users.len()))
                        .id_salt("channel_info_users")
                        .show(ui, |ui| {
                            ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    for user in &ch.users {
                                        let prefix = user.mode.prefix();
                                        let display = if user.is_away() {
                                            format!("{}{} (away)", prefix, user.nick)
                                        } else {
                                            format!("{}{}", prefix, user.nick)
                                        };
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
                        refresh = true;
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });

        if load_ban_list {
            self.send_command(IrcCommand::Mode(
                channel_name.clone(),
                Some("+b".to_string()),
                Vec::new(),
            ));
        }
        if refresh {
            // Clear displayed data only when both refresh requests were queued;
            // otherwise a disconnected click preserves the last snapshot.
            let modes_sent =
                self.send_command(IrcCommand::Mode(channel_name.clone(), None, Vec::new()));
            let bans_sent = self.send_command(IrcCommand::Mode(
                channel_name.clone(),
                Some("+b".to_string()),
                Vec::new(),
            ));
            if modes_sent && bans_sent {
                if let Some(ch) = self.channels.get_mut(&channel_name) {
                    ch.bans.clear();
                    ch.ban_list_complete = false;
                }
            } else {
                self.add_server_message(super::types::ChatMessage::system_fmt(
                    "Not connected - channel information was not refreshed",
                    &self.timestamp_format,
                ));
            }
        }
        if close {
            self.show_channel_info = false;
            self.channel_info_target = None;
        }
    }

    /// Show the IRC color picker popup
    pub fn show_color_picker_window(&mut self, ctx: &egui::Context) -> Option<String> {
        let mut result: Option<String> = None;
        let mut close = false;

        // IRC color palette (mIRC standard colors)
        let colors: [(u8, &str, Color32); 16] = [
            (0, "White", Color32::WHITE),
            (1, "Black", Color32::BLACK),
            (2, "Blue", Color32::from_rgb(0, 0, 127)),
            (3, "Green", Color32::from_rgb(0, 147, 0)),
            (4, "Red", Color32::from_rgb(255, 0, 0)),
            (5, "Brown", Color32::from_rgb(127, 0, 0)),
            (6, "Purple", Color32::from_rgb(156, 0, 156)),
            (7, "Orange", Color32::from_rgb(252, 127, 0)),
            (8, "Yellow", Color32::from_rgb(255, 255, 0)),
            (9, "Lt Green", Color32::from_rgb(0, 252, 0)),
            (10, "Cyan", Color32::from_rgb(0, 147, 147)),
            (11, "Lt Cyan", Color32::from_rgb(0, 255, 255)),
            (12, "Lt Blue", Color32::from_rgb(0, 0, 252)),
            (13, "Pink", Color32::from_rgb(255, 0, 255)),
            (14, "Grey", Color32::from_rgb(127, 127, 127)),
            (15, "Lt Grey", Color32::from_rgb(210, 210, 210)),
        ];

        egui::Window::new("IRC Colors")
            .collapsible(false)
            .resizable(false)
            .default_width(220.0)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(if self.color_picker_fg {
                    "Select foreground color:"
                } else {
                    "Select background color:"
                });
                ui.add_space(4.0);

                // 4x4 color grid
                egui::Grid::new("color_grid")
                    .spacing([4.0, 4.0])
                    .show(ui, |ui| {
                        for (idx, (code, name, color)) in colors.iter().enumerate() {
                            // Use contrasting border for visibility
                            let border = if *code == 0 || *code == 15 {
                                Color32::DARK_GRAY
                            } else {
                                Color32::TRANSPARENT
                            };

                            let btn = egui::Button::new("")
                                .fill(*color)
                                .stroke(egui::Stroke::new(1.0, border))
                                .min_size(Vec2::new(40.0, 25.0));

                            if ui.add(btn).on_hover_text(*name).clicked() {
                                if self.color_picker_fg {
                                    // First click - foreground color, now ask for background
                                    result = Some(format!("\x03{:02}", code));
                                    self.color_picker_fg = false; // Switch to background selection
                                } else {
                                    // Second click - background color
                                    result = Some(format!(",{:02}", code));
                                    close = true;
                                }
                            }

                            // 4 colors per row
                            if (idx + 1) % 4 == 0 {
                                ui.end_row();
                            }
                        }
                    });

                ui.add_space(8.0);
                ui.separator();

                ui.horizontal(|ui| {
                    if !self.color_picker_fg {
                        // Already selected foreground, show option to skip background
                        if ui.button("No Background").clicked() {
                            close = true;
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });

                // Show formatting shortcuts help
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Shortcuts: Ctrl+B Bold, Ctrl+U Underline, Ctrl+I Italic")
                        .small()
                        .color(Color32::GRAY),
                );
            });

        if close {
            self.show_color_picker = false;
            self.color_picker_fg = true; // Reset for next time
        }

        result
    }
}
