//! Shared runtime helpers for the telora workspace.
//!
//! `telora-common` consolidates code that used to live in several
//! workspace binaries:
//!
//!   * [`paths`] — the socket-path resolver cascade used by the
//!     daemon, the GUI, and the CLI. Mirrors the four-step
//!     `$XDG_RUNTIME_DIR/telora/` → `/run/user/<uid>/telora/` →
//!     `/tmp/telora-<uid>/` precedence that `telora-daemon` shipped
//!     pre-extraction.
//!   * [`config`] — the `telora.toml` cascade every binary loads.
//!   * [`env`] — the `config` 0.13 environment-source helper that
//!     decodes `TELORA_PATHS__SOCKET_DIR`-style keys into nested
//!     `paths.socket_dir` fields. Moved here from `telora-daemon` in
//!     EPIC #134/#64 so the GUI can mirror the daemon cascade without
//!     re-implementing the prefix/separator logic.
//!   * [`socket_bind`] — the Unix-socket bind helper that the daemon
//!     and the GUI both used to open-code. The shared helper applies
//!     the daemon's stricter directory and permission checks to every
//!     caller.
//!   * [`protocol`] — the JSON-per-line protocol of the daemon and
//!     GUI control sockets, with the client used by every binary.
//!   * [`cache`] — the Voxora model-cache resolver shared by the daemon
//!     and the legacy `telora-models` compatibility CLI. It keeps the
//!     legacy `voxora/models/huggingface` layout and validates overrides.
//!
//! `telora-common` is a leaf crate — it does not depend on the
//! binary crates and pulls only the dependencies strictly needed to
//! implement the shared surface (`anyhow`, `dirs`, `log`,
//! `nix` with the minimum feature set, `tokio::net`, `socket2`,
//! `config` for the env-var cascade, `serde_json` for the protocol).

pub mod cache;
pub mod config;
pub mod env;
pub mod paths;
pub mod protocol;
pub mod socket_bind;

pub use cache::{default_voxora_cache_dir, resolve_voxora_cache, sanitize_voxora_cache_override};
pub use env::telora_env_source;
pub use paths::{PathsConfig, ResolvedPaths, resolve};
pub use socket_bind::{adopt_systemd_listener, bind_unix_socket, bind_unix_socket_manual};
