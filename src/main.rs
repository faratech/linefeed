#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod irc;
mod gui;
mod icon_data;

use eframe::egui;
use tokio::sync::mpsc;
use std::sync::Arc;

use irc::{IrcClient, IrcCommand, IrcMessage};
use gui::{IrcApp, ChatMessage};

// System tray support (Windows only)
#[cfg(windows)]
mod tray {
    use std::sync::atomic::{AtomicBool, Ordering};
    use crate::icon_data;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, ShowWindow, SetForegroundWindow, FlashWindowEx,
        SW_HIDE, SW_SHOW, FLASHWINFO, FLASHW_ALL, FLASHW_TIMERNOFG,
    };
    use windows::core::w;

    static TRAY_ACTIVE: AtomicBool = AtomicBool::new(false);
    static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

    fn find_our_window() -> Option<HWND> {
        unsafe {
            match FindWindowW(None, w!("fmIRC")) {
                Ok(hwnd) if !hwnd.is_invalid() => Some(hwnd),
                _ => None,
            }
        }
    }

    pub fn show_window() {
        if let Some(hwnd) = find_our_window() {
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
            }
            tracing::info!("Window shown via Win32");
        } else {
            tracing::warn!("Could not find window to show");
        }
    }

    pub fn hide_window() {
        if let Some(hwnd) = find_our_window() {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            tracing::info!("Window hidden via Win32");
        }
    }

    pub fn create_tray_icon() -> Option<tray_icon::TrayIcon> {
        use tray_icon::{TrayIconBuilder, menu::{Menu, MenuItem, PredefinedMenuItem}};
        use tray_icon::Icon;

        let menu = Menu::new();
        let show_item = MenuItem::with_id("show", "Show fmIRC", true, None);
        let quit_item = MenuItem::with_id("quit", "Quit", true, None);
        let _ = menu.append(&show_item);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&quit_item);

        let icon = Icon::from_rgba(
            icon_data::ICON_RGBA.to_vec(),
            icon_data::ICON_WIDTH,
            icon_data::ICON_HEIGHT,
        ).ok()?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("fmIRC")
            .with_icon(icon)
            .build()
            .ok()?;

        TRAY_ACTIVE.store(true, Ordering::SeqCst);
        tracing::info!("System tray icon created");
        Some(tray)
    }

    pub fn setup_event_handler() {
        use tray_icon::menu::MenuEvent;
        use tray_icon::TrayIconEvent;

        // Menu clicks
        std::thread::spawn(|| {
            let rx = MenuEvent::receiver();
            loop {
                if let Ok(event) = rx.recv() {
                    match event.id.0.as_str() {
                        "show" => show_window(),
                        "quit" => {
                            EXIT_REQUESTED.store(true, Ordering::SeqCst);
                            show_window(); // Show so egui can process close
                        }
                        _ => {}
                    }
                }
            }
        });

        // Double-click on tray icon
        std::thread::spawn(|| {
            let rx = TrayIconEvent::receiver();
            loop {
                if let Ok(event) = rx.recv() {
                    if matches!(event, tray_icon::TrayIconEvent::DoubleClick { .. }) {
                        show_window();
                    }
                }
            }
        });
    }

    pub fn is_active() -> bool {
        TRAY_ACTIVE.load(Ordering::SeqCst)
    }

    pub fn should_exit() -> bool {
        EXIT_REQUESTED.load(Ordering::SeqCst)
    }

    /// Flash the taskbar to alert the user of a notification
    pub fn flash_window() {
        if let Some(hwnd) = find_our_window() {
            unsafe {
                let mut flash_info = FLASHWINFO {
                    cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                    hwnd,
                    dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
                    uCount: 3,
                    dwTimeout: 0,
                };
                let _ = FlashWindowEx(&mut flash_info);
            }
            tracing::debug!("Flashed taskbar for notification");
        }
    }
}

fn load_icon() -> egui::IconData {
    egui::IconData {
        rgba: icon_data::ICON_RGBA.to_vec(),
        width: icon_data::ICON_WIDTH,
        height: icon_data::ICON_HEIGHT,
    }
}

