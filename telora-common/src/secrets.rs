//! Secrets resolver (closes #166).
//!
//! Reads operator secrets like the MiniMax bearer token from a
//! `mode 0600` `.env` file at the XDG-compliant path
//! (`$XDG_CONFIG_HOME/telora/.env`, default
//! `~/.config/telora/.env`) or from the system-wide
//! `/etc/telora/.env`. The `.env` file is the canonical home for
//! the token; `telora.toml` should never carry a bearer secret.
//!
//! # Cascade
//!
//! [`resolve_minimax_api_key`] consults sources in this order
//! (first non-empty wins):
//!
//! 1. `cli_env_file` — `--minimax-env-file PATH` override
//!    (operator runs the daemon with a project-local `.env`).
//! 2. `/etc/telora/.env` — system-wide fallback for shared hosts
//!    (root-installed daemon, multi-user). Not all installs have
//!    this; missing file is silent.
//! 3. `$XDG_CONFIG_HOME/telora/.env` — the canonical user
//!    location; default `~/.config/telora/.env` when
//!    `XDG_CONFIG_HOME` is unset.
//! 4. `$VOXORA_MINIMAX_API_KEY` — voxora-config's explicit-prefix
//!    alias.
//! 5. `$MINIMAX_API_KEY` — canonical alias (mirrors
//!    `voxora-config/src/minimax.rs:42-59`).
//!
//! `dotenvy::from_path` uses "set if currently unset" semantics:
//! an `Environment=MINIMAX_API_KEY=…` line in the systemd unit
//! (or any pre-exported shell variable) always wins over the
//! value in the `.env` file. The file is therefore the *default*,
//! not a hard override.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

/// Canonical user-location for the telora `.env` file.
///
/// Joins `$XDG_CONFIG_HOME` (or `~/.config` when the env var is
/// unset) with `telora/.env`. The returned path is NOT guaranteed
/// to exist — callers must handle `NotFound` silently (a fresh
/// install will not have one yet).
pub fn user_telora_dotenv_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("telora").join(".env"))
}

/// Canonical system-wide location for the telora `.env` file.
///
/// `/etc/telora/.env`. Useful for shared hosts where the daemon
/// runs as root and the operator wants a single source of truth
/// across multiple users. Missing-file is silent — most installs
/// will not have this; the user-level `.env` is the common case.
pub fn system_telora_dotenv_path() -> PathBuf {
    PathBuf::from("/etc/telora/.env")
}

/// Resolve the MiniMax bearer token honouring the cascade.
///
/// Returns the **list of paths the resolver consulted** alongside
/// the resolution result, so callers can log a clear
/// "searched here, here, and here; set MINIMAX_API_KEY in
/// `~/.config/telora/.env` to enable" message when the resolver
/// fails. `Vec<PathBuf>` is non-empty even on success — the
/// caller can pass it through to `log::info!` for an audit-trail
/// line like "loaded MiniMax token from
/// /home/wolf/.config/telora/.env".
///
/// # Arguments
///
/// * `cli_env_file` — when `Some`, the resolver loads this path
///   instead of the default `/etc/telora/.env` →
///   `$XDG_CONFIG_HOME/telora/.env` cascade. The CLI flag
///   `--minimax-env-file PATH` populates this argument; CI
///   runners and one-off dev shells use it to point at a
///   project-local `.env`.
///
/// # Errors
///
/// `Err(_)` only when every source returned empty / missing /
/// whitespace. The error message names the canonical env var
/// (`MINIMAX_API_KEY`) and the expected `.env` file path so the
/// operator can fix it without reading the source.
pub fn resolve_minimax_api_key(cli_env_file: Option<&Path>) -> (Vec<PathBuf>, Result<String>) {
    let mut searched = Vec::new();

    // `.env` files (CLI override first, then system, then user).
    if let Some(p) = cli_env_file {
        load_dotenv_into(p, &mut searched);
    } else {
        load_dotenv_into(&system_telora_dotenv_path(), &mut searched);
        if let Some(user_path) = user_telora_dotenv_path() {
            load_dotenv_into(&user_path, &mut searched);
        }
    }

    let result = (|| -> Result<String> {
        for var in ["VOXORA_MINIMAX_API_KEY", "MINIMAX_API_KEY"] {
            if let Ok(k) = std::env::var(var)
                && !k.trim().is_empty()
            {
                return Ok(k);
            }
        }
        Err(anyhow!(
            "MINIMAX_API_KEY not set. Searched: {searched:?}. \
             Add `MINIMAX_API_KEY=sk-…` to ~/.config/telora/.env (mode 0600), \
             /etc/telora/.env, or export it in the systemd unit."
        ))
    })();

    (searched, result)
}

