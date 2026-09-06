//! GUI-side resolver for the `[paths]` section of `telora.toml` plus the
//! `TELORA_PATHS__*` env-var cascade. Mirrors
//! `telora-daemon/src/main.rs::load_config` for the `[paths]` keys only
//! — STT settings remain daemon-only and the GUI has never read them.
//!
//! Before EPIC #134 / sub-issue #64, the GUI called
//! `telora_common::paths::daemon_socket_path` /
//! `telora_common::paths::control_socket_path` with an empty
//! `PathsConfig::default()`. That meant a customised
//! `[paths] socket_dir = "/tmp/foo"` (or
//! `TELORA_PATHS__SOCKET_DIR=/tmp/foo`) had no effect on the GUI: every
//! ad-hoc `./telora-gui` invocation ignored the operator's overrides
//! and bound to `$XDG_RUNTIME_DIR/telora/control.sock` regardless.
//! The flat `TELORA_DAEMON_SOCKET` / `TELORA_CONTROL_SOCKET` vars
//! published by the systemd unit kept working because the shared
//! resolver already honoured them, but a host customised via
//! `telora.toml` instead of systemd (development shells, container
//! smoke tests, the `gui.toml` workflow in `COMPATIBILITY.md`)
//! silently regressed.
//!
//! The four-tier cascade mirrors `telora-daemon/src/main.rs:101-163`:
//!
//!   1. `/etc/telora.toml` (lowest priority)
//!   2. `~/.config/telora/config.toml` (XDG-style user config)
//!   3. (elided) `--config <path>` CLI override — the GUI has no
//!      config flag today, so the layer is omitted entirely instead of
//!      wired to a hidden knob an operator cannot set.
//!   4. `TELORA_PATHS__*` env vars (highest priority; same helper
//!      the daemon uses so the separator behaviour stays in lock-step).

use config::{Config, File};
use serde::Deserialize;
use telora_common::env::telora_env_source;

/// Document-level wrapper used purely for deserialisation. The
/// `config` crate's `try_deserialize::<PathsConfig>` looks for keys
/// at the top level; without this wrapper a `TELORA_PATHS__SOCKET_DIR`
/// value (which `telora_env_source` decodes to a nested
/// `paths.socket_dir` key) would have nowhere to land and silently
/// fail to populate the `PathsConfig` struct. The daemon-side
/// `DaemonConfig` (which already has a `paths` field) carries the
/// same shape; this private struct is the GUI-side equivalent.
#[derive(Deserialize)]
struct TeloraPathsRoot {
    #[serde(default)]
    paths: telora_common::paths::PathsConfig,
}

