//! Opening hyperlinks in the system browser.
//!
//! egui's hyperlink widget only queues an [`egui::OutputCommand::OpenUrl`];
//! eframe's native backends never act on it (only its web backend does), so
//! without this module clicks do nothing on desktop platforms. The GUI layer
//! drains those commands each frame and hands the URLs to [`open_in_browser`].

/// Maximum accepted URL length. IRC messages are short and hostile input
/// should not reach the OS shell handler at any size, let alone megabytes.
const MAX_URL_LEN: usize = 2048;

/// Drain links from the viewport currently being drawn. Immediate native
/// viewports must do this before returning their output to eframe.
fn take_pending_urls(ctx: &egui::Context) -> Vec<String> {
    ctx.output_mut(|output| {
        let mut urls = Vec::new();
        output.commands.retain(|command| {
            if let egui::OutputCommand::OpenUrl(open) = command {
                urls.push(open.url.clone());
                false
            } else {
                true
            }
        });
        urls
    })
}

pub(crate) fn dispatch_pending_urls(ctx: &egui::Context) {
    for url in take_pending_urls(ctx) {
        open_in_browser(&url);
    }
}

/// True when `url` is safe to hand to the OS shell handler: an http(s)
/// absolute URL free of whitespace and control characters.
///
/// The link renderer only produces http://, https:// and www. segments, but
/// this is the last gate before shell execution, so it validates rather than
/// trusting its caller. Anything else (other schemes, `javascript:`,
/// embedded control characters) is rejected.
pub fn is_openable_url(url: &str) -> bool {
    let scheme_ok = url.len() >= 8 && {
        let lower = url.to_ascii_lowercase();
        lower.starts_with("https://") || lower.starts_with("http://")
    };
    scheme_ok
        && url.len() <= MAX_URL_LEN
        && !url
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
}

/// Open `url` in the user's default browser. Failures are logged, never
/// panicked on or surfaced as a dialog: a dead browser handler must not take
/// down the client.
pub fn open_in_browser(url: &str) {
    if !is_openable_url(url) {
        tracing::warn!("Refusing to open non-http(s) URL of {} bytes", url.len());
        return;
    }

    let result = spawn_open(url);
    match result {
        Ok(()) => tracing::info!("Opened URL in browser"),
        Err(err) => tracing::error!("Failed to open URL: {err}"),
    }
}

#[cfg(windows)]
fn spawn_open(url: &str) -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};

    // ShellExecuteW dispatches on the registered protocol handler; the verb
    // "open" plus our http(s)-only validation keeps arbitrary program
    // execution out of reach.
    let wide: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    let hinst = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns an instance handle greater than 32 on success;
    // smaller values are SE_ERR_* error codes.
    let code = hinst.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecuteW failed with code {code}"))
    }
}

#[cfg(not(windows))]
fn spawn_open(url: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};

    #[cfg(target_os = "macos")]
    const OPENER: &str = "open";
    #[cfg(not(target_os = "macos"))]
    const OPENER: &str = "xdg-open";

    // Detached from the console so the browser cannot tie up our stdio.
    let mut child = Command::new(OPENER)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{OPENER} failed: {e}"))?;

    // Reap on a throwaway thread: blocking here would stall the UI while the
    // opener runs (xdg-open can wait for the browser), and dropping the Child
    // would leave a zombie behind for the rest of the session.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drains_links_once_and_preserves_other_output_commands() {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(Default::default(), |ui| {
            ui.ctx()
                .open_url(egui::OpenUrl::new_tab("https://example.com/one"));
            ui.ctx().copy_text("copied text".into());
            ui.ctx()
                .open_url(egui::OpenUrl::same_tab("https://example.com/two"));
            assert_eq!(
                take_pending_urls(ui.ctx()),
                ["https://example.com/one", "https://example.com/two"]
            );
            assert!(take_pending_urls(ui.ctx()).is_empty());
        });
        output.textures_delta.clear();
        assert!(matches!(output.platform_output.commands.as_slice(),
            [egui::OutputCommand::CopyText(text)] if text == "copied text"));
    }

    #[test]
    fn child_url_output_is_drained_in_its_own_viewport() {
        use std::cell::Cell;
        use std::rc::Rc;
        let ctx = egui::Context::default();
        ctx.set_embed_viewports(false);
        let saw_child_link = Rc::new(Cell::new(false));
        let saw_link = saw_child_link.clone();
        egui::Context::set_immediate_viewport_renderer(move |ctx, mut viewport| {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    viewport_id: viewport.ids.this,
                    viewports: [
                        (egui::ViewportId::ROOT, egui::ViewportInfo::default()),
                        (
                            viewport.ids.this,
                            egui::ViewportInfo {
                                parent: Some(viewport.ids.parent),
                                ..Default::default()
                            },
                        ),
                    ]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                },
                |ui| (viewport.viewport_ui_cb)(ui),
            );
            output.textures_delta.clear();
            assert!(output.platform_output.commands.is_empty());
            saw_link.set(true);
        });
        let mut output = ctx.run_ui(Default::default(), |ui| {
            ui.ctx()
                .open_url(egui::OpenUrl::new_tab("https://root.example"));
            ui.ctx().show_viewport_immediate(
                egui::ViewportId::from_hash_of("link-test"),
                Default::default(),
                |ui, class| {
                    assert!(class == egui::ViewportClass::Immediate);
                    ui.ctx()
                        .open_url(egui::OpenUrl::new_tab("https://child.example"));
                    assert_eq!(take_pending_urls(ui.ctx()), ["https://child.example"]);
                },
            );
            assert_eq!(take_pending_urls(ui.ctx()), ["https://root.example"]);
        });
        output.textures_delta.clear();
        assert!(saw_child_link.get());
    }

    #[test]
    fn accepts_ordinary_web_urls() {
        assert!(is_openable_url("https://example.com/chat"));
        assert!(is_openable_url("http://example.com"));
        // Longest plausible link still passes.
        let long = format!("https://example.com/{}", "a".repeat(2000));
        assert!(is_openable_url(&long));
    }

    #[test]
    fn rejects_non_http_schemes() {
        for url in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "ftp://example.com/pub",
            "https:/no-double-slash.example",
            "example.com/no-scheme",
            "",
        ] {
            assert!(!is_openable_url(url), "{url} should be rejected");
        }
    }

    #[test]
    fn rejects_whitespace_and_control_characters() {
        assert!(!is_openable_url("https://example.com/a b"));
        assert!(!is_openable_url("https://example.com/a\tb"));
        assert!(!is_openable_url("https://example.com/a\nb"));
        assert!(!is_openable_url("https://example.com/a\u{0}b"));
        // A scheme smuggled behind control bytes is not opened as http(s).
        assert!(!is_openable_url("jav\u{1}ascript:https://x"));
    }

    #[test]
    fn rejects_absurd_lengths() {
        let too_long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        assert!(!is_openable_url(&too_long));
    }
}
