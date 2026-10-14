use async_channel::Sender;
use gtk4::prelude::*;
use gtk4::{Application, glib};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use log::{info, warn};

mod clipboard;
mod config;
mod connection;
mod session;
mod text;
mod tray;
mod ui;

use config::GuiConfig;
use connection::ControlServer;
use session::{Effect, Event, Session};
use telora_common::paths::ResolvedPaths;
use telora_common::protocol::{self, ErrorCode, Reply};
use tray::{TrayCommand, TrayHandle, TrayState};
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
    Event(Event),
    QueryStatus,
    CopyLast,
}

#[derive(Debug)]
enum DaemonCommand {
    Start { response_tx: Sender<AppAction> },
    Stop { response_tx: Sender<AppAction> },
    Cancel,
    CopyLast { response_tx: Sender<AppAction> },
    Status { response_tx: Sender<AppAction> },
}

fn main() {
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        let resolved_paths = match telora_common::config::resolve_paths() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{e:#}");
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

    let resolved_paths = match telora_common::config::resolve_paths() {
        Ok(r) => Arc::new(r),
        Err(e) => {
            log::error!("{e:#}");
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
                    _ = handle_daemon_commands(daemon_rx, resolved_for_client) => {}
                }
            });
        });

        // ----- Tray icon wiring (closes #193) -----
        //
        // The tray needs its own runtime because ksni's `spawn().await`
        // must be called from inside a tokio runtime context, and the
        // GTK main loop does not provide one. A dedicated OS thread
        // with a single-threaded `tokio::Runtime` is the simplest
        // isolation: it runs the ksni background task for the lifetime
        // of the GUI and forwards user actions back through `tx` so
        // they hit the same `AppAction` pipeline as hotkey / CLI
        // triggers. State updates flow the other way via a
        // `std::sync::Mutex<Option<TrayHandle>>` shared with the GTK
        // loop — the lock is held only briefly to read the `Option`,
        // and reads return `None` until the tray finishes registering.
        let (tray_cmd_tx, tray_cmd_rx) = async_channel::unbounded::<TrayCommand>();
        let tray_handle_slot: Arc<std::sync::Mutex<Option<TrayHandle>>> =
            Arc::new(std::sync::Mutex::new(None));
        let tray_handle_slot_for_thread = Arc::clone(&tray_handle_slot);
        let tx_for_tray_thread = tx.clone();
        let tray_cmd_rx_for_thread = tray_cmd_rx.clone();

        if !gui_config.enable_tray {
            info!("Tray disabled by gui.toml (enable_tray = false)");
        }
        let enable_tray = gui_config.enable_tray;
        thread::spawn(move || {
            if !enable_tray {
                return;
            }
            let rt = Runtime::new().expect("Failed to create tray tokio runtime");
            rt.block_on(async move {
                match tray::spawn_tray(tray_cmd_tx).await {
                    Ok(Some(handle)) => {
                        // Hand the handle to the GTK loop. The lock is
                        // uncontended in practice (only the GTK loop
                        // ever reads it), but we still wrap it in a
                        // `Mutex` so the `Option` write is sound.
                        if let Ok(mut slot) = tray_handle_slot_for_thread.lock() {
                            *slot = Some(handle);
                        }
                        info!("Tray command dispatcher started; awaiting menu events");
                        while let Ok(cmd) = tray_cmd_rx_for_thread.recv().await {
                            let action = match cmd {
                                TrayCommand::Toggle => AppAction::Event(Event::Toggle),
                                TrayCommand::MenuCancel => AppAction::Event(Event::Cancel),
                                TrayCommand::MenuCopyLast => AppAction::CopyLast,
                                TrayCommand::MenuStatus => AppAction::QueryStatus,
                                TrayCommand::MenuQuit => {
                                    info!("Quit requested from tray menu");
                                    // Exit cleanly so systemd --user can
                                    // restart us if the operator has an
                                    // `Restart=on-failure` policy.
                                    std::process::exit(0);
                                }
                            };
                            if tx_for_tray_thread.send(action).await.is_err() {
                                // GTK loop is gone — nothing left to do.
                                break;
                            }
                        }
                    }
                    Ok(None) => {
                        warn!(
                            "SNI tray not available; GUI running in OSD-only mode \
                             (closes #193 fallback path)"
                        );
                    }
                    Err(e) => {
                        warn!("Failed to spawn SNI tray icon ({e}); falling back to OSD-only mode");
                    }
                }
            });
        });
        // ----- end tray wiring -----

        let osd = Osd::new(app);
        let osd_clone = osd.clone();
        let tx_back = tx.clone();
        let tray_handle_for_loop = Arc::clone(&tray_handle_slot);

        glib::MainContext::default().spawn_local(async move {
            let tray_handle_clone = Arc::clone(&tray_handle_for_loop);
            let set_tray = move |state: TrayState| {
                if let Some(handle) = tray_handle_clone.lock().unwrap().as_ref() {
                    handle.set_state(state);
                }
            };
            set_tray(TrayState::Idle);

            let mut session = Session::default();
            while let Ok(action) = rx.recv().await {
                let event = match action {
                    AppAction::Event(event) => event,
                    AppAction::QueryStatus => {
                        let _ = daemon_tx.send(DaemonCommand::Status {
                            response_tx: tx_back.clone(),
                        });
                        continue;
                    }
                    AppAction::CopyLast => {
                        let _ = daemon_tx.send(DaemonCommand::CopyLast {
                            response_tx: tx_back.clone(),
                        });
                        continue;
                    }
                };
                for effect in session.handle(event) {
                    match effect {
                        Effect::SendStart => {
                            let _ = daemon_tx.send(DaemonCommand::Start {
                                response_tx: tx_back.clone(),
                            });
                        }
                        Effect::SendStop => {
                            let _ = daemon_tx.send(DaemonCommand::Stop {
                                response_tx: tx_back.clone(),
                            });
                        }
                        Effect::SendCancel => {
                            let _ = daemon_tx.send(DaemonCommand::Cancel);
                        }
                        Effect::ShowOsd { text, color } => osd_clone.show(&text, &color),
                        Effect::HideOsd => osd_clone.hide(),
                        Effect::HideOsdAfter { secs, generation } => {
                            let tx_timer = tx_back.clone();
                            glib::timeout_add_seconds_local(secs, move || {
                                let _ = tx_timer
                                    .send_blocking(AppAction::Event(Event::HideOsd { generation }));
                                glib::ControlFlow::Break
                            });
                        }
                        Effect::Tray(state) => set_tray(state),
                    }
                }
            }
        });
    });

    app.run();
}

