//! IRC text formatting, color parsing, and rendering

use egui::{Color32, RichText};

/// IRC color codes (mIRC standard)
const IRC_COLORS: [Color32; 16] = [
    Color32::WHITE,                   // 0: White
    Color32::BLACK,                   // 1: Black
    Color32::from_rgb(0, 0, 127),     // 2: Blue (navy)
    Color32::from_rgb(0, 147, 0),     // 3: Green
    Color32::from_rgb(255, 0, 0),     // 4: Red
    Color32::from_rgb(127, 0, 0),     // 5: Brown (maroon)
    Color32::from_rgb(156, 0, 156),   // 6: Purple
    Color32::from_rgb(252, 127, 0),   // 7: Orange
    Color32::from_rgb(255, 255, 0),   // 8: Yellow
    Color32::from_rgb(0, 252, 0),     // 9: Light Green
    Color32::from_rgb(0, 147, 147),   // 10: Cyan (teal)
    Color32::from_rgb(0, 255, 255),   // 11: Light Cyan
    Color32::from_rgb(0, 0, 252),     // 12: Light Blue
    Color32::from_rgb(255, 0, 255),   // 13: Pink
    Color32::from_rgb(127, 127, 127), // 14: Grey
    Color32::from_rgb(210, 210, 210), // 15: Light Grey
];

/// Extended mIRC colors 16-98 (see https://modern.ircdocs.horse/formatting#colors-16-98)
const IRC_COLORS_EXTENDED: [u32; 83] = [
    0x470000, 0x472100, 0x474700, 0x324700, 0x004700, 0x00472c, 0x004747, 0x002747, 0x000047,
    0x2e0047, 0x470047, 0x47002a, // 16-27
    0x740000, 0x743a00, 0x747400, 0x517400, 0x007400, 0x007449, 0x007474, 0x004074, 0x000074,
    0x4b0074, 0x740074, 0x740045, // 28-39
    0xb50000, 0xb56300, 0xb5b500, 0x7db500, 0x00b500, 0x00b571, 0x00b5b5, 0x0063b5, 0x0000b5,
    0x7500b5, 0xb500b5, 0xb5006b, // 40-51
    0xff0000, 0xff8c00, 0xffff00, 0xb2ff00, 0x00ff00, 0x00ffa0, 0x00ffff, 0x008cff, 0x0000ff,
    0xa500ff, 0xff00ff, 0xff0098, // 52-63
    0xff5959, 0xffb459, 0xffff71, 0xcfff60, 0x6fff6f, 0x65ffc9, 0x6dffff, 0x59b4ff, 0x5959ff,
    0xc459ff, 0xff66ff, 0xff59bc, // 64-75
    0xff9c9c, 0xffd39c, 0xffff9c, 0xe2ff9c, 0x9cff9c, 0x9cffdb, 0x9cffff, 0x9cd3ff, 0x9c9cff,
    0xdc9cff, 0xff9cff, 0xff94d3, // 76-87
    0x000000, 0x131313, 0x282828, 0x363636, 0x4d4d4d, 0x656565, 0x818181, 0x9f9f9f, 0xbcbcbc,
    0xe2e2e2, 0xffffff, // 88-98
];

/// Resolve an IRC color code (0-99) to a color. 99 is "default" per the spec;
/// unknown codes also fall back to the default (None) rather than being
/// wrapped with modulo into an unrelated palette entry.
fn irc_color(code: usize) -> Option<Color32> {
    match code {
        0..=15 => Some(IRC_COLORS[code]),
        16..=98 => {
            let rgb = IRC_COLORS_EXTENDED[code - 16];
            Some(Color32::from_rgb(
                (rgb >> 16) as u8,
                (rgb >> 8) as u8,
                rgb as u8,
            ))
        }
        _ => None, // 99 = default color
    }
}

/// Curated palette of distinct, readable colors for nick coloring
const NICK_COLORS: [Color32; 16] = [
    Color32::from_rgb(255, 100, 100), // Light red
    Color32::from_rgb(100, 255, 100), // Light green
    Color32::from_rgb(100, 200, 255), // Light blue
    Color32::from_rgb(255, 200, 100), // Orange/gold
    Color32::from_rgb(255, 100, 255), // Pink/magenta
    Color32::from_rgb(100, 255, 255), // Cyan
    Color32::from_rgb(255, 255, 100), // Yellow
    Color32::from_rgb(200, 150, 255), // Light purple
    Color32::from_rgb(255, 150, 150), // Salmon
    Color32::from_rgb(150, 255, 200), // Mint
    Color32::from_rgb(150, 200, 255), // Sky blue
    Color32::from_rgb(255, 200, 150), // Peach
    Color32::from_rgb(200, 255, 150), // Lime
    Color32::from_rgb(255, 150, 200), // Rose
    Color32::from_rgb(150, 255, 255), // Aqua
    Color32::from_rgb(255, 220, 180), // Tan
];

