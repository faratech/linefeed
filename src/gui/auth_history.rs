//! Keep optional history behind account login, without delaying registration
//! or assuming that a services NOTICE means authentication succeeded.
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use super::{ChatMessage, ConversationId, HistoryPlacement, Session};
use crate::irc::numerics::*;
use crate::irc::{IrcCommand, IrcMessage};

const LOGIN_JOIN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(super) struct AccountHistory {
    required: bool,
    awaiting: bool,
    attempt_started: Option<Instant>,
    /// When the latest login attempt stops holding automatic JOINs. Unlike
    /// `attempt_started` it survives an inconclusive verification probe, so a
    /// network that never emits any account signal cannot hold joins forever.
    login_deadline: Option<Instant>,
    /// Set once a login definitively failed or timed out. Releases only the
    /// deferred JOINs; history keeps waiting for a confirmed login.
    joins_released: bool,
    queued_logins: usize,
    services: HashSet<String>,
    deferred: HashMap<ConversationId, HistoryPlacement>,
    automatic_joins: Vec<AutomaticJoin>,
    probe: Option<AccountProbe>,
    next_probe: u64,
    whois: VecDeque<AccountWhois>,
}

struct AutomaticJoin {
    target: String,
    key: Option<String>,
    owner: Option<ConversationId>,
}

struct AccountProbe {
    request_id: u64,
    started: Instant,
}

pub(super) struct AccountWhois {
    id: u64,
    group: u64,
    server: Option<String>,
    target: String,
    label: Option<String>,
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let value: Vec<_> = value.chars().collect();
    let mut matched = vec![false; value.len() + 1];
    matched[0] = true;
    for ch in pattern.chars() {
        if ch == '*' {
            for index in 1..matched.len() {
                matched[index] |= matched[index - 1];
            }
        } else {
            for index in (1..matched.len()).rev() {
                matched[index] = matched[index - 1] && (ch == '?' || ch == value[index - 1]);
            }
            matched[0] = false;
        }
    }
    matched[value.len()]
}

fn login_service(target: &str, content: &str) -> Option<String> {
    let service = target.split('@').next()?.to_ascii_lowercase();
    if !matches!(
        service.as_str(),
        "nickserv" | "authserv" | "ns" | "as" | "q" | "x"
    ) {
        return None;
    }
    let mut words = content.trim_start_matches(':').split_whitespace();
    let verb = words.next()?;
    if ![
        "AUTH",
        "LOGIN",
        "IDENTIFY",
        "ID",
        "SIDENTIFY",
        "AUTHENTICATE",
    ]
    .iter()
    .any(|candidate| verb.eq_ignore_ascii_case(candidate))
        || words.next().is_none()
    {
        return None;
    }
    Some(match service.as_str() {
        "ns" => "nickserv".into(),
        "as" => "authserv".into(),
        _ => service,
    })
}

fn line_login_service(line: &str, depth: usize) -> Option<String> {
    line_login_service_with_context(line, depth, false)
}

