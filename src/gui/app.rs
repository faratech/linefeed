//! The multi-server application: every connection is a `Session`; `App`
//! owns them together with everything that is not about one connection —
//! persisted settings, the updater, notifications, the window manager, and
//! the dialogs that are global (Settings).
//!
//! Window model (mIRC style): every session has a status window plus one
//! window per channel and query. See `windows.rs` for placement; drawing and
//! routing live here. A window draws by temporarily pointing its session's
//! `current_conversation` at the window's target and swapping the window's input
//! line into the session, so the session's command, history and completion
//! code serves every window without knowing about windows.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use egui::{Color32, RichText};

use super::conversations::ConversationId;
use super::snap::CaptionAction;
use super::windows::{
    ConnId, LayoutFile, LayoutMode, PersistKey, PresentWindow, TargetView, WindowKey,
    WindowManager, merge_drafts,
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
    /// Favorite explicitly loaded into the editor. Kept separately from the
    /// credential-scoping form_profile, which endpoint edits must clear.
    pub editing_profile: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WindowShortcut {
    Cycle { reverse: bool },
    Close,
    ToggleMaximized,
    Select(usize),
    Action(super::actions::ActionId),
}

pub struct App {
    pub(crate) uploads: super::uploads::UploadManager,
    pub(crate) workspace: super::workspace::WorkspaceStore,
    pending_workspace_views: Vec<(
        ConnId,
        Option<ConversationId>,
        super::workspace::WorkspaceViewState,
    )>,
    workspace_restores: HashMap<u64, (WindowKey, super::identity::ScrollAnchor, HashSet<String>)>,
    next_workspace_restore: u64,
    pub(crate) archive: super::archive::ArchiveService,
    pub(crate) search: super::search::SearchUi,
    pub(crate) automation: super::automation::AutomationUi,
    pub(crate) window_switcher: super::actions::WindowSwitcher,
    pub(crate) sessions: Vec<Session>,
    pub(crate) activity: super::attention::ActivityState,
    pub(crate) ui_ctx: Option<egui::Context>,
    attention_notification: Option<super::attention::AttentionEvent>,
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
    /// Full button and clip rectangles for viewport acceptance tests.
    #[cfg(test)]
    pub(crate) settings_action_rects: HashMap<&'static str, (egui::Rect, egui::Rect)>,
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
    /// Each session's `current_conversation` as of last frame, to follow focus
    /// changes the session made itself (own JOIN, /query).
    last_current: HashMap<ConnId, Option<ConversationId>>,
    /// Geometry a detached viewport was created with; kept constant while it
    /// is open so the builder never fights the user moving it.
    spawn_geometry: HashMap<WindowKey, (Option<egui::Pos2>, Option<egui::Vec2>)>,
    /// The window whose formatting bar opened the colour picker.
    color_target: Option<WindowKey>,
    layout_dirty_since: Option<Instant>,
    /// The window set reported to `WindowManager::sync`, rebuilt only when
    /// its fingerprint changes (tabs opened/closed/renamed, casemapping or
    /// network identity changes): rebuilding it canonicalizes and clones
    /// every tab name, which is pure churn on a steady-state frame.
    present_cache: Vec<PresentWindow>,
    present_fingerprint: Option<u64>,
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
    pending_inputs: Vec<(ConnId, Option<ConversationId>, String)>,
    /// When the App started: the persisted active window is only restored
    /// while startup auto-joins are still arriving.
    started: Instant,
    /// The user has picked a window (click, switchbar, shortcut, typing a
    /// /query): the persisted active window no longer applies.
    user_activated: bool,
    /// The window to return to if a "New connection" status window is
    /// dismissed without connecting.
    focus_before_new: Option<WindowKey>,
    /// Only explicitly canceled pristine sessions may be discarded. A saved
    /// idle connection must not disappear when a different editor is canceled.
    cancelled_new_sessions: HashSet<ConnId>,
    /// The session the open Settings window edits, fixed when it opened so
    /// switching windows meanwhile cannot move its edits to another network.
    pub(crate) settings_session: Option<ConnId>,
    /// Connection form at editor opening, so Cancel restores this session's
    /// profile and credentials rather than another network's defaults.
    pub(crate) settings_form_before: Option<(ConnId, Settings, Option<String>)>,
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
    pub(crate) fn queue_attention_notification(
        &mut self,
        event: super::attention::AttentionEvent,
        title: &str,
        body: &str,
        force: bool,
    ) {
        self.attention_notification = Some(event);
        self.notify(title, body, force);
        self.attention_notification = None;
    }
    pub(crate) fn redact_activity(&mut self, reference: &super::identity::MessageRef) {
        let matches = |candidate: &super::identity::MessageRef| {
            candidate.conversation.network == reference.conversation.network
                && (candidate
                    .archive_id
                    .as_ref()
                    .is_some_and(|id| reference.archive_id.as_ref() == Some(id))
                    || (candidate.conversation == reference.conversation
                        && candidate
                            .server_msgid
                            .as_ref()
                            .is_some_and(|id| reference.server_msgid.as_ref() == Some(id))))
        };
        for item in &mut self.activity.items {
            if matches(&item.reference) {
                item.preview = "<message redacted>".into();
            }
        }
        if let Some(held) = &mut self.deferred_notification {
            let before = held.contexts.len();
            held.contexts.retain(|context| {
                context
                    .as_ref()
                    .is_none_or(|(event, _, _)| !matches(&event.reference))
            });
            if held.contexts.len() != before {
                held.count = held.contexts.len();
                if let Some(Some((_, title, body))) = held.contexts.last() {
                    held.title = title.clone();
                    held.body = body.clone();
                } else {
                    held.title.clear();
                    held.body.clear();
                }
            }
        }
        if self
            .deferred_notification
            .as_ref()
            .is_some_and(|held| held.count == 0)
        {
            self.deferred_notification = None;
        }
    }
    pub(crate) fn begin_reply(
        &mut self,
        reference: super::identity::MessageRef,
    ) -> Result<(), String> {
        if reference
            .server_msgid
            .as_ref()
            .is_none_or(|id| id.is_empty())
        {
            return Err("This message has no server ID and cannot be replied to".into());
        }
        let index = self
            .sessions
            .iter()
            .position(|session| {
                session.is_connected()
                    && session.logical_network_key().as_ref()
                        == Some(&reference.conversation.network)
            })
            .ok_or("Connect to this network before replying")?;
        if !self.sessions[index].enabled_caps.contains("message-tags") {
            return Err("This server does not support message replies".into());
        }
        let conversation = self.sessions[index]
            .id_for_target(&reference.conversation.target)
            .ok_or("Open this conversation before replying")?;
        if self.sessions[index].is_channel_name(&reference.conversation.target)
            && !self.sessions[index]
                .conversation_channel(conversation)
                .is_some_and(|channel| channel.joined)
        {
            return Err("Join this channel before replying".into());
        }
        let key = WindowKey::new(self.sessions[index].id, Some(conversation));
        if self.windows.get(&key).is_none() {
            return Err("Open this conversation before replying".into());
        }
        self.user_activate(key);
        if let Some(window) = self.windows.get_mut(&key) {
            window.view.composer.reply_to = Some(reference);
            window.view.input_focus_request = true;
        }
        if let Some(ctx) = &self.ui_ctx {
            self.focus_active_viewport(ctx);
            ctx.request_repaint();
        }
        Ok(())
    }
    pub(crate) fn navigate_message(&mut self, reference: super::identity::MessageRef) {
        let found = self
            .sessions
            .iter()
            .enumerate()
            .find_map(|(index, session)| {
                if session.logical_network_key().as_ref() != Some(&reference.conversation.network) {
                    return None;
                }
                let id = session.id_for_target(&reference.conversation.target);
                let found = id.and_then(|id| {
                    session.conversation_channel(id).and_then(|channel| {
                        channel
                            .messages
                            .iter()
                            .find(|message| {
                                reference.archive_id.as_deref()
                                    == Some(message.message_key.as_str())
                                    || reference
                                        .server_msgid
                                        .as_ref()
                                        .is_some_and(|id| message.msgid.as_ref() == Some(id))
                            })
                            .map(|message| (id, message.message_key.clone()))
                    })
                });
                let found = found.or_else(|| {
                    reference.archive_id.as_ref().and_then(|archive_id| {
                        session.conversations.iter().find_map(|(target, channel)| {
                            channel
                                .messages
                                .iter()
                                .find(|message| &message.message_key == archive_id)
                                .and_then(|message| {
                                    session
                                        .id_for_target(target)
                                        .map(|id| (id, message.message_key.clone()))
                                })
                        })
                    })
                });
                found.map(|(id, message)| (index, id, message))
            });
        if let Some((index, id, message)) = found {
            let key = WindowKey::new(self.sessions[index].id, Some(id));
            self.user_activate(key);
            if let Some(window) = self.windows.get_mut(&key) {
                let timeline = window.view.timeline.get_mut();
                timeline.reveal = Some(message);
                timeline.restore_pending = false;
                timeline.restored_hold = false;
                timeline.hydrating = false;
            }
            if let Some(ctx) = &self.ui_ctx {
                self.focus_active_viewport(ctx);
                ctx.request_repaint();
            }
        } else {
            self.open_archive_context(reference);
        }
    }
    /// The production constructor: persisted settings and layout.
    pub fn load() -> Self {
        // An unreadable file is quarantined to a unique backup (or, when that
        // fails or the file cannot be read at all, not saved over).
        let (settings, writable) = Settings::load_checked();
        let needs_ids = settings.server_favorites.iter().any(|f| f.id.is_empty());
        let mut app = Self::with_settings(settings, LayoutFile::load());
        app.workspace = super::workspace::WorkspaceStore::open_default();
        if let Some(directory) = dirs::config_dir() {
            app.automation = super::automation::AutomationUi::load_from(
                directory.join("linefeed/automation.json"),
            );
        }
        app.persist_layout = true;
        app.settings_writable = writable;
        #[cfg(not(test))]
        app.install_archive();
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
                } else if layout.auto_connect.contains(&favorite.id) {
                    // Rewritten by an older version, which dropped the tick
                    // with the id.
                    favorite.auto_connect = true;
                }
            }
            ids.insert(favorite.id.clone());
        }
        ChatMessage::configure_default_timestamp_format(&settings.timestamp_format);
        Self {
            workspace: super::workspace::WorkspaceStore::default(),
            pending_workspace_views: Vec::new(),
            workspace_restores: HashMap::new(),
            next_workspace_restore: 1,
            automation: super::automation::AutomationUi::default(),
            window_switcher: super::actions::WindowSwitcher::default(),
            sessions: Vec::new(),
            activity: Default::default(),
            ui_ctx: None,
            attention_notification: None,
            archive: Default::default(),
            uploads: Default::default(),
            search: Default::default(),
            next_conn: 1,
            settings,
            updater: crate::updater::Controller::default(),
            windows: WindowManager::from_layout(layout),
            favorites_ui: FavoritesUi::default(),
            show_settings: false,
            settings_tab: 0,
            #[cfg(test)]
            settings_action_rects: HashMap::new(),
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
            present_cache: Vec::new(),
            present_fingerprint: None,
            persist_layout: false,
            settings_writable: true,
            tray_hidden: false,
            tile_request: None,
            pending_inputs: Vec::new(),
            started: Instant::now(),
            user_activated: false,
            focus_before_new: None,
            cancelled_new_sessions: HashSet::new(),
            settings_session: None,
            settings_form_before: None,
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
            self.workspace.restored = None;
        } else if self.settings.resume_workspace
            && let Some(snapshot) = self.workspace.restored.take()
        {
            self.restore_workspace(snapshot);
        }
        {
            let auto: Vec<ServerFavorite> = self
                .settings
                .server_favorites
                .iter()
                .filter(|f| f.auto_connect)
                .cloned()
                .collect();
            for favorite in auto {
                let mut candidate = Session::with_settings(self.settings.clone());
                candidate.load_favorite(&favorite);
                if let Ok(config) = candidate.session_from_form()
                    && self.sessions.iter().any(|session| {
                        session.logical_network_key().as_ref()
                            == Some(&config.logical_network_key())
                    })
                {
                    continue;
                }
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
        session.set_automation(self.automation.config.clone());
        session.id = self.next_conn;
        session.archive_client = self
            .archive
            .client
            .is_enabled()
            .then(|| self.archive.client.clone());
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

    /// Network and application background work both need a visible-window
    /// heartbeat: those workers can finish without producing a native event.
    pub fn wants_polling(&self) -> bool {
        self.sessions.iter().any(|s| {
            s.is_connected() || s.is_connecting() || s.awaiting_reconnect() || s.has_worker()
        }) || !self.workspace_restores.is_empty()
            || (self.settings.resume_workspace && self.workspace.is_enabled())
            || self.search.has_pending_work()
            || self.uploads.has_pending_work()
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
        if session.membership_managed_by_bouncer()
            && let Some(network) = session.logical_network_key()
        {
            return network.storage_key();
        }
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
        self.sessions[index].commit_upload_settings();
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

    /// What layout.json holds: the window layout, and the copy of which
    /// favorites connect at startup.
    pub(crate) fn layout_file(&self) -> LayoutFile {
        let mut layout = self.windows.layout();
        layout.auto_connect = self
            .settings
            .server_favorites
            .iter()
            .filter(|f| f.auto_connect)
            .map(|f| super::derived_profile_id(&f.name, &f.host, &f.port))
            .collect();
        layout
    }

    fn store_settings(&mut self) {
        // layout.json keeps a copy of the auto-connect ticks.
        self.windows.dirty = true;
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
        self.ui_ctx = Some(root_ui.ctx().clone());
        let ctx = root_ui.ctx().clone();
        self.observe_root_input(&ctx);
        self.tick(&ctx, true);
        self.handle_shortcuts(&ctx);
        self.show_dialogs(&ctx);
        self.show_window_switcher(&ctx);

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
            .map(|w| w.key)
            .collect()
    }

    /// The main window is hidden, minimized or covered: drain every session
    /// so the buffers cannot fill, keep timers and lifecycles running.
    /// Unless the main window went to the tray, detached windows are still
    /// on screen, and one of them may have focus.
    pub fn background_tick(&mut self, ctx: &egui::Context) {
        self.ui_ctx = Some(ctx.clone());
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
        // Embedded viewport fallback shares root input. Keyboard events still
        // belong to the child composer which owns egui's focus.
        let embedded_child_keyboard = self.windows.windows.iter().any(|window| {
            window.detached
                && window.view.input_had_focus
                && ctx.memory(|memory| {
                    memory.has_focus(egui::Id::unique((
                        "composer",
                        window.key.conn,
                        window.key.conversation,
                    )))
                })
        });
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
            && (!embedded_child_keyboard || ctx.input(|input| input.pointer.any_pressed()))
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
            let active_view = self
                .windows
                .active
                .filter(|key| key.conn == session.id)
                .and_then(|key| self.windows.get(&key));
            let reading = active_view
                .filter(|window| {
                    !window.view.timeline.borrow().restore_pending
                        && !window.view.timeline.borrow().restored_hold
                        && window
                            .view
                            .timeline
                            .borrow()
                            .anchor
                            .as_ref()
                            .is_some_and(|anchor| anchor.at_bottom)
                })
                .and_then(|window| window.key.conversation);
            session.reading_conversation = reading;
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
        self.tick_history_services();
        self.tick_uploads();
        self.run_lifecycles(ctx);
        self.process_events();
        self.flush_deferred_notification();
        self.sync_windows();
        self.checkpoint_workspace();
        self.route_connection_editor();
        self.update_global_dialog_owner();
    }

    fn run_lifecycles(&mut self, ctx: &egui::Context) {
        for index in 0..self.sessions.len() {
            // One live session per endpoint: two would share log files and
            // nick state on the same network. Sessions earlier in this pass
            // have already started their workers, so the first one wins.
            let id = self.sessions[index].id;
            self.sessions[index].blocked_networks = self
                .sessions
                .iter()
                .filter(|session| {
                    session.id != id
                        && (session.is_connected()
                            || session.is_connecting()
                            || session.has_worker()
                            || session.has_pending_connect()
                            || session.awaiting_reconnect())
                })
                .flat_map(|session| {
                    session
                        .active_session
                        .iter()
                        .chain(session.connection.staged_config())
                        .map(|config| config.logical_network_key())
                })
                .collect();
            let blocked: Vec<_> = self
                .sessions
                .iter()
                .filter(|s| {
                    s.id != id && (s.is_connected() || s.has_worker() || s.awaiting_reconnect())
                })
                .filter_map(|s| s.active_endpoint_key())
                .collect();
            self.sessions[index].blocked_endpoints = if self.sessions[index].bouncer.is_none() {
                blocked
            } else {
                Vec::new()
            };
            self.sessions[index].process_connection_lifecycle(ctx);
        }

        // Drop sessions whose status window was closed once they are gone.
        let closing = &self.closing_sessions;
        let before = self.sessions.len();
        let mut removed = Vec::new();
        self.sessions.retain(|s| {
            let done = closing.contains(&s.id) && s.is_fully_stopped();
            if done {
                removed.push(s.id);
            }
            !done
        });
        for id in removed {
            self.closing_sessions.remove(&id);
            self.last_current.remove(&id);
            if self.settings_session == Some(id) {
                self.show_settings = false;
                self.settings_session = None;
                self.settings_form_before = None;
                self.favorites_ui = FavoritesUi::default();
            }
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
                    SessionEvent::SearchRequested {
                        query,
                        all_networks,
                        conversation,
                    } => {
                        let before = self.sessions[index].current_conversation;
                        self.sessions[index].current_conversation = conversation;
                        self.open_search(index, query, all_networks);
                        self.sessions[index].current_conversation = before;
                    }
                    SessionEvent::Notify {
                        title,
                        body,
                        force,
                        attention,
                    } => {
                        if let Some(event) = attention {
                            self.handle_attention(event, &title, &body, force);
                        } else {
                            self.notify(&title, &body, force);
                        }
                    }
                    SessionEvent::RevealMessage(reference) => {
                        self.claim_global_dialogs(Some(WindowKey::new(
                            self.sessions[index].id,
                            self.sessions[index].current_conversation,
                        )));
                        self.navigate_message(reference);
                    }
                    SessionEvent::MessageRedacted(reference) => self.redact_activity(&reference),
                    SessionEvent::OpenSettings => {
                        let conn = self.sessions[index].id;
                        self.open_settings(conn);
                    }
                    SessionEvent::UploadRequested {
                        conversation,
                        source,
                        composer_nonce,
                    } => {
                        self.handle_upload_request(index, conversation, composer_nonce, source);
                    }
                    SessionEvent::UploadTokenReady { job_id, token } => {
                        self.accept_upload_token(&job_id, token)
                    }
                    SessionEvent::SaveConfig => {
                        self.sessions[index].export_config(&mut self.settings);
                        self.save_settings();
                    }
                    SessionEvent::SaveAutoPerform => self.save_auto_perform_from(index),
                    SessionEvent::ConversationRenamed { id, old, new } => {
                        self.rename_target_metadata(self.sessions[index].id, id, &old, &new);
                    }
                    SessionEvent::ConversationMerged { survivor, retired } => {
                        self.merge_conversation_windows(self.sessions[index].id, survivor, retired);
                    }
                    SessionEvent::EndpointChanged => {
                        let conn = self.sessions[index].id;
                        if let Some(status) = self.windows.get_mut(&WindowKey::new(conn, None)) {
                            status.view = TargetView::default();
                        }
                        self.pending_inputs.retain(|(owner, _, _)| *owner != conn);
                        for key in [
                            &mut self.main_window,
                            &mut self.focused_detached,
                            &mut self.global_dialog_owner,
                            &mut self.pending_focus,
                            &mut self.focus_before_new,
                            &mut self.restore_after_update,
                        ] {
                            if key.is_some_and(|key| key.conn == conn && key.conversation.is_some())
                            {
                                *key = None;
                            }
                        }
                        if self.color_target.is_some_and(|key| key.conn == conn) {
                            self.color_target = None;
                        }
                        self.dialog_owner.remove(&conn);
                        self.spawn_geometry
                            .retain(|key, _| key.conn != conn || key.conversation.is_none());
                        self.last_current.insert(conn, None);
                        self.sessions[index].show_color_picker = false;
                        self.sessions[index].show_channel_info = false;
                        self.sessions[index].channel_info_target = None;
                        self.sessions[index].show_channel_list = false;
                        self.snap.forget_connection(conn);
                        self.present_fingerprint = None;
                    }
                    SessionEvent::DuplicateEndpoint => self.focus_duplicate_of(index),
                    SessionEvent::NewBouncerNetwork(config) => {
                        let config = *config;
                        let network = config.logical_network_key();
                        if let Some(session) = self.sessions.iter().find(|session| {
                            session.logical_network_key().as_ref() == Some(&network)
                        }) {
                            let key = WindowKey::new(session.id, None);
                            self.activate(key);
                        } else {
                            let added = self.add_session();
                            self.sessions[added].activate_session(config.clone());
                            self.sessions[added].load_connection_form(&config);
                            self.sessions[added].show_connect_dialog = false;
                            self.sessions[added].queue_current_connection(0);
                            self.activate(WindowKey::new(self.sessions[added].id, None));
                        }
                    }
                    SessionEvent::NewServer {
                        host,
                        port,
                        use_tls,
                    } => self.open_server(&host, &port, use_tls),
                    SessionEvent::Window {
                        command,
                        conversation,
                    } => {
                        let key = WindowKey::new(self.sessions[index].id, conversation);
                        self.window_command(command, key);
                    }
                }
            }
        }
    }

    /// Names are metadata; the window and native viewport keep the same ID.
    fn rename_target_metadata(&mut self, conn: ConnId, id: ConversationId, old: &str, new: &str) {
        let Some(index) = self.session_index(conn) else {
            return;
        };
        let old_key = self.sessions[index].conversation_key(old);
        let new_key = self.sessions[index].conversation_key(new);
        self.rename_activity_conversation(&old_key, &new_key);
        if let Some(window) = self.windows.get_mut(&WindowKey::new(conn, Some(id)))
            && let Some(reply) = &mut window.view.composer.reply_to
            && reply.conversation == old_key
        {
            reply.conversation = new_key.clone();
        }
        // Every session mirrors the rules (its copy gates All and highlight
        // alerts and is exported back over the canonical one), so migrate
        // them all; a rule for another network never matches.
        self.settings
            .attention
            .rename_conversation(&old_key, &new_key);
        for session in &mut self.sessions {
            session.attention.rename_conversation(&old_key, &new_key);
        }
        let network = Self::network_identity(&self.sessions[index]);
        let old_persist = PersistKey {
            network: network.clone(),
            target: Some(self.sessions[index].network_support.canonicalize(old)),
        };
        let new_persist = PersistKey {
            network,
            target: Some(self.sessions[index].network_support.canonicalize(new)),
        };
        self.windows
            .rename_metadata(&WindowKey::new(conn, Some(id)), &old_persist, new_persist);
        self.present_fingerprint = None;
    }

    /// Only a real collision retires a runtime ID. Source placement wins, and
    /// both conversations' pending and materialized drafts survive the merge.
    fn merge_conversation_windows(
        &mut self,
        conn: ConnId,
        survivor: ConversationId,
        retired: ConversationId,
    ) {
        let old_key = WindowKey::new(conn, Some(retired));
        let new_key = WindowKey::new(conn, Some(survivor));
        self.reconcile_activity_conversation(conn, survivor);
        let effective_draft = |key: &WindowKey| {
            self.pending_inputs
                .iter()
                .rev()
                .find(|(owner, id, _)| *owner == conn && *id == key.conversation)
                .map(|(_, _, text)| text.clone())
                .or_else(|| self.windows.get(key).map(|w| w.view.input_text.clone()))
                .unwrap_or_default()
        };
        let draft = merge_drafts(&effective_draft(&new_key), &effective_draft(&old_key));
        let had_source = self.windows.get(&new_key).is_some();
        let had_pending = self.pending_inputs.iter().any(|(owner, id, _)| {
            *owner == conn && (*id == old_key.conversation || *id == new_key.conversation)
        });
        self.pending_inputs.retain(|(owner, id, _)| {
            *owner != conn || (*id != old_key.conversation && *id != new_key.conversation)
        });
        self.windows.merge_conversations(new_key, old_key);
        if let Some(window) = self.windows.get_mut(&new_key) {
            window.view.input_text = draft;
        } else if had_pending || !draft.is_empty() {
            self.pending_inputs.push((conn, Some(survivor), draft));
        }
        for key in [
            &mut self.main_window,
            &mut self.focused_detached,
            &mut self.color_target,
            &mut self.global_dialog_owner,
            &mut self.pending_focus,
            &mut self.focus_before_new,
            &mut self.restore_after_update,
        ] {
            if *key == Some(old_key) {
                *key = Some(new_key);
            }
        }
        if self
            .main_window
            .as_ref()
            .is_some_and(|key| self.windows.get(key).is_some_and(|w| w.detached))
        {
            self.main_window = None;
        }
        if self
            .focused_detached
            .as_ref()
            .is_some_and(|key| self.windows.get(key).is_none_or(|w| !w.detached))
        {
            self.focused_detached = None;
        }
        for key in self.dialog_owner.values_mut() {
            if *key == old_key {
                *key = new_key;
            }
        }
        if let Some(current) = self.last_current.get_mut(&conn)
            && *current == Some(retired)
        {
            *current = Some(survivor);
        }
        self.snap.merge_window(new_key, old_key);
        self.spawn_geometry.remove(&old_key);
        if had_source && let Some(window) = self.windows.get(&new_key) {
            if window.detached {
                self.spawn_geometry
                    .insert(new_key, (window.os_pos, window.os_size));
            } else {
                self.spawn_geometry.remove(&new_key);
            }
        }
        self.present_fingerprint = None;
    }

    /// Persist `/perform` edits where they belong. Auto-perform is part of a
    /// server profile (it typically identifies to that network's services):
    /// it goes to the favorite of the live connection, if that favorite is
    /// still for the live endpoint, or to the Quick Connect defaults only
    /// when those are for this same server. Never to another network's
    /// profile or defaults, whatever the connect form currently shows (#367).
    fn save_auto_perform_from(&mut self, index: usize) {
        let save = self.sessions[index].auto_perform_save();
        let endpoint = (&save.endpoint.0, &save.endpoint.1, save.endpoint.2);
        let favorite = save.profile.as_ref().and_then(|id| {
            self.settings.server_favorites.iter_mut().find(|favorite| {
                favorite.id == *id
                    && (!save.live
                        || same_endpoint(
                            (&favorite.host, &favorite.port, favorite.use_tls),
                            (endpoint.0, endpoint.1, endpoint.2),
                        ))
            })
        });
        if let Some(favorite) = favorite {
            favorite.auto_perform = save.list;
            self.save_settings();
        } else if same_endpoint(
            (endpoint.0, endpoint.1, endpoint.2),
            (
                &self.settings.server_host,
                &self.settings.server_port,
                self.settings.use_tls,
            ),
        ) {
            self.settings.auto_perform = save.list;
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
        let Some(endpoint) = self.sessions[index].logical_network_key() else {
            return;
        };
        let id = self.sessions[index].id;
        if let Some(other) = self
            .sessions
            .iter()
            .find(|s| s.id != id && s.logical_network_key().as_ref() == Some(&endpoint))
        {
            let key = WindowKey::new(other.id, None);
            self.activate(key);
        }
    }

    /// `/window ...` typed in the window `key`.
    pub(crate) fn window_command(&mut self, command: WindowCommand, key: WindowKey) {
        let Some(index) = self.session_index(key.conn) else {
            return;
        };
        if key
            .conversation
            .is_some_and(|id| self.sessions[index].target_name(id).is_none())
        {
            return;
        }
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
        // An editor still open for an earlier new connection is canceled (and
        // that window discarded) first, so focus returns to a window that
        // stays.
        self.retire_open_editor(None);
        self.focus_before_new = self.windows.active;
        let index = self.add_session();
        self.sessions[index].show_connect_dialog = true;
        let key = WindowKey::new(self.sessions[index].id, None);
        self.user_activate(key);
        self.open_connection_settings(self.sessions[index].id);
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
        if self.sessions[index].show_connect_dialog {
            self.open_connection_settings(conn);
        }
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
                let context = self
                    .attention_notification
                    .clone()
                    .map(|event| (event, title.to_string(), body.to_string()));
                if held.contexts.len() < 1000 {
                    held.contexts.push(context);
                } else if let Some(last) = held.contexts.last_mut() {
                    *last = context;
                    held.count = held.contexts.len();
                }
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
        let Some(mut held) = self.deferred_notification.take() else {
            return;
        };
        if held.contexts.len() == held.count {
            held.contexts.retain(|context| {
                context
                    .as_ref()
                    .is_none_or(|(event, _, _)| self.settings.attention.eligible(event))
            });
            held.count = held.contexts.len();
            if held.count == 0 {
                return;
            }
            if let Some(Some((_, title, body))) = held.contexts.last() {
                held.title = title.clone();
                held.body = body.clone();
            }
        }
        let prior = self.attention_notification.take();
        self.attention_notification = held
            .contexts
            .iter()
            .rev()
            .find_map(|context| context.as_ref().map(|(event, _, _)| event.clone()));
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
        self.attention_notification = prior;
    }

    /// Hand a notification that passed the policy to the OS. Tests record it
    /// instead of spawning helpers.
    fn dispatch_notification(&mut self, title: &str, body: &str) {
        if self
            .attention_notification
            .as_ref()
            .is_some_and(|event| self.settings.attention.sound_for(event))
        {
            super::message_ui::play_notification_sound();
        }
        #[cfg(test)]
        self.dispatched_notifications
            .push((title.to_string(), body.to_string()));
        #[cfg(not(test))]
        super::show_desktop_notification(title, body);
    }

    /// Reconcile windows with the sessions' tabs and follow focus changes a
    /// session made itself (own JOIN of a requested channel, /query).
    /// Allocation-free fingerprint of everything the reported window set
    /// is built from: the sessions, their network identities, the tab names
    /// and the casemapping that canonicalizes them into persist keys.
    /// Order-independent, since channel iteration order is not stable.
    fn window_set_fingerprint(&self) -> u64 {
        fn fnv(hash: u64, bytes: &[u8]) -> u64 {
            bytes.iter().fold(hash, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
            })
        }
        let mut total = fnv(0xcbf2_9ce4_8422_2325, &self.sessions.len().to_le_bytes());
        for session in &self.sessions {
            let identity = Self::network_identity(session);
            let mut session_hash = fnv(0xcbf2_9ce4_8422_2325, &session.id.to_le_bytes());
            session_hash = fnv(session_hash, identity.as_bytes());
            session_hash = fnv(
                session_hash,
                session.network_support.channel_types.as_bytes(),
            );
            let mapping = match session.network_support.case_mapping {
                super::CaseMapping::Ascii => 0u8,
                super::CaseMapping::StrictRfc1459 => 1,
                super::CaseMapping::Rfc1459 => 2,
                super::CaseMapping::Utf8Rfc8265 => 3,
            };
            session_hash = fnv(session_hash, &[mapping]);
            session_hash = fnv(
                session_hash,
                &session.conversations.revision().to_le_bytes(),
            );
            total = total.wrapping_add(session_hash);
        }
        total
    }

    fn sync_windows(&mut self) {
        // Rebuild the reported window set only when it can have changed;
        // the fingerprint is allocation-free, the rebuild is not.
        let fingerprint = self.window_set_fingerprint();
        if self.present_fingerprint != Some(fingerprint) {
            self.present_fingerprint = Some(fingerprint);
            self.present_cache.clear();
            for session in &self.sessions {
                let network = Self::network_identity(session);
                self.present_cache.push(PresentWindow {
                    key: WindowKey::new(session.id, None),
                    persist: PersistKey {
                        network: network.clone(),
                        target: None,
                    },
                });
                for id in session.conversation_ids() {
                    let name = session.target_name(id).expect("live conversation");
                    self.present_cache.push(PresentWindow {
                        key: WindowKey::new(session.id, Some(id)),
                        persist: PersistKey {
                            network: network.clone(),
                            target: Some(session.network_support.canonicalize(name)),
                        },
                    });
                }
            }
        }
        let previous_active = self.windows.active;
        // Unchanged sets reconcile inside `sync` to nothing, so this costs
        // a slice comparison on steady-state frames.
        let appeared = self.windows.sync(&self.present_cache);
        if !appeared.is_empty() {
            self.windows.dirty = true;
        }
        if let Some(key) = &self.pending_focus
            && let Some(window) = self.windows.get_mut(key)
        {
            window.view.input_focus_request = true;
            self.pending_focus = None;
        }
        if self.pending_focus.is_some_and(|key| {
            self.session_index(key.conn).is_none_or(|index| {
                key.conversation
                    .is_some_and(|id| self.sessions[index].target_name(id).is_none())
            })
        }) {
            self.pending_focus = None;
        }
        let sessions = &self.sessions;
        let windows = &mut self.windows;
        self.pending_inputs.retain(|(conn, target, input)| {
            if !sessions
                .iter()
                .any(|s| s.id == *conn && target.is_none_or(|id| s.target_name(id).is_some()))
            {
                return false;
            }
            // The draft belongs to the window it was captured from, never
            // to whatever window the session happens to select by now.
            let key = WindowKey::new(*conn, *target);
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
            let current = session.current_conversation;
            // A session seen for the first time only records its selection:
            // which window starts active is decided below, not by whichever
            // session happened to be created last.
            let previous = self.last_current.insert(session.id, current);
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
            let key = WindowKey::new(previous.conn, self.sessions[index].current_conversation);
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
                    .map(|first| WindowKey::new(first.id, first.current_conversation))
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
        self.window_switcher.recent.retain(|old| *old != key);
        self.window_switcher.recent.insert(0, key);
        self.window_switcher.recent.truncate(256);
        self.user_activated = true;
        self.activate(key);
    }

    pub(crate) fn activate(&mut self, key: WindowKey) {
        if let Some(previous) = self.windows.active.filter(|previous| *previous != key)
            && let Some(window) = self.windows.get_mut(&previous)
        {
            let timeline = window.view.timeline.get_mut();
            timeline.entry_read_boundary = timeline.last_read.clone();
            timeline.caught_up_tail = None;
        }
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
        if key
            .conversation
            .is_some_and(|id| self.sessions[index].target_name(id).is_none())
        {
            return;
        }
        self.sessions[index].select_conversation(key.conversation);
        // The status window has no read-position report of its own, so
        // showing it in a focused session is what marks its lines seen.
        if key.conversation.is_none() && self.sessions[index].window_focused {
            self.sessions[index].server_unread = 0;
        }
        self.last_current
            .insert(key.conn, self.sessions[index].current_conversation);
        if self.windows.get(&key).is_some_and(|w| !w.detached) {
            self.main_window = Some(key);
        }
        if carry_focus {
            match self.windows.get_mut(&key) {
                Some(window) => window.view.input_focus_request = true,
                None => self.pending_focus = Some(key),
            }
        }
        self.windows.active = Some(key);
        self.windows.dirty = true;
    }

    fn finish_frame(&mut self) {
        if self
            .activity
            .owner
            .is_some_and(|key| self.windows.get(&key).is_none())
        {
            self.activity.owner = None;
        }
        if self.windows.dirty {
            self.windows.dirty = false;
            self.layout_dirty_since.get_or_insert_with(Instant::now);
        }
        if let Some(since) = self.layout_dirty_since
            && since.elapsed() >= LAYOUT_SAVE_DELAY
        {
            self.layout_dirty_since = None;
            if self.persist_layout {
                self.layout_file().save();
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
            || self.window_switcher.open
            || self.confirm_disconnect.is_some()
            || self.sessions.iter().any(|s| s.show_channel_list)
            || active
                .is_some_and(|s| s.show_connect_dialog && !s.is_connected() && !s.is_connecting())
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let shortcuts = self.take_shortcuts(ctx);
        self.apply_shortcuts(ctx, shortcuts);
    }

    /// Consume application keys before TextEdit can also act on them. Input
    /// belongs to the current viewport, so native detached windows call this
    /// inside their own callback and apply the result after focus bookkeeping.
    fn take_shortcuts(&self, ctx: &egui::Context) -> Vec<WindowShortcut> {
        if self.dialog_blocks_shortcuts() {
            return Vec::new();
        }
        ctx.input_mut(|i| {
            let mut shortcuts = Vec::new();
            // A modifier may already have been released by the end of this
            // input batch. Its state on each key event is authoritative.
            i.events.retain(|event| {
                let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    repeat,
                    ..
                } = event
                else {
                    return true;
                };
                let binding = self
                    .automation
                    .config
                    .shortcuts
                    .iter()
                    .find(|binding| binding.chord.matches(*key, *modifiers));
                if binding.is_some_and(|binding| *repeat && !binding.action.repeatable()) {
                    return false;
                }
                let shortcut = binding.and_then(|binding| {
                    use super::actions::ActionId;
                    if binding.action.composer_only()
                        && !self
                            .windows
                            .active
                            .and_then(|key| self.windows.get(&key))
                            .is_some_and(|window| window.view.input_had_focus)
                    {
                        return None;
                    }
                    Some(match &binding.action {
                        ActionId::CycleWindow { reverse } => {
                            WindowShortcut::Cycle { reverse: *reverse }
                        }
                        ActionId::CloseWindow => WindowShortcut::Close,
                        ActionId::ToggleMaximized => WindowShortcut::ToggleMaximized,
                        ActionId::SelectOrdinal { index } => WindowShortcut::Select(*index),
                        action => WindowShortcut::Action(action.clone()),
                    })
                });
                if let Some(shortcut) = shortcut {
                    shortcuts.push(shortcut);
                    false
                } else {
                    true
                }
            });
            shortcuts
        })
    }

    fn apply_shortcuts(&mut self, ctx: &egui::Context, shortcuts: Vec<WindowShortcut>) {
        let context = self
            .windows
            .active
            .map(|window| super::actions::ActionContext { window });
        for shortcut in shortcuts {
            match shortcut {
                WindowShortcut::Cycle { reverse } => {
                    self.cycle_window(reverse);
                    self.focus_active_viewport(ctx);
                }
                WindowShortcut::Close => {
                    if let Some(key) = self.windows.active {
                        self.request_close(key);
                    }
                }
                WindowShortcut::ToggleMaximized => self.toggle_maximized(),
                WindowShortcut::Select(index) => {
                    if let Some(key) = self.ordered_windows().get(index).cloned() {
                        self.user_activate(key);
                        self.focus_active_viewport(ctx);
                    }
                }
                WindowShortcut::Action(action) => {
                    if let Some(context) = context {
                        self.dispatch_action(ctx, action, context);
                    }
                }
            }
        }
    }

    pub(crate) fn apply_format_action(&mut self, key: WindowKey, action: super::actions::ActionId) {
        use super::actions::ActionId;
        let Some(index) = self.session_index(key.conn) else {
            return;
        };
        if action == ActionId::FormatColor {
            self.color_target = Some(key);
            self.sessions[index].show_color_picker = true;
            self.sessions[index].color_picker_fg = true;
        } else if let Some(window) = self.windows.get_mut(&key) {
            let character = match action {
                ActionId::FormatBold => '\x02',
                ActionId::FormatUnderline => '\x1f',
                ActionId::FormatItalic => '\x1d',
                ActionId::FormatReset => '\x0f',
                _ => return,
            };
            window.view.insert_composer_text(&character.to_string());
            window.view.input_focus_request = true;
        }
    }

    pub(crate) fn focus_active_viewport(&self, ctx: &egui::Context) {
        let Some(key) = &self.windows.active else {
            return;
        };
        if self.windows.get(key).is_some_and(|window| window.detached) {
            self.focus_detached(ctx, key);
        } else {
            // A shortcut from a detached window can select an attached one
            // while the main window is minimized.
            ctx.send_viewport_cmd_to(
                egui::ViewportId::ROOT,
                egui::ViewportCommand::Minimized(false),
            );
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
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
        self.user_activate(order[next]);
    }

    pub(crate) fn toggle_maximized(&mut self) {
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
        match key.conversation {
            Some(id) => {
                if let Some(target) = self.sessions[index].target_name(id).map(str::to_string) {
                    self.sessions[index].close_tab(&target);
                }
            }
            None => {
                let session = &self.sessions[index];
                if session.is_connected() || session.is_connecting() || session.has_worker() {
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
        if let Some(target) = self.color_target
            && !self.windows.get(&target).is_some_and(|w| w.detached)
        {
            self.show_color_picker_for(ctx, &target);
        }
    }

    /// Settings also owns connection setup; no separate Connect window is drawn.
    fn show_global_dialogs(&mut self, ctx: &egui::Context) {
        self.draw_activity(ctx, self.global_dialog_owner);
        self.route_connection_editor();
        self.show_history_windows(ctx);
        self.show_uploads(ctx);
        if self.show_settings {
            self.show_settings_window(ctx);
            // Live global edits still apply to every session.
            self.push_config();
        }
        if let Some(conn) = self.confirm_disconnect {
            self.show_confirm_disconnect(ctx, conn);
        }
    }

    /// Legacy session requests (startup or failed registration) open the same
    /// Connection tab that the toolbar and New connection use.
    fn route_connection_editor(&mut self) {
        let Some(session) = self.sessions.get(self.active_index()) else {
            return;
        };
        if session.show_connect_dialog
            && !session.is_connected()
            && !session.is_connecting()
            && !self.show_settings
        {
            self.open_connection_settings(session.id);
        }
    }

    /// The detached window the application's dialogs show in, if they were
    /// asked for from one that is still detached.
    fn global_dialog_window(&self) -> Option<WindowKey> {
        self.global_dialog_owner
            .filter(|key| self.windows.get(key).is_some_and(|w| w.detached))
    }

    /// An explicit request keeps its source even if another panel is already
    /// open or focus bookkeeping still names a different native window.
    pub(super) fn claim_global_dialogs(&mut self, source: Option<WindowKey>) {
        self.global_dialog_owner = source.filter(|source| {
            self.windows
                .get(source)
                .is_some_and(|window| window.detached)
        });
        self.global_dialogs_were_open = true;
    }

    /// When the application's dialogs open, note the detached window they
    /// belong to: the status window being closed, or the window the user
    /// is working in.
    fn update_global_dialog_owner(&mut self) {
        let open = self.uploads.open
            || self.search.visible()
            || self.show_settings
            || self.confirm_disconnect.is_some()
            || self
                .sessions
                .get(self.active_index())
                .is_some_and(|s| s.show_connect_dialog && !s.is_connected() && !s.is_connecting());
        if !open {
            self.global_dialog_owner = None;
        } else if !self.global_dialogs_were_open {
            let detached = |key: &WindowKey| self.windows.get(key).is_some_and(|w| w.detached);
            let closing = self
                .confirm_disconnect
                .map(|conn| WindowKey::new(conn, None))
                .filter(|key| detached(key));
            self.global_dialog_owner =
                closing.or_else(|| self.windows.active.filter(|key| detached(key)));
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
                    window.view.insert_composer_text(&code);
                }
            } else {
                self.color_target = None;
            }
        }
    }

    /// Open Settings for the network `conn`: Connection edits that session's
    /// form until the window closes; the other preferences are global.
    pub(crate) fn open_settings(&mut self, conn: ConnId) {
        if !self.show_settings {
            self.begin_settings_edit(conn);
        }
        self.show_settings = true;
    }

    /// Retargeting an open editor to another session is a Cancel of the
    /// first: its scratch form goes back to the opening snapshot (#208), the
    /// favorites selection is dropped and a canceled pristine new connection
    /// is discarded, rather than the snapshot being replaced and lost. The
    /// window that was active stays active.
    fn retire_open_editor(&mut self, next: Option<ConnId>) {
        if !self.show_settings
            || self
                .settings_session
                .is_none_or(|owner| Some(owner) == next)
        {
            return;
        }
        let active = self.windows.active;
        self.cancel_settings_window();
        if let Some(key) = active
            && self.windows.active != Some(key)
            && self.session_index(key.conn).is_some()
        {
            self.activate(key);
        }
    }

    fn begin_settings_edit(&mut self, conn: ConnId) {
        self.retire_open_editor(Some(conn));
        self.global_dialog_owner = self
            .windows
            .active
            .filter(|key| self.windows.get(key).is_some_and(|window| window.detached));
        self.settings_session = Some(conn);
        self.settings_form_before = self.session_index(conn).map(|index| {
            let session = &self.sessions[index];
            let mut form = Settings::default();
            session.export_form(&mut form);
            session.export_config(&mut form);
            (conn, form, session.form_profile.clone())
        });
        self.favorites_ui = FavoritesUi {
            editing_profile: self
                .session_index(conn)
                .and_then(|index| self.sessions[index].form_profile.clone()),
            ..FavoritesUi::default()
        };
    }

    /// Connect always chooses this session's complete Connection tab, even if
    /// Settings was already showing another tab or network.
    fn open_connection_settings(&mut self, conn: ConnId) {
        if !self.show_settings || self.settings_session != Some(conn) {
            self.begin_settings_edit(conn);
        }
        self.settings_tab = 0;
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
        let conn = self.sessions[index].id;
        if self.sessions.len() > 1 && self.sessions[index].is_pristine() {
            self.cancelled_new_sessions.insert(conn);
        }
        if let Some((owner, form, profile)) = self.settings_form_before.take()
            && owner == conn
        {
            self.sessions[index].cancel_connect_dialog(&form);
            self.sessions[index].form_profile = profile;
        } else {
            self.sessions[index].cancel_connect_dialog(&self.settings);
        }
        if self.settings_session == Some(conn) {
            self.show_settings = false;
            self.settings_session = None;
        }
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
    pub(crate) fn drop_cancelled_new_sessions(&mut self) {
        if self.sessions.len() < 2 {
            return;
        }
        let pristine: Vec<ConnId> = self
            .cancelled_new_sessions
            .drain()
            .filter(|conn| {
                self.sessions.iter().any(|session| {
                    session.id == *conn && session.is_pristine() && !session.show_connect_dialog
                })
            })
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

    /// Name and connection status at the left, the controls at the right.
    /// What does not fit beside them moves down: first the status line, and
    /// in a window too narrow for the controls beside the name, everything
    /// wraps.
    fn draw_toolbar(&mut self, ui: &mut egui::Ui) {
        let fit_id = ui.scope_id().with("fit");
        let previous: Option<ToolbarFit> = ui.ctx().data(|data| data.get_temp(fit_id));
        let spacing = ui.spacing().item_spacing.x;
        let available = ui.available_width();
        let rows = toolbar_rows(previous, available, spacing);
        let mut fit = previous.unwrap_or_default();
        fit.rows = rows;
        let active = self.active_index();
        let label = self.network_label(&self.sessions[active]);
        if rows == ToolbarRows::Wrapped {
            ui.horizontal_wrapped(|ui| {
                ui.heading("Linefeed");
                ui.separator();
                self.sessions[active].draw_status_summary(ui, &label);
            });
            let row = ui.spacing().interact_size.y;
            let controls = ui
                .allocate_ui_with_layout(
                    egui::vec2(available, row),
                    egui::Layout::right_to_left(egui::Align::Center).with_main_wrap(true),
                    |ui| {
                        self.draw_toolbar_controls(ui);
                        ui.min_rect()
                    },
                )
                .inner;
            // Wrapped controls say nothing about the width they need.
            if controls.height() < row * 1.5 {
                fit.controls = controls.width();
            }
        } else {
            ui.horizontal(|ui| {
                fit.heading = ui.heading("Linefeed").rect.width();
                if rows == ToolbarRows::One {
                    ui.separator();
                    fit.status = ui
                        .scope(|ui| self.sessions[active].draw_status_summary(ui, &label))
                        .response
                        .rect
                        .width();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.draw_toolbar_controls(ui);
                    fit.controls = ui.min_rect().width();
                });
            });
            if rows == ToolbarRows::Two {
                fit.status = ui
                    .horizontal_wrapped(|ui| {
                        self.sessions[active].draw_status_summary(ui, &label);
                        ui.min_rect().width()
                    })
                    .inner;
            }
        }
        ui.ctx().data_mut(|data| data.insert_temp(fit_id, fit));
        if toolbar_rows(Some(fit), available, spacing) < rows {
            // What was just measured does not fit this arrangement: redraw
            // now. A roomier one waits for the next frame, so two
            // arrangements that disagree can never keep the app repainting.
            ui.ctx().request_discard("toolbar arrangement");
            ui.ctx().request_repaint();
        }
    }

    /// The toolbar's controls, in a right-to-left layout: the first one
    /// drawn sits at the right edge.
    fn draw_toolbar_controls(&mut self, ui: &mut egui::Ui) {
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
            ui.separator();
            if ui.button("File sharing…").clicked() {
                self.claim_global_dialogs(None);
                self.uploads.open = true;
                ui.close();
            }
        });
        let count = self.activity.items.iter().filter(|item| !item.read).count();
        let activity_label = if count == 0 {
            "Activity".into()
        } else {
            format!("Activity ({count})")
        };
        if ui.button(activity_label).clicked() {
            self.toggle_activity(None);
        }
        if ui
            .button("Search")
            .on_hover_text("Search saved history (Ctrl+F)")
            .clicked()
        {
            self.open_search(self.active_index(), String::new(), false);
        }
        ui.separator();
        let active = self.active_index();
        if !self.sessions[active].is_connected() && !self.sessions[active].is_connecting() {
            if ui.button("Connect").clicked() {
                self.sessions[active].show_connect_dialog = true;
                self.open_connection_settings(self.sessions[active].id);
            }
        } else if self.sessions[active].is_connected() && ui.button("Disconnect").clicked() {
            let quit = self.quit_reason();
            self.sessions[active].request_manual_disconnect(Some(quit));
        }
        let session = &mut self.sessions[active];
        if session.is_connected() {
            session.draw_join_box(ui);
        }
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
                    let key = WindowKey::new(session.id, target);
                    let detached = self.windows.get(&key).is_some_and(|w| w.detached);
                    let text = switchbar_text(session, target, detached);
                    let selected = self.windows.active.as_ref() == Some(&key);
                    let mut response = ui.selectable_label(selected, text);
                    if detached {
                        response = response.on_hover_text("In its own window");
                    }
                    if response.clicked() {
                        clicked = Some(key);
                    }
                    response.context_menu(|ui| {
                        let detach_label = if detached { "Reattach" } else { "Detach" };
                        if ui.button(detach_label).clicked() {
                            toggle_detach = Some(key);
                            ui.close();
                        }
                        if let Some(name) = target.and_then(|id| session.target_name(id))
                            && session.is_channel_name(name)
                            && ui.button("Channel Info").clicked()
                        {
                            info = Some(key);
                            ui.close();
                        }
                        let close_label = match target.and_then(|id| session.target_name(id)) {
                            None => "Disconnect and close",
                            Some(name) if session.tab_close_needs_part(name) => "Part",
                            Some(_) => "Close",
                        };
                        if ui.button(close_label).clicked() {
                            close = Some(key);
                            ui.close();
                        }
                    });
                }
            }
        });
        if let Some(key) = clicked {
            self.user_activate(key);
            self.focus_detached(ui.ctx(), &key);
        }
        if let Some(key) = toggle_detach {
            self.toggle_detached(&key);
        }
        if let Some(key) = info
            && let (Some(index), Some(id)) = (self.session_index(key.conn), key.conversation)
            && let Some(target) = self.sessions[index].target_name(id).map(str::to_string)
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
                if self.activity.owner == Some(*key) {
                    self.activity.owner = None;
                }
                self.spawn_geometry.remove(key);
            }
            self.windows.dirty = true;
        }
        if self.windows.get(key).is_some_and(|w| !w.detached) {
            self.user_activate(*key);
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
        match key.conversation {
            Some(id) => self.sessions[index]
                .target_name(id)
                .map(|target| format!("{target} @ {label}"))
                .unwrap_or_default(),
            None => format!("Status: {label}"),
        }
    }

    /// The attached window that fills the chat area when maximized.
    fn maximized_window(&self) -> Option<WindowKey> {
        let attached = |key: &WindowKey| self.windows.get(key).is_some_and(|w| !w.detached);
        if let Some(active) = &self.windows.active
            && attached(active)
        {
            return Some(*active);
        }
        if let Some(main) = &self.main_window
            && attached(main)
        {
            return Some(*main);
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
        // Drawing constrains each window to the area and writes the clamped
        // rect back (and saves layout.json), so skip a degenerate frame
        // (a restored tiny window) rather than collapse the layout. A queued
        // tile request waits for a real area.
        if !super::windows::usable_area(area) {
            return;
        }
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
            .map(|w| w.key)
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
                    Some(CaptionAction::Close) => close.push(key),
                    Some(CaptionAction::Maximize) => maximize = Some(key),
                    Some(CaptionAction::Detach) => detach = Some(key),
                    None => {}
                }
                let rect = shown.response.rect;
                if let Some(pos) = pressed_at
                    && rect.contains(pos)
                    && ctx.layer_id_at(pos) == Some(shown.response.layer_id)
                {
                    activate = Some(key);
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
            && self.windows.active.as_ref() != Some(key)
        {
            self.user_activate(*key);
        }
        if let Some(key) = maximize {
            self.user_activate(*key);
            self.windows.mode = LayoutMode::Maximized;
            self.windows.dirty = true;
        }
        if let Some(key) = detach {
            self.toggle_detached(key);
        }
        // The active window is always the front one. egui raises a window on
        // its first visible frame and on a click; a click also makes that
        // window active (above), so this only overrides windows appearing
        // on their own, which must not cover the one the user is typing in.
        if let Some(active) = self.windows.active
            && self.windows.get(&active).is_some_and(|w| !w.detached)
        {
            ctx.move_to_top(egui::LayerId::new(
                egui::Order::Middle,
                child_window_id(&active),
            ));
        }
        for key in close {
            self.request_close(*key);
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
            .map(|w| w.key)
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
        let mut shortcuts = Vec::new();
        let mut activity = false;
        for key in keys {
            let title = format!("{} — {}", self.window_title(&key), MAIN_WINDOW_TITLE);
            let (pos, size) = *self.spawn_geometry.entry(key).or_insert_with(|| {
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
            let result = ctx.show_viewport_immediate(id, builder, |ui, class| {
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
                // Embedded viewports share root input, which was already
                // handled by App::ui. Native viewports have separate input.
                let window_shortcuts = if class == egui::ViewportClass::EmbeddedWindow {
                    Vec::new()
                } else {
                    self.take_shortcuts(ui.ctx())
                };
                let mut reattach_clicked = false;
                egui::Panel::top(("detached_header", key.conn, key.conversation)).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(self.window_title(&key)).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Activity").clicked() {
                                self.toggle_activity(Some(key));
                            }
                            if ui
                                .small_button("Reattach")
                                .on_hover_text("Put back inside the main window")
                                .clicked()
                            {
                                reattach_clicked = true;
                            }
                        });
                    });
                });
                if let Some(index) = self.session_index(key.conn) {
                    ui.horizontal_wrapped(|ui| {
                        self.sessions[index].draw_account_security_status(ui)
                    });
                }
                egui::CentralPanel::default().show(ui, |ui| {
                    self.draw_window_contents(ui, &key);
                });
                self.draw_activity(ui.ctx(), Some(key));
                self.show_detached_window_dialogs(ui.ctx(), &key);
                crate::opener::dispatch_pending_urls(ui.ctx());
                (
                    close_requested,
                    is_focused,
                    outer,
                    inner,
                    any_input,
                    pressed,
                    reattach_clicked,
                    window_shortcuts,
                )
            });
            let (
                close_requested,
                is_focused,
                outer,
                inner,
                any_input,
                pressed,
                reattach_clicked,
                window_shortcuts,
            ) = result;
            if !window_shortcuts.is_empty() {
                shortcuts.push((key, window_shortcuts));
            }
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
                focused = Some(key);
            }
            activity |= any_input;
            if close_requested {
                close.push(key);
            }
            if reattach_clicked {
                reattach.push(key);
            }
        }
        if let Some(key) = &focused
            && self.focused_detached.as_ref() != Some(key)
            && self.windows.active.as_ref() != Some(key)
        {
            self.user_activate(*key);
        }
        self.focused_detached = focused;
        if activity {
            self.note_activity();
        }
        // Apply native input after updating focus, or a cycle/select action
        // would immediately be undone by the source window's old focus flag.
        for (key, actions) in shortcuts {
            if self.windows.get(&key).is_some() {
                self.user_activate(key);
                self.apply_shortcuts(ctx, actions);
            }
        }
        for key in reattach {
            self.toggle_detached(&key);
        }
        for key in close {
            // A status window asks before disconnecting, in its own OS
            // window (the main one may be minimized), and stays if the
            // user cancels.
            if key.conversation.is_some() {
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
            .map(|w| w.key)
            .collect();
        self.spawn_geometry.retain(|key, _| detached.contains(key));
    }

    /// Draw one window's contents (topic, user list, scrollback, input) for
    /// its session, with the window's input line swapped in.
    pub(crate) fn draw_window_contents(&mut self, ui: &mut egui::Ui, key: &WindowKey) {
        let Some(index) = self.session_index(key.conn) else {
            return;
        };
        if key
            .conversation
            .is_some_and(|id| self.sessions[index].target_name(id).is_none())
        {
            return;
        }
        let view_is_active = self.windows.active == Some(*key);
        let App {
            sessions, windows, ..
        } = self;
        let Some(window) = windows.get_mut(key) else {
            return;
        };
        let session = &mut sessions[index];
        let detached = window.detached;
        let (changed_to, (picker, dialog)) =
            with_window_view(session, &mut window.view, &key.conversation, |s| {
                let before = (
                    s.show_color_picker,
                    s.show_channel_list,
                    s.show_channel_info,
                );
                s.view_is_active = view_is_active;
                s.draw_target(ui);
                s.handle_message_actions();
                (
                    !before.0 && s.show_color_picker,
                    (!before.1 && s.show_channel_list) || (!before.2 && s.show_channel_info),
                )
            });
        if view_is_active {
            self.mark_activity_seen(*key);
        }
        if picker {
            self.color_target = Some(*key);
        }
        // A dialog asked for from this window (/list, the colour button)
        // shows where the user is: in this window's OS window if detached.
        if dialog {
            if detached {
                self.dialog_owner.insert(key.conn, *key);
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
        if key
            .conversation
            .is_some_and(|id| self.sessions[index].target_name(id).is_none())
        {
            return;
        }
        let App {
            sessions, windows, ..
        } = self;
        let Some(window) = windows.get_mut(key) else {
            return;
        };
        let (changed_to, ()) = with_window_view(
            &mut sessions[index],
            &mut window.view,
            &key.conversation,
            |s| s.process_input(),
        );
        if let Some(target) = changed_to {
            self.user_activate(WindowKey::new(key.conn, target));
        }
    }

    // ----- update restart -------------------------------------------------

    pub(crate) fn capture_workspace(&mut self) -> super::workspace::WorkspaceSnapshot {
        use super::workspace::{WorkspaceSession, WorkspaceSnapshot, WorkspaceView};
        let mut snapshot = self.update_session();
        let active = snapshot
            .active_position()
            .min(self.sessions.len().saturating_sub(1));
        let mut snapshots = snapshot.take_others();
        snapshots.insert(active.min(snapshots.len()), snapshot);
        let active_conn = self.sessions.get(active).map(|session| session.id);
        let mut active_position = 0;
        let mut entries = Vec::new();
        for (index, mut state) in snapshots.into_iter().enumerate() {
            let Some(session) = self.sessions.get(index) else {
                continue;
            };
            if self.closing_sessions.contains(&session.id) {
                continue;
            }
            let Some(config) = state.session.as_ref() else {
                continue;
            };
            let expected_network = config.logical_network_key();
            let mut views = Vec::new();
            if !session.update_changes_endpoint() {
                for window in self
                    .windows
                    .windows
                    .iter()
                    .filter(|window| window.key.conn == session.id)
                {
                    let target = match window.key.conversation {
                        Some(id) => match session.target_name(id) {
                            Some(name) => Some(name.to_owned()),
                            None => continue,
                        },
                        None => None,
                    };
                    let mut view = window.view.workspace_state();
                    if session.draft_for_owner_is_sensitive(window.key.conversation, &view.draft) {
                        view.draft.clear();
                        view.reply_to = None;
                    }
                    views.push(WorkspaceView {
                        target,
                        state: view,
                    });
                }
                for (conn, target, pending) in &self.pending_workspace_views {
                    if *conn != session.id
                        || target.is_some_and(|id| session.target_name(id).is_none())
                    {
                        continue;
                    }
                    let name = target.and_then(|id| session.target_name(id).map(str::to_owned));
                    let mut pending = pending.clone();
                    if session.draft_for_owner_is_sensitive(*target, &pending.draft) {
                        pending.draft.clear();
                        pending.reply_to = None;
                    }
                    if let Some(view) = views.iter_mut().find(|view| view.target == name) {
                        view.state = pending;
                    } else {
                        views.push(WorkspaceView {
                            target: name,
                            state: pending,
                        });
                    }
                }
                for (conn, target, draft) in &self.pending_inputs {
                    if *conn != session.id
                        || target.is_some_and(|id| session.target_name(id).is_none())
                    {
                        continue;
                    }
                    let name = target.and_then(|id| session.target_name(id).map(str::to_owned));
                    let sensitive = session.draft_for_owner_is_sensitive(*target, draft);
                    let draft = if sensitive {
                        String::new()
                    } else {
                        draft.clone()
                    };
                    if let Some(view) = views.iter_mut().find(|view| view.target == name) {
                        view.state.draft = draft;
                        if sensitive {
                            view.state.reply_to = None;
                        }
                    } else {
                        views.push(WorkspaceView {
                            target: name,
                            state: super::workspace::WorkspaceViewState {
                                draft,
                                ..Default::default()
                            },
                        });
                    }
                }
            }
            state.set_input(String::new());
            state.set_drafts(Vec::new());
            if active_conn == Some(session.id) {
                active_position = entries.len();
            }
            entries.push(WorkspaceSession {
                expected_network,
                state,
                views,
                case_mapping: session.network_support.case_mapping,
            });
        }
        let mut snapshot = WorkspaceSnapshot::new(entries, active_position);
        snapshot.activity = self.activity_records();
        snapshot
    }

    fn restore_workspace(&mut self, snapshot: super::workspace::WorkspaceSnapshot) {
        let active = snapshot.active_session;
        let activity = snapshot.activity;
        let mut hydrate = Vec::new();
        for mut entry in snapshot.sessions {
            // Refresh changed passwords only for the same committed network
            // and services identity; a profile edit cannot cross-bind drafts.
            if let Some(saved) = entry.state.session.as_mut()
                && let Some(favorite) = saved.profile_id.as_ref().and_then(|id| {
                    self.settings
                        .server_favorites
                        .iter()
                        .find(|favorite| &favorite.id == id)
                })
            {
                let mut current = Session::with_settings(self.settings.clone());
                current.load_favorite(favorite);
                if let Ok(config) = current.session_from_form()
                    && config.logical_network_key() == entry.expected_network
                    && config.services_identity_key() == saved.services_identity_key()
                {
                    saved.server.password = config.server.password;
                    saved.server.sasl_password = config.server.sasl_password;
                    saved.server.sasl_required = config.server.sasl_required;
                    saved.server.accept_invalid_certs = config.server.accept_invalid_certs;
                    saved.server.tls_client_key_path = config.server.tls_client_key_path;
                }
            }
            let index = self.add_session();
            let session = &mut self.sessions[index];
            session.network_support.case_mapping = entry.case_mapping;
            session.restore_update_session(entry.state);
            session.input_text.clear();
            let conn = session.id;
            for mut view in entry.views {
                let target = match view.target {
                    Some(target) => match session.id_for_target(&target) {
                        Some(id) => Some(id),
                        None => continue,
                    },
                    None => None,
                };
                let anchor =
                    view.state
                        .viewport
                        .as_ref()
                        .filter(|anchor| !anchor.at_bottom && anchor.message.is_some())
                        .cloned()
                        .or_else(|| {
                            view.state.last_read.clone().map(|message| {
                                super::identity::ScrollAnchor {
                                    message: Some(message),
                                    fraction: 0.0,
                                    at_bottom: true,
                                }
                            })
                        });
                if let Some(anchor) = anchor {
                    if view.state.viewport.is_none() {
                        view.state.viewport = Some(anchor.clone());
                    }
                    hydrate.push((WindowKey::new(conn, target), anchor));
                }
                self.pending_workspace_views
                    .push((conn, target, view.state));
            }
        }
        if let Some(session) = self.sessions.get(active) {
            self.restore_after_update =
                Some(WindowKey::new(session.id, session.current_conversation));
        }
        self.restore_activity(activity);
        for (key, anchor) in hydrate {
            self.queue_workspace_restore(key, anchor);
        }
    }

    fn queue_workspace_restore(&mut self, key: WindowKey, anchor: super::identity::ScrollAnchor) {
        let Some(reference) = &anchor.message else {
            return;
        };
        let Some(index) = self.session_index(key.conn) else {
            return;
        };
        if self.sessions[index].logical_network_key().as_ref()
            != Some(&reference.conversation.network)
        {
            return;
        }
        let Some(id) = key.conversation else {
            return;
        };
        let Some(channel) = self.sessions[index].conversation_channel(id) else {
            return;
        };
        let initial = channel
            .messages
            .iter()
            .map(|row| row.message_key.clone())
            .collect();
        let token = self.next_workspace_restore;
        self.next_workspace_restore = token
            .checked_add(1)
            .expect("workspace restore IDs exhausted");
        let radius = (self.sessions[index].max_scrollback.saturating_sub(1) / 4).min(125);
        if self
            .archive
            .client
            .restore_context(token, reference.clone(), radius)
        {
            self.workspace_restores
                .insert(token, (key, anchor, initial));
            if let Some(window) = self.windows.get_mut(&key) {
                window.view.set_workspace_hydrating(true);
            }
        }
    }

    pub(crate) fn handle_workspace_archive_reply(
        &mut self,
        token: u64,
        reference: super::identity::MessageRef,
        records: Vec<super::archive::ArchiveRecord>,
    ) {
        let Some((key, anchor, initial)) = self.workspace_restores.remove(&token) else {
            return;
        };
        if anchor.message.as_ref() != Some(&reference) {
            return;
        }
        let Some(index) = self.session_index(key.conn) else {
            return;
        };
        let Some(id) = key.conversation else {
            return;
        };
        if self.sessions[index].logical_network_key().as_ref()
            != Some(&reference.conversation.network)
            || self.sessions[index].target_name(id).is_none()
        {
            return;
        }
        if let Some(window) = self.windows.get_mut(&key) {
            window.view.set_workspace_hydrating(false);
            if !window.view.timeline.borrow().restore_pending {
                return;
            }
        }
        let format = self.sessions[index].timestamp_format.clone();
        let mut context = Vec::new();
        for record in records
            .into_iter()
            .filter(|record| record.conversation == reference.conversation)
        {
            let body = if record.redacted {
                "<message redacted>"
            } else {
                &record.body
            };
            let mut message = if record.system {
                ChatMessage::system_fmt(body, &format)
            } else if record.action {
                ChatMessage::action_fmt(&record.sender, body, &format)
            } else {
                ChatMessage::new_fmt(&record.sender, body, &format)
            }
            .without_logging();
            message.timestamp = record.display_time;
            message.message_key = record.id;
            message.msgid = record.msgid;
            message.server_time = record.server_time;
            message.account = record.account;
            message.reply_to = record.reply_to;
            message.reactions = record.reactions;
            context.push(message);
        }
        let maximum = self.sessions[index].max_scrollback;
        let Some(channel) = self.sessions[index].conversation_channel_mut(id) else {
            return;
        };
        let mut matched = vec![false; context.len()];
        let mut preserve_live = HashSet::new();
        let mut retained = Vec::new();
        for existing in channel.messages.drain(..) {
            let duplicate = context.iter().enumerate().position(|(at, restored)| {
                !matched[at]
                    && (existing.message_key == restored.message_key
                        || existing
                            .msgid
                            .as_ref()
                            .is_some_and(|msgid| restored.msgid.as_ref() == Some(msgid))
                        || (initial.contains(&existing.message_key)
                            && !existing.pending_echo
                            && existing.server_time.is_none()
                            && existing.msgid.is_none()
                            && existing.sender == restored.sender
                            && existing.content == restored.content
                            && existing.timestamp == restored.timestamp))
            });
            if let Some(at) = duplicate {
                matched[at] = true;
                if !initial.contains(&existing.message_key) {
                    preserve_live.insert(at);
                    retained.push(existing);
                }
            } else {
                retained.push(existing);
            }
        }
        context = context
            .into_iter()
            .enumerate()
            .filter_map(|(at, row)| (!preserve_live.contains(&at)).then_some(row))
            .collect();
        // Keep the requested context and the newest arrivals. The bounded
        // archive remains available for rows outside this in-memory window.
        let remaining = maximum.saturating_sub(context.len());
        if retained.len() > remaining {
            retained.drain(..retained.len() - remaining);
        }
        let mut messages: std::collections::VecDeque<_> = context.into_iter().collect();
        Session::merge_catch_up_history(&mut messages, retained.into_iter().collect());
        channel.messages = messages;
        channel.history_loaded = true;
        if let Some(target) = self.sessions[index].target_name(id).map(str::to_owned) {
            self.sessions[index].refresh_reply_previews(&target);
        }
        if let Some(window) = self.windows.get_mut(&key) {
            let mut timeline = window.view.timeline.borrow_mut();
            timeline.anchor = Some(anchor);
            timeline.restore_pending = true;
        }
    }

    pub(crate) fn handle_workspace_archive_error(&mut self, token: u64, message: String) {
        let Some((key, _, _)) = self.workspace_restores.remove(&token) else {
            return;
        };
        if let Some(window) = self.windows.get_mut(&key) {
            window.view.set_workspace_hydrating(false);
        }
        if let Some(index) = self.session_index(key.conn) {
            self.sessions[index].add_server_message(ChatMessage::system(&format!(
                "Reading position could not be restored: {message}"
            )));
        }
    }

    fn checkpoint_workspace(&mut self) {
        if let Some(error) = self.workspace.take_error()
            && let Some(session) = self.sessions.first_mut()
        {
            session.add_server_message(ChatMessage::system(&error));
        }
        let pending = std::mem::take(&mut self.pending_workspace_views);
        for (conn, conversation, state) in pending {
            if let Some(window) = self.windows.get_mut(&WindowKey::new(conn, conversation)) {
                window.view.restore_workspace_state(state);
                let hydrating = self
                    .workspace_restores
                    .values()
                    .any(|(key, _, _)| *key == WindowKey::new(conn, conversation));
                window.view.set_workspace_hydrating(hydrating);
            } else if self.session_index(conn).is_some_and(|index| {
                conversation.is_none_or(|id| self.sessions[index].target_name(id).is_some())
            }) {
                self.pending_workspace_views
                    .push((conn, conversation, state));
            }
        }
        if self.settings.resume_workspace && self.workspace.checkpoint_due() {
            let snapshot = self.capture_workspace();
            self.workspace.save(snapshot);
        }
    }

    /// Snapshot every session for an update restart. The active session is
    /// the top-level snapshot, which is all an older binary reads; the
    /// others follow in `others`.
    pub fn update_session(&mut self) -> UpdateSession {
        let active = self.active_index();
        let mut snapshots = Vec::new();
        for index in 0..self.sessions.len() {
            let conn = self.sessions[index].id;
            let target = self.sessions[index].current_conversation;
            let mut drafts: Vec<_> = self
                .windows
                .windows
                .iter()
                .filter(|window| window.key.conn == conn)
                .map(|window| (window.key.conversation, window.view.input_text.clone()))
                .collect();
            // A restored target may not have appeared yet. Until sync moves
            // that input into its window, the queued copy remains authoritative.
            for (_, pending_target, input) in self
                .pending_inputs
                .iter()
                .filter(|(pending_conn, _, _)| *pending_conn == conn)
            {
                if let Some((_, draft)) = drafts
                    .iter_mut()
                    .find(|(draft_target, _)| draft_target == pending_target)
                {
                    draft.clone_from(input);
                } else {
                    drafts.push((*pending_target, input.clone()));
                }
            }
            // Classify each draft in its owning conversation, including queued inputs.
            // A retired owner must never fall through to the status window.
            drafts.retain(|(owner, _)| {
                owner.is_none_or(|id| self.sessions[index].target_name(id).is_some())
            });
            for (owner, draft) in &mut drafts {
                if self.sessions[index].draft_for_owner_is_sensitive(*owner, draft) {
                    draft.clear();
                }
            }
            if self.sessions[index].update_changes_endpoint() {
                // A pending /server switch snapshots its new endpoint. Drafts
                // still displayed in the old server's windows stay there.
                drafts.clear();
            }
            let input = drafts
                .iter()
                .find(|(draft_target, _)| *draft_target == target)
                .map(|(_, input)| input.clone())
                .unwrap_or_default();
            let mut snapshot = self.sessions[index].update_session();
            // Older binaries restore only this selected draft and ignore the
            // new per-window collection.
            snapshot.set_input(input);
            snapshot.set_drafts(
                drafts
                    .into_iter()
                    .filter_map(|(id, input)| match id {
                        Some(id) => self.sessions[index]
                            .target_name(id)
                            .map(|name| (Some(name.to_string()), input)),
                        None => Some((None, input)),
                    })
                    .collect(),
            );
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
        for mut snapshot in ordered {
            let input = snapshot.input().to_string();
            let drafts = snapshot.take_drafts();
            let index = self.add_session();
            self.sessions[index].restore_update_session(snapshot);
            self.sessions[index].input_text.clear();
            let conn = self.sessions[index].id;
            if let Some(drafts) = drafts {
                for (target, input) in drafts {
                    let id = match target {
                        Some(target) => match self.sessions[index].id_for_target(&target) {
                            Some(id) => Some(id),
                            None => continue,
                        },
                        None => None,
                    };
                    self.pending_inputs.push((conn, id, input));
                }
            } else {
                self.pending_inputs
                    .push((conn, self.sessions[index].current_conversation, input));
            }
        }
        // The active session's window is the one to come back to.
        if let Some(session) = self.sessions.get(position) {
            self.restore_after_update =
                Some(WindowKey::new(session.id, session.current_conversation));
        }
    }

    /// Before an update restart or exit: save, and send QUIT everywhere.
    pub fn prepare_update_shutdown(&mut self) {
        let _ = self.archive.client.flush_barrier(Duration::from_secs(2));
        if self.settings.resume_workspace {
            let snapshot = self.capture_workspace();
            if !self.workspace.finish(snapshot) {
                tracing::warn!(
                    "The latest workspace checkpoint could not be written before shutdown"
                );
            }
        }
        if self.persist_layout {
            self.layout_file().save();
        }
        if !self.sessions.is_empty() {
            let active = self.active_index();
            let session = &self.sessions[active];
            // Behaviour settings only: the connect form is a scratch copy
            // that only an explicit validated Save or Connect adopts (#316).
            session.export_config(&mut self.settings);
        }
        self.store_settings();
        for session in &mut self.sessions {
            session.prepare_update_shutdown();
        }
    }

    /// Wait (bounded) for every worker to flush QUIT and exit.
    pub fn join_workers(&mut self, deadline: Instant) {
        for session in &mut self.sessions {
            session.join_worker_until(deadline);
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
    contexts: Vec<Option<(super::attention::AttentionEvent, String, String)>>,
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
    egui::Id::unique(("linefeed_window", key.conn, key.conversation))
}

pub(crate) fn detached_viewport_id(key: &WindowKey) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(("linefeed_detached", key.conn, key.conversation))
}

/// How the toolbar is arranged in the width at hand.
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
enum ToolbarRows {
    /// Too narrow for the controls beside the name: everything wraps.
    Wrapped,
    /// The controls share the name's row; the status line goes below.
    Two,
    /// Name and status at the left, controls at the right.
    #[default]
    One,
}

/// The toolbar's arrangement and the widths its parts were last drawn at.
#[derive(Clone, Copy, Debug, Default)]
struct ToolbarFit {
    rows: ToolbarRows,
    heading: f32,
    status: f32,
    controls: f32,
}

/// The roomiest arrangement the measured parts fit in. Nothing is measured
/// before the first frame, which therefore tries the single row. Moving to
/// a roomier arrangement needs some slack, so that a status line whose
/// width keeps changing (the lag figure) cannot flip the layout back and
/// forth.
fn toolbar_rows(fit: Option<ToolbarFit>, available: f32, spacing: f32) -> ToolbarRows {
    const SLACK: f32 = 24.0;
    let Some(fit) = fit else {
        return ToolbarRows::One;
    };
    let fits = |rows: ToolbarRows, width: f32| {
        let slack = if rows > fit.rows { SLACK } else { 0.0 };
        width + slack <= available
    };
    // Name, separator, status and controls, with a gap between each.
    let one_row = fit.heading + fit.status + fit.controls + 4.0 * spacing;
    let two_rows = fit.heading + fit.controls + spacing;
    if fits(ToolbarRows::One, one_row) {
        ToolbarRows::One
    } else if fits(ToolbarRows::Two, two_rows) {
        ToolbarRows::Two
    } else {
        ToolbarRows::Wrapped
    }
}

/// Switchbar button text, coloured like the old sidebar: unread channels
/// green, unread queries orange, unread status yellow, queries light blue.
fn switchbar_text(session: &Session, target: Option<ConversationId>, detached: bool) -> RichText {
    let text = switchbar_label(session, target);
    // Detached windows are set in italics: a marker glyph would depend on
    // the fonts installed.
    if detached { text.italics() } else { text }
}

fn switchbar_label(session: &Session, target: Option<ConversationId>) -> RichText {
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
        Some(id) => {
            let Some(name) = session.target_name(id) else {
                return RichText::new("");
            };
            let unread = session
                .conversation_channel(id)
                .map(|c| c.unread)
                .unwrap_or(0);
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
    target: &Option<ConversationId>,
    f: impl FnOnce(&mut Session) -> R,
) -> (Option<Option<ConversationId>>, R) {
    let saved_current = std::mem::replace(&mut session.current_conversation, *target);
    swap_view(session, view);
    let result = f(session);
    swap_view(session, view);
    let after = session.current_conversation;
    let exists = |session: &Session, target: &Option<ConversationId>| {
        target
            .as_ref()
            .is_none_or(|id| session.target_name(*id).is_some())
    };
    if &after == target {
        if exists(session, &saved_current) {
            session.current_conversation = saved_current;
        }
        (None, result)
    } else if exists(session, target) {
        // The command moved focus (/query, /join of a joined channel).
        (Some(after), result)
    } else {
        // The window's own tab went away (/close, /part of a query): keep
        // the session's selection where it was, if that still exists; the
        // App moves activity off a vanished active window (`sync_windows`).
        if exists(session, &saved_current) {
            session.current_conversation = saved_current;
        }
        (None, result)
    }
}

fn swap_view(session: &mut Session, view: &mut TargetView) {
    std::mem::swap(&mut session.input_text, &mut view.input_text);
    std::mem::swap(&mut session.composer, &mut view.composer);
    std::mem::swap(&mut session.timeline, &mut view.timeline);
    // The sent-line history travels with the input line: each window
    // recalls only what was sent from that window.
    std::mem::swap(&mut session.command_history, &mut view.command_history);
    std::mem::swap(&mut session.history_index, &mut view.history_index);
    std::mem::swap(&mut session.history_temp, &mut view.history_temp);
    std::mem::swap(&mut session.tab_completion, &mut view.tab_completion);
    std::mem::swap(&mut session.selected_user, &mut view.selected_user);
    std::mem::swap(
        &mut session.input_focus_request,
        &mut view.input_focus_request,
    );
    std::mem::swap(&mut session.input_had_focus, &mut view.input_had_focus);
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
#[path = "workspace_app_tests.rs"]
mod workspace_resume_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::snap;
    use crate::irc::{IrcCommand, IrcMessage};
    use tokio::sync::mpsc;

    fn target_key(app: &App, conn: ConnId, target: &str) -> WindowKey {
        let index = app.session_index(conn).expect("test session");
        WindowKey::new(
            conn,
            Some(
                app.sessions[index]
                    .id_for_target(target)
                    .expect("test target"),
            ),
        )
    }

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

    #[test]
    fn offline_workspace_saving_keeps_the_event_loop_polling() {
        let mut app = test_app();
        assert!(!app.wants_polling());
        let path = std::env::temp_dir()
            .join(format!(
                "linefeed-offline-poll-{}",
                super::super::new_profile_id()
            ))
            .join("workspace.json");
        app.workspace = super::super::workspace::WorkspaceStore::open(path);
        app.settings.resume_workspace = true;
        assert!(app.wants_polling());
        app.settings.resume_workspace = false;
        assert!(!app.wants_polling());
    }

    #[test]
    fn offline_workspace_hydration_keeps_the_event_loop_polling() {
        let mut app = test_app();
        assert!(!app.wants_polling());
        app.workspace_restores.insert(
            1,
            (
                WindowKey::new(app.sessions[0].id, None),
                super::super::identity::ScrollAnchor::default(),
                HashSet::new(),
            ),
        );
        assert!(app.wants_polling());
        app.workspace_restores.clear();
        assert!(!app.wants_polling());
    }

    #[test]
    fn sync_windows_reconciles_a_replaced_window_manager() {
        // The manager can be swapped wholesale (startup layout restore);
        // the next sync must still reconcile it even though the sessions'
        // window set did not change in between.
        let mut app = test_app();
        app.sync_windows();
        let before = app.windows.windows.len();
        assert!(before > 0);
        app.windows = WindowManager::from_layout(LayoutFile::default());
        assert_eq!(app.windows.windows.len(), 0);
        app.sync_windows();
        assert_eq!(app.windows.windows.len(), before);
    }

    /// Make session `index` look registered, capturing what it sends.
    fn connect(app: &mut App, index: usize) -> mpsc::UnboundedReceiver<IrcCommand> {
        let (tx, rx) = mpsc::unbounded_channel();
        let session = &mut app.sessions[index];
        if session.active_session.is_none() {
            let config = session
                .session_from_form()
                .expect("registered fixture needs a valid endpoint");
            session.activate_session(config);
        }
        session.test_set_registered(tx);
        rx
    }

    fn feed(app: &mut App, index: usize, line: &str) {
        app.sessions[index].handle_incoming_message(IrcMessage::parse(line).unwrap());
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<IrcCommand>) -> Vec<String> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .flat_map(super::super::flatten_test_commands)
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

    /// Model eframe's nested native viewport render, rather than egui's
    /// default embedded fallback, so child input/output stays separate.
    fn native_detached_frame(
        app: &mut App,
        ctx: &egui::Context,
        source: &WindowKey,
        events: Vec<egui::Event>,
        minimized: bool,
        inject_link: bool,
    ) -> Vec<egui::OutputCommand> {
        use std::cell::RefCell;
        use std::rc::Rc;

        ctx.set_embed_viewports(false);
        let source_id = detached_viewport_id(source);
        let pending_events = Rc::new(RefCell::new(events));
        let child_output = Rc::new(RefCell::new(Vec::new()));
        let collected_output = child_output.clone();
        egui::Context::set_immediate_viewport_renderer(move |ctx, mut viewport| {
            let is_source = viewport.ids.this == source_id;
            let mut events = if is_source {
                std::mem::take(&mut *pending_events.borrow_mut())
            } else {
                Vec::new()
            };
            let modifiers = events.iter().find_map(|event| match event {
                egui::Event::Key { modifiers, .. } => Some(*modifiers),
                _ => None,
            });
            if let Some(modifiers) = modifiers {
                events.insert(0, egui::Event::ModifiersChanged(modifiers));
            }
            let input = egui::RawInput {
                viewport_id: viewport.ids.this,
                viewports: [
                    (egui::ViewportId::ROOT, egui::ViewportInfo::default()),
                    (
                        viewport.ids.this,
                        egui::ViewportInfo {
                            parent: Some(viewport.ids.parent),
                            focused: Some(is_source),
                            ..Default::default()
                        },
                    ),
                ]
                .into_iter()
                .collect(),
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(720.0, 520.0),
                )),
                events,
                focused: is_source,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                if inject_link && is_source {
                    // The URL validator rejects this before any OS launch.
                    ui.ctx()
                        .open_url(egui::OpenUrl::new_tab("file:///link-test"));
                }
                (viewport.viewport_ui_cb)(ui);
            });
            output.textures_delta.clear();
            collected_output
                .borrow_mut()
                .extend(output.platform_output.commands);
        });
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1024.0, 768.0),
                )),
                focused: false,
                ..Default::default()
            },
            |ui| {
                if minimized {
                    app.ui_detached_only(ui.ctx(), true);
                } else {
                    app.ui(ui);
                }
            },
        );
        output.textures_delta.clear();
        child_output.take()
    }

    fn shortcut_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn native_detached_shortcuts_use_child_input_once_even_while_minimized() {
        for minimized in [false, true] {
            for (key, modifiers) in [
                (egui::Key::W, egui::Modifiers::CTRL),
                (egui::Key::Tab, egui::Modifiers::CTRL),
                (
                    egui::Key::Tab,
                    egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                ),
                (egui::Key::Num1, egui::Modifiers::ALT),
                (egui::Key::M, egui::Modifiers::CTRL),
            ] {
                let (mut app, mut rx_a, mut rx_b) = two_networks();
                let source = target_key(&app, app.sessions[1].id, "#debian");
                let root = target_key(&app, app.sessions[0].id, "#rust");
                app.toggle_detached(&source);
                let ctx = egui::Context::default();
                {
                    let window = app.windows.get_mut(&source).unwrap();
                    window.view.input_text = "keep this word".into();
                    window.view.input_focus_request = true;
                }
                for _ in 0..2 {
                    native_detached_frame(&mut app, &ctx, &source, vec![], minimized, false);
                }
                // Source binding must work even before root focus bookkeeping
                // has caught up with the native child.
                app.activate(root);
                drain(&mut rx_a);
                drain(&mut rx_b);
                native_detached_frame(
                    &mut app,
                    &ctx,
                    &source,
                    vec![shortcut_event(key, modifiers)],
                    minimized,
                    false,
                );
                assert!(drain(&mut rx_a).is_empty());
                if key == egui::Key::W {
                    assert_eq!(drain(&mut rx_b), ["PART #debian"]);
                    assert_eq!(
                        app.windows.get(&source).unwrap().view.input_text,
                        "keep this word",
                        "Ctrl+W also reached TextEdit"
                    );
                } else if key == egui::Key::M {
                    assert_eq!(app.windows.mode, LayoutMode::Floating);
                } else {
                    let expected = if key == egui::Key::Tab && modifiers.shift {
                        WindowKey::new(source.conn, None)
                    } else {
                        WindowKey::new(app.sessions[0].id, None)
                    };
                    assert_eq!(app.windows.active, Some(expected));
                }
            }
        }
    }

    #[test]
    fn shortcut_events_keep_their_own_modifiers_after_release_in_the_same_batch() {
        for (key, modifiers, expected) in [
            (egui::Key::W, egui::Modifiers::CTRL, WindowShortcut::Close),
            (
                egui::Key::Tab,
                egui::Modifiers::CTRL,
                WindowShortcut::Cycle { reverse: false },
            ),
            (
                egui::Key::Tab,
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                WindowShortcut::Cycle { reverse: true },
            ),
            (
                egui::Key::M,
                egui::Modifiers::CTRL,
                WindowShortcut::ToggleMaximized,
            ),
            (
                egui::Key::Num9,
                egui::Modifiers::ALT,
                WindowShortcut::Select(8),
            ),
        ] {
            let (app, _rx_a, _rx_b) = two_networks();
            let mut output = egui::Context::default().run_ui(
                egui::RawInput {
                    events: vec![
                        egui::Event::ModifiersChanged(modifiers),
                        shortcut_event(key, modifiers),
                        egui::Event::ModifiersChanged(egui::Modifiers::NONE),
                    ],
                    ..Default::default()
                },
                |ui| {
                    assert!(ui.ctx().input(|input| input.modifiers.is_none()));
                    assert_eq!(
                        app.take_shortcuts(ui.ctx()).as_slice(),
                        std::slice::from_ref(&expected)
                    );
                    assert!(!ui.ctx().input(|input| input.key_pressed(key)));
                },
            );
            output.textures_delta.clear();
        }
    }

    #[test]
    fn a_nick_change_moves_the_alert_rule_in_every_copy() {
        use super::super::attention::AlertLevel;
        for level in [AlertLevel::Muted, AlertLevel::All] {
            let (mut app, _rx_a, _rx_b) = two_networks();
            assert!(app.sessions[0].open_query("alice"));
            let old_key = app.sessions[0].conversation_key("alice");
            let new_key = app.sessions[0].conversation_key("alice_");
            app.sessions[0].attention.rule_mut(old_key.clone()).level = level;
            app.sessions[0].export_config(&mut app.settings);
            app.push_config();

            feed(&mut app, 0, ":alice!u@h NICK alice_");
            app.process_events();

            // The session's own copy drives the All and highlight gates.
            assert_eq!(
                app.sessions[0].attention.rule(&new_key).map(|r| r.level),
                Some(level)
            );
            assert!(app.sessions[0].attention.rule(&old_key).is_none());
            assert_eq!(
                app.settings.attention.rule(&new_key).map(|r| r.level),
                Some(level)
            );
            // The next export from the session must not undo the move.
            app.sessions[0].export_config(&mut app.settings);
            assert_eq!(
                app.settings.attention.rule(&new_key).map(|r| r.level),
                Some(level)
            );
            assert!(app.settings.attention.rule(&old_key).is_none());
            // Every session mirrors the rules, so the other copy moves too.
            assert!(app.sessions[1].attention.rule(&old_key).is_none());
            assert!(app.sessions[1].attention.rule(&new_key).is_some());
        }
    }

    #[test]
    fn protocol_renames_preserve_window_drafts_history_and_placement() {
        for (old, new, channel) in [
            ("alice", "bob", false),
            ("alice", "ALICE", false),
            ("#rust", "#new", true),
            ("#rust", "#RUST", true),
        ] {
            for detached in [false, true] {
                let (mut app, _rx_a, _rx_b) = two_networks();
                let conn = app.sessions[0].id;
                if !channel {
                    assert!(app.sessions[0].open_query(old));
                }
                app.sync_windows();
                let old_key = target_key(&app, conn, old);
                let new_key = old_key;
                let viewport = detached_viewport_id(&old_key);
                let rect =
                    egui::Rect::from_min_size(egui::pos2(30.0, 40.0), egui::vec2(500.0, 400.0));
                {
                    let source = app.windows.get_mut(&old_key).unwrap();
                    source.view.input_text = "unsent draft".into();
                    source.view.command_history = vec!["one".into(), "two".into()];
                    source.view.history_index = Some(0);
                    source.view.history_temp = "draft before browsing".into();
                    source.detached = detached;
                    source.child_rect = Some(rect);
                    source.pending_place = Some(rect);
                    source.os_pos = Some(egui::pos2(123.0, 234.0));
                    source.os_size = Some(egui::vec2(720.0, 480.0));
                }
                app.activate(old_key);
                app.main_window = (!detached).then_some(old_key);
                app.focused_detached = detached.then_some(old_key);
                app.color_target = Some(old_key);
                app.global_dialog_owner = Some(old_key);
                app.pending_focus = Some(old_key);
                app.focus_before_new = Some(old_key);
                app.restore_after_update = Some(old_key);
                app.dialog_owner.insert(conn, old_key);
                app.sessions[0].channel_info_target = old_key.conversation;
                if detached {
                    app.spawn_geometry
                        .insert(old_key, (Some(egui::Pos2::ZERO), None));
                }
                feed(
                    &mut app,
                    0,
                    &if channel {
                        format!(":server RENAME {old} {new} :renamed")
                    } else {
                        format!(":{old}!u@h NICK {new}")
                    },
                );
                app.process_events();
                assert_eq!(app.pending_focus, Some(new_key));
                assert_eq!(app.color_target, Some(new_key));
                assert_eq!(app.global_dialog_owner, Some(new_key));
                assert_eq!(app.focus_before_new, Some(new_key));
                assert_eq!(app.restore_after_update, Some(new_key));
                assert_eq!(app.dialog_owner.get(&conn), Some(&new_key));
                assert_eq!(app.sessions[0].channel_info_target, new_key.conversation);
                assert_eq!(app.sessions[0].id_for_target(new), new_key.conversation);
                assert_eq!(detached_viewport_id(&new_key), viewport);
                assert_eq!(app.focused_detached, detached.then_some(new_key));
                if detached {
                    assert_eq!(
                        app.spawn_geometry.get(&new_key),
                        Some(&(Some(egui::Pos2::ZERO), None))
                    );
                    assert!(app.spawn_geometry.contains_key(&old_key));
                }
                app.sync_windows();
                assert!(app.windows.get(&old_key).is_some());
                let renamed = app.windows.get(&new_key).unwrap();
                assert_eq!(renamed.view.input_text, "unsent draft");
                assert_eq!(renamed.view.command_history, ["one", "two"]);
                assert_eq!(renamed.view.history_index, Some(0));
                assert_eq!(renamed.view.history_temp, "draft before browsing");
                assert_eq!(renamed.detached, detached);
                assert_eq!(renamed.child_rect, Some(rect));
                assert_eq!(renamed.pending_place, Some(rect));
                assert_eq!(renamed.os_pos, Some(egui::pos2(123.0, 234.0)));
                assert_eq!(app.windows.active, Some(new_key));
            }
        }
    }

    #[test]
    fn protocol_rename_collisions_merge_both_drafts_and_histories() {
        for (old, new, channel) in [("alice", "bob", false), ("#rust", "#new", true)] {
            let (mut app, _rx_a, _rx_b) = two_networks();
            let conn = app.sessions[0].id;
            if !channel {
                assert!(app.sessions[0].open_query(old));
                assert!(app.sessions[0].open_query(new));
            }
            feed(
                &mut app,
                0,
                &format!(":peer!u@h PRIVMSG {old} :source content"),
            );
            feed(
                &mut app,
                0,
                &format!(":peer!u@h PRIVMSG {new} :destination content"),
            );
            app.sync_windows();
            let old_key = target_key(&app, conn, old);
            let retired_key = target_key(&app, conn, new);
            let new_key = old_key;
            {
                let source = app.windows.get_mut(&old_key).unwrap();
                source.view.input_text = "stale source input".into();
                source.view.command_history = vec!["source command".into()];
                source.detached = true;
                source.os_pos = Some(egui::pos2(40.0, 50.0));
            }
            {
                let destination = app.windows.get_mut(&retired_key).unwrap();
                destination.view.input_text = "destination draft".into();
                destination.view.command_history = vec!["destination command".into()];
                destination.os_pos = Some(egui::pos2(900.0, 900.0));
            }
            app.pending_inputs
                .push((conn, old_key.conversation, "queued source draft".into()));
            app.activate(old_key);
            feed(
                &mut app,
                0,
                &if channel {
                    format!(":server RENAME {old} {new} :renamed")
                } else {
                    format!(":{old}!u@h NICK {new}")
                },
            );
            app.process_events();
            app.sync_windows();
            assert_eq!(app.sessions[0].id_for_target(new), new_key.conversation);
            assert!(app.windows.get(&retired_key).is_none());
            let renamed = app.windows.get(&new_key).unwrap();
            assert_eq!(
                renamed.view.input_text,
                "queued source draft\ndestination draft"
            );
            assert_eq!(
                renamed.view.command_history,
                ["source command", "destination command"]
            );
            assert!(renamed.detached);
            assert_eq!(renamed.os_pos, Some(egui::pos2(40.0, 50.0)));
            assert!(app.pending_inputs.is_empty());
            let rows = &app.sessions[0].conversations[new].messages;
            assert!(rows.iter().any(|row| row.content == "source content"));
            assert!(rows.iter().any(|row| row.content == "destination content"));
        }
    }

    #[test]
    fn a_rename_before_the_first_window_sync_preserves_queued_input() {
        let mut app = test_app();
        let _rx = connect(&mut app, 0);
        let conn = app.sessions[0].id;
        assert!(app.sessions[0].open_query("alice"));
        app.pending_inputs.push((
            conn,
            app.sessions[0].id_for_target("alice"),
            "queued draft".into(),
        ));
        feed(&mut app, 0, ":alice!u@h NICK ALICE");
        app.process_events();
        app.sync_windows();
        let renamed = target_key(&app, conn, "ALICE");
        assert_eq!(
            app.windows.get(&renamed).unwrap().view.input_text,
            "queued draft"
        );
        assert!(app.pending_inputs.is_empty());
    }

    #[test]
    fn closing_and_reopening_a_name_cannot_reuse_its_view_or_stale_actions() {
        let (mut app, mut rx_a, _rx_b) = two_networks();
        let conn = app.sessions[0].id;
        assert!(app.sessions[0].open_query("alice"));
        app.sync_windows();
        let old = target_key(&app, conn, "alice");
        app.pending_focus = Some(old);
        app.windows.get_mut(&old).unwrap().view.input_text = "/quit stale action".into();
        app.sessions[0].close_tab("alice");
        assert!(app.sessions[0].open_query("alice"));
        let reopened = target_key(&app, conn, "alice");
        assert_ne!(old, reopened);
        assert_ne!(detached_viewport_id(&old), detached_viewport_id(&reopened));
        drain(&mut rx_a);
        // The obsolete window can still exist until this frame's sync, but
        // its ID must not operate on the newly opened conversation.
        app.submit_window_input(&old);
        app.window_command(WindowCommand::Close, old);
        app.window_command(WindowCommand::New, old);
        assert_eq!(app.sessions.len(), 2);
        assert!(app.sessions[0].is_connected());
        assert!(drain(&mut rx_a).is_empty());
        app.sync_windows();
        assert!(app.windows.get(&old).is_none());
        assert!(app.pending_focus.is_none());
        assert!(
            app.windows
                .get(&reopened)
                .unwrap()
                .view
                .input_text
                .is_empty()
        );
    }

    #[test]
    fn a_status_sourced_server_switch_cannot_restore_a_retired_selection() {
        let mut app = test_app();
        app.sessions[0].apply_unprofiled_endpoint("a.example", "6697", true);
        assert!(app.sessions[0].start_session_from_form());
        let _rx = connect(&mut app, 0);
        assert!(app.sessions[0].open_query("alice"));
        app.sync_windows();
        let conn = app.sessions[0].id;
        let old = target_key(&app, conn, "alice");
        app.activate(old);
        app.sessions[0].test_set_disconnected();
        let status = WindowKey::new(conn, None);
        app.windows.get_mut(&status).unwrap().view.input_text = "/server b.example 6697".into();

        app.submit_window_input(&status);
        assert_eq!(app.sessions[0].current_conversation, None);
        assert!(
            app.sessions[0]
                .target_name(old.conversation.unwrap())
                .is_none()
        );
        app.process_events();
        app.sync_windows();
        assert_eq!(app.windows.active, Some(status));
        let _new_rx = connect(&mut app, 0);
        feed(&mut app, 0, ":me!u@h JOIN #new");
        app.process_events();
        app.sync_windows();
        let new = target_key(&app, conn, "#new");
        assert_ne!(old, new);
        assert_eq!(app.sessions[0].current_conversation, new.conversation);
        assert_eq!(app.windows.active, Some(new));
    }

    #[test]
    fn an_endpoint_change_retires_target_ui_work_and_resets_status_editing() {
        let mut app = test_app();
        app.sessions[0].apply_unprofiled_endpoint("a.example", "6697", true);
        assert!(app.sessions[0].start_session_from_form());
        let _rx = connect(&mut app, 0);
        assert!(app.sessions[0].open_query("alice"));
        app.sync_windows();
        let conn = app.sessions[0].id;
        let old = target_key(&app, conn, "alice");
        let status = WindowKey::new(conn, None);
        {
            let window = app.windows.get_mut(&status).unwrap();
            window.view.input_text = "private status draft".into();
            window.view.command_history = vec!["private status command".into()];
            window.os_pos = Some(egui::pos2(40.0, 80.0));
        }
        app.pending_focus = Some(old);
        app.color_target = Some(status);
        app.global_dialog_owner = Some(old);
        app.focus_before_new = Some(old);
        app.restore_after_update = Some(old);
        app.focused_detached = Some(old);
        app.dialog_owner.insert(conn, old);
        app.spawn_geometry.insert(old, (None, None));
        app.spawn_geometry
            .insert(status, (Some(egui::pos2(40.0, 80.0)), None));
        app.pending_inputs
            .push((conn, old.conversation, "private queued draft".into()));
        app.sessions[0].show_color_picker = true;
        app.sessions[0].request_manual_disconnect(None);
        app.sessions[0].test_set_disconnected();
        app.sessions[0].apply_unprofiled_endpoint("b.example", "6697", true);
        assert!(app.sessions[0].start_session_from_form());
        let new = WindowKey::new(conn, Some(app.sessions[0].ensure_target("alice")));
        assert_ne!(new, old);
        app.process_events();
        app.sync_windows();
        assert!(app.pending_focus.is_none());
        assert!(app.pending_inputs.is_empty());
        assert!(app.color_target.is_none());
        assert!(app.global_dialog_owner.is_none());
        assert!(app.focus_before_new.is_none());
        assert!(app.restore_after_update.is_none());
        assert!(app.focused_detached.is_none());
        assert!(!app.dialog_owner.contains_key(&conn));
        assert!(!app.spawn_geometry.contains_key(&old));
        assert!(app.spawn_geometry.contains_key(&status));
        assert!(!app.sessions[0].show_color_picker);
        let status = app.windows.get(&status).unwrap();
        assert!(status.view.input_text.is_empty());
        assert!(status.view.command_history.is_empty());
        assert_eq!(status.os_pos, Some(egui::pos2(40.0, 80.0)));
        let new = app.windows.get(&new).unwrap();
        assert!(new.view.input_text.is_empty());
        assert!(!new.view.input_focus_request);
    }

    #[test]
    fn cycling_retains_the_conversation_window_across_part_and_join() {
        let (mut app, mut rx_a, _rx_b) = two_networks();
        let conn = app.sessions[0].id;
        let conversation = target_key(&app, conn, "#rust");
        let viewport = detached_viewport_id(&conversation);
        {
            let window = app.windows.get_mut(&conversation).unwrap();
            window.detached = true;
            window.view.input_text = "kept while cycling".into();
            window.view.command_history = vec!["earlier line".into()];
        }
        app.sessions[0].process_command("/cycle");
        assert_eq!(drain(&mut rx_a), ["PART #rust :Cycling", "JOIN #rust"]);
        feed(&mut app, 0, ":me!u@h PART #rust :Cycling");
        app.process_events();
        app.sync_windows();
        assert_eq!(target_key(&app, conn, "#rust"), conversation);
        assert!(!app.sessions[0].conversations["#rust"].joined);
        feed(&mut app, 0, ":me!u@h JOIN #rust");
        app.process_events();
        app.sync_windows();
        assert_eq!(target_key(&app, conn, "#rust"), conversation);
        assert_eq!(detached_viewport_id(&conversation), viewport);
        let window = app.windows.get(&conversation).unwrap();
        assert!(window.detached);
        assert_eq!(window.view.input_text, "kept while cycling");
        assert_eq!(window.view.command_history, ["earlier line"]);
        assert!(app.sessions[0].conversations["#rust"].joined);
    }

    #[test]
    fn embedded_detached_shortcuts_are_not_applied_twice() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let source = target_key(&app, app.sessions[1].id, "#debian");
        app.toggle_detached(&source);
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![
                    egui::Event::ModifiersChanged(egui::Modifiers::CTRL),
                    shortcut_event(egui::Key::M, egui::Modifiers::CTRL),
                ],
                ..Default::default()
            },
            |ui| app.ui(ui),
        );
        output.textures_delta.clear();
        assert_eq!(app.windows.mode, LayoutMode::Floating);
    }

    #[test]
    fn native_detached_shortcuts_respect_modals_and_drain_link_output() {
        let (mut app, _rx_a, mut rx_b) = two_networks();
        let source = target_key(&app, app.sessions[1].id, "#debian");
        app.toggle_detached(&source);
        app.show_settings = true;
        let output = native_detached_frame(
            &mut app,
            &egui::Context::default(),
            &source,
            vec![shortcut_event(egui::Key::W, egui::Modifiers::CTRL)],
            true,
            true,
        );
        assert!(drain(&mut rx_b).is_empty());
        assert!(
            output
                .iter()
                .all(|command| !matches!(command, egui::OutputCommand::OpenUrl(_)))
        );
    }

    #[test]
    fn detached_search_and_files_requests_claim_their_source_when_another_panel_is_open() {
        for files in [false, true] {
            let (mut app, _rx_a, _rx_b) = two_networks();
            let source = target_key(&app, app.sessions[1].id, "#debian");
            app.windows.get_mut(&source).unwrap().detached = true;
            let attached = target_key(&app, app.sessions[0].id, "#rust");
            app.activate(attached);
            app.search.open = true;
            app.global_dialogs_were_open = true;
            app.global_dialog_owner = None;
            app.sessions[1].events.push(if files {
                SessionEvent::UploadRequested {
                    conversation: source.conversation,
                    source: super::super::uploads::UploadSource::Files(Vec::new()),
                    composer_nonce: app
                        .windows
                        .get(&source)
                        .unwrap()
                        .view
                        .composer
                        .nonce
                        .clone(),
                }
            } else {
                SessionEvent::SearchRequested {
                    query: "needle".into(),
                    all_networks: false,
                    conversation: source.conversation,
                }
            });
            app.process_events();
            app.update_global_dialog_owner();
            assert_eq!(app.global_dialog_window(), Some(source));
            assert_eq!(app.windows.active, Some(attached));

            // This is the dialog draw path used when only detached native
            // windows remain visible and the root window is minimized.
            let ctx = egui::Context::default();
            let mut output = None;
            for _ in 0..2 {
                let mut frame = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(900.0, 700.0),
                        )),
                        ..Default::default()
                    },
                    |ui| app.show_detached_window_dialogs(ui.ctx(), &source),
                );
                frame.textures_delta.clear();
                output = Some(frame);
            }
            let text = texts(output.as_ref().unwrap());
            assert!(text.iter().any(|text| text
                == if files {
                    "File sharing"
                } else {
                    "History search"
                }));
        }
    }

    #[test]
    fn an_attached_search_request_reclaims_panels_from_an_old_detached_owner() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let detached = target_key(&app, app.sessions[1].id, "#debian");
        app.windows.get_mut(&detached).unwrap().detached = true;
        let attached = target_key(&app, app.sessions[0].id, "#rust");
        app.activate(detached);
        app.search.open = true;
        app.global_dialog_owner = Some(detached);
        app.global_dialogs_were_open = true;
        app.sessions[0].events.push(SessionEvent::SearchRequested {
            query: "needle".into(),
            all_networks: false,
            conversation: attached.conversation,
        });
        app.process_events();
        app.update_global_dialog_owner();
        assert_eq!(app.global_dialog_window(), None);

        // Configurable Search actions call open_search directly, bypassing
        // SessionEvent; they use the same explicit conversation scope.
        app.global_dialog_owner = Some(detached);
        app.open_search(0, String::new(), true);
        app.update_global_dialog_owner();
        assert_eq!(app.global_dialog_window(), None);
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
        app.sessions[0].server_host = "a.example".into();
        app.sessions[second].server_host = "b.example".into();
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
        assert!(!app.sessions[0].is_connecting());
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
                .all(|s| s.is_connecting() && !s.show_connect_dialog)
        );
        assert!(app.sessions.iter().all(|s| s.profile_id.is_some()));
        assert_ne!(app.sessions[0].profile_id, app.sessions[1].profile_id);
        assert_eq!(app.network_label(&app.sessions[1]), "C");
    }

    #[test]
    fn input_typed_in_one_window_is_sent_to_that_windows_session_and_target() {
        let (mut app, mut rx_a, mut rx_b) = two_networks();
        let conn_b = app.sessions[1].id;
        let key = target_key(&app, conn_b, "#debian");
        app.windows.get_mut(&key).unwrap().view.input_text = "hello debian".into();
        app.submit_window_input(&key);

        assert!(drain(&mut rx_a).is_empty());
        assert_eq!(drain(&mut rx_b), ["PRIVMSG #debian :hello debian"]);
        assert!(app.windows.get(&key).unwrap().view.input_text.is_empty());
        // The other session's own selection is untouched.
        assert_eq!(app.sessions[0].current_target_name(), Some("#rust"));
    }

    #[test]
    fn each_window_keeps_its_own_unsent_input() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        let debian = target_key(&app, app.sessions[1].id, "#debian");
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

    /// Recall the input line in one window the way the Up key does,
    /// returning what the user would see (and could send) there.
    fn recall_in(app: &mut App, key: &WindowKey) -> String {
        let index = app.session_index(key.conn).unwrap();
        let window = app.windows.get_mut(key).unwrap();
        with_window_view(
            &mut app.sessions[index],
            &mut window.view,
            &key.conversation,
            |s| {
                s.history_up();
                s.input_text.clone()
            },
        )
        .1
    }

    #[test]
    fn drafts_sends_and_history_stay_in_their_window_and_network() {
        let (mut app, mut rx_a, mut rx_b) = two_networks();
        // The same channel name on both networks: only the connection key
        // tells the two #same windows apart.
        feed(&mut app, 0, ":me!u@h JOIN #same");
        feed(&mut app, 1, ":me!u@h JOIN #same");
        app.sync_windows();
        let a_rust = target_key(&app, app.sessions[0].id, "#rust");
        let a_same = target_key(&app, app.sessions[0].id, "#same");
        let b_same = target_key(&app, app.sessions[1].id, "#same");
        let b_debian = target_key(&app, app.sessions[1].id, "#debian");
        let draft =
            |app: &App, key: &WindowKey| app.windows.get(key).unwrap().view.input_text.clone();
        app.windows.get_mut(&a_rust).unwrap().view.input_text = "for rust only".into();
        app.windows.get_mut(&a_same).unwrap().view.input_text = "net A same draft".into();
        app.windows.get_mut(&b_same).unwrap().view.input_text = "net B same draft".into();
        app.windows.get_mut(&b_debian).unwrap().view.input_text = "for debian only".into();

        // A real frame draws every window; no draft moves.
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        assert_eq!(draft(&app, &a_rust), "for rust only");
        assert_eq!(draft(&app, &a_same), "net A same draft");
        assert_eq!(draft(&app, &b_same), "net B same draft");
        assert_eq!(draft(&app, &b_debian), "for debian only");

        // Submitting net B's #same (while net A's window is the active one)
        // puts the text on net B's wire, addressed to net B's #same only.
        app.activate(a_rust);
        app.submit_window_input(&b_same);
        assert!(drain(&mut rx_a).is_empty());
        assert_eq!(drain(&mut rx_b), ["PRIVMSG #same :net B same draft"]);
        assert_eq!(draft(&app, &b_same), "");
        assert_eq!(draft(&app, &a_same), "net A same draft");

        // Net A's #same sends on net A's wire only.
        app.submit_window_input(&a_same);
        assert_eq!(drain(&mut rx_a), ["PRIVMSG #same :net A same draft"]);
        assert!(drain(&mut rx_b).is_empty());

        // History is per window: net B's #same recalls its own sent line,
        // net A's #same recalls its own, and windows that sent nothing
        // recall nothing from anywhere else.
        app.windows.get_mut(&b_same).unwrap().view.input_text = "b typing".into();
        assert_eq!(recall_in(&mut app, &b_same), "net B same draft");
        app.windows.get_mut(&a_same).unwrap().view.input_text = "a typing".into();
        assert_eq!(recall_in(&mut app, &a_same), "net A same draft");
        assert_eq!(recall_in(&mut app, &b_debian), "for debian only");
        assert_eq!(recall_in(&mut app, &a_rust), "for rust only");
        // Recalling never sends anything by itself.
        assert!(drain(&mut rx_a).is_empty() && drain(&mut rx_b).is_empty());

        // A detached window on net B is its own OS window: its draft still
        // belongs to it alone through real frames, and submitting it still
        // goes only to net B's #debian.
        app.toggle_detached(&b_debian);
        app.windows.get_mut(&b_debian).unwrap().view.input_text = "detached draft".into();
        frame(&mut app, &ctx);
        assert_eq!(draft(&app, &b_debian), "detached draft");
        assert_eq!(draft(&app, &a_rust), "for rust only");
        app.submit_window_input(&b_debian);
        assert!(drain(&mut rx_a).is_empty());
        assert_eq!(drain(&mut rx_b), ["PRIVMSG #debian :detached draft"]);
    }

    #[test]
    fn restored_draft_lands_in_its_own_window_even_if_selection_moved() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let debian = target_key(&app, app.sessions[1].id, "#debian");
        app.activate(debian);
        app.windows.get_mut(&debian).unwrap().view.input_text = "unsent".into();
        let json = serde_json::to_string(&app.update_session()).unwrap();
        let snapshot: UpdateSession = serde_json::from_str(&json).unwrap();

        let mut restored = App::with_settings(test_settings(), LayoutFile::default());
        restored.start(Some(snapshot));
        // The session's selection moved before the first window sync.
        restored.sessions[1].select_conversation(None);
        restored.sync_windows();
        let key = target_key(&restored, restored.sessions[1].id, "#debian");
        assert_eq!(
            restored.windows.get(&key).unwrap().view.input_text,
            "unsent"
        );
        let status = WindowKey::new(restored.sessions[1].id, None);
        assert_eq!(restored.windows.get(&status).unwrap().view.input_text, "");
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
                target_key(&app, a, "#alpha"),
                target_key(&app, a, "#rust"),
                WindowKey::new(b, None),
                target_key(&app, b, "#debian"),
            ]
        );
        app.activate(order[0]);
        let mut visited = Vec::new();
        for _ in 0..order.len() {
            app.cycle_window(false);
            visited.push(app.windows.active.unwrap());
        }
        let mut expected = order[1..].to_vec();
        expected.push(order[0]);
        assert_eq!(visited, expected);
    }

    #[test]
    fn closing_a_joined_channel_window_sends_part_and_keeps_it_until_confirmed() {
        let (mut app, mut rx_a, _rx_b) = two_networks();
        let key = target_key(&app, app.sessions[0].id, "#rust");
        app.request_close(key);
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
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        app.activate(rust);
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
    fn perform_while_idle_goes_to_the_favorite_in_the_form_not_the_one_last_connected() {
        let mut settings = test_settings();
        for (name, host, list) in [
            ("A", "irc.a.example", "/msg NickServ IDENTIFY apass"),
            ("B", "irc.b.example", "/msg NickServ IDENTIFY bpass"),
        ] {
            settings.server_favorites.push(ServerFavorite {
                id: name.to_lowercase(),
                name: name.into(),
                host: host.into(),
                port: "6697".into(),
                use_tls: true,
                auto_perform: list.into(),
                ..ServerFavorite::default()
            });
        }
        let mut app = App::with_settings(settings, LayoutFile::default());
        app.start(None);
        let a = app.settings.server_favorites[0].clone();
        let b = app.settings.server_favorites[1].clone();
        app.sessions[0].load_favorite(&a);
        assert!(app.sessions[0].start_session_from_form());
        app.sessions[0].process_command("/disconnect");
        app.process_events();

        // Pick B in the form, but do not connect.
        app.sessions[0].load_favorite(&b);
        app.sessions[0].process_command("/perform /join #b");
        app.process_events();

        assert_eq!(
            app.settings.server_favorites[0].auto_perform,
            "/msg NickServ IDENTIFY apass"
        );
        assert_eq!(
            app.settings.server_favorites[1].auto_perform,
            "/msg NickServ IDENTIFY bpass\n/join #b"
        );
    }

    #[test]
    fn perform_after_server_switch_never_reaches_the_old_favorite_or_defaults() {
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

        // Switch to another network, then identify there.
        app.sessions[0].process_command("/server irc.other.example 6697");
        app.sessions[0].process_command("/perform /msg NickServ IDENTIFY otherpass");
        app.process_events();

        let saved = app.settings_file.clone();
        let favorite_list = |app: &App| app.settings.server_favorites[0].auto_perform.clone();
        assert_eq!(favorite_list(&app), "/msg NickServ IDENTIFY oftcpass");
        assert_eq!(
            app.settings.auto_perform,
            "/msg NickServ IDENTIFY liberapass"
        );
        if let Some(saved) = saved {
            assert!(!saved.auto_perform.contains("otherpass"));
            assert!(
                saved
                    .server_favorites
                    .iter()
                    .all(|favorite| !favorite.auto_perform.contains("otherpass"))
            );
        }
        // The line is session-only, and says so.
        assert!(
            app.sessions[0]
                .server_messages
                .iter()
                .any(|m| m.content.contains("this connection only"))
        );
        assert_eq!(
            app.sessions[0]
                .active_session
                .as_ref()
                .unwrap()
                .auto_perform,
            "/msg NickServ IDENTIFY otherpass"
        );
        // The form still describes the saved profile.
        assert_eq!(
            app.sessions[0].auto_perform,
            "/msg NickServ IDENTIFY oftcpass"
        );

        // Switching to the Quick Connect defaults' own endpoint persists
        // there (and only there).
        app.sessions[0].process_command("/server irc.libera.chat 6697");
        app.sessions[0].process_command("/perform /join #libera");
        app.process_events();
        assert_eq!(app.settings.auto_perform, "/join #libera");
        assert_eq!(favorite_list(&app), "/msg NickServ IDENTIFY oftcpass");
    }

    #[test]
    fn detaching_the_active_window_activates_the_one_the_main_window_shows() {
        let (mut app, mut rx_a, mut rx_b) = two_networks();
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        let debian = target_key(&app, app.sessions[1].id, "#debian");
        app.activate(rust);
        app.activate(debian);
        app.toggle_detached(&debian);
        let shown = app.maximized_window().unwrap();
        assert_ne!(shown, debian);
        assert_eq!(app.windows.active, Some(shown));
        // Ctrl+W in the main window closes what it shows, not the detached one.
        app.request_close(app.windows.active.unwrap());
        assert!(
            drain(&mut rx_b).is_empty(),
            "the detached channel was parted"
        );
        let _ = drain(&mut rx_a);

        // Working in the main window again after the detached one took over.
        app.activate(debian);
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
        let debian = target_key(&app, app.sessions[1].id, "#debian");
        app.activate(debian);
        app.request_close(debian);
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
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        app.user_activate(rust);
        // Bob writes much later.
        feed(&mut app, 0, ":bob!u@h PRIVMSG me :hi");
        app.sync_windows();
        assert_eq!(app.windows.active, Some(rust));
        assert_eq!(app.sessions[0].current_target_name(), Some("#rust"));
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
        let debian = target_key(&app, app.sessions[1].id, "#debian");
        assert_eq!(app.windows.active, Some(debian));
        assert_eq!(app.sessions[1].current_target_name(), Some("#debian"));
    }

    #[test]
    fn the_unified_connection_editor_stays_visible_when_selection_changes() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        app.new_connection();
        let edited = app.settings_session;
        assert!(app.dialog_blocks_shortcuts());
        app.user_activate(target_key(&app, app.sessions[0].id, "#rust"));
        // Settings is one visible, pinned editor rather than an active-only
        // Connect popup, so shortcuts stay blocked until it is dismissed.
        assert!(app.show_settings);
        assert_eq!(app.settings_session, edited);
        assert!(app.dialog_blocks_shortcuts());
        app.cancel_settings_window();
        assert!(!app.dialog_blocks_shortcuts());
    }

    #[test]
    fn input_focus_follows_a_window_switch() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        let debian = target_key(&app, app.sessions[1].id, "#debian");
        app.activate(rust);
        app.windows.get_mut(&rust).unwrap().view.input_had_focus = true;
        app.cycle_window(false);
        let next = app.windows.active.unwrap();
        assert!(app.windows.get(&next).unwrap().view.input_focus_request);

        // Drawing the window hands the request to its input line.
        let ctx = egui::Context::default();
        app.activate(debian);
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
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        app.activate(rust);
        app.windows.get_mut(&rust).unwrap().view.input_had_focus = true;
        app.windows.get_mut(&rust).unwrap().view.input_text = "/query carol".into();
        app.submit_window_input(&rust);
        app.sync_windows();
        let carol = target_key(&app, app.sessions[0].id, "carol");
        assert_eq!(app.windows.active, Some(carol));
        assert!(app.windows.get(&carol).unwrap().view.input_focus_request);
    }

    #[test]
    fn closing_an_idle_status_window_cancels_its_pending_reconnect() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let conn = app.sessions[1].id;
        app.sessions[1].test_set_disconnected();
        app.sessions[1].apply_unprofiled_endpoint("irc.example", "6697", true);
        assert!(app.sessions[1].start_session_from_form());
        app.sessions[1].test_set_disconnected();
        app.sessions[1].mark_connection_lost();
        app.sessions[1].test_set_retry_state(0, None); // retry due now
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
        app.sessions[0].test_set_disconnected();
        app.sessions[0].apply_unprofiled_endpoint("irc.example", "6697", true);
        assert!(app.sessions[0].start_session_from_form());
        app.sessions[0].test_set_disconnected();
        app.sessions[0].mark_connection_lost();
        app.sessions[0].test_set_retry_state(0, Some(Instant::now()));
        assert!(app.sessions[0].awaiting_reconnect());
        // Session B tries the same server meanwhile.
        app.sessions[1].request_manual_disconnect(None);
        app.sessions[1].test_set_disconnected();
        app.sessions[1].apply_unprofiled_endpoint("irc.example", "6697", true);
        assert!(app.sessions[1].start_session_from_form());

        app.run_lifecycles(&egui::Context::default());
        assert!(!app.sessions[1].is_connecting());
        assert!(!app.sessions[1].has_worker());
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
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        let carol = target_key(&app, app.sessions[0].id, "carol");
        app.activate(rust);
        app.windows.get_mut(&carol).unwrap().view.input_text = "/close".into();
        app.submit_window_input(&carol);
        app.sync_windows();
        assert!(app.windows.get(&carol).is_none());
        assert_eq!(app.windows.active, Some(rust));
        assert_eq!(app.sessions[0].current_target_name(), Some("#rust"));
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
        let debian = target_key(&app, app.sessions[1].id, "#debian");
        app.user_activate(debian);
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
        let key = target_key(&app, app.sessions[0].id, "friend");
        assert!(app.windows.get(&key).is_some());
        app.request_close(key);
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
        assert!(!app.sessions[1].is_connected());
        // The worker is gone (none in tests): the session and its windows go.
        app.run_lifecycles(&egui::Context::default());
        app.sync_windows();
        assert_eq!(app.sessions.len(), 1);
        assert!(app.windows.windows.iter().all(|w| w.key.conn != conn_b));
        // The other network is untouched.
        assert!(app.sessions[0].is_connected());
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
        assert!(app.sessions[1].is_connecting());
        // The first session keeps its connection and endpoint.
        assert!(app.sessions[0].is_connected());
        assert_eq!(app.active_index(), 1);
    }

    fn quick_connect_defaults() -> Settings {
        let mut settings = test_settings();
        settings.server_host = "irc.libera.example".into();
        settings.password = "hunter2".into();
        settings.sasl_username = "acct".into();
        settings.sasl_password = "saslpw".into();
        settings.auto_perform = "/msg NickServ IDENTIFY hunter2".into();
        settings
    }

    fn assert_quick_connect_defaults_intact(app: &App) {
        let saved = app.settings_file.clone().expect("shutdown saved settings");
        for settings in [&saved, &app.settings] {
            assert_eq!(settings.server_host, "irc.libera.example");
            assert_eq!(settings.password, "hunter2");
            assert_eq!(settings.sasl_username, "acct");
            assert_eq!(settings.sasl_password, "saslpw");
            assert_eq!(settings.auto_perform, "/msg NickServ IDENTIFY hunter2");
        }
    }

    #[test]
    fn shutdown_does_not_adopt_a_new_sessions_connect_form() {
        let mut app = App::with_settings(quick_connect_defaults(), LayoutFile::default());
        app.start(None);
        let _rx = connect(&mut app, 0);
        app.sessions[0].process_command("/server -m irc.oftc.example 6697");
        app.process_events();
        assert_eq!(app.active_index(), 1);

        app.prepare_update_shutdown();

        assert_quick_connect_defaults_intact(&app);
    }

    #[test]
    fn shutdown_does_not_adopt_a_scratch_settings_edit() {
        let mut app = App::with_settings(quick_connect_defaults(), LayoutFile::default());
        app.start(None);
        let conn = app.sessions[0].id;
        app.open_settings(conn);
        app.sessions[0].server_host = "scratch.example".into();
        app.sessions[0].nickname = "scratchnick".into();
        app.sessions[0].sasl_username.clear();
        app.sessions[0].sasl_password.clear();

        app.prepare_update_shutdown();

        assert_quick_connect_defaults_intact(&app);
        assert_eq!(app.settings.nickname, "me");
    }

    #[test]
    fn shutdown_still_flushes_behaviour_settings() {
        let mut app = App::with_settings(quick_connect_defaults(), LayoutFile::default());
        app.start(None);
        app.sessions[0].quit_message = "bye for now".into();

        app.prepare_update_shutdown();

        assert_eq!(app.settings_file.unwrap().quit_message, "bye for now");
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
        assert!(!app.sessions[second].is_connecting());
        assert!(!app.sessions[second].has_worker());
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
            Some(target_key(&app, app.sessions[0].id, "#rust"))
        );
    }

    #[test]
    fn a_join_in_a_background_session_does_not_steal_the_active_window() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        app.activate(rust);
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
            app.sessions[0].conversations["#rust"]
                .messages
                .iter()
                .all(|m| m.content != "spam")
        );
    }

    #[test]
    fn update_snapshot_carries_every_session_and_restores_them() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let debian = target_key(&app, app.sessions[1].id, "#debian");
        app.activate(debian);
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
        assert!(restored.sessions.iter().all(|s| s.is_connecting()));
        let key = target_key(&restored, restored.sessions[1].id, "#debian");
        assert_eq!(
            restored.windows.get(&key).unwrap().view.input_text,
            "unsent"
        );
        assert_eq!(restored.windows.active, Some(key));
    }

    #[test]
    fn update_snapshot_restores_every_windows_draft_including_pending_input() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        feed(&mut app, 0, ":me!u@h JOIN #same");
        feed(&mut app, 1, ":me!u@h JOIN #same");
        app.sessions[0].open_query("alice");
        app.sync_windows();
        let detached = target_key(&app, app.sessions[0].id, "#same");
        app.toggle_detached(&detached);
        let mut expected = Vec::new();
        for window in &mut app.windows.windows {
            let index = usize::from(window.key.conn == app.sessions[1].id);
            let text = format!("draft {index} {:?}", window.key.conversation);
            window.view.input_text = text.clone();
            expected.push((
                index,
                window
                    .key
                    .conversation
                    .and_then(|id| app.sessions[index].target_name(id))
                    .map(str::to_string),
                text,
            ));
        }
        // This tab exists in the session but its window has not been synced.
        feed(&mut app, 0, ":me!u@h JOIN #pending");
        app.pending_inputs.push((
            app.sessions[0].id,
            app.sessions[0].id_for_target("#pending"),
            "queued draft".into(),
        ));
        expected.push((0, Some("#pending".into()), "queued draft".into()));
        let selected = target_key(&app, app.sessions[1].id, "#debian");
        app.activate(selected);
        let legacy_input = app.windows.get(&selected).unwrap().view.input_text.clone();

        let json = serde_json::to_string(&app.update_session()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["input"], legacy_input);
        assert!(value["drafts"].is_array());
        let snapshot = serde_json::from_str(&json).unwrap();
        let mut restored = App::with_settings(test_settings(), LayoutFile::default());
        restored.start(Some(snapshot));
        restored.sync_windows();
        for (index, target, text) in expected {
            let key = match target {
                Some(target) => target_key(&restored, restored.sessions[index].id, &target),
                None => WindowKey::new(restored.sessions[index].id, None),
            };
            assert_eq!(restored.windows.get(&key).unwrap().view.input_text, text);
        }
    }

    #[test]
    fn new_snapshot_drafts_override_legacy_input_even_when_empty() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let mut snapshot = app.update_session();
        snapshot.set_input("stale legacy draft".into());
        snapshot.set_drafts(Vec::new());
        let mut restored = App::with_settings(test_settings(), LayoutFile::default());
        restored.start(Some(snapshot));
        restored.sync_windows();
        assert!(
            restored
                .windows
                .windows
                .iter()
                .all(|window| window.view.input_text.is_empty())
        );
    }

    #[test]
    fn an_update_during_a_server_switch_does_not_restore_old_server_drafts() {
        let mut app = test_app();
        app.sessions[0].apply_unprofiled_endpoint("a.example", "6697", true);
        assert!(app.sessions[0].start_session_from_form());
        let _rx = connect(&mut app, 0);
        feed(&mut app, 0, ":me!u@h JOIN #secret-a");
        app.sync_windows();
        let conn = app.sessions[0].id;
        let channel = target_key(&app, conn, "#secret-a");
        app.activate(channel);
        for window in &mut app.windows.windows {
            window.view.input_text = "private draft for A".into();
        }
        app.pending_inputs
            .push((conn, Some(ConversationId(9999)), "queued A draft".into()));
        app.sessions[0].apply_unprofiled_endpoint("b.example", "6697", true);
        assert!(app.sessions[0].switch_session_from_form("Changing servers"));
        assert!(app.sessions[0].update_changes_endpoint());

        let snapshot = app.update_session();
        let value = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(value["session"]["server"]["host"], "b.example");
        assert_eq!(value["input"], "");
        assert_eq!(value["drafts"], serde_json::json!([]));
        let mut restored = App::with_settings(test_settings(), LayoutFile::default());
        restored.start(Some(snapshot));
        restored.sync_windows();
        assert!(restored.sessions[0].conversations.is_empty());
        assert!(
            restored
                .windows
                .windows
                .iter()
                .all(|window| window.view.input_text.is_empty())
        );
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
        assert!(app.sessions[0].is_connecting());
        let key = target_key(&app, app.sessions[0].id, "#rust");
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
        let mut json: serde_json::Value =
            serde_json::from_str(include_str!("testdata/update_session_0_1_4.json")).unwrap();
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
        let secret = target_key(&app, conn, "#secret");
        assert_eq!(app.windows.active, Some(secret));
        assert_eq!(
            app.windows.get(&secret).unwrap().view.input_text,
            "half typed draft"
        );
        assert!(app.windows.get(&target_key(&app, conn, "carol")).is_some());
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
    fn auto_connect_ticks_come_back_after_0_1_4_rewrote_settings() {
        let favorite = |name: &str, host: &str, auto: bool| ServerFavorite {
            name: name.into(),
            host: host.into(),
            port: "6697".into(),
            use_tls: true,
            auto_connect: auto,
            ..ServerFavorite::default()
        };
        // This version ticked Libera; layout.json keeps a copy.
        let mut settings = test_settings();
        settings.server_favorites = vec![
            favorite("Libera", "irc.libera.chat", true),
            favorite("OFTC", "irc.oftc.net", false),
        ];
        let layout = App::with_settings(settings.clone(), LayoutFile::default()).layout_file();
        assert_eq!(
            layout.auto_connect,
            [crate::gui::derived_profile_id(
                "Libera",
                "irc.libera.chat",
                "6697"
            )]
        );
        let layout: LayoutFile =
            serde_json::from_str(&serde_json::to_string(&layout).unwrap()).unwrap();

        // 0.1.4 saved settings.json without `id` and `auto_connect`.
        let mut rewritten = settings.clone();
        for f in &mut rewritten.server_favorites {
            f.id.clear();
            f.auto_connect = false;
        }
        let app = App::with_settings(rewritten, layout.clone());
        let ticks: Vec<bool> = app
            .settings
            .server_favorites
            .iter()
            .map(|f| f.auto_connect)
            .collect();
        assert_eq!(ticks, [true, false]);

        // A favorite this version saved (it has its id) keeps its own tick.
        let mut untick = App::with_settings(settings, LayoutFile::default())
            .settings
            .clone();
        untick.server_favorites[0].auto_connect = false;
        let app = App::with_settings(untick, layout);
        assert!(!app.settings.server_favorites[0].auto_connect);
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
        let debian = target_key(&app, app.sessions[1].id, "#debian");
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
    fn a_degenerate_viewport_leaves_floating_windows_and_the_saved_layout_alone() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        feed(&mut app, 0, ":alice!u@h PRIVMSG #rust :one");
        feed(&mut app, 1, ":bob!u@h PRIVMSG #debian :two");
        app.windows.mode = LayoutMode::Floating;
        app.tile_request = Some(TileRequest::Tile);
        let ctx = egui::Context::default();
        for _ in 0..5 {
            frame(&mut app, &ctx);
        }
        let rects = |app: &App| -> Vec<Option<egui::Rect>> {
            app.windows.windows.iter().map(|w| w.child_rect).collect()
        };
        let settled = rects(&app);
        assert!(
            settled
                .iter()
                .all(|rect| rect.is_some_and(|r| r.height() > 100.0))
        );
        app.layout_dirty_since = None;
        app.windows.dirty = false;

        // A restored minimized-size window: the toolbar and switchbar leave
        // the chat area no height.
        for size in [egui::vec2(160.0, 30.0), egui::vec2(1024.0, 60.0)] {
            for _ in 0..3 {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        ..Default::default()
                    },
                    |ui| app.ui(ui),
                );
                output.textures_delta.clear();
            }
        }
        assert_eq!(rects(&app), settled);
        assert!(!app.windows.dirty);
        assert!(
            app.layout_dirty_since.is_none(),
            "layout.json would be saved"
        );

        // Back to a normal frame: still the same layout.
        frame(&mut app, &ctx);
        frame(&mut app, &ctx);
        assert_eq!(rects(&app), settled);
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
            cells.insert(window.key, window.child_rect.unwrap());
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
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        app.activate(rust);
        let ctx = egui::Context::default();
        for _ in 0..3 {
            frame(&mut app, &ctx);
        }
        feed(&mut app, 0, ":carol!u@h PRIVMSG me :psst");
        for _ in 0..3 {
            frame(&mut app, &ctx);
        }
        let carol = target_key(&app, app.sessions[0].id, "carol");
        let rect = app.windows.get(&carol).unwrap().child_rect.unwrap();
        assert!(
            rect.width() < 1024.0 * 0.7 && rect.height() < 768.0 * 0.7,
            "{rect:?}"
        );
        assert_eq!(app.windows.active, Some(rust));
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
        app.activate(status);
        for _ in 0..3 {
            frame(&mut app, &ctx);
        }
        let carol = target_key(&app, app.sessions[0].id, "carol");
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
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        let debian = target_key(&app, app.sessions[1].id, "#debian");
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
        let debian = target_key(&app, app.sessions[1].id, "#debian");
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
        assert_eq!(app.detached_dialog_owner(debian.conn), Some(debian));
        // Reattached: the dialogs go back to the main window.
        app.toggle_detached(&debian);
        assert_eq!(app.detached_dialog_owner(debian.conn), None);
    }

    #[test]
    fn a_detached_window_sends_what_is_typed_while_the_main_window_is_minimized() {
        let (mut app, _rx_a, mut rx_b) = two_networks();
        let debian = target_key(&app, app.sessions[1].id, "#debian");
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
        let key = app.windows.active.unwrap();
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
        let key = app.windows.active.unwrap();
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
        let key = app.windows.active.unwrap();
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
        let key = app.windows.active.unwrap();
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
        let key = app.windows.active.unwrap();
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
        let key = app.windows.active.unwrap();
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
        let key = app.windows.active.unwrap();
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
        let key = app.windows.active.unwrap();
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
        let key = app.windows.active.unwrap();
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
        let key = app.windows.active.unwrap();
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
        let key = app.windows.active.unwrap();
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
        let debian = target_key(&app, app.sessions[1].id, "#debian");
        app.toggle_detached(&debian);
        app.activate(debian);
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
        assert!(shown.iter().any(|t| t == "Behavior"), "{shown:?}");
    }

    #[test]
    fn closing_a_detached_status_window_asks_in_it() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let status = WindowKey::new(app.sessions[1].id, None);
        app.toggle_detached(&status);
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        app.request_close(status);
        frame(&mut app, &ctx);
        assert_eq!(app.confirm_disconnect, Some(status.conn));
        assert!(app.windows.get(&status).unwrap().detached);
        assert_eq!(app.global_dialog_window(), Some(status));
    }

    #[test]
    fn the_caption_close_button_parts_a_channel_and_tile_still_fills_the_area() {
        let (mut app, mut rx_a, _rx_b) = two_networks();
        let ctx = egui::Context::default();
        let mut time = floating_windows(&mut app, &ctx);
        app.tile_request = Some(TileRequest::Tile);
        for _ in 0..3 {
            pointer_frame(&mut app, &ctx, time, vec![]);
            time += 0.05;
        }
        // Tiled: every attached window in its own cell inside the area.
        let area = app.windows.area.unwrap();
        let rects: Vec<egui::Rect> = app
            .windows
            .windows
            .iter()
            .map(|w| w.child_rect.unwrap())
            .collect();
        for (i, a) in rects.iter().enumerate() {
            assert!(area.expand(1.0).contains_rect(*a), "{a:?} outside {area:?}");
            for b in &rects[i + 1..] {
                assert!(
                    !a.shrink(1.0).intersects(b.shrink(1.0)),
                    "{a:?} overlaps {b:?}"
                );
            }
        }

        drain(&mut rx_a);
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        let close = ctx
            .read_response(snap::close_button_id(&rust))
            .unwrap()
            .rect
            .center();
        for events in [
            vec![egui::Event::PointerMoved(close)],
            vec![primary(close, true)],
            vec![primary(close, false)],
            vec![],
        ] {
            pointer_frame(&mut app, &ctx, time, events);
            time += 0.05;
        }
        assert_eq!(drain(&mut rx_a), ["PART #rust"]);
    }

    #[test]
    fn double_clicking_a_caption_maximizes_its_window() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let ctx = egui::Context::default();
        let mut time = floating_windows(&mut app, &ctx);
        let other = app.ordered_windows()[0];
        app.activate(other);
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
        let debian = target_key(&app, app.sessions[1].id, "#debian");
        app.toggle_detached(&debian);
        app.request_close(debian);
        assert_eq!(drain(&mut rx_b), ["PART #debian"]);
    }

    #[test]
    fn unread_does_not_increase_for_a_target_visible_in_a_focused_detached_window() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let debian = target_key(&app, app.sessions[1].id, "#debian");
        app.toggle_detached(&debian);
        app.activate(debian);
        let ctx = egui::Context::default();
        frame(&mut app, &ctx); // Establish an actual at-bottom viewport report.

        let (tx, rx) = mpsc::channel(8);
        app.sessions[1].test_set_incoming(rx);
        // Focus is in the detached window.
        app.focused_detached = Some(debian);
        app.focused = true;
        tx.try_send(IrcMessage::parse(":bob!u@h PRIVMSG #debian :seen").unwrap())
            .unwrap();
        app.tick(&ctx, true);
        assert_eq!(app.sessions[1].conversations["#debian"].unread, 0);

        // Nothing of ours is focused: the same message is unread.
        app.focused_detached = None;
        app.focused = false;
        tx.try_send(IrcMessage::parse(":bob!u@h PRIVMSG #debian :unseen").unwrap())
            .unwrap();
        app.tick(&ctx, true);
        assert_eq!(app.sessions[1].conversations["#debian"].unread, 1);
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
        while !app.sessions.iter().all(|s| s.is_connected()) {
            assert!(Instant::now() < deadline, "sessions never registered");
            app.background_tick(&ctx);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.sessions.iter().all(|s| s.has_worker()));
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
        assert!(app.sessions[0].is_connected());
        let b_lines = lines_b.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(b_lines.iter().any(|l| l.starts_with("QUIT")), "{b_lines:?}");

        app.sessions[0].request_manual_disconnect(Some("bye".into()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.sessions[0].has_worker() {
            assert!(Instant::now() < deadline);
            app.background_tick(&ctx);
            std::thread::sleep(Duration::from_millis(5));
        }
        let a_lines = lines_a.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(a_lines.iter().any(|l| l == "QUIT :bye"), "{a_lines:?}");
    }

    #[test]
    fn startup_and_connect_use_the_single_settings_connection_editor() {
        let mut app = test_app();
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        let rendered = texts(&frame(&mut app, &ctx));
        assert!(app.show_settings);
        assert_eq!(app.settings_tab, 0);
        assert_eq!(app.settings_session, Some(app.sessions[0].id));
        assert!(
            rendered.iter().any(|text| text == "Settings"),
            "{rendered:?}"
        );
        assert!(!rendered.iter().any(|text| text == "Connect to Server"));
        assert!(
            rendered.iter().any(|text| text == "Save & Close"),
            "{rendered:?}"
        );

        app.settings_tab = 3;
        app.open_connection_settings(app.sessions[0].id);
        assert_eq!(app.settings_tab, 0);
        app.new_connection();
        assert_eq!(app.settings_index(), 1);
        assert_eq!(app.settings_tab, 0);
    }

    #[test]
    fn canceling_another_new_connection_keeps_a_saved_idle_connection() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        app.new_connection();
        let saved = app.sessions.last().unwrap().id;
        assert!(app.close_settings_window());
        assert!(app.session_index(saved).is_some());
        app.new_connection();
        app.cancel_settings_window();
        assert_eq!(app.sessions.len(), 3);
        assert!(app.session_index(saved).is_some());
        assert!(!app.show_settings);
    }

    #[test]
    fn activating_the_status_window_clears_its_unread_badge() {
        let mut app = test_app();
        assert!(app.sessions[0].open_query("alice"));
        app.sync_windows();
        let status = WindowKey::new(app.sessions[0].id, None);
        let alice = target_key(&app, app.sessions[0].id, "alice");
        app.user_activate(alice);
        app.sessions[0].set_window_focused(true);
        app.sessions[0].add_server_message(ChatMessage::system("notice"));
        assert_eq!(app.sessions[0].server_unread, 1);

        app.user_activate(status);

        assert_eq!(app.sessions[0].server_unread, 0);
        // It stays clear while the status window is the one being read.
        app.sessions[0].add_server_message(ChatMessage::system("another"));
        assert_eq!(app.sessions[0].server_unread, 0);

        // An unfocused session keeps its badge until focus returns.
        app.user_activate(alice);
        app.sessions[0].set_window_focused(false);
        app.sessions[0].add_server_message(ChatMessage::system("later"));
        app.user_activate(status);
        assert_eq!(app.sessions[0].server_unread, 1);
    }

    /// Where each painted text landed after a few frames at this width.
    fn text_rects_at_width(app: &mut App, width: f32) -> Vec<(String, egui::Rect)> {
        fn collect(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(text) => out.push((
                    text.galley.text().to_string(),
                    egui::Rect::from_min_size(text.pos, text.galley.size()),
                )),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect(s, out)),
                _ => {}
            }
        }
        let ctx = egui::Context::default();
        let mut out = Vec::new();
        for _ in 0..3 {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 768.0),
                    )),
                    ..Default::default()
                },
                |ui| app.ui(ui),
            );
            output.textures_delta.clear();
            out.clear();
            for clipped in &output.shapes {
                collect(&clipped.shape, &mut out);
            }
        }
        out
    }

    fn rect_of(rects: &[(String, egui::Rect)], text: &str) -> egui::Rect {
        rects
            .iter()
            .find(|(painted, _)| painted == text)
            .unwrap_or_else(|| panic!("{text:?} was not painted"))
            .1
    }

    const TOOLBAR_CONTROLS: [&str; 6] = [
        "Join",
        "Disconnect",
        "Search",
        "Activity",
        "Window",
        "Settings",
    ];

    #[test]
    fn the_toolbar_arrangement_needs_slack_to_grow_but_none_to_shrink() {
        let fit = |rows| {
            Some(ToolbarFit {
                rows,
                heading: 100.0,
                status: 400.0,
                controls: 600.0,
            })
        };
        // One row needs 100 + 400 + 600 + 4 gaps of 8 = 1132.
        assert_eq!(toolbar_rows(None, 300.0, 8.0), ToolbarRows::One);
        assert_eq!(
            toolbar_rows(fit(ToolbarRows::One), 1132.0, 8.0),
            ToolbarRows::One
        );
        assert_eq!(
            toolbar_rows(fit(ToolbarRows::One), 1131.0, 8.0),
            ToolbarRows::Two
        );
        assert_eq!(
            toolbar_rows(fit(ToolbarRows::Two), 1140.0, 8.0),
            ToolbarRows::Two
        );
        assert_eq!(
            toolbar_rows(fit(ToolbarRows::Two), 1156.0, 8.0),
            ToolbarRows::One
        );
        // Name and controls need 100 + 600 + 8 = 708.
        assert_eq!(
            toolbar_rows(fit(ToolbarRows::Two), 708.0, 8.0),
            ToolbarRows::Two
        );
        assert_eq!(
            toolbar_rows(fit(ToolbarRows::One), 707.0, 8.0),
            ToolbarRows::Wrapped
        );
        assert_eq!(
            toolbar_rows(fit(ToolbarRows::Wrapped), 720.0, 8.0),
            ToolbarRows::Wrapped
        );
        assert_eq!(
            toolbar_rows(fit(ToolbarRows::Wrapped), 732.0, 8.0),
            ToolbarRows::Two
        );
    }

    #[test]
    fn a_wide_toolbar_is_one_row_with_the_controls_at_the_right() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let rects = text_rects_at_width(&mut app, 1600.0);
        let name = rect_of(&rects, "Linefeed");
        let status = rect_of(&rects, "Connected · not authenticated");
        assert!(name.left() < 20.0, "{name:?}");
        let mut left = status.right();
        for control in TOOLBAR_CONTROLS {
            let rect = rect_of(&rects, control);
            assert!(
                (rect.center().y - status.center().y).abs() < 6.0,
                "{control} shares the status row: {rect:?} vs {status:?}"
            );
            assert!(rect.left() > left, "{control} follows in order: {rect:?}");
            left = rect.right();
        }
        assert!(
            left > 1600.0 - 24.0,
            "Settings ends at the right edge: {left}"
        );
        assert!(
            rect_of(&rects, "Join").left() > 800.0,
            "the controls sit in the right half, not beside the name"
        );
    }

    #[test]
    fn a_default_width_toolbar_keeps_the_controls_right_and_moves_status_below() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let rects = text_rects_at_width(&mut app, 1024.0);
        let name = rect_of(&rects, "Linefeed");
        let settings = rect_of(&rects, "Settings");
        let status = rect_of(&rects, "Connected · not authenticated");
        assert!(settings.right() > 1024.0 - 24.0, "{settings:?}");
        assert!(
            (settings.center().y - name.center().y).abs() < 8.0,
            "the controls share the name's row: {settings:?} vs {name:?}"
        );
        assert!(
            status.top() > settings.bottom() - 2.0,
            "the status line is the second row: {status:?} vs {settings:?}"
        );
        for control in TOOLBAR_CONTROLS {
            let rect = rect_of(&rects, control);
            assert!(
                rect.left() > name.right() && rect.right() <= 1024.0,
                "{control}: {rect:?}"
            );
        }
    }

    #[test]
    fn a_narrow_toolbar_wraps_inside_the_window() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let rects = text_rects_at_width(&mut app, 640.0);
        for text in TOOLBAR_CONTROLS
            .into_iter()
            .chain(["Linefeed", "Connected · not authenticated"])
        {
            let rect = rect_of(&rects, text);
            assert!(
                rect.left() >= 0.0 && rect.right() <= 640.0,
                "{text} stays inside a 640 px window: {rect:?}"
            );
        }
        // Widening again restores the single row.
        let rects = text_rects_at_width(&mut app, 1600.0);
        let status = rect_of(&rects, "Connected · not authenticated");
        let settings = rect_of(&rects, "Settings");
        assert!((settings.center().y - status.center().y).abs() < 6.0);
    }

    fn focus_frame(app: &mut App, ctx: &egui::Context, focused: bool) -> egui::FullOutput {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1024.0, 768.0),
                )),
                focused,
                ..Default::default()
            },
            |ui| app.ui(ui),
        );
        output.textures_delta.clear();
        output
    }

    /// Draw until a viewport report has been taken as read, and say whether
    /// the last frame painted the unread marker.
    fn unread_marker_after_frames(app: &mut App, ctx: &egui::Context, focused: bool) -> bool {
        let mut output = focus_frame(app, ctx, focused);
        for _ in 0..3 {
            output = focus_frame(app, ctx, focused);
        }
        texts(&output).iter().any(|text| text == "Unread messages")
    }

    #[test]
    fn messages_read_as_they_arrive_get_no_unread_marker() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        app.user_activate(rust);
        let ctx = egui::Context::default();
        assert!(!unread_marker_after_frames(&mut app, &ctx, true));

        for text in ["one", "two"] {
            feed(&mut app, 0, &format!(":bob!u@h PRIVMSG #rust :{text}"));
            assert!(
                !unread_marker_after_frames(&mut app, &ctx, true),
                "{text} was read as it arrived"
            );
        }
        assert_eq!(app.sessions[0].conversations["#rust"].unread, 0);
    }

    #[test]
    fn returning_to_a_read_window_shows_no_unread_marker() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        let status = WindowKey::new(app.sessions[0].id, None);
        app.user_activate(rust);
        let ctx = egui::Context::default();
        feed(&mut app, 0, ":bob!u@h PRIVMSG #rust :one");
        unread_marker_after_frames(&mut app, &ctx, true);

        app.user_activate(status);
        unread_marker_after_frames(&mut app, &ctx, true);
        app.user_activate(rust);
        assert!(
            !unread_marker_after_frames(&mut app, &ctx, true),
            "nothing arrived while the window was in the background"
        );

        // What arrives now is read on arrival; the old position is no marker.
        feed(&mut app, 0, ":bob!u@h PRIVMSG #rust :two");
        assert!(!unread_marker_after_frames(&mut app, &ctx, true));
    }

    #[test]
    fn the_unread_marker_shows_above_unread_messages_until_the_conversation_moves_on() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        let status = WindowKey::new(app.sessions[0].id, None);
        app.user_activate(rust);
        let ctx = egui::Context::default();
        feed(&mut app, 0, ":bob!u@h PRIVMSG #rust :one");
        unread_marker_after_frames(&mut app, &ctx, true);

        app.user_activate(status);
        unread_marker_after_frames(&mut app, &ctx, true);
        feed(&mut app, 0, ":bob!u@h PRIVMSG #rust :two");
        feed(&mut app, 0, ":bob!u@h PRIVMSG #rust :three");
        assert_eq!(app.sessions[0].conversations["#rust"].unread, 2);

        app.user_activate(rust);
        assert!(
            unread_marker_after_frames(&mut app, &ctx, true),
            "the marker stays above what was unread on entry, even once read"
        );
        assert_eq!(app.sessions[0].conversations["#rust"].unread, 0);

        feed(&mut app, 0, ":bob!u@h PRIVMSG #rust :four");
        assert!(
            !unread_marker_after_frames(&mut app, &ctx, true),
            "a message read on arrival retires the marker above older ones"
        );
    }

    #[test]
    fn messages_arriving_while_the_app_is_unfocused_get_the_unread_marker() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let rust = target_key(&app, app.sessions[0].id, "#rust");
        app.user_activate(rust);
        let ctx = egui::Context::default();
        unread_marker_after_frames(&mut app, &ctx, true);
        feed(&mut app, 0, ":bob!u@h PRIVMSG #rust :one");
        assert!(!unread_marker_after_frames(&mut app, &ctx, true));

        unread_marker_after_frames(&mut app, &ctx, false);
        feed(&mut app, 0, ":bob!u@h PRIVMSG #rust :two");
        assert!(unread_marker_after_frames(&mut app, &ctx, false));
        assert_eq!(app.sessions[0].conversations["#rust"].unread, 1);

        // Focus returns: the message is read and stays marked as the new one.
        assert!(unread_marker_after_frames(&mut app, &ctx, true));
        assert_eq!(app.sessions[0].conversations["#rust"].unread, 0);

        // A second absence marks its own messages, not the earlier ones.
        unread_marker_after_frames(&mut app, &ctx, false);
        feed(&mut app, 0, ":bob!u@h PRIVMSG #rust :three");
        unread_marker_after_frames(&mut app, &ctx, false);
        let boundary = app
            .windows
            .get(&rust)
            .and_then(|window| window.view.timeline.borrow().entry_read_boundary.clone())
            .expect("the new unread message is marked");
        let two = app.sessions[0].conversations["#rust"]
            .messages
            .iter()
            .find(|message| message.content == "two")
            .map(|message| message.message_key.clone());
        assert_eq!(
            boundary.archive_id, two,
            "the marker sits under the last read message"
        );
    }

    #[test]
    fn next_unread_includes_a_status_window_with_unread_lines() {
        let mut app = test_app();
        assert!(app.sessions[0].open_query("alice"));
        app.sync_windows();
        let status = WindowKey::new(app.sessions[0].id, None);
        let alice = target_key(&app, app.sessions[0].id, "alice");
        app.user_activate(alice);
        app.sessions[0].set_window_focused(true);
        app.sessions[0].add_server_message(ChatMessage::system("notice"));
        assert_eq!(app.sessions[0].server_unread, 1);

        app.cycle_unread();

        assert_eq!(app.windows.active, Some(status));
    }

    #[test]
    fn retargeting_the_editor_cancels_the_first_sessions_edit() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let (first, second) = (app.sessions[0].id, app.sessions[1].id);
        let host = app.sessions[0].server_host.clone();
        let password = app.sessions[0].password.clone();
        app.open_settings(first);
        app.sessions[0].server_host = "scratch.example".into();
        app.sessions[0].password.clear();

        // Connect on the other network while the editor is still open.
        app.open_connection_settings(second);
        assert_eq!(app.settings_session, Some(second));
        app.cancel_settings_window();

        assert_eq!(app.sessions[0].server_host, host);
        assert_eq!(app.sessions[0].password, password);
        // Reopening the first editor starts from the restored form.
        app.open_settings(first);
        app.cancel_settings_window();
        assert_eq!(app.sessions[0].server_host, host);
    }

    #[test]
    fn stacking_new_connections_discards_the_first_pristine_one_like_cancel() {
        let mut app = test_app();
        let original = app.sessions[0].id;
        app.sync_windows();
        app.new_connection();
        let first = app.sessions.last().unwrap().id;
        app.sessions.last_mut().unwrap().server_host = "typed.example".into();

        app.new_connection();
        let second = app.sessions.last().unwrap().id;
        // The first canceled pristine connection is gone, not left idle.
        assert!(app.session_index(first).is_none());
        assert_eq!(app.settings_session, Some(second));
        assert_eq!(app.sessions.len(), 2);

        app.cancel_settings_window();

        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.sessions[0].id, original);
        assert_eq!(app.windows.active.map(|key| key.conn), Some(original));
        assert!(app.settings_form_before.is_none());
        assert!(!app.show_settings);
    }

    #[test]
    fn retargeting_from_a_new_connection_discards_it_and_keeps_the_active_window() {
        let (mut app, _rx_a, _rx_b) = two_networks();
        let (first, second) = (app.sessions[0].id, app.sessions[1].id);
        app.sync_windows();
        app.user_activate(WindowKey::new(second, None));
        app.new_connection();
        let fresh = app.sessions.last().unwrap().id;
        app.sessions.last_mut().unwrap().server_host = "typed.example".into();

        // Toolbar Connect on an existing session while the editor is open.
        app.user_activate(WindowKey::new(first, None));
        app.open_connection_settings(first);

        assert!(app.session_index(fresh).is_none(), "stray window dropped");
        assert_eq!(app.settings_session, Some(first));
        assert!(app.show_settings);
        assert_eq!(app.windows.active.map(|key| key.conn), Some(first));
    }

    #[test]
    fn an_existing_editor_stays_pinned_when_another_idle_window_is_selected() {
        let mut app = test_app();
        let first = app.sessions[0].id;
        app.open_settings(first);
        let second = app.add_session();
        app.sync_windows();
        app.user_activate(WindowKey::new(app.sessions[second].id, None));
        app.route_connection_editor();
        assert_eq!(app.settings_session, Some(first));
    }
}

