//! mIRC-style window model.
//!
//! Every session contributes a status window plus one window per channel and
//! query. Windows live inside the main OS window either maximized (only the
//! active one is drawn, filling the chat area) or floating (movable egui
//! windows that can be tiled and cascaded). Any window can be detached into
//! its own OS window (an egui viewport) and re-attached.
//!
//! The set of windows is derived from the sessions every frame (`sync`); this
//! module only keeps what the sessions do not know: placement, the per-window
//! input line, and which window is active. Placement survives restarts in
//! `layout.json`, keyed by network identity and canonical target name rather
//! than by connection id, so a reconnected network finds its windows again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use egui::{Pos2, Rect, Vec2};
use serde::{Deserialize, Serialize};

use super::TabCompletion;

/// Identifies a running session for the lifetime of the process. Never reused.
pub(crate) type ConnId = u32;

/// Network identity of a session that never connected: it means nothing
/// after a restart, so its windows are never written to layout.json.
pub(crate) const UNCONNECTED_PREFIX: &str = "unconnected:";

/// One window: a session's status window (`target == None`) or one of its
/// channel/query tabs, named by the session's tab key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct WindowKey {
    pub conn: ConnId,
    pub target: Option<String>,
}

impl WindowKey {
    pub(crate) fn new(conn: ConnId, target: Option<String>) -> Self {
        Self { conn, target }
    }
}

/// Stable identity of a window across restarts: the network (profile id or
/// endpoint identity) plus the casemapped target, `None` for the status window.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct PersistKey {
    pub network: String,
    pub target: Option<String>,
}

/// A window that should exist this frame, as reported by the sessions.
#[derive(Clone, PartialEq)]
pub(crate) struct PresentWindow {
    pub key: WindowKey,
    pub persist: PersistKey,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum LayoutMode {
    /// Only the active window is drawn, filling the chat area.
    #[default]
    Maximized,
    /// Every attached window is a movable child window.
    Floating,
}

/// The editing state of one window's input line. It is swapped into the
/// owning session while that window draws or handles input, so the session's
/// input, history and completion code serves every window unchanged.
#[derive(Default)]
pub(crate) struct TargetView {
    pub input_text: String,
    /// This window's own sent-line history: lines sent here are recallable
    /// here (Up/Down), never in another channel or on another network.
    pub command_history: Vec<String>,
    pub history_index: Option<usize>,
    pub history_temp: String,
    pub tab_completion: Option<TabCompletion>,
    pub selected_user: Option<String>,
    /// Focus this window's input line on its next draw.
    pub input_focus_request: bool,
    /// Whether this window's input line had focus when last drawn.
    pub input_had_focus: bool,
}

pub(crate) struct TargetWindow {
    pub key: WindowKey,
    pub persist: PersistKey,
    /// Shown in its own OS window instead of inside the main window.
    pub detached: bool,
    /// Last known outer rect as a floating child window.
    pub child_rect: Option<Rect>,
    /// A one-shot placement (tile/cascade) applied on the next frame.
    pub pending_place: Option<Rect>,
    /// Last known OS-window geometry while detached (logical points).
    pub os_pos: Option<Pos2>,
    pub os_size: Option<Vec2>,
    pub view: TargetView,
}

impl TargetWindow {
    fn new(key: WindowKey, persist: PersistKey, saved: Option<&SavedWindow>) -> Self {
        let mut window = Self {
            key,
            persist,
            detached: false,
            child_rect: None,
            pending_place: None,
            os_pos: None,
            os_size: None,
            view: TargetView::default(),
        };
        if let Some(saved) = saved {
            window.detached = saved.detached;
            window.child_rect = saved.child_rect.map(rect_from_array);
            window.os_pos = saved.os_pos.map(|[x, y]| Pos2::new(x, y));
            window.os_size = saved.os_size.map(|[w, h]| Vec2::new(w, h));
        }
        window
    }

