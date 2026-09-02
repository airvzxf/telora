//! Socket and runtime path resolution for telora-daemon.
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
//! tries to bind.

use anyhow::{Context, Result};
use config::Environment;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// User-supplied path overrides from `telora.toml` `[paths]`. Mirrors
/// the deserialisation struct in [`crate::socket::PathsConfig`] but
/// is intentionally decoupled so the resolver can be called from
/// contexts (e.g. sub-issue #34's CLI plumbing) that do not have a
/// full config in hand.
#[derive(Debug, Clone, Default)]
pub struct PathsConfig {
    /// Override for `XDG_RUNTIME_DIR`. Surfaced via `telora.toml`
    /// `[paths] runtime_dir = "..."`; consumed by sub-issue #34's
    /// systemd-aware wiring.
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
    /// Consumed by sub-issue #34 when wiring the GUI's control
    /// socket to the daemon; the daemon itself only needs the
    /// concrete socket files.
    #[allow(dead_code)]
    pub socket_dir: PathBuf,
    pub daemon_sock: PathBuf,
    /// Consumed by sub-issue #34 when wiring the GUI's control
    /// socket and `telora-ctl` to the daemon.
    #[allow(dead_code)]
    pub control_sock: PathBuf,
}

/// Returns an empty [`PathsConfig`]. Surface for sub-issue #34 to
/// keep call-sites concise; the daemon reads its overrides through
/// [`crate::socket::DaemonConfig::paths`] today.
#[allow(dead_code)]
pub fn default_paths_config() -> PathsConfig {
    PathsConfig::default()
}

/// Build the `TELORA_*` environment-variable source for the config
/// cascade.
///
/// `config` 0.13's `Environment::with_prefix("TELORA")` defaults its
/// key separator to `""` (no splitting) and its prefix separator to
/// `"_"`. With those defaults `TELORA_PATHS__SOCKET_DIR` is
/// registered as a single flat key `paths__socket_dir` (with two
/// underscores as part of the name) and is silently dropped during
/// deserialisation — no field in [`crate::socket::DaemonConfig`]
/// matches. The fix sets both separators explicitly:
///
///   * `.prefix_separator("_")` keeps the `TELORA_` prefix matching
///     (without it, `config` auto-derives the prefix separator from
///     the key separator and the `TELORA_` prefix no longer matches
///     once `.separator("__")` is set).
///   * `.separator("__")` makes the env parser treat any remaining
///     double-underscore as a path separator, turning
///     `paths__socket_dir` into the nested key `paths.socket_dir`
///     that the struct expects.
///
/// `main.rs::load_config` uses this helper to keep the source
/// construction in one place; integration tests call it directly to
/// pin the behaviour. Returns the [`Environment`] so callers can add
/// it to their own [`config::Config`] builder (the helper does not
/// own the builder so the test can compose other sources around it).
#[allow(dead_code)]
pub fn telora_env_source() -> Environment {
    Environment::with_prefix("TELORA")
        .prefix_separator("_")
        .separator("__")
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
    let uid = nix_current_uid();
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

fn nix_current_uid() -> u32 {
    nix::unistd::getuid().as_raw() as u32
}

fn is_writable(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false)
}

/// Create `p` (and any missing parents) with mode `0o700`. Refuses
/// to continue if the resulting mode would leak to group or other.
pub fn ensure_dir_0700(p: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
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
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    // XDG_RUNTIME_DIR is process-global, so serialise tests that
    // touch it to avoid cross-talk.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
    /// socket bind is exercised by `socket::tests::bind_creates_…`.
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

    /// Create a tempdir without pulling in the `tempfile` crate
    /// (the daemon already has `tempdir` only via dev-deps; we keep
    /// this lean to avoid adding deps just for tests).
    fn tempfile_like() -> PathBuf {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = base.join(format!("telora-paths-test-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create test tempdir");
        dir
    }
}
