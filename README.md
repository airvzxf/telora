# Telora

A professional Speech-to-Text Assistant for Linux, featuring a high-performance Rust daemon using Whisper (CUDA-accelerated), a GTK4 Wayland overlay GUI (`telora-gui`), and a CLI control client (`telora`).

## Features

- **Daemon**: Rust-based, using `whisper-rs` for local, privacy-focused transcription. Now configurable via CLI or TOML.
- **Model Manager**: Integrated CLI tool to download and manage Whisper models (Tiny, Base, Small, etc.).
- **GUI Client (`telora-gui`)**: GTK4 Layer Shell OSD overlay for Wayland, providing visual feedback during recording.
- **CLI Client (`telora`)**: Lightweight CLI to toggle recording and control the assistant from scripts or hotkeys.
- **Packaging**: Ready for Arch Linux (PKGBUILD provided).
- **Multi-Distribution Support**: Verified on Arch, Fedora, and Debian using an automated test matrix.

## Installation (Arch Linux)

This project uses a containerized build process to ensure CUDA and GTK compatibility.

### 1. Build the binaries
You must build the binaries first using Podman:
```bash
./scripts/build
```

### 2. Install the package
You can then install the package using the provided PKGBUILD:
```bash
cd pkg
makepkg -si
```
*Dependencies from official Arch repos (`gtk4`, `gtk4-layer-shell`, `cuda`, etc.) will be installed automatically.*

## Configuration

You can configure the daemon using a TOML file. Configuration files are
merged from lowest to highest priority in this order:

1. **System Config**: `/etc/telora.toml`
2. **User Config**: `~/.config/telora/config.toml`
3. **Explicit Config File**: `--config my_config.toml`
4. **Environment Variables**: e.g. `TELORA_LANGUAGE=fr`

After those sources are merged, direct value flags such as `--language en`
override the resulting configuration.

### Example Configuration (`config.toml`)

```toml
# Engine family: "whisper" (whisper.cpp via voxora-whisper) or
# "qwen3-asr" (Qwen3-ASR via voxora-qwen3asr). Pick one and stick
# with it for a given install; switching is just an edit.
model_kind = "whisper"

# Hugging Face identifier (or local path; voxora-hf resolves both).
# A few common examples:
#   ggerganov/whisper.cpp/ggml-base.bin   — Whisper base (~142 MB)
#   ggerganov/whisper.cpp/ggml-large-v3.bin
#   Qwen/Qwen3-ASR-0.6B                   — Qwen3-ASR 0.6B (~1.7 GB)
#   Qwen/Qwen3-ASR-1.7B                   — Qwen3-ASR 1.7B
model_id = "ggerganov/whisper.cpp/ggml-base.bin"

# Legacy field; kept so older configs keep working. New configs
# should set `model_id` directly. If `model_id` is empty and
# `model_path` is set, the daemon treats `model_path` as the model id.
model_path = ""

# Language code (ISO 639-1, e.g. "es", "en", "fr"). The daemon
# translates this to the engine-specific vocabulary internally:
# whisper gets the ISO code as-is; qwen3-asr gets the full English
# name ("english", "chinese", …).
language = "es"

# Maximum recording time in seconds.
# The daemon will automatically stop and process the audio if this limit is reached.
# This example sets 300 seconds (5 minutes); when the field is omitted,
# the daemon default is 600 seconds (10 minutes). Lower it to prevent memory abuse.
max_recording_seconds = 300
```

### GUI Client Configuration (`gui.toml`)

The GUI client has its own optional configuration file at `~/.config/telora/gui.toml`
(XDG-compliant). It controls the paste-shortcut behaviour for the `toggle-type`
command. If the file is missing, sensible defaults are used.

