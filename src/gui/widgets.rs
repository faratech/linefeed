//! Small building blocks the redesigned window is drawn from.

// Until the top bar and sidebar are drawn with these.
#![cfg_attr(not(test), allow(dead_code))]

use egui::Color32;

use super::{fonts, theme};

/// Emphasised text: the semibold cut of the interface font.
pub(crate) fn strong(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).family(fonts::semibold())
}

/// A count in a pill, such as a conversation's unread messages.
pub(crate) fn badge(ui: &mut egui::Ui, text: &str, fill: Color32) -> egui::Response {
    let palette = theme::palette(ui.ctx());
    let size = egui::TextStyle::Small.resolve(ui.style()).size;
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::new(size, fonts::semibold()),
        palette.on_accent,
    );
    let pill = egui::vec2((galley.size().x + 12.0).max(22.0), 22.0);
    let (rect, response) = ui.allocate_exact_size(pill, egui::Sense::hover());
    ui.painter().rect_filled(rect, 11.0, fill);
    ui.painter().galley(
        rect.center() - galley.size() / 2.0,
        galley,
        palette.on_accent,
    );
    response
}

/// A flat text button as tall as a control, optionally with a count badge
/// after its label.
pub(crate) fn badge_button(
    ui: &mut egui::Ui,
    label: &str,
    count: Option<(&str, Color32)>,
) -> egui::Response {
    let palette = theme::palette(ui.ctx());
    let height = theme::control_height(ui);
    let text = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::TextStyle::Button.resolve(ui.style()),
        palette.text,
    );
    let small = egui::TextStyle::Small.resolve(ui.style()).size;
    let pill = count.map(|(count, fill)| {
        let galley = ui.painter().layout_no_wrap(
            count.to_owned(),
            egui::FontId::new(small, fonts::semibold()),
            palette.on_accent,
        );
        let width = (galley.size().x + 12.0).max(22.0);
        (galley, fill, width)
    });
    let padding = 12.0;
    let width =
        padding * 2.0 + text.size().x + pill.as_ref().map_or(0.0, |(_, _, width)| 8.0 + width);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, 10.0, palette.raise);
    }
    let text_pos = egui::pos2(rect.left() + padding, rect.center().y - text.size().y / 2.0);
    let text_width = text.size().x;
    ui.painter().galley(text_pos, text, palette.text);
    if let Some((galley, fill, pill_width)) = pill {
        let pill_rect = egui::Rect::from_min_size(
            egui::pos2(text_pos.x + text_width + 8.0, rect.center().y - 11.0),
            egui::vec2(pill_width, 22.0),
        );
        ui.painter().rect_filled(pill_rect, 11.0, fill);
        ui.painter().galley(
            pill_rect.center() - galley.size() / 2.0,
            galley,
            palette.on_accent,
        );
    }
    response
}

/// One clickable row of a list, as wide as the space it is in and as tall
/// as a control. `add` lays its contents out left to right; labels inside
/// do not take the click.
pub(crate) fn list_row(
    ui: &mut egui::Ui,
    selected: bool,
    add: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let palette = theme::palette(ui.ctx());
    let height = theme::control_height(ui);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click(),
    );
    if selected {
        ui.painter().rect_filled(rect, 8.0, palette.active_row());
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 8.0, palette.raise);
    }
    let mut content = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(10.0, 0.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    content.style_mut().interaction.selectable_labels = false;
    add(&mut content);
    response
}

/// One line of text, cut with an ellipsis where it does not fit. The full
/// text shows on hover when it was cut.
pub(crate) fn truncated(ui: &mut egui::Ui, text: egui::RichText) -> egui::Response {
    ui.add(egui::Label::new(text).truncate())
}

/// The heading of a group in a list.
pub(crate) fn section_label(ui: &mut egui::Ui, text: &str) {
    let palette = theme::palette(ui.ctx());
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .small()
            .family(fonts::semibold())
            .color(palette.muted),
    );
}

