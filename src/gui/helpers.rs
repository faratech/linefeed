//! Helper utilities for the GUI module

/// Check if a name is an IRC channel (starts with # or &)
pub fn is_channel(name: &str) -> bool {
    name.starts_with('#') || name.starts_with('&')
}

/// Convert a nick or partial mask into a complete `nick!user@host` ban mask.
/// A full mask (a '!' followed later by an '@') is returned unchanged; partial
/// forms like "nick", "nick@host", or "nick!user" are completed with `*`
/// wildcards so the result is always a valid mask.
pub fn nick_to_mask(arg: &str) -> String {
    fn or_star(s: &str) -> &str {
        if s.is_empty() { "*" } else { s }
    }
    // Already a full nick!user@host mask? Pass it through.
    if let Some(bang) = arg.find('!') {
        if arg[bang + 1..].contains('@') {
            return arg.to_string();
        }
    }
    // Split off an optional host, then an optional user, defaulting the rest to '*'.
    let (nick_user, host) = match arg.split_once('@') {
        Some((lhs, rhs)) => (lhs, rhs),
        None => (arg, "*"),
    };
    let (nick, user) = match nick_user.split_once('!') {
        Some((n, u)) => (n, u),
        None => (nick_user, "*"),
    };
    format!("{}!{}@{}", or_star(nick), or_star(user), or_star(host))
}

/// Get current local time formatted according to preference
/// "short" = [HH:MM], "long" = [HH:MM:SS], "full" = [MM-DD HH:MM]
pub fn current_time_formatted(format: &str) -> String {
    #[cfg(unix)]
    let (month, day, hours, minutes, seconds) = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let t = secs as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let res = unsafe { libc::localtime_r(&t, &mut tm) };
        if res.is_null() {
            // localtime_r failed; fall back to a UTC decomposition rather than
            // formatting the zeroed tm (which would render as 1900/epoch garbage).
            let days = secs / 86400;
            let (_, m, d) = days_to_ymd(days);
            (m as u64, d as u64, (secs % 86400) / 3600, (secs % 3600) / 60, secs % 60)
        } else {
            ((tm.tm_mon + 1) as u64, tm.tm_mday as u64, tm.tm_hour as u64, tm.tm_min as u64, tm.tm_sec as u64)
        }
    };

    #[cfg(windows)]
    let (month, day, hours, minutes, seconds) = {
        use std::mem::MaybeUninit;
        #[repr(C)]
        struct SYSTEMTIME {
            year: u16, month: u16, day_of_week: u16, day: u16,
            hour: u16, minute: u16, second: u16, milliseconds: u16,
        }
        unsafe extern "system" {
            fn GetLocalTime(lpSystemTime: *mut SYSTEMTIME);
        }
        let mut st = MaybeUninit::<SYSTEMTIME>::uninit();
        unsafe {
            GetLocalTime(st.as_mut_ptr());
            let st = st.assume_init();
            (st.month as u64, st.day as u64, st.hour as u64, st.minute as u64, st.second as u64)
        }
    };

    #[cfg(not(any(unix, windows)))]
    let (month, day, hours, minutes, seconds) = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Fallback to UTC - approximate month/day
        let days = secs / 86400;
        let (_, m, d) = days_to_ymd(days);
        let hours = (secs % 86400) / 3600;
        let minutes = (secs % 3600) / 60;
        let seconds = secs % 60;
        (m as u64, d as u64, hours, minutes, seconds)
    };

    match format {
        "long" => format!("[{:02}:{:02}:{:02}]", hours, minutes, seconds),
        "full" => format!("[{:02}-{:02} {:02}:{:02}]", month, day, hours, minutes),
        _ => format!("[{:02}:{:02}]", hours, minutes), // "short" or default
    }
}

/// Format a Unix timestamp as a human-readable string
pub fn format_timestamp(ts: u64) -> String {
    if ts == 0 {
        return "Unknown".to_string();
    }
    let days = ts / 86400;
    let time_secs = ts % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;

    let (year, month, day) = days_to_ymd(days);
    let month_name = match month {
        1 => "Jan", 2 => "Feb", 3 => "Mar", 4 => "Apr",
        5 => "May", 6 => "Jun", 7 => "Jul", 8 => "Aug",
        9 => "Sep", 10 => "Oct", 11 => "Nov", 12 => "Dec",
        _ => "???",
    };
    format!("{} {}, {} {:02}:{:02} UTC", month_name, day, year, hours, minutes)
}

/// Convert days since Unix epoch (1970-01-01) to (year, month, day)
pub fn days_to_ymd(days: u64) -> (u32, u32, u32) {
    // Algorithm based on Howard Hinnant's date algorithms
    // http://howardhinnant.github.io/date_algorithms.html
    let z = days as i64 + 719468; // days from year 0
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // year of era [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365]
    let mp = (5 * doy + 2) / 153; // month [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // day [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // month [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as u32, m, d)
}

/// Truncate a string to N characters efficiently, adding "..." if truncated
/// Uses char_indices to avoid collecting into a Vec
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}...", &s[..idx]),
        None => s.to_string(),
    }
}

/// Simple wildcard mask matching for ignore list.
/// Supports `*` (any run of chars) and `?` (single char). Iterative two-pointer
/// matcher with backtracking on the last `*` — no per-position allocation or
/// recursion (the previous version cloned the remaining text and recursed at
/// every position, which was quadratic/super-linear on adversarial input).
pub fn mask_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None; // pattern index just after the last '*'
    let mut star_ti = 0usize;           // text index when that '*' was taken

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(s) = star {
            // Backtrack: let the last '*' absorb one more text char.
            pi = s + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    // Any pattern remainder must be all '*' to match the now-exhausted text.
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nick_to_mask_completes_partials() {
        assert_eq!(nick_to_mask("nick"), "nick!*@*");
        assert_eq!(nick_to_mask("nick!user@host"), "nick!user@host");
        assert_eq!(nick_to_mask("nick@host"), "nick!*@host");
        assert_eq!(nick_to_mask("nick!user"), "nick!user@*");
        assert_eq!(nick_to_mask("*!*@*"), "*!*@*");
        assert_eq!(nick_to_mask("*@host"), "*!*@host");
    }

    #[test]
    fn mask_matches_globs() {
        assert!(mask_matches("*", "anything"));
        assert!(mask_matches("a*c", "abc"));
        assert!(mask_matches("a*c", "ac"));
        assert!(mask_matches("a?c", "abc"));
        assert!(!mask_matches("a?c", "ac"));
        assert!(mask_matches("nick!*@*", "nick!user@host"));
        assert!(!mask_matches("nick!*@*", "other!user@host"));
        assert!(mask_matches("*@*.example.com", "n!u@host.example.com"));
        assert!(!mask_matches("abc", "abcd"));
        assert!(mask_matches("", ""));
        assert!(!mask_matches("", "x"));
        assert!(mask_matches("**a", "a"));
    }
}
