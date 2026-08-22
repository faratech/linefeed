use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum IrcCommand {
    // Connection
    Pass(String),
    Nick(String),
    User { username: String, realname: String },
    Quit(Option<String>),
    Away(Option<String>),

    // Channel
    // When sending: (channel, optional key, None, None)
    // When receiving with extended-join: (channel, None, account, realname)
    Join(String, Option<String>, Option<String>, Option<String>), // (channel, key, account, realname)
    Part(String, Option<String>),
    Topic(String, Option<String>),
    Names(Option<String>),
    List(Option<String>),
    Kick(String, String, Option<String>), // channel, nick, reason
    Invite(String, String),               // nick, channel

    // Messaging
    Privmsg(String, String),
    Notice(String, String),
    Wallops(String),

    // Server queries
    Ping(String),
    Pong(String),
    Who(String),
    Whois(String),
    Whowas(String),
    Mode(String, Option<String>, Vec<String>),
    Userhost(String), // space-separated nicks
    Ison(String),     // space-separated nicks

    // IRCv3 Monitor (friend list)
    Monitor(String, Option<String>), // (subcommand: +/-/C/L/S, optional targets)

    // IRCv3 account-notify: user logged in/out of account
    Account(String), // account name, or "*" if logged out

    // IRCv3 away-notify: user away status changed (from prefix)
    // Away(Option<String>) is reused - None means back, Some(msg) means away

    // IRCv3 CHGHOST: user changed their host
    Chghost(String, String), // (new_user, new_host)

    // IRCv3 batch: start/end of a batch
    Batch(String, Option<String>, Option<String>), // (+/-reference, type, params)

    // Server info
    Time(Option<String>),
    Motd(Option<String>),
    Admin(Option<String>),
    Info(Option<String>),
    Version(Option<String>), // Server VERSION (not CTCP)
    Lusers,
    Links(Option<String>),
    Stats(String),
    Trace(Option<String>),

    // Numeric replies (server responses)
    Numeric(u16, Vec<String>),

    // CAP negotiation: (target, subcommand, remaining params)
    // For a multiline `CAP * LS * :caps` the remaining params are ["*", "caps"];
    // for the final `CAP * LS :caps` they are ["caps"]. The trailing capability
    // list is always the last element.
    Cap(String, String, Vec<String>),

    // SASL authentication
    Authenticate(String),

    // Raw/unknown
    Raw(String),
}

#[derive(Debug, Clone)]
pub struct IrcMessage {
    pub tags: Option<String>,
    pub prefix: Option<String>,
    pub command: IrcCommand,
    pub raw: String,
}

impl IrcMessage {
    pub fn parse(line: &str) -> Option<Self> {
        // Strip only the IRC framing terminator. General trimming would corrupt
        // meaningful spaces at the end of a trailing parameter.
        let line = if let Some(without_lf) = line.strip_suffix('\n') {
            without_lf.strip_suffix('\r').unwrap_or(without_lf)
        } else {
            line
        };
        let raw = line.to_string();
        let mut remaining = line.trim_start();
        if remaining.is_empty() {
            return None;
        }

        // Parse optional tags (@key=value;key2=value2). The grammar permits
        // multiple spaces between sections (RFC 1459), hence the trim_start.
        let tags = if remaining.starts_with('@') {
            let end = remaining.find(' ')?;
            let tags = remaining[1..end].to_string();
            remaining = remaining[end + 1..].trim_start();
            Some(tags)
        } else {
            None
        };

        // Parse optional prefix (:nick!user@host)
        let prefix = if remaining.starts_with(':') {
            let end = remaining.find(' ')?;
            let prefix = remaining[1..end].to_string();
            remaining = remaining[end + 1..].trim_start();
            Some(prefix)
        } else {
            None
        };

        // Parse command and params
        let parts: Vec<&str> = remaining.splitn(2, ' ').collect();
        let cmd = parts[0].to_uppercase();
        let params_str = parts.get(1).copied().unwrap_or("");

        // Parse parameters (handle :trailing)
        let params = Self::parse_params(params_str);

        let command = Self::parse_command(&cmd, params);

        Some(IrcMessage {
            tags,
            prefix,
            command,
            raw,
        })
    }

