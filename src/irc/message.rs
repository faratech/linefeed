use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum IrcCommand {
    // Connection
    Pass(String),
    Nick(String),
    User {
        username: String,
        realname: String,
    },
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
    /// Nefarious targeted channel messages. The char is the minimum status
    /// prefix advertised in STATUSMSG (`@`, `%`, or `+`).
    StatusMessage(char, String, String),
    Tagmsg(String),

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
    Setname(String),

    // IRCv3 batch: start/end of a batch
    Batch(String, Option<String>, Option<String>), // (+/-reference, type, params)

    // IRCv3 draft/chathistory. Linefeed currently requests the latest page;
    // keeping it typed prevents targets from being spliced into a raw command.
    ChathistoryLatest(String, String, usize), // (target, reference or *, limit)
    ChathistoryBefore(String, String, usize),
    ChathistoryAfter(String, String, usize),
    ChathistoryAround(String, String, usize),
    ChathistoryBetween(String, String, String, usize),
    ChathistoryTargets(String, String, usize),
    ChathistoryTarget(String, String),

    // IRCv3 and Nefarious client-facing extensions.
    StandardReply {
        kind: String,
        command: String,
        code: String,
        context: Vec<String>,
        description: String,
    },
    Markread(String, Option<String>),
    Redact(String, String, Option<String>),
    Rename(String, String, Option<String>),
    Relocate(String, String, Option<String>),
    /// Informational extension responses that should remain structured rather
    /// than falling through to raw IRC framing in the server buffer.
    Extension(String, Vec<String>),

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

        // Tags and/or a prefix with nothing after them have no command. The
        // unterminated forms ("@t=1", ":srv") return None above, so the ones
        // with trailing spaces must too, not become Raw(" ") (#304).
        if remaining.is_empty() {
            return None;
        }

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

    /// Whether `params` carries what the typed form of `cmd` needs. Required
    /// identity parameters (target, channel, nick, ...) must be present and
    /// non-empty; message text may legitimately be empty. Commands that are
    /// not listed have no required parameters. PING/PONG stay lenient: a bare
    /// PING is still answered with an empty token rather than ignored.
    fn has_required_params(cmd: &str, params: &[String]) -> bool {
        // (parameters that must be present, leading ones that must be non-empty)
        let (present, named) = match cmd {
            "PRIVMSG" | "NOTICE" | "WALLCHOPS" | "WALLHOPS" | "WALLVOICES" => (2, 1),
            "JOIN" | "PART" | "NICK" | "TOPIC" | "TAGMSG" | "MODE" | "AUTHENTICATE" | "ACCOUNT"
            | "SETNAME" | "MARKREAD" | "BATCH" => (1, 1),
            "KICK" | "INVITE" | "CAP" | "CHGHOST" | "REDACT" | "RENAME" | "RELOCATE" => (2, 2),
            // FAIL/WARN/NOTE <command> <code> [<context>...] <description>:
            // without the description, "last param" would reuse the code (or
            // the command) as the description.
            "FAIL" | "WARN" | "NOTE" => (3, 2),
            // CHATHISTORY TARGETS <target> <timestamp>
            "CHATHISTORY"
                if params
                    .first()
                    .is_some_and(|p| p.eq_ignore_ascii_case("TARGETS")) =>
            {
                (3, 3)
            }
            _ => (0, 0),
        };
        params.len() >= present && params[..named].iter().all(|param| !param.is_empty())
    }

    /// The untyped form of a command line, for unknown commands and for known
    /// commands that arrive without the parameters they require.
    fn raw_command(cmd: &str, params: &[String]) -> IrcCommand {
        if params.is_empty() {
            IrcCommand::Raw(cmd.to_string())
        } else {
            IrcCommand::Raw(format!("{} {}", cmd, params.join(" ")))
        }
    }

    fn parse_command(cmd: &str, params: Vec<String>) -> IrcCommand {
        // A line missing required parameters must not become a typed command
        // with fabricated empty targets (phantom tabs, a rename to "", a
        // false logout); it stays raw and is shown as-is.
        if !Self::has_required_params(cmd, &params) {
            return Self::raw_command(cmd, &params);
        }
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
            "WALLCHOPS" => IrcCommand::StatusMessage(
                '@',
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned().unwrap_or_default(),
            ),
            "WALLHOPS" => IrcCommand::StatusMessage(
                '%',
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned().unwrap_or_default(),
            ),
            "WALLVOICES" => IrcCommand::StatusMessage(
                '+',
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned().unwrap_or_default(),
            ),
            "TAGMSG" => IrcCommand::Tagmsg(params.first().cloned().unwrap_or_default()),
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
            "ACCOUNT" => IrcCommand::Account(params.first().cloned().unwrap_or_default()),
            // IRCv3 chghost
            "CHGHOST" => IrcCommand::Chghost(
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned().unwrap_or_default(),
            ),
            "SETNAME" => IrcCommand::Setname(params.first().cloned().unwrap_or_default()),
            "CHATHISTORY"
                if params
                    .first()
                    .is_some_and(|p| p.eq_ignore_ascii_case("TARGETS")) =>
            {
                IrcCommand::ChathistoryTarget(
                    params.get(1).cloned().unwrap_or_default(),
                    params.get(2).cloned().unwrap_or_default(),
                )
            }
            "FAIL" | "WARN" | "NOTE" => {
                let command = params.first().cloned().unwrap_or_default();
                let code = params.get(1).cloned().unwrap_or_default();
                let description = params.last().cloned().unwrap_or_default();
                let context = if params.len() > 3 {
                    params[2..params.len() - 1].to_vec()
                } else {
                    Vec::new()
                };
                IrcCommand::StandardReply {
                    kind: cmd.to_string(),
                    command,
                    code,
                    context,
                    description,
                }
            }
            "MARKREAD" => IrcCommand::Markread(
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned(),
            ),
            "REDACT" => IrcCommand::Redact(
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned().unwrap_or_default(),
                params.get(2).cloned(),
            ),
            "RENAME" => IrcCommand::Rename(
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned().unwrap_or_default(),
                params.get(2).cloned(),
            ),
            "RELOCATE" => IrcCommand::Relocate(
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned().unwrap_or_default(),
                params.get(2).cloned(),
            ),
            "METADATA" | "WEBPUSH" | "BOUNCER" | "PERSISTENCE" | "REGISTER" | "VERIFY"
            | "AUTHTOKEN" | "TOKEN" => IrcCommand::Extension(cmd.to_string(), params),
            // IRCv3 batch
            "BATCH" => IrcCommand::Batch(
                params.first().cloned().unwrap_or_default(),
                params.get(1).cloned(),
                params.get(2).cloned(),
            ),
            _ => {
                // A numeric reply is exactly three ASCII digits. parse::<u16>
                // alone would also take "1", "+001" and "0001" (aliasing
                // RPL_WELCOME) and "1234".
                match cmd.parse::<u16>() {
                    Ok(num) if cmd.len() == 3 && cmd.bytes().all(|b| b.is_ascii_digit()) => {
                        IrcCommand::Numeric(num, params)
                    }
                    _ => Self::raw_command(cmd, &params),
                }
            }
        }
    }

    /// The sender's nickname (or server name): the prefix up to the first '!'
    /// or '@', since the grammar is `nickname [ [ "!" user ] "@" host ]`.
    /// A prefix with no name before the separator has no sender.
    pub fn get_sender_nick(&self) -> Option<String> {
        self.prefix
            .as_deref()
            .and_then(|p| p.split(['!', '@']).next())
            .filter(|nick| !nick.is_empty())
            .map(str::to_string)
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

    /// Nefarious draft/oper-tag. An empty value still identifies a displayed
    /// operator; deployments may optionally include the oper name.
    pub fn get_oper(&self) -> Option<String> {
        self.get_tag("draft/oper").map(|name| {
            if name.is_empty() {
                "operator".into()
            } else {
                name
            }
        })
    }

    /// Reply relation. Nefarious accepts the ratified `+reply` spelling and
    /// the older `+draft/reply` spelling for client compatibility.
    pub fn get_reply(&self) -> Option<String> {
        self.get_tag("+reply")
            .or_else(|| self.get_tag("+draft/reply"))
            .filter(|value| !value.is_empty())
    }

    /// Stable server-assigned identity used for replay merging and redaction.
    pub fn get_msgid(&self) -> Option<String> {
        self.get_tag("msgid").filter(|value| !value.is_empty())
    }

    /// Preserve the canonical wire timestamp as well as its display rendering.
    pub fn get_server_time_raw(&self) -> Option<String> {
        self.get_tag("time")
            .filter(|value| parse_iso8601_utc(value).is_some())
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

impl IrcCommand {
    /// The wire form of this command without its CRLF terminator, or `None`
    /// when any field contains CR, LF or NUL. Display interpolates fields
    /// verbatim, so those bytes would end the line early and let the rest of
    /// the field be read by the server as further protocol commands (#285).
    /// Anything that writes a command to the wire must go through this.
    pub fn to_wire_line(&self) -> Option<String> {
        let line = self.to_string();
        if line.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | 0)) {
            None
        } else {
            Some(line)
        }
    }
}