```toml
# Default paste shortcut to simulate after putting the transcribed text in
# the clipboard. Most graphical apps accept this.
paste_shortcut = "ctrl+v"

# Per-app overrides. The key is the focused window's `app_id` (visible via
# `wlrctl toplevel list` on wlroots-based compositors such as Sway, Hyprland,
# labwc, river, Wayfire). The value is a `wtype`-compatible shortcut.
#
# On compositors without `wlrctl` (GNOME, KDE), or when no app_id matches,
# the default above is used.
[paste_shortcut_by_app]
Alacritty = "shift+insert"
kitty = "ctrl+shift+v"
foot = "ctrl+shift+v"
wezterm = "ctrl+shift+v"
"org.gnome.Terminal" = "ctrl+shift+v"
```

**Supported shortcut tokens** (combined with `+`): `ctrl`, `shift`, `alt`,
`super`, plus a final key. Key names are case-insensitive on the user side
and normalized to libxkbcommon's canonical spelling (e.g. `insert` →
`Insert`); without that normalization, `wtype` would type the literal text
"insert" instead of pressing the Insert key.

Recognized keys: `v`, any single letter or digit, `insert`, `delete`/`del`,
`home`, `end`, `up`, `down`, `left`, `right`, `pageup`/`pgup`/`prior`,
`pagedown`/`pgdn`/`next`, `return`/`enter`, `tab`, `escape`/`esc`,
`backspace`/`bs`, `space`, `F1`–`F24`.

**How it works (toggle-type):**

1. Every MIME type the source application advertised is read into memory
   via `wl-clipboard-rs` (a Rust wrapper around the `wlr-data-control` /
   `ext-data-control` Wayland protocols). The snapshot keeps each MIME
   type and its raw bytes verbatim, so `text/html` + `text/plain` +
   `image/png` + ... are all preserved across the paste cycle.
   Sensitive content (`x-kde-passwordManagerHint`, used by KDE password
   managers) is *not* backed up to avoid holding secrets in process
   memory.
2. The transcribed text is written to the clipboard as
   `text/plain;charset=utf-8`.
3. The configured paste shortcut is simulated via `wtype`.
4. After a short delay (so the focused app can read the data), the
   original clipboard contents are restored by republishing every MIME
   type the snapshot holds in a single Wayland offer. Plain-text editors
   keep getting plain text, rich-text editors keep getting rich text,
   and image tools keep getting the image.

**Compositor fallback** — if the compositor does not expose
`wlr-data-control` or `ext-data-control` (rare on modern Wayland; affects
old Weston and very old GNOME), `wl-clipboard-rs` cannot operate.
Telora falls back to the `wl-copy` / `wl-paste` shell tools for that
cycle, which can only preserve a single MIME type per offer. The
on-screen overlay shows `⚠ Respaldo simple (formato único)` so the user
is aware that the multi-MIME fidelity is reduced. Updating the
compositor restores the full behaviour.

The clipboard's data is never written to disk; it lives only in the GUI
process memory for the few hundred milliseconds of a typical paste cycle.

## Customizing Systemd Services

If you need to change how the services start (e.g., adding environment variables like `RUST_LOG`), the best practice is to use a **drop-in override** rather than copying the entire file.

### Example: Enable Debug Logging

1.  Create an override for the user service:
    ```bash
    systemctl --user edit telora-daemon.service
    ```
2.  Add your changes in the editor that opens:
    ```ini
    [Service]
    Environment=RUST_LOG=debug
    ```
3.  Save and exit. Systemd will automatically reload.
4.  Restart the service:
    ```bash
    systemctl --user restart telora-daemon.service
    ```

This method preserves your changes even if the main package updates the service file.

## Client CLI & Controls

The `telora` CLI communicates with the `telora-gui` process via a Unix socket. Use it for integration with shortcuts or scripts:

```bash
# Toggle recording and TYPE the result
telora toggle-type

# Toggle recording and COPY the result to clipboard
telora toggle-copy

# Cancel current recording
telora cancel
```

Run `telora --help` for more details.

## Daemon Status & Monitoring

You can check the real-time status of the audio daemon (PID, current model, language, state, etc.) by running:

```bash
telora-daemon status
```

**Example Output:**

```text
Telora Daemon Status
ACTIVE     PID        KIND       MODEL                          LANG       MAX_SEC    STATE
---------- ---------- ---------- ------------------------------ ---------- ---------- ---------------
YES        1234       whisper    ggerganov/whisper.cpp/ggml-b… es         300        Idle

Full Model Id:   ggerganov/whisper.cpp/ggml-base.bin
Resolved Path:   /home/user/.cache/voxora/models/huggingface/ggerganov/whisper.cpp/ggml-base.bin/main/ggml-base.bin
Engine Kind:     whisper
```

## Security & Privacy

- **Memory Protection**: The daemon enforces a memory limit on audio buffers (configurable via `max_recording_seconds`) to prevent OOM crashes.
- **Socket Security**: IPC sockets live under `$XDG_RUNTIME_DIR/telora/` (fallback `/run/user/<uid>/telora/`); the parent directory is created with mode `0700` and the socket is tightened to `0600` through the pinned parent directory. The helper does not mutate the process-global umask. The remaining symlink-swap limitation is described in [Socket bind hardening](#socket-bind-hardening). All three systemd units (`telora.service`, `telora-daemon.service`, `telora-daemon.socket`) declare `RuntimeDirectory=telora` + `RuntimeDirectoryMode=0700`; the socket unit's `RemoveOnStop=yes` removes the daemon socket inode when the socket is stopped, so no `ExecStopPost` is required. Override the daemon's location with `[paths] socket_dir = "..."` in `telora.toml`. A pre-existing socket file is removed only after a `symlink_metadata` check confirms it is owned by the current UID.
- **Privacy**: Transcriptions are processed locally and never logged to disk or system logs. Temporary file communication has been replaced with secure direct memory transfer.

## Model Management

Telora is model-agnostic. The `telora.toml` file picks the engine family
(`model_kind`) and the Hugging Face identifier (`model_id`); the daemon
resolves, downloads, caches and loads the model via
[`voxora-bridge`](https://github.com/airvzxf/voxora).

```toml
# Whisper base (English-ish, ~142 MB).
model_kind = "whisper"
model_id   = "ggerganov/whisper.cpp/ggml-base.bin"

# Qwen3-ASR 0.6B (20 languages incl. Spanish/Chinese, ~1.7 GB).
# model_kind = "qwen3-asr"
# model_id   = "Qwen/Qwen3-ASR-0.6B"
```

Switch engines by editing `telora.toml` and reloading the daemon
(`telora-daemon refresh`). No code change required.

### Downloading a model

Models land in voxora's canonical cache:
`$XDG_CACHE_HOME/voxora/models/huggingface`. You can either let the
daemon download on first use (it logs progress) or pre-fetch with
either tool:

```bash
# Pre-fetch with voxora (recommended for new flows):
voxora download Qwen/Qwen3-ASR-0.6B
voxora download ggerganov/whisper.cpp/ggml-base.bin

# Or use the legacy telora-models wrapper:
telora-models download Qwen/Qwen3-ASR-0.6B
telora-models download ggerganov/whisper.cpp/ggml-base.bin

# See what's already cached:
voxora list
telora-models list
```

Both tools print the same view; `telora-models` exists only for
backwards compatibility with the pre-voxora packaging recipes and
will be retired (see `TODO.md`).

To use a different cache location, pass `--voxora-cache DIR` to the
`telora-daemon` or `telora-models` command, or set `VOXORA_CACHE_DIR`.
The CLI value takes precedence over the environment value. Absolute
paths must remain under `$XDG_CACHE_HOME`; traversal components,
whitespace-padded values, and escaping or dangling symlink prefixes are
rejected and fall back to the XDG default. Relative paths are retained
for backwards compatibility and are resolved relative to the process
working directory.

### Model Resolution (Precedence)

When the daemon resolves `model_id`, it goes through voxora-hf's
cache. A previously downloaded model is loaded from
`$XDG_CACHE_HOME/voxora/models/huggingface`; a new id is downloaded
there on first use. There is no per-user / per-system split anymore
— the cache is single-tenant per `$XDG_CACHE_HOME`.

## Usage

Start the assistant (this will automatically start the background daemon):

```bash
systemctl --user daemon-reload
systemctl --user enable --now telora-daemon.socket telora.service
```

`telora-daemon.socket` is the on-demand activation listener for the audio
daemon — systemd starts `telora-daemon` the first time anything connects to
`daemon.sock`. `telora.service` is the persistent Wayland OSD (`telora-gui`).
The GUI's `Requires=telora-daemon.socket` ensures the listener is up before
the GUI launches. Without enabling both, the GUI will sit idle waiting for
the socket that the daemon never starts.

### Development / ad-hoc runs

Outside systemd (dev shells, CI, ad-hoc debugging) start the binaries
directly. The daemon exposes `--no-activation` to skip the systemd
`LISTEN_FDS` lookup and bind `daemon.sock` manually under
`$XDG_RUNTIME_DIR/telora/`. The GUI does not need any flag — its
`bind_unix_socket` path is already filesystem-only.

```bash
RUST_LOG=info ./bin/telora-daemon
RUST_LOG=info ./bin/telora-gui
```

## Troubleshooting

### Socket / runtime dir

**Sockets**: by default, `telora-daemon` and `telora-gui` place their Unix
sockets under `$XDG_RUNTIME_DIR/telora/` (typically
`/run/user/<uid>/telora/`). The directory is created by systemd on all
three units (`RuntimeDirectory=telora` + `RuntimeDirectoryMode=0700` on
`telora.service`, `telora-daemon.service`, and `telora-daemon.socket`) so
the GUI can still bind `control.sock` even if the daemon socket is
stopped. The socket inode itself is removed by `RemoveOnStop=yes` on
`telora-daemon.socket`; no `ExecStopPost` is required.

**Override the daemon location**: set `[paths] socket_dir = "..."` in
`telora.toml` or `TELORA_PATHS__SOCKET_DIR=/tmp/foo`. These settings are
read by the daemon configuration cascade; the GUI and CLI use the shared
XDG runtime cascade and should run in the same user session.

**Inspect live sockets**:

```sh
ss -lx | grep telora
ls -la /run/user/$(id -u)/telora/
```

**Last-resort `/tmp/telora-<uid>/` fallback**: if `XDG_RUNTIME_DIR`
is unset and `/run/user/<uid>` is not writable, the resolver logs the
fallback and uses per-user `daemon.sock` and `control.sock` files there.
Inspect the directory with:

```sh
ls -la /tmp/telora-$(id -u)/
```

Remove an obsolete per-user fallback after stopping Telora with:

```sh
rm -rf /tmp/telora-$(id -u)
```

**EPERM on bind (historical)**: current systemd installations create a
private runtime directory with mode `0700` and sockets with mode `0600`. If you
still see a bind error, inspect `[paths] socket_dir` and remove only a
stale socket owned by your user.

### Socket bind hardening

The shared bind helper creates the parent directory with mode `0700`. On Linux
it opens the immediate parent with `O_PATH | O_NOFOLLOW | O_DIRECTORY`, keeps
that directory descriptor alive through `bind(2)`, and applies `chmod 0600`
relative to the pinned directory. Existing symlinks, directories, and regular
files at the socket name are rejected; stale socket files owned by the current
UID are removed to preserve restart idempotency.

Linux `bind(2)` does not accept an `O_NOFOLLOW` flag, so the helper rejects a
final-name symlink before calling it and pins the parent path independently.
A same-UID process can still race the final name and cause a bind conflict,
so this is not a complete atomic-publish solution. Systemd socket activation
remains the strongest production mitigation because systemd owns the listening
socket before the service starts.

## Development

### Workspace layout

```
telora-common/    Shared paths, Unix-socket bind, and Voxora cache policy
telora-daemon/    Audio capture + STT engine + IPC socket server
telora-gui/       GTK4 Wayland OSD overlay + GUI control socket
telora-ctl/       CLI control client (binary `telora`)
telora-models/    Thin voxora-hf wrapper (legacy, see TODO.md)
```

`telora-common` owns the socket-path resolver (`paths::resolve`,
`paths::daemon_socket_path`, `paths::control_socket_path`), the
Linux-hardened Unix-bind helper (`socket_bind::bind_unix_socket`), and the
Voxora cache policy (`cache::resolve_voxora_cache`). The daemon, GUI, CLI,
and legacy model wrapper consume the relevant parts of that shared surface;
new shared types belong there only when they preserve the leaf-crate boundary.
See [CONTRIBUTING.md](CONTRIBUTING.md) for the full structure and
license notes.

For detailed development instructions, local installation to `~/.local`, and coding standards, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Releases

Telora is released through `.github/workflows/release.yml`, which runs on every push of a `vX.Y.Z` tag. The workflow has six jobs that gate a release on three independent invariants:

1. **`validate-tag-input`** — the tag matches `^v[0-9]+\.[0-9]+\.[0-9]+$`. Fails fast on a malformed name.
2. **`verify-tag-reachability`** — the tag's commit is an ancestor of `origin/main`. Catches the "tag the branch tip then squash-merge" bug class.
3. **`verify-tag-signature`** — the tag was GPG/SSH-signed by a key listed in `.github/trusted-signers` (allow-list fetched from `origin/main`, not the tagged tree, so revoking a key on main takes effect on the next release).
4. **`build-release`** — pinned to the verified SHA, builds the 4 binaries (`telora-daemon`, `telora-gui`, `telora`, `telora-models`) with the same install step as `ci.yml` (CUDA toolkit + `gtk4-layer-shell` built from source), strips them, computes `SHA256SUMS` / `SHA512SUMS`, smoke-gates each binary with `--version` (or `--help` for the GUI), and generates a CycloneDX SBOM via `anchore/sbom-action`.
5. **`build-aur-package`** — runs in an `archlinux:latest` container, downloads the release artefacts, stages the binaries into `bin/` where the existing `PKGBUILD` expects them, updates `PKGBUILD`'s `pkgver` and the install hook's version line, runs `makepkg -s --noconfirm --nocheck`, and verifies the package contents (binaries + systemd units + `/etc/telora.toml`) with `tar -tf`.
6. **`publish`** — uploads the binaries + checksums + SBOM + AUR `.pkg.tar.zst` as a GitHub Release via `softprops/action-gh-release`, with auto-generated release notes.

The build is **cold by design** (no `Swatinem/rust-cache`): GitHub Actions caches are scoped per-ref, and tag-to-tag restore is forbidden. ~5 min build per release is acceptable for 1–2 releases/week.

### Cutting a release (the operator procedure)

This is the load-bearing manual control while the repo remains single-maintainer. The workflow's structural checks (①–④ below) are defense in depth, not a substitute for the procedure.

① **Fetch and align local `main` to remote.** Covers the local drift that the squash merge creates.

```bash
git fetch origin main
git checkout main
git reset --hard origin/main
```

② **Confirm the release bump is at HEAD and `Cargo.toml` matches the planned tag.**

```bash
git log --oneline -1   # expect: <sha> chore(release): vX.Y.Z — ...
grep '^version' Cargo.toml   # expect: version = "X.Y.Z"
```

Before tagging, validate the committed lockfile with the same release command
used by GitHub Actions. If this changes `Cargo.lock`, stop, commit the change,
and repeat the verification from the resulting merge commit:

```bash
cargo build --release --locked --workspace --bins
```

③ **Tag with `-s` (GPG-signed), pointing at the merge SHA.** Never tag a local-only commit before it reaches `main`, and never tag a branch tip that will be squashed.

```bash
git tag -s vX.Y.Z "$(git rev-parse HEAD)"
git push origin vX.Y.Z
```

④ **Verify the tag's commit equals `origin/main`'s HEAD.** This is the invariant the workflow guard checks.

```bash
[ "$(git rev-parse vX.Y.Z^{commit})" = "$(git rev-parse origin/main)" ] \
  || { echo "ORPHAN TAG — abort, re-tag after step ①"; exit 1; }
```

⑤ **Watch the `release.yml` run.** The `Verify · tag is reachable from main` and `Verify · tag is signed by a trusted signer` jobs run in parallel; `Build · release binaries` cold-builds only after both pass. The build is pinned to the immutable tag commit SHA so a tag force-push mid-run cannot redirect it. `Publish · GitHub Release` uploads the binary artefacts and creates the release page.

### Adding a new trusted signer

Append one entry to `.github/trusted-signers` (and, if the new signer uses PGP, append a `-----BEGIN PGP PUBLIC KEY BLOCK-----` to `.github/trusted-signers.asc`; if all signers are SSH-only the `.asc` file may be deleted). The format is documented in the file header. `git verify-tag` auto-detects which backend the tag used, so both formats work without conditional logic in the workflow.

### Removing a signer

Wait until the most recent tag signed by that key is at least one minor version old, so a compromise of the removed key cannot rewrite a release that's in production. The runner's `git verify-tag` step re-checks against the live allow-list on every release, so a removed signer surfaces immediately for the operator.

### Release artefacts

Each `vX.Y.Z` release attaches the following assets to the GitHub Release page:

| File | What it is | Consumer |
|---|---|---|
| `telora-daemon` | Speech-to-text daemon (43 MB) | Runs as a systemd user service |
| `telora-gui` | GTK4 Wayland OSD client (5.5 MB) | Runs as a systemd user service |
| `telora` | CLI toggle/control client (4 MB) | One-shot commands from a terminal or hotkey |
| `telora-models` | Model download/management CLI (10.7 MB) | One-shot, for first-time setup and model rotation |
| `SHA256SUMS`, `SHA512SUMS` | Checksums for the 4 binaries | Verification |
| `telora.sbom.cdx.json` | CycloneDX SBOM (auto-generated from Cargo.lock by `anchore/sbom-action`) | Audit, license compliance, vulnerability scanning |
| `telora-bin-X.Y.Z-1-x86_64.pkg.tar.zst` | Arch Linux AUR binary package, built by the `build-aur-package` job running `makepkg` in an `archlinux:latest` container | `pacman -U telora-bin-X.Y.Z-1-x86_64.pkg.tar.zst` for Arch users who want the package instead of the four raw binaries |

## Project Documents

- **[TODO.md](TODO.md)**: A list of planned features, ongoing tasks, and ideas for future development.
- **[COMPATIBILITY.md](COMPATIBILITY.md)**: Detailed information on Linux distribution compatibility and the automated testing matrix.

## Users

### Persona-Based Suggestions

| Persona | Suggestion |
| :--- | :--- |
| **Non-Technical User** | "Make it a one-click install; I don't want to use the terminal." |
| **DevOps** | "Automate the CUDA architecture detection in CI/CD." |
| **Ciberseguridad** | "Daemon runs as user; ensure socket permissions (0600) are strictly enforced." |
| **Sponsorship** | "Focus on the 'Privacy-First' aspect as a selling point against cloud APIs." |
| **Developer** | "Decouple the GUI from the business logic for easier testing." |

### User-Type Specific Features

- **Students**: "Lecture Mode" for long-form recordings (30+ mins) saved directly to Markdown.
- **Office Workers**: "Template Filler" for voice-activated form completion.
- **Power Users**: Custom "Initial Prompts" to help Whisper understand technical jargon or specific names.
- **Multilingual Users**: A quick-toggle shortcut to switch between primary and secondary languages.

## License

[GNU AFFERO | Version 3](LICENSE)