/// A span of text with optional formatting
#[derive(Clone, Debug)]
struct TextSpan {
    text: String,
    fg_color: Option<Color32>,
    bg_color: Option<Color32>,
    bold: bool,
    underline: bool,
    italic: bool,
    reverse: bool,
}

/// Get a consistent color for a nick based on hash
pub fn nick_color(nick: &str) -> Color32 {
    let hash: u32 = nick
        .bytes()
        .fold(0, |acc, b| acc.wrapping_add(b as u32).wrapping_mul(31));
    NICK_COLORS[(hash as usize) % NICK_COLORS.len()]
}

/// Strip IRC color codes and formatting from text, returning plain text
pub fn strip_irc_formatting(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\x02' | '\x1D' | '\x1F' | '\x16' | '\x0F' => {
                // Bold, italic, underline, reverse, reset - skip
            }
            '\x03' => {
                // Color code - skip digits
                // Format: \x03[fg[,bg]] where fg and bg are AT MOST 2 digits;
                // further digits are literal text and must be preserved.
                let mut fg_digits = 0usize;
                while fg_digits < 2 && chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                    chars.next();
                    fg_digits += 1;
                }
                // A background is only valid after a foreground, and only when a
                // digit follows the comma (a literal comma is preserved).
                if fg_digits > 0 && chars.peek() == Some(&',') {
                    let mut lookahead = chars.clone();
                    lookahead.next(); // skip the comma in the lookahead copy
                    if lookahead.peek().is_some_and(|c| c.is_ascii_digit()) {
                        chars.next(); // consume the comma for real
                        let mut bg_digits = 0usize;
                        while bg_digits < 2 && chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                            chars.next();
                            bg_digits += 1;
                        }
                    }
                }
            }
            _ => result.push(c),
        }
    }

    result
}

