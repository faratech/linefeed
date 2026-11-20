//! Helper utilities for the GUI module

/// Whether `value` is an extended ban ("$a:account", "$j:#chan", "~a:account",
/// "m:mask", and the named forms "~account:troll" / "account:troll"):
/// server-defined matching syntax, not a nick or a hostmask. It must reach
/// MODE unchanged. A nick can never start with `$` or contain ':', so a ':'
/// before any '!' or '@' marks an extban; a ':' after one is part of a host
/// (an IPv6 address such as `*@2001:db8::1`).
pub fn is_extended_ban(value: &str) -> bool {
    if value.starts_with('$') {
        return true;
    }
    value
        .find(':')
        .is_some_and(|colon| colon > 0 && !value[..colon].contains(['!', '@']))
}

/// Split a CTCP payload into `(command, arguments)`. The closing `\x01` is
/// optional on the wire (many clients omit it, and a long `/me` loses it to the
/// line limit), so it is stripped when present. A payload without an opening
/// `\x01` or without a command (a lone `\x01`, or `\x01\x01`) is not CTCP.
pub fn parse_ctcp(content: &str) -> Option<(&str, &str)> {
    let body = content.strip_prefix('\x01')?;
    let body = body.strip_suffix('\x01').unwrap_or(body);
    let (command, args) = body.split_once(' ').unwrap_or((body, ""));
    (!command.is_empty()).then_some((command, args))
}

/// The text of a CTCP ACTION (`/me`), or `None` for anything else.
pub fn ctcp_action_text(content: &str) -> Option<&str> {
    parse_ctcp(content)
        .filter(|(command, _)| command.eq_ignore_ascii_case("ACTION"))
        .map(|(_, args)| args)
}