    fn saved(&self) -> SavedWindow {
        SavedWindow {
            key: self.persist.clone(),
            child_rect: self.child_rect.map(rect_to_array),
            detached: self.detached,
            os_pos: self.os_pos.map(|p| [p.x, p.y]),
            os_size: self.os_size.map(|s| [s.x, s.y]),
        }
    }
}

#[derive(Default)]
pub(crate) struct WindowManager {
    /// Windows in creation order.
    pub windows: Vec<TargetWindow>,
    pub active: Option<WindowKey>,
    pub mode: LayoutMode,
    /// Placement of windows that are not open right now (parted channels,
    /// networks not connected yet), restored when they reappear.
    saved: HashMap<PersistKey, SavedWindow>,
    /// The persisted active window. When it appears, `sync` reports it in
    /// `restore_match`; the App decides whether to still honour it.
    restore_active: Option<PersistKey>,
    /// The persisted active window appeared during the last `sync`.
    pub restore_match: Option<WindowKey>,
    /// Set when placement changed and layout.json should be rewritten.
    pub dirty: bool,
    /// The chat area floating windows were last laid out in. When it
    /// changes (the main window is resized, the switchbar wraps onto a
    /// second line) the windows are scaled into the new area.
    pub area: Option<Rect>,
    /// The window set last reconciled by `sync`: an unchanged set makes
    /// the next `sync` a no-op, so steady-state frames skip the reconcile.
    last_present: Vec<PresentWindow>,
}

impl WindowManager {
    pub(crate) fn from_layout(layout: LayoutFile) -> Self {
        let mut saved = HashMap::new();
        for window in layout.windows {
            saved.insert(window.key.clone(), window);
        }
        Self {
            windows: Vec::new(),
            active: None,
            mode: layout.mode,
            saved,
            restore_active: layout.active,
            restore_match: None,
            dirty: false,
            area: layout.area.map(rect_from_array),
            last_present: Vec::new(),
        }
    }

    pub(crate) fn get(&self, key: &WindowKey) -> Option<&TargetWindow> {
        self.windows.iter().find(|w| &w.key == key)
    }

    pub(crate) fn get_mut(&mut self, key: &WindowKey) -> Option<&mut TargetWindow> {
        self.windows.iter_mut().find(|w| &w.key == key)
    }

    /// Reconcile the window list with the windows the sessions currently
    /// have. New windows pick up any saved placement; vanished windows leave
    /// theirs behind for when they come back. Returns the keys that appeared.
    pub(crate) fn sync(&mut self, present: &[PresentWindow]) -> Vec<WindowKey> {
        // Borrowed input: the App rebuilds this list only when the window
        // set changes, so the steady-state frame must not pay to clone or
        // re-key it. Only windows that appear or whose persist key changed
        // are cloned below. An unchanged set (the common frame) reconciles
        // to nothing at all.
        if self.last_present.len() == present.len()
            && self
                .last_present
                .iter()
                .zip(present.iter())
                .all(|(old, new)| old == new)
        {
            return Vec::new();
        }
        let mut seen: HashMap<&WindowKey, &PersistKey> = HashMap::with_capacity(present.len());
        for window in present {
            seen.insert(&window.key, &window.persist);
        }

        let mut removed = Vec::new();
        self.windows.retain(|window| {
            if seen.contains_key(&window.key) {
                true
            } else {
                removed.push(window.saved());
                false
            }
        });
        for mut saved in removed {
            // A window that closes goes back inside the main window: a query
            // closed in its own OS window must not reopen as one on the
            // next message.
            saved.detached = false;
            self.saved.insert(saved.key.clone(), saved);
        }
        if self
            .active
            .as_ref()
            .is_some_and(|key| !seen.contains_key(key))
        {
            self.active = None;
        }

        // Keep each persist key current: a casemapping or rename can change
        // it while the window stays open.
        for window in &mut self.windows {
            if let Some(persist) = seen.remove(&window.key)
                && window.persist != *persist
            {
                window.persist = persist.clone();
            }
        }

        let mut appeared: Vec<(&WindowKey, &PersistKey)> = seen.into_iter().collect();
        // Deterministic order for windows that appear in the same frame.
        appeared.sort_by(|a, b| {
            (a.0.conn, a.0.target.is_some(), &a.0.target).cmp(&(
                b.0.conn,
                b.0.target.is_some(),
                &b.0.target,
            ))
        });
        let mut keys = Vec::with_capacity(appeared.len());
        for (key, persist) in appeared {
            let saved = self.saved.remove(persist);
            if self.restore_active.as_ref() == Some(persist) {
                self.restore_active = None;
                self.restore_match = Some(key.clone());
            }
            self.windows.push(TargetWindow::new(
                key.clone(),
                persist.clone(),
                saved.as_ref(),
            ));
            keys.push(key.clone());
        }
        self.last_present = present.to_vec();
        keys
    }

