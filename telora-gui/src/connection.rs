use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use telora_common::paths;
use telora_common::socket_bind::bind_unix_socket;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

pub struct SocketClient;

impl SocketClient {
    pub async fn send_command(cmd: &str) -> Result<String> {
        let mut stream = UnixStream::connect(paths::daemon_socket_path())
            .await
            .context("Failed to connect to daemon")?;
        stream
            .write_all(cmd.as_bytes())
            .await
            .context("Failed to send command")?;

        // Half-close the write side so the server's `read_to_end`
        // (telora-daemon/src/socket.rs:200-203) reaches EOF and
        // proceeds to write the response. Without this the server
        // hangs forever waiting for the client's EOF; introduced by
        // PR #132 (ed326d2). Symptom: any `SocketClient::send_command`
        // call blocks indefinitely after the write, and the user sees
        // a frozen GUI / hotkey wrapper.
        stream
            .shutdown()
            .await
            .context("Failed to half-close write side of daemon socket")?;

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
