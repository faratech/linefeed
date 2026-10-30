//! The multi-server application: every connection is a `Session`; `App`
//! owns them together with everything that is not about one connection —
//! persisted settings, the updater, notifications, the window manager, and
//! the dialogs that are global (Settings).
//!
//! Window model (mIRC style): every session has a status window plus one
//! window per channel and query. See `windows.rs` for placement; drawing and
//! routing live here. A window draws by temporarily pointing its session's
//! `current_channel` at the window's target and swapping the window's input
//! line into the session, so the session's command, history and completion
//! code serves every window without knowing about windows.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use egui::{Color32, RichText};

use super::snap::CaptionAction;
use super::windows::{
    ConnId, LayoutFile, LayoutMode, PersistKey, PresentWindow, TargetView, WindowKey, WindowManager,
};
use super::{
    ChatMessage, ServerFavorite, Session, SessionEvent, Settings, UpdateSession, WindowCommand,
};

/// How long after startup the window that was active at the last exit may
/// still take focus when it reappears (auto-joins take a moment).
const RESTORE_ACTIVE_WINDOW: Duration = Duration::from_secs(30);
/// How long placement changes settle before layout.json is rewritten.
const LAYOUT_SAVE_DELAY: Duration = Duration::from_secs(2);
/// Desktop notifications that are not forced are spaced at least this far
/// apart: each one spawns a thread (and a subprocess on Linux/macOS).
const MIN_NOTIFICATION_INTERVAL: Duration = Duration::from_secs(2);
/// Messages drained per session per frame before yielding to drawing.
const MAX_MESSAGES_PER_FRAME: usize = 100;
/// The main OS window's title. `systray.rs` finds the main window by this
/// exact title, so no other window may ever use it.
pub(crate) const MAIN_WINDOW_TITLE: &str = "Linefeed";

/// Connect-dialog state for the global favorites list.
#[derive(Default)]
pub(crate) struct FavoritesUi {
    pub selected: Option<usize>,
    pub new_name: String,
    pub show_save_dialog: bool,
}

pub struct App {
    pub(crate) sessions: Vec<Session>,
    next_conn: ConnId,
    /// Canonical persisted settings. Behaviour settings are mirrored into
    /// every session (`Session::apply_config`); the server fields are the
    /// Quick Connect defaults new sessions start from.
    pub(crate) settings: Settings,
    pub updater: crate::updater::Controller,
    pub(crate) windows: WindowManager,
    pub(crate) favorites_ui: FavoritesUi,
    pub(crate) show_settings: bool,
    pub(crate) settings_tab: usize,
    /// Input history shared by every window.
    command_history: Vec<String>,
    last_activity: Instant,
    /// Rate-limit clock for best-effort notifications (highlights, kicks,
    /// invites); a flood inside the interval is dropped.
    last_notification_time: Option<Instant>,
    /// Separate clock for forced (private message) notifications, so a
    /// highlight can never mute a PM (#323).
    last_forced_notification_time: Option<Instant>,
    /// A forced notification held back by its rate limit; later ones fold
    /// into it, and `tick` delivers it once the interval has passed (#323).
    deferred_notification: Option<DeferredNotification>,
    /// Notifications that reached the OS stage (tests only).
    #[cfg(test)]
    pub(crate) dispatched_notifications: Vec<(String, String)>,
    /// What `store_settings` wrote (tests only: never the real file).
    #[cfg(test)]
    pub(crate) settings_file: Option<Settings>,
    /// Whether one of our OS windows had keyboard focus last frame.
    focused: bool,
    /// The detached window whose OS window had focus last frame.
    focused_detached: Option<WindowKey>,
    pub had_messages_this_frame: bool,
    /// Status window whose close is waiting for the user's confirmation.
    confirm_disconnect: Option<ConnId>,
    /// Sessions told to disconnect because their status window closed; they
    /// are dropped once their worker has flushed QUIT and exited.
    closing_sessions: HashSet<ConnId>,
    /// The attached window shown when maximized while the active window is
    /// detached.
    main_window: Option<WindowKey>,
    /// Each session's `current_channel` as of last frame, to follow focus
    /// changes the session made itself (own JOIN, /query).
    last_current: HashMap<ConnId, Option<String>>,
    /// Geometry a detached viewport was created with; kept constant while it
    /// is open so the builder never fights the user moving it.
    spawn_geometry: HashMap<WindowKey, (Option<egui::Pos2>, Option<egui::Vec2>)>,
    /// The window whose formatting bar opened the colour picker.
    color_target: Option<WindowKey>,
    layout_dirty_since: Option<Instant>,
    /// Off in tests: layout.json lives in the real config directory.
    persist_layout: bool,
    /// False when settings.json existed at startup but could not be read:
    /// saving would then replace the real file (favorites, passwords) with
    /// defaults, so nothing is saved this run (#316).
    pub(crate) settings_writable: bool,
    /// Set by the frame loop while the main window is hidden to the tray;
    /// detached windows are hidden with it.
    pub tray_hidden: bool,
    /// Tile/Cascade chosen from the Window menu, applied with the chat area.
    tile_request: Option<TileRequest>,
    /// Restored unsent input, placed into the session's selected window once
    /// that window exists.
    pending_inputs: Vec<(ConnId, String)>,
    /// When the App started: the persisted active window is only restored
    /// while startup auto-joins are still arriving.
    started: Instant,
    /// The user has picked a window (click, switchbar, shortcut, typing a
    /// /query): the persisted active window no longer applies.
    user_activated: bool,
    /// The window to return to if a "New connection" status window is
    /// dismissed without connecting.
    focus_before_new: Option<WindowKey>,
    /// The session the open Settings window edits, fixed when it opened so
    /// switching windows meanwhile cannot move its edits to another network.
    pub(crate) settings_session: Option<ConnId>,
    /// The window icon, built once for every detached window.
    icon: Option<std::sync::Arc<egui::IconData>>,
    /// The window that was active before an update restart.
    restore_after_update: Option<WindowKey>,
    /// A window that should take input focus once it exists (activated
    /// before its first sync, e.g. a query opened by /query).
    pending_focus: Option<WindowKey>,
    /// The detached windows were hidden because the main window went to
    /// the tray; they are shown again with it.
    detached_hidden: bool,
    /// Snap layouts of the floating windows (caption drags, the layout
    /// flyout and bar, Snap Assist).
    pub(crate) snap: super::snap::SnapUi,
    /// The window a session's Channel List / Channel Info dialog was opened
    /// from. When that window is detached, the dialogs show in its OS window
    /// instead of the main one.
    dialog_owner: HashMap<ConnId, WindowKey>,
    /// The detached window the application's own dialogs (Connect,
    /// Settings, the disconnect confirmation) were asked for from; they
    /// show in it, so they are seen even while the main window is
    /// minimized. `None`: the main window.
    global_dialog_owner: Option<WindowKey>,
    /// Whether any of those dialogs was open last frame.
    global_dialogs_were_open: bool,
}

