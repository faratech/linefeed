//! Snap layouts for the floating windows inside the main window, after
//! Windows 11. Resting the pointer on a window's maximize button opens a
//! grid of layouts; dragging a window by its caption to the top of the chat
//! area shows the same grid, and to a side or a corner previews that half or
//! quarter. A window dropped into a zone fills it, and the zones left over
//! offer the other windows (Snap Assist).

use std::time::Duration;

use egui::{
    Align, Align2, Color32, Id, LayerId, Layout, Order, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, UiBuilder, Vec2,
};

use super::app::App;
use super::windows::{LayoutMode, TILE_GAP, WindowKey};

/// A zone as fractions of the chat area: left, top, right, bottom.
type Zone = [f32; 4];

const THIRD: f32 = 1.0 / 3.0;
const TWO_THIRDS: f32 = 2.0 / 3.0;

/// The layouts Windows 11 offers on a landscape screen: halves, two thirds
/// and a third, thirds, a half and two quarters, and quarters.
pub(crate) const LAYOUTS: [&[Zone]; 5] = [
    &[[0.0, 0.0, 0.5, 1.0], [0.5, 0.0, 1.0, 1.0]],
    &[[0.0, 0.0, TWO_THIRDS, 1.0], [TWO_THIRDS, 0.0, 1.0, 1.0]],
    &[
        [0.0, 0.0, THIRD, 1.0],
        [THIRD, 0.0, TWO_THIRDS, 1.0],
        [TWO_THIRDS, 0.0, 1.0, 1.0],
    ],
    &[
        [0.0, 0.0, 0.5, 1.0],
        [0.5, 0.0, 1.0, 0.5],
        [0.5, 0.5, 1.0, 1.0],
    ],
    &[
        [0.0, 0.0, 0.5, 0.5],
        [0.5, 0.0, 1.0, 0.5],
        [0.0, 0.5, 0.5, 1.0],
        [0.5, 0.5, 1.0, 1.0],
    ],
];
const HALVES: usize = 0;
const QUARTERS: usize = 4;

/// How near an edge of the chat area the pointer dragging a window must
/// come for the window to snap, in points.
const EDGE: f32 = 10.0;
/// How long the pointer rests on a maximize button before the layouts
/// open, in seconds; passing over it on the way to Close opens nothing.
const FLYOUT_DELAY: f64 = 0.3;
/// One layout's picture in the grid (landscape).
const CARD: Vec2 = Vec2::new(66.0, 42.0);
/// Layout pictures per row of the grid, and the space between them.
const GRID_COLUMNS: usize = 3;
const GRID_SPACING: f32 = 8.0;

/// Where a window would snap: the zones of a layout, and the one it takes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SnapTarget {
    pub(crate) zones: Vec<Rect>,
    pub(crate) zone: usize,
}

impl SnapTarget {
    pub(crate) fn layout(area: Rect, layout: usize, zone: usize) -> Self {
        Self {
            zones: layout_rects(area, layout),
            zone,
        }
    }

    pub(crate) fn rect(&self) -> Rect {
        self.zones[self.zone]
    }

    /// The zones left for Snap Assist.
    fn rest(&self) -> Vec<Rect> {
        self.zones
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != self.zone)
            .map(|(_, rect)| *rect)
            .collect()
    }
}

fn portrait(area: Rect) -> bool {
    area.height() > area.width()
}

/// A zone turned on its side for a portrait area, as Windows does.
fn oriented(zone: Zone, portrait: bool) -> Zone {
    let [left, top, right, bottom] = zone;
    if portrait {
        [top, left, bottom, right]
    } else {
        zone
    }
}

fn zone_in(rect: Rect, [left, top, right, bottom]: Zone) -> Rect {
    Rect::from_min_max(
        rect.lerp_inside(Vec2::new(left, top)),
        rect.lerp_inside(Vec2::new(right, bottom)),
    )
}

/// A layout's zones as window rects in `area`, with the same gap between
/// windows as Tile.
pub(crate) fn layout_rects(area: Rect, layout: usize) -> Vec<Rect> {
    let portrait = portrait(area);
    LAYOUTS[layout]
        .iter()
        .map(|&zone| zone_in(area, oriented(zone, portrait)).shrink(TILE_GAP / 2.0))
        .collect()
}