impl fmt::Display for IrcCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Trailing form so a space or leading ':' in the password survives.
            IrcCommand::Pass(pass) => write!(f, "PASS :{}", pass),
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
            IrcCommand::StatusMessage(status, target, msg) => {
                let command = match status {
                    '@' => "WALLCHOPS",
                    '%' => "WALLHOPS",
                    '+' => "WALLVOICES",
                    _ => "WALLCHOPS",
                };
                write!(f, "{} {} :{}", command, target, msg)
            }
            IrcCommand::Tagmsg(target) => write!(f, "TAGMSG {}", target),
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
            IrcCommand::Setname(realname) => write!(f, "SETNAME :{}", realname),
            IrcCommand::Batch(reference, batch_type, params) => match (batch_type, params) {
                (Some(t), Some(p)) => write!(f, "BATCH {} {} {}", reference, t, p),
                (Some(t), None) => write!(f, "BATCH {} {}", reference, t),
                _ => write!(f, "BATCH {}", reference),
            },
            IrcCommand::ChathistoryLatest(target, reference, limit) => {
                write!(f, "CHATHISTORY LATEST {} {} {}", target, reference, limit)
            }
            IrcCommand::ChathistoryBefore(target, reference, limit) => {
                write!(f, "CHATHISTORY BEFORE {} {} {}", target, reference, limit)
            }
            IrcCommand::ChathistoryAfter(target, reference, limit) => {
                write!(f, "CHATHISTORY AFTER {} {} {}", target, reference, limit)
            }
            IrcCommand::ChathistoryAround(target, reference, limit) => {
                write!(f, "CHATHISTORY AROUND {} {} {}", target, reference, limit)
            }
            IrcCommand::ChathistoryBetween(target, first, second, limit) => write!(
                f,
                "CHATHISTORY BETWEEN {} {} {} {}",
                target, first, second, limit
            ),
            IrcCommand::ChathistoryTargets(first, second, limit) => {
                write!(f, "CHATHISTORY TARGETS {} {} {}", first, second, limit)
            }
            IrcCommand::ChathistoryTarget(target, timestamp) => {
                write!(f, "CHATHISTORY TARGETS {} {}", target, timestamp)
            }
            IrcCommand::StandardReply {
                kind,
                command,
                code,
                context,
                description,
            } => {
                write!(f, "{} {} {}", kind, command, code)?;
                for item in context {
                    write!(f, " {}", item)?;
                }
                write!(f, " :{}", description)
            }
            IrcCommand::Markread(target, timestamp) => match timestamp {
                Some(timestamp) => write!(f, "MARKREAD {} {}", target, timestamp),
                None => write!(f, "MARKREAD {}", target),
            },
            IrcCommand::Redact(target, msgid, reason) => match reason {
                Some(reason) => write!(f, "REDACT {} {} :{}", target, msgid, reason),
                None => write!(f, "REDACT {} {}", target, msgid),
            },
            IrcCommand::Rename(old, new, reason) => match reason {
                Some(reason) => write!(f, "RENAME {} {} :{}", old, new, reason),
                None => write!(f, "RENAME {} {}", old, new),
            },
            IrcCommand::Relocate(old, new, reason) => match reason {
                Some(reason) => write!(f, "RELOCATE {} {} :{}", old, new, reason),
                None => write!(f, "RELOCATE {} {}", old, new),
            },
            IrcCommand::Extension(name, params) => {
                write!(f, "{}", name)?;
                for param in params {
                    write!(f, " {}", param)?;
                }
                Ok(())
            }
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

    #[test]
    fn chathistory_latest_formats_wire_command() {
        assert_eq!(
            IrcCommand::ChathistoryLatest("#history".into(), "*".into(), 100).to_string(),
            "CHATHISTORY LATEST #history * 100"
        );
    }

    #[test]
    fn parses_nefarious_standard_replies_and_status_messages() {
        let fail = IrcMessage::parse(
            ":srv FAIL CHATHISTORY INVALID_TARGET LATEST #secret :No access to target",
        )
        .unwrap();
        assert_eq!(
            fail.command,
            IrcCommand::StandardReply {
                kind: "FAIL".into(),
                command: "CHATHISTORY".into(),
                code: "INVALID_TARGET".into(),
                context: vec!["LATEST".into(), "#secret".into()],
                description: "No access to target".into(),
            }
        );

        let wall = IrcMessage::parse(":alice!u@h WALLHOPS #chat :% staff message").unwrap();
        assert_eq!(
            wall.command,
            IrcCommand::StatusMessage('%', "#chat".into(), "% staff message".into())
        );
    }

    #[test]
    fn preserves_message_identity_tags() {
        let message = IrcMessage::parse(
            "@time=2026-09-12T12:00:00.123Z;msgid=42-A;account=alice;draft/oper=netadmin;+reply=prior :a PRIVMSG #c :hi",
        )
        .unwrap();
        assert_eq!(message.get_msgid().as_deref(), Some("42-A"));
        assert_eq!(
            message.get_server_time_raw().as_deref(),
            Some("2026-09-12T12:00:00.123Z")
        );
        assert_eq!(message.get_account().as_deref(), Some("alice"));
        assert_eq!(message.get_oper().as_deref(), Some("netadmin"));
        assert_eq!(message.get_reply().as_deref(), Some("prior"));
    }

    #[test]
    fn commands_missing_required_parameters_are_not_given_fabricated_values() {
        // #283: a hostile server or bouncer must not be able to mint empty
        // targets, channels, nicks or a bare "logged out" ACCOUNT by omitting
        // the parameters the typed command needs.
        for line in [
            ":evil PRIVMSG",
            ":evil PRIVMSG #c",
            ":evil PRIVMSG :",
            ":evil PRIVMSG : text",
            ":evil NOTICE",
            ":evil NOTICE me",
            ":evil WALLCHOPS #c",
            ":evil TAGMSG",
            ":evil JOIN",
            ":evil JOIN :",
            ":evil PART",
            ":n!u@h NICK",
            ":n!u@h NICK :",
            ":evil TOPIC",
            ":evil KICK",
            ":evil KICK #c",
            ":evil KICK #c :",
            ":evil INVITE me",
            ":evil MODE",
            ":evil CAP",
            ":evil CAP *",
            ":evil CAP * :",
            ":n!u@h ACCOUNT",
            ":n!u@h ACCOUNT :",
            ":n!u@h CHGHOST newuser",
            ":n!u@h SETNAME",
            ":evil BATCH",
            ":evil MARKREAD",
            ":evil REDACT #c",
            ":evil RENAME #old",
            ":evil RELOCATE #old",
            ":evil CHATHISTORY TARGETS #c",
        ] {
            let message = IrcMessage::parse(line).unwrap();
            assert!(
                matches!(message.command, IrcCommand::Raw(_)),
                "{line:?} was typed as {:?}",
                message.command
            );
            // The original line stays available for the server buffer.
            assert_eq!(message.raw, line);
        }

        // Complete lines, including the legitimately empty optional/text
        // parameters, keep their typed form.
        let account = IrcMessage::parse(":n!u@h ACCOUNT *").unwrap();
        assert_eq!(account.command, IrcCommand::Account("*".into()));
        let clear_topic = IrcMessage::parse(":n!u@h TOPIC #c :").unwrap();
        assert_eq!(
            clear_topic.command,
            IrcCommand::Topic("#c".into(), Some(String::new()))
        );
        let join = IrcMessage::parse(":n!u@h JOIN :#c").unwrap();
        assert_eq!(
            join.command,
            IrcCommand::Join("#c".into(), None, None, None)
        );
    }

    #[test]
    fn lines_with_no_command_are_dropped_with_or_without_a_trailing_space() {
        // #304: a tags-only or prefix-only fragment has no command, and one
        // trailing space must not flip it from "dropped" to a Raw(" ") that
        // the GUI shows as a spurious server message.
        for line in [
            "@t=1",
            "@t=1 ",
            "@t=1   ",
            ":srv",
            ":srv ",
            ":srv   ",
            "@t=1 :srv",
            "@t=1 :srv ",
            "   ",
            "",
        ] {
            assert!(
                IrcMessage::parse(line).is_none(),
                "{line:?} parsed as a message"
            );
        }

        // A command that follows is still found after the extra spaces.
        let message = IrcMessage::parse("@t=1  :srv  PING  :x").unwrap();
        assert_eq!(message.command, IrcCommand::Ping("x".into()));
    }

    #[test]
    fn standard_replies_without_a_description_are_not_given_a_fabricated_one() {
        // #302: FAIL/WARN/NOTE need <command> <code> <description>. With only
        // two parameters the code was reused as the description, and with one
        // the command was.
        for line in [
            ":srv FAIL CHATHISTORY INVALID_TARGET",
            ":srv WARN CHATHISTORY",
            ":srv NOTE",
            ":srv FAIL",
            ":srv FAIL * :",
            ":srv FAIL CHATHISTORY : desc",
        ] {
            let message = IrcMessage::parse(line).unwrap();
            assert!(
                matches!(message.command, IrcCommand::Raw(_)),
                "{line:?} was typed as {:?}",
                message.command
            );
            assert_eq!(message.raw, line);
        }

        // The smallest well-formed reply keeps the description, and an empty
        // description is still the description rather than a missing one.
        let note = IrcMessage::parse(":srv NOTE * REHASHED :config reloaded").unwrap();
        assert_eq!(
            note.command,
            IrcCommand::StandardReply {
                kind: "NOTE".into(),
                command: "*".into(),
                code: "REHASHED".into(),
                context: Vec::new(),
                description: "config reloaded".into(),
            }
        );
        let one_context = IrcMessage::parse(":srv WARN CMD CODE ctx :why").unwrap();
        assert_eq!(
            one_context.command,
            IrcCommand::StandardReply {
                kind: "WARN".into(),
                command: "CMD".into(),
                code: "CODE".into(),
                context: vec!["ctx".into()],
                description: "why".into(),
            }
        );
    }

    #[test]
    fn sender_nick_is_the_prefix_up_to_the_first_bang_or_at() {
        // #286: prefix = servername / nickname [ [ "!" user ] "@" host ]
        let nick_of = |line: &str| IrcMessage::parse(line).unwrap().get_sender_nick();
        assert_eq!(
            nick_of(":alice!u@h PRIVMSG #c :hi").as_deref(),
            Some("alice")
        );
        assert_eq!(
            nick_of(":alice@host PRIVMSG #c :hi").as_deref(),
            Some("alice")
        );
        assert_eq!(nick_of(":alice PRIVMSG #c :hi").as_deref(), Some("alice"));
        assert_eq!(
            nick_of(":irc.example.org NOTICE me :hi").as_deref(),
            Some("irc.example.org")
        );
        // No nick at all is no sender, not an empty identity.
        assert_eq!(nick_of(":!u@h PRIVMSG #c :hi"), None);
        assert_eq!(nick_of(":@h PRIVMSG #c :hi"), None);
        assert_eq!(nick_of("PRIVMSG #c :hi"), None);
    }

    #[test]
    fn only_three_digit_tokens_parse_as_numerics() {
        // #284: IRC numerics are exactly three ASCII digits; "1", "+001" and
        // "0001" must not alias RPL_WELCOME and "1234" is not a numeric.
        for canonical in [("001", 1), ("000", 0), ("372", 372), ("999", 999)] {
            let message = IrcMessage::parse(&format!(":srv {} me :hi", canonical.0)).unwrap();
            assert_eq!(
                message.command,
                IrcCommand::Numeric(canonical.1, vec!["me".into(), "hi".into()])
            );
        }
        for token in ["1", "01", "+001", "0001", "1234", "-01", "00１"] {
            let line = format!(":srv {token} me :hi");
            let message = IrcMessage::parse(&line).unwrap();
            assert!(
                matches!(message.command, IrcCommand::Raw(_)),
                "{token:?} was typed as {:?}",
                message.command
            );
            assert_eq!(message.raw, line);
        }
    }

    #[test]
    fn wire_line_refuses_fields_that_would_inject_extra_lines() {
        // #285: the Display form is verbatim, so the wire form must reject
        // CR/LF/NUL in any field rather than let one command become several.
        for command in [
            IrcCommand::Nick("a\r\nJOIN #x".into()),
            IrcCommand::Nick("a\nJOIN #x".into()),
            IrcCommand::Nick("a\rJOIN #x".into()),
            IrcCommand::Nick("a\0b".into()),
            IrcCommand::Privmsg("#c".into(), "hi\r\nPRIVMSG #evil :spam".into()),
            IrcCommand::Privmsg("#c\r\nQUIT".into(), "hi".into()),
            IrcCommand::Pass("pw\r\nOPER x y".into()),
            IrcCommand::User {
                username: "u".into(),
                realname: "r\nQUIT".into(),
            },
            IrcCommand::Mode("#c".into(), Some("+k".into()), vec!["key\nQUIT".into()]),
            IrcCommand::Cap("*".into(), "REQ".into(), vec!["a\r\nQUIT".into()]),
            IrcCommand::Raw("PRIVMSG #c :x\r\nQUIT".into()),
            IrcCommand::Extension("METADATA".into(), vec!["x\nQUIT".into()]),
        ] {
            assert_eq!(command.to_wire_line(), None, "{command:?}");
        }

        assert_eq!(
            IrcCommand::Privmsg("#c".into(), "hi there".into())
                .to_wire_line()
                .as_deref(),
            Some("PRIVMSG #c :hi there")
        );
        assert_eq!(
            IrcCommand::Raw("@label=1 WHOIS alice".into())
                .to_wire_line()
                .as_deref(),
            Some("@label=1 WHOIS alice")
        );
    }

    #[test]
    fn parser_never_panics_on_hostile_input() {
        let alphabet: Vec<char> = vec![
            '@', ':', ';', '=', '\\', ' ', 'P', 'R', 'I', 'V', 'M', 'S', 'G', '0', '1', '#',
            'a', '\r', '\n', '\t', 'é', '\u{1}', '*', '!', '.', '-', 'Z', 'T',
        ];
        let mut state: u64 = 0xfeed_face_1234_5678;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _case in 0..50000 {
            let len = (next() % 40) as usize;
            let input: String = (0..len)
                .map(|_| alphabet[(next() as usize) % alphabet.len()])
                .collect();
            if let Some(msg) = IrcMessage::parse(&input) {
                let _ = msg.get_sender_nick();
                let _ = msg.get_tag("time");
                let _ = msg.get_tag("account");
                let _ = msg.get_server_time();
                let _ = msg.get_msgid();
                // Re-parsing the raw form and the Display form must not panic.
                let _ = IrcMessage::parse(&msg.raw);
                let _ = msg.command.to_wire_line();
                if let Some(wire) = msg.command.to_wire_line() {
                    let _ = IrcMessage::parse(&wire);
                }
            }
            let _ = parse_iso8601_utc(&input);
        }
    }

    #[test]
    fn pass_is_sent_as_a_trailing_parameter() {
        // #231: a space or a leading ':' in the password must survive intact.
        assert_eq!(
            IrcCommand::Pass("secret".into()).to_string(),
            "PASS :secret"
        );
        assert_eq!(
            IrcCommand::Pass("my secret".into()).to_string(),
            "PASS :my secret"
        );
        assert_eq!(IrcCommand::Pass(":abc".into()).to_string(), "PASS ::abc");
    }
}
