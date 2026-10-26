#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod gui;
mod icon_data;
mod irc;
mod opener;
mod updater;

#[cfg(windows)]
mod systray;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use eframe::egui;
use std::sync::Arc;
#[cfg(windows)]
use std::sync::OnceLock;
use tokio::sync::mpsc;

use gui::ConnectionIntent;
use gui::{ChatMessage, IrcApp};
use irc::{IrcClient, IrcCommand, IrcMessage};

#[cfg(windows)]
static SINGLE_INSTANCE_MUTEX: OnceLock<isize> = OnceLock::new();

fn load_icon() -> egui::IconData {
    egui::IconData {
        rgba: icon_data::ICON_RGBA.to_vec(),
        width: icon_data::ICON_WIDTH,
        height: icon_data::ICON_HEIGHT,
    }
}

/// Load system fonts including Unicode block/box drawing and emoji support
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // Fonts with good Unicode coverage (block elements, box drawing, symbols)
    // These contain characters like ▄ █ ▓ ▒ ░ ─ │ etc.
    let symbol_font_paths: &[&str] = if cfg!(windows) {
        &[
            "C:\\Windows\\Fonts\\seguisym.ttf", // Segoe UI Symbol (best coverage)
            "C:\\Windows\\Fonts\\consola.ttf",  // Consolas (good monospace coverage)
            "C:\\Windows\\Fonts\\segoeui.ttf",  // Segoe UI
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "/System/Library/Fonts/Menlo.ttc",
            "/System/Library/Fonts/Monaco.ttf",
        ]
    } else {
        // Linux
        &[
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/unifont/unifont.ttf",
            "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
        ]
    };

    // Load symbol font for block/box drawing characters
    for path in symbol_font_paths {
        if let Ok(font_data) = std::fs::read(path) {
            fonts.font_data.insert(
                "symbols".to_owned(),
                Arc::new(egui::FontData::from_owned(font_data)),
            );

            // Add as fallback for all font families
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                if let Some(fonts_for_family) = fonts.families.get_mut(&family) {
                    fonts_for_family.push("symbols".to_owned());
                }
            }

            tracing::info!("Loaded symbol font from {}", path);
            break;
        }
    }

    // Emoji fonts (separate from symbol fonts)
    let emoji_font_paths: &[&str] = if cfg!(windows) {
        &[
            "C:\\Windows\\Fonts\\seguiemj.ttf", // Segoe UI Emoji
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "/System/Library/Fonts/Apple Color Emoji.ttc",
            "/Library/Fonts/Apple Color Emoji.ttc",
        ]
    } else {
        // Linux
        &[
            "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
            "/usr/share/fonts/noto-emoji/NotoColorEmoji.ttf",
            "/usr/share/fonts/google-noto-emoji/NotoColorEmoji.ttf",
        ]
    };

    for path in emoji_font_paths {
        if let Ok(font_data) = std::fs::read(path) {
            fonts.font_data.insert(
                "emoji".to_owned(),
                Arc::new(egui::FontData::from_owned(font_data)),
            );

            // Add emoji font as fallback for all font families
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                if let Some(fonts_for_family) = fonts.families.get_mut(&family) {
                    fonts_for_family.push("emoji".to_owned());
                }
            }

            tracing::info!("Loaded emoji font from {}", path);
            break;
        }
    }

    ctx.set_fonts(fonts);
}

