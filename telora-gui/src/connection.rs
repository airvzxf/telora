use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;
use telora_common::protocol;
use telora_common::socket_bind::bind_unix_socket;
use tokio::net::{UnixListener, UnixStream};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

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

    /// Accept one client and read its request. The stream is returned so
    /// the caller can write the reply.
    pub async fn next_request(&self) -> Result<(Result<String, protocol::Error>, UnixStream)> {
        let (mut stream, _) = self.listener.accept().await?;
        // Requests are served one at a time; a client that never sends
        // must not block the hotkey.
        let request = match tokio::time::timeout(
            REQUEST_TIMEOUT,
            protocol::read_message(&mut stream),
        )
        .await
        {
            Ok(message) => message.and_then(|m| protocol::decode_request(&m)),
            Err(_) => Err(protocol::Error::new(
                protocol::ErrorCode::BadRequest,
                "la petición no llegó a tiempo",
            )),
        };
        Ok((request, stream))
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    // The old server did a single read of up to 1024 bytes, so a
    // command that arrived in two writes was rejected as unknown.
    #[tokio::test]
    async fn a_request_split_across_writes_is_reassembled() {
        let dir = tempfile::tempdir().unwrap();
        let server = ControlServer::bind(&dir.path().join("control.sock")).unwrap();
        let path = server.socket_path().to_path_buf();
        let client = tokio::spawn(async move {
            let mut stream = UnixStream::connect(&path).await.unwrap();
            stream.write_all(b"{\"v\":1,\"cmd\":\"tog").await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            stream.write_all(b"gle_copy\"}\n").await.unwrap();
            stream
        });
        let (request, _stream) = server.next_request().await.unwrap();
        assert_eq!(request.unwrap(), "toggle_copy");
        drop(client.await.unwrap());
    }

    #[tokio::test]
    async fn a_silent_client_does_not_block_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let server = ControlServer::bind(&dir.path().join("control.sock")).unwrap();
        let _silent = UnixStream::connect(server.socket_path()).await.unwrap();
        let (request, _stream) = tokio::time::timeout(REQUEST_TIMEOUT * 2, server.next_request())
            .await
            .expect("the server must give up on a silent client")
            .unwrap();
        assert_eq!(request.unwrap_err().code, protocol::ErrorCode::BadRequest);
    }
}
