//! Per-session connection phases and ownership of their single worker.

use eframe::egui;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use super::{ChatMessage, Session, SessionConfig};
use crate::irc::client::{AuthenticationStatus, ClientEvent, ConnectionStatus};
use crate::irc::{IrcClient, IrcCommand, IrcMessage};

#[derive(Clone, Default)]
pub(super) enum RecoveryPolicy {
    #[default]
    ConfiguredAutoJoin,
    ExactMemberships(Vec<(String, Option<String>)>),
}

impl RecoveryPolicy {
    pub(super) fn is_none(&self) -> bool {
        matches!(self, Self::ConfiguredAutoJoin)
    }
    #[cfg(test)]
    pub(super) fn is_some(&self) -> bool {
        !self.is_none()
    }
    pub(super) fn as_ref(&self) -> Option<&Vec<(String, Option<String>)>> {
        match self {
            Self::ConfiguredAutoJoin => None,
            Self::ExactMemberships(rows) => Some(rows),
        }
    }
    pub(super) fn as_mut(&mut self) -> Option<&mut Vec<(String, Option<String>)>> {
        match self {
            Self::ConfiguredAutoJoin => None,
            Self::ExactMemberships(rows) => Some(rows),
        }
    }
    pub(super) fn take(&mut self) -> Option<Vec<(String, Option<String>)>> {
        match std::mem::take(self) {
            Self::ConfiguredAutoJoin => None,
            Self::ExactMemberships(rows) => Some(rows),
        }
    }
    pub(super) fn get_or_insert_with(
        &mut self,
        create: impl FnOnce() -> Vec<(String, Option<String>)>,
    ) -> &mut Vec<(String, Option<String>)> {
        if self.is_none() {
            *self = Self::ExactMemberships(create());
        }
        self.as_mut().unwrap()
    }
}

struct ConnectPlan {
    config: SessionConfig,
    attempts: u32,
}

enum WorkerThread {
    Native(JoinHandle<()>),
    #[cfg(test)]
    Simulated,
}
impl WorkerThread {
    fn is_finished(&self) -> bool {
        match self {
            Self::Native(handle) => handle.is_finished(),
            #[cfg(test)]
            Self::Simulated => false,
        }
    }
    fn has_thread(&self) -> bool {
        match self {
            Self::Native(_) => true,
            #[cfg(test)]
            Self::Simulated => false,
        }
    }
    fn join_finished(self) {
        match self {
            Self::Native(handle) => {
                if handle.is_finished() {
                    let _ = handle.join();
                }
            }
            #[cfg(test)]
            Self::Simulated => {}
        }
    }
}

struct Attempt {
    generation: u64,
    outgoing: Option<mpsc::UnboundedSender<IrcCommand>>,
    incoming: mpsc::Receiver<IrcMessage>,
    events: Option<std::sync::mpsc::Receiver<ClientEvent>>,
    worker: WorkerThread,
    attempts: u32,
}

enum AfterClose {
    Stop,
    Connect(Box<ConnectPlan>),
    Retry {
        attempts: u32,
        failed_at: Option<Instant>,
    },
}
struct RetiringAttempt {
    generation: u64,
    incoming: Option<mpsc::Receiver<IrcMessage>>,
    events: Option<std::sync::mpsc::Receiver<ClientEvent>>,
    worker: WorkerThread,
    drain: bool,
    retired_at: Instant,
    next: AfterClose,
}

enum ConnectionPhase {
    Idle,
    Queued(Box<ConnectPlan>),
    Connecting(Attempt),
    Registered(Attempt),
    Closing(RetiringAttempt),
    RetryWait {
        attempts: u32,
        failed_at: Option<Instant>,
    },
}