impl App {
    /// The production constructor: persisted settings and layout.
    pub fn load() -> Self {
        let (settings, writable) = match Settings::try_load() {
            Ok(Some(settings)) => (settings, true),
            Ok(None) => (Settings::default(), true),
            // Unparsable: `load` backs the file up and starts fresh, as
            // before, so saving can no longer lose anything.
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                (Settings::load(), true)
            }
            Err(error) => {
                tracing::error!("Settings could not be read, so none will be saved: {error}");
                (Settings::default(), false)
            }
        };
        let needs_ids = settings.server_favorites.iter().any(|f| f.id.is_empty());
        let mut app = Self::with_settings(settings, LayoutFile::load());
        app.persist_layout = true;
        app.settings_writable = writable;
        if needs_ids {
            // Profile ids must be stable before layout or auto-connect state
            // refers to them. Saving only adds the new field.
            app.store_settings();
        }
        app
    }

    /// An app with no session yet; call `start` (or `add_session`).
    pub(crate) fn with_settings(mut settings: Settings, layout: LayoutFile) -> Self {
        clamp_settings(&mut settings);
        if settings.nickname.is_empty() {
            settings.nickname = format!("Linefeed_{}", super::rand_suffix());
        }
        let mut ids = HashSet::new();
        for favorite in &mut settings.server_favorites {
            if favorite.id.is_empty() {
                favorite.id =
                    super::derived_profile_id(&favorite.name, &favorite.host, &favorite.port);
                if ids.contains(&favorite.id) {
                    // A duplicate favorite: ids must stay distinct.
                    favorite.id = super::new_profile_id();
                }
            }
            ids.insert(favorite.id.clone());
        }
        ChatMessage::configure_default_timestamp_format(&settings.timestamp_format);
        Self {
            sessions: Vec::new(),
            next_conn: 1,
            settings,
            updater: crate::updater::Controller::default(),
            windows: WindowManager::from_layout(layout),
            favorites_ui: FavoritesUi::default(),
            show_settings: false,
            settings_tab: 0,
            command_history: Vec::new(),
            last_activity: Instant::now(),
            last_notification_time: None,
            last_forced_notification_time: None,
            deferred_notification: None,
            #[cfg(test)]
            dispatched_notifications: Vec::new(),
            #[cfg(test)]
            settings_file: None,
            focused: true,
            focused_detached: None,
            had_messages_this_frame: false,
            confirm_disconnect: None,
            closing_sessions: HashSet::new(),
            main_window: None,
            last_current: HashMap::new(),
            spawn_geometry: HashMap::new(),
            color_target: None,
            layout_dirty_since: None,
            persist_layout: false,
            settings_writable: true,
            tray_hidden: false,
            tile_request: None,
            pending_inputs: Vec::new(),
            started: Instant::now(),
            user_activated: false,
            focus_before_new: None,
            settings_session: None,
            icon: None,
            restore_after_update: None,
            pending_focus: None,
            dialog_owner: HashMap::new(),
            global_dialog_owner: None,
            global_dialogs_were_open: false,
            detached_hidden: false,
            snap: Default::default(),
        }
    }

    /// Bring up the startup sessions: an update-restart snapshot when one
    /// exists, else every auto-connect profile, else one idle session with
    /// the Connect dialog, exactly like a single-server start.
    pub fn start(&mut self, snapshot: Option<UpdateSession>) {
        if let Some(snapshot) = snapshot {
            self.restore_update_session(snapshot);
        } else {
            let auto: Vec<ServerFavorite> = self
                .settings
                .server_favorites
                .iter()
                .filter(|f| f.auto_connect)
                .cloned()
                .collect();
            for favorite in auto {
                let index = self.add_session();
                let session = &mut self.sessions[index];
                session.load_favorite(&favorite);
                session.show_connect_dialog = false;
                if !session.start_session_from_form() {
                    session.show_connect_dialog = true;
                }
            }
        }
        if self.sessions.is_empty() {
            self.add_session();
        }
    }

    /// A new idle session started from the Quick Connect defaults.
    pub(crate) fn add_session(&mut self) -> usize {
        let mut session = Session::with_settings(self.settings.clone());
        session.id = self.next_conn;
        self.next_conn += 1;
        session.window_focused = self.focused;
        session.last_activity = self.last_activity;
        self.sessions.push(session);
        self.sessions.len() - 1
    }

    pub(crate) fn session_index(&self, conn: ConnId) -> Option<usize> {
        self.sessions.iter().position(|s| s.id == conn)
    }

    /// The session of the active window (the first session if none).
    pub(crate) fn active_index(&self) -> usize {
        self.windows
            .active
            .as_ref()
            .and_then(|key| self.session_index(key.conn))
            .unwrap_or(0)
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn minimize_to_tray(&self) -> bool {
        self.settings.minimize_to_tray
    }

    pub fn automatic_updates(&self) -> bool {
        self.settings.automatic_updates
    }

    pub fn font_size(&self) -> f32 {
        self.settings.font_size
    }

    /// Any session connected, connecting or waiting out a reconnect backoff:
    /// the frame loop must keep polling.
    pub fn wants_polling(&self) -> bool {
        self.sessions
            .iter()
            .any(|s| s.connected || s.connecting || s.awaiting_reconnect() || s.worker.is_some())
    }

    pub fn channel_list_loading(&self) -> bool {
        self.sessions.iter().any(|s| s.channel_list_loading)
    }

    /// The label a session's windows are titled with: its profile name, else
    /// the network's advertised name, else host:port.
    pub(crate) fn network_label(&self, session: &Session) -> String {
        if let Some(id) = &session.profile_id
            && let Some(favorite) = self.settings.server_favorites.iter().find(|f| &f.id == id)
            && !favorite.name.trim().is_empty()
        {
            return favorite.name.clone();
        }
        // The server chooses its NETWORK name: never trust it to be short or
        // free of control and direction-changing characters.
        if let Some(name) = &session.network_support.network_name {
            let name = sanitize_label(name);
            if !name.is_empty() {
                return name;
            }
        }
        session.active_endpoint_label()
    }

    /// Stable network identity for layout.json.
    fn network_identity(session: &Session) -> String {
        if let Some(id) = &session.profile_id {
            return format!("profile:{id}");
        }
        session
            .active_log_context()
            .map(|(network, _, _)| network)
            .unwrap_or_else(|| format!("{}{}", super::windows::UNCONNECTED_PREFIX, session.id))
    }

    // ----- settings -------------------------------------------------------

    /// Push the canonical behaviour settings into every session.
    fn push_config(&mut self) {
        for session in &mut self.sessions {
            session.apply_config(&self.settings);
        }
        ChatMessage::configure_default_timestamp_format(&self.settings.timestamp_format);
    }

    /// Adopt a session's edits as the persisted settings and save them:
    /// behaviour settings it changed itself (e.g. /ignore), and its connect
    /// form only when that form validates, so scratch or invalid form edits
    /// are never persisted by an unrelated save (#316).
    pub(crate) fn save_settings_from(&mut self, index: usize) {
        let session = &self.sessions[index];
        session.export_config(&mut self.settings);
        if session.session_from_form().is_ok() {
            session.export_form(&mut self.settings);
        }
        self.save_settings();
    }

    /// Save the canonical settings and mirror them into every session. The
    /// canonical settings only ever hold a validated connect form (see
    /// `save_settings_from`), so this never persists scratch edits (#316).
    pub(crate) fn save_settings(&mut self) {
        self.push_config();
        self.store_settings();
    }

    fn store_settings(&mut self) {
        if !self.settings_writable {
            tracing::warn!("Not saving settings: settings.json could not be read at startup");
            return;
        }
        #[cfg(test)]
        {
            self.settings_file = Some(self.settings.clone());
        }
        #[cfg(not(test))]
        {
            self.settings.save();
        }
    }

    // ----- frame ----------------------------------------------------------

    /// The full frame: pump every session, run timers and lifecycles, then
    /// draw the toolbar, switchbar, windows, detached windows and dialogs.
    pub fn ui(&mut self, root_ui: &mut egui::Ui) {
        let ctx = root_ui.ctx().clone();
        self.observe_root_input(&ctx);
        self.tick(&ctx, true);
        self.handle_shortcuts(&ctx);
        self.show_dialogs(&ctx);

        egui::Panel::top("toolbar").show(root_ui, |ui| self.draw_toolbar(ui));
        egui::Panel::top("switchbar").show(root_ui, |ui| self.draw_switchbar(ui));
        let central = egui::CentralPanel::default().show(root_ui, |ui| {
            if self.windows.mode == LayoutMode::Maximized {
                self.draw_maximized(ui);
            }
            ui.max_rect()
        });
        if self.windows.mode == LayoutMode::Floating {
            self.draw_floating(&ctx, central.inner);
        }
        self.draw_snap(&ctx, central.inner);
        self.draw_detached(&ctx);
        self.finish_frame();
    }

    /// The main window went to the system tray: its detached windows go with
    /// it, and come back where they were when it is restored. They are
    /// hidden with a viewport command, which eframe also applies on the
    /// logic-only passes a hidden main window gets.
    pub fn hide_detached_windows(&mut self, ctx: &egui::Context) {
        if !self.detached_hidden {
            for key in self.detached_keys() {
                ctx.send_viewport_cmd_to(
                    detached_viewport_id(&key),
                    egui::ViewportCommand::Visible(false),
                );
            }
            self.detached_hidden = true;
        }
        self.focused_detached = None;
    }

    /// A ui pass while the main window is minimized or covered: eframe runs
    /// one only while a detached window is on screen, and paints nothing of
    /// the main window. Keep the detached windows drawn and working. The
    /// pass's `logic` has already ticked every connection (`ticked`).
    pub fn ui_detached_only(&mut self, ctx: &egui::Context, ticked: bool) {
        if !ticked {
            self.background_tick(ctx);
        }
        self.draw_detached(ctx);
        self.finish_frame();
    }

    #[cfg(test)]
    pub(crate) fn detached_windows_hidden(&self) -> bool {
        self.detached_hidden
    }

    fn has_detached(&self) -> bool {
        self.windows.windows.iter().any(|w| w.detached)
    }

    fn detached_keys(&self) -> Vec<WindowKey> {
        self.windows
            .windows
            .iter()
            .filter(|w| w.detached)
            .map(|w| w.key.clone())
            .collect()
    }

    /// The main window is hidden, minimized or covered: drain every session
    /// so the buffers cannot fill, keep timers and lifecycles running.
    /// Unless the main window went to the tray, detached windows are still
    /// on screen, and one of them may have focus.
    pub fn background_tick(&mut self, ctx: &egui::Context) {
        let detached_live = !self.tray_hidden && self.has_detached();
        if !detached_live {
            self.focused_detached = None;
        }
        self.focused = self.focused_detached.is_some();
        if self.tray_hidden {
            // Detached windows are not shown, so egui closes them; they are
            // recreated where they last were (`os_pos`/`os_size`) on restore.
            self.spawn_geometry.clear();
        }
        self.tick(ctx, detached_live);
        self.finish_frame();
    }

    fn observe_root_input(&mut self, ctx: &egui::Context) {
        let root_focused = ctx.input(|i| i.focused);
        self.focused = root_focused || self.focused_detached.is_some();
        let (activity, acted_here) = ctx.input(|i| {
            let pressed = i.pointer.any_pressed()
                || i.events
                    .iter()
                    .any(|e| matches!(e, egui::Event::Key { pressed: true, .. }));
            (
                !i.keys_down.is_empty() || i.pointer.any_click() || pressed,
                pressed,
            )
        });
        if activity {
            self.note_activity();
        }
        // The user is working in the main window while a detached window is
        // the active one: the window the main window shows becomes active
        // (a click on a particular window or switchbar entry overrides this
        // later in the frame).
        if acted_here
            && self
                .windows
                .active
                .as_ref()
                .and_then(|k| self.windows.get(k))
                .is_some_and(|w| w.detached)
            && let Some(shown) = self.maximized_window()
        {
            self.user_activate(shown);
        }
    }

    fn note_activity(&mut self) {
        self.last_activity = Instant::now();
        for session in &mut self.sessions {
            session.note_user_activity(self.last_activity);
        }
    }

    /// Per-frame work that must run whether or not anything is drawn.
    fn tick(&mut self, ctx: &egui::Context, visible: bool) {
        self.had_messages_this_frame = false;
        let active_conn = self.windows.active.as_ref().map(|k| k.conn);
        for session in &mut self.sessions {
            // A message counts as seen only in the active window of a
            // focused OS window.
            // Regaining focus marks the selected tab seen (#305).
            session.set_window_focused(
                visible && self.focused && active_conn.is_none_or(|c| c == session.id),
            );
            session.last_activity = self.last_activity;
            let limit = if visible {
                Some(MAX_MESSAGES_PER_FRAME)
            } else {
                None
            };
            if session.pump(limit) {
                // More queued than one frame drains: come straight back.
                ctx.request_repaint();
            }
            self.had_messages_this_frame |= session.had_messages_this_frame;
            session.run_timed_tasks();
        }
        self.run_lifecycles(ctx);
        self.process_events();
        self.flush_deferred_notification();
        self.sync_windows();
        self.update_global_dialog_owner();
    }

    fn run_lifecycles(&mut self, ctx: &egui::Context) {
        for index in 0..self.sessions.len() {
            // One live session per endpoint: two would share log files and
            // nick state on the same network. Sessions earlier in this pass
            // have already started their workers, so the first one wins.
            let id = self.sessions[index].id;
            let blocked: Vec<_> = self
                .sessions
                .iter()
                .filter(|s| {
                    s.id != id && (s.connected || s.worker.is_some() || s.awaiting_reconnect())
                })
                .filter_map(|s| s.active_endpoint_key())
                .collect();
            self.sessions[index].blocked_endpoints = blocked;
            self.sessions[index].process_connection_lifecycle(ctx);
        }

        // Drop sessions whose status window was closed once they are gone.
        let closing = &self.closing_sessions;
        let before = self.sessions.len();
        let mut removed = Vec::new();
        self.sessions.retain(|s| {
            let done = closing.contains(&s.id)
                && s.worker.is_none()
                && s.closing_rx.is_none()
                && !s.connected
                && !s.connecting;
            if done {
                removed.push(s.id);
            }
            !done
        });
        for id in removed {
            self.closing_sessions.remove(&id);
            self.last_current.remove(&id);
        }
        if self.sessions.len() != before && self.sessions.is_empty() {
            let index = self.add_session();
            self.sessions[index].show_connect_dialog = true;
        }
    }

    pub(crate) fn process_events(&mut self) {
        for index in 0..self.sessions.len() {
            let events = std::mem::take(&mut self.sessions[index].events);
            for event in events {
                match event {
                    SessionEvent::Notify { title, body, force } => {
                        self.notify(&title, &body, force);
                    }
                    SessionEvent::OpenSettings => {
                        let conn = self.sessions[index].id;
                        self.open_settings(conn);
                    }
                    SessionEvent::SaveSettings => self.save_settings_from(index),
                    SessionEvent::SaveConfig => {
                        self.sessions[index].export_config(&mut self.settings);
                        self.save_settings();
                    }
                    SessionEvent::SaveAutoPerform => self.save_auto_perform_from(index),
                    SessionEvent::DuplicateEndpoint => self.focus_duplicate_of(index),
                    SessionEvent::NewServer {
                        host,
                        port,
                        use_tls,
                    } => self.open_server(&host, &port, use_tls),
                    SessionEvent::Window { command, target } => {
                        let key = WindowKey::new(self.sessions[index].id, target);
                        self.window_command(command, key);
                    }
                }
            }
        }
    }

    /// Persist `/perform` edits where they belong. Auto-perform is part of a
    /// server profile (it typically identifies to that network's services):
    /// it goes to the favorite the form was loaded from, or to the Quick
    /// Connect defaults only when those are for this same server. Never to
    /// another network's profile or defaults.
    fn save_auto_perform_from(&mut self, index: usize) {
        let session = &self.sessions[index];
        let list = session.auto_perform.clone();
        if let Some(id) = session.form_profile.clone()
            && let Some(favorite) = self
                .settings
                .server_favorites
                .iter_mut()
                .find(|favorite| favorite.id == id)
        {
            favorite.auto_perform = list;
            self.save_settings();
        } else if same_endpoint(
            (&session.server_host, &session.server_port, session.use_tls),
            (
                &self.settings.server_host,
                &self.settings.server_port,
                self.settings.use_tls,
            ),
        ) {
            self.settings.auto_perform = list;
            self.save_settings();
        } else {
            self.sessions[index].add_server_message(ChatMessage::system(
                "Auto-perform changed for this connection only: save it as a favorite to keep it",
            ));
        }
    }

    /// A session refused to start a second connection to an endpoint
    /// another session is live on: bring that session's status window
    /// forward.
    fn focus_duplicate_of(&mut self, index: usize) {
        let Some(endpoint) = self.sessions[index].active_endpoint_key() else {
            return;
        };
        let id = self.sessions[index].id;
        if let Some(other) = self
            .sessions
            .iter()
            .find(|s| s.id != id && s.active_endpoint_key().as_ref() == Some(&endpoint))
        {
            let key = WindowKey::new(other.id, None);
            self.activate(key);
        }
    }

    /// `/window ...` typed in the window `key`.
    pub(crate) fn window_command(&mut self, command: WindowCommand, key: WindowKey) {
        match command {
            WindowCommand::Tile => {
                self.windows.mode = LayoutMode::Floating;
                self.tile_request = Some(TileRequest::Tile);
            }
            WindowCommand::Cascade => {
                self.windows.mode = LayoutMode::Floating;
                self.tile_request = Some(TileRequest::Cascade);
            }
            WindowCommand::Maximize => self.windows.mode = LayoutMode::Maximized,
            WindowCommand::Restore => self.windows.mode = LayoutMode::Floating,
            WindowCommand::Detach => {
                if self.windows.get(&key).is_some_and(|w| !w.detached) {
                    self.toggle_detached(&key);
                }
            }
            WindowCommand::Attach => {
                if self.windows.get(&key).is_some_and(|w| w.detached) {
                    self.toggle_detached(&key);
                }
            }
            WindowCommand::Close => self.request_close(key),
            WindowCommand::New => self.new_connection(),
        }
        self.windows.dirty = true;
    }

    /// A new status window with its Connect dialog. If the dialog is
    /// dismissed without connecting, the window goes and focus returns.
    fn new_connection(&mut self) {
        self.focus_before_new = self.windows.active.clone();
        let index = self.add_session();
        self.sessions[index].show_connect_dialog = true;
        let key = WindowKey::new(self.sessions[index].id, None);
        self.user_activate(key);
    }

    /// `/server -m`: connect to an endpoint in a new status window.
    fn open_server(&mut self, host: &str, port: &str, use_tls: bool) {
        let index = self.add_session();
        let conn = self.sessions[index].id;
        let session = &mut self.sessions[index];
        // A fresh endpoint never inherits the defaults' credentials.
        session.apply_unprofiled_endpoint(host, port, use_tls);
        session.show_connect_dialog = false;
        if !session.start_session_from_form() {
            session.show_connect_dialog = true;
        }
        self.activate(WindowKey::new(conn, None));
    }

    /// Show a desktop notification, unless notifications are off or one of
    /// our windows has focus (private messages show regardless). Highlights
    /// and the like are best-effort: a flood inside the rate-limit interval
    /// is dropped. Private messages are never dropped, only paced: inside
    /// their own interval they are held and folded into one summary that
    /// `tick` delivers later (#323).
    pub(crate) fn notify(&mut self, title: &str, body: &str, force: bool) {
        // Title only, at debug: the body is message text, which must not
        // reach the default-level log (#228).
        tracing::debug!("Notification requested: {}", title);
        if !self.settings.notifications_enabled {
            return;
        }
        if self.focused && !force {
            return;
        }
        if force {
            self.flush_deferred_notification();
            if self
                .last_forced_notification_time
                .is_some_and(|last| last.elapsed() < MIN_NOTIFICATION_INTERVAL)
            {
                let held = self
                    .deferred_notification
                    .get_or_insert_with(DeferredNotification::default);
                held.title = title.to_string();
                held.body = body.to_string();
                held.count += 1;
                return;
            }
            self.last_forced_notification_time = Some(Instant::now());
        } else {
            if self
                .last_notification_time
                .is_some_and(|last| last.elapsed() < MIN_NOTIFICATION_INTERVAL)
            {
                tracing::debug!("Notification rate-limited");
                return;
            }
            self.last_notification_time = Some(Instant::now());
        }
        self.dispatch_notification(title, body);
    }

    /// Deliver a held private-message notification once its interval has
    /// passed; several go out as one summary. Turning notifications off
    /// discards it.
    fn flush_deferred_notification(&mut self) {
        if self.deferred_notification.is_none() {
            return;
        }
        if !self.settings.notifications_enabled {
            self.deferred_notification = None;
            return;
        }
        if self
            .last_forced_notification_time
            .is_some_and(|last| last.elapsed() < MIN_NOTIFICATION_INTERVAL)
        {
            return;
        }
        let Some(held) = self.deferred_notification.take() else {
            return;
        };
        let (title, body) = if held.count > 1 {
            (
                format!("{} new private messages", held.count),
                format!("{}: {}", held.title, held.body),
            )
        } else {
            (held.title, held.body)
        };
        self.last_forced_notification_time = Some(Instant::now());
        self.dispatch_notification(&title, &body);
    }

    /// Hand a notification that passed the policy to the OS. Tests record it
    /// instead of spawning helpers.
    fn dispatch_notification(&mut self, title: &str, body: &str) {
        #[cfg(test)]
        self.dispatched_notifications
            .push((title.to_string(), body.to_string()));
        #[cfg(not(test))]
        super::show_desktop_notification(title, body);
    }

    /// Reconcile windows with the sessions' tabs and follow focus changes a
    /// session made itself (own JOIN of a requested channel, /query).
    fn sync_windows(&mut self) {
        let mut present = Vec::new();
        for session in &self.sessions {
            let network = Self::network_identity(session);
            present.push(PresentWindow {
                key: WindowKey::new(session.id, None),
                persist: PersistKey {
                    network: network.clone(),
                    target: None,
                },
            });
            for name in session.channels.keys() {
                present.push(PresentWindow {
                    key: WindowKey::new(session.id, Some(name.clone())),
                    persist: PersistKey {
                        network: network.clone(),
                        target: Some(session.network_support.canonicalize(name)),
                    },
                });
            }
        }
        let previous_active = self.windows.active.clone();
        let appeared = self.windows.sync(present);
        if !appeared.is_empty() {
            self.windows.dirty = true;
        }
        if let Some(key) = &self.pending_focus
            && let Some(window) = self.windows.get_mut(key)
        {
            window.view.input_focus_request = true;
            self.pending_focus = None;
        }
        let sessions = &self.sessions;
        let windows = &mut self.windows;
        self.pending_inputs.retain(|(conn, input)| {
            let Some(session) = sessions.iter().find(|s| s.id == *conn) else {
                return false;
            };
            let key = WindowKey::new(*conn, session.current_channel.clone());
            match windows.get_mut(&key) {
                Some(window) => {
                    window.view.input_text = input.clone();
                    false
                }
                None => true,
            }
        });
        if let Some(main) = &self.main_window
            && self.windows.get(main).is_none()
        {
            self.main_window = None;
        }

        // Taken before `sync`, which clears an active window that vanished.
        let active_conn = previous_active.as_ref().map(|k| k.conn);
        let mut follow = None;
        for session in &self.sessions {
            let current = session.current_channel.clone();
            // A session seen for the first time only records its selection:
            // which window starts active is decided below, not by whichever
            // session happened to be created last.
            let previous = self.last_current.insert(session.id, current.clone());
            if previous.is_some_and(|previous| previous != current)
                && active_conn == Some(session.id)
            {
                follow = Some(WindowKey::new(session.id, current));
            }
        }
        if let Some(key) = follow {
            self.activate(key);
        }
        // The active window closed (a part confirmed, a query closed, a nick
        // change re-keyed it): stay on its network.
        if self.windows.active.is_none()
            && let Some(previous) = &previous_active
            && let Some(index) = self.session_index(previous.conn)
        {
            let key = WindowKey::new(previous.conn, self.sessions[index].current_channel.clone());
            self.activate(key);
        }
        // The window that was active when Linefeed last closed, once it
        // reappears: only during startup, and only if the user has not
        // picked a window yet.
        if let Some(key) = self.windows.restore_match.take() {
            if !self.user_activated && self.started.elapsed() < RESTORE_ACTIVE_WINDOW {
                self.activate(key);
            }
        } else if self.user_activated || self.started.elapsed() >= RESTORE_ACTIVE_WINDOW {
            self.windows.forget_restore_active();
        }
        if self.windows.active.is_none() {
            let start = self.restore_after_update.take().or_else(|| {
                self.sessions
                    .first()
                    .map(|first| WindowKey::new(first.id, first.current_channel.clone()))
            });
            if let Some(key) = start {
                self.activate(key);
            }
        }
    }

    /// Make a window the active one: select it in its session (clearing its
    /// unread count) and bring it forward.
    /// `activate` for a window the user picked.
    pub(crate) fn user_activate(&mut self, key: WindowKey) {
        self.user_activated = true;
        self.activate(key);
    }

    pub(crate) fn activate(&mut self, key: WindowKey) {
        let Some(index) = self.session_index(key.conn) else {
            return;
        };
        // Typing continues in the new window: if the input line of the
        // window being left had focus, the new window's input takes it.
        let carry_focus = self
            .windows
            .active
            .as_ref()
            .filter(|active| **active != key)
            .and_then(|active| self.windows.get(active))
            .is_some_and(|window| window.view.input_had_focus);
        if self.windows.get(&key).is_none()
            && key
                .target
                .as_ref()
                .is_some_and(|t| self.sessions[index].channel_key(t).is_none())
        {
            return;
        }
        self.sessions[index].select_tab(key.target.clone());
        self.last_current
            .insert(key.conn, self.sessions[index].current_channel.clone());
        if self.windows.get(&key).is_some_and(|w| !w.detached) {
            self.main_window = Some(key.clone());
        }
        if carry_focus {
            match self.windows.get_mut(&key) {
                Some(window) => window.view.input_focus_request = true,
                None => self.pending_focus = Some(key.clone()),
            }
        }
        self.windows.active = Some(key);
        self.windows.dirty = true;
    }

    fn finish_frame(&mut self) {
        if self.windows.dirty {
            self.windows.dirty = false;
            self.layout_dirty_since.get_or_insert_with(Instant::now);
        }
        if let Some(since) = self.layout_dirty_since
            && since.elapsed() >= LAYOUT_SAVE_DELAY
        {
            self.layout_dirty_since = None;
            if self.persist_layout {
                self.windows.layout().save();
            }
        }
    }

    // ----- ordering, shortcuts, closing ----------------------------------

    /// Every window in switchbar order: sessions in creation order, each
    /// with its status window first and then its tabs sorted by name.
    pub(crate) fn ordered_windows(&self) -> Vec<WindowKey> {
        let mut keys = Vec::new();
        for session in &self.sessions {
            for target in session.ordered_tabs() {
                keys.push(WindowKey::new(session.id, target));
            }
        }
        keys
    }

    fn dialog_blocks_shortcuts(&self) -> bool {
        // Only the active session's Connect dialog is ever shown.
        let active = self.sessions.get(self.active_index());
        self.show_settings
            || self.confirm_disconnect.is_some()
            || self.sessions.iter().any(|s| s.show_channel_list)
            || active.is_some_and(|s| s.show_connect_dialog && !s.connected && !s.connecting)
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.dialog_blocks_shortcuts() {
            return;
        }
        let (cycle, reverse, close, toggle_max, alt_index) = ctx.input(|i| {
            let digits = [
                egui::Key::Num1,
                egui::Key::Num2,
                egui::Key::Num3,
                egui::Key::Num4,
                egui::Key::Num5,
                egui::Key::Num6,
                egui::Key::Num7,
                egui::Key::Num8,
                egui::Key::Num9,
            ];
            let alt_index = if i.modifiers.alt {
                digits.iter().position(|k| i.key_pressed(*k))
            } else {
                None
            };
            (
                i.modifiers.ctrl && i.key_pressed(egui::Key::Tab),
                i.modifiers.shift,
                i.modifiers.ctrl && i.key_pressed(egui::Key::W),
                i.modifiers.ctrl && i.key_pressed(egui::Key::M),
                alt_index,
            )
        });
        if cycle {
            self.cycle_window(reverse);
        }
        if close && let Some(key) = self.windows.active.clone() {
            self.request_close(key);
        }
        if toggle_max {
            self.toggle_maximized();
        }
        if let Some(index) = alt_index
            && let Some(key) = self.ordered_windows().get(index).cloned()
        {
            self.user_activate(key);
        }
    }

    pub(crate) fn cycle_window(&mut self, reverse: bool) {
        let order = self.ordered_windows();
        if order.is_empty() {
            return;
        }
        let current = self
            .windows
            .active
            .as_ref()
            .and_then(|key| order.iter().position(|k| k == key))
            .unwrap_or(0);
        let next = if reverse {
            (current + order.len() - 1) % order.len()
        } else {
            (current + 1) % order.len()
        };
        self.user_activate(order[next].clone());
    }

    fn toggle_maximized(&mut self) {
        self.windows.mode = match self.windows.mode {
            LayoutMode::Maximized => LayoutMode::Floating,
            LayoutMode::Floating => LayoutMode::Maximized,
        };
        self.windows.dirty = true;
    }

    /// Close a window the way mIRC does: a joined channel is parted (the
    /// window goes when the server confirms), a query or dead channel tab
    /// closes, and a status window asks before disconnecting its server.
    pub(crate) fn request_close(&mut self, key: WindowKey) {
        let Some(index) = self.session_index(key.conn) else {
            return;
        };
        match &key.target {
            Some(target) => self.sessions[index].close_tab(target),
            None => {
                let session = &self.sessions[index];
                if session.connected || session.connecting || session.worker.is_some() {
                    self.confirm_disconnect = Some(key.conn);
                } else if self.sessions.len() > 1 {
                    // Idle, but possibly waiting out a reconnect backoff:
                    // stop it, or the closed window would reconnect.
                    self.sessions[index].request_manual_disconnect(None);
                    self.closing_sessions.insert(key.conn);
                }
            }
        }
    }

    /// The confirmed half of closing a status window.
    pub(crate) fn disconnect_and_close(&mut self, conn: ConnId) {
        let Some(index) = self.session_index(conn) else {
            return;
        };
        let quit = self.quit_reason();
        self.sessions[index].request_manual_disconnect(Some(quit));
        if self.sessions.len() > 1 {
            self.closing_sessions.insert(conn);
        } else {
            self.sessions[index].show_connect_dialog = true;
        }
    }

    fn quit_reason(&self) -> String {
        if self.settings.quit_message.is_empty() {
            "Linefeed".to_string()
        } else {
            self.settings.quit_message.clone()
        }
    }

    // ----- dialogs --------------------------------------------------------

    fn show_dialogs(&mut self, ctx: &egui::Context) {
        if self.global_dialog_window().is_none() {
            self.show_global_dialogs(ctx);
        }
        for index in 0..self.sessions.len() {
            let conn = self.sessions[index].id;
            if self.detached_dialog_owner(conn).is_some() {
                continue;
            }
            self.show_session_dialogs(ctx, index);
        }
        if let Some(target) = self.color_target.clone()
            && !self.windows.get(&target).is_some_and(|w| w.detached)
        {
            self.show_color_picker_for(ctx, &target);
        }
    }

    /// The application's own dialogs: Connect (for the active session),
    /// Settings, and the confirmation before disconnecting a server.
    fn show_global_dialogs(&mut self, ctx: &egui::Context) {
        let active = self.active_index();
        {
            let App {
                sessions,
                settings,
                favorites_ui,
                ..
            } = self;
            let session = &mut sessions[active];
            if session.show_connect_dialog
                && !session.connected
                && !session.connecting
                && session.show_connect_window(ctx, &mut settings.server_favorites, favorites_ui)
            {
                self.cancel_connect(active);
            }
        }
        self.drop_cancelled_new_sessions();

        if self.show_settings {
            self.show_settings_window(ctx);
            // Live edits apply to every session while the dialog is open.
            self.push_config();
        }
        if let Some(conn) = self.confirm_disconnect {
            self.show_confirm_disconnect(ctx, conn);
        }
    }

    /// The detached window the application's dialogs show in, if they were
    /// asked for from one that is still detached.
    fn global_dialog_window(&self) -> Option<WindowKey> {
        self.global_dialog_owner
            .clone()
            .filter(|key| self.windows.get(key).is_some_and(|w| w.detached))
    }

    /// When the application's dialogs open, note the detached window they
    /// belong to: the status window being closed, or the window the user
    /// is working in.
    fn update_global_dialog_owner(&mut self) {
        let open = self.show_settings
            || self.confirm_disconnect.is_some()
            || self
                .sessions
                .get(self.active_index())
                .is_some_and(|s| s.show_connect_dialog && !s.connected && !s.connecting);
        if !open {
            self.global_dialog_owner = None;
        } else if !self.global_dialogs_were_open {
            let detached = |key: &WindowKey| self.windows.get(key).is_some_and(|w| w.detached);
            let closing = self
                .confirm_disconnect
                .map(|conn| WindowKey::new(conn, None))
                .filter(|key| detached(key));
            self.global_dialog_owner =
                closing.or_else(|| self.windows.active.clone().filter(|key| detached(key)));
        }
        self.global_dialogs_were_open = open;
    }

    /// The detached window a session's dialogs belong to, if any.
    fn detached_dialog_owner(&self, conn: ConnId) -> Option<WindowKey> {
        self.dialog_owner
            .get(&conn)
            .filter(|key| self.windows.get(key).is_some_and(|w| w.detached))
            .cloned()
    }

    fn show_session_dialogs(&mut self, ctx: &egui::Context, index: usize) {
        let session = &mut self.sessions[index];
        if session.show_channel_list {
            session.show_channel_list_window(ctx);
        }
        if session.show_channel_info {
            session.show_channel_info_window(ctx);
        }
        if !session.show_channel_list && !session.show_channel_info {
            self.dialog_owner.remove(&session.id);
        }
    }

    /// The dialogs a detached window opened, drawn inside its OS window.
    fn show_detached_window_dialogs(&mut self, ctx: &egui::Context, key: &WindowKey) {
        if self.global_dialog_window().as_ref() == Some(key) {
            self.show_global_dialogs(ctx);
        }
        if self.detached_dialog_owner(key.conn).as_ref() == Some(key)
            && let Some(index) = self.session_index(key.conn)
        {
            self.show_session_dialogs(ctx, index);
        }
        if self.color_target.as_ref() == Some(key) {
            self.show_color_picker_for(ctx, key);
        }
    }

    fn show_color_picker_for(&mut self, ctx: &egui::Context, target: &WindowKey) {
        if let Some(index) = self.session_index(target.conn) {
            if self.sessions[index].show_color_picker {
                if let Some(code) = self.sessions[index].show_color_picker_window(ctx)
                    && let Some(window) = self.windows.get_mut(target)
                {
                    window.view.input_text.push_str(&code);
                }
            } else {
                self.color_target = None;
            }
        }
    }

    /// Open Settings for the network `conn`: its Connection and Automation
    /// tabs edit that session's form until the window closes.
    pub(crate) fn open_settings(&mut self, conn: ConnId) {
        if !self.show_settings {
            self.settings_session = Some(conn);
        }
        self.show_settings = true;
    }

    /// The session the Settings window edits.
    pub(crate) fn settings_index(&self) -> usize {
        self.settings_session
            .and_then(|conn| self.session_index(conn))
            .unwrap_or_else(|| self.active_index())
    }

    /// Cancel the Connect dialog of session `index`: discard its scratch
    /// form edits (#208) and the favorites selection, which the restored
    /// list may no longer index (#268). The canonical settings are what was
    /// last persisted.
    pub(crate) fn cancel_connect(&mut self, index: usize) {
        self.sessions[index].cancel_connect_dialog(&self.settings);
        self.favorites_ui = FavoritesUi::default();
    }

    /// The About tab's updates switch: an App-only setting, persisted
    /// without adopting any session's connect form (#316).
    pub(crate) fn set_automatic_updates(&mut self, enabled: bool) {
        self.settings.automatic_updates = enabled;
        self.updater.set_automatic(enabled);
        self.save_settings();
    }

    /// A status window opened with "New connection" whose Connect dialog was
    /// cancelled is discarded, as long as another session remains.
    fn drop_cancelled_new_sessions(&mut self) {
        if self.sessions.len() < 2 {
            return;
        }
        let pristine: Vec<ConnId> = self
            .sessions
            .iter()
            .filter(|s| s.is_pristine() && !s.show_connect_dialog)
            .map(|s| s.id)
            .collect();
        for conn in pristine {
            if self.sessions.len() > 1 {
                self.sessions.retain(|s| s.id != conn);
                self.last_current.remove(&conn);
                if let Some(key) = self.focus_before_new.take()
                    && self.session_index(key.conn).is_some()
                {
                    self.activate(key);
                }
            }
        }
    }

    fn show_confirm_disconnect(&mut self, ctx: &egui::Context, conn: ConnId) {
        let Some(index) = self.session_index(conn) else {
            self.confirm_disconnect = None;
            return;
        };
        let label = self.network_label(&self.sessions[index]);
        let mut decision = None;
        egui::Window::new("Close status window")
            .id(egui::Id::unique(("confirm_disconnect", conn)))
            .order(egui::Order::Foreground)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Disconnect from {label} and close all of its windows?"
                ));
                ui.horizontal(|ui| {
                    if ui.button("Disconnect").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
        match decision {
            Some(true) => {
                self.confirm_disconnect = None;
                self.disconnect_and_close(conn);
            }
            Some(false) => self.confirm_disconnect = None,
            None => {}
        }
    }

    // ----- toolbar and switchbar ------------------------------------------

    fn draw_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Linefeed");
            ui.separator();
            let active = self.active_index();
            let label = self.network_label(&self.sessions[active]);
            self.sessions[active].draw_status_summary(ui, &label);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Settings").clicked() {
                    let conn = self.sessions[self.active_index()].id;
                    self.open_settings(conn);
                }
                ui.menu_button("Window", |ui| {
                    if ui.button("Tile").clicked() {
                        self.tile_request = Some(TileRequest::Tile);
                        self.windows.mode = LayoutMode::Floating;
                        ui.close();
                    }
                    if ui.button("Cascade").clicked() {
                        self.tile_request = Some(TileRequest::Cascade);
                        self.windows.mode = LayoutMode::Floating;
                        ui.close();
                    }
                    let label = match self.windows.mode {
                        LayoutMode::Maximized => "Restore windows (Ctrl+M)",
                        LayoutMode::Floating => "Maximize (Ctrl+M)",
                    };
                    if ui.button(label).clicked() {
                        self.toggle_maximized();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("New connection...").clicked() {
                        self.new_connection();
                        ui.close();
                    }
                });
                ui.separator();
                let active = self.active_index();
                let session = &mut self.sessions[active];
                if !session.connected && !session.connecting {
                    if ui.button("Connect").clicked() {
                        session.show_connect_dialog = true;
                    }
                } else if session.connected && ui.button("Disconnect").clicked() {
                    let quit = self.quit_reason();
                    self.sessions[active].request_manual_disconnect(Some(quit));
                }
                let session = &mut self.sessions[active];
                if session.connected {
                    session.draw_join_box(ui);
                }
            });
        });
    }

    fn draw_switchbar(&mut self, ui: &mut egui::Ui) {
        let mut clicked = None;
        let mut close = None;
        let mut toggle_detach = None;
        let mut info = None;
        ui.horizontal_wrapped(|ui| {
            for (n, session) in self.sessions.iter().enumerate() {
                if n > 0 {
                    ui.separator();
                }
                let label = self.network_label(session);
                ui.label(RichText::new(format!("{label}:")).strong());
                for target in session.ordered_tabs() {
                    let key = WindowKey::new(session.id, target.clone());
                    let detached = self.windows.get(&key).is_some_and(|w| w.detached);
                    let text = switchbar_text(session, target.as_deref(), detached);
                    let selected = self.windows.active.as_ref() == Some(&key);
                    let mut response = ui.selectable_label(selected, text);
                    if detached {
                        response = response.on_hover_text("In its own window");
                    }
                    if response.clicked() {
                        clicked = Some(key.clone());
                    }
                    response.context_menu(|ui| {
                        let detach_label = if detached { "Reattach" } else { "Detach" };
                        if ui.button(detach_label).clicked() {
                            toggle_detach = Some(key.clone());
                            ui.close();
                        }
                        if let Some(name) = &target
                            && session.is_channel_name(name)
                            && ui.button("Channel Info").clicked()
                        {
                            info = Some(key.clone());
                            ui.close();
                        }
                        let close_label = match &target {
                            None => "Disconnect and close",
                            Some(name) if session.tab_close_needs_part(name) => "Part",
                            Some(_) => "Close",
                        };
                        if ui.button(close_label).clicked() {
                            close = Some(key.clone());
                            ui.close();
                        }
                    });
                }
            }
        });
        if let Some(key) = clicked {
            self.user_activate(key.clone());
            self.focus_detached(ui.ctx(), &key);
        }
        if let Some(key) = toggle_detach {
            self.toggle_detached(&key);
        }
        if let Some(key) = info
            && let (Some(index), Some(target)) = (self.session_index(key.conn), key.target)
        {
            self.sessions[index].open_channel_info(&target);
        }
        if let Some(key) = close {
            self.request_close(key);
        }
    }

    pub(crate) fn toggle_detached(&mut self, key: &WindowKey) {
        if let Some(window) = self.windows.get_mut(key) {
            window.detached = !window.detached;
            if !window.detached {
                self.spawn_geometry.remove(key);
            }
            self.windows.dirty = true;
        }
        if self.windows.get(key).is_some_and(|w| !w.detached) {
            self.user_activate(key.clone());
        } else {
            if self.main_window.as_ref() == Some(key) {
                self.main_window = None;
            }
            // The main window now shows another window; it becomes the
            // active one until the detached window takes focus, so the
            // toolbar, Ctrl+W and the unread state follow what is on screen.
            if self.windows.active.as_ref() == Some(key)
                && let Some(shown) = self.maximized_window()
            {
                self.activate(shown);
            }
        }
    }

    fn focus_detached(&self, ctx: &egui::Context, key: &WindowKey) {
        if self.windows.get(key).is_some_and(|w| w.detached) {
            ctx.send_viewport_cmd_to(detached_viewport_id(key), egui::ViewportCommand::Focus);
        }
    }

    // ----- window drawing -------------------------------------------------

    /// Title of a window: "#rust @ Libera", "Status: Libera".
    pub(crate) fn window_title(&self, key: &WindowKey) -> String {
        let Some(index) = self.session_index(key.conn) else {
            return String::new();
        };
        let label = self.network_label(&self.sessions[index]);
        match &key.target {
            Some(target) => format!("{target} @ {label}"),
            None => format!("Status: {label}"),
        }
    }

    /// The attached window that fills the chat area when maximized.
    fn maximized_window(&self) -> Option<WindowKey> {
        let attached = |key: &WindowKey| self.windows.get(key).is_some_and(|w| !w.detached);
        if let Some(active) = &self.windows.active
            && attached(active)
        {
            return Some(active.clone());
        }
        if let Some(main) = &self.main_window
            && attached(main)
        {
            return Some(main.clone());
        }
        self.ordered_windows().into_iter().find(|k| attached(k))
    }

    fn draw_maximized(&mut self, ui: &mut egui::Ui) {
        let Some(key) = self.maximized_window() else {
            ui.centered_and_justified(|ui| {
                ui.label("All windows are detached.");
            });
            return;
        };
        let title = self.window_title(&key);
        let mut action = None;
        let mut restore = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new(&title).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("✕")
                    .on_hover_text("Close (Ctrl+W)")
                    .clicked()
                {
                    action = Some(WindowAction::Close);
                }
                if ui
                    .small_button("Detach")
                    .on_hover_text("Open in its own window")
                    .clicked()
                {
                    action = Some(WindowAction::Detach);
                }
                // Resting on it opens the snap layouts, as on a floating
                // window's maximize button.
                let response = ui.small_button("Restore");
                if response.clicked() {
                    action = Some(WindowAction::Restore);
                }
                restore = Some(response);
            });
        });
        if let Some(restore) = restore {
            self.track_maximize_hover(ui.ctx(), &key, &restore);
        }
        ui.separator();
        self.draw_window_contents(ui, &key);
        match action {
            Some(WindowAction::Close) => self.request_close(key),
            Some(WindowAction::Detach) => self.toggle_detached(&key),
            Some(WindowAction::Restore) => {
                self.dismiss_flyout();
                self.toggle_maximized();
            }
            None => {}
        }
    }

    fn draw_floating(&mut self, ctx: &egui::Context, area: egui::Rect) {
        // Before any tile/cascade, so a placement made for this area is not
        // rescaled again.
        self.windows.fit_to_area(area);
        match self.tile_request.take() {
            Some(TileRequest::Tile) => self.windows.tile(area),
            Some(TileRequest::Cascade) => self.windows.cascade(area),
            None => {}
        }
        let keys: Vec<WindowKey> = self
            .windows
            .windows
            .iter()
            .filter(|w| !w.detached)
            .map(|w| w.key.clone())
            .collect();
        let pressed_at = ctx.input(|i| {
            if i.pointer.any_pressed() {
                i.pointer.interact_pos()
            } else {
                None
            }
        });
        let mut activate = None;
        let mut close = Vec::new();
        let mut maximize = None;
        let mut detach = None;
        let cascade = super::windows::cascade_rects(area, keys.len().max(1));
        for (n, key) in keys.iter().enumerate() {
            let title = self.window_title(key);
            let id = child_window_id(key);
            let (mut pending, known) = match self.windows.get_mut(key) {
                Some(window) => (window.pending_place.take(), window.child_rect),
                None => continue,
            };
            // A window seen for the first time gets an explicit placement:
            // egui lays a new window out in an unconstrained sizing pass, in
            // which the chat panels would claim the whole screen. It opens
            // under the active window instead of covering it (and catching
            // the user's next click or keystroke).
            if known.is_none() && pending.is_none() {
                pending = Some(cascade[n % cascade.len()]);
            }
            let default = known.or(pending).unwrap_or(cascade[0]);
            let active = self.windows.active.as_ref() == Some(key);
            // Our own caption (`draw_caption`) replaces egui's title bar.
            // The window stays movable: egui 0.37 drops the left and top
            // resize handles of a window that is not, and dragging the
            // background moves it as before (without snapping).
            let mut window = egui::Window::new(title)
                .id(id)
                .title_bar(false)
                .collapsible(false)
                .default_rect(default);
            window = match (pending, self.dragged_window_pos(ctx, key)) {
                // A tile/cascade/snap placement lies inside the area by
                // construction. Constraining this frame would use the
                // window's previous, larger size and shift it out of its
                // cell; from the next frame on the new size is known.
                (Some(rect), _) => window.constrain(false).fixed_rect(rect),
                (None, Some(pos)) => window.current_pos(pos).constrain_to(area),
                (None, None) => window.constrain_to(area),
            };
            let shown = window.show(ctx, |ui| {
                let action = self.draw_caption(ui, key, active);
                self.draw_window_contents(ui, key);
                action
            });
            if let Some(shown) = shown {
                match shown.inner.flatten() {
                    Some(CaptionAction::Close) => close.push(key.clone()),
                    Some(CaptionAction::Maximize) => maximize = Some(key.clone()),
                    Some(CaptionAction::Detach) => detach = Some(key.clone()),
                    None => {}
                }
                let rect = shown.response.rect;
                if let Some(pos) = pressed_at
                    && rect.contains(pos)
                    && ctx.layer_id_at(pos) == Some(shown.response.layer_id)
                {
                    activate = Some(key.clone());
                }
                if let Some(window) = self.windows.get_mut(key)
                    && window.child_rect != Some(rect)
                {
                    window.child_rect = Some(rect);
                    self.windows.dirty = true;
                }
            }
        }
        if let Some(key) = activate
            && self.windows.active.as_ref() != Some(&key)
        {
            self.user_activate(key);
        }
        if let Some(key) = maximize {
            self.user_activate(key);
            self.windows.mode = LayoutMode::Maximized;
            self.windows.dirty = true;
        }
        if let Some(key) = detach {
            self.toggle_detached(&key);
        }
        // The active window is always the front one. egui raises a window on
        // its first visible frame and on a click; a click also makes that
        // window active (above), so this only overrides windows appearing
        // on their own, which must not cover the one the user is typing in.
        if let Some(active) = self.windows.active.clone()
            && self.windows.get(&active).is_some_and(|w| !w.detached)
        {
            ctx.move_to_top(egui::LayerId::new(
                egui::Order::Middle,
                child_window_id(&active),
            ));
        }
        for key in close {
            self.request_close(key);
        }
    }

    /// Detached windows: one OS window each, drawn with immediate viewports
    /// (sessions live on this thread and are not `Send`).
    fn draw_detached(&mut self, ctx: &egui::Context) {
        if self.tray_hidden {
            // Not showing a viewport for a pass destroys its OS window; it is
            // recreated from the stored geometry on restore.
            self.spawn_geometry.clear();
            self.focused_detached = None;
            return;
        }
        let keys: Vec<WindowKey> = self
            .windows
            .windows
            .iter()
            .filter(|w| w.detached)
            .map(|w| w.key.clone())
            .collect();
        if std::mem::take(&mut self.detached_hidden) {
            // Back from a minimized main window.
            for key in &keys {
                ctx.send_viewport_cmd_to(
                    detached_viewport_id(key),
                    egui::ViewportCommand::Visible(true),
                );
            }
        }
        let mut focused = None;
        let mut close = Vec::new();
        let mut reattach = Vec::new();
        let mut activity = false;
        for key in keys {
            let title = format!("{} — {}", self.window_title(&key), MAIN_WINDOW_TITLE);
            let (pos, size) = *self.spawn_geometry.entry(key.clone()).or_insert_with(|| {
                self.windows
                    .get(&key)
                    .map(|w| (w.os_pos, w.os_size))
                    .unwrap_or((None, None))
            });
            let mut builder = egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size(size.unwrap_or(egui::vec2(720.0, 520.0)))
                .with_min_inner_size([320.0, 240.0])
                .with_icon(
                    self.icon
                        .get_or_insert_with(|| std::sync::Arc::new(crate::load_icon()))
                        .clone(),
                );
            if let Some(pos) = pos {
                builder = builder.with_position(pos);
            }
            let id = detached_viewport_id(&key);
            let result = ctx.show_viewport_immediate(id, builder, |ui, _class| {
                let (close_requested, is_focused, outer, inner, any_input, pressed) =
                    ui.ctx().input(|i| {
                        (
                            i.viewport().close_requested(),
                            i.focused,
                            i.viewport().outer_rect,
                            i.viewport().inner_rect,
                            !i.keys_down.is_empty() || i.pointer.any_pressed(),
                            i.pointer.any_pressed(),
                        )
                    });
                let mut reattach_clicked = false;
                egui::Panel::top(("detached_header", key.conn, key.target.clone())).show(
                    ui,
                    |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(self.window_title(&key)).strong());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .small_button("Reattach")
                                        .on_hover_text("Put back inside the main window")
                                        .clicked()
                                    {
                                        reattach_clicked = true;
                                    }
                                },
                            );
                        });
                    },
                );
                egui::CentralPanel::default().show(ui, |ui| {
                    self.draw_window_contents(ui, &key);
                });
                self.show_detached_window_dialogs(ui.ctx(), &key);
                (
                    close_requested,
                    is_focused,
                    outer,
                    inner,
                    any_input,
                    pressed,
                    reattach_clicked,
                )
            });
            let (close_requested, is_focused, outer, inner, any_input, pressed, reattach_clicked) =
                result;
            let mut moved = false;
            if let Some(window) = self.windows.get_mut(&key) {
                let pos = outer.map(|r| r.min);
                let size = inner.map(|r| r.size());
                if pos.is_some() && window.os_pos != pos {
                    window.os_pos = pos;
                    moved = true;
                }
                if size.is_some() && window.os_size != size {
                    window.os_size = size;
                    moved = true;
                }
            }
            self.windows.dirty |= moved;
            // Some window systems report focus late (or, under remote
            // desktops, not at all): a click in the window counts too.
            if is_focused || pressed {
                focused = Some(key.clone());
            }
            activity |= any_input;
            if close_requested {
                close.push(key.clone());
            }
            if reattach_clicked {
                reattach.push(key.clone());
            }
        }
        if let Some(key) = &focused
            && self.focused_detached.as_ref() != Some(key)
            && self.windows.active.as_ref() != Some(key)
        {
            self.user_activate(key.clone());
        }
        self.focused_detached = focused;
        if activity {
            self.note_activity();
        }
        for key in reattach {
            self.toggle_detached(&key);
        }
        for key in close {
            // A status window asks before disconnecting, in its own OS
            // window (the main one may be minimized), and stays if the
            // user cancels.
            if key.target.is_some() {
                self.spawn_geometry.remove(&key);
            }
            self.request_close(key);
        }
        // Forget geometry of viewports that are no longer detached.
        let detached: HashSet<WindowKey> = self
            .windows
            .windows
            .iter()
            .filter(|w| w.detached)
            .map(|w| w.key.clone())
            .collect();
        self.spawn_geometry.retain(|key, _| detached.contains(key));
    }

    /// Draw one window's contents (topic, user list, scrollback, input) for
    /// its session, with the window's input line swapped in.
    pub(crate) fn draw_window_contents(&mut self, ui: &mut egui::Ui, key: &WindowKey) {
        let Some(index) = self.session_index(key.conn) else {
            return;
        };
        let App {
            sessions,
            windows,
            command_history,
            ..
        } = self;
        let Some(window) = windows.get_mut(key) else {
            return;
        };
        let session = &mut sessions[index];
        let detached = window.detached;
        let (changed_to, (picker, dialog)) = with_window_view(
            session,
            &mut window.view,
            command_history,
            &key.target,
            |s| {
                let before = (
                    s.show_color_picker,
                    s.show_channel_list,
                    s.show_channel_info,
                );
                s.draw_target(ui);
                (
                    !before.0 && s.show_color_picker,
                    (!before.1 && s.show_channel_list) || (!before.2 && s.show_channel_info),
                )
            },
        );
        if picker {
            self.color_target = Some(key.clone());
        }
        // A dialog asked for from this window (/list, the colour button)
        // shows where the user is: in this window's OS window if detached.
        if dialog {
            if detached {
                self.dialog_owner.insert(key.conn, key.clone());
            } else {
                self.dialog_owner.remove(&key.conn);
            }
        }
        if let Some(target) = changed_to {
            self.user_activate(WindowKey::new(key.conn, target));
        }
    }

    /// Detach the first session's status window (tests outside this module).
    #[cfg(test)]
    pub(crate) fn detach_status_window_for_tests(&mut self) {
        self.sync_windows();
        let key = WindowKey::new(self.sessions[0].id, None);
        self.toggle_detached(&key);
    }

    /// Submit a window's input line as if Enter was pressed in it.
    #[cfg(test)]
    pub(crate) fn submit_window_input(&mut self, key: &WindowKey) {
        let Some(index) = self.session_index(key.conn) else {
            return;
        };
        let App {
            sessions,
            windows,
            command_history,
            ..
        } = self;
        let Some(window) = windows.get_mut(key) else {
            return;
        };
        let (changed_to, ()) = with_window_view(
            &mut sessions[index],
            &mut window.view,
            command_history,
            &key.target,
            |s| s.process_input(),
        );
        if let Some(target) = changed_to {
            self.user_activate(WindowKey::new(key.conn, target));
        }
    }

    // ----- update restart -------------------------------------------------

    /// Snapshot every session for an update restart. The active session is
    /// the top-level snapshot, which is all an older binary reads; the
    /// others follow in `others`.
    pub fn update_session(&mut self) -> UpdateSession {
        let active = self.active_index();
        let mut snapshots = Vec::new();
        for index in 0..self.sessions.len() {
            let conn = self.sessions[index].id;
            let target = self.sessions[index].current_channel.clone();
            let input = self
                .windows
                .get(&WindowKey::new(conn, target))
                .map(|w| w.view.input_text.clone())
                .unwrap_or_default();
            let mut snapshot = self.sessions[index].update_session();
            snapshot.set_input(input);
            snapshots.push(snapshot);
        }
        if snapshots.is_empty() {
            return UpdateSession::default();
        }
        let mut first = snapshots.remove(active);
        first.set_others(snapshots);
        first.set_active_position(active);
        first
    }

    fn restore_update_session(&mut self, mut snapshot: UpdateSession) {
        let mut ordered = snapshot.take_others();
        let position = snapshot.active_position().min(ordered.len());
        ordered.insert(position, snapshot);
        for snapshot in ordered {
            let input = snapshot.input().to_string();
            let index = self.add_session();
            self.sessions[index].restore_update_session(snapshot);
            self.sessions[index].input_text.clear();
            let conn = self.sessions[index].id;
            self.pending_inputs.push((conn, input));
        }
        // The active session's window is the one to come back to.
        if let Some(session) = self.sessions.get(position) {
            self.restore_after_update =
                Some(WindowKey::new(session.id, session.current_channel.clone()));
        }
    }

    /// Before an update restart or exit: save, and send QUIT everywhere.
    pub fn prepare_update_shutdown(&mut self) {
        if self.persist_layout {
            self.windows.layout().save();
        }
        if !self.sessions.is_empty() {
            let active = self.active_index();
            let session = &self.sessions[active];
            session.export_config(&mut self.settings);
            if session.session_from_form().is_ok() {
                session.export_form(&mut self.settings);
            }
        }
        self.store_settings();
        for session in &mut self.sessions {
            session.prepare_update_shutdown();
        }
    }

    /// Wait (bounded) for every worker to flush QUIT and exit.
    pub fn join_workers(&mut self, deadline: Instant) {
        for session in &mut self.sessions {
            if let Some(thread) = session.worker.take() {
                while !thread.is_finished() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if thread.is_finished() {
                    let _ = thread.join();
                }
            }
            session.msg_rx = None;
        }
    }
}