    /// Lay every attached window out in a grid inside `area`.
    pub(crate) fn tile(&mut self, area: Rect) {
        self.mode = LayoutMode::Floating;
        let rects = tile_rects(area, self.attached_count());
        self.apply_placements(rects);
    }

    /// Stack every attached window diagonally inside `area`, active last.
    pub(crate) fn cascade(&mut self, area: Rect) {
        self.mode = LayoutMode::Floating;
        let rects = cascade_rects(area, self.attached_count());
        self.apply_placements(rects);
    }

    /// Lay floating windows out for the chat area `area`: when it differs
    /// from the one they were placed in, scale every attached window's
    /// rect proportionally, so a tiled layout stays tiled and nothing is
    /// left off screen or overlapping after the area moves or resizes.
    pub(crate) fn fit_to_area(&mut self, area: Rect) {
        let Some(old) = self.area.replace(area) else {
            return;
        };
        let moved =
            (old.min - area.min).length() > 0.5 || (old.size() - area.size()).length() > 0.5;
        if !moved || old.width() <= 0.0 || old.height() <= 0.0 {
            return;
        }
        let scale = Vec2::new(area.width() / old.width(), area.height() / old.height());
        for window in self.windows.iter_mut().filter(|w| !w.detached) {
            let Some(rect) = window.child_rect else {
                continue;
            };
            let min = area.min + (rect.min - old.min) * scale;
            let fitted = Rect::from_min_size(min, rect.size() * scale);
            window.child_rect = Some(fitted);
            window.pending_place = Some(fitted);
        }
        self.dirty = true;
    }

    /// Put one attached window at `rect` (a snap layout zone).
    pub(crate) fn place(&mut self, key: &WindowKey, rect: Rect) {
        if let Some(window) = self.get_mut(key).filter(|w| !w.detached) {
            window.pending_place = Some(rect);
            window.child_rect = Some(rect);
            self.dirty = true;
        }
    }

    fn attached_count(&self) -> usize {
        self.windows.iter().filter(|w| !w.detached).count()
    }

    fn apply_placements(&mut self, rects: Vec<Rect>) {
        let active = self.active.clone();
        // The active window goes last so it ends up on top of a cascade.
        let mut order: Vec<usize> = (0..self.windows.len())
            .filter(|&i| !self.windows[i].detached)
            .collect();
        if let Some(active) = &active
            && let Some(pos) = order.iter().position(|&i| &self.windows[i].key == active)
        {
            let index = order.remove(pos);
            order.push(index);
        }
        for (index, rect) in order.into_iter().zip(rects) {
            let window = &mut self.windows[index];
            window.pending_place = Some(rect);
            window.child_rect = Some(rect);
        }
        self.dirty = true;
    }

    /// The layout to persist: open windows plus remembered closed ones.
    /// Stop waiting for the persisted active window: the user has chosen
    /// one, or startup is over.
    pub(crate) fn forget_restore_active(&mut self) {
        self.restore_active = None;
        self.restore_match = None;
    }

