//! Opening hyperlinks in the system browser.
//!
//! egui's hyperlink widget only queues an [`egui::OutputCommand::OpenUrl`];
//! eframe's native backends never act on it (only its web backend does), so
//! without this module clicks do nothing on desktop platforms. The GUI layer
//! drains those commands each frame and hands the URLs to [`open_in_browser`].

/// Maximum accepted URL length. IRC messages are short and hostile input
/// should not reach the OS shell handler at any size, let alone megabytes.
const MAX_URL_LEN: usize = 2048;

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
        && !url.bytes().any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
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
    use windows::core::{PCWSTR, w};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

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
