#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod gui;
mod icon_data;
mod irc;

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

fn main() -> eframe::Result<()> {
    // Check for existing instance FIRST (before any GUI setup)
    if !ensure_single_instance() {
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

    eframe::run_native(
        "Linefeed",
        options,
        Box::new(|cc| {
            // Load emoji fonts for comprehensive Unicode support
            setup_fonts(&cc.egui_ctx);

            // Reduce texture memory by discarding image data after GPU upload
            cc.egui_ctx.options_mut(|opts| {
                opts.reduce_texture_memory = true;
            });

            let app = IrcApp::new(cc);
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
            }))
        }),
    )
}

struct LinefeedApp {
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

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
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
                    self.app.connecting = false;
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

            rt.block_on(async {
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
                ctx.request_repaint();
            });
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_tick_observes_finished_connection_threads() {
        let handle = std::thread::spawn(|| {});
        while !handle.is_finished() {
            std::thread::yield_now();
        }

        let mut app = IrcApp::default();
        app.connected = true;
        let mut linefeed = LinefeedApp {
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
}
