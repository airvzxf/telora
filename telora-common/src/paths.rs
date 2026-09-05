//! Socket and runtime path resolution shared by `telora-daemon`,
//! `telora-gui`, and `telora-ctl`.
//!
//! All socket paths flow through this module. The resolver picks the
//! first writable location from:
//!
//!   1. `socket_dir` from the user config (if set).
//!   2. `$XDG_RUNTIME_DIR/telora/` (when XDG_RUNTIME_DIR is set and
//!      points to an existing writable directory).
//!   3. `/run/user/<uid>/telora/` (when that path exists and is writable).
//!   4. `/tmp/telora-<uid>/` (last-resort fallback; logs a warning).
//!
//! The parent directory is created with mode 0o700 before any caller
//! tries to bind. The post-creation `mode & 0o077 != 0` defensive
//! re-check closes the silent umask-leak the GUI's pre-extraction
//! `ensure_parent_dir_0700` did not perform.

use anyhow::{Context, Result};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// User-supplied path overrides from `telora.toml` `[paths]`.
///
/// The daemon's TOML mapper lives at
/// `telora_daemon::socket::PathsConfig` (re-exported as
/// `PathsConfigToml`) — a separate struct with the same fields but
/// `#[derive(Deserialize)]` so the binary can read the `[paths]`
/// section directly. The plain shape below is intentionally decoupled
/// from the deserialiser so the resolver can be called from contexts
/// (e.g. `telora-ctl`, which does not load the daemon's full config)
/// that do not have a `telora.toml` in hand.
#[derive(Debug, Clone, Default)]
pub struct PathsConfig {
    /// Override for `XDG_RUNTIME_DIR`. Surfaced via `telora.toml`
    /// `[paths] runtime_dir = "..."`; consumed by the systemd-aware
    /// wiring that EPIC #34 landed.
    #[allow(dead_code)]
    pub runtime_dir: Option<String>,
    pub socket_dir: Option<String>,
    pub daemon_socket: Option<String>,
    pub control_socket: Option<String>,
}

/// Concrete paths returned by [`resolve`]. All paths live under the
/// resolved `socket_dir`.
#[derive(Debug, Clone)]
pub struct ResolvedPaths {
    #[allow(dead_code)]
    pub socket_dir: PathBuf,
    pub daemon_sock: PathBuf,
    #[allow(dead_code)]
    pub control_sock: PathBuf,
}

/// Returns an empty [`PathsConfig`]. Surface kept for backwards
/// compatibility with callers that read their overrides through a
/// full daemon config and just need a blank default here.
#[allow(dead_code)]
pub fn default_paths_config() -> PathsConfig {
    PathsConfig::default()
}

/// Convenience helper that returns the canonical daemon socket path
/// using the same resolution cascade as [`resolve`]. Logs the error
/// and falls back to the resolver's last-resort `/tmp/telora-<uid>/daemon.sock`
/// rather than the pre-XDG global socket name — the shared cascade is the
/// single source of truth.
pub fn daemon_socket_path() -> PathBuf {
    match resolve(&PathsConfig::default()) {
        Ok(r) => r.daemon_sock,
        Err(e) => {
            log::error!("daemon_socket_path resolver failed: {}", e);
            last_resort_daemon_sock()
        }
    }
}

/// Convenience helper that returns the canonical GUI control socket
/// path. Logs the resolver error and falls back to the resolver's
/// last-resort `/tmp/telora-<uid>/control.sock`.
pub fn control_socket_path() -> PathBuf {
    match resolve(&PathsConfig::default()) {
        Ok(r) => r.control_sock,
        Err(e) => {
            log::error!("control_socket_path resolver failed: {}", e);
            last_resort_control_sock()
        }
    }
}

fn last_resort_daemon_sock() -> PathBuf {
    PathBuf::from(format!("/tmp/telora-{}", current_uid())).join("daemon.sock")
}

fn last_resort_control_sock() -> PathBuf {
    PathBuf::from(format!("/tmp/telora-{}", current_uid())).join("control.sock")
}

/// Resolve socket directory according to the four-step cascade and
/// ensure it exists with mode `0o700`.
pub fn resolve(cfg: &PathsConfig) -> Result<ResolvedPaths> {
    let socket_dir = pick_socket_dir(cfg)?;
    ensure_dir_0700(&socket_dir)
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
        "Falling back to a per-user /tmp/telora-<uid>/ dir — XDG_RUNTIME_DIR unset and /run/user/<uid> not writable; numeric UID is not logged"
    );
    Ok(PathBuf::from(format!("/tmp/telora-{uid}")))
}