/// Load a `.env` file via `dotenvy::from_path`. `NotFound` is
/// silent (the canonical install path is one of several sources);
/// a malformed file or permission error produces an `info!` line
/// so the operator can fix it without `RUST_LOG=debug`.
fn load_dotenv_into(p: &Path, searched: &mut Vec<PathBuf>) {
    searched.push(p.to_path_buf());
    match dotenvy::from_path(p) {
        Ok(()) => {}
        Err(e) if e.not_found() => {}
        Err(e) => log::info!("could not load {}: {e}", p.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Process-global env lock ─────────────────────────────────
    //
    // These tests mutate `MINIMAX_API_KEY`, `VOXORA_MINIMAX_API_KEY`,
    // and `XDG_CONFIG_HOME` to pin the cascade. Running them in
    // parallel would race; the lock mirrors the pattern from
    // `telora-daemon/src/transcriber.rs::tests::MINIMAX_ENV_LOCK`.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvRestore {
        keys: [Option<String>; 3],
    }

    impl EnvRestore {
        fn new() -> Self {
            Self {
                keys: [
                    std::env::var("VOXORA_MINIMAX_API_KEY").ok(),
                    std::env::var("MINIMAX_API_KEY").ok(),
                    std::env::var("XDG_CONFIG_HOME").ok(),
                ],
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            // SAFETY: tests hold `ENV_LOCK` for their entire
            // lifetime, so no other thread observes a
            // half-modified environment. `set_var` / `remove_var`
            // became `unsafe` in Rust 1.86 (the workspace
            // `rust-version`); the lock makes the UB surface
            // (data race on the env table) impossible.
            for (var, value) in [
                ("VOXORA_MINIMAX_API_KEY", &self.keys[0]),
                ("MINIMAX_API_KEY", &self.keys[1]),
                ("XDG_CONFIG_HOME", &self.keys[2]),
            ] {
                match value {
                    Some(v) => unsafe { std::env::set_var(var, v) },
                    None => unsafe { std::env::remove_var(var) },
                }
            }
        }
    }

    #[test]
    fn empty_paths_and_env_falls_through_to_error() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _restore = EnvRestore::new();
        // SAFETY: lock held.
        unsafe {
            std::env::remove_var("MINIMAX_API_KEY");
            std::env::remove_var("VOXORA_MINIMAX_API_KEY");
        }
        let (_, result) = resolve_minimax_api_key(Some(Path::new("/nonexistent/telora/.env")));
        let err = result.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("MINIMAX_API_KEY"),
            "error must name the canonical env var, got: {msg}"
        );
        assert!(
            msg.contains("Searched:"),
            "error must enumerate the searched paths, got: {msg}"
        );
    }

    #[test]
    fn env_var_wins_over_dotenv() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _restore = EnvRestore::new();
        // SAFETY: lock held.
        unsafe {
            std::env::set_var("MINIMAX_API_KEY", "from-env");
        }
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join("telora").join(".env");
        std::fs::create_dir_all(env_path.parent().unwrap()).unwrap();
        std::fs::write(&env_path, "MINIMAX_API_KEY=from-dotenv\n").unwrap();
        // SAFETY: lock held.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
        }
        let (_, result) = resolve_minimax_api_key(None);
        assert_eq!(result.unwrap(), "from-env");
    }

    #[test]
    fn dotenv_loads_when_env_var_absent() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _restore = EnvRestore::new();
        // SAFETY: lock held.
        unsafe {
            std::env::remove_var("MINIMAX_API_KEY");
            std::env::remove_var("VOXORA_MINIMAX_API_KEY");
        }
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join("telora").join(".env");
        std::fs::create_dir_all(env_path.parent().unwrap()).unwrap();
        std::fs::write(&env_path, "MINIMAX_API_KEY=from-dotenv\n").unwrap();
        // SAFETY: lock held.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
        }
        let (searched, result) = resolve_minimax_api_key(None);
        assert_eq!(result.unwrap(), "from-dotenv");
        assert!(
            searched.iter().any(|p| p == &env_path),
            "user .env must be in `searched`, got: {searched:?}"
        );
    }

    #[test]
    fn dotenv_with_comments_and_blank_lines() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _restore = EnvRestore::new();
        // SAFETY: lock held.
        unsafe {
            std::env::remove_var("MINIMAX_API_KEY");
            std::env::remove_var("VOXORA_MINIMAX_API_KEY");
        }
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join("telora").join(".env");
        std::fs::create_dir_all(env_path.parent().unwrap()).unwrap();
        std::fs::write(
            &env_path,
            "# This is a comment\n\
             \n\
             MINIMAX_API_KEY=sk-from-dotenv  # inline comment\n\
             \n\
             UNRELATED=value\n",
        )
        .unwrap();
        // SAFETY: lock held.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
        }
        let (_, result) = resolve_minimax_api_key(None);
        let key = result.unwrap();
        assert_eq!(key, "sk-from-dotenv");
    }

    #[test]
    fn missing_user_dotenv_is_silent() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _restore = EnvRestore::new();
        // SAFETY: lock held.
        unsafe {
            std::env::remove_var("MINIMAX_API_KEY");
            std::env::remove_var("VOXORA_MINIMAX_API_KEY");
        }
        let dir = tempfile::tempdir().unwrap();
        // Note: no `telora/` subdir created, so user path
        // `<dir>/telora/.env` does not exist.
        // SAFETY: lock held.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
        }
        // SAFETY: also clear the system-level /etc/telora/.env from
        // the test (it does not exist on this dev box anyway, but
        // belt + braces).
        let (_, result) = resolve_minimax_api_key(None);
        let err = result.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Searched:"), "got: {msg}");
    }

    #[test]
    fn user_path_resolves_to_xdg_config_home_telora_env() {
        // SAFETY: read-only env access; no mutation, no lock needed.
        let original = std::env::var("XDG_CONFIG_HOME").ok();
        // SAFETY: mutate XDG_CONFIG_HOME for the assertion. No
        // concurrent test reads this path (lock-free singleton
        // derived from process env). The test does not depend on
        // any other env-mutating test running in parallel because
        // `user_telora_dotenv_path()` only reads XDG_CONFIG_HOME.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/tmp/example-cfg");
        }
        let p = user_telora_dotenv_path().unwrap();
        assert_eq!(p, PathBuf::from("/tmp/example-cfg/telora/.env"));
        // SAFETY: restore.
        match original {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
    }
}