/// Enable Windows Efficiency Mode (EcoQoS) for the current process
/// This reduces CPU/battery usage by lowering priority and enabling power throttling
#[cfg(windows)]
fn enable_efficiency_mode() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetProcessInformation,
    };

    unsafe {
        let handle = GetCurrentProcess();

        // Note: deliberately no SetPriorityClass(IDLE_PRIORITY_CLASS) here.
        // Idle priority starves the UI and the IRC connection thread whenever
        // anything else saturates the CPU (frozen UI, delayed PONGs leading to
        // server ping timeouts). EcoQoS below provides the power savings
        // without starvation.

        // Enable EcoQoS power throttling
        let mut throttle_state = PROCESS_POWER_THROTTLING_STATE {
            Version: 1,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        };

        let _ = SetProcessInformation(
            handle,
            ProcessPowerThrottling,
            &mut throttle_state as *mut _ as *mut _,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
    tracing::info!("Efficiency mode enabled (EcoQoS)");
}

#[cfg(not(windows))]
fn enable_efficiency_mode() {
    // No-op on non-Windows platforms
}

/// Check if another instance is already running (Windows only)
/// Returns true if this is the only instance, false if another exists
#[cfg(windows)]
fn ensure_single_instance() -> bool {
    use windows::Win32::Foundation::GetLastError;
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::w;

    unsafe {
        if SINGLE_INSTANCE_MUTEX.get().is_some() {
            return true;
        }

        // Try to create a named mutex
        let mutex = match CreateMutexW(None, true, w!("Linefeed_SingleInstance")) {
            Ok(handle) => handle,
            Err(err) => {
                tracing::warn!("Failed to create single-instance mutex: {err}");
                return true;
            }
        };

        // If ERROR_ALREADY_EXISTS, another instance has the mutex
        if GetLastError().is_err() {
            // Another instance exists - send it a message to show itself
            systray::activate_existing_instance();
            return false;
        }

        // We got the mutex, we're the only instance
        let _ = SINGLE_INSTANCE_MUTEX.set(mutex.0 as isize);
        true
    }
}

#[cfg(not(windows))]
fn ensure_single_instance() -> bool {
    true // No single-instance check on non-Windows
}

/// Whether this process can show windows (Windows only). Session 0 and other
/// non-interactive window stations (services, headless launch harnesses such
/// as winget's validation) have no visible desktop, so wgpu cannot configure
/// a surface and eframe would panic.
#[cfg(windows)]
fn has_interactive_desktop() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
    };
    use windows::Win32::UI::WindowsAndMessaging::WSF_VISIBLE;

    unsafe {
        // If the station cannot be queried, assume a desktop and let eframe
        // report any real failure.
        let Ok(station) = GetProcessWindowStation() else {
            return true;
        };
        let mut flags = USEROBJECTFLAGS::default();
        let queried = GetUserObjectInformationW(
            HANDLE(station.0),
            UOI_FLAGS,
            Some((&raw mut flags).cast()),
            size_of::<USEROBJECTFLAGS>() as u32,
            None,
        );
        queried.is_err() || flags.dwFlags & WSF_VISIBLE as u32 != 0
    }
}

#[cfg(not(windows))]
fn has_interactive_desktop() -> bool {
    true
}

/// Whether startup may proceed past the single-instance gate (issue #199).
/// A normal launch checks before updater recovery, which can otherwise block
/// on the transaction lock or downgrade a `Prepared` transaction. A supervised
/// update launch defers the check until after recovery: the outgoing instance
/// may still hold the mutex while its replacement validates the handoff.
fn single_instance_allows_startup(supervised: bool) -> bool {
    supervised || ensure_single_instance()
}

fn main() -> eframe::Result<()> {
    if let Some(result) = updater::install::early_mode() {
        if let Err(error) = &result {
            eprintln!("Linefeed updater: {error}");
        }
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }
    // A GUI launched without a desktop has nothing to show: say so and exit
    // cleanly instead of panicking in surface setup. Updater modes above stay
    // headless-capable.
    if !has_interactive_desktop() {
        eprintln!("Linefeed is a desktop application and needs an interactive session.");
        return Ok(());
    }
    let supervised =
        updater::install::supervised_args().map_err(|e| eframe::Error::AppCreation(Box::new(e)))?;
    // Second launches activate the running instance before updater recovery
    // can stall on its lock or touch a pending transaction (issue #199).
    let startup_supervised = supervised.is_some();
    if !single_instance_allows_startup(startup_supervised) {
        return Ok(());
    }
    let startup = updater::install::startup(supervised)
        .map_err(|e| eframe::Error::AppCreation(Box::new(e)))?;
    let updater::install::Startup {
        paths,
        running: _running,
        acknowledgment,
        snapshot,
        update_error,
    } = startup;
    let shutdown = Arc::new(std::sync::Mutex::new(None::<updater::install::Armed>));
    let app_shutdown = shutdown.clone();
    // Supervised launches deferred this check until after recovery above;
    // normal launches already passed it before recovery (issue #199).
    if startup_supervised && !ensure_single_instance() {
        // Another instance is running, exit silently
        return Ok(());
    }

    // INFO by default: DEBUG logs every raw IRC line to stdout, which both
    // costs a formatting pass per message and lands in terminal scrollback /
    // session journals (credentials on those lines are additionally redacted
    // in the writer task as defense in depth).
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();

    tracing::info!("Starting Linefeed");

    // Enable Efficiency Mode to reduce CPU/battery usage
    enable_efficiency_mode();

    // Create tray icon (Windows only)
    #[cfg(windows)]
    {
        systray::create_tray_icon();
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 768.0])
            .with_min_inner_size([640.0, 480.0])
            .with_title("Linefeed")
            .with_icon(Arc::new(load_icon())),
        ..Default::default()
    };

    let result = eframe::run_native(
        "Linefeed",
        options,
        Box::new(move |cc| {
            // Load emoji fonts for comprehensive Unicode support
            setup_fonts(&cc.egui_ctx);

            // Reduce texture memory by discarding image data after GPU upload
            cc.egui_ctx.options_mut(|opts| {
                opts.reduce_texture_memory = true;
            });

            let mut app = IrcApp::new(cc);
            if let Some(snapshot) = snapshot {
                app.restore_update_session(snapshot);
            }
            app.updater.install_reason = update_error;
            app.updater
                .start(paths, cc.egui_ctx.clone(), app.automatic_updates);
            let mut style = (*cc.egui_ctx.global_style()).clone();
            style.spacing.item_spacing = egui::vec2(8.0, 4.0);
            cc.egui_ctx.set_global_style(style);
            gui::apply_font_size(&cc.egui_ctx, app.font_size);

            Ok(Box::new(LinefeedApp {
                app,
                connection_thread: None,
                last_message_time: std::time::Instant::now(),
                window_size_ok: false,
                list_loading_since: None,
                startup_ack: acknowledgment,
                shutdown: app_shutdown,
            }))
        }),
    );
    if result.is_ok()
        && let Ok(mut shutdown) = shutdown.lock()
        && let Some(armed) = shutdown.take()
    {
        armed.commit();
    }
    result
}

