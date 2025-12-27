//! IRC text formatting, color parsing, and rendering

use egui::{Color32, RichText};

/// IRC color codes (mIRC standard)
const IRC_COLORS: [Color32; 16] = [
    Color32::WHITE,                      // 0: White
    Color32::BLACK,                      // 1: Black
    Color32::from_rgb(0, 0, 127),        // 2: Blue (navy)
    Color32::from_rgb(0, 147, 0),        // 3: Green
    Color32::from_rgb(255, 0, 0),        // 4: Red
    Color32::from_rgb(127, 0, 0),        // 5: Brown (maroon)
    Color32::from_rgb(156, 0, 156),      // 6: Purple
    Color32::from_rgb(252, 127, 0),      // 7: Orange
    Color32::from_rgb(255, 255, 0),      // 8: Yellow
    Color32::from_rgb(0, 252, 0),        // 9: Light Green
    Color32::from_rgb(0, 147, 147),      // 10: Cyan (teal)
    Color32::from_rgb(0, 255, 255),      // 11: Light Cyan
    Color32::from_rgb(0, 0, 252),        // 12: Light Blue
    Color32::from_rgb(255, 0, 255),      // 13: Pink
    Color32::from_rgb(127, 127, 127),    // 14: Grey
    Color32::from_rgb(210, 210, 210),    // 15: Light Grey
];

/// Curated palette of distinct, readable colors for nick coloring
const NICK_COLORS: [Color32; 16] = [
    Color32::from_rgb(255, 100, 100),  // Light red
    Color32::from_rgb(100, 255, 100),  // Light green
    Color32::from_rgb(100, 200, 255),  // Light blue
    Color32::from_rgb(255, 200, 100),  // Orange/gold
    Color32::from_rgb(255, 100, 255),  // Pink/magenta
    Color32::from_rgb(100, 255, 255),  // Cyan
    Color32::from_rgb(255, 255, 100),  // Yellow
    Color32::from_rgb(200, 150, 255),  // Light purple
    Color32::from_rgb(255, 150, 150),  // Salmon
    Color32::from_rgb(150, 255, 200),  // Mint
    Color32::from_rgb(150, 200, 255),  // Sky blue
    Color32::from_rgb(255, 200, 150),  // Peach
    Color32::from_rgb(200, 255, 150),  // Lime
    Color32::from_rgb(255, 150, 200),  // Rose
    Color32::from_rgb(150, 255, 255),  // Aqua
    Color32::from_rgb(255, 220, 180),  // Tan
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
}

/// Get a consistent color for a nick based on hash
pub fn nick_color(nick: &str) -> Color32 {
    let hash: u32 = nick.bytes().fold(0, |acc, b| acc.wrapping_add(b as u32).wrapping_mul(31));
    NICK_COLORS[(hash as usize) % NICK_COLORS.len()]
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
                    fg_color = Some(IRC_COLORS[fg % 16]);
                } else {
                    // \x03 with no number resets colors
                    fg_color = None;
                    bg_color = None;
                }

                // Check for background color (comma followed by 1-2 digits)
                if chars.peek() == Some(&',') {
                    chars.next(); // consume comma
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
                        bg_color = Some(IRC_COLORS[bg % 16]);
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
                    });
                    current_text.clear();
                }
                underline = !underline;
            }
            '\x1D' | '\x16' => {
                // Italic toggle (\x1D is proper italic, \x16 is reverse/italic)
                if !current_text.is_empty() {
                    spans.push(TextSpan {
                        text: current_text.clone(),
                        fg_color,
                        bg_color,
                        bold,
                        underline,
                        italic,
                    });
                    current_text.clear();
                }
                italic = !italic;
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
                    });
                    current_text.clear();
                }
                fg_color = None;
                bg_color = None;
                bold = false;
                underline = false;
                italic = false;
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
        });
    }

    spans
}

/// Split text into URL and non-URL segments
fn split_urls(text: &str) -> Vec<(String, bool)> {
    let mut result = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        // Find the start of a URL
        let url_starts = ["http://", "https://", "www."];
        let mut earliest_url: Option<(usize, &str)> = None;

        for prefix in &url_starts {
            if let Some(pos) = remaining.find(prefix) {
                if earliest_url.is_none() || pos < earliest_url.unwrap().0 {
                    earliest_url = Some((pos, prefix));
                }
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
                    .find(|c: char| c.is_whitespace() || c == '>' || c == ')' || c == ']' || c == '"' || c == '\'')
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

/// Render IRC formatted text with colors, bold, underline, italic, and clickable URLs
pub fn render_irc_text(ui: &mut egui::Ui, text: &str, default_color: Color32) {
    let spans = parse_irc_colors(text);

    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for span in spans {
            let color = span.fg_color.unwrap_or(default_color);

            // Split the span text into URL and non-URL parts
            let segments = split_urls(&span.text);

            for (segment, is_url) in segments {
                if is_url {
                    // Render as clickable hyperlink
                    let url = if segment.starts_with("www.") {
                        format!("https://{}", segment)
                    } else {
                        segment.clone()
                    };
                    ui.hyperlink_to(
                        RichText::new(&segment).color(Color32::from_rgb(100, 150, 255)).underline(),
                        &url
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

                    let mut rich_text = RichText::new(&segment).color(effective_color);

                    if span.bold {
                        rich_text = rich_text.strong();
                    }
                    if span.underline {
                        rich_text = rich_text.underline();
                    }
                    if span.italic {
                        rich_text = rich_text.italics();
                    }

                    if let Some(bg) = span.bg_color {
                        rich_text = rich_text.background_color(bg);
                    }

                    ui.label(rich_text);
                }
            }
        }
    });
}
