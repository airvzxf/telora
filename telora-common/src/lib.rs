//! Shared runtime helpers for the telora workspace.
//!
//! `telora-common` consolidates code that used to live in three
//! places:
//!
//!   * [`paths`] — the socket-path resolver cascade used by the
//!     daemon, the GUI, and the CLI. Mirrors the four-step
//!     `$XDG_RUNTIME_DIR/telora/` → `/run/user/<uid>/telora/` →
//!     `/tmp/telora-<uid>/` precedence that `telora-daemon` shipped
//!     pre-extraction. The convenience wrappers
//!     [`paths::daemon_socket_path`] and [`paths::control_socket_path`]
//!     give callers a single line of glue that resolves with the
//!     full cascade and creation semantics.
//!   * [`socket_bind`] — the Unix-socket bind helper that the daemon
//!     and the GUI both used to open-code (the GUI's variant was the
//!     weaker of the two: it skipped the post-creation `mode &
//!     0o077 != 0` re-check). The shared helper applies the
//!     daemon's stricter checks to every caller.
//!
//! `telora-common` is a leaf crate — it does not depend on the
//! binary crates and pulls only the dependencies strictly needed to
//! implement the shared surface (`anyhow`, `log`, `nix` with the
//! minimum feature set, `tokio::net`, `socket2`).

pub mod paths;
pub mod socket_bind;

pub use paths::{
    PathsConfig, ResolvedPaths, control_socket_path, daemon_socket_path, default_paths_config,
    resolve,
};
pub use socket_bind::bind_unix_socket;