pub(super) struct ConnectionLifecycle {
    phase: ConnectionPhase,
    next_generation: u64,
}
impl Default for ConnectionLifecycle {
    fn default() -> Self {
        Self {
            phase: ConnectionPhase::Idle,
            next_generation: 1,
        }
    }
}
impl ConnectionLifecycle {
    fn next_generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("connection attempt ID exhausted");
        generation
    }
    pub(super) fn is_connected(&self) -> bool {
        matches!(self.phase, ConnectionPhase::Registered(_))
    }
    pub(super) fn is_connecting(&self) -> bool {
        matches!(
            self.phase,
            ConnectionPhase::Queued(_) | ConnectionPhase::Connecting(_)
        )
    }
    pub(super) fn is_closing(&self) -> bool {
        matches!(self.phase, ConnectionPhase::Closing(_))
    }
    pub(super) fn connection_lost(&self) -> bool {
        matches!(self.phase, ConnectionPhase::RetryWait { .. })
    }
    pub(super) fn has_owner(&self) -> bool {
        matches!(
            self.phase,
            ConnectionPhase::Connecting(_)
                | ConnectionPhase::Registered(_)
                | ConnectionPhase::Closing(_)
        )
    }
    pub(super) fn sender(&self) -> Option<&mpsc::UnboundedSender<IrcCommand>> {
        match &self.phase {
            ConnectionPhase::Connecting(a) | ConnectionPhase::Registered(a) => a.outgoing.as_ref(),
            _ => None,
        }
    }
    pub(super) fn has_worker(&self) -> bool {
        match &self.phase {
            ConnectionPhase::Connecting(a) | ConnectionPhase::Registered(a) => {
                a.worker.has_thread()
            }
            ConnectionPhase::Closing(a) => a.worker.has_thread(),
            _ => false,
        }
    }
    fn worker_finished(&self) -> bool {
        match &self.phase {
            ConnectionPhase::Connecting(a) | ConnectionPhase::Registered(a) => {
                a.worker.is_finished()
            }
            ConnectionPhase::Closing(a) => !a.worker.has_thread() || a.worker.is_finished(),
            _ => false,
        }
    }
    pub(super) fn attempts(&self) -> u32 {
        match &self.phase {
            ConnectionPhase::Queued(plan) => plan.attempts,
            ConnectionPhase::Connecting(a) | ConnectionPhase::Registered(a) => a.attempts,
            ConnectionPhase::RetryWait { attempts, .. } => *attempts,
            ConnectionPhase::Closing(a) => match &a.next {
                AfterClose::Connect(plan) => plan.attempts,
                AfterClose::Retry { attempts, .. } => *attempts,
                AfterClose::Stop => 0,
            },
            ConnectionPhase::Idle => 0,
        }
    }
    pub(super) fn failed_at(&self) -> Option<Instant> {
        match self.phase {
            ConnectionPhase::RetryWait { failed_at, .. } => failed_at,
            _ => None,
        }
    }
    pub(super) fn staged_config(&self) -> Option<&SessionConfig> {
        match &self.phase {
            ConnectionPhase::Closing(a) => match &a.next {
                AfterClose::Connect(plan) => Some(&plan.config),
                AfterClose::Stop | AfterClose::Retry { .. } => None,
            },
            _ => None,
        }
    }
    pub(super) fn staged_config_mut(&mut self) -> Option<&mut SessionConfig> {
        match &mut self.phase {
            ConnectionPhase::Closing(a) => match &mut a.next {
                AfterClose::Connect(plan) => Some(&mut plan.config),
                AfterClose::Stop | AfterClose::Retry { .. } => None,
            },
            _ => None,
        }
    }
    pub(super) fn has_pending_connect(&self) -> bool {
        self.is_connecting() || self.staged_config().is_some()
    }
    fn retry_after_close(&self) -> bool {
        matches!(&self.phase, ConnectionPhase::Closing(a) if matches!(a.next, AfterClose::Retry { .. }))
    }
    pub(super) fn generation(&self) -> Option<u64> {
        match &self.phase {
            ConnectionPhase::Connecting(a) | ConnectionPhase::Registered(a) => Some(a.generation),
            ConnectionPhase::Closing(a) => Some(a.generation),
            _ => None,
        }
    }
    pub(super) fn can_dispatch(&self, generation: u64) -> bool {
        match &self.phase {
            ConnectionPhase::Connecting(a) | ConnectionPhase::Registered(a) => {
                a.generation == generation
            }
            ConnectionPhase::Closing(a) => {
                a.drain && a.generation == generation && a.incoming.is_some()
            }
            _ => false,
        }
    }
    pub(super) fn take_messages(
        &mut self,
        limit: Option<usize>,
    ) -> (Option<u64>, Vec<IrcMessage>, bool) {
        let generation = self.generation();
        let receiver = match &mut self.phase {
            ConnectionPhase::Connecting(a) | ConnectionPhase::Registered(a) => {
                Some(&mut a.incoming)
            }
            ConnectionPhase::Closing(a) if a.drain => a.incoming.as_mut(),
            _ => None,
        };
        let messages: Vec<_> = match receiver {
            Some(rx) => {
                let incoming = std::iter::from_fn(|| rx.try_recv().ok());
                match limit {
                    Some(limit) => incoming.take(limit.saturating_add(1)).collect(),
                    None => incoming.collect(),
                }
            }
            None => Vec::new(),
        };
        let more = limit.is_some_and(|limit| messages.len() > limit);
        (generation, messages, more)
    }
    pub(super) fn accept_registration(&mut self) -> bool {
        let phase = std::mem::replace(&mut self.phase, ConnectionPhase::Idle);
        match phase {
            ConnectionPhase::Connecting(mut a) | ConnectionPhase::Registered(mut a) => {
                a.attempts = 0;
                self.phase = ConnectionPhase::Registered(a);
                true
            }
            other => {
                self.phase = other;
                false
            }
        }
    }
    pub(super) fn clear(&mut self) {
        self.phase = ConnectionPhase::Idle;
    }
    fn queue(&mut self, plan: ConnectPlan) {
        self.phase = ConnectionPhase::Queued(Box::new(plan));
    }
    fn begin_close(&mut self, next: AfterClose, drain: bool, quit_queued: bool) {
        let phase = std::mem::replace(&mut self.phase, ConnectionPhase::Idle);
        match phase {
            ConnectionPhase::Connecting(a) | ConnectionPhase::Registered(a) => {
                drop(a.outgoing);
                self.phase = ConnectionPhase::Closing(RetiringAttempt {
                    generation: a.generation,
                    incoming: quit_queued.then_some(a.incoming),
                    events: a.events,
                    worker: a.worker,
                    drain,
                    retired_at: Instant::now(),
                    next,
                });
            }
            ConnectionPhase::Closing(mut a) => {
                a.next = next;
                a.drain &= drain;
                self.phase = ConnectionPhase::Closing(a);
            }
            _ => {
                if let AfterClose::Connect(plan) = next {
                    self.queue(*plan);
                }
            }
        }
    }
    pub(super) fn release_closing_receiver(&mut self, worker_exited: bool) {
        if let ConnectionPhase::Closing(a) = &mut self.phase
            && (worker_exited || a.retired_at.elapsed() >= Duration::from_secs(2))
        {
            a.incoming = None;
        }
    }
    fn finish_close(&mut self) -> Option<ConnectPlan> {
        let phase = std::mem::replace(&mut self.phase, ConnectionPhase::Idle);
        if let ConnectionPhase::Closing(a) = phase {
            a.worker.join_finished();
            match a.next {
                AfterClose::Connect(plan) => Some(*plan),
                AfterClose::Stop => None,
                AfterClose::Retry {
                    attempts,
                    failed_at,
                } => {
                    self.phase = ConnectionPhase::RetryWait {
                        attempts,
                        failed_at,
                    };
                    None
                }
            }
        } else {
            self.phase = phase;
            None
        }
    }
    pub(super) fn enter_retry_wait(&mut self, now: Option<Instant>) {
        let attempts = self.attempts();
        let old = std::mem::replace(
            &mut self.phase,
            ConnectionPhase::RetryWait {
                attempts,
                failed_at: now,
            },
        );
        match old {
            ConnectionPhase::Connecting(a) | ConnectionPhase::Registered(a) => {
                if a.worker.has_thread() && !a.worker.is_finished() {
                    // Cancellation is signalled by dropping both channels,
                    // but the worker stays owned until it actually exits.
                    drop(a.outgoing);
                    drop(a.incoming);
                    self.phase = ConnectionPhase::Closing(RetiringAttempt {
                        generation: a.generation,
                        incoming: None,
                        events: a.events,
                        worker: a.worker,
                        drain: false,
                        retired_at: Instant::now(),
                        next: AfterClose::Retry {
                            attempts,
                            failed_at: now,
                        },
                    });
                } else {
                    a.worker.join_finished();
                }
            }
            _ => {}
        }
    }
}

