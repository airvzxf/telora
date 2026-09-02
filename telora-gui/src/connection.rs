use anyhow::{Context, Result};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::paths;

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

pub struct ControlServer {
    listener: UnixListener,
}

impl ControlServer {
    /// Bind the GUI's control Unix socket at `path`. Mirrors the
    /// daemon's bind routine (see `telora-daemon/src/socket.rs`)
    /// but is duplicated here because the GUI is a separate crate
    /// without access to the daemon's `paths` module or its
    /// `socket2`/`nix` deps.
    pub fn bind(path: &Path) -> Result<Self> {
        ensure_parent_dir_0700(path)?;
        remove_stale_socket(path)?;

        let listener = UnixListener::bind(path).map_err(|e| map_bind_error(e, path))?;

        // Set permissions to 0o600 (owner read/write only). The GUI
        // control socket has tighter security requirements than the
        // daemon socket because it accepts commands from the
        // telora-ctl CLI — see PROPOSAL.md security item S1.
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)
            .context("Failed to set control socket permissions to 0o600")?;

        Ok(Self { listener })
    }

    pub async fn next_command(&self) -> Result<String> {
        let (mut stream, _) = self.listener.accept().await?;
        let mut buf = [0; 1024];
        let n = stream.read(&mut buf).await?;
        Ok(String::from_utf8_lossy(&buf[..n]).trim().to_string())
    }
}

/// Create the parent directory of `path` with mode `0o700`. Inline
/// helper because the GUI crate does not depend on the daemon's
/// `paths` module.
fn ensure_parent_dir_0700(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "control socket path {} has no parent directory",
                path.display()
            )
        })?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(parent)
        .with_context(|| format!("DirBuilder::create({})", parent.display()))?;
    let mut perms = std::fs::metadata(parent)?.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(parent, perms)
        .with_context(|| format!("set_permissions(0o700) on {}", parent.display()))?;
    Ok(())
}

/// Remove a stale socket at `path` if it is owned by the current
/// UID. A socket owned by another UID triggers an actionable error
/// so the operator can clean it up.
fn remove_stale_socket(path: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!(
                "stat'ing existing control socket {}",
                path.display()
            )));
        }
    };

    let current_uid = current_uid();
    if meta.uid() != current_uid {
        return Err(anyhow::anyhow!(
            "stale control socket at {} owned by UID {} (current UID {}); run: sudo rm {}",
            path.display(),
            meta.uid(),
            current_uid,
            path.display()
        ));
    }

    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::from(e)
            .context(format!("removing stale control socket {}", path.display()))),
    }
}

/// Read the real UID of the current process from `/proc/self/status`
/// (Linux-only fallback when `nix`/`libc` are not in scope). Returns
/// `0` only when `/proc` is unavailable, which is fine for the
/// stale-socket comparison because the daemon and GUI run as the
/// same UID in any supported deployment.
fn current_uid() -> u32 {
    let Ok(contents) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            // /proc/self/status Uid line: "Uid:\t<real>\t<effective>\t<saved>\t<fs>"
            if let Some(first) = rest.split_whitespace().next() {
                return first.parse::<u32>().unwrap_or(0);
            }
        }
    }
    0
}

/// Translate a `bind(2)` failure into a user-actionable error.
fn map_bind_error(err: std::io::Error, path: &Path) -> anyhow::Error {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::AddrInUse => anyhow::anyhow!(
            "another telora-gui instance already holds {} (owner UID {:?}); check if a previous GUI session is still running",
            path.display(),
            std::fs::metadata(path).ok().map(|m| m.uid())
        ),
        ErrorKind::PermissionDenied => anyhow::anyhow!(
            "permission denied binding control socket at {} — parent directory not writable or sticky bit blocked removal of stale socket",
            path.display()
        ),
        _ => anyhow::Error::from(err).context(format!(
            "Failed to bind control socket at {}",
            path.display()
        )),
    }
}
