use anyhow::{Context, Result};
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

/// Status payload returned to clients over the unix socket.
///
/// `model_kind` is the voxora engine family (`whisper` or
/// `qwen3-asr`). `model_id` is the Hugging Face identifier the
/// daemon loaded (e.g. `ggerganov/whisper.cpp/ggml-base.bin`).
/// `model_path` is the resolved local file/directory the engine
/// actually loaded from — kept for the GUI's status display.
#[derive(Debug, Serialize, Deserialize)]
pub struct StatusResponse {
    pub active: bool,
    pub pid: u32,
    pub model_id: String,
    pub model_kind: String,
    pub model_path: String,
    pub language: String,
    pub max_recording_seconds: u32,
    pub state: String,
}

/// Configuration the daemon reads from `telora.toml` (or
/// `TELORA_*` env vars).
///
/// `model_kind` is a free-form string at the JSON layer so we can
/// pass it verbatim over the socket; the daemon validates it via
/// [`voxora_bridge::ModelKind::from_config`].
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SttConfig {
    pub model_id: String,
    pub model_kind: String,
    pub model_path: String,
    pub language: String,
    pub max_recording_seconds: u32,
}

#[derive(Debug)]
pub enum Command {
    Start,
    Stop {
        response_tx: oneshot::Sender<String>,
    },
    Cancel,
    GetStatus {
        response_tx: oneshot::Sender<StatusResponse>,
    },
    ReloadConfig {
        new_config: SttConfig,
        response_tx: oneshot::Sender<Result<()>>,
    },
}

pub struct SocketServer {
    listener: UnixListener,
    cmd_tx: mpsc::Sender<Command>,
}

impl SocketServer {
    pub fn bind(path: &str, cmd_tx: mpsc::Sender<Command>) -> Result<Self> {
        if std::fs::metadata(path).is_ok() {
            info!("Removing existing socket file: {}", path);
            std::fs::remove_file(path).context("Failed to remove existing socket")?;
        }

        let listener = UnixListener::bind(path).context("Failed to bind unix socket")?;

        // Set permissions to 0600 (owner read/write only)
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms).context("Failed to set socket permissions")?;

        info!("Listening on unix socket: {} (restricted to 0600)", path);

        Ok(Self { listener, cmd_tx })
    }

    pub async fn run(&self) {
        loop {
            match self.listener.accept().await {
                Ok((mut stream, _addr)) => {
                    let cmd_tx = self.cmd_tx.clone();
                    tokio::spawn(async move {
                        let mut buf = [0; 2048]; // Increased buffer size for config JSON
                        match stream.read(&mut buf).await {
                            Ok(n) if n > 0 => {
                                let command_str =
                                    String::from_utf8_lossy(&buf[..n]).trim().to_string();
                                info!("Received command: {}", command_str);

                                if command_str.starts_with("REFRESH") {
                                    let json_part =
                                        command_str.strip_prefix("REFRESH").unwrap_or("").trim();
                                    match serde_json::from_str::<SttConfig>(json_part) {
                                        Ok(new_config) => {
                                            let (tx, rx) = oneshot::channel();
                                            if let Err(e) = cmd_tx
                                                .send(Command::ReloadConfig {
                                                    new_config,
                                                    response_tx: tx,
                                                })
                                                .await
                                            {
                                                error!("Failed to send reload command: {}", e);
                                                let _ = stream
                                                    .write_all(b"ERROR: Internal channel error")
                                                    .await;
                                            } else {
                                                match rx.await {
                                                    Ok(Ok(())) => {
                                                        let _ = stream
                                                            .write_all(b"OK: Config reloaded")
                                                            .await;
                                                    }
                                                    Ok(Err(e)) => {
                                                        let _ = stream
                                                            .write_all(
                                                                format!("ERROR: {}", e).as_bytes(),
                                                            )
                                                            .await;
                                                    }
                                                    Err(_) => {
                                                        let _ = stream.write_all(b"ERROR: Reload cancelled or failed").await;
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            error!("Failed to parse config JSON: {}", e);
                                            let _ = stream
                                                .write_all(
                                                    format!("ERROR: Invalid config JSON: {}", e)
                                                        .as_bytes(),
                                                )
                                                .await;
                                        }
                                    }
                                    return;
                                }

                                match command_str.as_str() {
                                    "START" => {
                                        if let Err(e) = cmd_tx.send(Command::Start).await {
                                            error!("Failed to send start command: {}", e);
                                            let _ = stream
                                                .write_all(b"ERROR: Internal channel error")
                                                .await;
                                        } else {
                                            let _ = stream.write_all(b"STATUS: RECORDING").await;
                                        }
                                    }
                                    "STOP" => {
                                        let (tx, rx) = oneshot::channel();
                                        if let Err(e) =
                                            cmd_tx.send(Command::Stop { response_tx: tx }).await
                                        {
                                            error!("Failed to send stop command: {}", e);
                                            let _ = stream
                                                .write_all(b"ERROR: Internal channel error")
                                                .await;
                                        } else {
                                            // Wait for the transcription result from the main loop
                                            match rx.await {
                                                Ok(text) => {
                                                    let _ = stream.write_all(text.as_bytes()).await;
                                                }
                                                Err(_) => {
                                                    let _ = stream.write_all(b"ERROR: Transcription cancelled or failed").await;
                                                }
                                            }
                                        }
                                    }
                                    "CANCEL" => {
                                        let _ = cmd_tx.send(Command::Cancel).await;
                                        let _ = stream.write_all(b"STATUS: CANCELLED").await;
                                    }
                                    "STATUS" => {
                                        let (tx, rx) = oneshot::channel();
                                        if let Err(e) = cmd_tx
                                            .send(Command::GetStatus { response_tx: tx })
                                            .await
                                        {
                                            error!("Failed to send status command: {}", e);
                                            let _ = stream
                                                .write_all(b"ERROR: Internal channel error")
                                                .await;
                                        } else {
                                            match rx.await {
                                                Ok(status) => {
                                                    let json = serde_json::to_string(&status)
                                                        .unwrap_or_else(|_| "{}".to_string());
                                                    let _ = stream.write_all(json.as_bytes()).await;
                                                }
                                                Err(_) => {
                                                    let _ = stream
                                                        .write_all(b"ERROR: Failed to get status")
                                                        .await;
                                                }
                                            }
                                        }
                                    }
                                    _ => {
                                        let _ = stream.write_all(b"ERROR: Unknown command").await;
                                    }
                                };

                                // TODO: Implementing full bidirectional wait for transcription is tricky here without a shared state or response channel.
                                // Quick fix: The main loop will handle the logic, but how does it send back to THIS stream?
                                // Architecture choice:
                                // 1. Client connects, sends STOP, waits.
                                // 2. Socket task sends StopRecording to Main.
                                // 3. Socket task waits for Result from Main (via oneshot channel?).
                                // 4. Socket task writes Result to Stream.
                                //
                                // Let's implement that pattern in the next step (Main).
                                // For now, this is a good skeleton.
                            }
                            Ok(_) => {} // EOF
                            Err(e) => error!("Failed to read from socket: {}", e),
                        }
                    });
                }
                Err(e) => error!("Failed to accept connection: {}", e),
            }
        }
    }
}
