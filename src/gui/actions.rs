//! Named actions and portable, exact keyboard bindings.

use super::windows::WindowKey;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum ActionId {
    CycleWindow { reverse: bool },
    CloseWindow,
    ToggleMaximized,
    SelectOrdinal { index: usize },
    WindowSwitcher,
    SearchCurrent,
    SearchAll,
    NextUnread,
    FormatBold,
    FormatUnderline,
    FormatItalic,
    FormatColor,
    FormatReset,
    RunAlias { id: String },
}

impl ActionId {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::CycleWindow { reverse: false } => "Next window".into(),
            Self::CycleWindow { reverse: true } => "Previous window".into(),
            Self::CloseWindow => "Close window".into(),
            Self::ToggleMaximized => "Maximize / restore".into(),
            Self::SelectOrdinal { index } => format!("Window {}", index + 1),
            Self::WindowSwitcher => "Window switcher".into(),
            Self::SearchCurrent => "Search this conversation".into(),
            Self::SearchAll => "Search all saved logs".into(),
            Self::NextUnread => "Next unread conversation".into(),
            Self::FormatBold => "Bold".into(),
            Self::FormatUnderline => "Underline".into(),
            Self::FormatItalic => "Italic".into(),
            Self::FormatColor => "Color picker".into(),
            Self::FormatReset => "Reset formatting".into(),
            Self::RunAlias { id } => format!("Alias: {id}"),
        }
    }
    pub(crate) fn composer_only(&self) -> bool {
        matches!(
            self,
            Self::FormatBold
                | Self::FormatUnderline
                | Self::FormatItalic
                | Self::FormatColor
                | Self::FormatReset
        )
    }
    pub(crate) fn repeatable(&self) -> bool {
        matches!(
            self,
            Self::CycleWindow { .. } | Self::SelectOrdinal { .. } | Self::NextUnread
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ActionContext {
    pub window: WindowKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeyChord {
    pub key: String,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub mac_cmd: bool,
}
impl KeyChord {
    pub(crate) fn new(key: &str, ctrl: bool, shift: bool, alt: bool) -> Self {
        Self {
            key: key.into(),
            ctrl,
            shift,
            alt,
            mac_cmd: false,
        }
    }
    pub(crate) fn from_event(key: egui::Key, modifiers: egui::Modifiers) -> Self {
        Self {
            key: key.name().into(),
            ctrl: modifiers.ctrl,
            shift: modifiers.shift,
            alt: modifiers.alt,
            mac_cmd: modifiers.mac_cmd,
        }
    }
    pub(crate) fn valid(&self) -> bool {
        egui::Key::from_name(&self.key).is_some()
            && (self.ctrl
                || self.alt
                || self.mac_cmd
                || self
                    .key
                    .strip_prefix('F')
                    .and_then(|number| number.parse::<u8>().ok())
                    .is_some_and(|number| (1..=35).contains(&number)))
            && !(self.key == "Escape" || self.key == "Enter")
    }
    pub(crate) fn matches(&self, key: egui::Key, modifiers: egui::Modifiers) -> bool {
        self == &Self::from_event(key, modifiers)
    }
    pub(crate) fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.mac_cmd {
            parts.push("Cmd");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        parts.push(&self.key);
        parts.join("+")
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ShortcutBinding {
    pub chord: KeyChord,
    pub action: ActionId,
}

pub(crate) fn default_bindings() -> Vec<ShortcutBinding> {
    use ActionId::*;
    let mut bindings = vec![
        ("Tab", true, false, false, CycleWindow { reverse: false }),
        ("Tab", true, true, false, CycleWindow { reverse: true }),
        ("W", true, false, false, CloseWindow),
        ("M", true, false, false, ToggleMaximized),
        ("P", true, false, false, WindowSwitcher),
        ("F", true, false, false, SearchCurrent),
        ("F", true, true, false, SearchAll),
        ("A", false, false, true, NextUnread),
        ("B", true, false, false, FormatBold),
        ("U", true, false, false, FormatUnderline),
        ("I", true, false, false, FormatItalic),
        ("K", true, false, false, FormatColor),
        ("O", true, false, false, FormatReset),
    ]
    .into_iter()
    .map(|(key, ctrl, shift, alt, action)| ShortcutBinding {
        chord: KeyChord::new(key, ctrl, shift, alt),
        action,
    })
    .collect::<Vec<_>>();
    for index in 0..9 {
        bindings.push(ShortcutBinding {
            chord: KeyChord::new(&(index + 1).to_string(), false, false, true),
            action: SelectOrdinal { index },
        });
    }
    bindings
}

#[derive(Default)]
pub(crate) struct WindowSwitcher {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    pub focus: bool,
    pub owner: Option<WindowKey>,
    pub recent: Vec<WindowKey>,
}

pub(crate) fn match_score(query: &str, text: &str) -> Option<usize> {
    let query = query.to_lowercase();
    let text = text.to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    if text == query {
        return Some(0);
    }
    if text.starts_with(&query) {
        return Some(1);
    }
    if let Some(position) = text.find(&query) {
        return Some(10 + position);
    }
    let mut chars = query.chars();
    let mut wanted = chars.next();
    let mut gaps = 0;
    for character in text.chars() {
        if wanted == Some(character) {
            wanted = chars.next();
        } else {
            gaps += 1;
        }
        if wanted.is_none() {
            return Some(100 + gaps);
        }
    }
    None
}

impl super::App {
    pub(crate) fn dispatch_action(
        &mut self,
        ctx: &egui::Context,
        action: ActionId,
        context: ActionContext,
    ) {
        let key = context.window;
        let Some(index) = self.session_index(key.conn) else {
            return;
        };
        if key
            .conversation
            .is_some_and(|id| self.sessions[index].target_name(id).is_none())
        {
            return;
        }
        match action {
            ActionId::CycleWindow { reverse } => {
                self.cycle_window(reverse);
                self.focus_active_viewport(ctx);
            }
            ActionId::CloseWindow => self.request_close(key),
            ActionId::ToggleMaximized => self.toggle_maximized(),
            ActionId::SelectOrdinal { index } => {
                if let Some(key) = self.ordered_windows().get(index).copied() {
                    self.user_activate(key);
                    self.focus_active_viewport(ctx);
                }
            }
            ActionId::WindowSwitcher => {
                self.window_switcher.open = true;
                self.window_switcher.query.clear();
                self.window_switcher.selected = 0;
                self.window_switcher.focus = true;
                self.window_switcher.owner = Some(key);
                ctx.send_viewport_cmd_to(
                    egui::ViewportId::ROOT,
                    egui::ViewportCommand::Minimized(false),
                );
                ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
            }
            ActionId::SearchCurrent | ActionId::SearchAll => {
                let before = self.sessions[index].current_conversation;
                self.sessions[index].current_conversation = key.conversation;
                self.open_search(index, String::new(), action == ActionId::SearchAll);
                self.sessions[index].current_conversation = before;
            }
            ActionId::NextUnread => {
                self.cycle_unread();
                self.focus_active_viewport(ctx);
            }
            ActionId::RunAlias { id } => {
                if let Some(window) = self.windows.get_mut(&key) {
                    super::app::with_window_view(
                        &mut self.sessions[index],
                        &mut window.view,
                        &key.conversation,
                        |session| session.run_alias_id(&id),
                    );
                }
            }
            formatting => self.apply_format_action(key, formatting),
        }
    }
    pub(crate) fn draw_automation_editor(&mut self, ui: &mut egui::Ui, index: usize) {
        let scope = self.sessions[index]
            .logical_network_key()
            .map(|key| key.storage_key())
            .unwrap_or_default();
        if self.automation.draw(ui, &scope) {
            for session in &mut self.sessions {
                session.set_automation(self.automation.config.clone());
            }
        }
    }
    pub(crate) fn show_window_switcher(&mut self, ctx: &egui::Context) {
        if !self.window_switcher.open {
            return;
        }
        let mut windows = Vec::new();
        for (ordinal, key) in self.ordered_windows().into_iter().enumerate() {
            let Some(index) = self.session_index(key.conn) else {
                continue;
            };
            let session = &self.sessions[index];
            let target = key
                .conversation
                .and_then(|id| session.target_name(id))
                .unwrap_or("Status");
            let unread = key
                .conversation
                .and_then(|id| session.conversation_channel(id))
                .map_or(session.server_unread, |channel| channel.unread);
            let label = format!(
                "{} — {}{}",
                target,
                self.network_label(session),
                if unread > 0 {
                    format!("  [{unread} unread]")
                } else {
                    String::new()
                }
            );
            let recent = self
                .window_switcher
                .recent
                .iter()
                .position(|old| *old == key)
                .unwrap_or(usize::MAX);
            windows.push((recent, ordinal, key, label));
        }
        let mut chosen = None;
        let mut cancelled = false;
        egui::Window::new("Switch window")
            .id(egui::Id::unique("window_switcher"))
            .order(egui::Order::Foreground)
            .collapsible(false)
            .resizable(true)
            .default_size([500.0, 320.0])
            .show(ctx, |ui| {
                let interactive = !ui.is_sizing_pass();
                let input_id = egui::Id::unique("window_switcher_input");
                if interactive && std::mem::take(&mut self.window_switcher.focus) {
                    ui.memory_mut(|memory| memory.request_focus(input_id));
                }
                let response = ui.add_enabled(
                    interactive,
                    egui::TextEdit::singleline(&mut self.window_switcher.query)
                        .id(input_id)
                        .hint_text("Search conversations and networks…")
                        .desired_width(ui.available_width()),
                );
                if response.changed() {
                    self.window_switcher.selected = 0;
                    ctx.request_repaint();
                }
                let mut candidates = windows
                    .iter()
                    .filter_map(|(recent, ordinal, key, label)| {
                        match_score(&self.window_switcher.query, label)
                            .map(|score| (score, *recent, *ordinal, *key, label.clone()))
                    })
                    .collect::<Vec<_>>();
                candidates.sort_by_key(|entry| (entry.0, entry.1, entry.2));
                self.window_switcher.selected = self
                    .window_switcher
                    .selected
                    .min(candidates.len().saturating_sub(1));
                if interactive {
                    ui.input_mut(|input| {
                        if input.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                            cancelled = true;
                        }
                        if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)
                            && !candidates.is_empty()
                        {
                            self.window_switcher.selected =
                                (self.window_switcher.selected + 1) % candidates.len();
                        }
                        if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)
                            && !candidates.is_empty()
                        {
                            self.window_switcher.selected =
                                (self.window_switcher.selected + candidates.len() - 1)
                                    % candidates.len();
                        }
                        if input.consume_key(egui::Modifiers::NONE, egui::Key::Enter) {
                            chosen = candidates
                                .get(self.window_switcher.selected)
                                .map(|entry| entry.3);
                        }
                    });
                }
                egui::ScrollArea::vertical()
                    .max_height(250.0)
                    .show(ui, |ui| {
                        for (index, (_, _, _, key, label)) in candidates.iter().enumerate() {
                            let response =
                                ui.selectable_label(index == self.window_switcher.selected, label);
                            if interactive && response.clicked() {
                                chosen = Some(*key);
                            }
                            if index == self.window_switcher.selected
                                && ctx.input(|input| {
                                    input.key_pressed(egui::Key::ArrowDown)
                                        || input.key_pressed(egui::Key::ArrowUp)
                                })
                            {
                                response.scroll_to_me(Some(egui::Align::Center));
                            }
                        }
                        if candidates.is_empty() {
                            ui.label("No matching windows");
                        }
                    });
                if interactive && ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
        if chosen.is_some() || cancelled {
            self.window_switcher.open = false;
            if let Some(key) = chosen.or(self.window_switcher.owner)
                && self.windows.get(&key).is_some()
            {
                self.user_activate(key);
                self.focus_active_viewport(ctx);
                if let Some(window) = self.windows.get_mut(&key) {
                    window.view.input_focus_request = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_preserve_color_and_use_exact_event_modifiers() {
        let bindings = default_bindings();
        assert!(
            bindings
                .iter()
                .any(|b| b.chord.matches(egui::Key::K, egui::Modifiers::CTRL)
                    && b.action == ActionId::FormatColor)
        );
        assert!(!bindings.iter().any(|b| {
            b.chord
                .matches(egui::Key::W, egui::Modifiers::CTRL | egui::Modifiers::ALT)
        }));
        assert!(bindings.iter().all(|b| b.chord.valid()));
    }
    #[test]
    fn window_matches_rank_prefix_before_subsequence() {
        assert!(
            match_score("rust", "rust on network").unwrap()
                < match_score("rust", "r_u_s_t on network").unwrap()
        );
        assert!(match_score("none", "#rust").is_none());
    }
}
