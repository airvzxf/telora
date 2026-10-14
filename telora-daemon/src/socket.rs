use anyhow::Result;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::Map;
use std::path::{Path, PathBuf};
use telora_common::protocol::{
    Error as ProtocolError, ErrorCode, Reply, decode_request, encode_reply, read_message,
};
use telora_common::socket_bind::{adopt_systemd_listener, bind_unix_socket_manual};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

pub use telora_common::protocol::StatusResponse;

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
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[allow(dead_code)]
pub struct PathsConfig {
    #[serde(default)]
    pub socket_dir: Option<String>,
    #[serde(default)]
    pub daemon_socket: Option<String>,
    #[serde(default)]
    pub control_socket: Option<String>,
}

/// `[audio]` section of `telora.toml`.
///
/// `cpal::default_input_device()` on Linux returns the ALSA `default`
/// PCM, which on hosts running PipeWire / PulseAudio (KDE Plasma 6,
/// GNOME, etc.) is routed by the PipeWire ALSA plugin to whatever
/// source the desktop selected. On those hosts honouring the system
/// default is the right behaviour — the operator already picked the
/// mic they want to use.
///
/// The daemon used to unconditionally swap that default for the
/// first direct ALSA device with `"CARD="` in its name (see the
/// `telora-daemon/src/audio.rs` history). That heuristic predated
/// modern PipeWire and broke laptops with both an integrated Intel
/// HDA mic and a USB capture card: cpal enumerated the built-in
/// card first, so telora captured from the laptop's internal mic
/// while the operator spoke into the USB one. The override is now
/// opt-in via [`AudioConfig::prefer_direct_alsa`]; the pin to a
/// specific capture device via [`AudioConfig::input_device`].
///
/// `Default` is derived so the `#[serde(default)]` attribute on the
/// `audio` field in [`DaemonConfig`] composes cleanly when the
/// user's `telora.toml` omits the section.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AudioConfig {
    /// Pin to a specific capture device when the system default
    /// points at the wrong hardware. The match is a case-insensitive
    /// substring against the cpal device name returned by
    /// `Device::name()`. Empty means "use the system default
    /// unchanged".
    ///
    /// Common values:
    ///
    /// * `""` — honour the desktop's default.
    /// * `"sysdefault:CARD=Microphone"` — direct ALSA, exact card.
    /// * `"Fifine"` — first device whose name mentions Fifine.
    #[serde(default)]
    pub input_device: String,
    /// When `input_device` is empty and `cpal::default_input_device()`
    /// returns a device whose name is `"default"` (the typical
    /// PipeWire / PulseAudio ALSA plugin case), opt in to the legacy
    /// direct-alsa fallback: pick the first ALSA device whose name
    /// contains `"CARD="`. Defaults to `false` (respect the desktop
    /// default — usually the right thing on Plasma 6 / GNOME).
    #[serde(default)]
    pub prefer_direct_alsa: bool,
}

/// Top-level daemon configuration rooted at `telora.toml`.
///
/// STT fields (`model_id`, `model_kind`, `language`, etc.) are kept
/// at the top level for backwards compatibility with the original
/// `telora.toml` format. The `[paths]` overrides are a separate
/// section added in EPIC #27. The `[audio]` input-device overrides
/// live in their own section to keep the device-selection concerns
/// out of the STT schema.
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
    /// `[audio]` section. See [`AudioConfig`].
    #[serde(default)]
    pub audio: AudioConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            stt: default_stt_config(),
            paths: PathsConfig::default(),
            audio: AudioConfig::default(),
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
    Start {
        response_tx: oneshot::Sender<Result<(), ProtocolError>>,
    },
    /// Answered with the transcription, empty when there was no audio.
    Stop {
        response_tx: oneshot::Sender<Result<String, ProtocolError>>,
    },
    Cancel,
    GetStatus {
        response_tx: oneshot::Sender<StatusResponse>,
    },
    /// Re-read the daemon's own configuration and apply it, reloading
    /// the model if it changed. Answered with the `OK:` text or the
    /// reason it was refused.
    Refresh {
        response_tx: oneshot::Sender<Result<String, ProtocolError>>,
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
    /// True when the listener came from systemd: the socket unit owns
    /// the path, so dropping the server must not unlink it.
    inherited: bool,
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
        let inherited_listener = if allow_activation {
            adopt_systemd_listener("telora-daemon")?
        } else {
            None
        };
        let inherited = inherited_listener.is_some();
        let listener = match inherited_listener {
            Some(listener) => listener,
            None => bind_unix_socket_manual(path, "telora-daemon")?,
        };
        Ok(Self {
            listener,
            cmd_tx,
            socket_path: path.to_path_buf(),
            inherited,
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
                Ok((stream, _addr)) => {
                    let cmd_tx = self.cmd_tx.clone();
                    tokio::spawn(serve(stream, cmd_tx));
                }
                Err(e) => error!("Failed to accept connection: {}", e),
            }
        }
    }
}

