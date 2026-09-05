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
    `telora-ctl` CLI and GUI call.
  - `ensure_dir_0700()` — recursive `mkdir` plus a defensive
    post-create `mode & 0o077 != 0` re-check.
- **`socket_bind`** — the Unix-socket bind helper shared by the daemon and
  GUI. It pins the Linux parent directory with `O_PATH | O_NOFOLLOW`, removes
  only current-user stale sockets, rejects non-socket entries, and applies
  `chmod 0o600` through the pinned directory without changing the process
  umask.
- **`cache`** — Voxora model-cache resolution shared by `telora-daemon` and
  the legacy `telora-models` wrapper. It preserves the
  `voxora/models/huggingface` layout and rejects traversal and absolute paths
  outside the XDG cache tree. Relative overrides remain supported for
  backwards compatibility and are resolved relative to the process working
  directory.

## Consumers

- `telora-daemon`
- `telora-gui`
- `telora-ctl`
- `telora-models`

The model-management wrapper uses the shared cache resolver while it remains
available for backwards compatibility; new flows should call `voxora`
directly.

## Adding new shared code

New shared types belong here only when they fit the leaf-crate boundary and
serve more than one workspace consumer. If a repeated snippet is a candidate,
first confirm that it belongs to one of the modules below; avoid pulling daemon
protocol, GUI, audio, CUDA, or other binary-specific concerns into this crate.

| Module        | Concern                                       |
|---------------|-----------------------------------------------|
| `paths`       | Filesystem paths and runtime-directory layout |
| `socket_bind` | Unix-socket creation and permissions          |
| `cache`       | Voxora-cache resolution and sanitisation      |

Keep the crate's dependency footprint small — it is on the critical
path of every consumer's binary. Avoid `voxora-*`, GTK, audio, CUDA,
or CLI-only dependencies. `dirs` is used only by the cache resolver;
`nix` remains restricted to the `fs` and `user` features.
