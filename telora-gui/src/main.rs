use async_channel::Sender;
use gtk4::prelude::*;
use gtk4::{Application, glib};
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use log::info;

mod clipboard;
mod config;
mod connection;
mod focus;
mod input;
mod text;
mod ui;

use config::GuiConfig;
use connection::{ControlServer, SocketClient};
use telora_common::paths::{control_socket_path, daemon_socket_path};
use ui::Osd;

fn wait_for_wayland_display(max_wait_secs: u64) -> Result<(), String> {
    let xdg_runtime_dir =
        std::env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR is not set".to_string())?;

    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());

    let socket_path = Path::new(&xdg_runtime_dir).join(&display);

    let start = Instant::now();
    let mut attempt: u32 = 0;

    loop {
        if let Ok(meta) = std::fs::metadata(&socket_path)
            && meta.file_type().is_socket()
        {
            info!("Wayland display ready at {}", socket_path.display());
            return Ok(());
        }

        let elapsed = start.elapsed().as_secs();
        if elapsed >= max_wait_secs {
            return Err(format!(
                "Wayland display {} not available after {}s",
                socket_path.display(),
                elapsed
            ));
        }

        attempt += 1;
        let delay = (1u64 << attempt).min(10); // 1, 2, 4, 8, 10, 10, ...
        let remaining = max_wait_secs.saturating_sub(elapsed);
        let wait = delay.min(remaining);

        info!(
            "Waiting for Wayland compositor (attempt {})... retrying in {}s",
            attempt, wait
        );
        thread::sleep(Duration::from_secs(wait));
    }
}

#[derive(Debug, Clone)]
enum AppAction {
    ToggleRecording(String, bool), // mode, is_auto_stop
    CancelRecording,
    OsdUpdate(String, String), // Text, Color
    OsdHide,
}

#[derive(Debug)]
enum DaemonCommand {
    Start,
    Stop {
        mode: String,
        response_tx: Sender<AppAction>,
    },
    Cancel,
}