    pub(crate) fn layout(&self) -> LayoutFile {
        let lasting = |key: &PersistKey| !key.network.starts_with(UNCONNECTED_PREFIX);
        let mut windows: Vec<SavedWindow> = self
            .windows
            .iter()
            .filter(|w| lasting(&w.persist))
            .map(TargetWindow::saved)
            .collect();
        let open: std::collections::HashSet<&PersistKey> =
            self.windows.iter().map(|w| &w.persist).collect();
        let mut closed: Vec<&SavedWindow> = self
            .saved
            .values()
            .filter(|saved| !open.contains(&saved.key) && lasting(&saved.key))
            .collect();
        closed
            .sort_by(|a, b| (&a.key.network, &a.key.target).cmp(&(&b.key.network, &b.key.target)));
        windows.extend(
            closed
                .into_iter()
                .take(MAX_SAVED_WINDOWS.saturating_sub(windows.len()))
                .cloned(),
        );
        let active = self
            .active
            .as_ref()
            .and_then(|key| self.get(key))
            .map(|w| w.persist.clone())
            .or_else(|| self.restore_active.clone())
            .filter(lasting);
        LayoutFile {
            version: LAYOUT_VERSION,
            mode: self.mode,
            active,
            windows,
            area: self.area.map(rect_to_array),
            auto_connect: Vec::new(),
        }
    }
}

/// Upper bound on remembered window placements, so layout.json cannot grow
/// without limit across years of channels.
const MAX_SAVED_WINDOWS: usize = 500;
const LAYOUT_VERSION: u32 = 1;

/// Gap left between tiled windows and around the area, in points.
pub(crate) const TILE_GAP: f32 = 4.0;
/// Diagonal step between cascaded windows, in points.
const CASCADE_STEP: f32 = 26.0;

/// Grid rects for `n` windows inside `area`: as many columns as the square
/// root rounds up to, rows filled left to right, the last row's windows
/// widened to use the full width. Rects never overlap.
pub(crate) fn tile_rects(area: Rect, n: usize) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }
    let cols = (n as f64).sqrt().ceil() as usize;
    let rows = n.div_ceil(cols);
    let row_height = area.height() / rows as f32;
    let mut rects = Vec::with_capacity(n);
    for index in 0..n {
        let row = index / cols;
        let col = index % cols;
        let in_row = if row == rows - 1 {
            n - cols * (rows - 1)
        } else {
            cols
        };
        let width = area.width() / in_row as f32;
        let min = Pos2::new(
            area.min.x + col as f32 * width,
            area.min.y + row as f32 * row_height,
        );
        let cell = Rect::from_min_size(min, Vec2::new(width, row_height));
        rects.push(cell.shrink(TILE_GAP / 2.0));
    }
    rects
}

/// Cascade rects for `n` windows inside `area`: each window is offset one
/// step down and right from the previous one and wraps back to the corner
/// before leaving the area.
pub(crate) fn cascade_rects(area: Rect, n: usize) -> Vec<Rect> {
    let size = Vec2::new(
        (area.width() * 0.65).max(240.0).min(area.width()),
        (area.height() * 0.65).max(160.0).min(area.height()),
    );
    let room = (area.size() - size).max(Vec2::ZERO);
    let steps = ((room.x.min(room.y) / CASCADE_STEP).floor() as usize).max(1);
    (0..n)
        .map(|index| {
            let offset = (index % steps) as f32 * CASCADE_STEP;
            Rect::from_min_size(area.min + Vec2::splat(offset), size)
        })
        .collect()
}

fn rect_to_array(rect: Rect) -> [f32; 4] {
    [rect.min.x, rect.min.y, rect.width(), rect.height()]
}

fn rect_from_array([x, y, w, h]: [f32; 4]) -> Rect {
    Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SavedWindow {
    pub key: PersistKey,
    #[serde(default)]
    pub child_rect: Option<[f32; 4]>,
    #[serde(default)]
    pub detached: bool,
    #[serde(default)]
    pub os_pos: Option<[f32; 2]>,
    #[serde(default)]
    pub os_size: Option<[f32; 2]>,
}

/// The persisted window layout (`layout.json`, next to settings.json). It is
/// kept apart from settings.json so moving a window never rewrites the file
/// that holds credentials.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct LayoutFile {
    pub version: u32,
    pub mode: LayoutMode,
    pub active: Option<PersistKey>,
    pub windows: Vec<SavedWindow>,
    /// The chat area the floating windows were laid out in.
    pub area: Option<[f32; 4]>,
    /// A copy of which favorites connect at startup, by
    /// `derived_profile_id`. An older version that rewrites settings.json
    /// drops their `auto_connect` (and `id`); it comes back from here.
    pub auto_connect: Vec<String>,
}

