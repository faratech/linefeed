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
    strikethrough: bool,
    monospace: bool,
    /// A reset (`\x0F`) came between the previous span and this one.
    after_reset: bool,
}

/// Get a consistent color for a nick based on hash
pub fn nick_color(nick: &str) -> Color32 {
    let hash: u32 = nick
        .bytes()
        .fold(0, |acc, b| acc.wrapping_add(b as u32).wrapping_mul(31));
    NICK_COLORS[(hash as usize) % NICK_COLORS.len()]
}

/// Consume exactly six hex digits (`RRGGBB`) from the front of `chars`. When
/// fewer follow, nothing is consumed and the digits stay literal text.
fn take_hex_color(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<Color32> {
    let mut lookahead = chars.clone();
    let mut rgb = 0u32;
    for _ in 0..6 {
        rgb = rgb << 4 | lookahead.next()?.to_digit(16)?;
    }
    *chars = lookahead;
    Some(Color32::from_rgb(
        (rgb >> 16) as u8,
        (rgb >> 8) as u8,
        rgb as u8,
    ))
}

/// Consume the `[RRGGBB[,RRGGBB]]` that follows a `\x04` hex-color control.
/// A background is only valid after a foreground, and the comma is only
/// consumed when a full background follows it (as for `\x03`).
fn take_hex_colors(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> (Option<Color32>, Option<Color32>) {
    let Some(fg) = take_hex_color(chars) else {
        return (None, None);
    };
    let mut lookahead = chars.clone();
    if lookahead.next() == Some(',')
        && let Some(bg) = take_hex_color(&mut lookahead)
    {
        *chars = lookahead;
        return (Some(fg), Some(bg));
    }
    (Some(fg), None)
}

/// Strip IRC color codes and formatting from text, returning plain text
pub fn strip_irc_formatting(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\x02' | '\x1D' | '\x1F' | '\x16' | '\x0F' | '\x1E' | '\x11' => {
                // Bold, italic, underline, reverse, reset, strikethrough,
                // monospace - skip
            }
            '\x04' => {
                // Hex color: \x04[RRGGBB[,RRGGBB]]
                take_hex_colors(&mut chars);
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
    let mut strikethrough = false;
    let mut monospace = false;
    // `reset_seen`: a \x0F came since the last flushed span. A span records
    // it when its first character arrives (`span_after_reset`).
    let mut reset_seen = false;
    let mut span_after_reset = false;

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
                        strikethrough,
                        monospace,
                        after_reset: span_after_reset,
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
                        strikethrough,
                        monospace,
                        after_reset: span_after_reset,
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
                        strikethrough,
                        monospace,
                        after_reset: span_after_reset,
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
                        strikethrough,
                        monospace,
                        after_reset: span_after_reset,
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
                        strikethrough,
                        monospace,
                        after_reset: span_after_reset,
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
                        strikethrough,
                        monospace,
                        after_reset: span_after_reset,
                    });
                    current_text.clear();
                }
                fg_color = None;
                bg_color = None;
                bold = false;
                underline = false;
                italic = false;
                reverse = false;
                strikethrough = false;
                monospace = false;
                reset_seen = true;
            }
            '\x1E' | '\x11' | '\x04' => {
                // Strikethrough / monospace toggles and the hex colour
                if !current_text.is_empty() {
                    spans.push(TextSpan {
                        text: current_text.clone(),
                        fg_color,
                        bg_color,
                        bold,
                        underline,
                        italic,
                        reverse,
                        strikethrough,
                        monospace,
                        after_reset: span_after_reset,
                    });
                    current_text.clear();
                }
                match c {
                    '\x1E' => strikethrough = !strikethrough,
                    '\x11' => monospace = !monospace,
                    _ => match take_hex_colors(&mut chars) {
                        (Some(fg), bg) => {
                            fg_color = Some(fg);
                            if bg.is_some() {
                                bg_color = bg;
                            }
                        }
                        // A bare \x04 resets colors, like a bare \x03.
                        (None, _) => {
                            fg_color = None;
                            bg_color = None;
                        }
                    },
                }
            }
            _ => {
                if current_text.is_empty() {
                    span_after_reset = std::mem::take(&mut reset_seen);
                }
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
            strikethrough,
            monospace,
            after_reset: span_after_reset,
        });
    }

    spans
}

