# telora-common

Shared library crate for the telora workspace.

## What lives here

- **`paths`** — socket-path resolution and runtime directory helpers.
  - `PathsConfig`, `ResolvedPaths`, `resolve()` — the four-step cascade
    (`socket_dir` → `$XDG_RUNTIME_DIR/telora/` →
    `/run/user/<uid>/telora/` → `/tmp/telora-<uid>/`). Resolving does not
    touch the disk.
  - `ensure_dir_0700()` — creates missing directories at `0o700`; an
    existing one is never chmod'ed and must already be the user's and
    closed to group and others.
- **`config`** — the `telora.toml` cascade every binary reads
  (`/etc/telora.toml` → `$XDG_CONFIG_HOME/telora/config.toml` →
  `--config` → `TELORA_*`), and `resolve_paths()` for the sockets.
- **`protocol`** — the protocol of the daemon socket and the GUI control
  socket: one JSON line per request (`{"v":1,"cmd":"stop"}`) and per reply
  (`{"ok":true,…}` or `{"ok":false,"error":{"code":…,"message":…}}`), plus
  the client every binary uses (`protocol::call`).
- **`socket_bind`** — the Unix-socket bind helper shared by the daemon and
  GUI. In the daemon it adopts a validated systemd listener when
  `LISTEN_PID`/`LISTEN_FDS` are present; otherwise it uses the manual path.
  The manual path pins the Linux parent directory with `O_PATH | O_NOFOLLOW`,
  replaces a current-user socket only when nothing answers on it (a second
  instance fails instead of taking over), rejects non-socket entries, and
  applies `chmod 0o600` through the pinned directory without changing the
  process umask.
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
first confirm that it belongs to one of the modules below; avoid pulling GUI,
audio, CUDA, or other binary-specific concerns into this crate.

| Module        | Concern                                       |
|---------------|-----------------------------------------------|
| `paths`       | Filesystem paths and runtime-directory layout |
| `config`      | The shared `telora.toml` cascade              |
| `protocol`    | Socket messages and the client                |
| `socket_bind` | Unix-socket creation and permissions          |
| `cache`       | Voxora-cache resolution and sanitisation      |

Keep the crate's dependency footprint small — it is on the critical
path of every consumer's binary. Avoid `voxora-*`, GTK, audio, CUDA,
or CLI-only dependencies. `dirs` is used only by the cache resolver;
`nix` remains restricted to the `fs`, `user` and `socket` features.