impl LayoutFile {
    pub(crate) fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("linefeed").join("layout.json"))
    }

    /// Load the layout; a missing or unreadable file means the default
    /// (maximized) layout. Nothing in it is worth a backup.
    pub(crate) fn load_from(path: &Path) -> Self {
        let Ok(data) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str::<Self>(&data) {
            Ok(mut layout) => {
                layout.windows.truncate(MAX_SAVED_WINDOWS);
                layout
            }
            Err(error) => {
                tracing::warn!("Ignoring unreadable window layout {:?}: {}", path, error);
                Self::default()
            }
        }
    }

    pub(crate) fn load() -> Self {
        Self::path()
            .map(|path| Self::load_from(&path))
            .unwrap_or_default()
    }

    /// Write atomically and privately: window titles name channels and query
    /// partners.
    pub(crate) fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        let result = super::types::write_settings_file(&tmp, &data)
            .and_then(|_| std::fs::rename(&tmp, path));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    pub(crate) fn save(&self) {
        if let Some(path) = Self::path()
            && let Err(error) = self.save_to(&path)
        {
            tracing::warn!("Failed to save window layout: {}", error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(conn: ConnId, target: Option<&str>) -> WindowKey {
        WindowKey::new(conn, target.map(str::to_string))
    }

    fn present(conn: ConnId, network: &str, target: Option<&str>) -> PresentWindow {
        PresentWindow {
            key: key(conn, target),
            persist: PersistKey {
                network: network.to_string(),
                target: target.map(str::to_lowercase),
            },
        }
    }

    fn area() -> Rect {
        Rect::from_min_size(Pos2::new(10.0, 50.0), Vec2::new(1000.0, 700.0))
    }

    #[test]
    fn tile_places_every_floating_window_inside_the_central_rect_without_overlap() {
        for n in 1..=12 {
            let rects = tile_rects(area(), n);
            assert_eq!(rects.len(), n);
            for (i, a) in rects.iter().enumerate() {
                assert!(area().contains_rect(*a), "{n}: {a:?} outside");
                assert!(a.width() > 0.0 && a.height() > 0.0);
                for b in &rects[i + 1..] {
                    assert!(!a.intersects(*b), "{n}: {a:?} overlaps {b:?}");
                }
            }
        }
    }

    #[test]
    fn cascade_keeps_every_window_inside_the_area_and_offsets_each_one() {
        let rects = cascade_rects(area(), 30);
        assert_eq!(rects.len(), 30);
        for rect in &rects {
            assert!(area().contains_rect(*rect), "{rect:?}");
        }
        assert_ne!(rects[0].min, rects[1].min);
    }

    #[test]
    fn tile_puts_the_active_window_last_and_switches_to_floating() {
        let mut manager = WindowManager::default();
        manager.sync(&[
            present(1, "net", None),
            present(1, "net", Some("#a")),
            present(1, "net", Some("#b")),
        ]);
        manager.active = Some(key(1, Some("#a")));
        manager.tile(area());
        assert_eq!(manager.mode, LayoutMode::Floating);
        assert!(manager.windows.iter().all(|w| w.pending_place.is_some()));
        assert!(manager.dirty);
    }

    #[test]
    fn closed_windows_keep_their_placement_for_when_they_reopen() {
        let mut manager = WindowManager::default();
        manager.sync(&[present(1, "net", None), present(1, "net", Some("#a"))]);
        let rect = Rect::from_min_size(Pos2::new(20.0, 30.0), Vec2::new(300.0, 200.0));
        let window = manager.get_mut(&key(1, Some("#a"))).unwrap();
        window.child_rect = Some(rect);
        window.detached = true;

        // Parted: the window goes away, then the channel is joined again on
        // a later connection with a different connection id.
        manager.sync(&[present(1, "net", None)]);
        assert!(manager.get(&key(1, Some("#a"))).is_none());
        let appeared = manager.sync(&[present(2, "net", None), present(2, "net", Some("#A"))]);
        assert_eq!(appeared.len(), 2);
        let window = manager.get(&key(2, Some("#A"))).unwrap();
        assert_eq!(window.child_rect, Some(rect));
        // Closed windows come back inside the main window.
        assert!(!window.detached);
    }

    #[test]
    fn layout_round_trips_and_a_corrupt_layout_falls_back_to_maximized() {
        let dir = std::env::temp_dir().join(format!(
            "linefeed-layout-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let path = dir.join("layout.json");

        let mut manager = WindowManager {
            mode: LayoutMode::Floating,
            ..WindowManager::default()
        };
        manager.sync(&[present(1, "tls://irc.example:6697", Some("#rust"))]);
        manager.active = Some(key(1, Some("#rust")));
        {
            let window = manager.get_mut(&key(1, Some("#rust"))).unwrap();
            window.detached = true;
            window.os_pos = Some(Pos2::new(100.0, 120.0));
            window.os_size = Some(Vec2::new(640.0, 480.0));
        }
        let layout = manager.layout();
        layout.save_to(&path).unwrap();
        assert_eq!(LayoutFile::load_from(&path), layout);

        let mut restored = WindowManager::from_layout(LayoutFile::load_from(&path));
        assert_eq!(restored.mode, LayoutMode::Floating);
        restored.sync(&[present(7, "tls://irc.example:6697", Some("#Rust"))]);
        // Reported for the App to apply, never applied behind its back.
        assert_eq!(restored.active, None);
        assert_eq!(restored.restore_match, Some(key(7, Some("#Rust"))));
        let window = restored.get(&key(7, Some("#Rust"))).unwrap();
        assert!(window.detached);
        assert_eq!(window.os_size, Some(Vec2::new(640.0, 480.0)));

        std::fs::write(&path, "{ not json").unwrap();
        let fallback = LayoutFile::load_from(&path);
        assert_eq!(fallback.mode, LayoutMode::Maximized);
        assert!(fallback.windows.is_empty());
        assert_eq!(
            LayoutFile::load_from(&dir.join("missing.json")),
            LayoutFile::default()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn saved_layout_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "linefeed-layout-perm-{}-{}",
            std::process::id(),
            line!()
        ));
        let path = dir.join("layout.json");
        LayoutFile::default().save_to(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn windows_of_a_session_that_never_connected_are_not_persisted() {
        let mut manager = WindowManager::default();
        manager.sync(&[
            present(1, "tls://irc.example:6697", None),
            present(2, &format!("{UNCONNECTED_PREFIX}2"), None),
        ]);
        manager.active = Some(key(2, None));
        let layout = manager.layout();
        assert_eq!(layout.windows.len(), 1);
        assert_eq!(layout.windows[0].key.network, "tls://irc.example:6697");
        assert_eq!(layout.active, None);
    }

    #[test]
    fn tiled_windows_are_rescaled_when_the_chat_area_changes() {
        let mut manager = WindowManager::default();
        manager.sync(&[
            present(1, "net", None),
            present(1, "net", Some("#a")),
            present(1, "net", Some("#b")),
            present(1, "net", Some("#c")),
        ]);
        manager.fit_to_area(area());
        manager.tile(area());
        // The switchbar wraps (the area loses 22 points at the top) and the
        // main window is made narrower.
        let smaller = Rect::from_min_max(
            area().min + Vec2::new(0.0, 22.0),
            area().max - Vec2::new(200.0, 0.0),
        );
        manager.fit_to_area(smaller);
        let rects: Vec<Rect> = manager
            .windows
            .iter()
            .map(|w| w.pending_place.unwrap())
            .collect();
        for (i, a) in rects.iter().enumerate() {
            assert!(
                smaller.expand(0.01).contains_rect(*a),
                "{a:?} outside {smaller:?}"
            );
            for b in &rects[i + 1..] {
                assert!(!a.intersects(*b), "{a:?} overlaps {b:?}");
            }
        }
        // Same area again: nothing to do.
        for window in &mut manager.windows {
            window.pending_place = None;
        }
        manager.fit_to_area(smaller);
        assert!(manager.windows.iter().all(|w| w.pending_place.is_none()));
    }

    #[test]
    fn the_layout_area_is_remembered_with_the_layout() {
        let mut manager = WindowManager::default();
        manager.fit_to_area(area());
        let restored = WindowManager::from_layout(manager.layout());
        assert_eq!(restored.area, Some(area()));
    }

    #[test]
    fn active_window_is_cleared_when_it_disappears() {
        let mut manager = WindowManager::default();
        manager.sync(&[present(1, "net", None), present(1, "net", Some("#a"))]);
        manager.active = Some(key(1, Some("#a")));
        manager.sync(&[present(1, "net", None)]);
        assert_eq!(manager.active, None);
    }
}
