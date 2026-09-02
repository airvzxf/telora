//! End-to-end test for the `TELORA_*` env-var cascade.
//!
//! The `config` 0.13 crate's `Environment::with_prefix("TELORA")`
//! defaults its key separator to `""` (no splitting) and its prefix
//! separator to `"_"`. With those defaults, `TELORA_PATHS__SOCKET_DIR`
//! is registered as a single flat key `paths__socket_dir` (with two
//! underscores as part of the name) and is silently dropped during
//! deserialisation — no field in [`telora_daemon::DaemonConfig`]
//! matches.
//!
//! The fix lives in [`telora_daemon::paths::telora_env_source`],
//! which `telora-daemon/src/main.rs::load_config` calls into. The
//! helper sets both `.prefix_separator("_")` (to keep the `TELORA_`
//! prefix matching) and `.separator("__")` (so the rest of the name
//! is split on double-underscores into nested keys). This test
//! pins that behaviour so a future bump of `config` or a refactor
//! cannot silently regress the env-var cascade.
//!
//! Run with:
//!
//! ```text
//! cargo test -p telora-daemon --test config_env_cascade
//! ```
//!
//! Tests serialise on [`ENV_LOCK`] because `TELORA_*` env vars are
//! process-global.
#![allow(clippy::await_holding_lock)]

use config::{Config, File};
use telora_daemon::{DaemonConfig, paths::telora_env_source};

/// Process-global mutex serialising every test in this file. Each
/// test acquires it before touching `TELORA_*` env vars.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    // Recover from poisoning — the only state we mutate is the env
    // vars themselves, which the per-test [`EnvRestore`] always
    // restores.
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Restore every `TELORA_*` env var the test set on drop. We track
/// only the names this test file cares about; any other env vars
/// that might be present are left untouched.
struct EnvRestore(Vec<String>);

impl EnvRestore {
    fn new(names: &[&'static str]) -> Self {
        Self(names.iter().map(|s| (*s).to_owned()).collect())
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for name in &self.0 {
            unsafe { std::env::remove_var(name) };
        }
    }
}

/// Mirror the source cascade from `telora-daemon/src/main.rs::load_config`
/// with the optional layers (system / user / `--config`) elided. The
/// env-var source comes from the production helper, so this test
/// detects any regression introduced by changing either the helper or
/// its call site.
fn build_cfg_from_env() -> Config {
    Config::builder()
        // 1. System config — elided; `required(false)` lets us skip
        //    it cleanly without pulling in `/etc/telora.toml`.
        .add_source(File::with_name("/dev/null").required(false))
        // 4. Environment variables — same helper `main.rs` calls.
        .add_source(telora_env_source())
        .build()
        .expect("config build")
}

#[test]
fn telora_paths_socket_dir_env_var_reaches_daemon_config() {
    let _env_guard = lock_env();
    let _restore = EnvRestore::new(&["TELORA_PATHS__SOCKET_DIR"]);

    // SAFETY: serialised by `ENV_LOCK`.
    unsafe {
        std::env::set_var("TELORA_PATHS__SOCKET_DIR", "/tmp/override");
    }

    let cfg: DaemonConfig = build_cfg_from_env().try_deserialize().expect("deserialise");
    assert_eq!(
        cfg.paths.socket_dir.as_deref(),
        Some("/tmp/override"),
        "TELORA_PATHS__SOCKET_DIR must populate DaemonConfig.paths.socket_dir"
    );
}