/// Load system fonts including emoji support
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // Try to load system emoji fonts as fallback
    let emoji_font_paths: &[&str] = if cfg!(windows) {
        &[
            "C:\\Windows\\Fonts\\seguiemj.ttf",  // Segoe UI Emoji
            "C:\\Windows\\Fonts\\segoeui.ttf",   // Segoe UI (fallback)
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
            "/usr/share/fonts/truetype/unifont/unifont.ttf",
        ]
    };

    for path in emoji_font_paths {
        if let Ok(font_data) = std::fs::read(path) {
            fonts.font_data.insert(
                "emoji".to_owned(),
                Arc::new(egui::FontData::from_owned(font_data)),
            );

            // Add emoji font as fallback for all font families
            for family in [
                egui::FontFamily::Proportional,
                egui::FontFamily::Monospace,
            ] {
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
        GetCurrentProcess, SetPriorityClass, SetProcessInformation,
        ProcessPowerThrottling, IDLE_PRIORITY_CLASS,
        PROCESS_POWER_THROTTLING_STATE, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    };

    unsafe {
        let handle = GetCurrentProcess();

        // Set to idle priority class (lowest scheduling priority)
        let _ = SetPriorityClass(handle, IDLE_PRIORITY_CLASS);

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

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_target(false)
        .init();

    tracing::info!("Starting fmIRC");

    // Enable Efficiency Mode to reduce CPU/battery usage
    enable_efficiency_mode();

    // Create tray icon before eframe starts (Windows only)
    #[cfg(windows)]
    let _tray = {
        tray::setup_event_handler();
        tray::create_tray_icon()
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 768.0])
            .with_min_inner_size([640.0, 480.0])
            .with_title("fmIRC")
            .with_icon(Arc::new(load_icon())),
        ..Default::default()
    };

    eframe::run_native(
        "fmIRC",
        options,
        Box::new(|cc| {
            // Load emoji fonts for comprehensive Unicode support
            setup_fonts(&cc.egui_ctx);

            let mut style = (*cc.egui_ctx.style()).clone();
            style.spacing.item_spacing = egui::vec2(8.0, 4.0);
            cc.egui_ctx.set_style(style);

            Ok(Box::new(FmIrcApp {
                app: IrcApp::new(cc),
                connection_thread: None,
            }))
        }),
    )
}

struct FmIrcApp {
    app: IrcApp,
    connection_thread: Option<std::thread::JoinHandle<()>>,
}

impl eframe::App for FmIrcApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Tray handling (Windows only)
        #[cfg(windows)]
        {
            // Check if quit was requested from tray
            if tray::is_active() && tray::should_exit() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }

            // Intercept X button when minimize_to_tray is enabled
            if self.app.minimize_to_tray && tray::is_active() {
                if ctx.input(|i| i.viewport().close_requested()) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    tray::hide_window();
                }
            }
        }

        // Check if connection thread has finished (connection lost)
        if let Some(ref handle) = self.connection_thread {
            if handle.is_finished() {
                self.connection_thread = None;
                if self.app.connected {
                    // Connection was lost unexpectedly
                    self.app.mark_connection_lost();
                    self.app.add_server_message(ChatMessage::system("Connection lost"));
                    tracing::warn!("Connection lost, will attempt reconnect");
                }
            }
        }

        // Start connection if requested
        if self.app.connecting && self.connection_thread.is_none() {
            self.start_connection(ctx.clone());
        }

        // Auto-reconnect if enabled and connection was lost
        if self.app.should_reconnect() {
            self.app.start_reconnect();
        }

        // Main UI update
        self.app.update(ctx, frame);

        // Keep event loop alive for IRC processing
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

impl FmIrcApp {
    fn start_connection(&mut self, ctx: egui::Context) {
        let config = self.app.get_server_config();
        tracing::info!("Connecting to {}:{}", config.host, config.port);

        let (msg_tx, msg_rx) = mpsc::channel::<IrcMessage>(1000);
        let (cmd_tx, cmd_rx) = mpsc::channel::<IrcCommand>(100);

        self.app.msg_rx = Some(msg_rx);
        self.app.cmd_tx = Some(cmd_tx);

        self.connection_thread = Some(std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                let mut client = IrcClient::new(config.clone());
                match client.connect(msg_tx.clone(), cmd_rx).await {
                    Ok(_) => tracing::info!("Connection closed"),
                    Err(e) => {
                        tracing::error!("Connection error: {}", e);
                        let _ = msg_tx.send(IrcMessage {
                            tags: None,
                            prefix: None,
                            command: IrcCommand::Notice(
                                "*".to_string(),
                                format!("Connection error: {}", e),
                            ),
                            raw: String::new(),
                        }).await;
                    }
                }
                ctx.request_repaint();
            });
        }));
    }
}