    fn parse_params(s: &str) -> Vec<String> {
        let mut params = Vec::new();
        // Tolerate extra spaces before the first parameter; interior runs are
        // handled by the trim_start below (spaces inside a :trailing parameter
        // are preserved).
        let mut remaining = s.trim_start();

        while !remaining.is_empty() {
            if let Some(stripped) = remaining.strip_prefix(':') {
                // Trailing parameter (rest of the line)
                params.push(stripped.to_string());
                break;
            }

            if let Some(pos) = remaining.find(' ') {
                params.push(remaining[..pos].to_string());
                remaining = remaining[pos + 1..].trim_start();
            } else {
                params.push(remaining.to_string());
                break;
            }
        }

        params
    }

    fn parse_command(cmd: &str, params: Vec<String>) -> IrcCommand {
        match cmd {
            "PING" => IrcCommand::Ping(params.first().cloned().unwrap_or_default()),
            "PONG" => IrcCommand::Pong(params.first().cloned().unwrap_or_default()),
            "PRIVMSG" => IrcCommand::Privmsg(
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned().unwrap_or_default(),
            ),
            "NOTICE" => IrcCommand::Notice(
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned().unwrap_or_default(),
            ),
            "JOIN" => {
                // Standard JOIN: channel only
                // Extended-join (IRCv3): channel, account, realname
                let channel = params.first().cloned().unwrap_or_default();
                let account = params.get(1).cloned().filter(|a| a != "*");
                let realname = params.get(2).cloned();
                IrcCommand::Join(channel, None, account, realname)
            }
            "PART" => IrcCommand::Part(
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned(),
            ),
            "QUIT" => IrcCommand::Quit(params.first().cloned()),
            "AWAY" => IrcCommand::Away(params.first().cloned()),
            "NICK" => IrcCommand::Nick(params.first().cloned().unwrap_or_default()),
            "TOPIC" => IrcCommand::Topic(
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned(),
            ),
            "KICK" => IrcCommand::Kick(
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned().unwrap_or_default(),
                params.get(2).cloned(),
            ),
            "INVITE" => IrcCommand::Invite(
                params.first().cloned().unwrap_or_default(), // target nick (us)
                params.get(1).cloned().unwrap_or_default(),  // channel
            ),
            "MODE" => IrcCommand::Mode(
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned(),
                params.get(2..).map(|s| s.to_vec()).unwrap_or_default(),
            ),
            "CAP" => IrcCommand::Cap(
                params.first().cloned().unwrap_or_default(), // target (usually "*" or nick)
                params.get(1).cloned().unwrap_or_default(),  // subcommand (LS, ACK, NAK, etc.)
                params.get(2..).map(|s| s.to_vec()).unwrap_or_default(), // [continuation marker +] cap list
            ),
            "AUTHENTICATE" => IrcCommand::Authenticate(params.first().cloned().unwrap_or_default()),
            // IRCv3 account-notify
            "ACCOUNT" => {
                IrcCommand::Account(params.first().cloned().unwrap_or_else(|| "*".to_string()))
            }
            // IRCv3 chghost
            "CHGHOST" => IrcCommand::Chghost(
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned().unwrap_or_default(),
            ),
            // IRCv3 batch
            "BATCH" => IrcCommand::Batch(
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned(),
                params.get(2).cloned(),
            ),
            _ => {
                // Try to parse as numeric
                if let Ok(num) = cmd.parse::<u16>() {
                    IrcCommand::Numeric(num, params)
                } else {
                    IrcCommand::Raw(format!("{} {}", cmd, params.join(" ")))
                }
            }
        }
    }

    pub fn get_sender_nick(&self) -> Option<String> {
        self.prefix
            .as_ref()
            .map(|p| p.split('!').next().unwrap_or(p).to_string())
    }