/// Make a server-chosen name safe to show as a label: no control or
/// bidirectional-override characters, at most 32 characters.
fn sanitize_label(name: &str) -> String {
    const BIDI: &[char] = &[
        '\u{061C}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}',
        '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
    ];
    name.chars()
        .filter(|c| !c.is_control() && !BIDI.contains(c))
        .take(32)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Whether two connect forms point at the same server.
fn same_endpoint(a: (&str, &str, bool), b: (&str, &str, bool)) -> bool {
    a.0.trim().eq_ignore_ascii_case(b.0.trim()) && a.1.trim() == b.1.trim() && a.2 == b.2
}

/// A forced (private message) notification held back by the rate limiter.
#[derive(Default)]
struct DeferredNotification {
    title: String,
    body: String,
    count: usize,
}

enum WindowAction {
    Close,
    Detach,
    Restore,
}

enum TileRequest {
    Tile,
    Cascade,
}

fn child_window_id(key: &WindowKey) -> egui::Id {
    egui::Id::unique(("linefeed_window", key.conn, key.target.clone()))
}

pub(crate) fn detached_viewport_id(key: &WindowKey) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(("linefeed_detached", key.conn, key.target.clone()))
}

/// Switchbar button text, coloured like the old sidebar: unread channels
/// green, unread queries orange, unread status yellow, queries light blue.
fn switchbar_text(session: &Session, target: Option<&str>, detached: bool) -> RichText {
    let text = switchbar_label(session, target);
    // Detached windows are set in italics: a marker glyph would depend on
    // the fonts installed.
    if detached { text.italics() } else { text }
}