fn line_login_service_with_context(line: &str, depth: usize, wire: bool) -> Option<String> {
    if depth > 4 {
        return None;
    }
    let mut line = line.trim().trim_start_matches('/');
    if line.starts_with('@') {
        line = line.split_once(' ')?.1.trim_start();
    }
    let (command, args) = line.split_once(char::is_whitespace)?;
    let args = args.trim_start();
    if command.eq_ignore_ascii_case("RAW") || command.eq_ignore_ascii_case("QUOTE") {
        return line_login_service_with_context(args, depth + 1, true);
    }
    if command.eq_ignore_ascii_case("LABEL") {
        return line_login_service_with_context(
            args.split_once(char::is_whitespace)?.1,
            depth + 1,
            true,
        );
    }
    let (first, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    match command.to_ascii_uppercase().as_str() {
        // GUI /q opens a query, while a raw IRC Q command addresses the
        // network service. Preserve both meanings at their actual boundary.
        "Q" if wire => login_service(command, args),
        "MSG" | "PRIVMSG" | "NOTICE" | "N" | "QUERY" | "Q" | "SQUERY" => first
            .split(',')
            .find_map(|target| login_service(target, rest)),
        "NS" | "NICKSERV" => login_service("NickServ", args),
        "AS" | "AUTHSERV" => login_service("AuthServ", args),
        "X" => login_service(command, args),
        "AUTH" | "LOGIN" if !rest.is_empty() => Some("authserv".into()),
        "IDENTIFY" | "ID" | "SIDENTIFY" if !args.is_empty() => Some("nickserv".into()),
        _ => None,
    }
}

impl Session {
    pub(super) fn account_login_service(command: &IrcCommand) -> Option<String> {
        match command {
            IrcCommand::Privmsg(target, content) | IrcCommand::Notice(target, content) => target
                .split(',')
                .find_map(|target| login_service(target, content)),
            IrcCommand::Raw(line) => line_login_service_with_context(line, 0, true),
            _ => None,
        }
    }

    pub(super) fn note_account_login_sent(&mut self, service: String) {
        self.invalidate_upload_credentials();
        self.account_history.required = true;
        self.account_history.awaiting = true;
        self.account_history.attempt_started = Some(Instant::now());
        self.account_history.login_deadline = Some(Instant::now() + LOGIN_JOIN_TIMEOUT);
        self.account_history.joins_released = false;
        // Replies to an earlier login's WHOIS remain in the FIFO, but can no
        // longer confirm a later login or consume its verification exchange.
        self.account_history.probe = None;
        self.account_history.services.clear();
        self.account_history.services.insert(service);
    }

    pub(super) fn outgoing_account_whois(&self, command: &IrcCommand) -> Option<Vec<AccountWhois>> {
        let wire = match command {
            IrcCommand::Whois(_) | IrcCommand::Raw(_) => command.to_string(),
            _ => return None,
        };
        let message = IrcMessage::parse(&wire)?;
        let mut words = wire.split_whitespace();
        let mut verb = words.next()?;
        if verb.starts_with('@') {
            verb = words.next()?;
        }
        if !verb.eq_ignore_ascii_case("WHOIS") {
            return None;
        }
        let operands: Vec<_> = words.collect();
        if operands.len() > 2 {
            return None;
        }
        let targets = operands.last()?;
        let server = (operands.len() == 2).then(|| operands[0].to_owned());
        // Each target has its own END marker, including wildcard queries that
        // return no rows for us. Track duplicates separately in wire order.
        let label = message.get_tag("label").filter(|value| !value.is_empty());
        Some(
            targets
                .trim_start_matches(':')
                .split(',')
                .map(|nick| AccountWhois {
                    id: 0,
                    group: 0,
                    server: server.clone(),
                    target: nick.to_owned(),
                    label: label.clone(),
                })
                .collect(),
        )
    }

    pub(super) fn note_account_whois_sent(&mut self, requests: Vec<AccountWhois>) {
        let group = self
            .account_history
            .next_probe
            .checked_add(1)
            .expect("account verification IDs exhausted");
        for mut request in requests {
            self.account_history.next_probe = self
                .account_history
                .next_probe
                .checked_add(1)
                .expect("account verification IDs exhausted");
            request.id = self.account_history.next_probe;
            request.group = group;
            self.account_history.whois.push_back(request);
        }
    }

    pub(super) fn prepare_history_authentication(&mut self, auto_perform: &str) {
        self.account_history.queued_logins = auto_perform
            .lines()
            .filter(|line| line_login_service(line, 0).is_some())
            .count();
    }

    pub(super) fn auto_perform_can_run(&self, line: &str) -> bool {
        line_login_service(line, 0).is_none()
            || (!self.account_history.awaiting && self.account_history.probe.is_none())
    }

    pub(super) fn auto_perform_dequeued(&mut self, line: &str) {
        if line_login_service(line, 0).is_some() {
            self.account_history.queued_logins =
                self.account_history.queued_logins.saturating_sub(1);
        }
    }

    pub(super) fn confirm_history_authentication(&mut self) {
        self.account_history.required = false;
        self.account_history.awaiting = false;
        self.account_history.attempt_started = None;
        self.account_history.login_deadline = None;
        self.account_history.probe = None;
    }

    pub(super) fn reset_history_authentication(&mut self) {
        self.account_history = AccountHistory::default();
    }

    pub(super) fn redirect_automatic_history(
        &mut self,
        survivor: ConversationId,
        retired: ConversationId,
    ) {
        if let Some(placement) = self.account_history.deferred.remove(&retired) {
            self.account_history
                .deferred
                .entry(survivor)
                .or_insert(placement);
        }
        for join in &mut self.account_history.automatic_joins {
            if join.owner == Some(retired) {
                join.owner = Some(survivor);
            }
        }
    }

    pub(super) fn forget_automatic_history(&mut self, id: ConversationId) {
        self.account_history.deferred.remove(&id);
        self.account_history
            .automatic_joins
            .retain(|join| join.owner != Some(id));
    }

    /// Whether automatic JOINs must still wait: for a login that is pending
    /// (and neither definitively failed nor timed out) or still queued.
    fn automatic_joins_blocked(&self) -> bool {
        (self.account_history.required && !self.account_history.joins_released)
            || self.account_history.queued_logins > 0
    }

    /// A login that can no longer succeed (or whose outcome never arrived)
    /// must not strand the automatic JOINs. History is deliberately left
    /// waiting for a positive confirmation.
    fn release_automatic_joins(&mut self, reason: &str) {
        if !self.account_history.required || self.account_history.joins_released {
            return;
        }
        self.account_history.joins_released = true;
        self.add_server_message(ChatMessage::system(&format!(
            "Services login could not be confirmed ({reason}); joining channels anyway"
        )));
    }

    /// Configured and recovery JOINs must not race an asynchronous services
    /// login. Manual JOINs still use the regular transport path immediately.
    pub(super) fn send_automatic_join(&mut self, target: String, key: Option<String>) {
        if self.automatic_joins_blocked() {
            let owner = self.id_for_target(&target);
            self.cancel_automatic_join(&target);
            self.account_history
                .automatic_joins
                .push(AutomaticJoin { target, key, owner });
        } else {
            self.send_join_in_background(target, key);
        }
    }

    /// Current names and keys of the automatic JOINs still held back, so a
    /// connection lost before the login confirms keeps them as recovery intent.
    pub(super) fn deferred_automatic_join_targets(&self) -> Vec<(String, Option<String>)> {
        self.account_history
            .automatic_joins
            .iter()
            .map(|join| {
                let name = join
                    .owner
                    .and_then(|id| self.target_name(id))
                    .unwrap_or(&join.target);
                (name.to_owned(), join.key.clone())
            })
            .collect()
    }

    /// Accepted explicit membership changes supersede a deferred automatic
    /// request, including raw/tagged JOIN and PART forms.
    pub(super) fn cancel_automatic_join(&mut self, target: &str) {
        let support = &self.network_support;
        let conversations = &self.conversations;
        self.account_history.automatic_joins.retain(|join| {
            let current = join
                .owner
                .and_then(|id| conversations.target(id))
                .unwrap_or(&join.target);
            !support.identifiers_equal(current, target)
        });
    }

    pub(super) fn cancel_all_automatic_joins(&mut self) {
        self.account_history.automatic_joins.clear();
    }

    fn flush_authenticated_joins(&mut self) {
        let joins = std::mem::take(&mut self.account_history.automatic_joins);
        let mut sent = HashSet::new();
        for join in joins {
            let target = match join.owner {
                Some(id) => match self.target_name(id) {
                    Some(target) => target.to_owned(),
                    None => continue,
                },
                None => join.target,
            };
            if !sent.insert(self.network_support.canonicalize(&target))
                || self.is_parting(&target)
                || self
                    .channel_key(&target)
                    .and_then(|key| self.conversations.get(&key))
                    .is_some_and(|channel| channel.joined)
            {
                continue;
            }
            self.send_join_in_background(target, join.key);
        }
    }

    pub(super) fn request_automatic_history(
        &mut self,
        target: &str,
        placement: HistoryPlacement,
    ) -> bool {
        if !self.is_connected() || !self.chathistory_enabled || !self.logging_load_history {
            return false;
        }
        let Some(id) = self.id_for_target(target) else {
            return false;
        };
        if self.account_history.required || self.account_history.queued_logins > 0 {
            self.account_history.deferred.entry(id).or_insert(placement);
            return true;
        }
        self.request_latest_history(target, None, placement)
    }

    pub(super) fn flush_authenticated_history(&mut self) {
        if self
            .account_history
            .login_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
            || self
                .account_history
                .attempt_started
                .is_some_and(|started| started.elapsed() >= LOGIN_JOIN_TIMEOUT)
        {
            self.account_history.login_deadline = None;
            if self.account_history.queued_logins == 0 {
                self.release_automatic_joins("timed out");
            }
        }
        if self
            .account_history
            .attempt_started
            .is_some_and(|started| started.elapsed() >= LOGIN_JOIN_TIMEOUT)
            || self
                .account_history
                .probe
                .as_ref()
                .is_some_and(|probe| probe.started.elapsed() >= Duration::from_secs(15))
        {
            self.account_history.probe = None;
            self.account_history.awaiting = false;
            self.account_history.attempt_started = None;
            // History stays deferred until a positive account confirmation.
        }
        let probe_for_old_nick = self.account_history.probe.as_ref().is_some_and(|probe| {
            self.account_history
                .whois
                .iter()
                .find(|request| request.id == probe.request_id)
                .is_some_and(|request| !self.identifiers_equal(&request.target, &self.my_nick))
        });
        if probe_for_old_nick && self.account_history.awaiting {
            self.account_history.probe = None;
            self.request_account_verification();
        }
        if !self.automatic_joins_blocked() && self.is_connected() {
            self.flush_authenticated_joins();
        }
        if self.account_history.required
            || self.account_history.queued_logins > 0
            || !self.is_connected()
            || !self.chathistory_enabled
            || !self.logging_load_history
        {
            return;
        }
        let deferred = std::mem::take(&mut self.account_history.deferred);
        for (id, placement) in deferred {
            let Some(target) = self.target_name(id).map(str::to_owned) else {
                continue;
            };
            if self.is_channel_name(&target)
                && (!self
                    .conversation_channel(id)
                    .is_some_and(|channel| channel.joined)
                    || self.is_parting(&target))
            {
                continue;
            }
            self.request_latest_history(&target, None, placement);
        }
    }

    fn request_account_verification(&mut self) {
        if !(self.account_history.awaiting || self.account_history.required)
            || self.account_history.probe.is_some()
            || !self.is_connected()
        {
            return;
        }
        let label = self
            .enabled_caps
            .contains("labeled-response")
            .then(|| format!("lf-auth-{}", self.account_history.next_probe + 1));
        let nick = self.my_nick.clone();
        let command = if let Some(label) = &label {
            IrcCommand::Raw(format!("@label={label} WHOIS {nick}"))
        } else {
            IrcCommand::Whois(nick.clone())
        };
        if self.send_command(command) {
            self.account_history.awaiting = true;
            self.account_history.probe = Some(AccountProbe {
                request_id: self.account_history.next_probe,
                started: Instant::now(),
            });
        }
    }

    /// Returns true only for replies to the internal account-status probe.
    /// Authentication state never depends on free-form services text.
    pub(super) fn observe_history_authentication(&mut self, message: &IrcMessage) -> bool {
        let response_label = self.response_label(message);
        if let IrcCommand::Numeric(number, params) = &message.command {
            let recipient_is_self = params
                .first()
                .is_some_and(|nick| self.identifiers_equal(nick, &self.my_nick));
            if recipient_is_self {
                use crate::irc::client::AuthenticationStatus;
                if *number == RPL_LOGGEDIN
                    && let Some(account) = params
                        .get(2)
                        .filter(|account| !account.is_empty() && account.as_str() != "*")
                {
                    self.connection_status.authentication = AuthenticationStatus::LoggedIn {
                        account: account.clone(),
                    };
                }
                if *number == 901 {
                    self.connection_status.authentication = AuthenticationStatus::Connected;
                }
                if *number == RPL_SASLSUCCESS
                    && !matches!(
                        self.connection_status.authentication,
                        AuthenticationStatus::LoggedIn { .. }
                    )
                {
                    self.connection_status.authentication = AuthenticationStatus::Authenticated;
                }
                if *number == RPL_LOGGEDIN
                    && params
                        .get(2)
                        .is_some_and(|account| account != "*" && !account.is_empty())
                    || *number == RPL_SASLSUCCESS
                {
                    self.confirm_history_authentication();
                }
                if matches!(*number, ERR_SASLFAIL | ERR_SASLTOOLONG | ERR_SASLABORTED) {
                    self.account_history.awaiting = false;
                    self.release_automatic_joins("SASL failed");
                }
            }
            if recipient_is_self
                && *number == ERR_NOSUCHNICK
                && params.get(1).is_some_and(|target| {
                    self.account_history
                        .services
                        .contains(&target.to_ascii_lowercase())
                })
            {
                self.account_history.awaiting = false;
                self.account_history.attempt_started = None;
                self.account_history.probe = None;
                if self.account_history.queued_logins == 0 {
                    self.release_automatic_joins("services are unavailable");
                }
            }
            if recipient_is_self && *number == ERR_NOSUCHSERVER {
                // A remote WHOIS routing failure ends the entire command,
                // including all comma-separated targets, without any 318.
                let group = self
                    .account_history
                    .whois
                    .iter()
                    .find(|request| {
                        request.server.as_ref().is_some_and(|server| {
                            params
                                .get(1)
                                .is_some_and(|target| self.identifiers_equal(server, target))
                        }) && response_label == request.label
                    })
                    .map(|request| request.group);
                if let Some(group) = group {
                    self.account_history
                        .whois
                        .retain(|request| request.group != group);
                }
                return false;
            }
            let whois_reply = matches!(
                *number,
                RPL_AWAY
                    | RPL_WHOISREGNICK
                    | RPL_WHOISUSER
                    | RPL_WHOISSERVER
                    | RPL_WHOISOPERATOR
                    | RPL_WHOISIDLE
                    | RPL_ENDOFWHOIS
                    | RPL_WHOISCHANNELS
                    | RPL_WHOISSPECIAL
                    | RPL_WHOISACCOUNT
                    | RPL_WHOISBOT
                    | RPL_WHOISACTUALLY
                    | RPL_WHOISMARKS
                    | RPL_WHOISHOST
                    | RPL_WHOISMODES
                    | RPL_WHOISSECURE
                    | RPL_WHOISCERTFP
                    | RPL_WHOISKEYVALUE
                    | ERR_NOSUCHNICK
            );
            let exchange = (recipient_is_self && whois_reply)
                .then(|| {
                    self.account_history.whois.iter().position(|request| {
                        params.get(1).is_some_and(|nick| {
                            let pattern = self.network_support.canonicalize(&request.target);
                            let nick = self.network_support.canonicalize(nick);
                            pattern == nick || wildcard_matches(&pattern, &nick)
                        }) && response_label == request.label
                    })
                })
                .flatten();
            let internal = exchange.is_some_and(|index| {
                self.account_history
                    .probe
                    .as_ref()
                    .is_some_and(|probe| self.account_history.whois[index].id == probe.request_id)
            });
            if internal
                && *number == RPL_WHOISACCOUNT
                && params
                    .get(1)
                    .is_some_and(|nick| self.identifiers_equal(nick, &self.my_nick))
                && params
                    .get(2)
                    .is_some_and(|account| account != "*" && !account.is_empty())
            {
                self.account_history.required = false;
                self.account_history.awaiting = false;
                self.account_history.attempt_started = None;
                self.connection_status.authentication =
                    crate::irc::client::AuthenticationStatus::LoggedIn {
                        account: params[2].clone(),
                    };
            }
            if *number == RPL_ENDOFWHOIS || *number == ERR_NOSUCHNICK {
                if let Some(index) = exchange {
                    self.account_history.whois.remove(index);
                }
                if internal {
                    self.account_history.probe = None;
                    self.account_history.awaiting = false;
                    self.account_history.attempt_started = None;
                }
            }
            // Only the actively correlated probe is hidden. Manual and stale
            // replies remain visible and cannot authenticate a newer attempt.
            return internal;
        }
        match &message.command {
            IrcCommand::Account(account)
                if message
                    .get_sender_nick()
                    .is_some_and(|nick| self.identifiers_equal(&nick, &self.my_nick)) =>
            {
                if account != "*" && !account.is_empty() {
                    self.connection_status.authentication =
                        crate::irc::client::AuthenticationStatus::LoggedIn {
                            account: account.clone(),
                        };
                    self.confirm_history_authentication();
                } else {
                    self.connection_status.authentication =
                        crate::irc::client::AuthenticationStatus::Connected;
                }
            }
            IrcCommand::Notice(_, _)
                if message.get_sender_nick().is_some_and(|nick| {
                    self.account_history
                        .services
                        .contains(&nick.to_ascii_lowercase())
                }) =>
            {
                self.request_account_verification()
            }
            IrcCommand::Mode(target, _, _) if self.identifiers_equal(target, &self.my_nick) => {
                self.request_account_verification()
            }
            _ => {}
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::{Channel, Settings};
    use tokio::sync::mpsc;

    fn app(perform: &str) -> (Session, mpsc::UnboundedReceiver<IrcCommand>) {
        let mut app = Session::with_settings(Settings {
            nickname: "me".into(),
            server_host: "test.example".into(),
            auto_join_channels: "#room".into(),
            auto_perform: perform.into(),
            logging_enabled: false,
            logging_load_history: true,
            ..Settings::default()
        });
        assert!(app.start_session_from_form());
        let (tx, rx) = mpsc::unbounded_channel();
        app.test_set_connecting(tx, None);
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );
        (app, rx)
    }
    fn feed(app: &mut Session, line: &str) {
        app.handle_incoming_message(IrcMessage::parse(line).unwrap());
    }
    fn drain(rx: &mut mpsc::UnboundedReceiver<IrcCommand>) -> Vec<IrcCommand> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }
    fn history(commands: &[IrcCommand]) -> usize {
        commands
            .iter()
            .filter(|command| command.to_string().contains("CHATHISTORY "))
            .count()
    }

    #[test]
    fn automatic_joins_wait_for_configured_service_authentication() {
        let (mut app, mut rx) = app("/msg AuthServ AUTH account example-password");
        feed(&mut app, ":srv 001 me :Welcome");
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|command| matches!(command, IrcCommand::Join(..))),
            "registered-only channel joins must not race services authentication"
        );
        app.run_timed_tasks();
        let commands = drain(&mut rx);
        assert!(commands.iter().any(|command| matches!(command,
            IrcCommand::Privmsg(target, _) if target == "AuthServ")));
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, IrcCommand::Join(..)))
        );
        feed(&mut app, ":srv 900 me me!u@h account :Logged in");
        app.run_timed_tasks();
        assert_eq!(
            drain(&mut rx)
                .iter()
                .filter(|command| matches!(command,
                IrcCommand::Join(target, _, _, _) if target == "#room"))
                .count(),
            1
        );
    }

    #[test]
    fn authenticated_recovery_join_keeps_its_key_without_server_history() {
        let (mut app, mut rx) = app("/ns IDENTIFY example-password");
        app.logging_load_history = false;
        app.chathistory_enabled = false;
        app.ensure_target("#recover");
        app.pending_rejoins = super::super::connection::RecoveryPolicy::ExactMemberships(vec![(
            "#recover".into(),
            Some("saved-key".into()),
        )]);
        feed(&mut app, ":srv 001 me :Welcome");
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|command| matches!(command, IrcCommand::Join(..)))
        );
        app.run_timed_tasks();
        drain(&mut rx);
        feed(&mut app, ":srv 900 me me!u@h account :Logged in");
        app.run_timed_tasks();
        assert!(drain(&mut rx).iter().any(|command| matches!(command,
            IrcCommand::Join(target, key, _, _) if target == "#recover" && key.as_deref() == Some("saved-key"))));
    }

    #[test]
    fn anonymous_automatic_join_remains_immediate() {
        let (mut app, mut rx) = app("");
        feed(&mut app, ":srv 001 me :Welcome");
        assert!(drain(&mut rx).iter().any(|command| matches!(command,
            IrcCommand::Join(target, _, _, _) if target == "#room")));
    }

    #[test]
    fn manual_join_supersedes_an_authentication_deferred_join() {
        let (mut app, mut rx) = app("/ns IDENTIFY example-password");
        feed(&mut app, ":srv 001 me :Welcome");
        drain(&mut rx);
        assert!(app.send_join("#room".into(), Some("manual-key".into())));
        assert_eq!(
            drain(&mut rx).iter().filter(|command| matches!(command,
                IrcCommand::Join(target, key, _, _) if target == "#room" && key.as_deref() == Some("manual-key"))).count(),
            1
        );
        app.run_timed_tasks();
        drain(&mut rx);
        feed(&mut app, ":srv 900 me me!u@h account :Logged in");
        app.run_timed_tasks();
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|command| matches!(command, IrcCommand::Join(..)))
        );
    }

    #[test]
    fn closing_a_retained_tab_cancels_its_authentication_deferred_rejoin() {
        let (mut app, mut rx) = app("/ns IDENTIFY example-password");
        app.ensure_target("#room");
        feed(&mut app, ":srv 001 me :Welcome");
        drain(&mut rx);
        app.remove_channel("#room");
        app.run_timed_tasks();
        drain(&mut rx);
        feed(&mut app, ":srv 900 me me!u@h account :Logged in");
        app.run_timed_tasks();
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|command| matches!(command, IrcCommand::Join(..)))
        );
    }

    #[test]
    fn service_login_holds_channel_and_query_history_until_confirmed() {
        let (mut app, mut rx) = app("/msg AuthServ AUTH account example-password");
        let mut query = Channel::new();
        query.messages.push_back(
            super::super::ChatMessage::new_fmt("alice", "old", "short").with_irc_metadata(
                Some("2026-11-15T00:00:00.000Z".into()),
                Some("old-id".into()),
                None,
            ),
        );
        app.conversations.insert("alice".into(), query);
        feed(&mut app, ":srv 001 me :Welcome");
        feed(&mut app, ":me!u@h JOIN #room");
        assert_eq!(history(&drain(&mut rx)), 0);
        app.run_timed_tasks();
        let commands = drain(&mut rx);
        assert!(commands.iter().any(
            |command| matches!(command, IrcCommand::Privmsg(target, _) if target == "AuthServ")
        ));
        assert_eq!(history(&commands), 0);
        feed(
            &mut app,
            ":srv 900 me me!u@h account :You are now logged in",
        );
        assert_eq!(
            history(&drain(&mut rx)),
            0,
            "drain all incoming messages before issuing deferred work"
        );
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 2);
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 0);
    }

    #[test]
    fn query_and_notice_alias_logins_hold_automatic_history() {
        for perform in [
            "/query AuthServ AUTH account example-password",
            "/q AuthServ AUTH account example-password",
            "/n AuthServ AUTH account example-password",
        ] {
            let (mut app, mut rx) = app(perform);
            feed(&mut app, ":srv 001 me :Welcome");
            feed(&mut app, ":me!u@h JOIN #room");
            assert_eq!(history(&drain(&mut rx)), 0, "{perform}");
            app.run_timed_tasks();
            assert_eq!(history(&drain(&mut rx)), 0, "{perform}");
            feed(&mut app, ":srv 900 me me!u@h account :Logged in");
            app.run_timed_tasks();
            assert_eq!(history(&drain(&mut rx)), 1, "{perform}");
        }
    }

    #[test]
    fn anonymous_history_and_preregistered_sasl_are_not_delayed() {
        for authenticated in [false, true] {
            let (mut app, mut rx) = app("");
            if authenticated {
                feed(&mut app, ":srv 900 me me!u@h account :Logged in");
            }
            feed(&mut app, ":srv 001 me :Welcome");
            drain(&mut rx);
            feed(&mut app, ":me!u@h JOIN #room");
            assert_eq!(history(&drain(&mut rx)), 1);
        }
    }

    #[test]
    fn prior_sasl_success_does_not_bypass_a_later_service_login() {
        let (mut app, mut rx) = app("/ns IDENTIFY example-password");
        feed(&mut app, ":srv 900 me me!u@h first-account :Logged in");
        feed(&mut app, ":srv 001 me :Welcome");
        feed(&mut app, ":me!u@h JOIN #room");
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 0);
        feed(&mut app, ":me!u@h ACCOUNT final-account");
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 1);
    }

    #[test]
    fn service_notice_requires_authoritative_account_verification() {
        let (mut app, mut rx) = app("/as AUTH account example-password");
        feed(&mut app, ":srv 001 me :Welcome");
        feed(&mut app, ":me!u@h JOIN #room");
        app.run_timed_tasks();
        drain(&mut rx);
        feed(
            &mut app,
            ":AuthServ!s@services NOTICE me :Some free-form service reply",
        );
        let commands = drain(&mut rx);
        assert_eq!(history(&commands), 0);
        assert!(
            commands
                .iter()
                .any(|command| matches!(command,IrcCommand::Whois(nick) if nick=="me"))
        );
        feed(&mut app, ":srv 330 me me account :is logged in as");
        feed(&mut app, ":srv 318 me me :End of WHOIS");
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 1);
        assert!(
            !app.server_messages
                .iter()
                .chain(
                    app.conversations
                        .values()
                        .flat_map(|channel| channel.messages.iter())
                )
                .any(|row| row.content.contains("[WHOIS]"))
        );
    }

    #[test]
    fn failure_does_not_release_history_but_a_later_login_can() {
        let (mut app, mut rx) = app("/as AUTH account example-password");
        feed(&mut app, ":srv 001 me :Welcome");
        feed(&mut app, ":me!u@h JOIN #room");
        app.run_timed_tasks();
        drain(&mut rx);
        feed(
            &mut app,
            ":AuthServ!s@services NOTICE me :Authentication failed",
        );
        drain(&mut rx);
        feed(&mut app, ":srv 318 me me :End of WHOIS");
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 0);
        app.process_command("/as AUTH account another-example-password");
        drain(&mut rx);
        feed(&mut app, ":srv 900 me me!u@h account :Logged in");
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 1);
    }

    #[test]
    fn queued_logins_are_serial_and_only_the_final_success_releases_history() {
        let (mut app, mut rx) =
            app("/as AUTH first example-password\n/as AUTH second another-example-password");
        feed(&mut app, ":srv 001 me :Welcome");
        feed(&mut app, ":me!u@h JOIN #room");
        app.run_timed_tasks();
        drain(&mut rx);
        app.run_timed_tasks();
        assert!(drain(&mut rx).is_empty());
        feed(&mut app, ":srv 900 me me!u@h first :Logged in");
        app.run_timed_tasks();
        let commands = drain(&mut rx);
        assert_eq!(history(&commands), 0);
        assert!(
            commands
                .iter()
                .any(|command| command.to_string().contains("AUTH second "))
        );
        feed(&mut app, ":srv 900 me me!u@h second :Logged in");
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 1);
    }

    #[test]
    fn closing_or_parting_before_login_discards_deferred_history() {
        for close in [false, true] {
            let (mut app, mut rx) = app("/as AUTH account example-password");
            feed(&mut app, ":srv 001 me :Welcome");
            feed(&mut app, ":me!u@h JOIN #room");
            app.run_timed_tasks();
            drain(&mut rx);
            if close {
                app.request_manual_disconnect(None);
            } else {
                feed(&mut app, ":srv 900 me me!u@h account :Logged in");
                feed(&mut app, ":me!u@h PART #room");
            }
            drain(&mut rx);
            app.run_timed_tasks();
            assert_eq!(history(&drain(&mut rx)), 0);
        }
    }

    fn pending_login() -> (Session, mpsc::UnboundedReceiver<IrcCommand>) {
        let (mut app, mut rx) = app("/msg AuthServ AUTH account example-password");
        feed(&mut app, ":srv 001 me :Welcome");
        feed(&mut app, ":me!u@h JOIN #room");
        app.run_timed_tasks();
        drain(&mut rx);
        (app, rx)
    }

    #[test]
    fn older_manual_whois_stays_visible_and_cannot_confirm_login() {
        let (mut app, mut rx) = app("/msg AuthServ AUTH account example-password");
        feed(&mut app, ":srv 001 me :Welcome");
        app.process_command("/whois me");
        feed(&mut app, ":me!u@h JOIN #room");
        app.run_timed_tasks();
        drain(&mut rx);
        feed(&mut app, ":AuthServ!s@h NOTICE me :Login reply");
        drain(&mut rx);
        feed(&mut app, ":srv 330 me me previous-account :is logged in as");
        feed(&mut app, ":srv 318 me me :End of WHOIS");
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 0);
        assert!(
            app.server_messages
                .iter()
                .chain(
                    app.conversations
                        .values()
                        .flat_map(|channel| channel.messages.iter())
                )
                .any(|row| row.content.contains("previous-account"))
        );
        feed(&mut app, ":srv 330 me me account :is logged in as");
        feed(&mut app, ":srv 318 me me :End of WHOIS");
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 1);
    }

    #[test]
    fn new_login_invalidates_old_probe_even_with_labeled_response_batches() {
        for labeled in [false, true] {
            let (mut app, mut rx) = pending_login();
            if labeled {
                feed(&mut app, ":srv CAP me ACK :labeled-response");
            }
            feed(&mut app, ":AuthServ!s@h NOTICE me :First login reply");
            drain(&mut rx);
            let first = app.account_history.whois.back().unwrap().label.clone();
            app.process_command("/msg AuthServ AUTH other example-password");
            feed(&mut app, ":AuthServ!s@h NOTICE me :Second login reply");
            drain(&mut rx);
            let second = app.account_history.whois.back().unwrap().label.clone();
            let prefix = first
                .as_ref()
                .map(|label| format!("@label={label} "))
                .unwrap_or_default();
            feed(
                &mut app,
                &format!("{prefix}:srv 330 me me first :is logged in as"),
            );
            feed(&mut app, &format!("{prefix}:srv 318 me me :End of WHOIS"));
            app.run_timed_tasks();
            assert_eq!(history(&drain(&mut rx)), 0);
            if let Some(label) = second {
                feed(
                    &mut app,
                    &format!("@label={label} :srv BATCH +verify labeled-response"),
                );
                feed(
                    &mut app,
                    "@batch=verify :srv 330 me me other :is logged in as",
                );
                feed(&mut app, "@batch=verify :srv 318 me me :End of WHOIS");
                feed(&mut app, ":srv BATCH -verify");
            } else {
                feed(&mut app, ":srv 330 me me other :is logged in as");
                feed(&mut app, ":srv 318 me me :End of WHOIS");
            }
            app.run_timed_tasks();
            assert_eq!(history(&drain(&mut rx)), 1);
        }
    }

    #[test]
    fn completed_wildcard_whois_cannot_block_account_verification() {
        for terminator in ["318", "401"] {
            let (mut app, mut rx) = pending_login();
            app.process_command("/whois alice*");
            feed(
                &mut app,
                &format!(":srv {terminator} me alice* :No more replies"),
            );
            feed(&mut app, ":AuthServ!s@h NOTICE me :Login reply");
            drain(&mut rx);
            feed(&mut app, ":srv 330 me me account :is logged in as");
            feed(&mut app, ":srv 318 me me :End of WHOIS");
            app.run_timed_tasks();
            assert_eq!(history(&drain(&mut rx)), 1);
        }
    }

    #[test]
    fn remote_whois_routing_errors_retire_only_the_failed_command() {
        let (mut app, mut rx) = pending_login();
        app.process_command("/whois no-such-server me,other");
        app.process_command("/whois no-such-server me");
        assert_eq!(app.account_history.whois.len(), 3);
        feed(&mut app, ":srv 402 me no-such-server :No such server");
        assert_eq!(app.account_history.whois.len(), 1);
        assert!(app.account_history.required);
        feed(&mut app, ":srv 402 me no-such-server :No such server");
        assert!(app.account_history.whois.is_empty());
        feed(&mut app, ":AuthServ!s@h NOTICE me :Login reply");
        drain(&mut rx);
        feed(&mut app, ":srv 330 me me account :is logged in as");
        feed(&mut app, ":srv 318 me me :End of WHOIS");
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 1);
        assert!(
            app.server_messages
                .iter()
                .chain(
                    app.conversations
                        .values()
                        .flat_map(|channel| channel.messages.iter())
                )
                .any(|row| row.content.contains("No such server"))
        );
    }

    #[test]
    fn old_nick_probe_cannot_authenticate_the_current_nick() {
        let (mut app, mut rx) = pending_login();
        feed(&mut app, ":AuthServ!s@h NOTICE me :Login reply");
        drain(&mut rx);
        feed(&mut app, ":me!u@h NICK newme");
        feed(&mut app, ":srv 330 newme me somebody-else :is logged in as");
        app.run_timed_tasks();
        let commands = drain(&mut rx);
        assert_eq!(history(&commands), 0);
        assert!(
            commands
                .iter()
                .any(|command| matches!(command,IrcCommand::Whois(nick) if nick=="newme"))
        );
        feed(&mut app, ":srv 330 newme newme account :is logged in as");
        feed(&mut app, ":srv 318 newme newme :End of WHOIS");
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 1);
    }

    #[test]
    fn manual_whois_for_a_later_nickname_cannot_confirm_its_login() {
        let (mut app, mut rx) = pending_login();
        app.process_command("/whois newme");
        feed(&mut app, ":me!u@h NICK newme");
        feed(&mut app, ":AuthServ!s@h NOTICE newme :Login reply");
        drain(&mut rx);
        feed(
            &mut app,
            ":srv 330 newme newme previous-holder :is logged in as",
        );
        feed(&mut app, ":srv 318 newme newme :End of WHOIS");
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 0);
        feed(&mut app, ":srv 330 newme newme account :is logged in as");
        feed(&mut app, ":srv 318 newme newme :End of WHOIS");
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 1);
    }

    #[test]
    fn attempt_timeout_and_missing_service_allow_retry_without_releasing_history() {
        for missing_service in [false, true] {
            let (mut app, mut rx) = app(
                "/msg AuthServ AUTH first example-password\n/msg NickServ IDENTIFY example-password",
            );
            feed(&mut app, ":srv 001 me :Welcome");
            feed(&mut app, ":me!u@h JOIN #room");
            app.run_timed_tasks();
            drain(&mut rx);
            if missing_service {
                feed(&mut app, ":srv 401 me AuthServ :No such nick");
            } else {
                app.account_history.attempt_started =
                    Some(Instant::now() - Duration::from_secs(31));
            }
            app.run_timed_tasks();
            app.run_timed_tasks();
            let commands = drain(&mut rx);
            assert_eq!(history(&commands), 0);
            assert!(commands.iter().any(
                |command| matches!(command,IrcCommand::Privmsg(target,_) if target=="NickServ")
            ));
            feed(&mut app, ":srv 900 me me!u@h account :Logged in");
            app.run_timed_tasks();
            assert_eq!(history(&drain(&mut rx)), 1);
        }
    }

    #[test]
    fn delayed_service_confirmation_after_a_timeout_can_still_release_history() {
        let (mut app, mut rx) = pending_login();
        app.account_history.attempt_started = Some(Instant::now() - Duration::from_secs(31));
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 0);
        feed(&mut app, ":AuthServ!s@h NOTICE me :Delayed login reply");
        let commands = drain(&mut rx);
        assert!(
            commands
                .iter()
                .any(|command| matches!(command,IrcCommand::Whois(nick) if nick=="me"))
        );
        feed(&mut app, ":srv 330 me me account :is logged in as");
        feed(&mut app, ":srv 318 me me :End of WHOIS");
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 1);
    }

    #[test]
    fn historical_account_signals_and_service_notices_cannot_unlock_login() {
        let (mut app, mut rx) = pending_login();
        feed(&mut app, ":srv BATCH +past chathistory #room");
        feed(
            &mut app,
            "@batch=past :AuthServ!s@h NOTICE me :Old login reply",
        );
        feed(
            &mut app,
            "@batch=past :srv 900 me me!u@h account :Old login",
        );
        feed(&mut app, "@batch=past :srv 903 me :Old SASL success");
        feed(&mut app, "@batch=past :me!u@h ACCOUNT account");
        feed(&mut app, ":srv BATCH -past");
        app.run_timed_tasks();
        assert!(drain(&mut rx).is_empty());
        assert!(app.account_history.required);
    }

    #[test]
    fn non_login_sensitive_commands_do_not_gate_history() {
        for line in [
            "/oper operator example-password",
            "/cs IDENTIFY #room channel-key",
            "/ns SET PASSWORD example-password",
            "/msg alice LOGIN whatever",
            // /q is the query command, not a service-Q shortcut.
            "/q AUTH example-password",
        ] {
            assert!(line_login_service(line, 0).is_none(), "{line}");
        }
        for line in [
            "/as AUTH account example-password",
            "/msg NickServ IDENTIFY example-password",
            "/quote PRIVMSG AuthServ :AUTH account example-password",
            "/label x PRIVMSG NickServ :IDENTIFY example-password",
            "/query AuthServ AUTH account example-password",
            "/q AuthServ AUTH account example-password",
            "/n AuthServ AUTH account example-password",
            "/squery AuthServ AUTH account example-password",
            "/raw Q AUTH account example-password",
        ] {
            assert!(line_login_service(line, 0).is_some(), "{line}");
        }
    }

    fn lose_connection_then_reregister(
        app: &mut Session,
        lose: impl FnOnce(&mut Session),
    ) -> mpsc::UnboundedReceiver<IrcCommand> {
        lose(app);
        app.begin_connection_attempt();
        let (tx, rx) = mpsc::unbounded_channel();
        app.test_set_connecting(tx, None);
        feed(app, ":srv 001 me :Welcome");
        rx
    }

    fn joins_of(commands: &[IrcCommand]) -> Vec<String> {
        commands
            .iter()
            .filter_map(|command| match command {
                IrcCommand::Join(target, _, _, _) => Some(target.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn deferred_automatic_joins_survive_a_drop_before_login_confirmation() {
        for explicit in [false, true] {
            let (mut app, mut rx) = app("/ns IDENTIFY example-password");
            feed(&mut app, ":srv 001 me :Welcome");
            assert!(joins_of(&drain(&mut rx)).is_empty());
            assert_eq!(app.account_history.automatic_joins.len(), 1);
            let mut rx2 = lose_connection_then_reregister(&mut app, |app| {
                if explicit {
                    app.request_reconnect_after_close("x");
                    app.test_finish_close();
                } else {
                    app.mark_connection_lost();
                }
            });
            app.run_timed_tasks();
            assert!(joins_of(&drain(&mut rx2)).is_empty());
            feed(&mut app, ":srv 900 me me!u@h account :Logged in");
            app.run_timed_tasks();
            assert_eq!(joins_of(&drain(&mut rx2)), vec!["#room".to_string()]);
        }
    }

    #[test]
    fn deferred_automatic_joins_survive_repeated_drops_and_keep_their_key() {
        let (mut app, mut rx) = app("/ns IDENTIFY example-password");
        app.active_session.as_mut().unwrap().auto_join_channels = "#room secret".into();
        feed(&mut app, ":srv 001 me :Welcome");
        drain(&mut rx);
        let mut rx2 = lose_connection_then_reregister(&mut app, Session::mark_connection_lost);
        let mut rx3 = lose_connection_then_reregister(&mut app, Session::mark_connection_lost);
        drain(&mut rx2);
        app.run_timed_tasks();
        drain(&mut rx3);
        feed(&mut app, ":srv 900 me me!u@h account :Logged in");
        app.run_timed_tasks();
        assert!(drain(&mut rx3).iter().any(|command| matches!(command,
            IrcCommand::Join(target, key, _, _)
                if target == "#room" && key.as_deref() == Some("secret"))));
    }

    #[test]
    fn explicit_part_cancels_a_deferred_join_across_a_drop() {
        let (mut app, mut rx) = app("/ns IDENTIFY example-password");
        feed(&mut app, ":srv 001 me :Welcome");
        drain(&mut rx);
        app.process_command("/part #room");
        let mut rx2 = lose_connection_then_reregister(&mut app, Session::mark_connection_lost);
        app.run_timed_tasks();
        feed(&mut app, ":srv 900 me me!u@h account :Logged in");
        app.run_timed_tasks();
        assert!(joins_of(&drain(&mut rx2)).is_empty());
    }

    fn unconfirmed_login_notice(app: &Session) -> usize {
        app.server_messages
            .iter()
            .filter(|message| message.content.contains("could not be confirmed"))
            .count()
    }

    fn sent_login_awaiting_join() -> (Session, mpsc::UnboundedReceiver<IrcCommand>) {
        let (mut app, mut rx) = app("/msg NickServ IDENTIFY wrongpw");
        feed(&mut app, ":srv 001 me :Welcome");
        app.run_timed_tasks();
        let commands = drain(&mut rx);
        assert!(joins_of(&commands).is_empty());
        assert!(commands.iter().any(
            |command| matches!(command, IrcCommand::Privmsg(target, _) if target == "NickServ")
        ));
        (app, rx)
    }

    #[test]
    fn unconfirmed_login_releases_joins_after_timeout_but_not_history() {
        let (mut app, mut rx) = sent_login_awaiting_join();
        app.run_timed_tasks();
        assert!(joins_of(&drain(&mut rx)).is_empty());
        app.account_history.attempt_started = Some(Instant::now() - Duration::from_secs(31));
        app.run_timed_tasks();
        app.run_timed_tasks();
        let commands = drain(&mut rx);
        assert_eq!(joins_of(&commands), vec!["#room".to_string()]);
        assert_eq!(history(&commands), 0);
        assert!(app.account_history.required);
        assert_eq!(unconfirmed_login_notice(&app), 1);
        // The login can still be confirmed later, which then releases history.
        feed(&mut app, ":me!u@h JOIN #room");
        drain(&mut rx);
        feed(&mut app, ":srv 900 me me!u@h account :Logged in");
        app.run_timed_tasks();
        assert_eq!(history(&drain(&mut rx)), 1);
    }

    #[test]
    fn wrong_password_without_account_numerics_releases_joins_at_the_deadline() {
        let (mut app, mut rx) = sent_login_awaiting_join();
        feed(&mut app, ":NickServ!s@services NOTICE me :Invalid password");
        assert!(
            drain(&mut rx)
                .iter()
                .any(|c| matches!(c, IrcCommand::Whois(_)))
        );
        feed(&mut app, ":srv 318 me me :End of WHOIS");
        app.run_timed_tasks();
        // The probe ended without a confirmation, which clears the attempt
        // timer, but the join deadline still holds.
        assert!(joins_of(&drain(&mut rx)).is_empty());
        app.account_history.login_deadline = Some(Instant::now() - Duration::from_secs(1));
        app.run_timed_tasks();
        assert_eq!(joins_of(&drain(&mut rx)), vec!["#room".to_string()]);
        assert_eq!(unconfirmed_login_notice(&app), 1);
    }

    #[test]
    fn missing_services_release_joins_immediately_but_not_history() {
        let (mut app, mut rx) = sent_login_awaiting_join();
        feed(&mut app, ":srv 401 me NickServ :No such nick");
        app.run_timed_tasks();
        let commands = drain(&mut rx);
        assert_eq!(joins_of(&commands), vec!["#room".to_string()]);
        assert_eq!(history(&commands), 0);
        assert!(app.account_history.required);
        assert_eq!(unconfirmed_login_notice(&app), 1);
    }

    #[test]
    fn failed_login_keeps_the_recovery_key_and_does_not_release_early() {
        let (mut app, mut rx) = app("/ns IDENTIFY example-password");
        app.ensure_target("#recover");
        app.pending_rejoins = super::super::connection::RecoveryPolicy::ExactMemberships(vec![(
            "#recover".into(),
            Some("saved-key".into()),
        )]);
        feed(&mut app, ":srv 001 me :Welcome");
        app.run_timed_tasks();
        drain(&mut rx);
        app.run_timed_tasks();
        assert!(joins_of(&drain(&mut rx)).is_empty());
        feed(&mut app, ":srv 401 me NickServ :No such nick");
        app.run_timed_tasks();
        assert!(drain(&mut rx).iter().any(|command| matches!(command,
            IrcCommand::Join(target, key, _, _)
                if target == "#recover" && key.as_deref() == Some("saved-key"))));
    }

    #[test]
    fn a_failed_login_does_not_release_joins_before_the_next_queued_login() {
        let (mut app, mut rx) = app(
            "/msg AuthServ AUTH first example-password\n/msg NickServ IDENTIFY example-password",
        );
        feed(&mut app, ":srv 001 me :Welcome");
        app.run_timed_tasks();
        drain(&mut rx);
        feed(&mut app, ":srv 401 me AuthServ :No such nick");
        app.run_timed_tasks();
        let commands = drain(&mut rx);
        assert!(commands.iter().any(
            |command| matches!(command, IrcCommand::Privmsg(target, _) if target == "NickServ")
        ));
        assert!(joins_of(&commands).is_empty());
        assert_eq!(unconfirmed_login_notice(&app), 0);
        feed(&mut app, ":srv 401 me NickServ :No such nick");
        app.run_timed_tasks();
        assert_eq!(joins_of(&drain(&mut rx)), vec!["#room".to_string()]);
    }

    const TWO_LOGINS: &str =
        "/ns IDENTIFY first-password\n/msg AuthServ AUTH acct second-password\n/oper u p";

    fn start_with_held_second_login() -> (Session, mpsc::UnboundedReceiver<IrcCommand>) {
        let (mut app, mut rx) = app(TWO_LOGINS);
        feed(&mut app, ":srv 001 me :Welcome");
        app.run_timed_tasks();
        app.run_timed_tasks();
        let commands = drain(&mut rx);
        assert!(commands.iter().any(
            |command| matches!(command, IrcCommand::Privmsg(target, _) if target == "NickServ")
        ));
        assert!(!commands.iter().any(
            |command| matches!(command, IrcCommand::Privmsg(target, _) if target == "AuthServ")
        ));
        assert!(app.pending_auto_perform.is_some());
        (app, rx)
    }

    #[test]
    fn unfinished_auto_perform_does_not_run_on_the_next_connection_attempt() {
        let (mut app, _old) = start_with_held_second_login();
        app.mark_connection_lost();
        app.begin_connection_attempt();
        assert!(app.pending_auto_perform.is_none());
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.test_set_connecting(tx, None);
        app.run_timed_tasks();
        app.run_timed_tasks();
        assert!(drain(&mut rx).is_empty());
        // The new registration rebuilds the queue, and each line runs once.
        feed(&mut app, ":srv 001 me :Welcome");
        let mut commands = Vec::new();
        for _ in 0..3 {
            app.run_timed_tasks();
            commands.extend(drain(&mut rx));
            if app.account_history.awaiting {
                feed(&mut app, ":srv 401 me NickServ :No such nick");
            }
        }
        let count = |service: &str| {
            commands
                .iter()
                .flat_map(|command| match command {
                    IrcCommand::ClientChat { commands, .. } => commands.clone(),
                    other => vec![other.clone()],
                })
                .filter(|command| {
                    matches!(command,
                    IrcCommand::Privmsg(target, _) if target == service)
                })
                .count()
        };
        assert_eq!(count("NickServ"), 1);
        assert_eq!(count("AuthServ"), 1);
    }

    #[test]
    fn unfinished_auto_perform_is_not_drained_while_not_registered() {
        let (mut app, mut rx) = start_with_held_second_login();
        app.mark_connection_lost();
        app.account_history.attempt_started = Some(Instant::now() - Duration::from_secs(31));
        app.run_timed_tasks();
        app.run_timed_tasks();
        assert!(drain(&mut rx).is_empty());
        let (tx, mut connecting_rx) = mpsc::unbounded_channel();
        app.test_set_connecting(tx, None);
        app.account_history = Default::default();
        app.run_timed_tasks();
        assert!(drain(&mut connecting_rx).is_empty());
    }

    #[test]
    fn manual_disconnect_does_not_leave_auto_perform_noise() {
        let (mut app, mut rx) = start_with_held_second_login();
        app.request_manual_disconnect(None);
        drain(&mut rx);
        let before = app.server_messages.len();
        app.run_timed_tasks();
        app.run_timed_tasks();
        assert!(drain(&mut rx).is_empty());
        assert_eq!(app.server_messages.len(), before);
    }
}
