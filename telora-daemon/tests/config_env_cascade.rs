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
//! The fix lives in [`telora_daemon::telora_env_source`] (originally
//! at `telora_daemon::paths::telora_env_source` before the EPIC #28
//! `telora-common` extraction moved the runtime path helpers out; the
//! Voxora cache helpers were subsequently shared through
//! `telora_common::cache` by issue #103),
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
use telora_daemon::{DaemonConfig, telora_env_source};
use tempfile::TempDir;

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

/// Regression test for the EPIC #27 flat-config regression.
///
/// Before this fix, [`DaemonConfig`] wrapped the STT keys in a
/// nested `[stt]` section. `try_deserialize()` therefore failed with
/// `missing field 'stt'` against any `telora.toml` that used the
/// original flat top-level layout (every existing config file in the
/// wild, plus the repo-root default). `load_config` then fell back to
/// [`DaemonConfig::default`], which always loaded `ggml-base.bin`
/// regardless of what the user configured.
///
/// This test pins the fix: build a `Config` with the same source
/// order as `main.rs::load_config` (system file + user file + env),
/// point `HOME` at a tempdir whose `~/.config/telora/config.toml`
/// carries `model_id = "...ggml-large-v3.bin"` at the top level, and
/// assert that the configured id reaches `DaemonConfig.stt.model_id`.
#[test]
fn user_config_top_level_model_id_reaches_daemon_config() {
    let _env_guard = lock_env();
    // HOME is process-global too; restore it on drop so we don't
    // pollute any later test that looks at the user's real $HOME.
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
    let _telora_restore = EnvRestore::new(&["TELORA_PATHS__SOCKET_DIR"]);

    // Build a tempdir that mimics ~/.config/telora/.
    let tmp = TempDir::new().expect("tempdir");
    let cfg_dir = tmp.path().join(".config").join("telora");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir");
    let user_cfg = cfg_dir.join("config.toml");
    // Mirror the exact shape of the real user config at
    // ~/.config/telora/config.toml: flat top-level STT keys, no
    // nested [stt] section. This is the format every existing
    // telora.toml uses and the format EPIC #27 broke by introducing
    // a nested [stt] wrapper.
    std::fs::write(
        &user_cfg,
        "model_kind = \"whisper\"\n\
         model_id = \"ggerganov/whisper.cpp/ggml-large-v3.bin\"\n\
         language = \"es\"\n\
         max_recording_seconds = 1800\n",
    )
    .expect("write user config");

    // SAFETY: serialised by `ENV_LOCK`.
    let home_str = tmp.path().to_str().expect("utf-8 path").to_string();
    unsafe {
        std::env::set_var("HOME", &home_str);
    }

    // Same source order as `main.rs::load_config`: system file (we
    // use `/dev/null` so the test does not pick up the host's real
    // /etc/telora.toml), then the user file at $HOME/.config/telora/
    // config.toml, then the env-var helper used in production. We
    // elide the CLI --config layer because the test does not exercise
    // it.
    let cfg = Config::builder()
        .add_source(File::with_name("/dev/null").required(false))
        .add_source(
            File::with_name(&format!("{home_str}/.config/telora/config.toml")).required(false),
        )
        .add_source(telora_env_source())
        .build()
        .expect("config build");

    let daemon_cfg: DaemonConfig = cfg
        .try_deserialize()
        .expect("deserialise — flat STT keys must still load");

    assert_eq!(
        daemon_cfg.stt.model_id, "ggerganov/whisper.cpp/ggml-large-v3.bin",
        "user-configured model_id must reach DaemonConfig.stt.model_id; \
         if this assertion fails, the EPIC #27 flat-config regression is back \
         (DaemonConfig is again expecting a nested [stt] section)"
    );
    assert_eq!(
        daemon_cfg.stt.language, "es",
        "user-configured language must reach DaemonConfig.stt.language"
    );
    assert_eq!(
        daemon_cfg.stt.max_recording_seconds, 1800,
        "user-configured max_recording_seconds must reach DaemonConfig.stt.max_recording_seconds"
    );
    assert_eq!(
        daemon_cfg.stt.model_kind, "whisper",
        "user-configured model_kind must reach DaemonConfig.stt.model_kind"
    );
}
