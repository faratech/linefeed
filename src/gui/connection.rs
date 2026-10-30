//! Per-session connection worker lifecycle.
//!
//! Each `Session` owns at most one worker thread running `IrcClient::connect`
//! on its own current-thread tokio runtime. The frame loop (and the
//! background tick while hidden/minimized) drives `process_connection_lifecycle`
//! for every session, which observes worker exits, applies the session's
//! `ConnectionIntent`, runs auto-reconnect, and starts workers on request.

use eframe::egui;
use tokio::sync::mpsc;

use super::{ChatMessage, ConnectionIntent, Session};
use crate::irc::{IrcClient, IrcCommand, IrcMessage};

impl Session {
    pub(crate) fn background_tick(&mut self, ctx: &egui::Context) {
        self.update_minimal();
        self.process_connection_lifecycle(ctx);
    }

    pub(crate) fn process_connection_lifecycle(&mut self, ctx: &egui::Context) {
        if self
            .worker
            .as_ref()
            .is_some_and(|handle| handle.is_finished())
        {
            self.worker = None;
            match self.connection_intent {
                ConnectionIntent::ReconnectAfterClose => {
                    self.connection_intent = ConnectionIntent::None;
                    // An explicit endpoint switch is staged while the old
                    // socket closes. Promote it only now so late messages from
                    // the old server cannot be relabelled as the new session.
                    self.promote_pending_session();
                    self.connecting = true;
                    self.connected = false;
                    self.add_server_message(ChatMessage::system("Previous connection closed"));
                }
                ConnectionIntent::ManualDisconnect => {
                    self.connection_intent = ConnectionIntent::None;
                    // request_manual_disconnect already cleared `connecting`,
                    // so a set flag here is a connect requested while this
                    // worker was still exiting: keep it so it starts below.
                    self.connected = false;
                }
                ConnectionIntent::None => {
                    if self.connected {
                        // Connection was lost unexpectedly after registering.
                        self.mark_connection_lost();
                        self.add_server_message(ChatMessage::system("Connection lost"));
                        tracing::warn!("Connection lost, will attempt reconnect");
                    } else if self.connecting {
                        // The connection thread exited before we ever registered.
                        self.connecting = false;
                        self.mark_connection_lost();
                        self.add_server_message(ChatMessage::system("Connection failed"));
                        tracing::warn!("Initial connection failed, will attempt reconnect");
                    }
                }
            }
        }

        self.release_closing_connection(self.worker.is_none());

        // A worker told to close is still exiting. Nothing else schedules a
        // frame to observe it finish (its own repaint request can land before
        // the thread is joinable), and the next connect waits for it, so poll.
        if self.worker.is_some() && !self.connected && !self.connecting {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Auto-reconnect if enabled and connection was lost.
        if self.should_reconnect() {
            self.start_reconnect();
        }

        // Start connection if requested.
        if self.connecting && self.worker.is_none() {
            if !self.ensure_active_session() {
                return;
            }
            self.start_connection(ctx.clone());
        }
    }

    fn start_connection(&mut self, ctx: egui::Context) {
        let Some(config) = self.active_server_config() else {
            self.connecting = false;
            self.show_connect_dialog = true;
            self.add_server_message(ChatMessage::system(
                "Cannot connect without a validated server endpoint",
            ));
            return;
        };
        tracing::info!("Connecting to {}:{}", config.host, config.port);

        let (msg_tx, msg_rx) = mpsc::channel::<IrcMessage>(1000);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<IrcCommand>();

        // The previous worker is gone (connection_thread was None), so nothing
        // it negotiated or left half-received may leak into this socket.
        self.begin_connection_attempt();
        self.msg_rx = Some(msg_rx);
        self.cmd_tx = Some(cmd_tx);

        self.worker = Some(std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    // Surface the failure instead of panicking the worker thread.
                    // The thread then finishes, and update() resolves the stuck
                    // "connecting" state (and triggers reconnect) on the next frame.
                    tracing::error!("Failed to build tokio runtime: {}", e);
                    let _ = msg_tx.blocking_send(IrcMessage {
                        tags: None,
                        prefix: None,
                        command: IrcCommand::Notice(
                            "*".to_string(),
                            format!("Failed to start connection runtime: {}", e),
                        ),
                        raw: String::new(),
                    });
                    return;
                }
            };

            run_worker_runtime(rt, async {
                let mut client = IrcClient::new(config.clone());
                match client.connect(msg_tx.clone(), cmd_rx).await {
                    Ok(_) => tracing::info!("Connection closed"),
                    Err(e) => {
                        tracing::error!("Connection error: {}", e);
                        let _ = msg_tx
                            .send(IrcMessage {
                                tags: None,
                                prefix: None,
                                command: IrcCommand::Notice(
                                    "*".to_string(),
                                    format!("Connection error: {}", e),
                                ),
                                raw: String::new(),
                            })
                            .await;
                    }
                }
            });
            // The thread is done: wake the UI so the lifecycle observes it.
            ctx.request_repaint();
        }));
    }
}

