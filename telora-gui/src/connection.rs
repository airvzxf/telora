use anyhow::{Context, Result};
use std::path::Path;
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
        Ok(Self { listener })
    }

    pub async fn next_command(&self) -> Result<String> {
        let (mut stream, _) = self.listener.accept().await?;
        let mut buf = [0; 1024];
        let n = stream.read(&mut buf).await?;
        Ok(String::from_utf8_lossy(&buf[..n]).trim().to_string())
    }
}