fn main() {
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        // Print the resolved socket paths so the help text reflects
        // whatever the runtime would actually bind/connect to (XDG
        // cascade → /run/user/<uid>/ → /tmp fallback). Built with
        // `format!` because the literal `println!("...")` form
        // could not interpolate the dynamic paths.
        let daemon_sock = daemon_socket_path();
        let control_sock = control_socket_path();
        let bin_name = std::env::args()
            .next()
            .unwrap_or_else(|| "telora-gui".to_string());
        println!(
            "telora-gui {version} — Telora Assistant UI (Wayland overlay)\n\
             \n\
             USAGE:\n\
             {bin_name}\n\
             \n\
             DESCRIPTION:\n\
             Displays an OSD overlay on Wayland using the Layer Shell protocol.\n\
             It listens for control commands via Unix socket and relays them to\n\
             the telora-daemon for audio transcription.\n\
             \n\
             This binary is normally launched by systemd as a user service and\n\
             controlled via the `telora` CLI client.\n\
             \n\
             SOCKETS:\n\
             Control (listen):  {control_sock}\n\
             Daemon (connect):  {daemon_sock}\n\
             \n\
             ENVIRONMENT:\n\
             WAYLAND_DISPLAY     Wayland socket name (default: wayland-0)\n\
             XDG_RUNTIME_DIR     Runtime directory for Wayland socket\n\
             GSK_RENDERER        GTK render backend (set to \"gl\" by systemd service)\n\
             RUST_LOG            Log filter (default: info)\n\
             \n\
             SEE ALSO:\n\
             telora(1), telora-daemon(1), telora.service(5)",
            control_sock = control_sock.display(),
            daemon_sock = daemon_sock.display(),
            version = env!("CARGO_PKG_VERSION"),
        );
        std::process::exit(0);
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if let Err(e) = wait_for_wayland_display(60) {
        log::error!("{}", e);
        std::process::exit(1);
    }

    // Initialize GTK Application
    let app = Application::builder()
        .application_id("io.github.telora.client")
        .build();

    app.connect_activate(move |app| {
        // Keep the app running even without visible windows
        let _hold_guard = app.hold();

        // Load GUI configuration once. Cheap to clone (two strings + a small
        // map), so we hand copies to whichever thread needs it.
        let gui_config = GuiConfig::load();

        // Create async channel for communication between Tokio and GTK
        let (tx, rx) = async_channel::unbounded::<AppAction>();

        // Create mpsc channel for sending commands TO the Tokio runtime
        let (daemon_tx, daemon_rx) = mpsc::unbounded_channel::<DaemonCommand>();

        // Start Tokio Runtime in a separate thread
        // This happens AFTER GTK confirms we're the primary instance
        let tx_clone = tx.clone();
        let cfg_for_tokio = gui_config.clone();
        thread::spawn(move || {
            let rt = Runtime::new().expect("Failed to create Tokio runtime");
            rt.block_on(async {
                tokio::select! {
                    result = run_control_server(tx_clone.clone()) => {
                        if let Err(e) = result {
                            log::error!("Control server failed: {}", e);
                        }
                    }
                    _ = handle_daemon_commands(daemon_rx, tx_clone, cfg_for_tokio) => {}
                }
            });
        });

        let osd = Osd::new(app);
        let osd_clone = osd.clone();
        let tx_back = tx.clone();

        // GTK Main Loop Context
        glib::MainContext::default().spawn_local(async move {
            let mut recording = false;
            let mut current_mode = String::new();

            while let Ok(action) = rx.recv().await {
                match action {
                    AppAction::ToggleRecording(mode, is_auto_stop) => {
                        if !recording {
                            // START
                            recording = true;
                            current_mode = mode;
                            osd_clone.show("● GRABANDO", "red");
                            let _ = daemon_tx.send(DaemonCommand::Start);
                        } else {
                            // STOP
                            recording = false;
                            if is_auto_stop {
                                osd_clone.show("⏳ LÍMITE ALCANZADO", "orange");
                            } else {
                                osd_clone.show("Procesando...", "orange");
                            }
                            let _ = daemon_tx.send(DaemonCommand::Stop {
                                mode: current_mode.clone(),
                                response_tx: tx_back.clone(),
                            });
                        }
                    }
                    AppAction::CancelRecording => {
                        if recording {
                            recording = false;
                            osd_clone.show("Cancelado", "gray");
                            let _ = daemon_tx.send(DaemonCommand::Cancel);
                            // Delay hide
                            let tx_inner = tx_back.clone();
                            glib::timeout_add_seconds_local(1, move || {
                                let _ = tx_inner.send_blocking(AppAction::OsdHide);
                                glib::ControlFlow::Break
                            });
                        }
                    }
                    AppAction::OsdUpdate(text, color) => {
                        if !recording {
                            osd_clone.show(&text, &color);
                        }
                    }
                    AppAction::OsdHide => {
                        if !recording {
                            osd_clone.hide();
                        }
                    }
                }
            }
        });
    });

    app.run();
}