/// Run a connection worker's future to completion, then tear its runtime
/// down. Dropping a tokio `Runtime` waits indefinitely for `spawn_blocking`
/// work, and connecting resolves the host through one (`getaddrinfo`): with a
/// stuck resolver the thread would never finish, and `connection_thread`
/// being set blocks every later connect (#245).
fn run_worker_runtime<F: std::future::Future>(rt: tokio::runtime::Runtime, future: F) -> F::Output {
    let output = rt.block_on(future);
    rt.shutdown_background();
    output
}

#[cfg(test)]
mod tests {
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
        app.connected = true;
        app.worker = Some(handle);
        app.background_tick(&egui::Context::default());

        assert!(app.worker.is_none());
        assert!(app.connection_lost);
        assert!(!app.connected);
    }

    /// A plaintext IRC server that completes registration, then records every
    /// line until the client closes the socket (or goes silent).
    fn scripted_server() -> (u16, std::sync::mpsc::Receiver<Vec<String>>) {
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
        app.connecting = true;
        app
    }

    /// Drive the lifecycle the way the frame loop does until `done` holds.
    fn tick_until(app: &mut Session, what: &str, done: impl Fn(&Session) -> bool) {
        let ctx = egui::Context::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done(app) {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out: {what}; connecting={} connected={} intent={:?} thread={} messages={:?}",
                app.connecting,
                app.connected,
                app.connection_intent,
                app.worker.is_some(),
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
        while !app
            .worker
            .as_ref()
            .is_some_and(|handle| handle.is_finished())
        {
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
        app.worker = Some(handle);
        app.cmd_tx = Some(cmd_tx);
        app.msg_rx = Some(msg_rx);
        app.connecting = false;
        app.connected = true;
        (app, release)
    }

    #[test]
    fn late_welcome_from_the_closing_connection_does_not_cancel_a_server_switch() {
        let (mut app, release) = app_with_live_worker();
        app.connected = false;
        app.connecting = true;
        app.apply_unprofiled_endpoint("127.0.0.1", "2", false);
        assert!(app.switch_session_from_form("Changing servers"));

        // Registration finished on the old socket after QUIT was queued.
        app.handle_incoming_message(IrcMessage::parse(":old.example 001 tester :Welcome").unwrap());
        assert!(!app.connected);
        assert_eq!(app.connection_intent, ConnectionIntent::ReconnectAfterClose);

        finish_worker_and_tick(&mut app, release);

        // The staged endpoint was promoted and connected to, instead of the
        // old one being declared lost and auto-reconnected.
        assert_eq!(app.active_server_config().unwrap().port, 2);
        assert!(app.worker.is_some());
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

        assert!(
            app.worker.is_some(),
            "the connect request was silently dropped"
        );
        assert_eq!(app.connection_intent, ConnectionIntent::None);
    }

    #[test]
    fn reconnect_right_after_a_manual_disconnect_is_not_reported_as_a_failure() {
        let (mut app, release) = app_with_live_worker();
        app.request_manual_disconnect(Some("bye".into()));
        app.request_reconnect_after_close("Reconnecting");

        finish_worker_and_tick(&mut app, release);

        assert!(app.worker.is_some());
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
        app.connecting = false;
        app.request_manual_disconnect(None);
        assert_eq!(app.connection_intent, ConnectionIntent::ManualDisconnect);

        // ...then connect: the new worker owns a fresh intent, so its failure
        // is reported and retried like any other.
        assert!(app.start_session_from_form());
        app.background_tick(&egui::Context::default());
        assert_eq!(app.connection_intent, ConnectionIntent::None);
        tick_until(&mut app, "refused connection to be reported", |l| {
            l.server_messages
                .iter()
                .any(|message| message.content.contains("Connection failed"))
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
        assert!(app.closing_rx.is_some());
        // Wait for the worker without running the lifecycle: its pass drops the
        // retired receiver once the grace period is over, and closing that
        // receiver cancels the worker by itself, which would let this test
        // pass even with QUIT-based cancellation broken.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !app
            .worker
            .as_ref()
            .is_some_and(|handle| handle.is_finished())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the queued QUIT did not cancel the wedged worker"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            app.closing_rx.is_some(),
            "the worker only exited once its receiver was dropped"
        );

        // The lifecycle then observes the exit as an ordinary disconnect.
        tick_until(&mut app, "worker exit to be observed", |l| {
            l.worker.is_none()
        });
        assert!(!app.connecting && !app.connected);
        assert!(app.closing_rx.is_none());
    }

    #[test]
    fn manual_disconnect_delivers_quit_before_closing_the_socket() {
        let (port, lines_rx) = scripted_server();
        let mut app = app_for_port(port);
        tick_until(&mut app, "registration", |l| l.connected);

        app.request_manual_disconnect(Some("bye now".into()));
        tick_until(&mut app, "worker exit", |l| l.worker.is_none());

        let lines = lines_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("server never saw the connection close");
        assert!(
            lines.iter().any(|line| line == "QUIT :bye now"),
            "QUIT was lost on manual disconnect: {lines:?}"
        );
        assert!(!app.connected && !app.connecting);
        assert!(app.msg_rx.is_none());
    }
}