#[cfg(test)]
mod automation_shortcut_tests {
    use super::super::actions::{ActionContext, ActionId, KeyChord, ShortcutBinding};
    use super::*;
    fn app() -> (App, WindowKey, WindowKey) {
        let mut app = App::with_settings(
            Settings {
                logging_enabled: false,
                ..Settings::default()
            },
            LayoutFile::default(),
        );
        let index = app.add_session();
        app.sessions[index].show_connect_dialog = false;
        let first = app.sessions[index].ensure_history_target("#alpha");
        let second = app.sessions[index].ensure_history_target("#beta");
        let first = WindowKey::new(app.sessions[index].id, Some(first));
        let second = WindowKey::new(app.sessions[index].id, Some(second));
        app.sync_windows();
        app.user_activate(first);
        (app, first, second)
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
    fn remapping_replaces_builtin_shortcut_and_consumes_only_new_chord() {
        let (mut app, _, _) = app();
        app.automation.editing.shortcuts = vec![ShortcutBinding {
            chord: KeyChord::new("Q", true, false, false),
            action: ActionId::CloseWindow,
        }];
        app.automation.commit().unwrap();
        let context = egui::Context::default();
        let mut output = context.run_ui(
            egui::RawInput {
                events: vec![
                    key(egui::Key::W, egui::Modifiers::CTRL),
                    key(egui::Key::Q, egui::Modifiers::CTRL),
                ],
                ..Default::default()
            },
            |ui| {
                assert_eq!(app.take_shortcuts(ui.ctx()), [WindowShortcut::Close]);
                assert!(ui.ctx().input(|input| input.key_pressed(egui::Key::W)));
                assert!(!ui.ctx().input(|input| input.key_pressed(egui::Key::Q)));
            },
        );
        output.textures_delta.clear();
    }
    #[test]
    fn switcher_query_and_enter_same_frame_activate_matching_stable_id() {
        let (mut app, first, second) = app();
        app.windows.get_mut(&first).unwrap().view.input_text = "draft stays".into();
        let context = egui::Context::default();
        app.dispatch_action(
            &context,
            ActionId::WindowSwitcher,
            ActionContext { window: first },
        );
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            app.show_window_switcher(ui.ctx())
        });
        output.textures_delta.clear();
        let mut output = context.run_ui(
            egui::RawInput {
                events: vec![
                    egui::Event::Text("beta".into()),
                    key(egui::Key::Enter, egui::Modifiers::NONE),
                ],
                ..Default::default()
            },
            |ui| app.show_window_switcher(ui.ctx()),
        );
        output.textures_delta.clear();
        assert_eq!(app.windows.active, Some(second));
        assert!(!app.window_switcher.open);
        assert_eq!(
            app.windows.get(&first).unwrap().view.input_text,
            "draft stays"
        );
    }
    #[test]
    fn muted_conversation_cancels_its_already_queued_notification() {
        let mut app = App::with_settings(
            Settings {
                logging_enabled: false,
                logging_load_history: false,
                ..Settings::default()
            },
            LayoutFile::default(),
        );
        let event = super::super::attention::AttentionEvent {
            reference: super::super::identity::MessageRef {
                conversation: super::super::identity::ConversationKey {
                    network: super::super::identity::LogicalNetworkKey::Direct {
                        endpoint: super::super::identity::TransportEndpointKey::new(
                            "irc.example",
                            6697,
                            true,
                        ),
                    },
                    target: "alice".into(),
                },
                archive_id: Some("row".into()),
                server_msgid: None,
                server_time: None,
            },
            private: true,
            mention: false,
        };
        app.handle_attention(event.clone(), "PM from alice", "one", true);
        app.handle_attention(event.clone(), "PM from alice", "two", true);
        assert_eq!(app.dispatched_notifications.len(), 1);
        app.settings
            .attention
            .rule_mut(event.reference.conversation)
            .level = super::super::attention::AlertLevel::Muted;
        app.last_forced_notification_time = Some(Instant::now() - MIN_NOTIFICATION_INTERVAL);
        app.flush_deferred_notification();
        assert_eq!(app.dispatched_notifications.len(), 1);
        assert!(app.deferred_notification.is_none());
    }
}

