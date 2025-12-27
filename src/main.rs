#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod irc;
mod gui;
mod icon_data;

use eframe::egui;
use tokio::sync::mpsc;
use std::sync::Arc;

use irc::{IrcClient, IrcCommand, IrcMessage};
use gui::IrcApp;

fn load_icon() -> egui::IconData {
    egui::IconData {
        rgba: icon_data::ICON_RGBA.to_vec(),
        width: icon_data::ICON_WIDTH,
        height: icon_data::ICON_HEIGHT,
    }
}

fn main() -> eframe::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_target(false)
        .init();

    tracing::info!("Starting fmIRC");

    // Native options for the window
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([1024.0, 768.0])
        .with_min_inner_size([640.0, 480.0])
        .with_title("fmIRC")
        .with_icon(Arc::new(load_icon()));

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "fmIRC",
        options,
        Box::new(move |cc| {
            let mut style = (*cc.egui_ctx.style()).clone();
            style.spacing.item_spacing = egui::vec2(8.0, 4.0);
            cc.egui_ctx.set_style(style);

            let app = IrcApp::new(cc);

            Ok(Box::new(FmIrcApp {
                app,
                connection_thread: None,
            }) as Box<dyn eframe::App>)
        }),
    )
}

struct FmIrcApp {
    app: IrcApp,
    connection_thread: Option<std::thread::JoinHandle<()>>,
}

impl eframe::App for FmIrcApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Check if user wants to connect
        if self.app.connecting && self.connection_thread.is_none() {
            self.start_connection(ctx.clone());
        }

        self.app.update(ctx, frame);
    }
}

impl FmIrcApp {
    fn start_connection(&mut self, ctx: egui::Context) {
        let config = self.app.get_server_config();

        tracing::info!("Starting connection to {}:{}", config.host, config.port);

        // Create channels
        let (msg_tx, msg_rx) = mpsc::channel::<IrcMessage>(1000);
        let (cmd_tx, cmd_rx) = mpsc::channel::<IrcCommand>(100);

        self.app.msg_rx = Some(msg_rx);
        self.app.cmd_tx = Some(cmd_tx);

        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async move {
                let mut client = IrcClient::new(config.clone());

                match client.connect(msg_tx.clone(), cmd_rx).await {
                    Ok(_) => {
                        tracing::info!("Connection closed normally");
                    }
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
        });

        self.connection_thread = Some(handle);
    }
}
