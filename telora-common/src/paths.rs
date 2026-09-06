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

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// User-supplied path overrides from `telora.toml` `[paths]`.
///
/// The daemon's TOML mapper at
/// `telora_daemon::socket::PathsConfig` (re-exported as
/// `PathsConfigToml`) mirrors the same field set so the
/// `[paths]` section round-trips through both representations
/// without a translation step — the daemon-side copy exists mainly
/// because it sits inside the
/// `#[serde(flatten)]`-wrapped `DaemonConfig` and the GUI's
/// `load_paths_config` (issue #64) reads the same shape
/// directly. `Deserialize` lets the GUI's
/// `telora-gui/src/paths::load_paths_config` call
/// `Config::try_deserialize::<PathsConfig>` and hit every field the
/// daemon already populates; `Serialize` is provided for symmetry so
/// future reflective paths (e.g. `telora-gui status`) can round-trip
/// the resolved shape.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PathsConfig {
    #[serde(default)]
    pub socket_dir: Option<String>,
    #[serde(default)]
    pub daemon_socket: Option<String>,
    #[serde(default)]
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
    let dir = format!("/tmp/telora-{}", current_uid());
    // Pre-create the parent at mode 0o700 so the GUI's
    // `UnixStream::connect` does not race the daemon's bind-time
    // `ensure_dir_0700`. Without this, a cold-boot GUI on a host
    // with no XDG_RUNTIME_DIR and no writable `/run/user/<uid>/`
    // would hit `ENOENT` until the daemon first started and
    // `bind_unix_socket` materialised the directory.
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    PathBuf::from(dir).join("daemon.sock")
}

fn last_resort_control_sock() -> PathBuf {
    let dir = format!("/tmp/telora-{}", current_uid());
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    PathBuf::from(dir).join("control.sock")
}