struct LinefeedApp {
    startup_ack: Option<updater::install::Transaction>,
    shutdown: Arc<std::sync::Mutex<Option<updater::install::Armed>>>,
    app: IrcApp,
    connection_thread: Option<std::thread::JoinHandle<()>>,
    /// Track when we last received messages (for adaptive repaint intervals)
    last_message_time: std::time::Instant,
    /// Set once the window has been confirmed at a sane size; guards the
    /// startup tiny-window self-heal (see `ui`).
    window_size_ok: bool,
    /// When the current /list started, so a server that never sends
    /// RPL_LISTEND cannot pin the fast repaint rate forever.
    list_loading_since: Option<std::time::Instant>,
}

impl eframe::App for LinefeedApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.app.updater.poll();
        // UI callback proves eframe's native renderer initialized. This must
        // run before hidden/minimized early returns too.
        if let Some(tx) = self.startup_ack.take() {
            std::thread::spawn(move || {
                if let Err(e) = updater::install::confirm_startup(&tx) {
                    tracing::warn!("Could not confirm update startup: {e}");
                }
            });
        }
        if std::mem::take(&mut self.app.updater.restart_requested) {
            let snapshot = self.app.update_session();
            self.app.updater.prepare_restart(snapshot);
        }
        if self.app.updater.closing {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        // Tray handling (Windows only)
        #[cfg(windows)]
        {
            // Check if quit was requested from tray
            if systray::is_active() && systray::should_exit() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }

            // Handle restore from tray. Remember it for this frame: the
            // viewport can still read as minimized until the Win32 restore
            // lands, and the tray intercept below must not re-hide the window.
            let mut restoring = systray::take_restore_request();
            if restoring && ctx.input(|i| i.viewport().minimized.unwrap_or(false)) {
                // Un-minimize only when actually iconic: winit implements this
                // via SW_RESTORE, which would also collapse a merely maximized
                // window back to its normal size.
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            }

            // When hidden to the system tray, skip all UI work. A hidden
            // window has no taskbar button, so a restore normally arrives via
            // the tray (handled above); resync if something else re-showed
            // the window, e.g. a second instance's activate racing this frame.
            if systray::is_window_hidden() {
                self.background_tick(ctx);
                if systray::is_window_visible() {
                    systray::clear_hidden();
                    if ctx.input(|i| i.viewport().minimized.unwrap_or(false)) {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    }
                    restoring = true;
                } else {
                    // Still hidden, skip work
                    ctx.request_repaint_after(std::time::Duration::from_millis(250));
                    return;
                }
            }

            // Minimize-to-tray: intercept both the X button and minimize.
            // Hide outright (SW_HIDE) and never queue Minimized(true): winit
            // applies viewport commands after the frame, and a minimized
            // window is a *visible* window on Windows, so it would undo the
            // hide and leave a taskbar button behind.
            if self.app.minimize_to_tray && systray::is_active() && !restoring {
                let close_requested = ctx.input(|i| i.viewport().close_requested());
                let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
                if close_requested || minimized {
                    if close_requested {
                        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    }
                    systray::hide_window();
                    self.background_tick(ctx);
                    ctx.request_repaint_after(std::time::Duration::from_millis(250));
                    return;
                }
            }
        }

        // Check if window is minimized (cross-platform, including Linux)
        let is_minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if is_minimized {
            // Process messages to prevent buffer overflow, but skip rendering.
            self.background_tick(ctx);
            // Drain reasonably often while minimized: update_minimal() empties the
            // channel each call, so polling ~4x/second keeps the message-loss window
            // small without the cost of full-rate rendering.
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
            return;
        }

        // Self-heal a degenerately small startup window. eframe restores the window
        // size saved last session, but the minimize-to-tray flow can persist a tiny
        // (minimized) size, and the restore has no lower-bound clamp, so the app can
        // reopen as a tiny box. Only a bad restore can put a *visible* window below
        // our 640x480 minimum (winit enforces that min on user resizes), so if we see
        // a sub-minimum content rect, snap it back to the default size.
        if !self.window_size_ok {
            let logical = ctx.content_rect().size();
            if logical.x >= 640.0 && logical.y >= 480.0 {
                self.window_size_ok = true;
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(1024.0, 768.0)));
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        self.process_connection_lifecycle(ctx);

        // Main UI update
        self.app.ui(ui, frame);

        // Hyperlink clicks queue OpenUrl commands that eframe's native
        // backends ignore (only its web backend acts on them), so open them
        // here ourselves. Draining keeps the queue from growing unbounded if
        // the opener fails.
        let mut urls: Vec<String> = Vec::new();
        ctx.output_mut(|output| {
            for command in &output.commands {
                if let egui::OutputCommand::OpenUrl(open) = command {
                    urls.push(open.url.clone());
                }
            }
            output
                .commands
                .retain(|command| !matches!(command, egui::OutputCommand::OpenUrl(_)));
        });
        for url in urls {
            opener::open_in_browser(&url);
        }

        // Track when we last received messages for adaptive repaint intervals
        if self.app.had_messages_this_frame {
            self.last_message_time = std::time::Instant::now();
        }

        // Adaptive repaint interval based on activity level
        if self.app.connected
            || self.app.connecting
            // While waiting out the reconnect backoff nothing else wakes the
            // event loop: without a scheduled repaint the frame would never
            // re-run to observe the retry deadline passing.
            || self.app.awaiting_reconnect()
        {
            let interval = if self.app.channel_list_loading {
                // High-traffic mode during /list - fast polling for UI
                // responsiveness. A broken or hostile server that never sends
                // RPL_LISTEND (323) must not pin this rate forever.
                let since = *self
                    .list_loading_since
                    .get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= std::time::Duration::from_secs(30) {
                    std::time::Duration::from_millis(250)
                } else {
                    std::time::Duration::from_millis(16)
                }
            } else {
                self.list_loading_since = None;
                // Check how long since last message activity
                let idle_time = self.last_message_time.elapsed();
                if idle_time < std::time::Duration::from_secs(2) {
                    // Recent activity - responsive mode
                    std::time::Duration::from_millis(100)
                } else if idle_time < std::time::Duration::from_secs(30) {
                    // Moderate idle - reduced polling
                    std::time::Duration::from_millis(250)
                } else {
                    // Long idle - minimal polling
                    std::time::Duration::from_millis(500)
                }
            };
            ctx.request_repaint_after(interval);
        }
        // When disconnected and not connecting, no need to poll - egui handles UI events
    }

    // eframe passes a glow context only on Linux; other targets use wgpu.
    fn on_exit(&mut self, #[cfg(target_os = "linux")] _gl: Option<&eframe::glow::Context>) {
        self.app.prepare_update_shutdown();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        if let Some(thread) = self.connection_thread.take() {
            while !thread.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
        self.app.msg_rx = None;
        if let Some(armed) = self.app.updater.arm_on_exit(self.app.automatic_updates)
            && let Ok(mut shutdown) = self.shutdown.lock()
        {
            *shutdown = Some(armed);
        }
        // Remove the tray icon and free its HICON on shutdown (no-op elsewhere).
        #[cfg(windows)]
        systray::destroy_tray_icon();
    }
}

