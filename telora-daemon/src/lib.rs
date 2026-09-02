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
