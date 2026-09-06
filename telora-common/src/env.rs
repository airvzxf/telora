//! Configuration environment-source construction shared by `telora-daemon`
//! and `telora-gui`.
//!
//! The `TELORA_PATHS__*` env-var cascade uses the same helper in both
//! binaries (the `config` crate's default key separator is empty, so a
//! plain `Environment::with_prefix("TELORA")` silently drops the nested
//! keys). The build helper used to live in `telora-daemon` only; lifting
//! it into [`telora-common`] is what lets the GUI mirror the daemon's
//! cascade without re-implementing the separators.

use config::Environment;

/// Build the `TELORA_*` environment-variable source for the
/// `telora-daemon` and `telora-gui` config cascades.
///
/// `config` 0.13's default key separator is empty, so
/// `TELORA_PATHS__SOCKET_DIR` would otherwise be registered as a flat key and
/// silently dropped during deserialisation. Explicit separators turn it into
/// the nested `paths.socket_dir` key expected by
/// [`crate::paths::PathsConfig`] (and the daemon's `crate::socket::PathsConfig`
/// TOML mapper that mirrors the same field set).
pub fn telora_env_source() -> Environment {
    Environment::with_prefix("TELORA")
        .prefix_separator("_")
        .separator("__")
}
