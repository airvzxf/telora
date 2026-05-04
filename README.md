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
# Path to the model file.
# Can be an absolute path, or relative to:
# - $HOME/.local/share/telora/models/
# - /usr/share/telora/models/
# - ./models/
model_path = "ggml-base.bin"

# Language code (e.g., "es", "en", "fr")
# This is passed to the Whisper model.
language = "es"

# Maximum recording time in seconds.
# The daemon will automatically stop and process the audio if this limit is reached.
# Default is 300 seconds (5 minutes). Set to a higher value for long dictations,
# or lower to prevent memory abuse.
max_recording_seconds = 300
```

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
ACTIVE     PID        MODEL                          LANG       MAX_SEC    STATE
---------- ---------- ------------------------------ ---------- ---------- ---------------
YES        1234       ggml-base.bin                  es         300        Idle

Full Model Path: /usr/share/telora/models/ggml-base.bin
```

## Security & Privacy

- **Memory Protection**: The daemon enforces a memory limit on audio buffers (configurable via `max_recording_seconds`) to prevent OOM crashes.
- **Socket Security**: IPC sockets are restricted to the owner (`0600`), preventing unauthorized local access.
- **Privacy**: Transcriptions are processed locally and never logged to disk or system logs. Temporary file communication has been replaced with secure direct memory transfer.

## Model Management

Use `telora-models` to download and manage Whisper models:

```bash
# List available and installed models
telora-models list

# Download a predefined model
telora-models download base

# Download ANY model from whisper.cpp HuggingFace repo (e.g. large-v3-turbo-q8_0)
telora-models download large-v3-turbo-q8_0

# Download from a custom URL
telora-models download --url https://example.com/models/custom-whisper.bin

# Specify a custom output name
telora-models download base --out my-model.bin

# Download a model for all users (requires sudo)
sudo telora-models download base --global
```

### Model Resolution (Precedence)

When you specify a model (via CLI `--model` or TOML `model_path`), the daemon resolves the path using the following priority:

1.  **Explicit Path**: If you provide a full or relative path (e.g., `./my-models/tiny.bin`), it is used directly.
2.  **User Models**: `~/.local/share/telora/models/`
3.  **System Models**: `/usr/share/telora/models/`
4.  **Local Development**: `./models/` (current working directory)

**Note:** If two models have the same name, the **User** version shadows the **System** version.

## Usage

Start the assistant (this will automatically start the background daemon):

```bash
systemctl --user enable --now telora.service
```

The `telora` systemd service launches `telora-gui` (the Wayland OSD overlay), which in turn communicates with `telora-daemon` (the audio engine). Systemd handles both for you.

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