impl LinefeedApp {
    fn background_tick(&mut self, ctx: &egui::Context) {
        self.app.update_minimal();
        self.process_connection_lifecycle(ctx);
    }

    fn process_connection_lifecycle(&mut self, ctx: &egui::Context) {
        if self
            .connection_thread
            .as_ref()
            .is_some_and(|handle| handle.is_finished())
        {
            self.connection_thread = None;
            match self.app.connection_intent {
                ConnectionIntent::ReconnectAfterClose => {
                    self.app.connection_intent = ConnectionIntent::None;
                    // An explicit endpoint switch is staged while the old
                    // socket closes. Promote it only now so late messages from
                    // the old server cannot be relabelled as the new session.
                    self.app.promote_pending_session();
                    self.app.connecting = true;
                    self.app.connected = false;
                    self.app
                        .add_server_message(ChatMessage::system("Previous connection closed"));
                }
                ConnectionIntent::ManualDisconnect => {
                    self.app.connection_intent = ConnectionIntent::None;
                    // request_manual_disconnect already cleared `connecting`,
                    // so a set flag here is a connect requested while this
                    // worker was still exiting: keep it so it starts below.
                    self.app.connected = false;
                }
                ConnectionIntent::None => {
                    if self.app.connected {
                        // Connection was lost unexpectedly after registering.
                        self.app.mark_connection_lost();
                        self.app
                            .add_server_message(ChatMessage::system("Connection lost"));
                        tracing::warn!("Connection lost, will attempt reconnect");
                    } else if self.app.connecting {
                        // The connection thread exited before we ever registered.
                        self.app.connecting = false;
                        self.app.mark_connection_lost();
                        self.app
                            .add_server_message(ChatMessage::system("Connection failed"));
                        tracing::warn!("Initial connection failed, will attempt reconnect");
                    }
                }
            }
        }

        self.app
            .release_closing_connection(self.connection_thread.is_none());

        // A worker told to close is still exiting. Nothing else schedules a
        // frame to observe it finish (its own repaint request can land before
        // the thread is joinable), and the next connect waits for it, so poll.
        if self.connection_thread.is_some() && !self.app.connected && !self.app.connecting {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Auto-reconnect if enabled and connection was lost.
        if self.app.should_reconnect() {
            self.app.start_reconnect();
        }

        // Start connection if requested.
        if self.app.connecting && self.connection_thread.is_none() {
            if !self.app.ensure_active_session() {
                return;
            }
            self.start_connection(ctx.clone());
        }
    }

    fn start_connection(&mut self, ctx: egui::Context) {
        let Some(config) = self.app.active_server_config() else {
            self.app.connecting = false;
            self.app.show_connect_dialog = true;
            self.app.add_server_message(ChatMessage::system(
                "Cannot connect without a validated server endpoint",
            ));
            return;
        };
        tracing::info!("Connecting to {}:{}", config.host, config.port);

        let (msg_tx, msg_rx) = mpsc::channel::<IrcMessage>(1000);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<IrcCommand>();

        // The previous worker is gone (connection_thread was None), so nothing
        // it negotiated or left half-received may leak into this socket.
        self.app.begin_connection_attempt();
        self.app.msg_rx = Some(msg_rx);
        self.app.cmd_tx = Some(cmd_tx);

        self.connection_thread = Some(std::thread::spawn(move || {
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

    #[test]
    fn single_instance_gate_runs_before_recovery_except_when_supervised() {
        // Normal launches gate (and pass here) before recovery; supervised
        // update launches defer the gate until after recovery (issue #199).
        assert!(single_instance_allows_startup(false));
        assert!(single_instance_allows_startup(true));
    }

    #[test]
    fn background_tick_observes_finished_connection_threads() {
        let handle = std::thread::spawn(|| {});
        while !handle.is_finished() {
            std::thread::yield_now();
        }

        let mut app = IrcApp::default();
        app.connected = true;
        let mut linefeed = LinefeedApp {
            startup_ack: None,
            shutdown: Arc::new(std::sync::Mutex::new(None)),
            app,
            connection_thread: Some(handle),
            last_message_time: std::time::Instant::now(),
            window_size_ok: true,
            list_loading_since: None,
        };
        linefeed.background_tick(&egui::Context::default());

        assert!(linefeed.connection_thread.is_none());
        assert!(linefeed.app.connection_lost);
        assert!(!linefeed.app.connected);
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

    fn linefeed_for_port(port: u16) -> LinefeedApp {
        let mut app = IrcApp::with_settings(gui::Settings {
            server_host: "127.0.0.1".to_string(),
            server_port: port.to_string(),
            use_tls: false,
            nickname: "tester".to_string(),
            logging_enabled: false,
            ..gui::Settings::default()
        });
        app.connecting = true;
        LinefeedApp {
            startup_ack: None,
            shutdown: Arc::new(std::sync::Mutex::new(None)),
            app,
            connection_thread: None,
            last_message_time: std::time::Instant::now(),
            window_size_ok: true,
            list_loading_since: None,
        }
    }

    /// Drive the lifecycle the way the frame loop does until `done` holds.
    fn tick_until(linefeed: &mut LinefeedApp, what: &str, done: impl Fn(&LinefeedApp) -> bool) {
        let ctx = egui::Context::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done(linefeed) {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out: {what}; connecting={} connected={} intent={:?} thread={} messages={:?}",
                linefeed.app.connecting,
                linefeed.app.connected,
                linefeed.app.connection_intent,
                linefeed.connection_thread.is_some(),
                linefeed
                    .app
                    .server_messages
                    .iter()
                    .map(|m| m.content.clone())
                    .collect::<Vec<_>>()
            );
            linefeed.background_tick(&ctx);
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
    fn finish_worker_and_tick(linefeed: &mut LinefeedApp, release: std::sync::mpsc::Sender<()>) {
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !linefeed
            .connection_thread
            .as_ref()
            .is_some_and(|handle| handle.is_finished())
        {
            assert!(std::time::Instant::now() < deadline, "worker never exited");
            std::thread::yield_now();
        }
        linefeed.background_tick(&egui::Context::default());
    }

    /// An app that looks connected through a parked worker. The configured
    /// endpoint is an unused loopback port: the tests that follow only
    /// observe whether the lifecycle started another worker, never its result.
    fn linefeed_with_live_worker() -> (LinefeedApp, std::sync::mpsc::Sender<()>) {
        let mut linefeed = linefeed_for_port(1);
        assert!(linefeed.app.start_session_from_form());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<IrcCommand>();
        let (msg_tx, msg_rx) = mpsc::channel::<IrcMessage>(8);
        // The parked worker owns the far ends so the channels stay open.
        let (handle, release) = parked_worker((cmd_rx, msg_tx));
        linefeed.connection_thread = Some(handle);
        linefeed.app.cmd_tx = Some(cmd_tx);
        linefeed.app.msg_rx = Some(msg_rx);
        linefeed.app.connecting = false;
        linefeed.app.connected = true;
        (linefeed, release)
    }

    #[test]
    fn late_welcome_from_the_closing_connection_does_not_cancel_a_server_switch() {
        let (mut linefeed, release) = linefeed_with_live_worker();
        linefeed.app.connected = false;
        linefeed.app.connecting = true;
        linefeed
            .app
            .apply_unprofiled_endpoint("127.0.0.1", "2", false);
        assert!(linefeed.app.switch_session_from_form("Changing servers"));

        // Registration finished on the old socket after QUIT was queued.
        linefeed.app.handle_incoming_message(
            IrcMessage::parse(":old.example 001 tester :Welcome").unwrap(),
        );
        assert!(!linefeed.app.connected);
        assert_eq!(
            linefeed.app.connection_intent,
            ConnectionIntent::ReconnectAfterClose
        );

        finish_worker_and_tick(&mut linefeed, release);

        // The staged endpoint was promoted and connected to, instead of the
        // old one being declared lost and auto-reconnected.
        assert_eq!(linefeed.app.active_server_config().unwrap().port, 2);
        assert!(linefeed.connection_thread.is_some());
        assert!(
            !linefeed
                .app
                .server_messages
                .iter()
                .any(|message| message.content.contains("Connection lost"))
        );
    }

    #[test]
    fn connect_right_after_a_manual_disconnect_survives_the_old_workers_exit() {
        let (mut linefeed, release) = linefeed_with_live_worker();
        linefeed.app.request_manual_disconnect(Some("bye".into()));
        assert!(linefeed.app.start_session_from_form());

        finish_worker_and_tick(&mut linefeed, release);

        assert!(
            linefeed.connection_thread.is_some(),
            "the connect request was silently dropped"
        );
        assert_eq!(linefeed.app.connection_intent, ConnectionIntent::None);
    }

    #[test]
    fn reconnect_right_after_a_manual_disconnect_is_not_reported_as_a_failure() {
        let (mut linefeed, release) = linefeed_with_live_worker();
        linefeed.app.request_manual_disconnect(Some("bye".into()));
        linefeed.app.request_reconnect_after_close("Reconnecting");

        finish_worker_and_tick(&mut linefeed, release);

        assert!(linefeed.connection_thread.is_some());
        assert!(
            !linefeed
                .app
                .server_messages
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
        let mut linefeed = linefeed_for_port(port);
        linefeed.app.connecting = false;
        linefeed.app.request_manual_disconnect(None);
        assert_eq!(
            linefeed.app.connection_intent,
            ConnectionIntent::ManualDisconnect
        );

        // ...then connect: the new worker owns a fresh intent, so its failure
        // is reported and retried like any other.
        assert!(linefeed.app.start_session_from_form());
        linefeed.background_tick(&egui::Context::default());
        assert_eq!(linefeed.app.connection_intent, ConnectionIntent::None);
        tick_until(&mut linefeed, "refused connection to be reported", |l| {
            l.app
                .server_messages
                .iter()
                .any(|message| message.content.contains("Connection failed"))
        });
    }

    #[test]
    fn an_exiting_worker_is_polled_until_the_lifecycle_observes_it_finish() {
        let (mut linefeed, release) = linefeed_with_live_worker();
        linefeed.app.request_manual_disconnect(Some("bye".into()));
        let (delay_tx, delay_rx) = std::sync::mpsc::channel();
        let ctx = egui::Context::default();
        ctx.set_request_repaint_callback(move |info| {
            let _ = delay_tx.send(info.delay);
        });

        // Disconnected and not connecting, but the worker has not exited yet:
        // no other repaint source exists, so the lifecycle must ask for one.
        linefeed.process_connection_lifecycle(&ctx);
        assert!(
            delay_rx
                .try_recv()
                .is_ok_and(|delay| delay <= std::time::Duration::from_millis(100)),
            "no repaint was scheduled for the exiting worker"
        );

        // Once the worker is gone nothing keeps the idle window repainting.
        finish_worker_and_tick(&mut linefeed, release);
        while delay_rx.try_recv().is_ok() {}
        linefeed.process_connection_lifecycle(&ctx);
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
        let mut linefeed = linefeed_for_port(port);
        tick_until(&mut linefeed, "registration to start", |_| {
            accepted_rx.try_recv().is_ok()
        });

        // The retired receiver stays open, so cancellation has to come from the
        // queued QUIT alone, long before the registration deadline.
        linefeed.app.request_manual_disconnect(None);
        assert!(linefeed.app.closing_rx.is_some());
        // Wait for the worker without running the lifecycle: its pass drops the
        // retired receiver once the grace period is over, and closing that
        // receiver cancels the worker by itself, which would let this test
        // pass even with QUIT-based cancellation broken.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !linefeed
            .connection_thread
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
            linefeed.app.closing_rx.is_some(),
            "the worker only exited once its receiver was dropped"
        );

        // The lifecycle then observes the exit as an ordinary disconnect.
        tick_until(&mut linefeed, "worker exit to be observed", |l| {
            l.connection_thread.is_none()
        });
        assert!(!linefeed.app.connecting && !linefeed.app.connected);
        assert!(linefeed.app.closing_rx.is_none());
    }

    #[test]
    fn manual_disconnect_delivers_quit_before_closing_the_socket() {
        let (port, lines_rx) = scripted_server();
        let mut linefeed = linefeed_for_port(port);
        tick_until(&mut linefeed, "registration", |l| l.app.connected);

        linefeed
            .app
            .request_manual_disconnect(Some("bye now".into()));
        tick_until(&mut linefeed, "worker exit", |l| {
            l.connection_thread.is_none()
        });

        let lines = lines_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("server never saw the connection close");
        assert!(
            lines.iter().any(|line| line == "QUIT :bye now"),
            "QUIT was lost on manual disconnect: {lines:?}"
        );
        assert!(!linefeed.app.connected && !linefeed.app.connecting);
        assert!(linefeed.app.msg_rx.is_none());
    }
}