/// A window dragged to a side of the chat area takes that half, into a
/// corner that quarter. The top edge away from the corners opens the layout
/// bar instead, and gives `None` here.
pub(crate) fn edge_target(area: Rect, pointer: Pos2) -> Option<SnapTarget> {
    let corner = Vec2::new(
        (area.width() * 0.1).max(32.0),
        (area.height() * 0.1).max(32.0),
    );
    let at_left = pointer.x <= area.left() + EDGE;
    let at_right = pointer.x >= area.right() - EDGE;
    let at_top = pointer.y <= area.top() + EDGE;
    let at_bottom = pointer.y >= area.bottom() - EDGE;
    let left = pointer.x <= area.left() + corner.x;
    let right = pointer.x >= area.right() - corner.x;
    let top = pointer.y <= area.top() + corner.y;
    let bottom = pointer.y >= area.bottom() - corner.y;
    // Edge snaps follow the edge, whatever the area's orientation.
    let fixed = |layout: usize, zone: usize| SnapTarget {
        zones: LAYOUTS[layout]
            .iter()
            .map(|&z| zone_in(area, z).shrink(TILE_GAP / 2.0))
            .collect(),
        zone,
    };
    let quarter = if (at_left || at_top) && left && top {
        Some(0)
    } else if (at_right || at_top) && right && top {
        Some(1)
    } else if (at_left || at_bottom) && left && bottom {
        Some(2)
    } else if (at_right || at_bottom) && right && bottom {
        Some(3)
    } else {
        None
    };
    if let Some(zone) = quarter {
        Some(fixed(QUARTERS, zone))
    } else if at_left {
        Some(fixed(HALVES, 0))
    } else if at_right {
        Some(fixed(HALVES, 1))
    } else {
        None
    }
}

/// A window dragged here opens the layout bar.
fn at_top_edge(area: Rect, pointer: Pos2) -> bool {
    pointer.y <= area.top() + EDGE && edge_target(area, pointer).is_none()
}

pub(crate) fn maximize_button_id(key: &WindowKey) -> Id {
    Id::unique(("linefeed_snap_maximize", key.conn, key.target.clone()))
}

fn close_button_id(key: &WindowKey) -> Id {
    Id::unique(("linefeed_snap_close", key.conn, key.target.clone()))
}

pub(crate) fn caption_id(key: &WindowKey) -> Id {
    Id::unique(("linefeed_snap_caption", key.conn, key.target.clone()))
}

pub(crate) fn flyout_id() -> Id {
    Id::unique("linefeed_snap_flyout")
}

pub(crate) fn bar_id() -> Id {
    Id::unique("linefeed_snap_bar")
}

/// One zone's picture in a layout grid (`flyout_id` or `bar_id`).
pub(crate) fn zone_id(grid: Id, layout: usize, zone: usize) -> Id {
    grid.with(("zone", layout, zone))
}

/// A window offered for a free zone by Snap Assist.
pub(crate) fn assist_choice_id(zone: usize, key: &WindowKey) -> Id {
    Id::unique(("linefeed_snap_assist", zone, key.conn, key.target.clone()))
}

/// What the user did with a floating window's caption.
pub(crate) enum CaptionAction {
    Close,
    Maximize,
    Detach,
}

/// A window being dragged by its caption: which, and where the pointer
/// holds it relative to the window's top left corner.
struct Drag {
    key: WindowKey,
    grab: Vec2,
}

/// Snap Assist after a snap: the zones still free, and the windows placed
/// in this layout so far.
pub(crate) struct Assist {
    pub(crate) zones: Vec<Rect>,
    pub(crate) placed: Vec<WindowKey>,
}

/// The snap state of the main window's floating windows.
#[derive(Default)]
pub(crate) struct SnapUi {
    drag: Option<Drag>,
    /// The maximize button the pointer rests on, and since when.
    hover: Option<(WindowKey, f64)>,
    /// The open layout flyout: its window and the button it hangs from.
    pub(crate) flyout: Option<(WindowKey, Rect)>,
    /// The layout bar a window was dragged to; `Rect::NOTHING` until drawn.
    pub(crate) bar: Option<Rect>,
    pub(crate) assist: Option<Assist>,
}

/// A zone of a layout grid the pointer is on, and one that was clicked, as
/// (layout, zone).
#[derive(Default)]
struct GridPick {
    hovered: Option<(usize, usize)>,
    clicked: Option<(usize, usize)>,
}

fn card_size(portrait: bool) -> Vec2 {
    if portrait {
        Vec2::new(CARD.y, CARD.x)
    } else {
        CARD
    }
}

