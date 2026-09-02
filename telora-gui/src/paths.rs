//! Socket and runtime path resolution for telora-gui.
//!
//! Mirrors `telora-daemon/src/paths.rs` but is intentionally a copy
//! rather than a shared module (the telora-common refactor is in the
//! mid-term EPIC). Reads the same env vars and falls back through
//! the same cascade:
//!
//!   1. `socket_dir` from `TELORA_PATHS__SOCKET_DIR` if non-empty.
//!   2. `$XDG_RUNTIME_DIR/telora/` if XDG_RUNTIME_DIR is set and the
//!      directory is writable.
//!   3. `/run/user/<uid>/telora/` if that path is writable.
//!   4. `/tmp/telora-<uid>/` as last resort (logs a warning).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct PathsConfig {
    #[allow(dead_code)]
    pub runtime_dir: Option<String>,
    pub socket_dir: Option<String>,
    pub daemon_socket: Option<String>,
    pub control_socket: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPaths {
    #[allow(dead_code)]
    pub socket_dir: PathBuf,
    pub daemon_sock: PathBuf,
    pub control_sock: PathBuf,
}

pub fn resolve(cfg: &PathsConfig) -> Result<ResolvedPaths> {
    let socket_dir = pick_socket_dir(cfg)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&socket_dir)
        .with_context(|| format!("creating socket directory {}", socket_dir.display()))?;
    let daemon_sock = cfg
        .daemon_socket
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| socket_dir.join("daemon.sock"));
    let control_sock = cfg
        .control_socket
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| socket_dir.join("control.sock"));
    Ok(ResolvedPaths {
        socket_dir,
        daemon_sock,
        control_sock,
    })
}

fn pick_socket_dir(cfg: &PathsConfig) -> Result<PathBuf> {
    if let Some(s) = cfg.socket_dir.as_deref().filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(s));
    }
    let uid = current_uid();
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR")
        && !xdg.is_empty()
        && is_writable(Path::new(&xdg))
    {
        return Ok(PathBuf::from(xdg).join("telora"));
    }
    let run_user = format!("/run/user/{uid}");
    if is_writable(Path::new(&run_user)) {
        return Ok(PathBuf::from(run_user).join("telora"));
    }
    log::warn!(
        "Falling back to /tmp/telora-{}/ — XDG_RUNTIME_DIR unset and /run/user/<uid> not writable",
        uid
    );
    Ok(PathBuf::from(format!("/tmp/telora-{uid}")))
}

fn current_uid() -> u32 {
    let Ok(contents) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("Uid:")
            && let Some(first) = rest.split_whitespace().next()
        {
            return first.parse().unwrap_or(0);
        }
    }
    0
}

fn is_writable(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false)
}

/// Convenience helper that returns the canonical daemon socket path
/// using the same resolution cascade as [`resolve`]. Used by the
/// `SocketClient` to connect to the daemon.
pub fn daemon_socket_path() -> PathBuf {
    match resolve(&PathsConfig::default()) {
        Ok(r) => r.daemon_sock,
        Err(_) => PathBuf::from("/tmp/telora-sock"), // best-effort fallback
    }
}

/// Convenience helper that returns the canonical GUI control socket
/// path. Used by `telora-ctl` (see issue #34 wiring) and by the GUI
/// `ControlServer::bind` callsite.
pub fn control_socket_path() -> PathBuf {
    match resolve(&PathsConfig::default()) {
        Ok(r) => r.control_sock,
        Err(_) => PathBuf::from("/tmp/telora-control.sock"),
    }
}
