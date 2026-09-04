# telora-common

Shared library crate for the telora workspace.

## What lives here

- **`paths`** — socket-path resolution and runtime directory helpers.
  - `PathsConfig`, `ResolvedPaths`, `resolve()` — the four-step cascade
    (`socket_dir` → `$XDG_RUNTIME_DIR/telora/` →
    `/run/user/<uid>/telora/` → `/tmp/telora-<uid>/`) with the parent
    directory created at mode `0o700`.
  - `daemon_socket_path()`, `control_socket_path()` — convenience
    wrappers around `resolve(&PathsConfig::default())` that the
    `telora-ctl` CLI and the GUI's `--help` text call.
  - `ensure_dir_0700()` — recursive `mkdir` plus a defensive
    post-create `mode & 0o077 != 0` re-check that catches a kernel /
    filesystem combination which silently ignores `set_permissions`.
- **`socket_bind`** — atomic Unix-socket bind helper.
  - `bind_unix_socket(path, instance_name)` — single entry point
    used by both `telora-daemon::SocketServer::bind` and
    `telora-gui::ControlServer::bind`. Atomically tightens `umask`
    to `0o177` for the bind window (an RAII guard restores it even
    if the bind panics), removes a stale socket only if it is owned
    by the current UID, binds through `socket2` with an explicit
    backlog of 128, and finishes with a defensive `chmod 0o600`.
    `instance_name` parameterises the EADDRINUSE / EPERM messages
    so each binary can keep its distinct actionable hint.

## Consumers

- `telora-daemon`
- `telora-gui`
- `telora-ctl`

`telora-models` deliberately does **not** depend on `telora-common`
— it has its own (legacy) voxora-cache resolver that is being
retired independently of this crate.

## Adding new shared code

New shared types belong here. A good rule of thumb: if the same
snippet is being written twice across `telora-{daemon,gui,ctl}/`,
lift it into `telora-common` and re-export it. Each module owns
one concern:

| Module          | Concern                                       |
|-----------------|-----------------------------------------------|
| `paths`         | Filesystem paths and runtime-directory layout |
| `socket_bind`   | Unix-socket creation and permissions           |
| `cache` (TBD)   | voxora-cache resolution (planned follow-up)   |

Keep the crate's dependency footprint small — it is on the critical
path of every consumer's binary. Avoid `voxora-*`, GTK, audio,
CUDA, or any CLI-only deps. `nix` is restricted to the `fs` and
`user` features.