/// Convert a nick or partial mask into a complete `nick!user@host` ban mask.
/// A full mask (a '!' followed later by an '@') or an extended ban is returned
/// unchanged; partial forms like "nick", "nick@host", or "nick!user" are
/// completed with `*` wildcards so the result is always a valid mask.
pub fn nick_to_mask(arg: &str) -> String {
    fn or_star(s: &str) -> &str {
        if s.is_empty() { "*" } else { s }
    }
    if is_extended_ban(arg) {
        return arg.to_string();
    }
    // Already a full nick!user@host mask? Pass it through. A mask with an
    // empty component ("!user@host", "nick!@host", "nick!user@") is not
    // full: it falls through to the completion below, which fills the
    // empty component with '*'.
    if let Some((nick, user_host)) = arg.split_once('!')
        && !nick.is_empty()
        && let Some((user, host)) = user_host.split_once('@')
        && !user.is_empty()
        && !host.is_empty()
    {
        return arg.to_string();
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

/// Decompose an epoch timestamp as UTC into (month, day, hours, minutes, seconds).
fn epoch_to_utc_parts(secs: u64) -> (u64, u64, u64, u64, u64) {
    let days = secs / 86400;
    let (_, m, d) = days_to_ymd(days);
    (
        m as u64,
        d as u64,
        (secs % 86400) / 3600,
        (secs % 3600) / 60,
        secs % 60,
    )
}

/// Decompose `secs` shifted by the local offset `offset_at` reports for that
/// instant. The offset function is a parameter so the per-instant behaviour is
/// testable without a time-zone database.
#[cfg(any(windows, test))]
fn epoch_to_parts_with_offset(
    secs: u64,
    offset_at: impl Fn(u64) -> i64,
) -> (u64, u64, u64, u64, u64) {
    epoch_to_utc_parts(secs.saturating_add_signed(offset_at(secs)))
}

/// Decompose an epoch timestamp in LOCAL time into (month, day, hours, minutes, seconds).
fn epoch_to_local_parts(secs: u64) -> (u64, u64, u64, u64, u64) {
    #[cfg(unix)]
    {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let res = unsafe { libc::localtime_r(&t, &mut tm) };
        if res.is_null() {
            // localtime_r failed; fall back to a UTC decomposition rather than
            // formatting the zeroed tm (which would render as 1900/epoch garbage).
            epoch_to_utc_parts(secs)
        } else {
            (
                (tm.tm_mon + 1) as u64,
                tm.tm_mday as u64,
                tm.tm_hour as u64,
                tm.tm_min as u64,
                tm.tm_sec as u64,
            )
        }
    }

    #[cfg(windows)]
    {
        // Windows has no localtime_r, so ask the time-zone rules for the offset
        // in force at this very instant (not today's), which keeps timestamps
        // from the other side of a DST change correct.
        epoch_to_parts_with_offset(secs, windows_local_offset_secs_at)
    }

    #[cfg(not(any(unix, windows)))]
    {
        epoch_to_utc_parts(secs)
    }
}

/// Seconds the local time zone is ahead of UTC at epoch time `secs` (negative
/// west of Greenwich), for formatting a full calendar date in local time.
pub fn local_utc_offset_secs(secs: u64) -> i64 {
    #[cfg(unix)]
    {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            0
        } else {
            tm.tm_gmtoff as i64
        }
    }

    #[cfg(windows)]
    {
        windows_local_offset_secs_at(secs)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = secs;
        0
    }
}

/// Mirrors the Win32 struct name.
#[cfg(windows)]
#[allow(clippy::upper_case_acronyms)]
#[repr(C)]
struct SYSTEMTIME {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

#[cfg(windows)]
impl SYSTEMTIME {
    fn as_epoch_secs(&self) -> i64 {
        ymd_to_days(self.year as i64, self.month as u32, self.day as u32) * 86400
            + self.hour as i64 * 3600
            + self.minute as i64 * 60
            + self.second as i64
    }
}

/// Current UTC offset in seconds on Windows (local now minus UTC now). Only a
/// fallback for instants the per-instant conversion cannot handle.
#[cfg(windows)]
fn windows_local_offset_secs() -> i64 {
    use std::mem::MaybeUninit;
    use std::time::{SystemTime, UNIX_EPOCH};
    unsafe extern "system" {
        fn GetLocalTime(lpSystemTime: *mut SYSTEMTIME);
    }
    let mut st = MaybeUninit::<SYSTEMTIME>::uninit();
    let local_epoch = unsafe {
        GetLocalTime(st.as_mut_ptr());
        st.assume_init().as_epoch_secs()
    };
    let utc_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    local_epoch - utc_epoch
}

/// UTC offset in seconds that the local time zone had at epoch time `secs`,
/// using the zone's DST rules for that date (`SystemTimeToTzSpecificLocalTime`)
/// rather than today's offset. Falls back to the current offset for instants
/// outside the SYSTEMTIME range or if the conversion fails.
#[cfg(windows)]
fn windows_local_offset_secs_at(secs: u64) -> i64 {
    unsafe extern "system" {
        fn SystemTimeToTzSpecificLocalTime(
            lpTimeZoneInformation: *const core::ffi::c_void,
            lpUniversalTime: *const SYSTEMTIME,
            lpLocalTime: *mut SYSTEMTIME,
        ) -> i32;
    }
    let (year, month, day) = days_to_ymd(secs / 86400);
    if !(1601..=30827).contains(&year) {
        return windows_local_offset_secs();
    }
    let rem = secs % 86400;
    let utc = SYSTEMTIME {
        year: year as u16,
        month: month as u16,
        day_of_week: 0,
        day: day as u16,
        hour: (rem / 3600) as u16,
        minute: ((rem % 3600) / 60) as u16,
        second: (rem % 60) as u16,
        milliseconds: 0,
    };
    let mut local = SYSTEMTIME {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    // A null time zone means the current one, with the rules for `utc`'s year.
    let converted = unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) };
    if converted == 0 {
        return windows_local_offset_secs();
    }
    local.as_epoch_secs() - secs as i64
}

/// Get current local time formatted according to preference
/// "short" = [HH:MM], "long" = [HH:MM:SS], "full" = [MM-DD HH:MM]
pub fn current_time_formatted(format: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    epoch_time_formatted(secs, format)
}

/// Format an epoch timestamp (UTC) as LOCAL time in the user's timestamp format.
/// Used for IRCv3 server-time so bounced/replayed messages line up with the
/// local clock instead of showing raw UTC.
pub fn epoch_time_formatted(secs: u64, format: &str) -> String {
    let (month, day, hours, minutes, seconds) = epoch_to_local_parts(secs);
    match format {
        "long" => format!("[{:02}:{:02}:{:02}]", hours, minutes, seconds),
        "full" => format!("[{:02}-{:02} {:02}:{:02}]", month, day, hours, minutes),
        _ => format!("[{:02}:{:02}]", hours, minutes), // "short" or default
    }
}

/// Convert (year, month, day) to days since Unix epoch (inverse of days_to_ymd).
#[cfg(windows)]
fn ymd_to_days(y: i64, m: u32, d: u32) -> i64 {
    // Howard Hinnant's days_from_civil
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
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
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    };
    format!(
        "{} {}, {} {:02}:{:02} UTC",
        month_name, day, year, hours, minutes
    )
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

/// The body of a CTCP TIME reply: `YYYY-MM-DD HH:MM:SS` at `secs` since the
/// epoch shifted by `utc_offset` seconds (the local zone's offset). Pure so the
/// calendar formatting is testable on every platform.
pub fn format_ctcp_time(secs: u64, utc_offset: i64) -> String {
    let local = secs.saturating_add_signed(utc_offset);
    let (year, month, day) = days_to_ymd(local / 86400);
    let rem = local % 86400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
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
    let mut star_ti = 0usize; // text index when that '*' was taken

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
    fn local_time_uses_the_offset_in_force_at_the_instant() {
        // A zone that observes DST: UTC-4 until 2026-11-01T06:00:00Z, then UTC-5.
        let dst_end = 1_793_512_800;
        let offset_at = |secs: u64| if secs < dst_end { -4 * 3600 } else { -5 * 3600 };
        // 2026-10-20T18:30:00Z was 14:30 local; 2026-11-10T18:30:00Z is 13:30.
        let before = epoch_to_parts_with_offset(1_792_521_000, offset_at);
        assert_eq!(before, (10, 20, 14, 30, 0));
        let after = epoch_to_parts_with_offset(1_794_335_400, offset_at);
        assert_eq!(after, (11, 10, 13, 30, 0));
    }

    #[cfg(windows)]
    #[test]
    fn windows_offset_is_resolved_per_instant() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // The present instant agrees with the current-offset computation.
        assert_eq!(windows_local_offset_secs_at(now), windows_local_offset_secs());
        // Half a year apart the offsets differ by at most the DST shift (zero
        // in a zone without DST), and the epoch itself is convertible.
        let (winter, summer) = (1_768_478_400, 1_784_116_800);
        let shift = windows_local_offset_secs_at(winter) - windows_local_offset_secs_at(summer);
        assert!(shift.abs() <= 3600, "{shift}");
        assert_eq!(local_utc_offset_secs(winter), windows_local_offset_secs_at(winter));
        let _ = windows_local_offset_secs_at(0);
    }

    #[test]
    fn ctcp_time_is_a_calendar_date_not_a_day_count() {
        assert_eq!(format_ctcp_time(0, 0), "1970-01-01 00:00:00");
        assert_eq!(format_ctcp_time(1_700_000_000, 0), "2023-11-14 22:13:20");
        // 2000-02-29 00:00:00 UTC, with zone offsets crossing midnight.
        let leap_day = 951_782_400;
        assert_eq!(format_ctcp_time(leap_day + 3661, 0), "2000-02-29 01:01:01");
        assert_eq!(
            format_ctcp_time(leap_day + 3661, -7200),
            "2000-02-28 23:01:01"
        );
        assert_eq!(
            format_ctcp_time(leap_day + 82_800, 7200),
            "2000-03-01 01:00:00"
        );
        assert!(!format_ctcp_time(1_700_000_000, 0).contains("Day"));
    }

    #[test]
    fn ctcp_payloads_parse_with_or_without_the_closing_delimiter() {
        assert_eq!(parse_ctcp("\x01VERSION\x01"), Some(("VERSION", "")));
        assert_eq!(parse_ctcp("\x01VERSION"), Some(("VERSION", "")));
        assert_eq!(parse_ctcp("\x01PING 123\x01"), Some(("PING", "123")));
        assert_eq!(parse_ctcp("\x01PING 123"), Some(("PING", "123")));
        assert_eq!(parse_ctcp("\x01FOO a b\x01"), Some(("FOO", "a b")));
        // Not CTCP: no opening delimiter, or no command.
        assert_eq!(parse_ctcp("hello\x01"), None);
        assert_eq!(parse_ctcp("\x01"), None);
        assert_eq!(parse_ctcp("\x01\x01"), None);
        assert_eq!(parse_ctcp(""), None);

        assert_eq!(ctcp_action_text("\x01ACTION waves\x01"), Some("waves"));
        assert_eq!(ctcp_action_text("\x01ACTION waves"), Some("waves"));
        assert_eq!(ctcp_action_text("\x01action waves\x01"), Some("waves"));
        assert_eq!(ctcp_action_text("\x01ACTION\x01"), Some(""));
        assert_eq!(ctcp_action_text("\x01ACTION"), Some(""));
        assert_eq!(ctcp_action_text("\x01ACTIONS waves\x01"), None);
        assert_eq!(ctcp_action_text("\x01VERSION\x01"), None);
        assert_eq!(ctcp_action_text("plain"), None);
    }

    #[test]
    fn nick_to_mask_fills_empty_components() {
        assert_eq!(nick_to_mask("!user@host"), "*!user@host");
        assert_eq!(nick_to_mask("nick!@host"), "nick!*@host");
        assert_eq!(nick_to_mask("nick!user@"), "nick!user@*");
        assert_eq!(nick_to_mask("!@"), "*!*@*");
        assert_eq!(nick_to_mask("!user"), "*!user@*");
        assert_eq!(nick_to_mask("@host"), "*!*@host");
    }

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
    fn extended_bans_are_recognised_and_left_alone() {
        for extban in [
            "$a:account",
            "$a",
            "$~a",
            "$j:#chan",
            "$a:acc*",
            "~a:account",
            "~q:nick!user@host",
            "~t:5:*!*@host",
            "R:account",
            "m:*!*@host",
            // Named extbans (UnrealIRCd, Solanum, InspIRCd 4).
            "~account:troll",
            "$account:troll",
            "account:troll",
            "~quiet:*!*@host",
            "~timedban:5:*!*@host",
            "~1:x",
        ] {
            assert!(is_extended_ban(extban), "{extban}");
            assert_eq!(nick_to_mask(extban), extban);
        }
        for plain in [
            "nick",
            "a",
            "~",
            "nick!user@host",
            "*!*@host",
            "*@2001:db8::1",
            "x!*@2001:db8::1",
            "nick!user@2001:db8::1",
            "*!user@host:6667",
            ":",
            "",
        ] {
            assert!(!is_extended_ban(plain), "{plain}");
        }
    }

    fn naive_glob(pattern: &[char], text: &[char]) -> bool {
        if pattern.is_empty() {
            return text.is_empty();
        }
        match pattern[0] {
            '*' => (0..=text.len()).any(|skip| naive_glob(&pattern[1..], &text[skip..])),
            '?' => !text.is_empty() && naive_glob(&pattern[1..], &text[1..]),
            c => !text.is_empty() && text[0] == c && naive_glob(&pattern[1..], &text[1..]),
        }
    }

    #[test]
    fn mask_matches_agrees_with_a_naive_glob() {
        let alphabet: Vec<char> = vec!['a', 'b', '*', '?'];
        let mut state: u64 = 0xdead_beef_cafe_f00d;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for case in 0..20000 {
            let plen = (next() % 7) as usize;
            let tlen = (next() % 7) as usize;
            let pattern: String = (0..plen)
                .map(|_| alphabet[(next() as usize) % alphabet.len()])
                .collect();
            let text: String = (0..tlen).map(|_| alphabet[(next() as usize) % 2]).collect();
            let p: Vec<char> = pattern.chars().collect();
            let t: Vec<char> = text.chars().collect();
            assert_eq!(
                mask_matches(&pattern, &text),
                naive_glob(&p, &t),
                "case {case} pattern {pattern:?} text {text:?}"
            );
        }
    }

    #[test]
    fn days_to_ymd_matches_a_datetime_oracle() {
        // (days since epoch, year, month, day) generated with Python datetime.
        for (days, y, m, d) in [
            (0u64, 1970, 1, 1),
            (58, 1970, 2, 28),
            (59, 1970, 3, 1),
            (365, 1971, 1, 1),
            (730, 1972, 1, 1),
            (789, 1972, 2, 29),
            (10957, 2000, 1, 1),
            (11016, 2000, 2, 29),
            (47482, 2100, 1, 1),
            (47541, 2100, 3, 1),
            (719162, 3939, 1, 1),
        ] {
            assert_eq!(days_to_ymd(days), (y, m, d), "days {days}");
        }
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