/// A status dot: filled, or a ring so that it differs by shape as well as
/// by colour.
pub(crate) fn dot(ui: &mut egui::Ui, color: Color32, filled: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    if filled {
        ui.painter().circle_filled(rect.center(), 5.0, color);
    } else {
        ui.painter()
            .circle_stroke(rect.center(), 4.0, egui::Stroke::new(2.0, color));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::theme::{self, Density, DisplayPrefs, Palette};

    fn frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        add: impl FnMut(&mut egui::Ui),
    ) -> egui::FullOutput {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(400.0, 300.0),
                )),
                events,
                ..Default::default()
            },
            add,
        );
        output.textures_delta.clear();
        output
    }

    fn shapes(output: &egui::FullOutput) -> Vec<egui::Shape> {
        fn flatten(shape: &egui::Shape, out: &mut Vec<egui::Shape>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| flatten(s, out)),
                other => out.push(other.clone()),
            }
        }
        let mut out = Vec::new();
        for clipped in &output.shapes {
            flatten(&clipped.shape, &mut out);
        }
        out
    }

    fn texts(output: &egui::FullOutput) -> Vec<String> {
        shapes(output)
            .into_iter()
            .filter_map(|shape| match shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_list_row_fills_the_width_and_follows_the_control_height() {
        let ctx = egui::Context::default();
        for (density, height) in [(Density::Comfortable, 44.0), (Density::Compact, 32.0)] {
            theme::apply(
                &ctx,
                &DisplayPrefs {
                    density,
                    ..DisplayPrefs::default()
                },
            );
            let mut measured = None;
            frame(&ctx, Vec::new(), |ui| {
                let width = ui.available_width();
                let response = list_row(ui, false, |ui| {
                    ui.label("row");
                });
                measured = Some((response.rect.width(), width, response.rect.height()));
            });
            let (row_width, available, row_height) = measured.unwrap();
            assert_eq!(row_width, available);
            assert_eq!(row_height, height);
        }
    }

    #[test]
    fn only_a_selected_row_is_filled_with_the_active_row_colour() {
        let ctx = egui::Context::default();
        let active = Palette::dark().active_row();
        for selected in [true, false] {
            let output = frame(&ctx, Vec::new(), |ui| {
                list_row(ui, selected, |ui| {
                    ui.label("row");
                });
            });
            let filled = shapes(&output)
                .iter()
                .any(|shape| matches!(shape, egui::Shape::Rect(rect) if rect.fill == active));
            assert_eq!(filled, selected);
        }
    }

    #[test]
    fn a_click_on_a_label_inside_a_row_clicks_the_row() {
        let ctx = egui::Context::default();
        let mut label_pos = egui::Pos2::ZERO;
        frame(&ctx, Vec::new(), |ui| {
            list_row(ui, false, |ui| {
                label_pos = ui.label("row label").rect.center();
            });
        });
        let press = |pressed| egui::Event::PointerButton {
            pos: label_pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut clicked = false;
        for events in [
            vec![egui::Event::PointerMoved(label_pos), press(true)],
            vec![press(false)],
        ] {
            frame(&ctx, events, |ui| {
                clicked |= list_row(ui, false, |ui| {
                    ui.label("row label");
                })
                .clicked();
            });
        }
        assert!(clicked);
    }

    #[test]
    fn truncated_text_stays_inside_the_space_it_has() {
        let ctx = egui::Context::default();
        let mut width = f32::MAX;
        frame(&ctx, Vec::new(), |ui| {
            ui.allocate_ui(egui::vec2(120.0, 30.0), |ui| {
                width = truncated(ui, egui::RichText::new("x".repeat(400)))
                    .rect
                    .width();
            });
        });
        assert!(width <= 120.0, "{width}");
    }

    #[test]
    fn a_badge_is_a_pill_at_least_22_px_wide_showing_its_text() {
        let ctx = egui::Context::default();
        let fill = Palette::dark().accent;
        let mut rect = egui::Rect::NOTHING;
        let output = frame(&ctx, Vec::new(), |ui| {
            rect = badge(ui, "1", fill).rect;
        });
        assert!(rect.width() >= 22.0, "{rect:?}");
        assert!(texts(&output).contains(&"1".to_owned()));
        assert!(shapes(&output).iter().any(
            |shape| matches!(shape, egui::Shape::Rect(pill) if pill.fill == fill && pill.rect == rect)
        ));
    }

    #[test]
    fn a_badge_button_is_a_control_tall_click_target_with_its_count() {
        let ctx = egui::Context::default();
        theme::apply(&ctx, &DisplayPrefs::default());
        let fill = Palette::dark().mention;
        let mut rect = egui::Rect::NOTHING;
        let output = frame(&ctx, Vec::new(), |ui| {
            rect = badge_button(ui, "Activity", Some(("2", fill))).rect;
        });
        assert_eq!(rect.height(), 44.0);
        let texts = texts(&output);
        assert!(texts.contains(&"Activity".to_owned()) && texts.contains(&"2".to_owned()));
        assert!(shapes(&output).iter().any(
            |shape| matches!(shape, egui::Shape::Rect(pill) if pill.fill == fill && rect.contains_rect(pill.rect))
        ));

        let mut plain = egui::Rect::NOTHING;
        let output = frame(&ctx, Vec::new(), |ui| {
            plain = badge_button(ui, "Activity", None).rect;
        });
        assert!(plain.width() < rect.width());
        assert!(
            !shapes(&output)
                .iter()
                .any(|shape| matches!(shape, egui::Shape::Rect(pill) if pill.fill == fill))
        );

        let press = |pressed| egui::Event::PointerButton {
            pos: rect.center(),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut clicked = false;
        for events in [
            vec![egui::Event::PointerMoved(rect.center()), press(true)],
            vec![press(false)],
        ] {
            frame(&ctx, events, |ui| {
                clicked |= badge_button(ui, "Activity", Some(("2", fill))).clicked();
            });
        }
        assert!(clicked);
    }

    #[test]
    fn a_dot_is_filled_or_a_ring() {
        let ctx = egui::Context::default();
        let color = Palette::dark().online;
        for filled in [true, false] {
            let output = frame(&ctx, Vec::new(), |ui| {
                dot(ui, color, filled);
            });
            let circle = shapes(&output)
                .into_iter()
                .find_map(|shape| match shape {
                    egui::Shape::Circle(circle) => Some(circle),
                    _ => None,
                })
                .expect("a circle");
            assert_eq!(circle.fill == color, filled);
            assert_eq!(circle.stroke.width == 2.0, !filled);
        }
    }

    #[test]
    fn strong_and_section_labels_draw_without_installed_fonts() {
        let ctx = egui::Context::default();
        let output = frame(&ctx, Vec::new(), |ui| {
            ui.label(strong("Name"));
            section_label(ui, "Operators · 2");
        });
        let texts = texts(&output);
        assert!(texts.contains(&"Name".to_owned()), "{texts:?}");
        assert!(texts.contains(&"OPERATORS · 2".to_owned()), "{texts:?}");
    }
}
