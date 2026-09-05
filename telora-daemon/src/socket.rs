use anyhow::Result;
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::path::Path;
use telora_common::socket_bind::{bind_unix_socket, bind_unix_socket_manual};
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
/// [`voxora_bridge::EngineFamily::from_config`].
///
/// `Default` is derived so a flattened top-level field can supply
/// an empty `SttConfig` when the user's `telora.toml` omits every
/// STT key (see [`DaemonConfig`]). All fields also carry
/// `#[serde(default)]` because `#[serde(flatten)]` does not invoke
/// the inner struct's `Default` for partially-present data — every
/// missing field needs its own default to keep existing partial
/// `telora.toml` files (e.g. those that omit `model_path`) loading.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct SttConfig {
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub model_kind: String,
    #[serde(default)]
    pub model_path: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub max_recording_seconds: u32,
}

/// `[paths]` section of `telora.toml`. All fields are optional; an
/// empty section (or its absence) falls back to the resolver in
/// [`crate::paths`].
///
/// Sub-issue #34 is the consumer of these fields; until that wiring
/// lands the resolver is the only thing that touches them.
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub struct PathsConfig {
    #[serde(default)]
    pub runtime_dir: Option<String>,
    #[serde(default)]
    pub socket_dir: Option<String>,
    #[serde(default)]
    pub daemon_socket: Option<String>,
    #[serde(default)]
    pub control_socket: Option<String>,
}

/// Top-level daemon configuration rooted at `telora.toml`.
///
/// STT fields (`model_id`, `model_kind`, `language`, etc.) are kept
/// at the top level for backwards compatibility with the original
/// `telora.toml` format. The `[paths]` overrides are a separate
/// section added in EPIC #27.
#[derive(Debug, Clone, Deserialize)]
pub struct DaemonConfig {
    /// STT configuration, inlined at the top level via
    /// `#[serde(flatten)]` so existing `telora.toml` files with
    /// flat `model_id = "..."` continue to work.
    #[serde(flatten, default)]
    pub stt: SttConfig,
    /// `[paths]` section. See [`PathsConfig`].
    #[serde(default)]
    pub paths: PathsConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            stt: default_stt_config(),
            paths: PathsConfig::default(),
        }
    }
}

/// Default values for [`SttConfig`] when the user's `telora.toml`
/// does not supply them. Matches the legacy defaults so an upgrade
/// from a Whisper-only install still boots a working daemon.
pub fn default_stt_config() -> SttConfig {
    SttConfig {
        model_id: "ggerganov/whisper.cpp/ggml-base.bin".to_string(),
        model_kind: "whisper".to_string(),
        model_path: String::new(),
        language: "es".to_string(),
        max_recording_seconds: 600,
    }
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
    /// Reload the STT configuration. Handled atomically: the new
    /// `BridgeTranscriber` is built FIRST and the `stt_config`
    /// mutation is committed only on success — see
    /// `main::Command::ReloadConfig` for the failure semantics.
    ReloadConfig {
        new_config: SttConfig,
        response_tx: oneshot::Sender<Result<()>>,
    },
}

#[derive(Debug)]
pub struct SocketServer {
    listener: UnixListener,
    cmd_tx: mpsc::Sender<Command>,
}

impl SocketServer {
    /// Bind the daemon's control Unix socket at `path`. Delegates
    /// parent-directory creation, stale-socket ownership checks, Linux
    /// parent-path pinning, and permission tightening to
    /// [`telora_common::socket_bind::bind_unix_socket`], which is the
    /// single source of truth shared with `telora-gui`. The
    /// `instance_name` tag is hard-coded to `"telora-daemon"` so the
    /// EADDRINUSE remediation hint points the operator at
    /// `systemctl --user status telora-daemon` instead of the GUI's
    /// "previous session" hint.
    pub fn bind(path: &Path, cmd_tx: mpsc::Sender<Command>, foreground: bool) -> Result<Self> {
        let listener = if foreground {
            bind_unix_socket_manual(path, "telora-daemon")?
        } else {
            bind_unix_socket(path, "telora-daemon")?
        };
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
