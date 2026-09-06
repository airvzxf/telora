//! telora-daemon library surface.
//!
//! Historically `telora-daemon` shipped as a binary-only crate
//! (`main.rs` only). The daemon tests added in EPIC #27
//! (specifically sub-issue #38's `socket_lifecycle.rs` integration
//! test) reach into the daemon's [`paths`] module from outside the
//! crate, which `cargo`'s integration tests cannot do through a
//! binary-only crate. To unblock that test the daemon is split into a
//! library + binary:
//!
//!   * `lib.rs` (this file) re-exports the modules and items needed
//!     by external callers (integration tests and the binary). The
//!     internal modules (`audio`, `transcriber`) stay private
//!     to the crate; only the specific types the binary and tests need
//!     are re-exported.
//!   * `main.rs` becomes a thin wrapper that imports from the crate
//!     root via `use telora_daemon::*;`.
//!
//! The shared runtime paths, socket bind helper, and Voxora cache
//! resolver live in `telora-common`; this crate retains only the
//! daemon-specific configuration environment source.

pub mod socket;

mod audio;
mod transcriber;

// Re-export the deserialisable config types from `socket` so external
// callers (and `main.rs`) do not need to know about the internal
// `socket::` prefix. `PathsConfig` already exists in `paths::` with a
// different shape, so we expose the TOML-mapped one as
// `PathsConfigToml` to avoid the collision.
pub use socket::{
    Command, DaemonConfig, PathsConfig as PathsConfigToml, SocketServer, StatusResponse, SttConfig,
    default_stt_config,
};

// Re-export the runtime path resolver from `telora-common` under the
// historical `paths` module name. Tests, integration tests, and the
// binary reach into `telora_daemon::paths::*`; renaming the
// import-path everywhere would balloon this EPIC with churn that has
// nothing to do with the dedup itself.
pub use telora_common::paths;

// Re-export the env-var source helper for integration tests.
// `main.rs::load_config` also calls this helper so the test pins the
// production behaviour in one place. The function lifted out of the
// now-removed `cache_paths.rs` shim when EPIC #134 / #64 extracted
// it into `telora-common` so `telora-gui` could mirror the same
// cascade; the `pub use` chain here keeps the historical
// `telora_daemon::telora_env_source` import-path stable for existing
// callers (notably `tests/config_env_cascade.rs`).
pub use telora_common::env::telora_env_source;

// Keep the original cache helper names available to downstream daemon
// consumers while the binary and `telora-models` use the Path-based shared
// resolver directly.
pub use telora_common::cache::{default_voxora_cache_dir, sanitize_voxora_cache_override};

/// Resolve the Voxora cache using the daemon crate's historical string API.
///
/// New code should call [`telora_common::cache::resolve_voxora_cache`] so it
/// can preserve non-UTF-8 paths through a `Path` value.
///
/// # Errors
///
/// Returns an error when no usable XDG cache directory can be determined.
pub fn resolve_voxora_cache(
    args_override: Option<&str>,
    env_override: Option<&str>,
) -> anyhow::Result<std::path::PathBuf> {
    telora_common::cache::resolve_voxora_cache(
        args_override.map(std::path::Path::new),
        env_override.map(std::path::Path::new),
    )
}

// Re-export the items `main.rs` (the binary) needs from the
// otherwise-private `audio` and `transcriber` modules. Tests do not
// touch these; they are surfaced strictly for the binary wiring.
pub use audio::AudioEngine;
pub use transcriber::{BridgeTranscriber, NoopTranscriber, Transcriber};

#[cfg(test)]
mod voxora_migration_pins {
    //! Pin the voxora 0.4 migration contract: no workspace member may
    //! depend on `voxora-core` (the deprecation shim removed in voxora
    //! 0.4.0). If this test fails, either the migration was rolled
    //! back or a stray dep snuck in — both deserve a code review.

    #[test]
    fn no_workspace_member_depends_on_voxora_core() {
        for (member, body) in workspace_member_cargo_toms() {
            let offender = first_voxora_core_dep(body);
            assert!(
                offender.is_none(),
                "workspace member {member} declares `voxora-core` as a dependency \
                 (removed in voxora 0.4); offending line: {:?}. \
                 Use `voxora-traits` (the canonical home) or pull the type via \
                 `voxora-bridge`'s re-export.",
                offender.unwrap_or("<none>")
            );
        }
    }

    /// Read every workspace member's `Cargo.toml` and return
    /// (crate-name, contents) pairs. Cheap — runs once at test time.
    fn workspace_member_cargo_toms() -> Vec<(&'static str, &'static str)> {
        vec![
            (
                "telora-common",
                include_str!("../../telora-common/Cargo.toml"),
            ),
            (
                "telora-daemon",
                include_str!("../../telora-daemon/Cargo.toml"),
            ),
            ("telora-gui", include_str!("../../telora-gui/Cargo.toml")),
            ("telora", include_str!("../../telora-ctl/Cargo.toml")),
            (
                "telora-models",
                include_str!("../../telora-models/Cargo.toml"),
            ),
        ]
    }

    /// Walk `body` line-by-line and return the first line that looks
    /// like a `voxora-core = ...` dep declaration.
    ///
    /// TOML has no string-based "is this a dep?" check, so we model
    /// the minimum surface we need:
    ///   * comment lines (`# ...`) are ignored;
    ///   * `<key> = ...` lines where `<key>` is exactly `voxora-core`
    ///     count as deps.
    ///
    /// Comments that mention `voxora-core` are intentionally allowed
    /// — every workspace member has a comment explaining why the
    /// migration dropped it.
    fn first_voxora_core_dep(body: &str) -> Option<&str> {
        for line in body.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                continue;
            }
            let after_key = trimmed
                .strip_prefix("voxora-core")
                .or_else(|| trimmed.strip_prefix("\"voxora-core\""));
            if let Some(rest) = after_key
                && rest.trim_start().starts_with('=')
            {
                return Some(line);
            }
        }
        None
    }
}
