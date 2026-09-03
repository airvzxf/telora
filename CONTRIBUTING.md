# Contributing to Telora

Thank you for your interest in improving Telora!

## Project Structure

- `telora-daemon`: Rust daemon handling audio input and speech-to-text. Loads Whisper (whisper.cpp) or Qwen3-ASR (candle) through `voxora-bridge`.
- `telora-gui`: GTK4 client for Wayland OSD overlay, visual feedback and control.
- `telora-ctl`: CLI control client (binary name: `telora`) for sending commands to the GUI via Unix socket.
- `telora-models`: Thin wrapper around `voxora-hf`. Use `voxora-cli` directly for new flows.
- `pkg/`: Arch Linux packaging files.
- `scripts/`: Build and verification scripts.

## License compatibility

Telora is AGPL-3. The `voxora-bridge` crate (and the wider `voxora` workspace) is Apache-2.0. AGPL-3 §5 explicitly permits an AGPL work to depend on a non-copyleft library without propagating copyleft to that library, so `telora-daemon` linking `voxora-bridge` is fine. The copyleft boundary is at the daemon's source code, not at its transitive dependencies.

## Adding a new model

You do **not** need to touch the daemon source to support a new model. Set `model_id` (and the matching `model_kind`) in `telora.toml`:

```toml
model_kind = "qwen3-asr"
model_id   = "Qwen/Qwen3-ASR-0.6B"
```

The daemon resolves the id through `voxora-hf` (which downloads, caches and verifies) and loads the engine adapter that matches `model_kind`. To add a brand-new engine (e.g. Parakeet), implement `voxora_core::AsrEngine` in a new `voxora-*` adapter crate and re-export it from `voxora-bridge` behind a feature flag. No changes in `telora-daemon` are required for the engine itself; only a new variant on `voxora_engine::EngineFamily` (re-exported as `voxora_bridge::EngineFamily`). The enum is `#[non_exhaustive]`, so existing match arms remain source-compatible — but every consumer that exhaustively matches today must add a wildcard arm before the new variant will compile.

## Development Workflow

### 1. Prerequisites
- Rust (Edition 2024)
- Podman (for containerized builds)
- GTK4 and Layer Shell libraries (if building locally)
- CUDA Toolkit (for GPU acceleration)
- A checkout of [airvzxf/voxora](https://github.com/airvzxf/voxora) as a sibling directory (`../voxora`). The telora workspace uses path deps to pick up the bridge umbrella crate; this avoids a crates.io publish while voxora is pre-`0.1.0`.

### 2. Building
The recommended way to build is using the provided script, which ensures a consistent environment:
```bash
./scripts/build
```

### 3. Local Testing
You can run the binaries directly from the `bin/` directory after building:
```bash
# Start the daemon (loads the model referenced by telora.toml)
./bin/telora-daemon

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

You can also override the model for testing:
```bash
TELORA_MODEL_ID=Qwen/Qwen3-ASR-0.6B TELORA_MODEL_KIND=qwen3-asr ./bin/telora-daemon
```

## Questions?
Feel free to open an issue or a discussion on GitHub.
