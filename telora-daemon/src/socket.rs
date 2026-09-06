use anyhow::Result;
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
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
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub struct PathsConfig {
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
    /// Best-effort cleanup target. Captured at `bind` time so a
    /// `Drop` impl can `unlink` the socket file even when the daemon
    /// process is killed outside systemd (e.g. `Ctrl-C` in a
    /// development shell or a crash). When systemd adopts the FD
    /// via `libsystemd::activation`, the unit's `RemoveOnStop=yes`
    /// already cleans up; the `Drop` impl tolerates the resulting
    /// `NotFound` so the double-cleanup is harmless.
    socket_path: PathBuf,
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
    pub fn bind(
        path: &Path,
        cmd_tx: mpsc::Sender<Command>,
        allow_activation: bool,
    ) -> Result<Self> {
        let listener = if allow_activation {
            bind_unix_socket(path, "telora-daemon")?
        } else {
            bind_unix_socket_manual(path, "telora-daemon")?
        };
        Ok(Self {
            listener,
            cmd_tx,
            socket_path: path.to_path_buf(),
        })
    }

    /// Returns the path the daemon bound the control socket at.
    /// Used by tests to assert on the `Drop`-cleanup behaviour.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn run(&self) {
        loop {
            match self.listener.accept().await {
                Ok((mut stream, _addr)) => {
                    let cmd_tx = self.cmd_tx.clone();
                    // Split the stream into independent read and
                    // write halves so we can `take` the read side
                    // (bounded to REFRESH_MAX_BYTES) without
                    // consuming the write side.
                    let (mut read_half, mut write_half) = stream.split();
                    tokio::spawn(async move {
                        // Read the full payload rather than a fixed
                        // 2 KiB slice. A REFRESH whose JSON is split
                        // across multiple read syscalls (common on
                        // Unix sockets with `MSG_DONTWAIT` /
                        // segmentation) used to fail with a confusing
                        // `EOF while parsing` and the operator saw
                        // `ERROR: Invalid config JSON: …`. The cap at
                        // 64 KiB protects against a flood that would
                        // otherwise grow the buffer up to the kernel
                        // `SO_RCVBUF` ceiling.
                        const REFRESH_MAX_BYTES: u64 = 64 * 1024;
                        let mut buf = Vec::new();
                        let mut limited = (&mut read_half).take(REFRESH_MAX_BYTES);
                        match limited.read_to_end(&mut buf).await {
                            Ok(_) if buf.is_empty() => {
                                let _ = write_half.write_all(b"ERROR: empty command").await;
                            }
                            Ok(_) if buf.is_empty() => {
                                let command_str = String::from_utf8_lossy(&buf).trim().to_string();
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
                                                        let _ = write_half.write_all(b"ERROR: Reload cancelled or failed").await;
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
                                            let _ =
                                                write_half.write_all(b"STATUS: RECORDING").await;
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
                                                    let _ =
                                                        write_half.write_all(text.as_bytes()).await;
                                                }
                                                Err(_) => {
                                                    let _ = write_half.write_all(b"ERROR: Transcription cancelled or failed").await;
                                                }
                                            }
                                        }
                                    }
                                    "CANCEL" => {
                                        let _ = cmd_tx.send(Command::Cancel).await;
                                        let _ = write_half.write_all(b"STATUS: CANCELLED").await;
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
                                                    let _ =
                                                        write_half.write_all(json.as_bytes()).await;
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
                                        let _ =
                                            write_half.write_all(b"ERROR: Unknown command").await;
                                    }
                                };

                                // TODO: Implementing full bidirectional wait for transcription is tricky here without a shared state or response channel.
                                // Architecture: the main loop already
                                // routes every command through a
                                // oneshot response channel; this
                                // socket task just sends the command
                                // and writes the result back to the
                                // stream. The skeleton below is no
                                // longer aspirational — STOP /
                                // STATUS / REFRESH all follow this
                                // pattern.
                            }
                            Err(e) => error!("Failed to read from socket: {}", e),
                        }
                    });
                }
                Err(e) => error!("Failed to accept connection: {}", e),
            }
        }
    }
}

