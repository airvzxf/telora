# Contributing to Telora

Thank you for your interest in improving Telora!

## Project Structure

- `telora-daemon`: Rust daemon handling audio input and Whisper transcription (CUDA).
- `telora-gui`: GTK4 client for Wayland OSD overlay, visual feedback and control.
- `telora-ctl`: CLI control client (binary name: `telora`) for sending commands to the GUI via Unix socket.
- `telora-models`: Tool for managing Whisper models.
- `pkg/`: Arch Linux packaging files.
- `scripts/`: Build and verification scripts.

## Development Workflow

### 1. Prerequisites
- Rust (Edition 2024)
- Podman (for containerized builds)
- GTK4 and Layer Shell libraries (if building locally)
- CUDA Toolkit (for GPU acceleration)

### 2. Building
The recommended way to build is using the provided script, which ensures a consistent environment:
```bash
./scripts/build
```

### 3. Local Testing
You can run the binaries directly from the `bin/` directory after building:
```bash
# Start the daemon
./bin/telora-daemon --model ./models/ggml-base.bin

# In another terminal, run the GUI client (Wayland OSD overlay)
./bin/telora-gui

# Use the CLI client to control recording (e.g., from a hotkey)
./bin/telora toggle-type
```

## Finding Your First Task

A great place to start is by looking at our project roadmap and open tasks.

- **[TODO.md](TODO.md)**: This file lists planned features, known bugs, and ideas for improvement. It's the best place to find a task to work on.
- **[COMPATIBILITY.md](COMPATIBILITY.md)**: Before starting a new feature, please review our compatibility matrix. All changes must be verified against the supported Linux distributions to ensure Telora remains portable.

## Coding Standards

- **Rust**: Follow idiomatic Rust patterns. Use `cargo fmt` and `cargo clippy`.
- **Commits**: Use descriptive commit messages. Follow the format: `type: Description` (e.g., `fix: Audio buffer overflow`).
- **Privacy**: Never introduce code that logs transcriptions or sends data to external servers. Telora is strictly local.

## Debugging

To enable debug logs, use the `RUST_LOG` environment variable:
```bash
RUST_LOG=debug ./bin/telora-daemon
```

You can also override the model path for testing:
```bash
TELORA_MODEL_PATH=/path/to/model.bin ./bin/telora-daemon
```

## Questions?
Feel free to open an issue or a discussion on GitHub.