/// Characters that end a URL: whitespace or common surrounding punctuation.
/// Closing brackets are handled separately (see `url_length`): they end the
/// URL only when no opener inside it balances them.
fn url_terminates(c: char) -> bool {
    c.is_whitespace() || matches!(c, '>' | '"' | '\'')
}

/// Sentence punctuation that is not part of a URL when it comes last
/// ("see https://example.com/page, thanks").
fn url_trailing_punctuation(c: char) -> bool {
    matches!(c, ',' | '.' | '!' | '?' | ';' | ':')
}

/// Where one formatting span ends and the next begins in the concatenated
/// visible text of a message.
struct SpanBreak {
    /// Byte offset in the visible text at which the next span starts.
    at: usize,
    /// A reset (`\x0F`) separates the two spans.
    reset: bool,
}

/// Lower-case ASCII only: URL schemes and `www.` host labels are
/// case-insensitive (RFC 3986), so they are matched with the helpers below.
const URL_PREFIXES: [&str; 3] = ["http://", "https://", "www."];

fn starts_with_ignore_ascii_case(text: &str, prefix: &str) -> bool {
    text.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// Byte offset of the first case-insensitive occurrence of the ASCII
/// `prefix`. An ASCII match never starts or ends inside a multi-byte
/// character, so the offset is always a valid `str` index.
fn find_ignore_ascii_case(text: &str, prefix: &str) -> Option<usize> {
    text.as_bytes()
        .windows(prefix.len())
        .position(|window| window.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// A character that may continue a URL across a formatting control. Stricter
/// than what a plain run of text may hold (`url_length`): a control is no
/// licence to glue arbitrary punctuation such as `|` onto a link.
fn url_plain_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | ':'
                | '/'
                | '?'
                | '#'
                | '['
                | ']'
                | '@'
                | '!'
                | '$'
                | '&'
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | '%'
        )
}

/// Whether the URL being scanned ends at the span break `brk`, where `rest`
/// is the visible text from the break on. A formatting control may cut a
/// URL mid-word ("\x02https://ex\x02ample.com" is one link), but not after a
/// reset, before a character a URL cannot hold, or before a new link prefix.
fn break_ends_url(rest: &str, brk: &SpanBreak) -> bool {
    brk.reset
        || !rest.chars().next().is_some_and(url_plain_char)
        || URL_PREFIXES
            .iter()
            .any(|prefix| starts_with_ignore_ascii_case(rest, prefix))
}

/// Byte length of the URL at the start of `text` (which begins at offset
/// `base` of the visible text that `breaks` index). It runs to the first
/// terminator or unmatched closing bracket, so a Wikipedia-style
/// `/Rust_(programming_language)` keeps its parentheses while the `)` of
/// "(see https://example.com)" is left out, and loses trailing punctuation.
fn url_length(text: &str, base: usize, breaks: &[SpanBreak]) -> usize {
    let (mut parens, mut squares) = (0usize, 0usize);
    let mut end = text.len();
    // `text` starts at offset `base` of the visible text the breaks refer to.
    let mut next_break = breaks.partition_point(|brk| brk.at <= base);
    for (index, c) in text.char_indices() {
        if let Some(brk) = breaks.get(next_break)
            && brk.at == base + index
        {
            next_break += 1;
            if break_ends_url(&text[index..], brk) {
                end = index;
                break;
            }
        }
        match c {
            '(' => parens += 1,
            '[' => squares += 1,
            ')' if parens > 0 => parens -= 1,
            ']' if squares > 0 => squares -= 1,
            ')' | ']' => {
                end = index;
                break;
            }
            _ if url_terminates(c) => {
                end = index;
                break;
            }
            _ => {}
        }
    }
    text[..end].trim_end_matches(url_trailing_punctuation).len()
}

/// Byte ranges of the URLs in `text`, in order. `breaks` are the span
/// boundaries inside `text` (see `SpanBreak`).
fn find_urls(text: &str, breaks: &[SpanBreak]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut from = 0;

    while from < text.len() {
        // Find the start of a URL. A match only counts at the start of a word
        // (start of text, a formatting control, or after whitespace/punctuation)
        // so e.g. the "www." in "awww.so cute" is not turned into a bogus
        // hyperlink.
        let mut earliest_url: Option<(usize, usize)> = None;

        for prefix in &URL_PREFIXES {
            let mut search_from = from;
            while let Some(rel) = find_ignore_ascii_case(&text[search_from..], prefix) {
                let pos = search_from + rel;
                let at_word_start = pos == 0
                    || breaks.binary_search_by_key(&pos, |brk| brk.at).is_ok()
                    || text[..pos].chars().next_back().is_some_and(|c| {
                        c.is_whitespace()
                            || matches!(c, '(' | '[' | '<' | '{' | '"' | '\'' | ',' | ';' | ':')
                    });
                if at_word_start {
                    if earliest_url.is_none_or(|(earliest, _)| pos < earliest) {
                        earliest_url = Some((pos, prefix.len()));
                    }
                    break;
                }
                search_from = pos + prefix.len();
            }
        }

        let Some((start, prefix_len)) = earliest_url else {
            break;
        };
        let end = start + url_length(&text[start..], start, breaks);
        if end > start + prefix_len {
            ranges.push((start, end));
            from = end;
        } else {
            // Nothing but the prefix (and punctuation) after it: not a link.
            from = start + prefix_len;
        }
    }

    ranges
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
    pub strikethrough: bool,
    pub monospace: bool,
    pub is_url: bool,
}

fn render_segment(span: &TextSpan, text: String, is_url: bool) -> RenderSegment {
    RenderSegment {
        text,
        fg_color: span.fg_color,
        bg_color: span.bg_color,
        bold: span.bold,
        underline: span.underline,
        italic: span.italic,
        reverse: span.reverse,
        strikethrough: span.strikethrough,
        monospace: span.monospace,
        is_url,
    }
}

/// Parse IRC formatting and detect URLs once, producing render-ready segments.
pub fn layout_irc_text(text: &str) -> Vec<RenderSegment> {
    let spans = parse_irc_colors(text);
    // URLs are found in the visible text of the whole message. A formatting
    // control can cut a URL mid-domain ("\x02https://ex\x02ample.com"): the
    // visible characters continue into the next span with no separator on the
    // wire, so the pieces are glued onto the still-open hyperlink instead of
    // rendering a truncated link to the wrong host. A control only glues when
    // it plausibly sits inside one word: see `break_ends_url`.
    let visible: String = spans.iter().map(|span| span.text.as_str()).collect();
    let mut breaks = Vec::with_capacity(spans.len().saturating_sub(1));
    let mut offset = 0;
    for (index, span) in spans.iter().enumerate() {
        if index > 0 {
            breaks.push(SpanBreak {
                at: offset,
                reset: span.after_reset,
            });
        }
        offset += span.text.len();
    }
    let urls = find_urls(&visible, &breaks);

    let mut segments: Vec<RenderSegment> = Vec::new();
    let mut url_open = false;
    let mut next_url = 0;
    let mut span_start = 0;
    for span in &spans {
        let span_end = span_start + span.text.len();
        let mut at = span_start;
        while at < span_end {
            while next_url < urls.len() && urls[next_url].1 <= at {
                next_url += 1;
            }
            match urls.get(next_url) {
                Some(&(start, end)) if start <= at => {
                    let piece_end = end.min(span_end);
                    let piece = &visible[at..piece_end];
                    if url_open {
                        segments
                            .last_mut()
                            .expect("url_open implies a previous URL segment")
                            .text
                            .push_str(piece);
                    } else {
                        segments.push(render_segment(span, piece.to_string(), true));
                    }
                    // The URL runs on into the next span when a control cut it.
                    url_open = piece_end < end;
                    at = piece_end;
                }
                next => {
                    let piece_end = next.map_or(span_end, |&(start, _)| start.min(span_end));
                    segments.push(render_segment(
                        span,
                        visible[at..piece_end].to_string(),
                        false,
                    ));
                    at = piece_end;
                }
            }
        }
        span_start = span_end;
    }
    segments
}

/// The address a link segment opens: a bare `www.` host gets an https scheme.
fn link_target(text: &str) -> String {
    if starts_with_ignore_ascii_case(text, "www.") {
        format!("https://{text}")
    } else {
        text.to_string()
    }
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
                let url = link_target(&span.text);
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
                if span.strikethrough {
                    rich_text = rich_text.strikethrough();
                }
                if span.monospace {
                    rich_text = rich_text.monospace();
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
    fn hex_color_monospace_and_strikethrough_are_parsed_not_shown() {
        // \x04RRGGBB[,RRGGBB] sets an exact foreground / background (#264).
        let spans = parse_irc_colors("\x04FF0000red\x04 plain");
        assert_eq!(spans[0].text, "red");
        assert_eq!(spans[0].fg_color, Some(Color32::from_rgb(0xff, 0, 0)));
        assert_eq!(spans[1].text, " plain");
        assert_eq!(spans[1].fg_color, None);

        let spans = parse_irc_colors("\x04ff8000,0000FFboth\x04,1 tail");
        assert_eq!(spans[0].text, "both");
        assert_eq!(spans[0].fg_color, Some(Color32::from_rgb(0xff, 0x80, 0)));
        assert_eq!(spans[0].bg_color, Some(Color32::from_rgb(0, 0, 0xff)));
        // A bare \x04 resets both; the comma and digit stay literal.
        assert_eq!(spans[1].text, ",1 tail");
        assert_eq!((spans[1].fg_color, spans[1].bg_color), (None, None));

        // Fewer than six hex digits is a reset followed by literal text.
        let spans = parse_irc_colors("\x04FF00 not a color");
        assert_eq!(spans[0].text, "FF00 not a color");
        assert_eq!(spans[0].fg_color, None);
        // A background needs all six digits too.
        let spans = parse_irc_colors("\x04112233,4455 x");
        assert_eq!(spans[0].text, ",4455 x");
        assert_eq!(spans[0].fg_color, Some(Color32::from_rgb(0x11, 0x22, 0x33)));
        assert_eq!(spans[0].bg_color, None);

        let spans = parse_irc_colors("\x11code\x11 \x1Estruck\x1E");
        assert_eq!(spans[0].text, "code");
        assert!(spans[0].monospace && !spans[0].strikethrough);
        assert_eq!(spans[1].text, " ");
        assert!(!spans[1].monospace && !spans[1].strikethrough);
        assert_eq!(spans[2].text, "struck");
        assert!(spans[2].strikethrough && !spans[2].monospace);

        // Reset clears both new attributes along with the old ones.
        let spans = parse_irc_colors("\x11\x1Ex\x0Fy");
        assert!(spans[0].monospace && spans[0].strikethrough);
        assert_eq!(spans[1].text, "y");
        assert!(!spans[1].monospace && !spans[1].strikethrough);

        // The flags reach the render segments, and no control char is left.
        let segs = layout_irc_text("\x04FF0000\x1Edone\x1E");
        assert_eq!(segs.len(), 1);
        assert!(segs[0].strikethrough);
        assert_eq!(segs[0].text, "done");
        assert!(segs.iter().all(|seg| !seg.text.contains(char::is_control)));
    }

    #[test]
    fn strip_removes_hex_color_monospace_and_strikethrough() {
        assert_eq!(strip_irc_formatting("\x04FF0000red\x04"), "red");
        assert_eq!(strip_irc_formatting("\x04FF0000,00ff00bg\x04 ok"), "bg ok");
        assert_eq!(
            strip_irc_formatting("\x1Estruck\x1E \x11mono\x11"),
            "struck mono"
        );
        // Not six hex digits: only the control goes, the text stays.
        assert_eq!(strip_irc_formatting("\x04FF00 text"), "FF00 text");
        assert_eq!(strip_irc_formatting("\x04112233,4455 x"), ",4455 x");
        // Digits beyond the six belong to the text.
        assert_eq!(strip_irc_formatting("\x04FF000012"), "12");
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

    fn urls_in(text: &str) -> Vec<String> {
        layout_irc_text(text)
            .into_iter()
            .filter(|segment| segment.is_url)
            .map(|segment| segment.text)
            .collect()
    }

    #[test]
    fn urls_drop_trailing_punctuation_and_unmatched_brackets() {
        for (text, expected) in [
            (
                "see https://example.com/page, thanks",
                "https://example.com/page",
            ),
            (
                "go to https://example.com/page.",
                "https://example.com/page",
            ),
            ("really https://example.com/a?!", "https://example.com/a"),
            ("a: https://example.com/a; b", "https://example.com/a"),
            ("or www.example.com.", "www.example.com"),
            ("(see https://example.com/a)", "https://example.com/a"),
            ("(see https://example.com/a).", "https://example.com/a"),
            ("[https://example.com/a]", "https://example.com/a"),
            ("<https://example.com/a>", "https://example.com/a"),
            // Balanced brackets belong to the URL.
            (
                "https://en.wikipedia.org/wiki/Rust_(programming_language)",
                "https://en.wikipedia.org/wiki/Rust_(programming_language)",
            ),
            (
                "see https://en.wikipedia.org/wiki/Rust_(language), ok",
                "https://en.wikipedia.org/wiki/Rust_(language)",
            ),
            (
                "(https://en.wikipedia.org/wiki/Rust_(language))",
                "https://en.wikipedia.org/wiki/Rust_(language)",
            ),
            ("http://[::1]:8080/x.", "http://[::1]:8080/x"),
        ] {
            assert_eq!(urls_in(text), [expected], "{text}");
        }
        // The dropped characters stay as plain text.
        let segments = layout_irc_text("see https://example.com/page, thanks");
        let plain: String = segments
            .iter()
            .filter(|segment| !segment.is_url)
            .map(|segment| segment.text.as_str())
            .collect();
        assert_eq!(plain, "see , thanks");

        assert_eq!(
            urls_in("a https://x.test, b https://y.test."),
            ["https://x.test", "https://y.test"]
        );
        // A bare prefix is not a link.
        assert!(urls_in("http:// or www. or https://.").is_empty());
    }

    #[test]
    fn trimmed_punctuation_applies_to_urls_cut_by_formatting_controls_too() {
        // The control splits the domain; the sentence dot after the whole URL
        // is still dropped, while a dot inside it is kept.
        assert_eq!(
            urls_in("\u{2}https://exam\u{2}ple.com/x."),
            ["https://example.com/x"]
        );
        assert_eq!(
            urls_in("\u{2}https://exam\u{2}.\u{2}com\u{2}, then"),
            ["https://exam.com"]
        );
        // Balanced brackets count across the cut.
        assert_eq!(
            urls_in("see https://a.test/x_(\u{2}y\u{2}) now"),
            ["https://a.test/x_(y)"]
        );
    }

    #[test]
    fn urls_cut_by_formatting_controls_stay_whole() {
        // Bold wrapped around the middle of a domain: the fragments must glue
        // into one link to the full host, not a truncated wrong-host link.
        let segs = layout_irc_text("\u{2}https://exam\u{2}ple.com/x");
        let urls: Vec<_> = segs.iter().filter(|s| s.is_url).collect();
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].text, "https://example.com/x");

        // A URL that ends before the control must not swallow the next word.
        let segs = layout_irc_text("\u{2}https://a.test\u{2} plain words");
        assert!(
            !segs
                .iter()
                .any(|s| s.is_url && s.text.contains("plain"))
        );
        assert!(segs.iter().any(|s| s.text == " plain words"));

        // Color control inside the URL path.
        let segs = layout_irc_text("see \u{3}12https://a\u{3}b.test/c now");
        let url = segs.iter().find(|s| s.is_url).unwrap();
        assert_eq!(url.text, "https://ab.test/c");
    }

    fn plain_text_in(text: &str) -> String {
        layout_irc_text(text)
            .iter()
            .filter(|segment| !segment.is_url)
            .map(|segment| segment.text.as_str())
            .collect()
    }

    #[test]
    fn a_reset_or_a_non_url_character_ends_a_url_at_a_control() {
        // \x0F is a reset: the next visible text is a new word.
        assert_eq!(urls_in("https://a.test\x0F|more"), ["https://a.test"]);
        assert_eq!(plain_text_in("https://a.test\x0F|more"), "|more");
        assert_eq!(urls_in("https://a.test\x0Fmore"), ["https://a.test"]);
        assert_eq!(plain_text_in("https://a.test\x0Fmore"), "more");
        // A character a URL cannot hold ends it even after a plain toggle.
        assert_eq!(urls_in("\x02https://a.test\x02|more"), ["https://a.test"]);
        assert_eq!(plain_text_in("\x02https://a.test\x02|more"), "|more");
        // Mid-word cuts still glue (the #265 behaviour).
        assert_eq!(
            urls_in("https://a.test/x\x02y\x02z"),
            ["https://a.test/xyz"]
        );
    }

    #[test]
    fn a_piece_starting_with_a_url_prefix_begins_a_new_link() {
        assert_eq!(
            urls_in("\x02https://a.test\x02\x1Fhttps://b.test"),
            ["https://a.test", "https://b.test"]
        );
        assert_eq!(
            urls_in("\x02http://a.test\x02www.b.test"),
            ["http://a.test", "www.b.test"]
        );
        // Without a control between them nothing changes.
        assert_eq!(
            urls_in("https://a.testhttps://b.test"),
            ["https://a.testhttps://b.test"]
        );
    }

    #[test]
    fn url_schemes_and_www_prefixes_match_in_any_letter_case() {
        // RFC 3986 schemes and host names are case-insensitive (#294).
        for (text, expected) in [
            ("HTTPS://EXAMPLE.COM/path", "HTTPS://EXAMPLE.COM/path"),
            ("Https://example.com", "Https://example.com"),
            ("see HTTP://Example.com/x now", "HTTP://Example.com/x"),
            ("WWW.EXAMPLE.COM", "WWW.EXAMPLE.COM"),
            ("go to Www.Example.com.", "Www.Example.com"),
            ("(HTTPS://EXAMPLE.COM)", "HTTPS://EXAMPLE.COM"),
        ] {
            assert_eq!(urls_in(text), [expected], "{text}");
        }
        // The word-boundary rule still applies to every case.
        assert!(urls_in("aWWW.so cute").is_empty());
        assert!(urls_in("SeeHTTPS://a.test").is_empty());
        // A bare prefix is still not a link.
        assert!(urls_in("HTTPS:// or WWW.").is_empty());
        // A control before a new upper-case prefix still starts a new link.
        assert_eq!(
            urls_in("\x02https://a.test\x02\x1FHTTPS://b.test"),
            ["https://a.test", "HTTPS://b.test"]
        );
        // Non-ASCII text next to a match does not disturb the byte offsets.
        assert_eq!(urls_in("é HTTPS://É.test ü"), ["HTTPS://É.test"]);
    }

    #[test]
    fn bare_www_links_open_with_an_https_scheme_in_any_letter_case() {
        assert_eq!(link_target("www.example.com"), "https://www.example.com");
        assert_eq!(link_target("WWW.EXAMPLE.COM"), "https://WWW.EXAMPLE.COM");
        assert_eq!(link_target("Www.example.com"), "https://Www.example.com");
        assert_eq!(link_target("HTTPS://EXAMPLE.COM"), "HTTPS://EXAMPLE.COM");
        assert_eq!(link_target("http://a.test"), "http://a.test");
    }

    #[test]
    fn a_control_right_before_a_url_is_a_word_boundary() {
        assert_eq!(urls_in("See\x02https://a.test"), ["https://a.test"]);
        assert_eq!(plain_text_in("See\x02https://a.test"), "See");
        assert_eq!(urls_in("See\x0312www.a.test now"), ["www.a.test"]);
        // No control, no boundary: "awww.so" is still not a link.
        assert!(urls_in("awww.so cute").is_empty());
        assert!(urls_in("Seehttps://a.test").is_empty());
    }

    #[test]
    fn strip_and_layout_preserve_the_visible_text() {
        // Deterministic pseudo-random formatting soup: the stripped text must
        // always equal the concatenation of the parsed spans' visible text.
        let alphabet: Vec<char> = vec![
            'a', 'b', '1', '2', ',', ' ', '\x03', '\x04', '\x02', '\x0F', '\x1D', '\x1F', '\x16',
            '\x1E', '\x11', 'F', 'x',
        ];
        let mut state: u64 = 0x1234_5678_9abc_def0;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for case in 0..20000 {
            let len = (next() % 24) as usize;
            let input: String = (0..len)
                .map(|_| alphabet[(next() as usize) % alphabet.len()])
                .collect();
            let stripped = strip_irc_formatting(&input);
            let visible: String = parse_irc_colors(&input)
                .iter()
                .map(|span| span.text.as_str())
                .collect();
            assert_eq!(stripped, visible, "case {case} input {input:?}");
            // Layout must preserve every visible character exactly once.
            let laid: String = layout_irc_text(&input)
                .iter()
                .map(|segment| segment.text.as_str())
                .collect();
            assert_eq!(laid, visible, "layout case {case} input {input:?}");
        }
    }
}