fn current_uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

fn is_writable(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false)
}

/// Create `p` (and any missing parents) with mode `0o700`. Refuses
/// to continue if the resulting mode would leak to group or other.
pub fn ensure_dir_0700(p: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder
        .create(p)
        .with_context(|| format!("DirBuilder::create({})", p.display()))?;
    let mut perms = std::fs::metadata(p)?.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(p, perms)?;
    // Defensive: confirm the mode is actually 0o700 after creation.
    let mode = std::fs::metadata(p)?.permissions().mode();
    if mode & 0o077 != 0 {
        anyhow::bail!(
            "socket directory {} has insecure mode {:o} (expected 0o700)",
            p.display(),
            mode & 0o777
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    // XDG_RUNTIME_DIR is process-global, so serialise tests that
    // touch it to avoid cross-talk. `pub(crate)` so test modules in
    // the workspace that exercise the same env var (e.g. the
    // `telora-daemon` integration tests) can share the same lock.
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn resolve_prefers_xdg_runtime_dir_when_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile_like();
        // SAFETY: only this test holds the env lock, so racing
        // readers/writers are excluded for the duration of the call.
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &tmp);
        }

        let resolved = resolve(&PathsConfig::default()).expect("resolve should succeed");

        assert_eq!(resolved.socket_dir, tmp.join("telora"));
        assert_eq!(resolved.daemon_sock, tmp.join("telora").join("daemon.sock"));
        assert_eq!(
            resolved.control_sock,
            tmp.join("telora").join("control.sock")
        );

        let mode = std::fs::metadata(&resolved.socket_dir)
            .expect("socket_dir should exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "socket_dir mode must be 0o700");

        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_uses_explicit_socket_dir_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile_like();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &tmp);
        }

        let cfg = PathsConfig {
            socket_dir: Some(format!("{}/custom", tmp.display())),
            ..PathsConfig::default()
        };
        let resolved = resolve(&cfg).expect("resolve should succeed");

        assert_eq!(resolved.socket_dir, tmp.join("custom"));
        assert_eq!(resolved.daemon_sock, tmp.join("custom").join("daemon.sock"));

        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ensure_dir_0700_sets_strict_mode() {
        let tmp = tempfile_like();
        let target = tmp.join("nested/dir");
        ensure_dir_0700(&target).expect("ensure_dir_0700 should succeed");

        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// End-to-end check that mirrors the prompt's smoke test:
    /// set `XDG_RUNTIME_DIR` to a tempdir, run [`resolve`], and
    /// confirm the resolver picks `$XDG_RUNTIME_DIR/telora/` and
    /// that the directory exists with mode `0o700`. The actual
    /// socket bind is exercised by `socket_bind::tests`.
    #[test]
    fn resolve_then_ensure_dir_matches_smoke_test_expectations() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile_like();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &tmp);
        }

        let resolved = resolve(&PathsConfig::default()).expect("resolve should succeed");

        assert_eq!(resolved.socket_dir, tmp.join("telora"));
        assert_eq!(resolved.daemon_sock, tmp.join("telora").join("daemon.sock"));
        assert_eq!(
            resolved.control_sock,
            tmp.join("telora").join("control.sock")
        );

        let mode = std::fs::metadata(&resolved.socket_dir)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "socket dir mode must be 0o700 (smoke test expectation)"
        );

        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn daemon_socket_path_returns_resolver_cascade() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile_like();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &tmp);
        }

        let path = daemon_socket_path();
        assert_eq!(path, tmp.join("telora").join("daemon.sock"));

        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn control_socket_path_returns_resolver_cascade() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile_like();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &tmp);
        }

        let path = control_socket_path();
        assert_eq!(path, tmp.join("telora").join("control.sock"));

        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Create a tempdir without pulling in the `tempfile` crate
    /// beyond the dev-dep; the helper exists only to give each test
    /// a unique, garbage-collectable path.
    fn tempfile_like() -> PathBuf {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = base.join(format!("telora-common-paths-test-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create test tempdir");
        dir
    }
}