fn switchbar_label(session: &Session, target: Option<&str>) -> RichText {
    let marker = "";
    match target {
        None => {
            let unread = session.server_unread;
            let label = if unread > 0 {
                format!("Status ({unread}){marker}")
            } else {
                format!("Status{marker}")
            };
            if unread > 0 {
                RichText::new(label)
                    .strong()
                    .color(Color32::from_rgb(255, 255, 100))
            } else {
                RichText::new(label)
            }
        }
        Some(name) => {
            let unread = session.channels.get(name).map(|c| c.unread).unwrap_or(0);
            let is_query = !session.is_channel_name(name);
            let label = if unread > 0 {
                format!("{name} ({unread}){marker}")
            } else {
                format!("{name}{marker}")
            };
            match (unread > 0, is_query) {
                (true, true) => RichText::new(label)
                    .strong()
                    .color(Color32::from_rgb(255, 150, 50)),
                (true, false) => RichText::new(label)
                    .strong()
                    .color(Color32::from_rgb(100, 255, 100)),
                (false, true) => RichText::new(label).color(Color32::from_rgb(150, 200, 255)),
                (false, false) => RichText::new(label),
            }
        }
    }
}

/// Run `f` with `session` pointed at `target` and the window's input line,
/// history and completion swapped in. Returns `Some(new target)` when `f`
/// moved the session's focus elsewhere (e.g. /query, or the window's own
/// tab went away), so the caller can activate that window.
pub(crate) fn with_window_view<R>(
    session: &mut Session,
    view: &mut TargetView,
    history: &mut Vec<String>,
    target: &Option<String>,
    f: impl FnOnce(&mut Session) -> R,
) -> (Option<Option<String>>, R) {
    let saved_current = std::mem::replace(&mut session.current_channel, target.clone());
    swap_view(session, view, history);
    let result = f(session);
    swap_view(session, view, history);
    let after = session.current_channel.clone();
    let exists = |session: &Session, target: &Option<String>| {
        target
            .as_ref()
            .is_none_or(|name| session.channel_key(name).is_some())
    };
    if &after == target {
        session.current_channel = saved_current;
        (None, result)
    } else if exists(session, target) {
        // The command moved focus (/query, /join of a joined channel).
        (Some(after), result)
    } else {
        // The window's own tab went away (/close, /part of a query): keep
        // the session's selection where it was, if that still exists; the
        // App moves activity off a vanished active window (`sync_windows`).
        if exists(session, &saved_current) {
            session.current_channel = saved_current;
        }
        (None, result)
    }
}

