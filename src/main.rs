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

use gui::App;

#[cfg(windows)]
static SINGLE_INSTANCE_MUTEX: OnceLock<isize> = OnceLock::new();

pub(crate) fn load_icon() -> egui::IconData {
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
            .with_title(gui::MAIN_WINDOW_TITLE)
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

            let mut app = App::load();
            app.start(snapshot);
            app.updater.install_reason = update_error;
            let automatic_updates = app.automatic_updates();
            app.updater
                .start(paths, cc.egui_ctx.clone(), automatic_updates);
            let mut style = (*cc.egui_ctx.global_style()).clone();
            style.spacing.item_spacing = egui::vec2(8.0, 4.0);
            cc.egui_ctx.set_global_style(style);
            gui::apply_font_size(&cc.egui_ctx, app.font_size());

            Ok(Box::new(LinefeedApp {
                app,
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

impl LinefeedApp {
    /// While the main window is minimized or occluded, eframe runs no ui
    /// pass at all, only `App::logic`. Keep every connection drained and its timers,
    /// reconnects and lag pings running then, or the incoming buffers fill
    /// and connections time out. When a ui pass follows, it does this work.
    fn hidden_logic(&mut self, ctx: &egui::Context) {
        let hidden = ctx.input(|i| i.viewport().visible() == Some(false));
        if !hidden {
            return;
        }
        self.app.updater.poll();
        // Minimize-to-tray: a minimized main window gets no ui pass, so the
        // intercept in `ui` never sees the minimize. Hide it from here.
        #[cfg(windows)]
        if self.app.minimize_to_tray()
            && systray::is_active()
            && !systray::is_window_hidden()
            && !systray::restore_pending()
            && ctx.input(|i| i.viewport().minimized == Some(true))
        {
            // Posted, not sent: this runs inside eframe's event-loop
            // callback, where a synchronous ShowWindow deadlocks.
            systray::hide_window_async();
            self.app.tray_hidden = true;
        }
        // Detached windows minimize with the main window. Never force a ui
        // pass instead: it would paint the minimized main window, and a
        // present on a minimized window can block on Windows.
        self.app.hide_detached_windows(ctx);
        self.app.background_tick(ctx);
        // Nothing else wakes a hidden window: keep `logic` coming.
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

struct LinefeedApp {
    startup_ack: Option<updater::install::Transaction>,
    shutdown: Arc<std::sync::Mutex<Option<updater::install::Armed>>>,
    app: App,
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
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.hidden_logic(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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
                // Detached windows hide with the main window and are
                // recreated from their saved geometry on restore.
                self.app.tray_hidden = true;
                self.app.background_tick(ctx);
                if systray::is_window_visible() {
                    systray::clear_hidden();
                    self.app.tray_hidden = false;
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
            if self.app.minimize_to_tray() && systray::is_active() && !restoring {
                let close_requested = ctx.input(|i| i.viewport().close_requested());
                let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
                if close_requested || minimized {
                    if close_requested {
                        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    }
                    systray::hide_window();
                    self.app.tray_hidden = true;
                    self.app.background_tick(ctx);
                    ctx.request_repaint_after(std::time::Duration::from_millis(250));
                    return;
                }
            }
        }

        // Check if window is minimized (cross-platform, including Linux)
        let is_minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if is_minimized {
            // `logic` already ticked every connection this frame (it runs
            // before `ui`, and alone while the window is minimized). The
            // detached windows are not drawn, so egui closes them; they
            // reopen where they were when the window is restored.
            self.app.forget_detached_windows();
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

        // Main UI update
        self.app.tray_hidden = false;
        self.app.ui(ui);

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
        // While any session is connected, connecting or waiting out a
        // reconnect backoff, nothing else wakes the event loop: without a
        // scheduled repaint the frame would never observe new messages or
        // the retry deadline passing.
        if self.app.wants_polling() {
            let interval = if self.app.channel_list_loading() {
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
        // One deadline for all sessions: every worker flushes its QUIT
        // concurrently.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        self.app.join_workers(deadline);
        let automatic_updates = self.app.automatic_updates();
        if let Some(armed) = self.app.updater.arm_on_exit(automatic_updates)
            && let Ok(mut shutdown) = self.shutdown.lock()
        {
            *shutdown = Some(armed);
        }
        // Remove the tray icon and free its HICON on shutdown (no-op elsewhere).
        #[cfg(windows)]
        systray::destroy_tray_icon();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hermetic_linefeed() -> LinefeedApp {
        let mut app = App::with_settings(
            gui::Settings {
                logging_enabled: false,
                ..gui::Settings::default()
            },
            Default::default(),
        );
        app.start(None);
        LinefeedApp {
            startup_ack: None,
            shutdown: Arc::new(std::sync::Mutex::new(None)),
            app,
            last_message_time: std::time::Instant::now(),
            window_size_ok: true,
            list_loading_since: None,
        }
    }

    fn run_hidden(linefeed: &mut LinefeedApp, minimized: bool) -> Vec<egui::ViewportCommand> {
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input.viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                minimized: Some(minimized),
                occluded: Some(false),
                ..Default::default()
            },
        );
        let mut output = ctx.run_ui(input, |ui| linefeed.hidden_logic(ui.ctx()));
        output.textures_delta.clear();
        output
            .viewport_output
            .remove(&egui::ViewportId::ROOT)
            .map(|o| o.commands)
            .unwrap_or_default()
    }

    #[test]
    fn connections_keep_draining_while_the_window_is_minimized() {
        let mut linefeed = hermetic_linefeed();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        linefeed.app.sessions[0].msg_rx = Some(rx);
        tx.try_send(irc::IrcMessage::parse(":srv NOTICE * :while minimized").unwrap())
            .unwrap();

        // Visible: the ui pass does this work, `logic` must not.
        run_hidden(&mut linefeed, false);
        assert!(!tx.is_closed() && tx.capacity() == 3, "drained by logic while visible");

        let commands = run_hidden(&mut linefeed, true);
        assert!(
            linefeed.app.sessions[0]
                .server_messages
                .iter()
                .any(|m| m.content.contains("while minimized"))
        );
        // No detached window: no ui pass is needed.
        assert!(!commands.contains(&egui::ViewportCommand::RequestPaintWhileHidden));
    }

    #[test]
    fn detached_windows_hide_with_a_minimized_window_without_forcing_a_paint() {
        let mut linefeed = hermetic_linefeed();
        linefeed.app.detach_status_window_for_tests();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        linefeed.app.sessions[0].msg_rx = Some(rx);
        tx.try_send(irc::IrcMessage::parse(":srv NOTICE * :still live").unwrap())
            .unwrap();
        let ctx = egui::Context::default();
        // A normal frame first: the detached window's viewport exists.
        ctx.run_ui(egui::RawInput::default(), |ui| linefeed.app.ui(ui))
            .textures_delta
            .clear();
        let mut input = egui::RawInput::default();
        input.viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                minimized: Some(true),
                ..Default::default()
            },
        );
        let mut output = ctx.run_ui(input, |ui| linefeed.hidden_logic(ui.ctx()));
        output.textures_delta.clear();
        // Connections still tick...
        assert!(
            linefeed.app.sessions[0]
                .server_messages
                .iter()
                .any(|m| m.content.contains("still live"))
        );
        // ...the detached window is hidden (a bare egui context embeds
        // viewports, so the command itself is only visible under eframe)...
        assert!(linefeed.app.detached_windows_hidden());
        // ...and no paint of the minimized main window is forced.
        let root = output.viewport_output.get(&egui::ViewportId::ROOT);
        assert!(root.is_none_or(|o| !o
            .commands
            .contains(&egui::ViewportCommand::RequestPaintWhileHidden)));
    }

    #[test]
    fn single_instance_gate_runs_before_recovery_except_when_supervised() {
        // Normal launches gate (and pass here) before recovery; supervised
        // update launches defer the gate until after recovery (issue #199).
        assert!(single_instance_allows_startup(false));
        assert!(single_instance_allows_startup(true));
    }
}
