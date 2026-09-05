//! Configuration environment-source construction for the daemon.
//!
//! This helper remains daemon-local because it depends on the daemon's
//! `config`/`DaemonConfig` representation rather than on model-cache paths.

use config::Environment;

/// Build the `TELORA_*` environment-variable source for the daemon config
/// cascade.
///
/// `config` 0.13's default key separator is empty, so
/// `TELORA_PATHS__SOCKET_DIR` would otherwise be registered as a flat key and
/// silently dropped during deserialisation. Explicit separators turn it into
/// the nested `paths.socket_dir` key expected by [`crate::PathsConfigToml`].
pub fn telora_env_source() -> Environment {
    Environment::with_prefix("TELORA")
        .prefix_separator("_")
        .separator("__")
}
