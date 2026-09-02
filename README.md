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

You can configure the daemon using a TOML file. The daemon looks for configuration in the following order:

1.  **CLI Arguments**: (e.g., `--config my_config.toml` or `--language en`)
2.  **User Config**: `~/.config/telora/config.toml`
3.  **System Config**: `/etc/telora.toml`
4.  **Environment Variables**: (e.g., `TELORA_LANGUAGE=fr`)

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
# Default is 300 seconds (5 minutes). Set to a higher value for long dictations,
# or lower to prevent memory abuse.
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
- **Socket Security**: IPC sockets live under `$XDG_RUNTIME_DIR/telora/` (fallback `/run/user/<uid>/telora/`); the parent directory is created with mode `0700` and the sockets are created at `0600` **atomically at `bind(2)` time** via `umask 0o177`, so there is no follow-up `chmod` and no TOCTOU window. The systemd user units enforce `RuntimeDirectory=telora` + `RuntimeDirectoryMode=0700`, and `telora-daemon.service` runs an `ExecStopPost` to remove the sockets on stop. Override the location with `[paths] socket_dir = "..."` in `telora.toml`. A pre-existing socket file is removed only after a `symlink_metadata` check that confirms it is owned by the current UID, so an attacker cannot redirect the bind to a foreign socket.
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
# Pre-fetch with voxora-cli (recommended for new flows):
voxora-cli download Qwen/Qwen3-ASR-0.6B
voxora-cli download ggerganov/whisper.cpp/ggml-base.bin

# Or use the legacy telora-models wrapper:
telora-models download Qwen/Qwen3-ASR-0.6B
telora-models download ggerganov/whisper.cpp/ggml-base.bin

# See what's already cached:
voxora-cli list
telora-models list
```

Both tools print the same view; `telora-models` exists only for
backwards compatibility with the pre-voxora packaging recipes and
will be retired (see `TODO.md`).

### Model Resolution (Precedence)

When the daemon resolves `model_id`, it goes through voxora-hf's
cache. A previously downloaded model is loaded from
`$XDG_CACHE_HOME/voxora/models/huggingface`; a new id is downloaded
there on first use. There is no per-user / per-system split anymore
— the cache is single-tenant per `$XDG_CACHE_HOME`.

## Usage

Start the assistant (this will automatically start the background daemon):

```bash
systemctl --user enable --now telora.service
```

The `telora` systemd service launches `telora-gui` (the Wayland OSD overlay), which in turn communicates with `telora-daemon` (the audio engine). Systemd handles both for you.

## Troubleshooting

### Socket / runtime dir

**Sockets**: by default, `telora-daemon` and `telora-gui` place their Unix
sockets under `$XDG_RUNTIME_DIR/telora/` (typically
`/run/user/<uid>/telora/`). The directory is created by systemd
(`RuntimeDirectory=telora` + `RuntimeDirectoryMode=0700` in
`telora-daemon.service` and `telora.service`) and torn down on
`systemctl --user stop`.

**Override the location**: set `[paths] socket_dir = "..."` in
`telora.toml` (or `TELORA_PATHS__SOCKET_DIR=/tmp/foo`); both
daemon and CLI respect the cascade.

**Inspect live sockets**:

```sh
ss -lx | grep telora
ls -la /run/user/$(id -u)/telora/
```

**Legacy `/tmp/telora-sock` from before the XDG migration**:
removing it manually is a one-liner:

```sh
sudo rm -f /tmp/telora-sock /tmp/telora-control.sock
```

or with the AUR package's `pre_remove` hook:

```sh
sudo pacman -Rns telora-bin
```

**EPERM on bind (historical)**: this was the original symptom of
a stale `/tmp/telora-sock` owned by another UID. After the XDG
migration it can no longer happen — the runtime dir is owned by
the current user and torn down by systemd. If you still see it,
your `[paths] socket_dir` is pointing at `/tmp` and a stale file
is blocking the bind; remove it as above.

### Known limitation: `bind(2)` does not set `O_NOFOLLOW`

The daemon and GUI sockets are created with `umask 0o177` so the
mode is `0o600` atomically inside `bind(2)`, but the bind itself
does **not** pass `O_NOFOLLOW`. A symlink swap race between the
`symlink_metadata` ownership pre-check and the `bind(2)` syscall
is therefore theoretically open for the duration of one scheduling
slice. The current defence is the `symlink_metadata` pre-check
plus umask enforcement — adequate for the EPIC's stated threat
model (stale socket owned by another UID on the same machine) but
not a full TOCTOU fix. Adding `O_NOFOLLOW` to the bind path
(open the parent directory with `O_PATH`, then
`linkat(AT_FDCWD, name, parent_fd, name, AT_EMPTY_PATH)`) is
tracked as a mid-term follow-up.

## Development

For detailed development instructions, local installation to `~/.local`, and coding standards, see [CONTRIBUTING.md](CONTRIBUTING.md).

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
