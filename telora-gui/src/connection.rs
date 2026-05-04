use anyhow::{Context, Result};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

pub const DAEMON_SOCKET: &str = "/tmp/telora-sock";
pub const CONTROL_SOCKET: &str = "/tmp/telora-control.sock";

pub struct SocketClient;

impl SocketClient {
    pub async fn send_command(cmd: &str) -> Result<String> {
        let mut stream = UnixStream::connect(DAEMON_SOCKET)
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

pub struct ControlServer {
    listener: UnixListener,
}

impl ControlServer {
    pub fn bind() -> Result<Self> {
        if Path::new(CONTROL_SOCKET).exists() {
            let _ = std::fs::remove_file(CONTROL_SOCKET);
        }

        let listener =
            UnixListener::bind(CONTROL_SOCKET).context("Failed to bind control socket")?;
        Ok(Self { listener })
    }

    pub async fn next_command(&self) -> Result<String> {
        let (mut stream, _) = self.listener.accept().await?;
        let mut buf = [0; 1024];
        let n = stream.read(&mut buf).await?;
        Ok(String::from_utf8_lossy(&buf[..n]).trim().to_string())
    }
}