async fn handle_daemon_commands(
    mut rx: mpsc::UnboundedReceiver<DaemonCommand>,
    resolved_paths: Arc<ResolvedPaths>,
) {
    let daemon_sock: PathBuf = resolved_paths.daemon_sock.clone();
    // Safety net: the last delivered text can be copied again from the tray
    // or with `telora last` if the clipboard was overwritten meanwhile.
    let mut last_text: Option<String> = None;
    while let Some(cmd) = rx.recv().await {
        match cmd {
            DaemonCommand::Start { response_tx } => {
                let event = match protocol::call(&daemon_sock, "start").await {
                    Ok(Ok(_)) => Event::StartAccepted,
                    Ok(Err(e)) => {
                        log::error!("Daemon refused START: {e}");
                        Event::StartRejected(session::short_error(&e.message))
                    }
                    Err(e) => {
                        log::error!("Failed to reach daemon for START: {e:#}");
                        Event::StartRejected("Daemon no disponible".to_string())
                    }
                };
                let _ = response_tx.send(AppAction::Event(event)).await;
            }
            DaemonCommand::Stop { response_tx } => {
                let event = stop_and_deliver(&daemon_sock, &mut last_text).await;
                let _ = response_tx.send(AppAction::Event(event)).await;
            }
            DaemonCommand::CopyLast { response_tx } => {
                let message = match &last_text {
                    None => "No hay transcripción previa".to_string(),
                    Some(text) => match clipboard::copy(text) {
                        Ok(()) => "Última transcripción copiada".to_string(),
                        Err(reason) => format!("✘ {reason}"),
                    },
                };
                let _ = response_tx
                    .send(AppAction::Event(Event::Info(message)))
                    .await;
            }
            DaemonCommand::Cancel => match protocol::call(&daemon_sock, "cancel").await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => log::error!("Daemon refused CANCEL: {e}"),
                Err(e) => log::error!("Failed to send CANCEL: {e:#}"),
            },
            DaemonCommand::Status { response_tx } => {
                let text = match protocol::call(&daemon_sock, "status").await {
                    Ok(reply) => describe_status(reply),
                    Err(e) => {
                        log::error!("Failed to reach daemon for STATUS: {e:#}");
                        "✘ Daemon no disponible".to_string()
                    }
                };
                let _ = response_tx.send(AppAction::Event(Event::Info(text))).await;
            }
        }
    }
}