/// Parse IRC color codes and formatting from text
fn parse_irc_colors(input: &str) -> Vec<TextSpan> {
    let mut spans = Vec::new();
    let mut current_text = String::new();
    let mut fg_color: Option<Color32> = None;
    let mut bg_color: Option<Color32> = None;
    let mut bold = false;
    let mut underline = false;
    let mut italic = false;
    let mut reverse = false;

    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\x03' => {
                // Color code - push current span if not empty
                if !current_text.is_empty() {
                    spans.push(TextSpan {
                        text: current_text.clone(),
                        fg_color,
                        bg_color,
                        bold,
                        underline,
                        italic,
                        reverse,
                    });
                    current_text.clear();
                }

                // Parse foreground color (1-2 digits)
                let mut fg_str = String::new();
                while fg_str.len() < 2 {
                    if let Some(&next) = chars.peek() {
                        if next.is_ascii_digit() {
                            fg_str.push(chars.next().unwrap());
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }

                if let Ok(fg) = fg_str.parse::<usize>() {
                    fg_color = irc_color(fg);
                } else {
                    // \x03 with no number resets colors
                    fg_color = None;
                    bg_color = None;
                }

                // Check for background color (comma followed by 1-2 digits). A
                // background is only valid after a foreground (a bare "\x03,NN"
                // is a reset followed by literal text), and the comma is only
                // consumed when a digit actually follows it.
                if !fg_str.is_empty() && chars.peek() == Some(&',') {
                    let mut lookahead = chars.clone();
                    lookahead.next(); // skip the comma in the lookahead copy
                    if lookahead.peek().is_some_and(|c| c.is_ascii_digit()) {
                        chars.next(); // consume the comma for real
                        let mut bg_str = String::new();
                        while bg_str.len() < 2 {
                            if let Some(&next) = chars.peek() {
                                if next.is_ascii_digit() {
                                    bg_str.push(chars.next().unwrap());
                                } else {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }
                        if let Ok(bg) = bg_str.parse::<usize>() {
                            bg_color = irc_color(bg);
                        }
                    }
                }
            }
            '\x02' => {
                // Bold toggle
                if !current_text.is_empty() {
                    spans.push(TextSpan {
                        text: current_text.clone(),
                        fg_color,
                        bg_color,
                        bold,
                        underline,
                        italic,
                        reverse,
                    });
                    current_text.clear();
                }
                bold = !bold;
            }
            '\x1F' => {
                // Underline toggle
                if !current_text.is_empty() {
                    spans.push(TextSpan {
                        text: current_text.clone(),
                        fg_color,
                        bg_color,
                        bold,
                        underline,
                        italic,
                        reverse,
                    });
                    current_text.clear();
                }
                underline = !underline;
            }
            '\x1D' => {
                // Italic toggle
                if !current_text.is_empty() {
                    spans.push(TextSpan {
                        text: current_text.clone(),
                        fg_color,
                        bg_color,
                        bold,
                        underline,
                        italic,
                        reverse,
                    });
                    current_text.clear();
                }
                italic = !italic;
            }
            '\x16' => {
                // Reverse-video toggle
                if !current_text.is_empty() {
                    spans.push(TextSpan {
                        text: current_text.clone(),
                        fg_color,
                        bg_color,
                        bold,
                        underline,
                        italic,
                        reverse,
                    });
                    current_text.clear();
                }
                reverse = !reverse;
            }
            '\x0F' => {
                // Reset all formatting
                if !current_text.is_empty() {
                    spans.push(TextSpan {
                        text: current_text.clone(),
                        fg_color,
                        bg_color,
                        bold,
                        underline,
                        italic,
                        reverse,
                    });
                    current_text.clear();
                }
                fg_color = None;
                bg_color = None;
                bold = false;
                underline = false;
                italic = false;
                reverse = false;
            }
            _ => {
                current_text.push(c);
            }
        }
    }

    // Push remaining text
    if !current_text.is_empty() {
        spans.push(TextSpan {
            text: current_text,
            fg_color,
            bg_color,
            bold,
            underline,
            italic,
            reverse,
        });
    }

    spans
}

/// Split text into URL and non-URL segments
fn split_urls(text: &str) -> Vec<(String, bool)> {
    let mut result = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        // Find the start of a URL. A match only counts at the start of a word
        // (start of text or after whitespace/punctuation) so e.g. the "www." in
        // "awww.so cute" is not turned into a bogus hyperlink.
        let url_starts = ["http://", "https://", "www."];
        let mut earliest_url: Option<(usize, &str)> = None;

        for prefix in &url_starts {
            let mut search_from = 0;
            while let Some(rel) = remaining[search_from..].find(prefix) {
                let pos = search_from + rel;
                let at_word_start = pos == 0
                    || remaining[..pos].chars().next_back().is_some_and(|c| {
                        c.is_whitespace()
                            || matches!(c, '(' | '[' | '<' | '{' | '"' | '\'' | ',' | ';' | ':')
                    });
                if at_word_start {
                    if earliest_url.is_none() || pos < earliest_url.unwrap().0 {
                        earliest_url = Some((pos, prefix));
                    }
                    break;
                }
                search_from = pos + prefix.len();
            }
        }

        match earliest_url {
            Some((pos, _)) => {
                // Add text before the URL
                if pos > 0 {
                    result.push((remaining[..pos].to_string(), false));
                }

                // Find the end of the URL (first whitespace or end of string)
                let url_start = pos;
                let after_url = &remaining[url_start..];
                let url_end = after_url
                    .find(|c: char| {
                        c.is_whitespace()
                            || c == '>'
                            || c == ')'
                            || c == ']'
                            || c == '"'
                            || c == '\''
                    })
                    .unwrap_or(after_url.len());

                let url = &after_url[..url_end];
                result.push((url.to_string(), true));

                remaining = &remaining[url_start + url_end..];
            }
            None => {
                // No more URLs, add the rest as plain text
                result.push((remaining.to_string(), false));
                break;
            }
        }
    }

    result
}

/// A fully-parsed segment ready for rendering: formatting controls resolved
/// and URL detection already done. Cached per message (see
/// `ChatMessage::render_segments`) so the per-frame render path does no
/// parsing and no allocation.
#[derive(Clone, Debug)]
pub struct RenderSegment {
    pub text: String,
    pub fg_color: Option<Color32>,
    pub bg_color: Option<Color32>,
    pub bold: bool,
    pub underline: bool,
    pub italic: bool,
    pub reverse: bool,
    pub is_url: bool,
}

