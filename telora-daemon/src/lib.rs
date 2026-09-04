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
//!     by external callers (integration tests, future `telora-common`
//!     reuse). The internal modules (`audio`, `transcriber`, `vad`)
//!     stay private to the crate; only the specific types the binary
//!     and the tests need are re-exported.
//!   * `main.rs` becomes a thin wrapper that imports from the crate
//!     root via `use telora_daemon::*;`.
//!
//! The split is **structural only** — no behavior changes. All tests
//! that previously lived inside `socket::tests` and `paths::tests`
//! keep the same assertions and now run via `cargo test -p
//! telora-daemon --lib` instead of the old `--bin telora-daemon`
//! mode.

pub mod paths;
pub mod socket;

mod audio;
mod transcriber;
mod vad;

// Re-export the deserialisable config types from `socket` so external
// callers (and `main.rs`) do not need to know about the internal
// `socket::` prefix. `PathsConfig` already exists in `paths::` with a
// different shape, so we expose the TOML-mapped one as
// `PathsConfigToml` to avoid the collision.
pub use socket::{
    Command, DaemonConfig, PathsConfig as PathsConfigToml, SocketServer, StatusResponse, SttConfig,
    default_stt_config,
};

// Re-export the env-var source helper for integration tests.
// `main.rs::load_config` also calls this helper so the test pins the
// production behaviour in one place.
pub use paths::telora_env_source;

// Re-export the items `main.rs` (the binary) needs from the
// otherwise-private `audio` and `transcriber` modules. Tests do not
// touch these; they are surfaced strictly for the binary wiring.
pub use audio::AudioEngine;
pub use transcriber::{BridgeTranscriber, Transcriber};

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