/// Resolve socket paths according to explicit systemd environment values,
/// config overrides, and the runtime-directory cascade. Flat
/// `TELORA_DAEMON_SOCKET` / `TELORA_CONTROL_SOCKET` values are the canonical
/// service-unit surface and take precedence over legacy `[paths]` fields.
pub fn resolve(cfg: &PathsConfig) -> Result<ResolvedPaths> {
    let env_daemon = env_socket_path("TELORA_DAEMON_SOCKET");
    let env_control = env_socket_path("TELORA_CONTROL_SOCKET");
    let socket_dir = pick_socket_dir(cfg, env_daemon.as_deref(), env_control.as_deref())?;
    let daemon_sock = env_daemon
        .or_else(|| {
            cfg.daemon_socket
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| socket_dir.join("daemon.sock"));
    let control_sock = env_control
        .or_else(|| {
            cfg.control_socket
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| socket_dir.join("control.sock"));
    validate_unix_socket_path(&daemon_sock)
        .with_context(|| format!("validating daemon socket path {}", daemon_sock.display()))?;
    validate_unix_socket_path(&control_sock)
        .with_context(|| format!("validating control socket path {}", control_sock.display()))?;
    ensure_dir_0700(&socket_dir)
        .with_context(|| format!("creating socket directory {}", socket_dir.display()))?;
    Ok(ResolvedPaths {
        socket_dir,
        daemon_sock,
        control_sock,
    })
}

/// Validate that a Unix-socket path fits the kernel's `sun_path` limit.
///
/// On Linux, `sun_path` is 108 bytes including the trailing NUL; a path
/// longer than that makes `bind(2)` return `ENAMETOOLONG`, but only after
/// the daemon has spent potentially minutes loading the model and the
/// audio engine. Catch the failure at config-load time so the operator
/// sees a clear error before the bind is attempted.
#[cfg(target_os = "linux")]
fn validate_unix_socket_path(path: &Path) -> Result<()> {
    const SUN_PATH_MAX: usize = 108;
    let len = path.as_os_str().len();
    if len >= SUN_PATH_MAX {
        bail!(
            "socket path {} exceeds Linux sun_path limit ({} bytes, limit {})",
            path.display(),
            len,
            SUN_PATH_MAX
        );
    }
    Ok(())
}

/// On non-Linux targets, the `sun_path` limit is OS-specific (BSD: 104,
/// macOS: 104) and bind is rarely used there. Skip the check; the bind
/// will surface its own diagnostic if the path is invalid.
#[cfg(not(target_os = "linux"))]
fn validate_unix_socket_path(_path: &Path) -> Result<()> {
    Ok(())
}

fn env_socket_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn pick_socket_dir(
    cfg: &PathsConfig,
    env_daemon: Option<&Path>,
    env_control: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(path) = env_daemon.or(env_control) {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            return Ok(parent.to_path_buf());
        }
    }
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
    fn resolve_prefers_flat_systemd_socket_environment() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile_like();
        let daemon = tmp.join("daemon.sock");
        let control = tmp.join("control.sock");
        let previous_daemon = std::env::var_os("TELORA_DAEMON_SOCKET");
        let previous_control = std::env::var_os("TELORA_CONTROL_SOCKET");
        // SAFETY: this test holds `ENV_LOCK` and restores both variables below.
        unsafe {
            std::env::set_var("TELORA_DAEMON_SOCKET", &daemon);
            std::env::set_var("TELORA_CONTROL_SOCKET", &control);
        }

        let resolved = resolve(&PathsConfig::default()).expect("resolve should succeed");
        assert_eq!(resolved.daemon_sock, daemon);
        assert_eq!(resolved.control_sock, control);
        assert_eq!(resolved.socket_dir, tmp);

        unsafe {
            match previous_daemon {
                Some(value) => std::env::set_var("TELORA_DAEMON_SOCKET", value),
                None => std::env::remove_var("TELORA_DAEMON_SOCKET"),
            }
            match previous_control {
                Some(value) => std::env::set_var("TELORA_CONTROL_SOCKET", value),
                None => std::env::remove_var("TELORA_CONTROL_SOCKET"),
            }
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

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_rejects_daemon_socket_path_exceeding_sun_path_limit() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Build a socket_dir whose joined `/daemon.sock` exceeds 108
        // bytes (Linux sun_path limit). The directory does not need
        // to exist on disk — the sun_path check fires before the
        // `ensure_dir_0700` step.
        let too_long = format!("/{}/{}", "a".repeat(100), "b".repeat(50));
        let cfg = PathsConfig {
            socket_dir: Some(too_long),
            ..PathsConfig::default()
        };
        let joined = format!("{}/daemon.sock", cfg.socket_dir.as_deref().unwrap());
        assert!(
            joined.len() >= 108,
            "test pre-condition: joined path must exceed sun_path limit (got {} bytes)",
            joined.len()
        );

        let err = resolve(&cfg).expect_err("resolve should fail when socket path exceeds sun_path");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("sun_path"),
            "error must mention sun_path; got: {msg}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_accepts_daemon_socket_path_under_sun_path_limit() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Build a socket_dir whose joined `/daemon.sock` is exactly
        // 107 bytes (one under the limit). The resolver must accept
        // it; the sun_path validator must not fire.
        let tmp = tempfile_like();
        let socket_dir = format!("{}/telora", tmp.display());
        let joined = format!("{socket_dir}/daemon.sock");
        assert!(
            joined.len() < 108,
            "test pre-condition: joined path must fit under sun_path limit (got {} bytes)",
            joined.len()
        );

        let cfg = PathsConfig {
            socket_dir: Some(socket_dir.clone()),
            ..PathsConfig::default()
        };
        let resolved = resolve(&cfg).expect("resolve should succeed under sun_path limit");
        assert_eq!(resolved.socket_dir, PathBuf::from(&socket_dir));
        assert_eq!(resolved.daemon_sock, PathBuf::from(&joined));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
