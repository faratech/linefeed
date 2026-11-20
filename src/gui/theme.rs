//! Colours, sizes and display preferences for the whole interface.
//!
//! Drawing code asks [`palette`] and [`metrics`] for a role instead of
//! writing a colour or a size, so the dark and light themes and the two
//! spacings stay consistent everywhere.

use egui::Color32;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThemeChoice {
    #[default]
    Dark,
    Light,
    /// Follow the operating system; dark where it does not say.
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Density {
    #[default]
    Comfortable,
    Compact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MessageFont {
    #[default]
    Proportional,
    Monospace,
}

/// The display choices a user makes, as stored in `Settings`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DisplayPrefs {
    pub theme: ThemeChoice,
    pub density: Density,
    pub message_font: MessageFont,
    pub font_size: f32,
    pub color_nicks: bool,
}

pub(crate) const DEFAULT_FONT_SIZE: f32 = 15.0;

impl Default for DisplayPrefs {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::default(),
            density: Density::default(),
            message_font: MessageFont::default(),
            font_size: DEFAULT_FONT_SIZE,
            color_nicks: true,
        }
    }
}

/// Every colour the interface draws with, by role.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Palette {
    /// The chat ground.
    pub bg: Color32,
    /// Bars and side panels.
    pub panel: Color32,
    /// Controls, hover, pills.
    pub raise: Color32,
    pub line: Color32,
    pub text: Color32,
    pub muted: Color32,
    /// Timestamps and notes: the quietest text that still reads.
    pub faint: Color32,
    pub accent: Color32,
    /// Text on an `accent` or `mention` fill.
    pub on_accent: Color32,
    pub mention: Color32,
    /// The fill behind a row that mentions the user.
    pub mention_row: Color32,
    pub online: Color32,
    pub nicks: [Color32; 6],
    pub dark: bool,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

fn mix(from: Color32, to: Color32, amount: f32) -> Color32 {
    let channel = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount).round() as u8;
    Color32::from_rgb(
        channel(from.r(), to.r()),
        channel(from.g(), to.g()),
        channel(from.b(), to.b()),
    )
}

impl Palette {
    pub(crate) const fn dark() -> Self {
        Self {
            bg: rgb(0x1B1E23),
            panel: rgb(0x22262C),
            raise: rgb(0x2B3038),
            line: rgb(0x343A44),
            text: rgb(0xE4E7EB),
            muted: rgb(0xA3ABB8),
            faint: rgb(0x98A1AF),
            accent: rgb(0x6CA8FF),
            on_accent: rgb(0x0E1116),
            mention: rgb(0xF2A65A),
            mention_row: rgb(0x37302A),
            online: rgb(0x5BC98C),
            nicks: [
                rgb(0x7DB7FF),
                rgb(0x5ED0C0),
                rgb(0xC39BFF),
                rgb(0xF28FB5),
                rgb(0xB7D36B),
                rgb(0xF2A65A),
            ],
            dark: true,
        }
    }

    pub(crate) const fn light() -> Self {
        Self {
            bg: rgb(0xFBFBFA),
            panel: rgb(0xF1F2F4),
            raise: rgb(0xE6E8EC),
            line: rgb(0xD5D9E0),
            text: rgb(0x1D2129),
            muted: rgb(0x4E5766),
            faint: rgb(0x5F6878),
            accent: rgb(0x1F5FD1),
            on_accent: rgb(0xFFFFFF),
            mention: rgb(0xA4500B),
            mention_row: rgb(0xF7EBDD),
            online: rgb(0x1E8E5A),
            nicks: [
                rgb(0x1F5FD1),
                rgb(0x0B7268),
                rgb(0x6B3FC4),
                rgb(0xB0236B),
                rgb(0x4D6B0A),
                rgb(0xA4500B),
            ],
            dark: false,
        }
    }

    /// A stable colour for a nickname.
    #[cfg_attr(not(test), allow(dead_code))] // until message rows use it
    pub(crate) fn nick(&self, nick: &str) -> Color32 {
        let hash: u32 = nick
            .bytes()
            .fold(0, |acc, b| acc.wrapping_add(b as u32).wrapping_mul(31));
        self.nicks[(hash as usize) % self.nicks.len()]
    }

    /// The fill of the selected row in a list: the accent blended into the
    /// panel.
    pub(crate) fn active_row(&self) -> Color32 {
        mix(self.panel, self.accent, if self.dark { 0.24 } else { 0.14 })
    }
}