impl Session {
    pub(super) fn drain_connection_events(&mut self) {
        let events: Vec<_> = match &self.connection.phase {
            ConnectionPhase::Connecting(attempt) | ConnectionPhase::Registered(attempt) => attempt
                .events
                .as_ref()
                .map(|rx| rx.try_iter().collect())
                .unwrap_or_default(),
            ConnectionPhase::Closing(attempt) => attempt
                .events
                .as_ref()
                .map(|rx| rx.try_iter().collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        for event in events {
            if self.connection.is_closing() && !matches!(event, ClientEvent::ChatWritten { .. }) {
                continue;
            }
            match event {
                ClientEvent::Tls(tls) => self.connection_status.tls = tls,
                ClientEvent::Authentication(authentication) => {
                    if authentication == AuthenticationStatus::Authenticated
                        && matches!(
                            self.connection_status.authentication,
                            AuthenticationStatus::LoggedIn { .. }
                        )
                    {
                        continue;
                    }
                    self.connection_status.authentication = authentication;
                }
                ClientEvent::Registered => {
                    if matches!(
                        self.connection_status.authentication,
                        AuthenticationStatus::Disconnected
                            | AuthenticationStatus::Connecting
                            | AuthenticationStatus::Authenticating
                    ) {
                        self.connection_status.authentication = AuthenticationStatus::Connected;
                    }
                }
                ClientEvent::BouncerReady => self.bouncer_ready = true,
                ClientEvent::SaslSucceeded(mechanism) => {
                    self.connection_status.sasl_mechanism = Some(mechanism)
                }
                ClientEvent::Endpoint {
                    host,
                    port,
                    use_tls,
                } => {
                    self.connection_status.effective_host = Some(host);
                    self.connection_status.effective_port = Some(port);
                    self.connection_status.effective_use_tls = Some(use_tls);
                }
                ClientEvent::ChatWritten { id } => self.chat_written(&id),
                ClientEvent::Fatal(error) => {
                    self.request_manual_disconnect(None);
                    self.add_server_message(ChatMessage::system(&error));
                }
            }
        }
    }
    pub(crate) fn is_connected(&self) -> bool {
        self.connection.is_connected()
    }
    pub(crate) fn is_connecting(&self) -> bool {
        self.connection.is_connecting()
    }
    pub(crate) fn has_transport(&self) -> bool {
        self.connection.sender().is_some()
    }
    pub(crate) fn has_worker(&self) -> bool {
        self.connection.has_worker()
    }
    pub(crate) fn is_fully_stopped(&self) -> bool {
        !self.connection.has_owner() && !self.has_pending_connect() && !self.awaiting_reconnect()
    }
    pub(crate) fn connection_lost(&self) -> bool {
        self.connection.connection_lost()
    }
    #[cfg(test)]
    pub(crate) fn reconnect_attempts(&self) -> u32 {
        self.connection.attempts()
    }
    pub(crate) fn has_pending_connect(&self) -> bool {
        self.connection.has_pending_connect()
            || (self.connection.retry_after_close()
                && self.auto_reconnect
                && self.connection.attempts() < self.max_reconnect_attempts)
    }
    #[cfg(test)]
    pub(crate) fn is_closing(&self) -> bool {
        self.connection.is_closing()
    }

    #[cfg(test)]
    pub(crate) fn background_tick(&mut self, ctx: &egui::Context) {
        self.update_minimal();
        self.process_connection_lifecycle(ctx);
    }

    pub(crate) fn process_connection_lifecycle(&mut self, ctx: &egui::Context) {
        self.drain_connection_events();
        if self.connection.worker_finished() {
            // Complete the old stream before moving its ownership or endpoint.
            self.pump(None);
            self.mark_pending_sends_uncertain();
            if self.connection.is_closing() {
                if let Some(plan) = self.connection.finish_close() {
                    self.activate_session(plan.config.clone());
                    self.connection.queue(plan);
                    self.add_server_message(ChatMessage::system("Previous connection closed"));
                }
            } else {
                let registered = self.is_connected();
                self.mark_connection_lost();
                self.add_server_message(ChatMessage::system(if registered {
                    "Connection lost"
                } else {
                    "Connection failed"
                }));
            }
        }
        self.connection.release_closing_receiver(!self.has_worker());
        if self.connection.is_closing() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if self.should_reconnect() {
            self.start_reconnect();
        }
        if matches!(self.connection.phase, ConnectionPhase::Queued(_)) {
            self.start_connection(ctx.clone());
        }
    }

    pub(super) fn queue_current_connection(&mut self, attempts: u32) -> bool {
        if self.connection.has_owner() {
            return false;
        }
        let Some(config) = self.active_session.clone() else {
            return self.start_session_from_form();
        };
        self.connection.queue(ConnectPlan { config, attempts });
        true
    }

    pub(super) fn connect_validated_session(&mut self, config: SessionConfig, reason: &str) {
        if self.connection.has_owner() {
            self.capture_rejoin_memberships();
            let quit = self.send_command(IrcCommand::Quit(Self::clamp_quit_reason(Some(
                reason.to_string(),
            ))));
            self.connection.begin_close(
                AfterClose::Connect(Box::new(ConnectPlan {
                    config,
                    attempts: 0,
                })),
                true,
                quit,
            );
            self.mark_channel_tabs_unjoined();
        } else {
            self.activate_session(config.clone());
            self.connection.queue(ConnectPlan {
                config,
                attempts: 0,
            });
        }
        self.lag_ms = None;
        self.ping_sent_time = None;
    }

    pub(super) fn close_connection(&mut self, reason: Option<String>, reconnect: bool) {
        let next = if reconnect {
            let Some(config) = self
                .connection
                .staged_config()
                .cloned()
                .or_else(|| self.active_session.clone())
                .or_else(|| self.session_from_form().ok())
            else {
                self.start_session_from_form();
                return;
            };
            self.capture_rejoin_memberships();
            AfterClose::Connect(Box::new(ConnectPlan {
                config,
                attempts: 0,
            }))
        } else {
            AfterClose::Stop
        };
        let had_owner = self.connection.has_owner();
        let quit = self.send_command(IrcCommand::Quit(Self::clamp_quit_reason(reason)));
        self.connection.begin_close(next, reconnect, quit);
        if !had_owner && !reconnect {
            self.connection.clear();
        }
        self.lag_ms = None;
        self.ping_sent_time = None;
        self.mark_channel_tabs_unjoined();
    }

    fn start_connection(&mut self, ctx: egui::Context) {
        let phase = std::mem::replace(&mut self.connection.phase, ConnectionPhase::Idle);
        let ConnectionPhase::Queued(plan) = phase else {
            self.connection.phase = phase;
            return;
        };
        let config = plan.config.server.clone();
        if self
            .blocked_networks
            .contains(&plan.config.logical_network_key())
            || (plan.config.server.bouncer.is_none()
                && self.blocked_endpoints.contains(&plan.config.endpoint_key()))
        {
            self.add_server_message(ChatMessage::system(&format!(
                "Already connected to {} in another window",
                plan.config.endpoint_label()
            )));
            self.events.push(super::SessionEvent::DuplicateEndpoint);
            return;
        }
        let (msg_tx, msg_rx) = mpsc::channel::<IrcMessage>(1000);
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<IrcCommand>();
        self.begin_connection_attempt();
        self.bouncer_ready = false;
        self.connection_status = ConnectionStatus {
            authentication: AuthenticationStatus::Connecting,
            ..ConnectionStatus::default()
        };
        let generation = self.connection.next_generation();
        let worker = std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(error) => {
                    let _ = msg_tx.blocking_send(IrcMessage {
                        tags: None,
                        prefix: None,
                        command: IrcCommand::Notice(
                            "*".to_string(),
                            format!("Failed to start connection runtime: {error}"),
                        ),
                        raw: String::new(),
                    });
                    ctx.request_repaint();
                    return;
                }
            };
            run_worker_runtime(rt, async {
                let mut client = IrcClient::new(config);
                client.set_event_callback(std::sync::Arc::new(move |event| {
                    let _ = event_tx.send(event);
                }));
                let wake_ctx = ctx.clone();
                client.set_wake_callback(std::sync::Arc::new(move || wake_ctx.request_repaint()));
                if let Err(error) = client.connect(msg_tx.clone(), cmd_rx).await {
                    let _ = msg_tx
                        .send(IrcMessage {
                            tags: None,
                            prefix: None,
                            command: IrcCommand::Notice(
                                "*".to_string(),
                                format!("Connection error: {error}"),
                            ),
                            raw: String::new(),
                        })
                        .await;
                }
            });
            ctx.request_repaint();
        });
        self.connection.phase = ConnectionPhase::Connecting(Attempt {
            generation,
            outgoing: Some(cmd_tx),
            incoming: msg_rx,
            events: Some(event_rx),
            worker: WorkerThread::Native(worker),
            attempts: plan.attempts,
        });
    }

    pub(crate) fn join_worker_until(&mut self, deadline: Instant) {
        while self.has_worker() && !self.connection.worker_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.connection.clear();
    }

    #[cfg(test)]
    fn test_install_attempt(
        &mut self,
        tx: mpsc::UnboundedSender<IrcCommand>,
        rx: Option<mpsc::Receiver<IrcMessage>>,
        registered: bool,
    ) {
        let rx = rx.unwrap_or_else(|| mpsc::channel(1).1);
        let attempt = Attempt {
            generation: self.connection.next_generation(),
            outgoing: Some(tx),
            incoming: rx,
            events: None,
            worker: WorkerThread::Simulated,
            attempts: 0,
        };
        self.connection.phase = if registered {
            ConnectionPhase::Registered(attempt)
        } else {
            ConnectionPhase::Connecting(attempt)
        };
    }
    #[cfg(test)]
    pub(crate) fn test_set_registered(&mut self, tx: mpsc::UnboundedSender<IrcCommand>) {
        self.test_install_attempt(tx, None, true);
    }
    #[cfg(test)]
    pub(crate) fn test_set_connecting(
        &mut self,
        tx: mpsc::UnboundedSender<IrcCommand>,
        rx: Option<mpsc::Receiver<IrcMessage>>,
    ) {
        self.test_install_attempt(tx, rx, false);
    }
    #[cfg(test)]
    pub(crate) fn test_set_disconnected(&mut self) {
        self.connection.clear();
    }
    #[cfg(test)]
    pub(crate) fn test_drop_transport(&mut self) {
        match &mut self.connection.phase {
            ConnectionPhase::Connecting(a) | ConnectionPhase::Registered(a) => a.outgoing = None,
            _ => {}
        }
    }
    #[cfg(test)]
    pub(crate) fn test_set_incoming(&mut self, rx: mpsc::Receiver<IrcMessage>) {
        match &mut self.connection.phase {
            ConnectionPhase::Connecting(a) | ConnectionPhase::Registered(a) => a.incoming = rx,
            ConnectionPhase::Closing(a) => a.incoming = Some(rx),
            _ => {
                let (tx, _rx) = mpsc::unbounded_channel();
                self.test_set_connecting(tx, Some(rx));
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn test_attach_worker(&mut self, worker: JoinHandle<()>) {
        match &mut self.connection.phase {
            ConnectionPhase::Connecting(a) | ConnectionPhase::Registered(a) => {
                a.worker = WorkerThread::Native(worker)
            }
            ConnectionPhase::Closing(a) => a.worker = WorkerThread::Native(worker),
            _ => {
                let (tx, _rx) = mpsc::unbounded_channel();
                self.test_set_connecting(tx, None);
                self.test_attach_worker(worker);
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn test_set_retry_state(&mut self, attempts: u32, failed_at: Option<Instant>) {
        self.connection.phase = ConnectionPhase::RetryWait {
            attempts,
            failed_at,
        };
    }
    #[cfg(test)]
    pub(crate) fn test_request_connect(&mut self) {
        assert!(self.start_session_from_form());
    }
    #[cfg(test)]
    pub(crate) fn test_has_closing_receiver(&self) -> bool {
        matches!(&self.connection.phase, ConnectionPhase::Closing(a) if a.incoming.is_some())
    }
    #[cfg(test)]
    pub(crate) fn test_set_sender(&mut self, tx: mpsc::UnboundedSender<IrcCommand>) {
        match &mut self.connection.phase {
            ConnectionPhase::Connecting(a) | ConnectionPhase::Registered(a) => {
                a.outgoing = Some(tx)
            }
            _ => self.test_set_connecting(tx, None),
        }
    }
    #[cfg(test)]
    pub(crate) fn test_set_connected(&mut self, connected: bool) {
        if connected {
            if !self.connection.accept_registration() {
                let (tx, _rx) = mpsc::unbounded_channel();
                self.test_set_registered(tx);
            }
        } else {
            let phase = std::mem::replace(&mut self.connection.phase, ConnectionPhase::Idle);
            self.connection.phase = match phase {
                ConnectionPhase::Registered(a) => ConnectionPhase::Connecting(a),
                other => other,
            };
        }
    }
    #[cfg(test)]
    pub(crate) fn test_set_connecting_flag(&mut self, connecting: bool) {
        if connecting {
            if self.is_connected() {
                self.test_set_connected(false);
            } else if !self.is_connecting() {
                self.test_request_connect();
            }
        } else if matches!(self.connection.phase, ConnectionPhase::Queued(_)) {
            self.connection.clear();
        }
    }
    #[cfg(test)]
    pub(crate) fn test_set_attempts(&mut self, attempts: u32) {
        match &mut self.connection.phase {
            ConnectionPhase::Queued(plan) => plan.attempts = attempts,
            ConnectionPhase::Connecting(a) | ConnectionPhase::Registered(a) => {
                a.attempts = attempts
            }
            ConnectionPhase::RetryWait {
                attempts: current, ..
            } => *current = attempts,
            _ => self.test_set_retry_state(attempts, None),
        }
    }
    #[cfg(test)]
    pub(crate) fn test_set_lost(&mut self, lost: bool) {
        if lost {
            self.test_set_retry_state(self.reconnect_attempts(), None);
        } else if self.connection_lost() {
            self.connection.clear();
        }
    }
    #[cfg(test)]
    pub(crate) fn test_set_failed_at(&mut self, failed_at: Option<Instant>) {
        self.test_set_retry_state(self.reconnect_attempts(), failed_at);
    }
    #[cfg(test)]
    pub(crate) fn test_has_incoming(&self) -> bool {
        match &self.connection.phase {
            ConnectionPhase::Connecting(_) | ConnectionPhase::Registered(_) => true,
            ConnectionPhase::Closing(a) => a.drain && a.incoming.is_some(),
            _ => false,
        }
    }
    #[cfg(test)]
    pub(crate) fn test_worker_finished(&self) -> bool {
        self.connection.worker_finished()
    }
    #[cfg(test)]
    pub(crate) fn test_age_closing_grace(&mut self, duration: Duration) {
        if let ConnectionPhase::Closing(a) = &mut self.connection.phase {
            a.retired_at -= duration;
        }
    }
    #[cfg(test)]
    pub(crate) fn test_finish_close(&mut self) {
        if let Some(plan) = self.connection.finish_close() {
            self.activate_session(plan.config.clone());
            self.connection.queue(plan);
        }
    }
}

fn run_worker_runtime<F: std::future::Future>(rt: tokio::runtime::Runtime, future: F) -> F::Output {
    let output = rt.block_on(future);
    rt.shutdown_background();
    output
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gui::Settings;

    #[test]
    fn background_tick_observes_finished_connection_threads() {
        let handle = std::thread::spawn(|| {});
        while !handle.is_finished() {
            std::thread::yield_now();
        }

        let mut app = Session::with_settings(Settings {
            logging_enabled: false,
            ..Settings::default()
        });
        app.test_set_connected(true);
        app.test_attach_worker(handle);
        app.background_tick(&egui::Context::default());

        assert!(!app.has_worker());
        assert!(app.connection_lost());
        assert!(!app.is_connected());
    }

    /// A plaintext IRC server that completes registration, then records every
    /// line until the client closes the socket (or goes silent).
    pub(crate) fn scripted_server() -> (u16, std::sync::mpsc::Receiver<Vec<String>>) {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut lines = Vec::new();
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let text = line.trim_end().to_string();
                line.clear();
                match text.as_str() {
                    "CAP LS 302" => writer.write_all(b":srv CAP * LS :multi-prefix\r\n"),
                    "CAP REQ :multi-prefix" => {
                        writer.write_all(b":srv CAP * ACK :multi-prefix\r\n")
                    }
                    "CAP END" => writer.write_all(b":srv 001 tester :Welcome\r\n"),
                    _ => Ok(()),
                }
                .unwrap();
                lines.push(text);
            }
            let _ = done_tx.send(lines);
        });
        (port, done_rx)
    }

    fn app_for_port(port: u16) -> Session {
        let mut app = Session::with_settings(Settings {
            server_host: "127.0.0.1".to_string(),
            server_port: port.to_string(),
            use_tls: false,
            nickname: "tester".to_string(),
            logging_enabled: false,
            ..Settings::default()
        });
        app.test_set_connecting_flag(true);
        app
    }

    /// Drive the lifecycle the way the frame loop does until `done` holds.
    fn tick_until(app: &mut Session, what: &str, done: impl Fn(&Session) -> bool) {
        let ctx = egui::Context::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done(app) {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out: {what}; connecting={} connected={} closing={} thread={} messages={:?}",
                app.is_connecting(),
                app.is_connected(),
                app.is_closing(),
                app.has_worker(),
                app.server_messages
                    .iter()
                    .map(|m| m.content.clone())
                    .collect::<Vec<_>>()
            );
            app.background_tick(&ctx);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A stand-in for a live connection worker: alive until `release` is
    /// dropped or sent to, like a real thread still winding its socket down.
    fn parked_worker(
        channel_ends: impl Send + 'static,
    ) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Sender<()>) {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let _channel_ends = channel_ends;
            let _ = release_rx.recv();
        });
        (handle, release_tx)
    }

    /// Release the parked worker, wait for it to be observably finished, then
    /// run one lifecycle pass, as the frame loop would.
    fn finish_worker_and_tick(app: &mut Session, release: std::sync::mpsc::Sender<()>) {
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !app.test_worker_finished() {
            assert!(std::time::Instant::now() < deadline, "worker never exited");
            std::thread::yield_now();
        }
        app.background_tick(&egui::Context::default());
    }

    /// An app that looks connected through a parked worker. The configured
    /// endpoint is an unused loopback port: the tests that follow only
    /// observe whether the lifecycle started another worker, never its result.
    fn app_with_live_worker() -> (Session, std::sync::mpsc::Sender<()>) {
        let mut app = app_for_port(1);
        assert!(app.start_session_from_form());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<IrcCommand>();
        let (msg_tx, msg_rx) = mpsc::channel::<IrcMessage>(8);
        // The parked worker owns the far ends so the channels stay open.
        let (handle, release) = parked_worker((cmd_rx, msg_tx));
        app.test_attach_worker(handle);
        app.test_set_sender(cmd_tx);
        app.test_set_incoming(msg_rx);
        app.test_set_connecting_flag(false);
        app.test_set_connected(true);
        (app, release)
    }

    #[test]
    fn late_welcome_from_the_closing_connection_does_not_cancel_a_server_switch() {
        let (mut app, release) = app_with_live_worker();
        app.test_set_connected(false);
        app.test_set_connecting_flag(true);
        app.apply_unprofiled_endpoint("127.0.0.1", "2", false);
        assert!(app.switch_session_from_form("Changing servers"));

        // Registration finished on the old socket after QUIT was queued.
        app.handle_incoming_message(IrcMessage::parse(":old.example 001 tester :Welcome").unwrap());
        assert!(!app.is_connected());
        assert!(app.is_closing() && app.has_pending_connect());

        finish_worker_and_tick(&mut app, release);

        // The staged endpoint was promoted and connected to, instead of the
        // old one being declared lost and auto-reconnected.
        assert_eq!(app.active_server_config().unwrap().port, 2);
        assert!(app.has_worker());
        assert!(
            !app.server_messages
                .iter()
                .any(|message| message.content.contains("Connection lost"))
        );
    }

    #[test]
    fn connect_right_after_a_manual_disconnect_survives_the_old_workers_exit() {
        let (mut app, release) = app_with_live_worker();
        app.request_manual_disconnect(Some("bye".into()));
        assert!(app.start_session_from_form());

        finish_worker_and_tick(&mut app, release);

        assert!(app.has_worker(), "the connect request was silently dropped");
        assert!(!app.is_closing());
    }

    #[test]
    fn reconnect_right_after_a_manual_disconnect_is_not_reported_as_a_failure() {
        let (mut app, release) = app_with_live_worker();
        app.request_manual_disconnect(Some("bye".into()));
        app.request_reconnect_after_close("Reconnecting");

        finish_worker_and_tick(&mut app, release);

        assert!(app.has_worker());
        assert!(
            !app.server_messages
                .iter()
                .any(|message| message.content.contains("Connection failed"))
        );
    }

    #[test]
    fn a_stale_manual_disconnect_does_not_hide_the_next_connections_failure() {
        // A server that hangs up on every connection, so the worker fails fast.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                drop(stream);
            }
        });

        // Disconnect with no worker left to consume the intent...
        let mut app = app_for_port(port);
        app.test_set_connecting_flag(false);
        app.request_manual_disconnect(None);
        assert!(!app.has_pending_connect());

        // ...then connect: the new worker owns a fresh intent, so its failure
        // is reported and retried like any other.
        assert!(app.start_session_from_form());
        app.background_tick(&egui::Context::default());
        assert!(!app.is_closing());
        tick_until(&mut app, "refused connection to be reported", |l| {
            l.server_messages
                .iter()
                .any(|message| message.content.contains("Connection failed"))
        });
    }

    #[test]
    fn a_server_push_requests_an_immediate_repaint() {
        use std::io::{BufRead, Write};
        // Registration, then one post-registration push.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let text = line.trim_end().to_string();
                line.clear();
                match text.as_str() {
                    "CAP LS 302" => writer.write_all(b":srv CAP * LS :multi-prefix\r\n"),
                    "CAP REQ :multi-prefix" => {
                        writer.write_all(b":srv CAP * ACK :multi-prefix\r\n")
                    }
                    "CAP END" => writer.write_all(
                        b":srv 001 tester :Welcome\r\n:alice!a@h PRIVMSG tester :hello\r\n",
                    ),
                    _ => Ok(()),
                }
                .unwrap();
            }
        });

        let (delay_tx, delay_rx) = std::sync::mpsc::channel();
        let ctx = egui::Context::default();
        ctx.set_request_repaint_callback(move |info| {
            let _ = delay_tx.send(info.delay);
        });
        let mut app = app_for_port(port);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            app.background_tick(&ctx);
            let pushed = app
                .conversations
                .get("alice")
                .is_some_and(|ch| ch.messages.iter().any(|m| m.content == "hello"));
            if pushed {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "push never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let delays: Vec<_> = std::iter::from_fn(|| delay_rx.try_recv().ok()).collect();
        assert!(
            delays.iter().any(|delay| delay.is_zero()),
            "no immediate repaint was requested for the push: {delays:?}"
        );

        // Cleanup: disconnect so the worker ends instead of lingering.
        app.request_manual_disconnect(None);
        tick_until(&mut app, "worker to exit after disconnect", |l| {
            !l.has_worker()
        });
    }

    #[test]
    fn an_exiting_worker_is_polled_until_the_lifecycle_observes_it_finish() {
        let (mut app, release) = app_with_live_worker();
        app.request_manual_disconnect(Some("bye".into()));
        let (delay_tx, delay_rx) = std::sync::mpsc::channel();
        let ctx = egui::Context::default();
        ctx.set_request_repaint_callback(move |info| {
            let _ = delay_tx.send(info.delay);
        });

        // Disconnected and not connecting, but the worker has not exited yet:
        // no other repaint source exists, so the lifecycle must ask for one.
        app.process_connection_lifecycle(&ctx);
        assert!(
            delay_rx
                .try_recv()
                .is_ok_and(|delay| delay <= std::time::Duration::from_millis(100)),
            "no repaint was scheduled for the exiting worker"
        );

        // Once the worker is gone nothing keeps the idle window repainting.
        finish_worker_and_tick(&mut app, release);
        while delay_rx.try_recv().is_ok() {}
        app.process_connection_lifecycle(&ctx);
        assert!(delay_rx.try_recv().is_err());
    }

    #[test]
    fn worker_exit_does_not_wait_for_stuck_blocking_work() {
        // Stands in for a getaddrinfo that never returns.
        let (stuck_tx, stuck_rx) = std::sync::mpsc::channel::<()>();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let worker = std::thread::spawn(move || {
            run_worker_runtime(rt, async move {
                tokio::task::spawn_blocking(move || {
                    let _ = stuck_rx.recv();
                });
                // Give the blocking task time to start.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            });
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !worker.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let finished = worker.is_finished();
        // Release the stuck task either way so a failure does not hang the run.
        drop(stuck_tx);
        assert!(finished, "worker thread blocked on the runtime drop");
    }

    #[test]
    fn manual_disconnect_cancels_a_connection_that_never_registers() {
        // Reads the client's opening line and then says nothing, like a wedged
        // server. Waiting for that line puts the worker in the registration
        // phase, where the QUIT is consumed, not an earlier connect phase.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) > 0 {
                let _ = accepted_tx.send(());
            }
            std::thread::sleep(std::time::Duration::from_secs(30));
        });
        let mut app = app_for_port(port);
        tick_until(&mut app, "registration to start", |_| {
            accepted_rx.try_recv().is_ok()
        });

        // The retired receiver stays open, so cancellation has to come from the
        // queued QUIT alone, long before the registration deadline.
        app.request_manual_disconnect(None);
        assert!(app.test_has_closing_receiver());
        // Wait for the worker without running the lifecycle: its pass drops the
        // retired receiver once the grace period is over, and closing that
        // receiver cancels the worker by itself, which would let this test
        // pass even with QUIT-based cancellation broken.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !app.test_worker_finished() {
            assert!(
                std::time::Instant::now() < deadline,
                "the queued QUIT did not cancel the wedged worker"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            app.test_has_closing_receiver(),
            "the worker only exited once its receiver was dropped"
        );

        // The lifecycle then observes the exit as an ordinary disconnect.
        tick_until(&mut app, "worker exit to be observed", |l| !l.has_worker());
        assert!(!app.is_connecting() && !app.is_connected());
        assert!(!app.test_has_closing_receiver());
    }

    #[test]
    fn manual_disconnect_delivers_quit_before_closing_the_socket() {
        let (port, lines_rx) = scripted_server();
        let mut app = app_for_port(port);
        tick_until(&mut app, "registration", |l| l.is_connected());

        app.request_manual_disconnect(Some("bye now".into()));
        tick_until(&mut app, "worker exit", |l| !l.has_worker());

        let lines = lines_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("server never saw the connection close");
        assert!(
            lines.iter().any(|line| line == "QUIT :bye now"),
            "QUIT was lost on manual disconnect: {lines:?}"
        );
        assert!(!app.is_connected() && !app.is_connecting());
        assert!(!app.test_has_incoming());
    }

    #[test]
    fn reconnect_while_switching_keeps_the_requested_replacement_endpoint() {
        let (mut app, release) = app_with_live_worker();
        app.apply_unprofiled_endpoint("127.0.0.1", "2", false);
        assert!(app.switch_session_from_form("Changing servers"));
        app.request_reconnect_after_close("Retry requested server");
        assert_eq!(app.connection.staged_config().unwrap().server.port, 2);
        assert_eq!(app.update_session().session.unwrap().server.port, 2);
        finish_worker_and_tick(&mut app, release);
        assert_eq!(app.active_server_config().unwrap().port, 2);
        app.request_manual_disconnect(None);
    }

    #[test]
    fn registration_cancellation_stops_dispatching_the_already_drained_batch() {
        let mut app = app_for_port(1);
        let (tx, _rx) = mpsc::unbounded_channel();
        let (incoming, receiver) = mpsc::channel(4);
        app.test_set_connecting(tx, Some(receiver));
        incoming
            .try_send(IrcMessage::parse(":srv 464 tester :Bad password").unwrap())
            .unwrap();
        incoming
            .try_send(IrcMessage::parse(":srv NOTICE * :must not be displayed").unwrap())
            .unwrap();
        app.pump(None);
        assert!(app.is_closing());
        assert!(!app.has_pending_connect());
        assert!(
            app.server_messages
                .iter()
                .any(|message| message.content.contains("Registration failed"))
        );
        assert!(
            app.server_messages
                .iter()
                .all(|message| !message.content.contains("must not be displayed"))
        );
    }

    #[test]
    fn unexpected_loss_keeps_a_finishing_worker_owned_until_it_exits() {
        let (mut app, release) = app_with_live_worker();
        app.mark_connection_lost();
        assert!(app.is_closing() && app.has_worker());
        assert!(!app.has_transport());
        assert!(
            !app.queue_current_connection(1),
            "a second worker cannot replace the retiring owner"
        );
        finish_worker_and_tick(&mut app, release);
        assert!(!app.has_worker());
        assert!(app.connection_lost());
    }
}