/// Run STOP and put the text on the clipboard; the returned event says
/// what the user should see.
async fn stop_and_deliver(daemon_sock: &Path, last_text: &mut Option<String>) -> Event {
    let raw_text = match protocol::call(daemon_sock, "stop").await {
        Ok(reply) => match transcription(reply) {
            Ok(text) => text,
            Err(reason) => {
                log::error!("Daemon error: {reason}");
                return finished_error(&session::short_error(&reason));
            }
        },
        Err(e) => {
            log::error!("Failed to get result from daemon: {e:#}");
            return finished_error("Daemon no disponible");
        }
    };
    if raw_text.trim().is_empty() {
        return Event::Finished {
            message: "Sin texto".to_string(),
            color: session::COLOR_NEUTRAL.to_string(),
            hold_secs: 2,
            error: false,
        };
    }
    let cleaned = text::clean_transcription(&raw_text);
    *last_text = Some(cleaned.clone());
    match clipboard::copy(&cleaned) {
        Ok(()) => Event::Finished {
            message: "Copiado".to_string(),
            color: session::COLOR_OK.to_string(),
            hold_secs: 1,
            error: false,
        },
        Err(reason) => finished_error(&format!("{reason}; usa \"Copiar última transcripción\"")),
    }
}

/// The text of a STOP reply, or why there is none.
fn transcription(reply: Reply) -> Result<String, String> {
    let fields = reply.map_err(|e| e.message)?;
    fields
        .get("text")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| "respuesta del daemon sin texto".to_string())
}

fn finished_error(reason: &str) -> Event {
    Event::Finished {
        message: format!("✘ {reason}"),
        color: session::COLOR_ERROR.to_string(),
        hold_secs: session::ERROR_HOLD_SECS,
        error: true,
    }
}

/// One-line daemon summary for the tray's "Show status" item.
fn describe_status(reply: Reply) -> String {
    let status = match reply {
        Ok(mut fields) => fields.remove("status"),
        Err(e) => return format!("✘ {}", session::short_error(&e.message)),
    };
    let Some(Ok(status)) = status.map(serde_json::from_value::<protocol::StatusResponse>) else {
        return "✘ respuesta de estado mal formada".to_string();
    };
    let model = status.model_id.rsplit('/').next().unwrap_or("?");
    let engine = match status.engine.as_str() {
        "ready" => "listo",
        "loading" => "cargando modelo",
        other => other,
    };
    format!("{model} · {engine} · {}", status.state)
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
        let (request, mut stream) = match server.next_request().await {
            Ok(pair) => pair,
            Err(e) => {
                log::error!("Control server error: {e}");
                continue;
            }
        };
        let action = request.and_then(|cmd| {
            info!("Control command: {cmd}");
            control_action(&cmd)
        });
        let reply: Reply = match action {
            Ok(action) => {
                let _ = tx.send(action).await;
                Ok(Default::default())
            }
            Err(e) => {
                warn!("Rejected control request: {e}");
                Err(e)
            }
        };
        // The daemon's auto_stop sender may already be gone; ignore EPIPE.
        let _ = stream.write_all(&protocol::encode_reply(&reply)).await;
    }
}

fn control_action(cmd: &str) -> Result<AppAction, protocol::Error> {
    match cmd {
        "toggle_copy" => Ok(AppAction::Event(Event::Toggle)),
        "cancel" => Ok(AppAction::Event(Event::Cancel)),
        "auto_stop" => Ok(AppAction::Event(Event::AutoStop)),
        "last" => Ok(AppAction::CopyLast),
        other => Err(protocol::Error::new(
            ErrorCode::UnknownCommand,
            format!("comando desconocido: {other}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{describe_status, transcription};
    use telora_common::protocol::{Error, ErrorCode};

    fn fields(json: &str) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn status_summary_names_model_and_engine_state() {
        let reply = fields(
            r#"{"status":{"pid":1,"model_id":"ggerganov/whisper.cpp/ggml-large-v3.bin","model_kind":"whisper","model_path":"","language":"es","max_recording_seconds":600,"engine":"loading","state":"Idle"}}"#,
        );
        assert_eq!(
            describe_status(Ok(reply)),
            "ggml-large-v3.bin · cargando modelo · Idle"
        );
    }

    #[test]
    fn status_summary_surfaces_daemon_errors() {
        let error = Error::new(ErrorCode::Internal, "el daemon se está cerrando");
        assert_eq!(describe_status(Err(error)), "✘ el daemon se está cerrando");
    }

    // The GUI used to drop any dictation that began with "ERROR:".
    #[test]
    fn a_dictation_that_starts_with_error_is_kept() {
        let reply = Ok(fields(r#"{"text":"ERROR: no compila"}"#));
        assert_eq!(transcription(reply).unwrap(), "ERROR: no compila");
    }

    #[test]
    fn a_daemon_error_is_not_a_dictation() {
        let reply = Err(Error::new(ErrorCode::NoModel, "sin modelo"));
        assert_eq!(transcription(reply).unwrap_err(), "sin modelo");
    }
}