/// Sizes and row options that follow the display preferences.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Metrics {
    /// Height of buttons, fields and list rows.
    pub control_height: f32,
    /// Vertical gap between message rows.
    pub row_gap: f32,
    pub message_font: MessageFont,
    pub color_nicks: bool,
}

impl Metrics {
    fn of(prefs: &DisplayPrefs) -> Self {
        let (control_height, row_gap) = match prefs.density {
            Density::Comfortable => (44.0, 6.0),
            Density::Compact => (32.0, 0.0),
        };
        Self {
            control_height,
            row_gap,
            message_font: prefs.message_font,
            color_nicks: prefs.color_nicks,
        }
    }
}

#[derive(Clone, Copy)]
struct Applied {
    palette: Palette,
    metrics: Metrics,
}

fn applied_id() -> egui::Id {
    egui::Id::NULL.with("linefeed-theme")
}

fn applied(ctx: &egui::Context) -> Applied {
    ctx.data(|data| data.get_temp(applied_id()))
        .unwrap_or(Applied {
            palette: Palette::dark(),
            metrics: Metrics::of(&DisplayPrefs::default()),
        })
}

/// The palette in force; dark until [`apply`] has run.
#[cfg_attr(not(test), allow(dead_code))] // until the drawing code reads it
pub(crate) fn palette(ctx: &egui::Context) -> Palette {
    applied(ctx).palette
}

/// The sizes in force; comfortable until [`apply`] has run.
#[cfg_attr(not(test), allow(dead_code))] // until the drawing code reads it
pub(crate) fn metrics(ctx: &egui::Context) -> Metrics {
    applied(ctx).metrics
}

/// Put the display preferences into effect: egui's visuals, spacing and
/// text sizes, and the palette and metrics the drawing code reads.
pub(crate) fn apply(ctx: &egui::Context, prefs: &DisplayPrefs) {
    let dark = match prefs.theme {
        ThemeChoice::Dark => true,
        ThemeChoice::Light => false,
        ThemeChoice::System => ctx.system_theme() != Some(egui::Theme::Light),
    };
    let palette = if dark {
        Palette::dark()
    } else {
        Palette::light()
    };
    let metrics = Metrics::of(prefs);
    ctx.set_theme(if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });

    let mut style = (*ctx.global_style()).clone();
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.panel_fill = palette.panel;
    visuals.window_fill = palette.panel;
    visuals.extreme_bg_color = palette.bg;
    visuals.faint_bg_color = palette.raise;
    visuals.code_bg_color = palette.raise;
    visuals.hyperlink_color = palette.accent;
    visuals.window_stroke = egui::Stroke::new(1.0, palette.line);
    visuals.selection.bg_fill = palette.active_row();
    visuals.selection.stroke = egui::Stroke::new(1.0, palette.text);
    let radius = egui::CornerRadius::same(10);
    let widgets = &mut visuals.widgets;
    for (widget, fill, text) in [
        (&mut widgets.noninteractive, palette.panel, palette.text),
        (&mut widgets.inactive, palette.raise, palette.text),
        (
            &mut widgets.hovered,
            mix(palette.raise, palette.text, 0.08),
            palette.text,
        ),
        (
            &mut widgets.active,
            mix(palette.raise, palette.text, 0.16),
            palette.text,
        ),
        (&mut widgets.open, palette.raise, palette.text),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = egui::Stroke::new(1.0, palette.line);
        widget.fg_stroke = egui::Stroke::new(1.0, text);
        widget.corner_radius = radius;
    }
    style.visuals = visuals;

    style.spacing.item_spacing = egui::vec2(8.0, 4.0);
    style.spacing.interact_size.y = metrics.control_height;
    style.spacing.button_padding = egui::vec2(12.0, 4.0);

    let font_size = prefs.font_size.clamp(10.0, 24.0);
    for (text_style, size) in [
        (egui::TextStyle::Small, font_size * 0.85),
        (egui::TextStyle::Body, font_size),
        (egui::TextStyle::Button, font_size),
        (egui::TextStyle::Monospace, font_size),
        (egui::TextStyle::Heading, font_size * 1.2),
    ] {
        if let Some(font_id) = style.text_styles.get_mut(&text_style) {
            font_id.size = size;
        }
    }
    ctx.set_global_style(style);
    ctx.data_mut(|data| data.insert_temp(applied_id(), Applied { palette, metrics }));
}

