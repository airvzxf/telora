use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use telora_common::socket_bind::bind_unix_socket;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

pub struct SocketClient;

impl SocketClient {
    /// Send `cmd` to the daemon over its Unix socket and read back
    /// the full response. The daemon socket path is plumbed in by
    /// the caller (issue #64): the GUI resolves it once at startup
    /// from the `[paths]` cascade and reuses the same `PathBuf`
    /// across every command in a session, so ad-hoc `./telora-gui`
    /// invocations honour `telora.toml [paths]` / `TELORA_PATHS__*`
    /// for the first time.
    pub async fn send_command(cmd: &str, daemon_sock: &Path) -> Result<String> {
        let mut stream = UnixStream::connect(daemon_sock)
            .await
            .context("Failed to connect to daemon")?;
        stream
            .write_all(cmd.as_bytes())
            .await
            .context("Failed to send command")?;

        // Wait for response
        let mut buf = Vec::new();
        stream
            .read_to_end(&mut buf)
            .await
            .context("Failed to read response from daemon")?;
        Ok(String::from_utf8_lossy(&buf).to_string())
    }
}

#[derive(Debug)]
pub struct ControlServer {
    listener: UnixListener,
    socket_path: PathBuf,
}

impl ControlServer {
    /// Bind the GUI's control Unix socket at `path`. Delegates
    /// parent-directory creation, stale-socket ownership checks, Linux
    /// parent-path pinning, and permission tightening to
    /// [`telora_common::socket_bind::bind_unix_socket`], which is the
    /// single source of truth shared with `telora-daemon`. The
    /// `instance_name` tag is hard-coded to `"telora-gui"` so the
    /// EADDRINUSE remediation hint points at a previous GUI session
    /// (the daemon's `systemctl --user status telora-daemon` hint
    /// would be wrong here).
    pub fn bind(path: &Path) -> Result<Self> {
        let listener = bind_unix_socket(path, "telora-gui")?;
        Ok(Self {
            listener,
            socket_path: path.to_path_buf(),
        })
    }

    /// Returns the path the GUI bound the control socket at.
    /// Used by tests to assert on the `Drop`-cleanup behaviour.
    #[allow(dead_code)]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn next_command(&self) -> Result<String> {
        let (mut stream, _) = self.listener.accept().await?;
        let mut buf = [0; 1024];
        let n = stream.read(&mut buf).await?;
        Ok(String::from_utf8_lossy(&buf[..n]).trim().to_string())
    }
}

impl Drop for ControlServer {
    /// Best-effort unlink on drop so a `Ctrl-C` in a development
    /// shell, a panic, or any other non-systemd shutdown path does
    /// not leave a stale socket file behind.
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