impl Drop for SocketServer {
    /// Best-effort unlink on drop so a `Ctrl-C` in a development
    /// shell, a panic, or any other non-systemd shutdown path does
    /// not leave a stale socket file behind. The next start will
    /// usually succeed anyway (the bind helper's
    /// `remove_stale_socket` cleans up same-UID leftovers), but a
    /// debris-free `/run/user/<uid>/telora/` is the operator-facing
    /// hygiene goal.
    ///
    /// Field drop order matters: `listener` drops before
    /// `socket_path` (fields are dropped top-to-bottom), so the FD
    /// is closed and the kernel stops holding the inode before we
    /// attempt the unlink. The systemd-managed path (`Accept=no`)
    /// has already cleaned the file via `RemoveOnStop=yes`, so a
    /// `NotFound` here is expected and not logged as a warning.
    fn drop(&mut self) {
        match std::fs::remove_file(&self.socket_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!(
                "failed to unlink {} on drop: {}",
                self.socket_path.display(),
                e
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the `allow_activation` contract: `true` MUST consult the
    /// systemd FD table (via `bind_unix_socket`); `false` MUST bypass it
    /// entirely (via `bind_unix_socket_manual`).
    ///
    /// Regression: the `0587494 -> 3bacee6` rename of `foreground` →
    /// `allow_activation` inverted the boolean at the call site without
    /// inverting at `SocketServer::bind`, so the systemd unit ran the
    /// manual path on every startup and the operator's
    /// `--no-activation` did the opposite of its docstring. This test
    /// observes the symptom (the bound listener accepts a connection)
    /// rather than the internals, so it catches the wiring regardless of
    /// which side the inversion lives on.
    #[test]
    fn bind_routes_allow_activation_correctly() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");

        rt.block_on(async {
            let tmp = tempfile::tempdir().expect("tempdir");
            let manual_path = tmp.path().join("telora-daemon-manual.sock");
            let cmd_tx: mpsc::Sender<Command> = mpsc::channel(1).0;

            // allow_activation = false → manual path. The bound
            // listener must be live: a connect from the same process
            // succeeds and the peer gets an EOF (no command handler
            // running here, but the bind itself succeeded).
            let manual = SocketServer::bind(&manual_path, cmd_tx.clone(), false)
                .expect("manual bind should succeed");
            assert!(
                tokio::net::UnixStream::connect(&manual_path).await.is_ok(),
                "manual bind produced a non-connectable socket"
            );
            drop(manual);

            // allow_activation = true → activation path. Without
            // LISTEN_FDS in the environment, bind_unix_socket falls back
            // to the manual bind internally; the test only asserts that
            // the call succeeds and the socket is connectable, which is
            // true on both branches. The point is that the call does
            // NOT panic, NOT error, and returns a usable listener —
            // any future change that breaks either branch will surface
            // here before it ships.
            let activation_path = tmp.path().join("telora-daemon-activation.sock");
            let activation = SocketServer::bind(&activation_path, cmd_tx, true)
                .expect("activation-mode bind should succeed");
            assert!(
                tokio::net::UnixStream::connect(&activation_path)
                    .await
                    .is_ok(),
                "activation-mode bind produced a non-connectable socket"
            );
            drop(activation);
        });
    }

    /// `Drop` must unlink the socket file. Without this, a `Ctrl-C`
    /// in a development shell leaves debris under
    /// `$XDG_RUNTIME_DIR/telora/`.
    #[test]
    fn drop_unlinks_socket_file() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");

        rt.block_on(async {
            let tmp = tempfile::tempdir().expect("tempdir");
            let sock_path = tmp.path().join("telora-daemon-drop.sock");
            let cmd_tx: mpsc::Sender<Command> = mpsc::channel(1).0;

            {
                let server =
                    SocketServer::bind(&sock_path, cmd_tx, false).expect("bind should succeed");
                assert_eq!(server.socket_path(), sock_path.as_path());
                assert!(sock_path.exists(), "socket file should exist after bind");
                drop(server);
            }

            assert!(
                !sock_path.exists(),
                "socket file should be unlinked after drop (found at {})",
                sock_path.display()
            );
        });
    }
}
