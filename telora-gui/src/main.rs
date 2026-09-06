use async_channel::Sender;
use gtk4::prelude::*;
use gtk4::{Application, glib};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
mod paths;
mod text;
mod ui;

use config::GuiConfig;
use connection::{ControlServer, SocketClient};
use telora_common::paths::ResolvedPaths;
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
    /// Idempotent STOP dispatched by the daemon when the recording
    /// safety limit (`max_recording_seconds`) fires. Distinct from
    /// `ToggleRecording` so the GUI never accidentally STARTS a new
    /// recording in response to a duplicate `AUTO_STOP`.
    AutoStop,
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
        // whatever the runtime would actually bind/connect to.
        // Resolves through the same `PathsConfig` cascade
        // `telora-gui/src/paths::load_paths_config` uses (issue #64):
        // `/etc/telora.toml` → `~/.config/telora/config.toml` →
        // `TELORA_PATHS__*` env vars, falling back to the XDG
        // cascade that the helper used to default to. Built with
        // `format!` because the literal `println!("...")` form could
        // not interpolate the dynamic paths.
        let paths_cfg = paths::load_paths_config();
        let resolved_paths = match telora_common::paths::resolve(&paths_cfg) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Error resolving socket path: {}", e);
                std::process::exit(1);
            }
        };
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
            control_sock = resolved_paths.control_sock.display(),
            daemon_sock = resolved_paths.daemon_sock.display(),
            version = env!("CARGO_PKG_VERSION"),
        );
        std::process::exit(0);
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if let Err(e) = wait_for_wayland_display(60) {
        log::error!("{}", e);
        std::process::exit(1);
    }

    // Resolve the operator-supplied `[paths]` overrides once at
    // startup. The resolved `PathBuf`s are cloned into the GTK
    // activation closure and the tokio worker thread so both bind /
    // connect against the same paths. `load_paths_config` never
    // panics (missing files / malformed TOML / errored env source
    // all fall back to `PathsConfig::default()`), so the resolver
    // here can fail only when the XDG cascade itself is unwritable —
    // which is exactly the same failure mode the pre-fix GUI hit on
    // the `connect_activate` path. Issue #64.
    let paths_cfg = paths::load_paths_config();
    let resolved_paths = match telora_common::paths::resolve(&paths_cfg) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            log::error!("Error resolving socket path: {}", e);
            std::process::exit(1);
        }
    };

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

        // Hand each tokio task its own clone of the resolved paths
        // (`PathBuf` is `Clone` and the `Arc` makes the inner
        // `ResolvedPaths` trivially shareable). The control server
        // bind path and the daemon-client connect path must agree,
        // so both come from the same `Arc<ResolvedPaths>` captured
        // before the threads are spawned.
        let resolved_for_tokio = Arc::clone(&resolved_paths);

        // Start Tokio Runtime in a separate thread
        // This happens AFTER GTK confirms we're the primary instance
        let tx_clone = tx.clone();
        let cfg_for_tokio = gui_config.clone();
        thread::spawn(move || {
            let rt = Runtime::new().expect("Failed to create Tokio runtime");
            rt.block_on(async {
                let resolved_for_control = Arc::clone(&resolved_for_tokio);
                let resolved_for_client = Arc::clone(&resolved_for_tokio);
                tokio::select! {
                    result = run_control_server(tx_clone.clone(), resolved_for_control) => {
                        if let Err(e) = result {
                            log::error!("Control server failed: {}", e);
                        }
                    }
                    _ = handle_daemon_commands(daemon_rx, tx_clone, cfg_for_tokio, resolved_for_client) => {}
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
                    AppAction::AutoStop => {
                        // Idempotent STOP. Only acts when the GUI is
                        // currently recording — duplicate `AUTO_STOP`
                        // deliveries (network blip, retry, double
                        // buffer flush) are no-ops. The `mode` is
                        // recovered from `current_mode` so the daemon
                        // still knows whether to TYPE or COPY.
                        if recording {
                            recording = false;
                            osd_clone.show("⏳ LÍMITE ALCANZADO", "orange");
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
    resolved_paths: Arc<ResolvedPaths>,
) {
    // Snapshot the daemon socket path once: every command in this
    // loop connects to the same address, and threading the path
    // through `SocketClient::send_command` lets the GUI honour
    // `[paths] socket_dir` / `TELORA_PATHS__SOCKET_DIR` for the
    // first time (issue #64). `PathBuf::clone` is cheap (one
    // `Arc`-style refcount bump), so doing it at the loop top
    // would also work; here we do it once outside the loop for a
    // tighter borrow on `resolved_paths`.
    let daemon_sock: PathBuf = resolved_paths.daemon_sock.clone();
    while let Some(cmd) = rx.recv().await {
        match cmd {
            DaemonCommand::Start => {
                let _ = SocketClient::send_command("START", &daemon_sock).await;
            }
            DaemonCommand::Stop { mode, response_tx } => {
                // The STOP command now returns the transcription result directly
                match SocketClient::send_command("STOP", &daemon_sock).await {
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
                let _ = SocketClient::send_command("CANCEL", &daemon_sock).await;
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

async fn run_control_server(
    tx: Sender<AppAction>,
    resolved_paths: Arc<ResolvedPaths>,
) -> anyhow::Result<()> {
    // Use the operator-supplied control-socket path resolved at
    // startup from the `[paths]` cascade + `TELORA_PATHS__*` env
    // vars (issue #64). Clone-once keeps the loop body free of
    // borrow juggling on `resolved_paths`.
    let control_sock: PathBuf = resolved_paths.control_sock.clone();
    let server = ControlServer::bind(&control_sock)?;
    info!("Control server listening on {}...", control_sock.display());

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
                        let _ = tx.send(AppAction::AutoStop).await;
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