fn swap_view(session: &mut Session, view: &mut TargetView, history: &mut Vec<String>) {
    std::mem::swap(&mut session.input_text, &mut view.input_text);
    std::mem::swap(&mut session.history_index, &mut view.history_index);
    std::mem::swap(&mut session.history_temp, &mut view.history_temp);
    std::mem::swap(&mut session.tab_completion, &mut view.tab_completion);
    std::mem::swap(&mut session.selected_user, &mut view.selected_user);
    std::mem::swap(
        &mut session.input_focus_request,
        &mut view.input_focus_request,
    );
    std::mem::swap(&mut session.input_had_focus, &mut view.input_had_focus);
    std::mem::swap(&mut session.command_history, history);
}

/// The same floors the Settings dialog enforces, applied to values loaded
/// from disk (an older version or a hand edit must not disable them, #144).
fn clamp_settings(settings: &mut Settings) {
    settings.auto_away_minutes = settings.auto_away_minutes.max(1);
    settings.logging_history_lines = settings.logging_history_lines.max(10);
    settings.max_scrollback = settings.max_scrollback.max(100);
    settings.reconnect_delay_secs = settings.reconnect_delay_secs.max(1);
    settings.max_reconnect_attempts = settings.max_reconnect_attempts.max(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::snap;
    use crate::irc::{IrcCommand, IrcMessage};
    use tokio::sync::mpsc;

    fn test_settings() -> Settings {
        Settings {
            nickname: "me".to_string(),
            logging_enabled: false,
            logging_load_history: false,
            ..Settings::default()
        }
    }

    /// A hermetic app: no settings.json, no layout.json, one idle session.
    fn test_app() -> App {
        let mut app = App::with_settings(test_settings(), LayoutFile::default());
        app.start(None);
        app
    }

    /// Make session `index` look registered, capturing what it sends.
    fn connect(app: &mut App, index: usize) -> mpsc::UnboundedReceiver<IrcCommand> {
        let (tx, rx) = mpsc::unbounded_channel();
        let session = &mut app.sessions[index];
        session.cmd_tx = Some(tx);
        session.connected = true;
        session.connecting = false;
        rx
    }

    fn feed(app: &mut App, index: usize, line: &str) {
        app.sessions[index].handle_incoming_message(IrcMessage::parse(line).unwrap());
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<IrcCommand>) -> Vec<String> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|c| c.to_string())
            .collect()
    }

    fn frame(app: &mut App, ctx: &egui::Context) -> egui::FullOutput {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1024.0, 768.0),
                )),
                ..Default::default()
            },
            |ui| app.ui(ui),
        );
        output.textures_delta.clear();
        output
    }

    fn texts(output: &egui::FullOutput) -> Vec<String> {
        let mut out = Vec::new();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, &mut out);
        }
        out
    }

    fn collect_text(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => out.push(text.galley.text().to_string()),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect_text(s, out)),
            _ => {}
        }
    }

    fn two_networks() -> (
        App,
        mpsc::UnboundedReceiver<IrcCommand>,
        mpsc::UnboundedReceiver<IrcCommand>,
    ) {
        let mut app = test_app();
        let second = app.add_session();
        let rx_a = connect(&mut app, 0);
        let rx_b = connect(&mut app, second);
        feed(&mut app, 0, ":me!u@h JOIN #rust");
        feed(&mut app, second, ":me!u@h JOIN #debian");
        app.sync_windows();
        (app, rx_a, rx_b)
    }

    #[test]
    fn startup_without_profiles_shows_one_idle_session_with_the_connect_dialog() {
        let app = test_app();
        assert_eq!(app.sessions.len(), 1);
        assert!(app.sessions[0].show_connect_dialog);
        assert!(!app.sessions[0].connecting);
    }

    #[test]
    fn auto_connect_profiles_each_start_their_own_session_at_startup() {
        let mut settings = test_settings();
        for (name, host, auto) in [
            ("A", "a.example", true),
            ("B", "b.example", false),
            ("C", "c.example", true),
        ] {
            settings.server_favorites.push(ServerFavorite {
                name: name.into(),
                host: host.into(),
                port: "6697".into(),
                use_tls: true,
                auto_connect: auto,
                ..ServerFavorite::default()
            });
        }
        let mut app = App::with_settings(settings, LayoutFile::default());
        app.start(None);
        let hosts: Vec<String> = app
            .sessions
            .iter()
            .map(|s| s.active_server_config().unwrap().host)
            .collect();
        assert_eq!(hosts, ["a.example", "c.example"]);
        assert!(
            app.sessions
                .iter()
                .all(|s| s.connecting && !s.show_connect_dialog)
        );
        assert!(app.sessions.iter().all(|s| s.profile_id.is_some()));
        assert_ne!(app.sessions[0].profile_id, app.sessions[1].profile_id);
        assert_eq!(app.network_label(&app.sessions[1]), "C");
    }

    #[test]
    fn input_typed_in_one_window_is_sent_to_that_windows_session_and_target() {
        let (mut app, mut rx_a, mut rx_b) = two_networks();
        let conn_b = app.sessions[1].id;
        let key = WindowKey::new(conn_b, Some("#debian".into()));
        app.windows.get_mut(&key).unwrap().view.input_text = "hello debian".into();
        app.submit_window_input(&key);

        assert!(drain(&mut rx_a).is_empty());
        assert_eq!(drain(&mut rx_b), ["PRIVMSG #debian :hello debian"]);
        assert!(app.windows.get(&key).unwrap().view.input_text.is_empty());
        // The other session's own selection is untouched.
        assert_eq!(app.sessions[0].current_channel.as_deref(), Some("#rust"));
    }

    #[test]
    fn each_window_keeps_its_own_unsent_input() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let rust = WindowKey::new(app.sessions[0].id, Some("#rust".into()));
        let debian = WindowKey::new(app.sessions[1].id, Some("#debian".into()));
        app.windows.get_mut(&rust).unwrap().view.input_text = "draft one".into();
        app.windows.get_mut(&debian).unwrap().view.input_text = "draft two".into();
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        assert_eq!(app.windows.get(&rust).unwrap().view.input_text, "draft one");
        assert_eq!(
            app.windows.get(&debian).unwrap().view.input_text,
            "draft two"
        );
        assert!(app.sessions[0].input_text.is_empty() && app.sessions[1].input_text.is_empty());
    }

    #[test]
    fn switchbar_order_matches_ctrl_tab_order() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        feed(&mut app, 0, ":me!u@h JOIN #alpha");
        app.sync_windows();
        let order = app.ordered_windows();
        let a = app.sessions[0].id;
        let b = app.sessions[1].id;
        assert_eq!(
            order,
            [
                WindowKey::new(a, None),
                WindowKey::new(a, Some("#alpha".into())),
                WindowKey::new(a, Some("#rust".into())),
                WindowKey::new(b, None),
                WindowKey::new(b, Some("#debian".into())),
            ]
        );
        app.activate(order[0].clone());
        let mut visited = Vec::new();
        for _ in 0..order.len() {
            app.cycle_window(false);
            visited.push(app.windows.active.clone().unwrap());
        }
        let mut expected = order[1..].to_vec();
        expected.push(order[0].clone());
        assert_eq!(visited, expected);
    }

    #[test]
    fn closing_a_joined_channel_window_sends_part_and_keeps_it_until_confirmed() {
        let (mut app, mut rx_a, _rx_b) = two_networks();
        let key = WindowKey::new(app.sessions[0].id, Some("#rust".into()));
        app.request_close(key.clone());
        assert_eq!(drain(&mut rx_a), ["PART #rust"]);
        app.sync_windows();
        assert!(
            app.windows.get(&key).is_some(),
            "window closed before the server confirmed"
        );
        feed(&mut app, 0, ":me!u@h PART #rust");
        app.sync_windows();
        assert!(app.windows.get(&key).is_none());
    }

    #[test]
    fn window_commands_arrange_detach_and_close_the_window_they_are_typed_in() {
        let (mut app, mut rx_a, _rx_b) = two_networks();
        let rust = WindowKey::new(app.sessions[0].id, Some("#rust".into()));
        app.activate(rust.clone());
        app.windows.get_mut(&rust).unwrap().view.input_text = "/window detach".into();
        app.submit_window_input(&rust);
        app.process_events();
        assert!(app.windows.get(&rust).unwrap().detached);

        app.windows.get_mut(&rust).unwrap().view.input_text = "/window attach".into();
        app.submit_window_input(&rust);
        app.process_events();
        assert!(!app.windows.get(&rust).unwrap().detached);

        app.sessions[0].process_command("/window tile");
        app.process_events();
        assert_eq!(app.windows.mode, LayoutMode::Floating);
        app.sessions[0].process_command("/window max");
        app.process_events();
        assert_eq!(app.windows.mode, LayoutMode::Maximized);

        app.windows.get_mut(&rust).unwrap().view.input_text = "/window close".into();
        app.submit_window_input(&rust);
        app.process_events();
        assert_eq!(drain(&mut rx_a), ["PART #rust"]);

        let before = app.sessions.len();
        app.sessions[0].process_command("/window new");
        app.process_events();
        assert_eq!(app.sessions.len(), before + 1);
        assert!(app.sessions.last().unwrap().show_connect_dialog);

        app.sessions[0].process_command("/window sideways");
        assert!(app.sessions[0].events.is_empty());
    }

    #[test]
    fn perform_goes_to_the_profile_it_belongs_to_never_another_networks_defaults() {
        let mut settings = test_settings();
        settings.server_host = "irc.libera.chat".into();
        settings.server_port = "6697".into();
        settings.use_tls = true;
        settings.auto_perform = "/msg NickServ IDENTIFY liberapass".into();
        settings.server_favorites.push(ServerFavorite {
            name: "OFTC".into(),
            host: "irc.oftc.net".into(),
            port: "6697".into(),
            use_tls: true,
            auto_perform: "/msg NickServ IDENTIFY oftcpass".into(),
            ..ServerFavorite::default()
        });
        let mut app = App::with_settings(settings, LayoutFile::default());
        app.start(None);
        let favorite = app.settings.server_favorites[0].clone();
        app.sessions[0].load_favorite(&favorite);
        assert!(app.sessions[0].start_session_from_form());

        app.sessions[0].process_command("/perform /join #x");
        app.process_events();
        let saved = app.settings_file.clone().unwrap();
        // The OFTC profile got its own list...
        assert_eq!(
            saved.server_favorites[0].auto_perform,
            "/msg NickServ IDENTIFY oftcpass\n/join #x"
        );
        // ...and Libera's defaults never got OFTC's IDENTIFY.
        assert_eq!(saved.server_host, "irc.libera.chat");
        assert_eq!(saved.auto_perform, "/msg NickServ IDENTIFY liberapass");

        // An unprofiled session on another server persists nowhere.
        let second = app.add_session();
        app.sessions[second].apply_unprofiled_endpoint("irc.other.example", "6697", true);
        app.sessions[second].process_command("/perform /join #y");
        app.process_events();
        assert_eq!(
            app.settings_file.clone().unwrap().auto_perform,
            "/msg NickServ IDENTIFY liberapass"
        );
        assert!(
            app.sessions[second]
                .server_messages
                .iter()
                .any(|m| m.content.contains("this connection only"))
        );
    }

    #[test]
    fn detaching_the_active_window_activates_the_one_the_main_window_shows() {
        let (mut app, mut rx_a, mut rx_b) = two_networks();
        let rust = WindowKey::new(app.sessions[0].id, Some("#rust".into()));
        let debian = WindowKey::new(app.sessions[1].id, Some("#debian".into()));
        app.activate(rust.clone());
        app.activate(debian.clone());
        app.toggle_detached(&debian);
        let shown = app.maximized_window().unwrap();
        assert_ne!(shown, debian);
        assert_eq!(app.windows.active, Some(shown.clone()));
        // Ctrl+W in the main window closes what it shows, not the detached one.
        app.request_close(app.windows.active.clone().unwrap());
        assert!(
            drain(&mut rx_b).is_empty(),
            "the detached channel was parted"
        );
        let _ = drain(&mut rx_a);

        // Working in the main window again after the detached one took over.
        app.activate(debian.clone());
        let ctx = egui::Context::default();
        ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| app.observe_root_input(ui.ctx()),
        )
        .textures_delta
        .clear();
        assert_ne!(app.windows.active, Some(debian));
    }

    #[test]
    fn closing_the_active_window_keeps_focus_on_its_network() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        feed(&mut app, 1, ":me!u@h JOIN #ubuntu");
        app.sync_windows();
        let debian = WindowKey::new(app.sessions[1].id, Some("#debian".into()));
        app.activate(debian.clone());
        app.request_close(debian.clone());
        feed(&mut app, 1, ":me!u@h PART #debian");
        app.sync_windows();
        assert_eq!(
            app.windows.active.as_ref().map(|k| k.conn),
            Some(app.sessions[1].id)
        );
    }

    #[test]
    fn the_remembered_active_window_does_not_steal_focus_once_the_user_picked_one() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let network = App::network_identity(&app.sessions[0]);
        let layout = LayoutFile {
            active: Some(PersistKey {
                network,
                target: Some("bob".into()),
            }),
            ..LayoutFile::default()
        };
        app.windows = WindowManager::from_layout(layout);
        app.sync_windows();
        let rust = WindowKey::new(app.sessions[0].id, Some("#rust".into()));
        app.user_activate(rust.clone());
        // Bob writes much later.
        feed(&mut app, 0, ":bob!u@h PRIVMSG me :hi");
        app.sync_windows();
        assert_eq!(app.windows.active, Some(rust));
        assert_eq!(app.sessions[0].current_channel.as_deref(), Some("#rust"));
    }

    #[test]
    fn the_remembered_active_window_is_restored_through_activate_at_startup() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let network = App::network_identity(&app.sessions[1]);
        app.windows = WindowManager::from_layout(LayoutFile {
            active: Some(PersistKey {
                network,
                target: Some("#debian".into()),
            }),
            ..LayoutFile::default()
        });
        app.sync_windows();
        let debian = WindowKey::new(app.sessions[1].id, Some("#debian".into()));
        assert_eq!(app.windows.active, Some(debian));
        assert_eq!(app.sessions[1].current_channel.as_deref(), Some("#debian"));
    }

    #[test]
    fn a_hidden_connect_dialog_does_not_block_shortcuts() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        app.new_connection();
        assert!(app.dialog_blocks_shortcuts());
        // The user goes back to another window without dismissing it.
        app.user_activate(WindowKey::new(app.sessions[0].id, Some("#rust".into())));
        assert!(!app.dialog_blocks_shortcuts());
    }

    #[test]
    fn input_focus_follows_a_window_switch() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let rust = WindowKey::new(app.sessions[0].id, Some("#rust".into()));
        let debian = WindowKey::new(app.sessions[1].id, Some("#debian".into()));
        app.activate(rust.clone());
        app.windows.get_mut(&rust).unwrap().view.input_had_focus = true;
        app.cycle_window(false);
        let next = app.windows.active.clone().unwrap();
        assert!(app.windows.get(&next).unwrap().view.input_focus_request);

        // Drawing the window hands the request to its input line.
        let ctx = egui::Context::default();
        app.activate(debian.clone());
        app.windows
            .get_mut(&debian)
            .unwrap()
            .view
            .input_focus_request = true;
        frame(&mut app, &ctx);
        frame(&mut app, &ctx);
        let view = &app.windows.get(&debian).unwrap().view;
        assert!(!view.input_focus_request);
        assert!(view.input_had_focus);
    }

    #[test]
    fn input_focus_follows_into_a_query_opened_from_the_keyboard() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let rust = WindowKey::new(app.sessions[0].id, Some("#rust".into()));
        app.activate(rust.clone());
        app.windows.get_mut(&rust).unwrap().view.input_had_focus = true;
        app.windows.get_mut(&rust).unwrap().view.input_text = "/query carol".into();
        app.submit_window_input(&rust);
        app.sync_windows();
        let carol = WindowKey::new(app.sessions[0].id, Some("carol".into()));
        assert_eq!(app.windows.active, Some(carol.clone()));
        assert!(app.windows.get(&carol).unwrap().view.input_focus_request);
    }

    #[test]
    fn closing_an_idle_status_window_cancels_its_pending_reconnect() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let conn = app.sessions[1].id;
        app.sessions[1].apply_unprofiled_endpoint("irc.example", "6697", true);
        assert!(app.sessions[1].start_session_from_form());
        app.sessions[1].connecting = false;
        app.sessions[1].mark_connection_lost();
        app.sessions[1].last_disconnect_time = None; // retry due now
        assert!(app.sessions[1].awaiting_reconnect());

        app.request_close(WindowKey::new(conn, None));
        assert!(
            app.confirm_disconnect.is_none(),
            "idle windows close at once"
        );
        app.run_lifecycles(&egui::Context::default());
        assert!(
            app.session_index(conn).is_none(),
            "the closed session reconnected"
        );
    }

    #[test]
    fn a_session_waiting_to_reconnect_keeps_its_endpoint() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        // Session A on irc.example dropped and waits out its backoff.
        app.sessions[0].apply_unprofiled_endpoint("irc.example", "6697", true);
        assert!(app.sessions[0].start_session_from_form());
        app.sessions[0].connecting = false;
        app.sessions[0].mark_connection_lost();
        app.sessions[0].last_disconnect_time = Some(Instant::now());
        assert!(app.sessions[0].awaiting_reconnect());
        // Session B tries the same server meanwhile.
        app.sessions[1].request_manual_disconnect(None);
        app.sessions[1].apply_unprofiled_endpoint("irc.example", "6697", true);
        assert!(app.sessions[1].start_session_from_form());

        app.run_lifecycles(&egui::Context::default());
        assert!(!app.sessions[1].connecting);
        assert!(app.sessions[1].worker.is_none());
        assert!(
            app.sessions[1]
                .server_messages
                .iter()
                .any(|m| m.content.contains("Already connected"))
        );
        assert!(app.sessions[0].awaiting_reconnect(), "A lost its claim");
    }

    #[test]
    fn server_chosen_network_names_are_sanitized_for_labels() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let evil = format!("Libera{}{}", '\u{202E}', "x".repeat(80));
        feed(
            &mut app,
            0,
            &format!(":srv 005 me NETWORK={evil} :are supported"),
        );
        let label = app.network_label(&app.sessions[0]);
        assert!(label.chars().count() <= 32, "{label}");
        assert!(!label.contains('\u{202E}'));
        assert!(label.starts_with("Libera"));
    }

    #[test]
    fn close_typed_in_a_window_that_is_not_active_keeps_the_selection() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        feed(&mut app, 0, ":carol!u@h PRIVMSG me :hi");
        app.sync_windows();
        let rust = WindowKey::new(app.sessions[0].id, Some("#rust".into()));
        let carol = WindowKey::new(app.sessions[0].id, Some("carol".into()));
        app.activate(rust.clone());
        app.windows.get_mut(&carol).unwrap().view.input_text = "/close".into();
        app.submit_window_input(&carol);
        app.sync_windows();
        assert!(app.windows.get(&carol).is_none());
        assert_eq!(app.windows.active, Some(rust));
        assert_eq!(app.sessions[0].current_channel.as_deref(), Some("#rust"));
    }

    #[test]
    fn settings_stay_with_the_network_they_were_opened_for() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let first = app.sessions[0].id;
        app.user_activate(WindowKey::new(first, None));
        app.open_settings(first);
        app.user_activate(WindowKey::new(app.sessions[1].id, None));
        assert_eq!(app.settings_index(), 0);
    }

    #[test]
    fn cancelling_a_new_connection_returns_to_the_previous_window() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let debian = WindowKey::new(app.sessions[1].id, Some("#debian".into()));
        app.user_activate(debian.clone());
        app.new_connection();
        let new = app.sessions.len() - 1;
        app.cancel_connect(new);
        app.drop_cancelled_new_sessions();
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.windows.active, Some(debian));
    }

    #[test]
    fn closing_a_query_window_closes_it_without_sending_anything() {
        let (mut app, mut rx_a, _rx_b) = two_networks();
        feed(&mut app, 0, ":friend!u@h PRIVMSG me :hi");
        app.sync_windows();
        let key = WindowKey::new(app.sessions[0].id, Some("friend".into()));
        assert!(app.windows.get(&key).is_some());
        app.request_close(key.clone());
        app.sync_windows();
        assert!(app.windows.get(&key).is_none());
        assert!(drain(&mut rx_a).is_empty());
    }

    #[test]
    fn closing_a_status_window_asks_before_disconnecting() {
        let (mut app, mut rx_a, mut rx_b) = two_networks();
        let conn_b = app.sessions[1].id;
        app.request_close(WindowKey::new(conn_b, None));
        assert_eq!(app.confirm_disconnect, Some(conn_b));
        assert!(
            drain(&mut rx_b).is_empty(),
            "disconnected before confirming"
        );

        app.disconnect_and_close(conn_b);
        assert!(drain(&mut rx_b).iter().any(|line| line.starts_with("QUIT")));
        assert!(!app.sessions[1].connected);
        // The worker is gone (none in tests): the session and its windows go.
        app.run_lifecycles(&egui::Context::default());
        app.sync_windows();
        assert_eq!(app.sessions.len(), 1);
        assert!(app.windows.windows.iter().all(|w| w.key.conn != conn_b));
        // The other network is untouched.
        assert!(app.sessions[0].connected);
        assert!(drain(&mut rx_a).is_empty());
    }

    #[test]
    fn closing_the_last_status_window_disconnects_but_keeps_the_session() {
        let mut app = test_app();
        let mut rx = connect(&mut app, 0);
        let conn = app.sessions[0].id;
        app.request_close(WindowKey::new(conn, None));
        app.disconnect_and_close(conn);
        app.run_lifecycles(&egui::Context::default());
        assert_eq!(app.sessions.len(), 1);
        assert!(app.sessions[0].show_connect_dialog);
        assert!(drain(&mut rx).iter().any(|line| line.starts_with("QUIT")));
    }

    #[test]
    fn server_dash_m_opens_a_new_session_without_the_defaults_credentials() {
        let mut settings = test_settings();
        settings.server_host = "irc.secret.example".into();
        settings.password = "hunter2".into();
        settings.sasl_username = "acct".into();
        settings.sasl_password = "saslpw".into();
        settings.auto_perform = "/msg NickServ IDENTIFY hunter2".into();
        let mut app = App::with_settings(settings, LayoutFile::default());
        app.start(None);
        let _rx = connect(&mut app, 0);

        app.sessions[0].process_command("/server -m irc.other.example 6697");
        app.process_events();

        assert_eq!(app.sessions.len(), 2);
        let config = app.sessions[1].active_server_config().unwrap();
        assert_eq!(config.host, "irc.other.example");
        assert_eq!(config.port, 6697);
        assert!(config.use_tls);
        assert_eq!(config.password, None);
        assert_eq!(config.sasl_username, None);
        assert_eq!(config.sasl_password, None);
        assert!(app.sessions[1].connecting);
        // The first session keeps its connection and endpoint.
        assert!(app.sessions[0].connected);
        assert_eq!(app.active_index(), 1);
    }

    #[test]
    fn a_second_session_to_a_live_endpoint_is_refused() {
        let mut app = test_app();
        app.sessions[0].apply_unprofiled_endpoint("irc.example", "6697", true);
        assert!(app.sessions[0].start_session_from_form());
        let _rx = connect(&mut app, 0);
        let second = app.add_session();
        app.sessions[second].apply_unprofiled_endpoint("IRC.example", "6697", true);
        assert!(app.sessions[second].start_session_from_form());

        app.run_lifecycles(&egui::Context::default());
        assert!(!app.sessions[second].connecting);
        assert!(app.sessions[second].worker.is_none());
        assert!(
            app.sessions[second]
                .server_messages
                .iter()
                .any(|m| m.content.contains("Already connected"))
        );
    }

    #[test]
    fn startup_starts_on_the_first_network_and_follows_its_auto_join() {
        let mut app = test_app();
        let second = app.add_session();
        let _rx_a = connect(&mut app, 0);
        let _rx_b = connect(&mut app, second);
        app.sync_windows();
        assert_eq!(
            app.windows.active,
            Some(WindowKey::new(app.sessions[0].id, None))
        );
        // Both networks auto-join after registering; the second one last.
        feed(&mut app, 0, ":me!u@h JOIN #rust");
        feed(&mut app, second, ":me!u@h JOIN #debian");
        app.sync_windows();
        assert_eq!(
            app.windows.active,
            Some(WindowKey::new(app.sessions[0].id, Some("#rust".into())))
        );
    }

    #[test]
    fn a_join_in_a_background_session_does_not_steal_the_active_window() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let rust = WindowKey::new(app.sessions[0].id, Some("#rust".into()));
        app.activate(rust.clone());
        app.sync_windows();
        feed(&mut app, 1, ":me!u@h JOIN #other");
        app.sync_windows();
        assert_eq!(app.windows.active, Some(rust));
    }

    #[test]
    fn settings_saved_from_one_session_reach_every_session() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        app.sessions[1].ignore_list.push("troll".into());
        // export + push, without touching the real settings.json
        app.sessions[1].export_config(&mut app.settings);
        app.push_config();
        assert_eq!(app.settings.ignore_list, ["troll"]);
        assert_eq!(app.sessions[0].ignore_list, ["troll"]);
        feed(&mut app, 0, ":troll!u@h PRIVMSG #rust :spam");
        assert!(
            app.sessions[0].channels["#rust"]
                .messages
                .iter()
                .all(|m| m.content != "spam")
        );
    }

    #[test]
    fn update_snapshot_carries_every_session_and_restores_them() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        app.sessions[0].apply_unprofiled_endpoint("a.example", "6697", true);
        assert!(app.sessions[0].start_session_from_form());
        app.sessions[1].apply_unprofiled_endpoint("b.example", "6697", true);
        assert!(app.sessions[1].start_session_from_form());
        for index in 0..2 {
            app.sessions[index].connected = true;
            app.sessions[index].connecting = false;
        }
        let debian = WindowKey::new(app.sessions[1].id, Some("#debian".into()));
        app.activate(debian.clone());
        app.windows.get_mut(&debian).unwrap().view.input_text = "unsent".into();

        let json = serde_json::to_string(&app.update_session()).unwrap();
        let snapshot: UpdateSession = serde_json::from_str(&json).unwrap();
        let mut restored = App::with_settings(test_settings(), LayoutFile::default());
        restored.start(Some(snapshot));
        restored.sync_windows();

        let hosts: Vec<String> = restored
            .sessions
            .iter()
            .map(|s| s.active_server_config().unwrap().host)
            .collect();
        // Networks keep their order; the active one is also the top-level
        // snapshot, which is all an older binary reads.
        assert_eq!(hosts, ["a.example", "b.example"]);
        let top: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(top["session"]["server"]["host"], "b.example");
        assert!(restored.sessions.iter().all(|s| s.connecting));
        let key = WindowKey::new(restored.sessions[1].id, Some("#debian".into()));
        assert_eq!(
            restored.windows.get(&key).unwrap().view.input_text,
            "unsent"
        );
        assert_eq!(restored.windows.active, Some(key));
    }

    #[test]
    fn old_single_session_update_snapshot_restores_as_one_session() {
        // The shape 0.1.4 writes: no `others`.
        let old = r##"{
            "session": {
                "server": {"host": "irc.example", "port": 6697, "use_tls": true,
                           "nick": "me", "username": "me"},
                "auto_join_channels": "", "auto_perform": "", "set_invisible": false
            },
            "reconnect": true,
            "tabs": [["#rust", null, true]],
            "selected": "#rust",
            "input": "half typed",
            "away": null
        }"##;
        let snapshot: UpdateSession = serde_json::from_str(old).unwrap();
        let mut app = App::with_settings(test_settings(), LayoutFile::default());
        app.start(Some(snapshot));
        app.sync_windows();
        assert_eq!(app.sessions.len(), 1);
        assert!(app.sessions[0].connecting);
        let key = WindowKey::new(app.sessions[0].id, Some("#rust".into()));
        assert_eq!(app.windows.get(&key).unwrap().view.input_text, "half typed");
    }

    /// A loopback IRC server that completes registration for any nick, echoes
    /// JOINs, and streams every line it receives.
    fn echo_server() -> (u16, std::sync::mpsc::Receiver<String>) {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut nick = String::from("*");
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let text = line.trim_end().to_string();
                line.clear();
                let words: Vec<&str> = text.split(' ').collect();
                let reply = match words.as_slice() {
                    ["CAP", "LS", ..] => ":srv CAP * LS :multi-prefix".to_string(),
                    // Like a real server: a request for anything not
                    // offered is refused whole.
                    ["CAP", "REQ", caps @ ..] => {
                        let caps = caps.join(" ");
                        let reply = if caps.trim_start_matches(':') == "multi-prefix" {
                            "ACK"
                        } else {
                            "NAK"
                        };
                        format!(":srv CAP * {reply} {caps}")
                    }
                    ["NICK", new] => {
                        nick = new.trim_start_matches(':').to_string();
                        String::new()
                    }
                    ["CAP", "END"] => format!(":srv 001 {nick} :Welcome"),
                    ["JOIN", channels, ..] => channels
                        .split(',')
                        .map(|c| format!(":{nick}!u@h JOIN {c}"))
                        .collect::<Vec<_>>()
                        .join("\r\n"),
                    _ => String::new(),
                };
                if !reply.is_empty() {
                    let _ = writer.write_all(format!("{reply}\r\n").as_bytes());
                }
                if tx.send(text).is_err() {
                    break;
                }
            }
        });
        (port, rx)
    }

    #[test]
    fn a_snapshot_written_by_0_1_4_restores_its_connection_channels_and_draft() {
        // The bytes 0.1.4's `update_session()` wrote (generated with that
        // release's code), read the way the updater hands them over; only the
        // port is pointed at a local server.
        let (port, lines) = echo_server();
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/gui/testdata/update_session_0_1_4.json"
        );
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(fixture).unwrap()).unwrap();
        json["session"]["server"]["port"] = port.into();
        let path = std::env::temp_dir().join(format!(
            "linefeed-update-session-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, json.to_string()).unwrap();
        let snapshot: UpdateSession = crate::updater::install::read_json(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        let mut app = App::with_settings(test_settings(), LayoutFile::default());
        app.start(Some(snapshot));
        let ctx = egui::Context::default();
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !seen.iter().any(|l: &String| l.starts_with("AWAY")) {
            assert!(Instant::now() < deadline, "restore incomplete: {seen:?}");
            app.background_tick(&ctx);
            seen.extend(lines.try_iter());
            std::thread::sleep(Duration::from_millis(5));
        }
        // The connection 0.1.4 had: its server password and nick, invisible
        // mode, both channels (one keyed), the away message and auto-perform.
        for line in [
            "PASS :server-pass",
            "NICK tester_",
            "MODE tester_ +i",
            "JOIN #rust",
            "JOIN #secret k3y",
            "AWAY :lunch",
        ] {
            assert!(seen.iter().any(|l| l == line), "{line} missing: {seen:?}");
        }
        while !seen.iter().any(|l| l.starts_with("PRIVMSG NickServ")) {
            assert!(
                Instant::now() < deadline,
                "auto-perform never ran: {seen:?}"
            );
            app.background_tick(&ctx);
            seen.extend(lines.try_iter());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            seen.iter().any(|l| l == "PRIVMSG NickServ :hello"),
            "{seen:?}"
        );
        // The selected window and its unsent draft, and the query.
        let conn = app.sessions[0].id;
        let secret = WindowKey::new(conn, Some("#secret".into()));
        assert_eq!(app.windows.active, Some(secret.clone()));
        assert_eq!(
            app.windows.get(&secret).unwrap().view.input_text,
            "half typed draft"
        );
        assert!(
            app.windows
                .get(&WindowKey::new(conn, Some("carol".into())))
                .is_some()
        );
    }

    #[test]
    fn favorites_without_ids_get_the_same_ids_back_after_a_downgrade() {
        // As 0.1.4 writes them: no `id`, and it drops one whenever it saves.
        let old = r##"{"server_favorites":[
            {"name":"Libera","host":"irc.libera.chat","port":"6697","use_tls":true,
             "password":"","nickname":"","auto_join":"","auto_perform":""},
            {"name":"Libera","host":"irc.libera.chat","port":"6697","use_tls":true,
             "password":"","nickname":"","auto_join":"","auto_perform":""},
            {"name":"OFTC","host":"irc.oftc.net","port":"6697","use_tls":true,
             "password":"","nickname":"","auto_join":"","auto_perform":"","id":"kept"}]}"##;
        let ids = || -> Vec<String> {
            let settings: Settings = serde_json::from_str(old).unwrap();
            App::with_settings(settings, LayoutFile::default())
                .settings
                .server_favorites
                .iter()
                .map(|f| f.id.clone())
                .collect()
        };
        let first = ids();
        assert_eq!(first[0], ids()[0], "derived ids are stable");
        assert_eq!(
            first[0],
            crate::gui::derived_profile_id("Libera", "irc.libera.chat", "6697")
        );
        assert_ne!(first[0], first[1], "a duplicate favorite got the same id");
        assert_eq!(first[2], "kept");
    }

    #[test]
    fn settings_from_0_1_4_load_and_new_fields_default() {
        let old = r##"{"server_favorites":[{"name":"Libera","host":"irc.libera.chat",
            "port":"6697","use_tls":true,"password":"","nickname":"",
            "auto_join":"#rust","auto_perform":""}]}"##;
        let settings: Settings = serde_json::from_str(old).unwrap();
        let favorite = &settings.server_favorites[0];
        assert!(favorite.id.is_empty() && !favorite.auto_connect);
        let app = App::with_settings(settings, LayoutFile::default());
        assert_eq!(app.settings.server_favorites[0].id.len(), 16);
    }

    #[test]
    fn new_settings_file_deserializes_with_the_0_1_4_schema() {
        // The favorite shape 0.1.4 requires: these fields have no serde
        // default there, so they must stay present in every new file.
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct OldFavorite {
            name: String,
            host: String,
            port: String,
            use_tls: bool,
            password: String,
            nickname: String,
            auto_join: String,
            auto_perform: String,
        }
        #[derive(serde::Deserialize)]
        struct OldSettings {
            server_favorites: Vec<OldFavorite>,
        }
        let mut settings = test_settings();
        settings.server_favorites.push(ServerFavorite {
            name: "Libera".into(),
            host: "irc.libera.chat".into(),
            id: "0123456789abcdef".into(),
            auto_connect: true,
            ..ServerFavorite::default()
        });
        let json = serde_json::to_string(&settings).unwrap();
        let old: OldSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(old.server_favorites[0].host, "irc.libera.chat");
    }

    #[test]
    fn detached_window_titles_never_match_the_main_window_title() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        feed(&mut app, 0, ":srv 005 me NETWORK=Linefeed :are supported");
        app.sync_windows();
        for window in &app.windows.windows {
            let title = format!("{} — {}", app.window_title(&window.key), MAIN_WINDOW_TITLE);
            assert_ne!(title, MAIN_WINDOW_TITLE);
        }
    }

    #[test]
    fn a_maximized_frame_draws_the_active_window_and_the_switchbar() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        feed(&mut app, 1, ":bob!u@h PRIVMSG #debian :apt is fine");
        let debian = WindowKey::new(app.sessions[1].id, Some("#debian".into()));
        app.activate(debian);
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        let output = frame(&mut app, &ctx);
        let texts = texts(&output);
        assert!(texts.iter().any(|t| t.contains("apt is fine")), "{texts:?}");
        assert!(
            texts.iter().any(|t| t.contains("#rust")),
            "switchbar missing"
        );
    }

    #[test]
    fn floating_windows_draw_every_attached_window() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        feed(&mut app, 0, ":alice!u@h PRIVMSG #rust :borrowck says no");
        feed(&mut app, 1, ":bob!u@h PRIVMSG #debian :apt is fine");
        app.windows.mode = LayoutMode::Floating;
        app.tile_request = Some(TileRequest::Tile);
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        let output = frame(&mut app, &ctx);
        let texts = texts(&output);
        assert!(
            texts.iter().any(|t| t.contains("borrowck says no")),
            "{texts:?}"
        );
        assert!(texts.iter().any(|t| t.contains("apt is fine")), "{texts:?}");
    }

    #[test]
    fn tiled_windows_keep_their_cells_after_layout_settles() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        feed(
            &mut app,
            0,
            ":carol!u@h PRIVMSG me :private hello from Libera",
        );
        feed(
            &mut app,
            1,
            ":carol!u@h PRIVMSG me :private hello from OFTC",
        );
        feed(&mut app, 0, ":me!u@h JOIN #linux");
        app.windows.mode = LayoutMode::Floating;
        let ctx = egui::Context::default();
        for _ in 0..3 {
            frame(&mut app, &ctx);
        }
        app.tile_request = Some(TileRequest::Tile);
        let mut cells = HashMap::new();
        frame(&mut app, &ctx);
        for window in &app.windows.windows {
            cells.insert(window.key.clone(), window.child_rect.unwrap());
        }
        for _ in 0..5 {
            frame(&mut app, &ctx);
        }
        for (i, window) in app.windows.windows.iter().enumerate() {
            let cell = cells[&window.key];
            let now = window.child_rect.unwrap();
            for other in &app.windows.windows[i + 1..] {
                assert!(
                    !now.shrink(1.0)
                        .intersects(other.child_rect.unwrap().shrink(1.0)),
                    "{:?} overlaps {:?}",
                    window.key,
                    other.key
                );
            }
            assert!(
                (now.width() - cell.width()).abs() < 2.0
                    && (now.height() - cell.height()).abs() < 2.0,
                "{:?} grew from {cell:?} to {now:?}",
                window.key
            );
        }
    }

    #[test]
    fn a_window_opened_by_an_incoming_message_does_not_cover_the_active_one() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        app.windows.mode = LayoutMode::Floating;
        let rust = WindowKey::new(app.sessions[0].id, Some("#rust".into()));
        app.activate(rust.clone());
        let ctx = egui::Context::default();
        for _ in 0..3 {
            frame(&mut app, &ctx);
        }
        feed(&mut app, 0, ":carol!u@h PRIVMSG me :psst");
        for _ in 0..3 {
            frame(&mut app, &ctx);
        }
        let carol = WindowKey::new(app.sessions[0].id, Some("carol".into()));
        let rect = app.windows.get(&carol).unwrap().child_rect.unwrap();
        assert!(
            rect.width() < 1024.0 * 0.7 && rect.height() < 768.0 * 0.7,
            "{rect:?}"
        );
        assert_eq!(app.windows.active, Some(rust.clone()));
        // Where the two overlap, the active window is the one on top.
        let active_rect = app.windows.get(&rust).unwrap().child_rect.unwrap();
        let overlap = rect.intersect(active_rect);
        if overlap.is_positive() {
            assert_eq!(
                ctx.layer_id_at(overlap.center()),
                Some(egui::LayerId::new(
                    egui::Order::Middle,
                    child_window_id(&rust)
                ))
            );
        }
    }

    #[test]
    fn activating_a_window_brings_it_above_the_ones_covering_it() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        app.windows.mode = LayoutMode::Floating;
        let status = WindowKey::new(app.sessions[0].id, None);
        let ctx = egui::Context::default();
        app.activate(WindowKey::new(app.sessions[1].id, None));
        for _ in 0..3 {
            frame(&mut app, &ctx);
        }
        feed(&mut app, 0, ":carol!u@h PRIVMSG me :psst");
        for _ in 0..6 {
            frame(&mut app, &ctx);
        }
        app.activate(status.clone());
        for _ in 0..3 {
            frame(&mut app, &ctx);
        }
        let carol = WindowKey::new(app.sessions[0].id, Some("carol".into()));
        let a = app.windows.get(&status).unwrap().child_rect.unwrap();
        let b = app.windows.get(&carol).unwrap().child_rect.unwrap();
        let overlap = a.intersect(b);
        assert!(overlap.is_positive(), "{a:?} {b:?}");
        assert_eq!(
            ctx.layer_id_at(overlap.center()),
            Some(egui::LayerId::new(
                egui::Order::Middle,
                child_window_id(&status)
            ))
        );
    }

    #[test]
    fn detached_window_renders_in_its_own_viewport_and_hides_with_the_tray() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        feed(&mut app, 1, ":bob!u@h PRIVMSG #debian :apt is fine");
        let rust = WindowKey::new(app.sessions[0].id, Some("#rust".into()));
        let debian = WindowKey::new(app.sessions[1].id, Some("#debian".into()));
        app.activate(rust);
        app.toggle_detached(&debian);
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        let output = frame(&mut app, &ctx);
        // A bare Context embeds viewports as windows, so the detached
        // window's contents appear in this output.
        assert!(texts(&output).iter().any(|t| t.contains("apt is fine")));

        app.tray_hidden = true;
        app.background_tick(&ctx);
        assert!(
            app.spawn_geometry.is_empty(),
            "stale creation geometry kept"
        );
        let output = ctx.run_ui(egui::RawInput::default(), |_ui| app.draw_detached(&ctx));
        assert!(!texts(&output).iter().any(|t| t.contains("apt is fine")));
        assert!(app.spawn_geometry.is_empty());

        app.tray_hidden = false;
        let output = frame(&mut app, &ctx);
        assert!(texts(&output).iter().any(|t| t.contains("apt is fine")));
    }

    #[test]
    fn a_dialog_asked_for_in_a_detached_window_belongs_to_that_window() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let debian = WindowKey::new(app.sessions[1].id, Some("#debian".into()));
        app.toggle_detached(&debian);
        {
            let window = app.windows.get_mut(&debian).unwrap();
            window.view.input_text = "/list".into();
            window.view.input_focus_request = true;
        }
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        frame(&mut app, &ctx);
        assert!(app.windows.get(&debian).unwrap().view.input_had_focus);
        // Enter in the detached window's input line.
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1024.0, 768.0),
                )),
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| app.ui(ui),
        )
        .textures_delta
        .clear();
        let index = app.session_index(debian.conn).unwrap();
        assert!(app.sessions[index].show_channel_list, "/list was not run");
        assert_eq!(app.detached_dialog_owner(debian.conn), Some(debian.clone()));
        // Reattached: the dialogs go back to the main window.
        app.toggle_detached(&debian);
        assert_eq!(app.detached_dialog_owner(debian.conn), None);
    }

    #[test]
    fn a_detached_window_sends_what_is_typed_while_the_main_window_is_minimized() {
        let (mut app, _rx_a, mut rx_b) = two_networks();
        let debian = WindowKey::new(app.sessions[1].id, Some("#debian".into()));
        app.toggle_detached(&debian);
        {
            let window = app.windows.get_mut(&debian).unwrap();
            window.view.input_text = "still here".into();
            window.view.input_focus_request = true;
        }
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        frame(&mut app, &ctx);
        drain(&mut rx_b);
        // The main window is minimized: eframe runs ui passes only for the
        // detached window, after `logic` ticked the connections.
        app.background_tick(&ctx);
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1024.0, 768.0),
                )),
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| app.ui_detached_only(ui.ctx(), true),
        )
        .textures_delta
        .clear();
        assert_eq!(drain(&mut rx_b), ["PRIVMSG #debian :still here"]);
    }

    fn pointer_frame(
        app: &mut App,
        ctx: &egui::Context,
        time: f64,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1024.0, 768.0),
                )),
                time: Some(time),
                events,
                ..Default::default()
            },
            |ui| app.ui(ui),
        );
        output.textures_delta.clear();
        output
    }

    fn primary(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// Floating windows for two networks, drawn a few times; returns the
    /// next frame time.
    fn floating_windows(app: &mut App, ctx: &egui::Context) -> f64 {
        app.windows.mode = LayoutMode::Floating;
        let mut time = 1.0;
        for _ in 0..4 {
            pointer_frame(app, ctx, time, vec![]);
            time += 0.05;
        }
        time
    }

    fn assert_placed(app: &App, key: &WindowKey, expected: egui::Rect) {
        let rect = app.windows.get(key).unwrap().child_rect.unwrap();
        assert!(
            (rect.min - expected.min).length() < 1.0
                && (rect.size() - expected.size()).length() < 1.0,
            "{key:?} at {rect:?}, expected {expected:?}"
        );
    }

    /// Press on `from`, drag through `path` and release on its last point;
    /// returns the next frame time.
    fn drag(
        app: &mut App,
        ctx: &egui::Context,
        mut time: f64,
        from: egui::Pos2,
        path: &[egui::Pos2],
    ) -> f64 {
        let mut step = |app: &mut App, events| {
            pointer_frame(app, ctx, time, events);
            time += 0.05;
        };
        step(app, vec![egui::Event::PointerMoved(from)]);
        step(app, vec![primary(from, true)]);
        for &pos in path {
            step(app, vec![egui::Event::PointerMoved(pos)]);
        }
        step(app, vec![primary(*path.last().unwrap(), false)]);
        step(app, vec![]);
        step(app, vec![]);
        time
    }

    #[test]
    fn resting_on_the_maximize_button_offers_layouts_that_snap_the_window() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let ctx = egui::Context::default();
        let mut time = floating_windows(&mut app, &ctx);
        let key = app.windows.active.clone().unwrap();
        let button = ctx
            .read_response(snap::maximize_button_id(&key))
            .unwrap()
            .rect;
        pointer_frame(
            &mut app,
            &ctx,
            time,
            vec![egui::Event::PointerMoved(button.center())],
        );
        time += 0.1;
        pointer_frame(&mut app, &ctx, time, vec![]);
        assert!(
            app.snap.flyout.is_none(),
            "opened before the pointer rested"
        );
        time += 0.4;
        pointer_frame(&mut app, &ctx, time, vec![]);
        assert!(app.snap.flyout.is_some(), "no layouts after resting");
        time += 0.05;
        pointer_frame(&mut app, &ctx, time, vec![]);

        // Left half of the halves layout.
        let zone = ctx
            .read_response(snap::zone_id(snap::flyout_id(), 0, 0))
            .unwrap()
            .rect;
        for events in [
            vec![egui::Event::PointerMoved(zone.center())],
            vec![primary(zone.center(), true)],
            vec![primary(zone.center(), false)],
            vec![],
            vec![],
        ] {
            time += 0.05;
            pointer_frame(&mut app, &ctx, time, events);
        }
        let area = app.windows.area.unwrap();
        let halves = snap::layout_rects(area, 0);
        assert_placed(&app, &key, halves[0]);
        assert!(app.snap.flyout.is_none());

        // Snap Assist offers the other windows for the right half.
        assert_eq!(app.snap.assist.as_ref().unwrap().zones, vec![halves[1]]);
        let other = app
            .ordered_windows()
            .into_iter()
            .find(|k| *k != key)
            .unwrap();
        let choice = ctx
            .read_response(snap::assist_choice_id(0, &other))
            .unwrap()
            .rect;
        for events in [
            vec![egui::Event::PointerMoved(choice.center())],
            vec![primary(choice.center(), true)],
            vec![primary(choice.center(), false)],
            vec![],
            vec![],
        ] {
            time += 0.05;
            pointer_frame(&mut app, &ctx, time, events);
        }
        assert_placed(&app, &other, halves[1]);
        assert_eq!(app.windows.active, Some(other), "picked, not focused");
        assert!(app.snap.assist.is_none(), "every zone is filled");
    }

    #[test]
    fn dragging_a_caption_to_a_side_snaps_the_window_to_that_half() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let ctx = egui::Context::default();
        let time = floating_windows(&mut app, &ctx);
        let key = app.windows.active.clone().unwrap();
        let caption = ctx.read_response(snap::caption_id(&key)).unwrap().rect;
        let area = app.windows.area.unwrap();
        let from = caption.left_center() + egui::vec2(40.0, 0.0);
        let edge = egui::pos2(area.left() + 2.0, area.center().y);
        drag(
            &mut app,
            &ctx,
            time,
            from,
            &[from + egui::vec2(30.0, 10.0), edge],
        );
        let halves = snap::layout_rects(area, 0);
        assert_placed(&app, &key, halves[0]);
        assert_eq!(app.snap.assist.as_ref().unwrap().zones, vec![halves[1]]);
    }

    #[test]
    fn dragging_a_caption_to_the_top_opens_the_layout_bar() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let ctx = egui::Context::default();
        let mut time = floating_windows(&mut app, &ctx);
        let key = app.windows.active.clone().unwrap();
        let caption = ctx.read_response(snap::caption_id(&key)).unwrap().rect;
        let area = app.windows.area.unwrap();
        let from = caption.left_center() + egui::vec2(40.0, 0.0);
        let top = area.center_top() + egui::vec2(0.0, 2.0);
        for events in [
            vec![egui::Event::PointerMoved(from)],
            vec![primary(from, true)],
            vec![egui::Event::PointerMoved(from + egui::vec2(20.0, 20.0))],
            vec![egui::Event::PointerMoved(top)],
            vec![],
        ] {
            pointer_frame(&mut app, &ctx, time, events);
            time += 0.05;
        }
        assert!(app.snap.bar.is_some(), "no layout bar at the top");
        // Top right quarter.
        let zone = ctx
            .read_response(snap::zone_id(snap::bar_id(), 4, 1))
            .unwrap()
            .rect;
        for events in [
            vec![egui::Event::PointerMoved(zone.center())],
            vec![primary(zone.center(), false)],
            vec![],
            vec![],
        ] {
            pointer_frame(&mut app, &ctx, time, events);
            time += 0.05;
        }
        assert!(app.snap.bar.is_none());
        assert_placed(&app, &key, snap::layout_rects(area, 4)[1]);
        assert_eq!(app.snap.assist.as_ref().unwrap().zones.len(), 3);
    }

    #[test]
    fn a_window_dropped_away_from_the_edges_just_moves() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let ctx = egui::Context::default();
        let time = floating_windows(&mut app, &ctx);
        let key = app.windows.active.clone().unwrap();
        let before = app.windows.get(&key).unwrap().child_rect.unwrap();
        let caption = ctx.read_response(snap::caption_id(&key)).unwrap().rect;
        let from = caption.left_center() + egui::vec2(40.0, 0.0);
        drag(
            &mut app,
            &ctx,
            time,
            from,
            &[from + egui::vec2(20.0, 20.0), from + egui::vec2(60.0, 40.0)],
        );
        let after = app.windows.get(&key).unwrap().child_rect.unwrap();
        assert!(
            ((after.min - before.min) - egui::vec2(60.0, 40.0)).length() < 1.5,
            "{before:?} -> {after:?}"
        );
        assert!(app.snap.assist.is_none());
    }

    #[test]
    fn snapping_a_maximized_window_returns_to_floating_windows() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        assert_eq!(app.windows.mode, LayoutMode::Maximized);
        let key = app.windows.active.clone().unwrap();
        let area = egui::Rect::from_min_size(egui::pos2(0.0, 60.0), egui::vec2(1024.0, 700.0));
        let target = snap::SnapTarget::layout(area, 1, 0);
        app.snap_window(&key, target.clone());
        assert_eq!(app.windows.mode, LayoutMode::Floating);
        assert_eq!(
            app.windows.get(&key).unwrap().child_rect,
            Some(target.rect())
        );
        assert_eq!(
            app.snap.assist.as_ref().unwrap().zones,
            vec![target.zones[1]]
        );
    }

    #[test]
    fn escape_or_a_click_elsewhere_dismisses_snap_assist() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let ctx = egui::Context::default();
        let mut time = floating_windows(&mut app, &ctx);
        let key = app.windows.active.clone().unwrap();
        let area = app.windows.area.unwrap();
        app.snap_window(
            &key,
            snap::SnapTarget::layout(area, snap::LAYOUTS.len() - 1, 0),
        );
        pointer_frame(&mut app, &ctx, time, vec![]);
        time += 0.05;
        assert_eq!(app.snap.assist.as_ref().unwrap().zones.len(), 3);
        let escape = egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        pointer_frame(&mut app, &ctx, time, vec![escape]);
        time += 0.05;
        assert!(app.snap.assist.is_none());

        // A click inside the snapped window, outside the free zones.
        app.snap_window(&key, snap::SnapTarget::layout(area, 0, 0));
        for _ in 0..2 {
            pointer_frame(&mut app, &ctx, time, vec![]);
            time += 0.05;
        }
        assert!(app.snap.assist.is_some());
        let inside = snap::layout_rects(area, 0)[0].center();
        for events in [
            vec![egui::Event::PointerMoved(inside)],
            vec![primary(inside, true)],
            vec![primary(inside, false)],
        ] {
            pointer_frame(&mut app, &ctx, time, events);
            time += 0.05;
        }
        assert!(app.snap.assist.is_none());
    }

    fn button(pos: egui::Pos2, button: egui::PointerButton, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn escape() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// Rest the pointer on `key`'s maximize button until the layouts open;
    /// returns the next frame time.
    fn open_flyout(app: &mut App, ctx: &egui::Context, mut time: f64, key: &WindowKey) -> f64 {
        let at = ctx
            .read_response(snap::maximize_button_id(key))
            .unwrap()
            .rect
            .center();
        pointer_frame(app, ctx, time, vec![egui::Event::PointerMoved(at)]);
        for _ in 0..2 {
            time += 0.4;
            pointer_frame(app, ctx, time, vec![]);
        }
        assert!(app.snap.flyout.is_some(), "no layouts after resting");
        time + 0.05
    }

    #[test]
    fn a_floating_window_still_resizes_from_its_left_edge() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let ctx = egui::Context::default();
        let mut time = floating_windows(&mut app, &ctx);
        let key = app.windows.active.clone().unwrap();
        let placed = egui::Rect::from_min_max(egui::pos2(300.0, 200.0), egui::pos2(700.0, 600.0));
        app.windows.place(&key, placed);
        for _ in 0..3 {
            pointer_frame(&mut app, &ctx, time, vec![]);
            time += 0.05;
        }
        let edge = egui::pos2(placed.left() + 1.0, placed.center().y);
        let to = edge - egui::vec2(60.0, 0.0);
        drag(
            &mut app,
            &ctx,
            time,
            edge,
            &[edge - egui::vec2(20.0, 0.0), to],
        );
        let rect = app.windows.get(&key).unwrap().child_rect.unwrap();
        assert!(
            (rect.left() - (placed.left() - 60.0)).abs() < 3.0,
            "{rect:?}"
        );
        assert!((rect.right() - placed.right()).abs() < 3.0, "{rect:?}");
    }

    #[test]
    fn escape_closes_the_layouts_while_the_pointer_still_rests_on_the_button() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let ctx = egui::Context::default();
        let time = floating_windows(&mut app, &ctx);
        let key = app.windows.active.clone().unwrap();
        let mut time = open_flyout(&mut app, &ctx, time, &key);
        pointer_frame(&mut app, &ctx, time, vec![escape()]);
        for _ in 0..4 {
            time += 0.4;
            pointer_frame(&mut app, &ctx, time, vec![]);
            assert!(app.snap.flyout.is_none(), "reopened while still resting");
        }
    }

    #[test]
    fn clicking_maximize_with_the_layouts_open_closes_them() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let ctx = egui::Context::default();
        let time = floating_windows(&mut app, &ctx);
        let key = app.windows.active.clone().unwrap();
        let mut time = open_flyout(&mut app, &ctx, time, &key);
        let at = ctx
            .read_response(snap::maximize_button_id(&key))
            .unwrap()
            .rect
            .center();
        for events in [vec![primary(at, true)], vec![primary(at, false)], vec![]] {
            pointer_frame(&mut app, &ctx, time, events);
            time += 0.05;
        }
        assert_eq!(app.windows.mode, LayoutMode::Maximized);
        assert!(app.snap.flyout.is_none());
    }

    #[test]
    fn a_snap_made_while_maximized_lands_in_the_current_chat_area() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        // The area floating windows were last laid out in, before the main
        // window was resized.
        app.windows.area = Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 60.0),
            egui::vec2(600.0, 400.0),
        ));
        frame(&mut app, &ctx);
        let area = app.windows.area.unwrap();
        assert!(area.width() > 900.0, "area not followed while maximized");
        let key = app.windows.active.clone().unwrap();
        let target = snap::SnapTarget::layout(area, 0, 0);
        app.snap_window(&key, target.clone());
        for _ in 0..3 {
            frame(&mut app, &ctx);
        }
        assert_placed(&app, &key, target.rect());
    }

    #[test]
    fn a_right_button_drag_does_not_snap() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let ctx = egui::Context::default();
        let mut time = floating_windows(&mut app, &ctx);
        let key = app.windows.active.clone().unwrap();
        let before = app.windows.get(&key).unwrap().child_rect.unwrap();
        let caption = ctx.read_response(snap::caption_id(&key)).unwrap().rect;
        let area = app.windows.area.unwrap();
        let from = caption.left_center() + egui::vec2(40.0, 0.0);
        let edge = egui::pos2(area.left() + 2.0, area.center().y);
        let secondary = egui::PointerButton::Secondary;
        // The drag starts with the pointer already at the edge.
        for events in [
            vec![egui::Event::PointerMoved(from)],
            vec![button(from, secondary, true)],
            vec![egui::Event::PointerMoved(edge)],
            vec![button(edge, secondary, false)],
            vec![escape()],
            vec![],
        ] {
            pointer_frame(&mut app, &ctx, time, events);
            time += 0.05;
        }
        assert_eq!(app.windows.get(&key).unwrap().child_rect.unwrap(), before);
        assert!(app.snap.assist.is_none());
    }

    #[test]
    fn settings_asked_for_in_a_detached_window_open_in_it_while_minimized() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let debian = WindowKey::new(app.sessions[1].id, Some("#debian".into()));
        app.toggle_detached(&debian);
        app.activate(debian.clone());
        {
            let window = app.windows.get_mut(&debian).unwrap();
            window.view.input_text = "/settings".into();
            window.view.input_focus_request = true;
        }
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        frame(&mut app, &ctx);
        // The main window is minimized: `logic` ticks, then a ui pass draws
        // only the detached window.
        let minimized = |app: &mut App, events: Vec<egui::Event>| {
            app.background_tick(&ctx);
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1024.0, 768.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| app.ui_detached_only(ui.ctx(), true),
            );
            output.textures_delta.clear();
            texts(&output)
        };
        let enter = egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        minimized(&mut app, vec![enter]);
        minimized(&mut app, vec![]);
        let shown = minimized(&mut app, vec![]);
        assert!(app.show_settings, "/settings was not run");
        assert_eq!(app.global_dialog_window(), Some(debian));
        assert!(shown.iter().any(|t| t == "Automation"), "{shown:?}");
    }

    #[test]
    fn closing_a_detached_status_window_asks_in_it() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let status = WindowKey::new(app.sessions[1].id, None);
        app.toggle_detached(&status);
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        app.request_close(status.clone());
        frame(&mut app, &ctx);
        assert_eq!(app.confirm_disconnect, Some(status.conn));
        assert!(app.windows.get(&status).unwrap().detached);
        assert_eq!(app.global_dialog_window(), Some(status));
    }

    #[test]
    fn double_clicking_a_caption_maximizes_its_window() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let ctx = egui::Context::default();
        let mut time = floating_windows(&mut app, &ctx);
        let other = app.ordered_windows()[0].clone();
        app.activate(other.clone());
        for _ in 0..2 {
            pointer_frame(&mut app, &ctx, time, vec![]);
            time += 0.05;
        }
        let caption = ctx.read_response(snap::caption_id(&other)).unwrap().rect;
        let at = caption.left_center() + egui::vec2(30.0, 0.0);
        for events in [
            vec![egui::Event::PointerMoved(at)],
            vec![primary(at, true)],
            vec![primary(at, false)],
            vec![primary(at, true)],
            vec![primary(at, false)],
        ] {
            pointer_frame(&mut app, &ctx, time, events);
            time += 0.05;
        }
        assert_eq!(app.windows.mode, LayoutMode::Maximized);
        assert_eq!(app.windows.active, Some(other));
    }

    #[test]
    fn closing_a_detached_channel_window_parts_the_channel() {
        let (mut app, _rx_a, mut rx_b) = two_networks();
        let debian = WindowKey::new(app.sessions[1].id, Some("#debian".into()));
        app.toggle_detached(&debian);
        app.request_close(debian);
        assert_eq!(drain(&mut rx_b), ["PART #debian"]);
    }

    #[test]
    fn unread_does_not_increase_for_a_target_visible_in_a_focused_detached_window() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let debian = WindowKey::new(app.sessions[1].id, Some("#debian".into()));
        app.toggle_detached(&debian);
        app.activate(debian.clone());
        let ctx = egui::Context::default();

        let (tx, rx) = mpsc::channel(8);
        app.sessions[1].msg_rx = Some(rx);
        // Focus is in the detached window.
        app.focused_detached = Some(debian.clone());
        app.focused = true;
        tx.try_send(IrcMessage::parse(":bob!u@h PRIVMSG #debian :seen").unwrap())
            .unwrap();
        app.tick(&ctx, true);
        assert_eq!(app.sessions[1].channels["#debian"].unread, 0);

        // Nothing of ours is focused: the same message is unread.
        app.focused_detached = None;
        app.focused = false;
        tx.try_send(IrcMessage::parse(":bob!u@h PRIVMSG #debian :unseen").unwrap())
            .unwrap();
        app.tick(&ctx, true);
        assert_eq!(app.sessions[1].channels["#debian"].unread, 1);
    }

    fn notifying_app() -> App {
        let mut app = test_app();
        app.focused = false;
        app.settings.notifications_enabled = true;
        app
    }

    #[test]
    fn a_private_message_notification_is_not_muted_by_a_recent_highlight() {
        let mut app = notifying_app();
        app.notify("bob mentioned you in #rust", "hi me", false);
        app.notify("PM from carol", "psst", true);
        assert_eq!(app.dispatched_notifications.len(), 2);
    }

    #[test]
    fn private_messages_inside_the_interval_are_held_and_summarised_not_dropped() {
        let mut app = notifying_app();
        app.notify("PM from carol", "one", true);
        app.notify("PM from dave", "two", true);
        app.notify("PM from erin", "three", true);
        assert_eq!(app.dispatched_notifications.len(), 1);
        // Once the interval has passed, the next tick delivers one summary.
        app.last_forced_notification_time =
            Some(Instant::now() - MIN_NOTIFICATION_INTERVAL - Duration::from_millis(1));
        app.background_tick(&egui::Context::default());
        assert_eq!(app.dispatched_notifications.len(), 2);
        let (title, body) = &app.dispatched_notifications[1];
        assert_eq!(title, "2 new private messages");
        assert_eq!(body, "PM from erin: three");
    }

    fn age_forced_notification_clock(app: &mut App) {
        app.last_forced_notification_time =
            Some(Instant::now() - MIN_NOTIFICATION_INTERVAL - Duration::from_millis(1));
    }

    #[test]
    fn a_highlight_notification_is_not_muted_by_a_recent_private_message() {
        let mut app = notifying_app();
        app.notify("PM from alice", "psst", true);
        app.notify("bob mentioned you in #room", "hey", false);
        assert_eq!(app.dispatched_notifications.len(), 2);
    }

    #[test]
    fn a_single_held_back_private_message_is_delivered_unchanged() {
        let mut app = notifying_app();
        app.notify("PM from alice", "first", true);
        app.notify("PM from bob", "second", true);
        assert_eq!(app.dispatched_notifications.len(), 1);
        age_forced_notification_clock(&mut app);
        app.background_tick(&egui::Context::default());
        assert_eq!(
            app.dispatched_notifications[1],
            ("PM from bob".to_string(), "second".to_string())
        );
        // Delivered once only.
        age_forced_notification_clock(&mut app);
        app.background_tick(&egui::Context::default());
        assert_eq!(app.dispatched_notifications.len(), 2);
    }

    #[test]
    fn a_new_private_message_after_the_interval_dispatches_the_held_one_first() {
        let mut app = notifying_app();
        app.notify("PM from alice", "first", true);
        app.notify("PM from bob", "second", true);
        age_forced_notification_clock(&mut app);
        // No tick ran in between: the held message must not be overtaken.
        app.notify("PM from carol", "third", true);
        let titles: Vec<&str> = app
            .dispatched_notifications
            .iter()
            .map(|(title, _)| title.as_str())
            .collect();
        assert_eq!(titles, ["PM from alice", "PM from bob"]);
        age_forced_notification_clock(&mut app);
        app.background_tick(&egui::Context::default());
        assert_eq!(app.dispatched_notifications[2].0, "PM from carol");
    }

    #[test]
    fn highlight_floods_are_still_rate_limited_and_disabled_notifications_drop_held_ones() {
        let mut app = notifying_app();
        app.notify("bob mentioned you", "a", false);
        app.notify("bob mentioned you", "b", false);
        assert_eq!(app.dispatched_notifications.len(), 1);

        app.notify("PM from carol", "one", true);
        app.notify("PM from carol", "two", true);
        assert!(app.deferred_notification.is_some());
        app.settings.notifications_enabled = false;
        app.last_forced_notification_time =
            Some(Instant::now() - MIN_NOTIFICATION_INTERVAL - Duration::from_millis(1));
        app.background_tick(&egui::Context::default());
        assert_eq!(app.dispatched_notifications.len(), 2);
        assert!(app.deferred_notification.is_none());
    }

    #[test]
    fn a_focused_window_suppresses_highlights_but_not_private_messages() {
        let mut app = notifying_app();
        app.focused = true;
        app.notify("bob mentioned you", "a", false);
        assert!(app.dispatched_notifications.is_empty());
        app.notify("PM from carol", "psst", true);
        assert_eq!(app.dispatched_notifications.len(), 1);
    }

    #[test]
    fn notifications_never_log_message_text() {
        // Skipped notifications (disabled or focused) must not leak the body
        // to the default-level log either (#228).
        let mut app = notifying_app();
        app.settings.notifications_enabled = false;
        let logs = crate::gui::tests::capture_logs(|| {
            app.notify("PM from alice", "meet me at the secret place", true);
        });
        assert!(!logs.contains("secret place"), "{logs}");

        app.settings.notifications_enabled = true;
        app.focused = true;
        let logs = crate::gui::tests::capture_logs(|| {
            app.notify(
                "bob mentioned you in #room",
                "the password is swordfish",
                false,
            );
        });
        assert!(!logs.contains("swordfish"), "{logs}");
    }

    #[test]
    fn notifications_from_a_background_session_reach_the_app() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        app.focused = false;
        app.settings.notifications_enabled = true;
        feed(&mut app, 1, ":carol!u@h PRIVMSG me :psst");
        app.process_events();
        assert_eq!(
            app.dispatched_notifications,
            [("PM from carol".to_string(), "psst".to_string())]
        );
    }

    /// Two real loopback servers registered by two sessions of one app.
    #[test]
    fn two_sessions_register_independently_and_route_messages_to_their_own_buffers() {
        let (port_a, lines_a) = crate::gui::connection::tests::scripted_server();
        let (port_b, lines_b) = crate::gui::connection::tests::scripted_server();
        let mut app = test_app();
        let second = app.add_session();
        for (index, port) in [(0, port_a), (second, port_b)] {
            let session = &mut app.sessions[index];
            session.apply_unprofiled_endpoint("127.0.0.1", &port.to_string(), false);
            session.nickname = "tester".into();
            assert!(session.start_session_from_form());
        }
        // Distinct endpoints (ports), so the duplicate guard stays out of it.
        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app.sessions.iter().all(|s| s.connected) {
            assert!(Instant::now() < deadline, "sessions never registered");
            app.background_tick(&ctx);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.sessions.iter().all(|s| s.worker.is_some()));
        assert!(
            app.sessions[0]
                .server_messages
                .iter()
                .any(|m| m.content.contains("Welcome"))
        );
        assert!(
            app.sessions[1]
                .server_messages
                .iter()
                .any(|m| m.content.contains("Welcome"))
        );

        // Disconnecting one leaves the other connected.
        let conn_b = app.sessions[1].id;
        app.disconnect_and_close(conn_b);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.sessions.len() > 1 {
            assert!(Instant::now() < deadline, "closed session never went away");
            app.background_tick(&ctx);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.sessions[0].connected);
        let b_lines = lines_b.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(b_lines.iter().any(|l| l.starts_with("QUIT")), "{b_lines:?}");

        app.sessions[0].request_manual_disconnect(Some("bye".into()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.sessions[0].worker.is_some() {
            assert!(Instant::now() < deadline);
            app.background_tick(&ctx);
            std::thread::sleep(Duration::from_millis(5));
        }
        let a_lines = lines_a.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(a_lines.iter().any(|l| l == "QUIT :bye"), "{a_lines:?}");
    }
}
