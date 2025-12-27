//! IRC numeric reply codes (RFC 1459, RFC 2812, and common extensions)

#![allow(dead_code)]  // Some constants are defined for completeness but not yet used

// Connection/Welcome replies
pub const RPL_WELCOME: u16 = 1;
pub const RPL_YOURHOST: u16 = 2;
pub const RPL_CREATED: u16 = 3;
pub const RPL_MYINFO: u16 = 4;
pub const RPL_ISUPPORT: u16 = 5;

// Away status
pub const RPL_UNAWAY: u16 = 305;
pub const RPL_NOWAWAY: u16 = 306;

// WHOIS responses
pub const RPL_WHOISUSER: u16 = 311;
pub const RPL_WHOISSERVER: u16 = 312;
pub const RPL_WHOISOPERATOR: u16 = 313;
pub const RPL_WHOISIDLE: u16 = 317;
pub const RPL_ENDOFWHOIS: u16 = 318;
pub const RPL_WHOISCHANNELS: u16 = 319;
pub const RPL_WHOISACCOUNT: u16 = 330;
pub const RPL_WHOISACTUALLY: u16 = 338;
pub const RPL_WHOISSECURE: u16 = 671;  // Non-standard, but widely used

// WHO responses
pub const RPL_WHOREPLY: u16 = 352;
pub const RPL_ENDOFWHO: u16 = 315;

// Channel list
pub const RPL_LISTSTART: u16 = 321;
pub const RPL_LIST: u16 = 322;
pub const RPL_LISTEND: u16 = 323;

// Topic
pub const RPL_TOPIC: u16 = 332;
pub const RPL_TOPICWHOTIME: u16 = 333;

// Names list
pub const RPL_NAMREPLY: u16 = 353;
pub const RPL_ENDOFNAMES: u16 = 366;

// MOTD
pub const RPL_MOTDSTART: u16 = 375;
pub const RPL_MOTD: u16 = 372;
pub const RPL_ENDOFMOTD: u16 = 376;

// Error codes
pub const ERR_NOSUCHNICK: u16 = 401;
pub const ERR_NOSUCHSERVER: u16 = 402;
pub const ERR_NOSUCHCHANNEL: u16 = 403;
pub const ERR_CANNOTSENDTOCHAN: u16 = 404;
pub const ERR_TOOMANYCHANNELS: u16 = 405;
pub const ERR_WASNOSUCHNICK: u16 = 406;
pub const ERR_NOTEXTTOSEND: u16 = 412;
pub const ERR_UNKNOWNCOMMAND: u16 = 421;
pub const ERR_NOMOTD: u16 = 422;
pub const ERR_NONICKNAMEGIVEN: u16 = 431;
pub const ERR_ERRONEUSNICKNAME: u16 = 432;
pub const ERR_NICKNAMEINUSE: u16 = 433;
pub const ERR_NICKCOLLISION: u16 = 436;
pub const ERR_NOTONCHANNEL: u16 = 442;
pub const ERR_USERONCHANNEL: u16 = 443;
pub const ERR_NOTREGISTERED: u16 = 451;
pub const ERR_NEEDMOREPARAMS: u16 = 461;
pub const ERR_ALREADYREGISTERED: u16 = 462;
pub const ERR_PASSWDMISMATCH: u16 = 464;
pub const ERR_YOUREBANNEDCREEP: u16 = 465;
pub const ERR_KEYSET: u16 = 467;
pub const ERR_CHANNELISFULL: u16 = 471;
pub const ERR_UNKNOWNMODE: u16 = 472;
pub const ERR_INVITEONLYCHAN: u16 = 473;
pub const ERR_BANNEDFROMCHAN: u16 = 474;
pub const ERR_BADCHANNELKEY: u16 = 475;
pub const ERR_CHANOPRIVSNEEDED: u16 = 482;

// SASL authentication (IRCv3)
pub const RPL_LOGGEDIN: u16 = 900;
pub const RPL_LOGGEDOUT: u16 = 901;
pub const RPL_NICKLOCKED: u16 = 902;
pub const RPL_SASLSUCCESS: u16 = 903;
pub const ERR_SASLFAIL: u16 = 904;
pub const ERR_SASLTOOLONG: u16 = 905;
pub const ERR_SASLABORTED: u16 = 906;
pub const ERR_SASLALREADY: u16 = 907;
pub const RPL_SASLMECHS: u16 = 908;

/// Categorize a numeric code
pub fn numeric_category(code: u16) -> NumericCategory {
    match code {
        1..=99 => NumericCategory::Connection,
        200..=399 => NumericCategory::Reply,
        400..=599 => NumericCategory::Error,
        600..=699 => NumericCategory::Extended,
        900..=908 => NumericCategory::Sasl,
        _ => NumericCategory::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericCategory {
    Connection,
    Reply,
    Error,
    Extended,
    Sasl,
    Unknown,
}
