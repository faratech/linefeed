//! Keep optional history behind account login, without delaying registration
//! or assuming that a services NOTICE means authentication succeeded.
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use super::{ConversationId, HistoryPlacement, Session};
use crate::irc::numerics::*;
use crate::irc::{IrcCommand, IrcMessage};

#[derive(Default)]
pub(super) struct AccountHistory {
    required: bool,
    awaiting: bool,
    attempt_started: Option<Instant>,
    queued_logins: usize,
    services: HashSet<String>,
    deferred: HashMap<ConversationId, HistoryPlacement>,
    probe: Option<AccountProbe>,
    next_probe: u64,
    whois: VecDeque<AccountWhois>,
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
    }

    pub(super) fn forget_automatic_history(&mut self, id: ConversationId) {
        self.account_history.deferred.remove(&id);
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
            .attempt_started
            .is_some_and(|started| started.elapsed() >= Duration::from_secs(30))
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
}
