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
    Join(String),
    Part(String, Option<String>),
    Topic(String, Option<String>),
    Names(Option<String>),
    List(Option<String>),
    Kick(String, String, Option<String>),  // channel, nick, reason
    Invite(String, String),                 // nick, channel

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
    Mode(String, Option<String>, Option<String>),
    Userhost(String),   // space-separated nicks
    Ison(String),       // space-separated nicks

    // Server info
    Time(Option<String>),
    Motd(Option<String>),
    Admin(Option<String>),
    Info(Option<String>),
    Version(Option<String>),  // Server VERSION (not CTCP)
    Lusers,
    Links(Option<String>),
    Stats(String),
    Trace(Option<String>),

    // Numeric replies (server responses)
    Numeric(u16, Vec<String>),

    // CAP negotiation
    Cap(String, Option<String>),

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
        let raw = line.to_string();
        let mut remaining = line.trim();

        // Parse optional tags (@key=value;key2=value2)
        let tags = if remaining.starts_with('@') {
            let end = remaining.find(' ')?;
            let tags = remaining[1..end].to_string();
            remaining = &remaining[end + 1..];
            Some(tags)
        } else {
            None
        };

        // Parse optional prefix (:nick!user@host)
        let prefix = if remaining.starts_with(':') {
            let end = remaining.find(' ')?;
            let prefix = remaining[1..end].to_string();
            remaining = &remaining[end + 1..];
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
        let mut remaining = s;

        while !remaining.is_empty() {
            if remaining.starts_with(':') {
                // Trailing parameter (rest of the line)
                params.push(remaining[1..].to_string());
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
            "PING" => IrcCommand::Ping(params.get(0).cloned().unwrap_or_default()),
            "PONG" => IrcCommand::Pong(params.get(0).cloned().unwrap_or_default()),
            "PRIVMSG" => IrcCommand::Privmsg(
                params.get(0).cloned().unwrap_or_default(),
                params.get(1).cloned().unwrap_or_default(),
            ),
            "NOTICE" => IrcCommand::Notice(
                params.get(0).cloned().unwrap_or_default(),
                params.get(1).cloned().unwrap_or_default(),
            ),
            "JOIN" => IrcCommand::Join(params.get(0).cloned().unwrap_or_default()),
            "PART" => IrcCommand::Part(
                params.get(0).cloned().unwrap_or_default(),
                params.get(1).cloned(),
            ),
            "QUIT" => IrcCommand::Quit(params.get(0).cloned()),
            "AWAY" => IrcCommand::Away(params.get(0).cloned()),
            "NICK" => IrcCommand::Nick(params.get(0).cloned().unwrap_or_default()),
            "TOPIC" => IrcCommand::Topic(
                params.get(0).cloned().unwrap_or_default(),
                params.get(1).cloned(),
            ),
            "KICK" => IrcCommand::Kick(
                params.get(0).cloned().unwrap_or_default(),
                params.get(1).cloned().unwrap_or_default(),
                params.get(2).cloned(),
            ),
            "MODE" => IrcCommand::Mode(
                params.get(0).cloned().unwrap_or_default(),
                params.get(1).cloned(),
                params.get(2).cloned(),
            ),
            "CAP" => IrcCommand::Cap(
                params.get(0).cloned().unwrap_or_default(),
                params.get(1).cloned(),
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
        self.prefix.as_ref().map(|p| {
            p.split('!').next().unwrap_or(p).to_string()
        })
    }

    /// Get a specific IRCv3 tag value
    #[allow(dead_code)]
    pub fn get_tag(&self, key: &str) -> Option<String> {
        self.tags.as_ref().and_then(|tags| {
            for part in tags.split(';') {
                if let Some((k, v)) = part.split_once('=') {
                    if k == key {
                        return Some(v.to_string());
                    }
                } else if part == key {
                    return Some(String::new());
                }
            }
            None
        })
    }
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
            IrcCommand::Join(channel) => write!(f, "JOIN {}", channel),
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
            IrcCommand::Mode(target, mode, param) => {
                if let Some(m) = mode {
                    if let Some(p) = param {
                        write!(f, "MODE {} {} {}", target, m, p)
                    } else {
                        write!(f, "MODE {} {}", target, m)
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
            IrcCommand::Cap(sub, param) => {
                if let Some(p) = param {
                    write!(f, "CAP {} :{}", sub, p)
                } else {
                    write!(f, "CAP {}", sub)
                }
            }
            IrcCommand::Numeric(num, params) => {
                write!(f, "{:03} {}", num, params.join(" "))
            }
            IrcCommand::Raw(raw) => write!(f, "{}", raw),
        }
    }
}