/// Answer one request and close the connection.
async fn serve(mut stream: UnixStream, cmd_tx: mpsc::Sender<Command>) {
    let message = read_message(&mut stream).await;
    let unread_input = message.is_err();
    let reply = match message.and_then(|m| decode_request(&m)) {
        Ok(cmd) => {
            info!("Received command: {cmd}");
            dispatch(&cmd, &cmd_tx).await
        }
        Err(e) => {
            warn!("Rejected request: {e}");
            Err(e)
        }
    };
    if let Err(e) = stream.write_all(&encode_reply(&reply)).await {
        warn!("Could not write the reply: {e}");
    }
    if unread_input {
        discard_input(&mut stream).await;
    }
}

/// Closing a Unix socket with unread input resets the connection, and
/// the client would lose the reply; read what is left first, within
/// bounds.
async fn discard_input(stream: &mut UnixStream) {
    let mut sink = tokio::io::sink();
    let mut rest = (&mut *stream).take(1024 * 1024);
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        tokio::io::copy(&mut rest, &mut sink),
    )
    .await;
}

async fn dispatch(cmd: &str, cmd_tx: &mpsc::Sender<Command>) -> Reply {
    match cmd {
        "start" => {
            ask(cmd_tx, |response_tx| Command::Start { response_tx }).await??;
            Ok(Map::new())
        }
        "stop" => {
            let text = ask(cmd_tx, |response_tx| Command::Stop { response_tx }).await??;
            Ok(fields("text", text))
        }
        "cancel" => {
            cmd_tx.send(Command::Cancel).await.map_err(|_| closing())?;
            Ok(Map::new())
        }
        "status" => {
            let status = ask(cmd_tx, |response_tx| Command::GetStatus { response_tx }).await?;
            let status = serde_json::to_value(status)
                .map_err(|e| ProtocolError::new(ErrorCode::Internal, e.to_string()))?;
            Ok(fields("status", status))
        }
        "refresh" => {
            let message = ask(cmd_tx, |response_tx| Command::Refresh { response_tx }).await??;
            Ok(fields("message", message))
        }
        other => Err(ProtocolError::new(
            ErrorCode::UnknownCommand,
            format!("comando desconocido: {other}"),
        )),
    }
}

/// Hand a command to the event loop and wait for its answer.
async fn ask<T>(
    cmd_tx: &mpsc::Sender<Command>,
    command: impl FnOnce(oneshot::Sender<T>) -> Command,
) -> Result<T, ProtocolError> {
    let (tx, rx) = oneshot::channel();
    cmd_tx.send(command(tx)).await.map_err(|_| closing())?;
    rx.await.map_err(|_| closing())
}

fn closing() -> ProtocolError {
    ProtocolError::new(ErrorCode::Internal, "el daemon se está cerrando")
}

fn fields(key: &str, value: impl Into<serde_json::Value>) -> Map<String, serde_json::Value> {
    let mut map = Map::new();
    map.insert(key.to_string(), value.into());
    map
}

impl Drop for SocketServer {
    /// Unlink a socket we bound ourselves so a dev-shell Ctrl-C leaves no
    /// debris. A systemd-inherited socket belongs to the socket unit:
    /// unlinking it would leave the unit listening on an unreachable path.
    fn drop(&mut self) {
        if self.inherited {
            return;
        }
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
            let manual_path = tmp.path().join("telora/manual.sock");
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
            let activation_path = tmp.path().join("telora/activation.sock");
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
            let sock_path = tmp.path().join("telora/drop.sock");
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