/// The size of the layout grid, without its frame. Known up front, so the
/// popup is placed right from its first frame.
fn grid_size(portrait: bool) -> Vec2 {
    let card = card_size(portrait);
    let rows = LAYOUTS.len().div_ceil(GRID_COLUMNS);
    Vec2::new(
        card.x * GRID_COLUMNS as f32 + GRID_SPACING * (GRID_COLUMNS - 1) as f32,
        card.y * rows as f32 + GRID_SPACING * (rows - 1) as f32,
    )
}

/// Draw the layout pictures. Hovering is decided from the pointer
/// position: while a window is being dragged egui reports nothing as
/// hovered.
fn layout_grid(ui: &mut egui::Ui, grid: Id, portrait: bool, pointer: Option<Pos2>) -> GridPick {
    let card = card_size(portrait);
    let (grid_rect, _) = ui.allocate_exact_size(grid_size(portrait), Sense::hover());
    let mut pick = GridPick::default();
    for (layout, zones) in LAYOUTS.iter().enumerate() {
        let (row, column) = (layout / GRID_COLUMNS, layout % GRID_COLUMNS);
        let card_rect = Rect::from_min_size(
            grid_rect.min
                + Vec2::new(
                    column as f32 * (card.x + GRID_SPACING),
                    row as f32 * (card.y + GRID_SPACING),
                ),
            card,
        );
        for (zone, &fractions) in zones.iter().enumerate() {
            let rect = zone_in(card_rect, oriented(fractions, portrait)).shrink(1.5);
            let response = ui.interact(rect, zone_id(grid, layout, zone), Sense::click());
            let hot = pointer.is_some_and(|p| rect.contains(p));
            if hot {
                pick.hovered = Some((layout, zone));
            }
            if response.clicked() {
                pick.clicked = Some((layout, zone));
            }
            let visuals = ui.visuals();
            let fill = if hot {
                visuals.selection.bg_fill
            } else {
                visuals.widgets.inactive.bg_fill
            };
            ui.painter().rect(
                rect,
                3.0,
                fill,
                visuals.widgets.noninteractive.bg_stroke,
                StrokeKind::Inside,
            );
        }
    }
    pick
}

impl App {
    /// The caption of a floating window: its title, a maximize button that
    /// opens the snap layouts when the pointer rests on it, and a close
    /// button. Dragging the caption moves the window and can snap it; a
    /// double click maximizes it.
    pub(super) fn draw_caption(
        &mut self,
        ui: &mut egui::Ui,
        key: &WindowKey,
        active: bool,
    ) -> Option<CaptionAction> {
        let height = ui.spacing().interact_size.y + 4.0;
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
        let caption = ui.interact(rect, caption_id(key), Sense::click_and_drag());
        let visuals = ui.visuals().clone();
        let fill = if active {
            visuals.widgets.active.weak_bg_fill
        } else {
            visuals.faint_bg_color
        };
        ui.painter().rect_filled(rect, 3.0, fill);

        let button = Vec2::splat(height - 4.0);
        let close_rect = Rect::from_center_size(
            Pos2::new(rect.right() - button.x / 2.0 - 2.0, rect.center().y),
            button,
        );
        let max_rect = close_rect.translate(Vec2::new(-(button.x + 2.0), 0.0));
        let title_rect = Rect::from_min_max(
            Pos2::new(rect.left() + 6.0, rect.top()),
            Pos2::new(max_rect.left() - 4.0, rect.bottom()),
        );
        let title = RichText::new(self.window_title(key));
        let title = if active { title.strong() } else { title };
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(title_rect)
                .layout(Layout::left_to_right(Align::Center)),
            |ui| {
                ui.add(egui::Label::new(title).truncate().selectable(false));
            },
        );

        let maximize = ui.interact(max_rect, maximize_button_id(key), Sense::click());
        let close = ui
            .interact(close_rect, close_button_id(key), Sense::click())
            .on_hover_text("Close (Ctrl+W)");
        let stroke = Stroke::new(1.2, visuals.text_color());
        if maximize.hovered() {
            ui.painter()
                .rect_filled(max_rect, 2.0, visuals.widgets.hovered.bg_fill);
        }
        ui.painter()
            .rect_stroke(max_rect.shrink(5.5), 0.0, stroke, StrokeKind::Middle);
        if close.hovered() {
            ui.painter()
                .rect_filled(close_rect, 2.0, Color32::from_rgb(196, 43, 28));
        }
        let cross = close_rect.shrink(6.0);
        ui.painter()
            .line_segment([cross.left_top(), cross.right_bottom()], stroke);
        ui.painter()
            .line_segment([cross.right_top(), cross.left_bottom()], stroke);