#[cfg(test)]
mod message_navigation_tests {
    use super::*;
    #[test]
    fn archived_reply_preserves_draft_and_rejects_closed_or_wrong_network_destinations() {
        let mut app = App::with_settings(
            Settings {
                nickname: "me".into(),
                logging_enabled: false,
                logging_load_history: false,
                ..Settings::default()
            },
            LayoutFile::default(),
        );
        let index = app.add_session();
        app.sessions[index].show_connect_dialog = false;
        assert!(app.sessions[index].start_session_from_form());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.sessions[index].test_set_registered(tx);
        app.sessions[index]
            .enabled_caps
            .insert("message-tags".into());
        let id = app.sessions[index].ensure_history_target("#room");
        app.sessions[index]
            .conversations
            .channel_mut(id)
            .unwrap()
            .joined = true;
        app.sync_windows();
        let key = WindowKey::new(app.sessions[index].id, Some(id));
        app.windows.get_mut(&key).unwrap().view.input_text = "keep my draft".into();
        let reference = super::super::identity::MessageRef {
            conversation: app.sessions[index].conversation_key("#room"),
            archive_id: Some("archived".into()),
            server_msgid: Some("parent".into()),
            server_time: None,
        };
        assert!(app.begin_reply(reference.clone()).is_ok());
        let view = &app.windows.get(&key).unwrap().view;
        assert_eq!(view.input_text, "keep my draft");
        assert_eq!(view.composer.reply_to, Some(reference.clone()));
        let mut closed = reference.clone();
        closed.conversation.target = "#closed".into();
        assert!(app.begin_reply(closed).is_err());
        let mut wrong = reference;
        wrong.conversation.network = super::super::identity::LogicalNetworkKey::Direct {
            endpoint: super::super::identity::TransportEndpointKey::new(
                "other.example",
                6697,
                true,
            ),
        };
        assert!(app.begin_reply(wrong).is_err());
        assert_eq!(
            app.windows.get(&key).unwrap().view.input_text,
            "keep my draft"
        );
    }
}