/// Parse IRC formatting and detect URLs once, producing render-ready segments.
pub fn layout_irc_text(text: &str) -> Vec<RenderSegment> {
    let mut segments = Vec::new();
    for span in parse_irc_colors(text) {
        for (segment, is_url) in split_urls(&span.text) {
            segments.push(RenderSegment {
                text: segment,
                fg_color: span.fg_color,
                bg_color: span.bg_color,
                bold: span.bold,
                underline: span.underline,
                italic: span.italic,
                reverse: span.reverse,
                is_url,
            });
        }
    }
    segments
}

/// Render pre-parsed segments (see `layout_irc_text`).
pub fn render_segments(ui: &mut egui::Ui, segments: &[RenderSegment], default_color: Color32) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for span in segments {
            let mut color = span.fg_color.unwrap_or(default_color);
            let mut bg_color = span.bg_color;
            if span.reverse {
                let old_color = color;
                color = bg_color.unwrap_or(Color32::BLACK);
                bg_color = Some(old_color);
            }

            if span.is_url {
                // Render as clickable hyperlink
                let url = if span.text.starts_with("www.") {
                    format!("https://{}", span.text)
                } else {
                    span.text.clone()
                };
                ui.hyperlink_to(
                    RichText::new(&span.text)
                        .color(Color32::from_rgb(100, 150, 255))
                        .underline(),
                    &url,
                );
            } else {
                // Render as regular text with formatting
                // For bold, brighten the color slightly to make it more visible
                let effective_color = if span.bold {
                    let r = (color.r() as u16 + 40).min(255) as u8;
                    let g = (color.g() as u16 + 40).min(255) as u8;
                    let b = (color.b() as u16 + 40).min(255) as u8;
                    Color32::from_rgb(r, g, b)
                } else {
                    color
                };

                let mut rich_text = RichText::new(&span.text).color(effective_color);

                if span.bold {
                    rich_text = rich_text.strong();
                }
                if span.underline {
                    rich_text = rich_text.underline();
                }
                if span.italic {
                    rich_text = rich_text.italics();
                }

                if let Some(bg) = bg_color {
                    rich_text = rich_text.background_color(bg);
                }

                ui.label(rich_text);
            }
        }
    });
}

/// Render IRC formatted text with colors, bold, underline, italic, and clickable URLs.
/// Parses on every call: use only for rarely-rendered or frequently-changing text
/// (message scrollback goes through the per-message cache instead).
pub fn render_irc_text(ui: &mut egui::Ui, text: &str, default_color: Color32) {
    render_segments(ui, &layout_irc_text(text), default_color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_control_is_not_italic() {
        let spans = parse_irc_colors("\x16rev\x16 plain");
        assert_eq!(spans[0].text, "rev");
        assert!(spans[0].reverse);
        assert!(!spans[0].italic);
        assert_eq!(spans[1].text, " plain");
        assert!(!spans[1].reverse);
    }

    #[test]
    fn strip_keeps_digits_beyond_two_per_color() {
        // "\x03042026" = color 04 followed by the literal text "2026".
        assert_eq!(strip_irc_formatting("\x03042026 party"), "2026 party");
        assert_eq!(strip_irc_formatting("\x034,051234"), "1234");
    }

    #[test]
    fn color_99_and_out_of_palette_codes_mean_default() {
        let spans = parse_irc_colors("\x0399text");
        assert_eq!(spans[0].text, "text");
        assert_eq!(spans[0].fg_color, None);

        // Extended palette code 52 is pure red, not `52 % 16`.
        let spans = parse_irc_colors("\x0352warning");
        assert_eq!(spans[0].fg_color, Some(Color32::from_rgb(0xff, 0, 0)));
    }

    #[test]
    fn background_requires_foreground() {
        // "\x03,2 pm" is a color reset followed by the literal text ",2 pm".
        let spans = parse_irc_colors("\x03,2 pm meeting");
        assert_eq!(spans[0].text, ",2 pm meeting");
        assert_eq!(spans[0].bg_color, None);
        // Same for the stripper.
        assert_eq!(strip_irc_formatting("\x03,2 pm"), ",2 pm");
    }

    #[test]
    fn urls_only_match_at_word_boundaries() {
        let segs = layout_irc_text("awww.so cute");
        assert!(segs.iter().all(|s| !s.is_url));

        let segs = layout_irc_text("see www.example.com now");
        let url: Vec<_> = segs.iter().filter(|s| s.is_url).collect();
        assert_eq!(url.len(), 1);
        assert_eq!(url[0].text, "www.example.com");

        let segs = layout_irc_text("(https://rust-lang.org)");
        assert!(
            segs.iter()
                .any(|s| s.is_url && s.text == "https://rust-lang.org")
        );
    }
}