async fn handle_daemon_commands(
    mut rx: mpsc::UnboundedReceiver<DaemonCommand>,
    _tx: Sender<AppAction>,
    gui_config: GuiConfig,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            DaemonCommand::Start => {
                let _ = SocketClient::send_command("START").await;
            }
            DaemonCommand::Stop { mode, response_tx } => {
                // The STOP command now returns the transcription result directly
                match SocketClient::send_command("STOP").await {
                    Ok(raw_text)
                        if !raw_text.trim().is_empty() && !raw_text.starts_with("ERROR:") =>
                    {
                        let cleaned = text::clean_transcription(&raw_text);
                        let is_auto = mode == "AUTO";
                        let paste_outcome = if mode == "TYPE" || is_auto {
                            input::type_text(&cleaned, &gui_config)
                        } else {
                            input::copy_text(&cleaned);
                            clipboard::PasteOutcome::Ok
                        };

                        if is_auto {
                            let _ = response_tx
                                .send(AppAction::OsdUpdate(
                                    "⏳ LÍMITE ALCANZADO".to_string(),
                                    "orange".to_string(),
                                ))
                                .await;
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        } else {
                            let (msg, color) = outcome_osd(&paste_outcome, mode == "TYPE");
                            let _ = response_tx.send(AppAction::OsdUpdate(msg, color)).await;
                            let hold_secs = if paste_outcome.is_failure() { 3 } else { 1 };
                            tokio::time::sleep(std::time::Duration::from_secs(hold_secs)).await;
                        }

                        let _ = response_tx.send(AppAction::OsdHide).await;
                    }
                    Ok(text) if text.starts_with("ERROR:") => {
                        log::error!("Daemon error: {}", text);
                        let _ = response_tx.send(AppAction::OsdHide).await;
                    }
                    Ok(_) => {
                        // Empty result
                        let _ = response_tx.send(AppAction::OsdHide).await;
                    }
                    Err(e) => {
                        log::error!("Failed to get result from daemon: {}", e);
                        let _ = response_tx.send(AppAction::OsdHide).await;
                    }
                }
            }
            DaemonCommand::Cancel => {
                let _ = SocketClient::send_command("CANCEL").await;
            }
        }
    }
}

fn outcome_osd(outcome: &clipboard::PasteOutcome, is_type_mode: bool) -> (String, String) {
    match outcome {
        clipboard::PasteOutcome::Ok => {
            if is_type_mode {
                ("Escrito".to_string(), "green".to_string())
            } else {
                ("Copiado".to_string(), "green".to_string())
            }
        }
        clipboard::PasteOutcome::Partial { skipped } => {
            let count = skipped.len();
            let label = if is_type_mode { "Escrito" } else { "Copiado" };
            (
                format!(
                    "{label} ⚠ {count} tipo{plural} perdido{plural2}",
                    plural = if count == 1 { "" } else { "s" },
                    plural2 = if count == 1 { "" } else { "s" }
                ),
                // A Partial means the receiving app already got the text
                // but lost fidelity on a few MIME types. It is not an
                // error, just a degraded success — surface it in the same
                // green as `Ok` so the colour does not suggest something
                // went wrong; only the trailing '⚠ N tipos perdidos'
                // tells the user that some clipboard content was dropped.
                "green".to_string(),
            )
        }
        clipboard::PasteOutcome::FallbackSingleMime { .. } => (
            "⚠ Respaldo simple (formato único)".to_string(),
            "orange".to_string(),
        ),
        clipboard::PasteOutcome::Refused { .. } => (
            "✘ Cancelado (portapapeles protegido)".to_string(),
            "gray".to_string(),
        ),
    }
}

async fn run_control_server(tx: Sender<AppAction>) -> anyhow::Result<()> {
    let server = ControlServer::bind(&control_socket_path())?;
    info!("Control server listening...");

    loop {
        match server.next_command().await {
            Ok(cmd) => {
                info!("Control command: {}", cmd);
                match cmd.as_str() {
                    "TOGGLE_TYPE" => {
                        let _ = tx
                            .send(AppAction::ToggleRecording("TYPE".to_string(), false))
                            .await;
                    }
                    "TOGGLE_COPY" => {
                        let _ = tx
                            .send(AppAction::ToggleRecording("COPY".to_string(), false))
                            .await;
                    }
                    "CANCEL" => {
                        let _ = tx.send(AppAction::CancelRecording).await;
                    }
                    "AUTO_STOP" => {
                        let _ = tx
                            .send(AppAction::ToggleRecording("AUTO".to_string(), true))
                            .await;
                    }
                    _ => {}
                }
            }
            Err(e) => {
                log::error!("Control server error: {}", e);
            }
        }
    }
}