#[cfg(test)]
mod automation_privacy_tests {
    use super::super::automation::{AliasDef, AutomationConfig};
    use super::*;
    fn fixture() -> (App, usize, ConversationId, ConversationId) {
        let mut app = App::with_settings(
            Settings {
                server_host: "irc.example".into(),
                nickname: "me".into(),
                logging_enabled: false,
                logging_load_history: false,
                ..Settings::default()
            },
            LayoutFile::default(),
        );
        let index = app.add_session();
        let session = &mut app.sessions[index];
        let config = session.session_from_form().unwrap();
        session.activate_session(config);
        session.show_connect_dialog = false;
        let room = session.ensure_history_target("#room");
        let service = session.ensure_history_target("NickServ");
        session.select_conversation(Some(room));
        let mut config = AutomationConfig::default();
        config.aliases = vec![
            AliasDef {
                id: "target".into(),
                name: "relay".into(),
                scope: None,
                steps: vec!["/msg $target $*".into()],
            },
            AliasDef {
                id: "required".into(),
                name: "needed".into(),
                scope: None,
                steps: vec!["/msg NickServ IDENTIFY $1".into()],
            },
        ];
        session.set_automation(std::sync::Arc::new(config));
        app.sync_windows();
        (app, index, room, service)
    }
    #[test]
    fn workspace_and_updater_filter_drafts_in_each_owning_target_and_preserve_benign_drafts() {
        for secret in [
            "IDENTIFY scratch-secret",
            "/relay IDENTIFY scratch-secret",
            "/needed",
        ] {
            let (mut app, index, room, service) = fixture();
            let conn = app.sessions[index].id;
            app.windows
                .get_mut(&WindowKey::new(conn, Some(room)))
                .unwrap()
                .view
                .input_text = "benign draft".into();
            app.windows
                .get_mut(&WindowKey::new(conn, Some(service)))
                .unwrap()
                .view
                .input_text = secret.into();
            let workspace = serde_json::to_string(&app.capture_workspace()).unwrap();
            assert!(!workspace.contains(secret), "workspace retained {secret}");
            assert!(workspace.contains("benign draft"));
            let update = serde_json::to_string(&app.update_session()).unwrap();
            assert!(!update.contains(secret), "updater retained {secret}");
            assert!(update.contains("benign draft"));
            assert_eq!(app.sessions[index].current_conversation, Some(room));
        }
    }
    #[test]
    fn pending_draft_privacy_uses_original_owner_and_drops_retired_owner() {
        let (mut app, index, room, service) = fixture();
        let conn = app.sessions[index].id;
        app.pending_workspace_views.push((
            conn,
            Some(service),
            super::super::workspace::WorkspaceViewState {
                draft: "/relay IDENTIFY queued-secret".into(),
                ..Default::default()
            },
        ));
        app.pending_inputs
            .push((conn, Some(service), "/needed".into()));
        let retired = app.sessions[index].ensure_history_target("#retired");
        app.sessions[index].remove_channel("#retired");
        app.pending_inputs
            .push((conn, Some(retired), "retired-owner-secret".into()));
        let workspace = serde_json::to_string(&app.capture_workspace()).unwrap();
        assert!(!workspace.contains("queued-secret"));
        assert!(!workspace.contains("/needed"));
        assert!(!workspace.contains("retired-owner-secret"));
        let update = serde_json::to_string(&app.update_session()).unwrap();
        assert!(!update.contains("queued-secret"));
        assert!(!update.contains("/needed"));
        assert!(!update.contains("retired-owner-secret"));
        assert_eq!(app.sessions[index].current_conversation, Some(room));
    }
    #[test]
    fn session_snapshot_and_command_history_keep_global_bare_credential_filter() {
        let (mut app, index, _, _) = fixture();
        app.sessions[index].input_text = "identify scratch-secret".into();
        assert!(app.sessions[index].update_session().input().is_empty());
        assert!(app.sessions[index].draft_is_sensitive("ID scratch-secret"));
    }
}