        if caption.drag_started()
            && let Some(origin) = ui.input(|i| i.pointer.press_origin())
            && let Some(window) = self.windows.get(key).and_then(|w| w.child_rect)
        {
            self.snap.drag = Some(Drag {
                key: key.clone(),
                grab: origin - window.min,
            });
            self.snap.flyout = None;
            self.snap.hover = None;
        }
        self.track_maximize_hover(ui.ctx(), key, &maximize);

        let mut action = None;
        caption.context_menu(|ui| {
            if ui.button("Maximize").clicked() {
                action = Some(CaptionAction::Maximize);
            }
            if ui.button("Detach").clicked() {
                action = Some(CaptionAction::Detach);
            }
            if ui.button("Close").clicked() {
                action = Some(CaptionAction::Close);
            }
        });
        if close.clicked() {
            action = Some(CaptionAction::Close);
        } else if maximize.clicked() || caption.double_clicked() {
            action = Some(CaptionAction::Maximize);
        }
        action
    }

    /// Open the layout flyout once the pointer has rested on `button`, a
    /// window's maximize (or, maximized, restore) button.
    pub(super) fn track_maximize_hover(
        &mut self,
        ctx: &egui::Context,
        key: &WindowKey,
        button: &egui::Response,
    ) {
        if self.snap.drag.is_some() {
            return;
        }
        if !button.hovered() {
            if self.snap.hover.as_ref().is_some_and(|(k, _)| k == key) {
                self.snap.hover = None;
            }
            return;
        }
        let now = ctx.input(|i| i.time);
        match &self.snap.hover {
            Some((k, since)) if k == key => {
                let waited = now - since;
                if waited >= FLYOUT_DELAY {
                    self.snap.flyout = Some((key.clone(), button.rect));
                } else {
                    ctx.request_repaint_after(Duration::from_secs_f64(FLYOUT_DELAY - waited));
                }
            }
            _ => {
                self.snap.hover = Some((key.clone(), now));
                ctx.request_repaint_after(Duration::from_secs_f64(FLYOUT_DELAY));
            }
        }
    }

    /// Where the window being dragged by its caption goes this frame.
    pub(super) fn dragged_window_pos(&self, ctx: &egui::Context, key: &WindowKey) -> Option<Pos2> {
        let drag = self.snap.drag.as_ref().filter(|d| &d.key == key)?;
        ctx.input(|i| i.pointer.latest_pos()).map(|p| p - drag.grab)
    }

    /// Put `key` into a snap zone, leaving the other zones to Snap Assist.
    pub(crate) fn snap_window(&mut self, key: &WindowKey, target: SnapTarget) {
        self.windows.mode = LayoutMode::Floating;
        self.windows.place(key, target.rect());
        self.user_activate(key.clone());
        let rest = target.rest();
        self.snap.assist = (!rest.is_empty()).then(|| Assist {
            zones: rest,
            placed: vec![key.clone()],
        });
    }

    /// The snap overlays over the chat area `area`: the layout bar while a
    /// window is dragged to the top, the flyout of a maximize button, the
    /// preview of where a window would go, and Snap Assist.
    pub(super) fn draw_snap(&mut self, ctx: &egui::Context, area: Rect) {
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.snap.flyout = None;
            self.snap.bar = None;
            self.snap.assist = None;
        }
        let pointer = ctx.input(|i| i.pointer.latest_pos());
        let mut preview = None;

        if let Some(key) = self.snap.drag.as_ref().map(|d| d.key.clone()) {
            let mut target = None;
            if let Some(p) = pointer {
                let near_bar = self
                    .snap
                    .bar
                    .is_some_and(|bar| bar.expand(16.0).contains(p));
                if at_top_edge(area, p) || near_bar {
                    let (rect, pick) = self.draw_layout_grid_area(
                        ctx,
                        bar_id(),
                        Align2::CENTER_TOP,
                        area.center_top() + Vec2::new(0.0, 6.0),
                        portrait(area),
                        pointer,
                    );
                    self.snap.bar = Some(rect);
                    target = pick
                        .hovered
                        .map(|(layout, zone)| SnapTarget::layout(area, layout, zone));
                } else {
                    self.snap.bar = None;
                    target = edge_target(area, p);
                }
            }
            if ctx.input(|i| i.pointer.primary_down()) {
                preview = target.as_ref().map(SnapTarget::rect);
            } else {
                self.snap.drag = None;
                self.snap.bar = None;
                if let Some(target) = target {
                    self.snap_window(&key, target);
                }
            }
        }

        if let Some((key, anchor)) = self.snap.flyout.clone() {
            if self.windows.get(&key).is_none_or(|w| w.detached) {
                self.snap.flyout = None;
            } else {
                let (rect, pick) = self.draw_layout_grid_area(
                    ctx,
                    flyout_id(),
                    Align2::RIGHT_TOP,
                    anchor.right_bottom() + Vec2::new(0.0, 2.0),
                    portrait(area),
                    pointer,
                );
                if let Some((layout, zone)) = pick.clicked {
                    self.snap.flyout = None;
                    self.snap.hover = None;
                    self.snap_window(&key, SnapTarget::layout(area, layout, zone));
                } else {
                    preview = preview.or_else(|| {
                        pick.hovered
                            .map(|(layout, zone)| layout_rects(area, layout)[zone])
                    });
                    // Open while the pointer is on the button, the flyout or
                    // between them.
                    let keep = pointer.is_some_and(|p| anchor.union(rect).expand(8.0).contains(p));
                    if !keep {
                        self.snap.flyout = None;
                    }
                }
            }
        }

        if let Some(rect) = preview {
            let color = ctx.global_style().visuals.selection.bg_fill;
            ctx.layer_painter(LayerId::new(
                Order::Foreground,
                Id::unique("linefeed_snap_preview"),
            ))
            .rect(
                rect,
                6.0,
                color.gamma_multiply(0.35),
                Stroke::new(2.0, color),
                StrokeKind::Inside,
            );
        }

        self.draw_snap_assist(ctx);
    }

    /// A layout grid in a popup frame above the windows; returns the
    /// popup's rect and what `layout_grid` returns.
    fn draw_layout_grid_area(
        &self,
        ctx: &egui::Context,
        id: Id,
        pivot: Align2,
        pos: Pos2,
        portrait: bool,
        pointer: Option<Pos2>,
    ) -> (Rect, GridPick) {
        let frame = egui::Frame::popup(&ctx.global_style());
        let size = grid_size(portrait) + frame.total_margin().sum();
        let min = pos
            - Vec2::new(
                size.x * pivot.x().to_factor(),
                size.y * pivot.y().to_factor(),
            );
        let shown = egui::Area::new(id)
            .order(Order::Tooltip)
            .fixed_pos(min)
            .show(ctx, |ui| {
                frame
                    .show(ui, |ui| layout_grid(ui, id, portrait, pointer))
                    .inner
            });
        (shown.response.rect, shown.inner)
    }

    /// Snap Assist: each zone left free by a snap lists the other windows;
    /// picking one puts it there. A click elsewhere or Escape dismisses it.
    fn draw_snap_assist(&mut self, ctx: &egui::Context) {
        if self.windows.mode != LayoutMode::Floating {
            self.snap.assist = None;
        }
        let Some(assist) = &self.snap.assist else {
            return;
        };
        let candidates: Vec<WindowKey> = self
            .ordered_windows()
            .into_iter()
            .filter(|key| {
                self.windows.get(key).is_some_and(|w| !w.detached) && !assist.placed.contains(key)
            })
            .collect();
        if candidates.is_empty() || assist.zones.is_empty() {
            self.snap.assist = None;
            return;
        }
        let zones = assist.zones.clone();
        let titles: Vec<String> = candidates.iter().map(|k| self.window_title(k)).collect();
        let mut pick = None;
        for (index, zone) in zones.iter().enumerate() {
            egui::Area::new(Id::unique(("linefeed_snap_assist_zone", index)))
                .order(Order::Foreground)
                .fixed_pos(zone.min)
                .constrain(false)
                .show(ctx, |ui| {
                    let frame = egui::Frame::popup(ui.style());
                    let margin = frame.total_margin().sum();
                    frame.show(ui, |ui| {
                        let size = (zone.size() - margin).max(Vec2::ZERO);
                        ui.set_min_size(size);
                        ui.set_max_size(size);
                        ui.label(RichText::new("Choose a window for this space").strong());
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .id_salt(("snap_assist", index))
                            .show(ui, |ui| {
                                for (key, title) in candidates.iter().zip(&titles) {
                                    let (rect, _) = ui.allocate_exact_size(
                                        Vec2::new(
                                            ui.available_width(),
                                            ui.spacing().interact_size.y + 6.0,
                                        ),
                                        Sense::hover(),
                                    );
                                    let response = ui.interact(
                                        rect,
                                        assist_choice_id(index, key),
                                        Sense::click(),
                                    );
                                    let visuals = ui.visuals();
                                    if response.hovered() {
                                        ui.painter().rect_filled(
                                            rect,
                                            3.0,
                                            visuals.widgets.hovered.bg_fill,
                                        );
                                    }
                                    ui.painter().text(
                                        rect.left_center() + Vec2::new(8.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        title,
                                        egui::TextStyle::Button.resolve(ui.style()),
                                        visuals.text_color(),
                                    );
                                    if response.clicked() {
                                        pick = Some((index, key.clone()));
                                    }
                                }
                            });
                    });
                });
        }
        if let Some((index, key)) = pick {
            // As on Windows, the window picked comes forward with focus.
            self.windows.place(&key, zones[index]);
            self.user_activate(key.clone());
            if let Some(assist) = &mut self.snap.assist {
                assist.zones.remove(index);
                assist.placed.push(key);
            }
            return;
        }
        let pressed_elsewhere = ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| !zones.iter().any(|zone| zone.contains(p)))
        });
        if pressed_elsewhere {
            self.snap.assist = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::from_min_size(Pos2::new(0.0, 60.0), Vec2::new(1200.0, 700.0))
    }

    #[test]
    fn every_layout_fills_the_area_without_overlap() {
        for area in [
            area(),
            Rect::from_min_size(Pos2::ZERO, Vec2::new(500.0, 900.0)),
        ] {
            for layout in 0..LAYOUTS.len() {
                let rects = layout_rects(area, layout);
                for (i, a) in rects.iter().enumerate() {
                    assert!(area.contains_rect(*a), "{layout}: {a:?} outside {area:?}");
                    for b in &rects[i + 1..] {
                        assert!(!a.intersects(*b), "{layout}: {a:?} overlaps {b:?}");
                    }
                }
                // Only the gaps between windows are left uncovered.
                let covered: f32 = rects.iter().map(|r| r.area()).sum();
                let gaps = (area.width() + area.height()) * TILE_GAP * rects.len() as f32;
                assert!(covered > area.area() - gaps, "{layout}: {covered}");
            }
        }
    }

    #[test]
    fn a_portrait_area_turns_the_layouts_on_their_side() {
        let tall = Rect::from_min_size(Pos2::ZERO, Vec2::new(500.0, 900.0));
        let halves = layout_rects(tall, HALVES);
        // Stacked, not side by side.
        assert!(halves[0].bottom() < halves[1].top());
        assert!((halves[0].width() - halves[1].width()).abs() < 0.5);
    }

    #[test]
    fn sides_snap_to_halves_and_corners_to_quarters() {
        let area = area();
        let left = edge_target(area, Pos2::new(area.left() + 2.0, area.center().y)).unwrap();
        assert_eq!(left.rect(), layout_rects(area, HALVES)[0]);
        assert_eq!(left.rest(), vec![layout_rects(area, HALVES)[1]]);
        let right = edge_target(area, Pos2::new(area.right() - 2.0, area.center().y)).unwrap();
        assert_eq!(right.rect(), layout_rects(area, HALVES)[1]);
        let quarters = layout_rects(area, QUARTERS);
        // A corner along either of its edges.
        let top_left = edge_target(area, Pos2::new(area.left() + 2.0, area.top() + 20.0));
        assert_eq!(top_left.unwrap().rect(), quarters[0]);
        let top_right = edge_target(area, Pos2::new(area.right() - 30.0, area.top() + 2.0));
        assert_eq!(top_right.unwrap().rect(), quarters[1]);
        let bottom_left = edge_target(area, Pos2::new(area.left() + 40.0, area.bottom() - 1.0));
        assert_eq!(bottom_left.unwrap().rect(), quarters[2]);
        let bottom_right = edge_target(area, Pos2::new(area.right(), area.bottom()));
        assert_eq!(bottom_right.unwrap().rect(), quarters[3]);
        // The middle of the area, and of the top and bottom edges, do not snap.
        assert!(edge_target(area, area.center()).is_none());
        assert!(edge_target(area, area.center_bottom()).is_none());
        assert!(edge_target(area, area.center_top()).is_none());
        assert!(at_top_edge(area, area.center_top()));
        assert!(!at_top_edge(area, area.left_top()));
    }
}