    /// Get a specific IRCv3 tag value
    pub fn get_tag(&self, key: &str) -> Option<String> {
        self.tags.as_ref().and_then(|tags| {
            // A repeated tag key means the sender revised the value; per the
            // IRCv3 message-tags spec, receivers keep only the final one.
            let mut found: Option<String> = None;
            for part in tags.split(';') {
                if let Some((k, v)) = part.split_once('=') {
                    if k == key {
                        found = Some(unescape_tag_value(v));
                    }
                } else if part == key {
                    found = Some(String::new());
                }
            }
            found
        })
    }

    /// Get server-time from tags (IRCv3 server-time) as a Unix timestamp.
    /// The tag value is ISO 8601 in UTC (e.g. "2025-12-28T19:30:00.000Z");
    /// strict parsing here also means a malicious tag value cannot smuggle
    /// arbitrary text (e.g. newlines) into timestamps or log lines. The GUI
    /// converts the timestamp to local time in the user's chosen format.
    pub fn get_server_time(&self) -> Option<u64> {
        self.get_tag("time").and_then(|iso| parse_iso8601_utc(&iso))
    }

    /// Get account name from tags (IRCv3 account tag)
    pub fn get_account(&self) -> Option<String> {
        self.get_tag("account")
    }

    /// Get batch reference from tags (IRCv3 batch)
    pub fn get_batch(&self) -> Option<String> {
        self.get_tag("batch")
    }
}

/// Parse the canonical IRCv3 server-time form
/// `YYYY-MM-DDThh:mm:ss.sssZ` into Unix epoch seconds.
fn parse_iso8601_utc(iso: &str) -> Option<u64> {
    fn digits(bytes: &[u8]) -> Option<u32> {
        bytes.iter().try_fold(0_u32, |value, byte| {
            let digit = byte.checked_sub(b'0')?;
            if digit > 9 {
                return None;
            }
            value.checked_mul(10)?.checked_add(u32::from(digit))
        })
    }

    let bytes = iso.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return None;
    }

    let year = digits(&bytes[0..4])?;
    let month = digits(&bytes[5..7])?;
    let day = digits(&bytes[8..10])?;
    let hours = digits(&bytes[11..13])?;
    let minutes = digits(&bytes[14..16])?;
    let seconds = digits(&bytes[17..19])?;
    // Parse milliseconds even though this API intentionally returns seconds.
    let _milliseconds = digits(&bytes[20..23])?;

    if year < 1970 || !(1..=12).contains(&month) || hours > 23 || minutes > 59 || seconds > 59 {
        return None;
    }

    let leap_year =
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days_in_month = match month {
        2 if leap_year => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day == 0 || day > days_in_month {
        return None;
    }

    // Howard Hinnant's days_from_civil
    let year = i64::from(year);
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 } as i64;
    let doy = 153_i64
        .checked_mul(mp)?
        .checked_add(2)?
        .checked_div(5)?
        .checked_add(i64::from(day))?
        .checked_sub(1)?;
    let doe = yoe
        .checked_mul(365)?
        .checked_add(yoe / 4)?
        .checked_sub(yoe / 100)?
        .checked_add(doy)?;
    let days = era
        .checked_mul(146_097)?
        .checked_add(doe)?
        .checked_sub(719_468)?;
    let days = u64::try_from(days).ok()?;
    days.checked_mul(86_400)?
        .checked_add(u64::from(hours).checked_mul(3_600)?)?
        .checked_add(u64::from(minutes).checked_mul(60)?)?
        .checked_add(u64::from(seconds))
}

