//! The fonts the interface draws with, loaded from the system.
//!
//! This build carries no fonts of its own, so every glyph comes from a font
//! file found on the system: a proportional face for the interface and
//! messages, a semibold cut of it for emphasis, a monospace face, and
//! symbol and emoji faces every family falls back to.

use std::sync::Arc;

/// Name of the font family used for emphasis (names, headings, buttons).
pub(crate) const SEMIBOLD: &str = "semibold";

/// The family to draw emphasised text with.
pub(crate) fn semibold() -> egui::FontFamily {
    egui::FontFamily::Name(SEMIBOLD.into())
}

/// Candidate font files for each role, best first.
pub(crate) struct FontPlan {
    pub proportional: &'static [&'static str],
    pub semibold: &'static [&'static str],
    pub monospace: &'static [&'static str],
    pub symbols: &'static [&'static str],
    pub emoji: &'static [&'static str],
}

/// The candidates for the platform this was built for.
pub(crate) fn plan() -> FontPlan {
    if cfg!(windows) {
        FontPlan {
            proportional: &["C:\\Windows\\Fonts\\segoeui.ttf"],
            semibold: &[
                "C:\\Windows\\Fonts\\seguisb.ttf",
                "C:\\Windows\\Fonts\\segoeuib.ttf",
            ],
            monospace: &["C:\\Windows\\Fonts\\consola.ttf"],
            // Block elements, box drawing and other symbols.
            symbols: &[
                "C:\\Windows\\Fonts\\seguisym.ttf",
                "C:\\Windows\\Fonts\\consola.ttf",
                "C:\\Windows\\Fonts\\segoeui.ttf",
            ],
            emoji: &["C:\\Windows\\Fonts\\seguiemj.ttf"],
        }
    } else if cfg!(target_os = "macos") {
        FontPlan {
            proportional: &[
                "/System/Library/Fonts/SFNS.ttf",
                "/System/Library/Fonts/Helvetica.ttc",
            ],
            semibold: &[],
            monospace: &[
                "/System/Library/Fonts/Menlo.ttc",
                "/System/Library/Fonts/Monaco.ttf",
            ],
            symbols: &[
                "/System/Library/Fonts/Menlo.ttc",
                "/System/Library/Fonts/Monaco.ttf",
            ],
            emoji: &[
                "/System/Library/Fonts/Apple Color Emoji.ttc",
                "/Library/Fonts/Apple Color Emoji.ttc",
            ],
        }
    } else {
        FontPlan {
            proportional: &[
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            ],
            semibold: &[
                "/usr/share/fonts/truetype/noto/NotoSans-SemiBold.ttf",
                "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            ],
            monospace: &[
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
                "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
            ],
            symbols: &[
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                "/usr/share/fonts/truetype/unifont/unifont.ttf",
                "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
            ],
            emoji: &[
                "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
                "/usr/share/fonts/noto-emoji/NotoColorEmoji.ttf",
                "/usr/share/fonts/google-noto-emoji/NotoColorEmoji.ttf",
            ],
        }
    }
}

/// Build the font definitions from whichever candidates `read` can load.
///
/// Each family lists its own face first, then the symbol and emoji faces.
/// A family whose own face is missing borrows the nearest one that loaded
/// (semibold falls back to proportional), so text is drawable as long as
/// any font was found at all.
pub(crate) fn definitions(
    plan: &FontPlan,
    read: impl Fn(&str) -> Option<Vec<u8>>,
) -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::empty();
    let mut load = |name: &str, candidates: &[&str]| {
        let data = candidates.iter().find_map(|path| read(path))?;
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(egui::FontData::from_owned(data)));
        Some(name.to_owned())
    };
    let proportional = load("proportional", plan.proportional);
    let semibold_face = load("semibold", plan.semibold);
    let monospace = load("monospace", plan.monospace);
    let fallbacks: Vec<String> = [load("symbols", plan.symbols), load("emoji", plan.emoji)]
        .into_iter()
        .flatten()
        .collect();

    let family = |own: &[&Option<String>]| -> Vec<String> {
        own.iter()
            .find_map(|face| face.as_ref())
            .cloned()
            .into_iter()
            .chain(fallbacks.iter().cloned())
            .collect()
    };
    let families = [
        (
            egui::FontFamily::Proportional,
            family(&[&proportional, &monospace]),
        ),
        (
            egui::FontFamily::Monospace,
            family(&[&monospace, &proportional]),
        ),
        (
            semibold(),
            family(&[&semibold_face, &proportional, &monospace]),
        ),
    ];
    fonts.families.extend(families);
    fonts
}

/// Load the system fonts into `ctx`.
pub fn install(ctx: &egui::Context) {
    let fonts = definitions(&plan(), |path| {
        let data = std::fs::read(path).ok()?;
        tracing::info!("Loaded font {path}");
        Some(data)
    });
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_family_lists_its_own_face_then_the_shared_fallbacks() {
        let defs = definitions(&plan(), |_| Some(vec![0u8; 4]));
        let prop = &defs.families[&egui::FontFamily::Proportional];
        let mono = &defs.families[&egui::FontFamily::Monospace];
        let semi = &defs.families[&semibold()];
        assert_eq!(
            (prop[0].as_str(), mono[0].as_str(), semi[0].as_str()),
            ("proportional", "monospace", "semibold")
        );
        for family in [prop, mono, semi] {
            assert!(family.contains(&"symbols".to_owned()), "{family:?}");
            assert!(family.contains(&"emoji".to_owned()), "{family:?}");
        }
    }

    #[test]
    fn a_system_with_no_fonts_still_defines_every_family() {
        let defs = definitions(&plan(), |_| None);
        for family in [
            egui::FontFamily::Proportional,
            egui::FontFamily::Monospace,
            semibold(),
        ] {
            assert!(defs.families.contains_key(&family), "{family:?}");
        }
        assert!(defs.font_data.is_empty());
    }

    #[test]
    fn a_missing_semibold_face_falls_back_to_the_proportional_one() {
        let plan = plan();
        let defs = definitions(&plan, |path| {
            (!plan.semibold.contains(&path)).then(|| vec![0u8; 4])
        });
        assert_eq!(defs.families[&semibold()][0], "proportional");
        assert!(!defs.font_data.contains_key("semibold"));
    }

    #[test]
    fn a_system_with_only_a_symbols_font_draws_everything_with_it() {
        let plan = plan();
        // A file that is a candidate for the symbols role and no other.
        let Some(only) = plan.symbols.iter().copied().find(|path| {
            !plan.proportional.contains(path)
                && !plan.semibold.contains(path)
                && !plan.monospace.contains(path)
        }) else {
            return;
        };
        let defs = definitions(&plan, |path| (path == only).then(|| vec![0u8; 4]));
        for family in [
            egui::FontFamily::Proportional,
            egui::FontFamily::Monospace,
            semibold(),
        ] {
            assert_eq!(defs.families[&family], ["symbols"], "{family:?}");
        }
    }

    #[test]
    fn the_first_readable_candidate_wins() {
        let plan = plan();
        let wanted = *plan.proportional.last().unwrap();
        let seen = std::cell::RefCell::new(Vec::new());
        let defs = definitions(&plan, |path| {
            seen.borrow_mut().push(path.to_owned());
            (path == wanted).then(|| vec![7u8; 4])
        });
        let bytes: &[u8] = (*defs.font_data["proportional"].font).as_ref();
        assert_eq!(bytes, [7u8; 4]);
        assert!(
            seen.borrow()
                .iter()
                .any(|path| path == plan.proportional[0])
        );
    }
}