/// WCAG contrast ratio between two opaque colours (1.0 to 21.0).
pub(crate) fn contrast(a: Color32, b: Color32) -> f32 {
    fn luminance(color: Color32) -> f32 {
        let channel = |value: u8| {
            let c = value as f32 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// `color` unless it is too close to `ground` to read: then the nearest
/// step towards black or white that reaches `min` contrast.
#[cfg_attr(not(test), allow(dead_code))] // until the drawing code reads it
pub(crate) fn readable_on(color: Color32, ground: Color32, min: f32) -> Color32 {
    if contrast(color, ground) >= min {
        return color;
    }
    let towards = if contrast(Color32::WHITE, ground) >= contrast(Color32::BLACK, ground) {
        Color32::WHITE
    } else {
        Color32::BLACK
    };
    (1..=10)
        .map(|step| mix(color, towards, step as f32 / 10.0))
        .find(|candidate| contrast(*candidate, ground) >= min)
        .unwrap_or(towards)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_text_role_reads_on_every_ground_it_is_drawn_on() {
        for p in [Palette::dark(), Palette::light()] {
            for ground in [p.bg, p.panel, p.raise, p.mention_row] {
                for text in [p.text, p.muted, p.faint] {
                    assert!(contrast(text, ground) >= 4.5, "{text:?} on {ground:?}");
                }
            }
            for nick in p.nicks {
                assert!(
                    contrast(nick, p.bg) >= 4.5 && contrast(nick, p.mention_row) >= 4.5,
                    "{nick:?}"
                );
            }
            assert!(contrast(p.on_accent, p.accent) >= 4.5);
            assert!(contrast(p.on_accent, p.mention) >= 4.5);
            assert!(contrast(p.accent, p.bg) >= 4.5);
            assert!(contrast(p.mention, p.mention_row) >= 4.5);
        }
    }

    #[test]
    fn applying_prefs_sets_visuals_sizes_and_the_stored_palette() {
        let ctx = egui::Context::default();
        apply(
            &ctx,
            &DisplayPrefs {
                theme: ThemeChoice::Light,
                density: Density::Compact,
                message_font: MessageFont::Monospace,
                font_size: 18.0,
                color_nicks: false,
            },
        );
        assert_eq!(palette(&ctx).bg, Color32::from_rgb(0xFB, 0xFB, 0xFA));
        assert!(!ctx.global_style().visuals.dark_mode);
        assert_eq!(ctx.global_style().spacing.interact_size.y, 32.0);
        assert_eq!(
            ctx.global_style().text_styles[&egui::TextStyle::Body].size,
            18.0
        );
        assert_eq!(metrics(&ctx).row_gap, 0.0);
        assert_eq!(metrics(&ctx).message_font, MessageFont::Monospace);
        assert!(!metrics(&ctx).color_nicks);
    }

    #[test]
    fn a_context_nothing_was_applied_to_gets_the_dark_comfortable_defaults() {
        let ctx = egui::Context::default();
        assert_eq!(palette(&ctx).bg, Palette::dark().bg);
        assert_eq!(metrics(&ctx).control_height, 44.0);
        assert_eq!(metrics(&ctx).row_gap, 6.0);
    }

    #[test]
    fn font_size_is_clamped_and_scales_the_other_text_styles() {
        let ctx = egui::Context::default();
        apply(
            &ctx,
            &DisplayPrefs {
                font_size: 40.0,
                ..DisplayPrefs::default()
            },
        );
        let style = ctx.global_style();
        assert_eq!(style.text_styles[&egui::TextStyle::Body].size, 24.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Monospace].size, 24.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Small].size, 24.0 * 0.85);
        assert_eq!(
            style.text_styles[&egui::TextStyle::Heading].size,
            24.0 * 1.2
        );
    }

    #[test]
    fn readable_on_leaves_good_colours_alone_and_fixes_bad_ones() {
        let light = Palette::light();
        assert_eq!(readable_on(light.text, light.bg, 3.0), light.text);
        assert!(contrast(readable_on(Color32::WHITE, light.bg, 3.0), light.bg) >= 3.0);
        let dark = Palette::dark();
        assert!(contrast(readable_on(Color32::BLACK, dark.bg, 3.0), dark.bg) >= 3.0);
    }

    #[test]
    fn a_nick_always_gets_the_same_colour_from_the_palette() {
        let p = Palette::dark();
        assert_eq!(p.nick("alice"), p.nick("alice"));
        assert!(p.nicks.contains(&p.nick("bob")));
    }

    #[test]
    fn the_active_row_is_the_accent_blended_into_the_panel() {
        for p in [Palette::dark(), Palette::light()] {
            let row = p.active_row();
            assert_ne!(row, p.panel);
            assert!(contrast(p.text, row) >= 4.5, "{row:?}");
        }
    }
}