/// Unescape an IRCv3 message-tag value per the spec
/// (https://ircv3.net/specs/extensions/message-tags#escaping-values):
/// `\:` -> `;`, `\s` -> space, `\r` -> CR, `\n` -> LF, `\\` -> `\`.
/// A lone trailing backslash and unrecognized escapes drop the backslash.
fn unescape_tag_value(value: &str) -> String {
    if !value.contains('\\') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(':') => out.push(';'),
                Some('s') => out.push(' '),
                Some('r') => out.push('\r'),
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl fmt::Display for IrcCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IrcCommand::Pass(pass) => write!(f, "PASS {}", pass),
            IrcCommand::Nick(nick) => write!(f, "NICK {}", nick),
            IrcCommand::User { username, realname } => {
                write!(f, "USER {} 0 * :{}", username, realname)
            }
            IrcCommand::Quit(msg) => {
                if let Some(m) = msg {
                    write!(f, "QUIT :{}", m)
                } else {
                    write!(f, "QUIT")
                }
            }
            IrcCommand::Away(msg) => {
                if let Some(m) = msg {
                    write!(f, "AWAY :{}", m)
                } else {
                    write!(f, "AWAY")
                }
            }
            IrcCommand::Join(channel, key, _account, _realname) => {
                // When sending, only channel and optional key are used
                if let Some(k) = key {
                    write!(f, "JOIN {} {}", channel, k)
                } else {
                    write!(f, "JOIN {}", channel)
                }
            }
            IrcCommand::Part(channel, msg) => {
                if let Some(m) = msg {
                    write!(f, "PART {} :{}", channel, m)
                } else {
                    write!(f, "PART {}", channel)
                }
            }
            IrcCommand::Privmsg(target, msg) => write!(f, "PRIVMSG {} :{}", target, msg),
            IrcCommand::Notice(target, msg) => write!(f, "NOTICE {} :{}", target, msg),
            IrcCommand::Ping(server) => write!(f, "PING :{}", server),
            IrcCommand::Pong(server) => write!(f, "PONG :{}", server),
            IrcCommand::Topic(channel, topic) => {
                if let Some(t) = topic {
                    write!(f, "TOPIC {} :{}", channel, t)
                } else {
                    write!(f, "TOPIC {}", channel)
                }
            }
            IrcCommand::Who(mask) => write!(f, "WHO {}", mask),
            IrcCommand::Whois(nick) => write!(f, "WHOIS {}", nick),
            IrcCommand::Mode(target, mode, params) => {
                if let Some(m) = mode {
                    if params.is_empty() {
                        write!(f, "MODE {} {}", target, m)
                    } else {
                        write!(f, "MODE {} {} {}", target, m, params.join(" "))
                    }
                } else {
                    write!(f, "MODE {}", target)
                }
            }
            IrcCommand::Names(channel) => {
                if let Some(c) = channel {
                    write!(f, "NAMES {}", c)
                } else {
                    write!(f, "NAMES")
                }
            }
            IrcCommand::List(channel) => {
                if let Some(c) = channel {
                    write!(f, "LIST {}", c)
                } else {
                    write!(f, "LIST")
                }
            }
            IrcCommand::Kick(channel, user, reason) => {
                if let Some(r) = reason {
                    write!(f, "KICK {} {} :{}", channel, user, r)
                } else {
                    write!(f, "KICK {} {}", channel, user)
                }
            }
            IrcCommand::Invite(nick, channel) => write!(f, "INVITE {} {}", nick, channel),
            IrcCommand::Wallops(msg) => write!(f, "WALLOPS :{}", msg),
            IrcCommand::Whowas(nick) => write!(f, "WHOWAS {}", nick),
            IrcCommand::Userhost(nicks) => write!(f, "USERHOST {}", nicks),
            IrcCommand::Ison(nicks) => write!(f, "ISON {}", nicks),
            IrcCommand::Monitor(subcmd, targets) => {
                if let Some(t) = targets {
                    write!(f, "MONITOR {} {}", subcmd, t)
                } else {
                    write!(f, "MONITOR {}", subcmd)
                }
            }
            IrcCommand::Time(server) => {
                if let Some(s) = server {
                    write!(f, "TIME {}", s)
                } else {
                    write!(f, "TIME")
                }
            }
            IrcCommand::Motd(server) => {
                if let Some(s) = server {
                    write!(f, "MOTD {}", s)
                } else {
                    write!(f, "MOTD")
                }
            }
            IrcCommand::Admin(server) => {
                if let Some(s) = server {
                    write!(f, "ADMIN {}", s)
                } else {
                    write!(f, "ADMIN")
                }
            }
            IrcCommand::Info(server) => {
                if let Some(s) = server {
                    write!(f, "INFO {}", s)
                } else {
                    write!(f, "INFO")
                }
            }
            IrcCommand::Version(server) => {
                if let Some(s) = server {
                    write!(f, "VERSION {}", s)
                } else {
                    write!(f, "VERSION")
                }
            }
            IrcCommand::Lusers => write!(f, "LUSERS"),
            IrcCommand::Links(mask) => {
                if let Some(m) = mask {
                    write!(f, "LINKS {}", m)
                } else {
                    write!(f, "LINKS")
                }
            }
            IrcCommand::Stats(query) => write!(f, "STATS {}", query),
            IrcCommand::Trace(target) => {
                if let Some(t) = target {
                    write!(f, "TRACE {}", t)
                } else {
                    write!(f, "TRACE")
                }
            }
            IrcCommand::Cap(_target, sub, params) => {
                // When sending CAP commands, we don't include target (server adds it)
                if params.is_empty() {
                    write!(f, "CAP {}", sub)
                } else {
                    write!(f, "CAP {} :{}", sub, params.join(" "))
                }
            }
            IrcCommand::Authenticate(data) => write!(f, "AUTHENTICATE {}", data),
            IrcCommand::Account(account) => write!(f, "ACCOUNT {}", account),
            IrcCommand::Chghost(user, host) => write!(f, "CHGHOST {} {}", user, host),
            IrcCommand::Batch(reference, batch_type, params) => match (batch_type, params) {
                (Some(t), Some(p)) => write!(f, "BATCH {} {} {}", reference, t, p),
                (Some(t), None) => write!(f, "BATCH {} {}", reference, t),
                _ => write!(f, "BATCH {}", reference),
            },
            IrcCommand::Numeric(num, params) => {
                write!(f, "{:03} {}", num, params.join(" "))
            }
            IrcCommand::Raw(raw) => write!(f, "{}", raw),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_ls_multiline_carries_continuation_marker_and_caps() {
        // Continuation line: rest = ["*", caps]
        let m = IrcMessage::parse(":srv CAP * LS * :multi-prefix sasl=PLAIN").unwrap();
        match m.command {
            IrcCommand::Cap(target, sub, rest) => {
                assert_eq!(target, "*");
                assert_eq!(sub, "LS");
                assert_eq!(
                    rest,
                    vec!["*".to_string(), "multi-prefix sasl=PLAIN".to_string()]
                );
            }
            other => panic!("expected Cap, got {:?}", other),
        }
        // Final line: rest = [caps]
        let m = IrcMessage::parse(":srv CAP * LS :away-notify").unwrap();
        if let IrcCommand::Cap(_, sub, rest) = m.command {
            assert_eq!(sub, "LS");
            assert_eq!(rest, vec!["away-notify".to_string()]);
        } else {
            panic!("expected Cap");
        }
    }

    #[test]
    fn tag_values_are_unescaped() {
        // wire: k1=a\sb -> "a b"; k2=x\:y -> "x;y"; k3 (no value) -> ""
        let m = IrcMessage::parse("@k1=a\\sb;k2=x\\:y;k3 PRIVMSG #c :hi").unwrap();
        assert_eq!(m.get_tag("k1").as_deref(), Some("a b"));
        assert_eq!(m.get_tag("k2").as_deref(), Some("x;y"));
        assert_eq!(m.get_tag("k3").as_deref(), Some(""));
        assert_eq!(m.get_tag("missing"), None);
    }

    #[test]
    fn repeated_tag_keys_keep_the_final_value() {
        // A bouncer prepending its own `time` must not shadow the inner one.
        let m = IrcMessage::parse(
            "@time=2020-01-01T00:00:00.000Z;time=2025-12-28T19:30:00.000Z :n!u@h PRIVMSG #c :hi",
        )
        .unwrap();
        assert_eq!(
            m.get_tag("time").as_deref(),
            Some("2025-12-28T19:30:00.000Z")
        );
    }

    #[test]
    fn lone_ctcp_byte_notice_parses() {
        let m = IrcMessage::parse(":x NOTICE me :\u{0001}").unwrap();
        if let IrcCommand::Notice(_, content) = m.command {
            assert_eq!(content, "\u{0001}");
        } else {
            panic!("expected Notice");
        }
    }

    #[test]
    fn server_time_parses_to_epoch() {
        let m = IrcMessage::parse("@time=2025-12-28T19:30:00.000Z :n!u@h PRIVMSG #c :hi").unwrap();
        // 2025-12-28 19:30:00 UTC
        assert_eq!(m.get_server_time(), Some(1_766_950_200));

        // Malformed / injected values are rejected outright.
        let m = IrcMessage::parse("@time=19:30\\nforged :n!u@h PRIVMSG #c :hi").unwrap();
        assert_eq!(m.get_server_time(), None);
        let m = IrcMessage::parse("@time=not-a-time :n!u@h PRIVMSG #c :hi").unwrap();
        assert_eq!(m.get_server_time(), None);
    }

    #[test]
    fn server_time_rejects_overflow_and_noncanonical_dates() {
        assert_eq!(
            parse_iso8601_utc("9223372036854775807-01-01T00:00:00.000Z"),
            None
        );
        for invalid in [
            "2025-02-29T00:00:00.000Z", // impossible calendar date
            "2025-04-31T00:00:00.000Z",
            "2025-01-01T00:00Z",         // missing seconds and fraction
            "2025-01-01T00:00:00Z",      // missing milliseconds
            "2025-01-01T00:00:00.000",   // missing Z
            "2025-01-01T00:00:00.000ZZ", // repeated Z
            "2025-01-01T00:00:60.000Z",  // out-of-range seconds
            "2025-01-01T24:00:00.000Z",  // out-of-range hour
            "1969-12-31T23:59:59.999Z",  // cannot fit the u64 epoch API
        ] {
            assert_eq!(parse_iso8601_utc(invalid), None, "accepted {invalid}");
        }

        assert!(parse_iso8601_utc("2024-02-29T23:59:59.999Z").is_some());
        assert!(parse_iso8601_utc("9999-12-31T23:59:59.999Z").is_some());
    }

    #[test]
    fn trailing_parameter_spaces_survive_line_framing() {
        let message = IrcMessage::parse(":alice!u@h PRIVMSG me :hello  \r\n").unwrap();
        assert_eq!(message.raw, ":alice!u@h PRIVMSG me :hello  ");
        match message.command {
            IrcCommand::Privmsg(target, content) => {
                assert_eq!(target, "me");
                assert_eq!(content, "hello  ");
            }
            other => panic!("expected Privmsg, got {other:?}"),
        }

        // A lone CR is not an IRC line terminator and must not be silently
        // discarded by the parser.
        let message = IrcMessage::parse(":alice PRIVMSG me :hello\r").unwrap();
        assert!(matches!(
            message.command,
            IrcCommand::Privmsg(_, ref content) if content == "hello\r"
        ));
    }

    #[test]
    fn consecutive_spaces_are_tolerated() {
        // RFC 1459 permits multiple spaces between message sections.
        let m = IrcMessage::parse(":nick!u@h  PRIVMSG  #chan  :hi  there").unwrap();
        match m.command {
            IrcCommand::Privmsg(target, content) => {
                assert_eq!(target, "#chan");
                // Spaces inside the trailing parameter are preserved.
                assert_eq!(content, "hi  there");
            }
            other => panic!("expected Privmsg, got {:?}", other),
        }

        let m = IrcMessage::parse("@t=1  :nick!u@h  KICK  #chan  bob  :bye").unwrap();
        match m.command {
            IrcCommand::Kick(channel, nick, reason) => {
                assert_eq!(channel, "#chan");
                assert_eq!(nick, "bob");
                assert_eq!(reason.as_deref(), Some("bye"));
            }
            other => panic!("expected Kick, got {:?}", other),
        }
    }

    #[test]
    fn mode_preserves_all_parameters() {
        let m = IrcMessage::parse(":oper MODE #chan +kl key 50").unwrap();
        match m.command {
            IrcCommand::Mode(target, mode, params) => {
                assert_eq!(target, "#chan");
                assert_eq!(mode.as_deref(), Some("+kl"));
                assert_eq!(params, vec!["key".to_string(), "50".to_string()]);
                assert_eq!(
                    IrcCommand::Mode(target, mode, params).to_string(),
                    "MODE #chan +kl key 50"
                );
            }
            other => panic!("expected Mode, got {:?}", other),
        }
    }
}
