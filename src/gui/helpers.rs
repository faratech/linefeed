//! Helper utilities for the GUI module

/// Check if a name is an IRC channel (starts with # or &)
pub fn is_channel(name: &str) -> bool {
    name.starts_with('#') || name.starts_with('&')
}

/// Convert a nick or mask to a proper ban mask
/// If already contains !, @, or *, returns as-is
/// Otherwise, converts "nick" to "nick!*@*"
pub fn nick_to_mask(arg: &str) -> String {
    if arg.contains('!') || arg.contains('@') || arg.contains('*') {
        arg.to_string()
    } else {
        format!("{}!*@*", arg)
    }
}

/// Get current local time as [HH:MM] string
pub fn current_time_hhmm() -> String {
    #[cfg(unix)]
    let (hours, minutes) = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let t = secs as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&t, &mut tm) };
        (tm.tm_hour as u64, tm.tm_min as u64)
    };

    #[cfg(windows)]
    let (hours, minutes) = {
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
            (st.hour as u64, st.minute as u64)
        }
    };

    #[cfg(not(any(unix, windows)))]
    let (hours, minutes) = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Fallback to UTC
        let hours = (secs % 86400) / 3600;
        let minutes = (secs % 3600) / 60;
        (hours, minutes)
    };

    format!("[{:02}:{:02}]", hours, minutes)
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

/// Simple wildcard mask matching for ignore list
/// Supports * (any chars) and ? (single char)
pub fn mask_matches(pattern: &str, text: &str) -> bool {
    let mut pattern_chars = pattern.chars().peekable();
    let mut text_chars = text.chars().peekable();

    while let Some(p) = pattern_chars.next() {
        match p {
            '*' => {
                // Skip consecutive wildcards
                while pattern_chars.peek() == Some(&'*') {
                    pattern_chars.next();
                }
                // If * is at end, match rest
                if pattern_chars.peek().is_none() {
                    return true;
                }
                // Try matching rest of pattern from each position
                let remaining_pattern: String = pattern_chars.collect();
                while text_chars.peek().is_some() {
                    let remaining_text: String = text_chars.clone().collect();
                    if mask_matches(&remaining_pattern, &remaining_text) {
                        return true;
                    }
                    text_chars.next();
                }
                return mask_matches(&remaining_pattern, "");
            }
            '?' => {
                if text_chars.next().is_none() {
                    return false;
                }
            }
            c => {
                if text_chars.next() != Some(c) {
                    return false;
                }
            }
        }
    }
    text_chars.peek().is_none()
}