/// Load and merge configuration from the same four-tier cascade the
/// daemon uses (`/etc/telora.toml` → `~/.config/telora/config.toml`
/// → env vars), but elide the `--config` CLI layer because the GUI
/// does not currently expose a config flag. Returns
/// `PathsConfig::default()` on missing files / deserialise failure so
/// the resolver's XDG cascade (`$XDG_RUNTIME_DIR/telora` →
/// `/run/user/<uid>/telora` → `/tmp/telora-<uid>`) keeps being the
/// sole fallback for ad-hoc `./telora-gui` invocations.
///
/// The two `warn!` branches match the daemon's `load_config`
/// convention (`telora-daemon/src/main.rs:129-137`): never panic, never
/// silently swallow a malformed file — log a one-line diagnostic and
/// fall back to defaults so the operator can still launch the GUI.
pub fn load_paths_config() -> telora_common::paths::PathsConfig {
    let mut builder =
        Config::builder().add_source(File::with_name("/etc/telora.toml").required(false));

    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        let user_cfg = format!("{home}/.config/telora/config.toml");
        builder = builder.add_source(File::with_name(&user_cfg).required(false));
    }

    builder = builder.add_source(telora_env_source());

    match builder.build() {
        Ok(c) => match c.try_deserialize::<TeloraPathsRoot>() {
            Ok(root) => root.paths,
            Err(e) => {
                log::warn!("gui paths config: {e}; using defaults");
                telora_common::paths::PathsConfig::default()
            }
        },
        Err(e) => {
            log::warn!("gui paths config build failed: {e}; using defaults");
            telora_common::paths::PathsConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Process-global mutex serialising every test in this file.
    /// `TELORA_PATHS__*` / `HOME` are process-global, so we cannot
    /// let cargo's parallel test runner race two tests against the
    /// same environment. The daemon-side
    /// `telora-daemon/tests/config_env_cascade.rs` follows the same
    /// pattern (its own local `ENV_LOCK` rather than reaching into
    /// `telora-common`'s `pub(crate)` lock).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        // Recover from poisoning — the only state we mutate is the
        // env vars themselves, which the per-test `EnvRestore` /
        // `HomeRestore` always restores.
        match ENV_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Pin the env-var cascade path for the GUI. Mirrors
    /// `telora-daemon/tests/config_env_cascade.rs:86-102` so the
    /// daemon and GUI cascade stay in lock-step — a future bump of
    /// `config` that flips its default key separator must trip both
    /// tests at once.
    #[test]
    fn telora_paths_socket_dir_env_var_reaches_paths_config() {
        let _guard = lock_env();

        // SAFETY: only this test holds the env lock.
        unsafe {
            std::env::set_var("TELORA_PATHS__SOCKET_DIR", "/tmp/override");
        }

        let cfg = load_paths_config();
        assert_eq!(
            cfg.socket_dir.as_deref(),
            Some("/tmp/override"),
            "TELORA_PATHS__SOCKET_DIR must populate PathsConfig.socket_dir"
        );

        // SAFETY: serialised by `ENV_LOCK`.
        unsafe {
            std::env::remove_var("TELORA_PATHS__SOCKET_DIR");
        }
    }

    /// Pin the user-config-file cascade path. Mirrors the `HOME`
    /// restore idiom from
    /// `telora-daemon/tests/config_env_cascade.rs:120-202` (a local
    /// `HomeRestore` RAII guard) so a panic in the middle of the test
    /// still leaves the host's `$HOME` untouched.
    #[test]
    fn user_config_socket_dir_reaches_paths_config() {
        let _guard = lock_env();

        struct HomeRestore(Option<String>);
        impl Drop for HomeRestore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => unsafe {
                        std::env::set_var("HOME", v);
                    },
                    None => unsafe {
                        std::env::remove_var("HOME");
                    },
                }
            }
        }
        let prev_home = std::env::var("HOME").ok();
        let _home_restore = HomeRestore(prev_home);

        let tmp = TempDir::new().expect("tempdir");
        let cfg_dir = tmp.path().join(".config").join("telora");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir");
        let user_cfg = cfg_dir.join("config.toml");
        std::fs::write(&user_cfg, "[paths]\nsocket_dir = \"/tmp/from-file\"\n")
            .expect("write user config");

        let home_str = tmp.path().to_str().expect("utf-8 path").to_string();
        // SAFETY: serialised by `ENV_LOCK`.
        unsafe {
            std::env::set_var("HOME", &home_str);
        }

        let cfg = load_paths_config();
        assert_eq!(
            cfg.socket_dir.as_deref(),
            Some("/tmp/from-file"),
            "~/.config/telora/config.toml [paths] section must populate PathsConfig.socket_dir"
        );
    }

    /// Env vars win over the user config file — the daemon cascade's
    /// documented precedence. Without this invariant the operator
    /// cannot override a stock config without editing it.
    #[test]
    fn env_var_overrides_user_config() {
        let _guard = lock_env();

        struct HomeRestore(Option<String>);
        impl Drop for HomeRestore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => unsafe {
                        std::env::set_var("HOME", v);
                    },
                    None => unsafe {
                        std::env::remove_var("HOME");
                    },
                }
            }
        }
        let prev_home = std::env::var("HOME").ok();
        let _home_restore = HomeRestore(prev_home);

        struct EnvRestore;
        impl Drop for EnvRestore {
            fn drop(&mut self) {
                // SAFETY: serialised by `ENV_LOCK`.
                unsafe {
                    std::env::remove_var("TELORA_PATHS__SOCKET_DIR");
                }
            }
        }
        let _env_restore = EnvRestore;

        let tmp = TempDir::new().expect("tempdir");
        let cfg_dir = tmp.path().join(".config").join("telora");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir");
        std::fs::write(
            cfg_dir.join("config.toml"),
            "[paths]\nsocket_dir = \"/tmp/from-file\"\n",
        )
        .expect("write user config");

        let home_str = tmp.path().to_str().expect("utf-8 path").to_string();
        // SAFETY: serialised by `ENV_LOCK`.
        unsafe {
            std::env::set_var("HOME", &home_str);
            std::env::set_var("TELORA_PATHS__SOCKET_DIR", "/tmp/from-env");
        }

        let cfg = load_paths_config();
        assert_eq!(
            cfg.socket_dir.as_deref(),
            Some("/tmp/from-env"),
            "env var must take precedence over the user-config file"
        );
    }

    /// Empty host with no config files and no env vars: every field
    /// must be `None`, which means the shared resolver falls back to
    /// the XDG cascade on the call site (i.e. behaviour is identical
    /// to the pre-fix GUI for the default host).
    #[test]
    fn missing_files_yield_defaults() {
        let _guard = lock_env();

        struct HomeRestore(Option<String>);
        impl Drop for HomeRestore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => unsafe {
                        std::env::set_var("HOME", v);
                    },
                    None => unsafe {
                        std::env::remove_var("HOME");
                    },
                }
            }
        }
        let prev_home = std::env::var("HOME").ok();
        let _home_restore = HomeRestore(prev_home);

        let tmp = TempDir::new().expect("tempdir");
        let home_str = tmp.path().to_str().expect("utf-8 path").to_string();
        // SAFETY: serialised by `ENV_LOCK`.
        unsafe {
            std::env::set_var("HOME", &home_str);
            std::env::remove_var("TELORA_PATHS__SOCKET_DIR");
            std::env::remove_var("TELORA_PATHS__DAEMON_SOCKET");
            std::env::remove_var("TELORA_PATHS__CONTROL_SOCKET");
        }

        let cfg = load_paths_config();
        assert!(
            cfg.socket_dir.is_none(),
            "missing files + no env vars must leave socket_dir unset (got {:?})",
            cfg.socket_dir
        );
        assert!(
            cfg.daemon_socket.is_none(),
            "missing files + no env vars must leave daemon_socket unset"
        );
        assert!(
            cfg.control_socket.is_none(),
            "missing files + no env vars must leave control_socket unset"
        );
    }

    /// A garbage file must not panic; we log a warning and fall back
    /// to `PathsConfig::default()` (matching the daemon's
    /// `load_config` convention).
    #[test]
    fn malformed_user_config_yields_defaults() {
        let _guard = lock_env();

        struct HomeRestore(Option<String>);
        impl Drop for HomeRestore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => unsafe {
                        std::env::set_var("HOME", v);
                    },
                    None => unsafe {
                        std::env::remove_var("HOME");
                    },
                }
            }
        }
        let prev_home = std::env::var("HOME").ok();
        let _home_restore = HomeRestore(prev_home);

        let tmp = TempDir::new().expect("tempdir");
        let cfg_dir = tmp.path().join(".config").join("telora");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir");
        std::fs::write(
            cfg_dir.join("config.toml"),
            "this is not valid toml = = = [[[\n",
        )
        .expect("write garbage config");

        let home_str = tmp.path().to_str().expect("utf-8 path").to_string();
        // SAFETY: serialised by `ENV_LOCK`.
        unsafe {
            std::env::set_var("HOME", &home_str);
        }

        // Must not panic.
        let cfg = load_paths_config();
        assert!(
            cfg.socket_dir.is_none(),
            "malformed user config must fall back to defaults"
        );
        assert!(
            cfg.daemon_socket.is_none(),
            "malformed user config must fall back to defaults"
        );
        assert!(
            cfg.control_socket.is_none(),
            "malformed user config must fall back to defaults"
        );
    }
}
