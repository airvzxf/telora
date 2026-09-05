# Contributing to Telora

Thank you for your interest in improving Telora!

## Project Structure

- `telora-common`: Shared library crate. Owns the socket-path resolver (`paths::resolve`, `paths::daemon_socket_path`, `paths::control_socket_path`), the Unix-bind helper with Linux parent-path hardening (`socket_bind::bind_unix_socket`), and the shared Voxora cache resolver (`cache::{default_voxora_cache_dir, resolve_voxora_cache, sanitize_voxora_cache_override}`). Consumed by `telora-daemon`, `telora-gui`, `telora-ctl`, and the legacy `telora-models` wrapper.
- `telora-daemon`: Rust daemon handling audio input and speech-to-text. Loads Whisper (whisper.cpp) or Qwen3-ASR (candle) through `voxora-bridge`.
- `telora-gui`: GTK4 client for Wayland OSD overlay, visual feedback and control.
- `telora-ctl`: CLI control client (binary name: `telora`) for sending commands to the GUI via Unix socket.
- `telora-models`: Thin wrapper around `voxora-hf`. Use `voxora` directly for new flows.
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

The daemon resolves the id through `voxora-hf` (which downloads, caches and verifies) and loads the engine adapter that matches `model_kind`. To add a brand-new engine (e.g. Parakeet), implement `voxora_traits::AsrEngine` in a new `voxora-*` adapter crate and re-export it from `voxora-bridge` behind a feature flag (the umbrella crate re-exports the whole `voxora_traits` surface, so consumers stay on `voxora_bridge::AsrEngine` without seeing the deprecation warning that the old `voxora-core` shim used to emit). No changes in `telora-daemon` are required for the engine itself; only a new variant on `voxora_engine::EngineFamily` (re-exported as `voxora_bridge::EngineFamily`). The enum is `#[non_exhaustive]`, so existing match arms remain source-compatible — but every consumer that exhaustively matches today must add a wildcard arm before the new variant will compile.

## Development Workflow

### 1. Prerequisites
- Rust (Edition 2024, MSRV 1.86) installed via [rustup](https://rustup.rs/) (any stable toolchain `>= 1.86` works; the project's CI pins `stable` via `dtolnay/rust-toolchain`).
- Podman (for containerized builds)
- GTK4 and Layer Shell libraries (if building locally)
- CUDA Toolkit (for GPU acceleration)
- voxora 0.4.x, pinned via the workspace `[workspace.dependencies]` block in the top-level `Cargo.toml` (`voxora-bridge`, `voxora-registry`, `voxora-hf`, `voxora-traits` — all `"0.4"`). voxora 0.4 dropped the `voxora-core` deprecation shim that 0.3 shipped; the traits it used to re-export now live in `voxora-traits`. No sibling checkout of [airvzxf/voxora](https://github.com/airvzxf/voxora) is required: the daemon resolves everything through registry crates since commit `b4a252b` (`fix: consume voxora 0.2.0`).

### 2. Cargo.lock and dependency changes

`Cargo.lock` is committed and must stay synchronized with every workspace
manifest. When adding a workspace crate or changing a dependency, run
`cargo update --workspace`, review the lockfile diff, and commit `Cargo.lock`
in the same pull request. Before review, run the locked workspace checks:

```bash
cargo build --locked --workspace --all-targets
cargo test --locked --workspace --no-fail-fast
```

### 3. Building

The recommended way to build is using the provided script, which ensures a consistent environment:
```bash
./scripts/build
```

### 4. Local Testing
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

The model cache can be overridden for testing with `--voxora-cache DIR`
or `VOXORA_CACHE_DIR=DIR`. The CLI value wins over the environment value;
invalid absolute paths, traversal components, whitespace padding, and
escaping symlink prefixes fall back to the XDG cache directory.

Running the daemon and GUI outside systemd requires no special flag — they
both bind their sockets manually under `$XDG_RUNTIME_DIR/telora/` (with
the standard `/tmp/telora-<uid>/` last-resort fallback documented in the
README troubleshooting section). The daemon does expose
`--no-activation` to *force* the manual bind even when `LISTEN_FDS` is
inherited from a parent shell, which is useful for isolating
filesystem-only behaviour during debugging.

## REFRESH memory behavior

The `telora-daemon refresh` command (or `telora-daemon reload` followed
by a model-bearing REFRESH) replaces the active model without
restarting the daemon. Because the new `BridgeTranscriber` is awaited
to completion *before* the old one is dropped
(`telora-daemon/src/main.rs:585-632`), RSS briefly peaks at roughly
the sum of the old and new model weights.

Approximate resident-set peaks during REFRESH:

| model           | single load | REFRESH peak |
|-----------------|------------:|-------------:|
| ggml-tiny       |   ~150 MB   |    ~300 MB   |
| ggml-base       |   ~290 MB   |    ~580 MB   |
| ggml-small      |   ~970 MB   |    ~1.9 GB   |
| ggml-medium     |   ~1.5 GB   |    ~3.0 GB   |
| ggml-large-v3   |   ~3.1 GB   |    ~6.2 GB   |

Recommendation: keep at least **2× the larger model's footprint** of
free RAM available when running `telora-daemon refresh`, and run
`ggml-large-v3` only on hosts with **≥16 GB RAM**. After issue #94
ships, the daemon drops the old engine before building the new one,
so the REFRESH peak collapses to ~1× the new model's size.

## Release workflow trust model

Anyone with `repo: write` on `airvzxf/telora` can dispatch
`.github/workflows/release.yml:9-19` on any tag matching the regex
`^v[0-9]+\.[0-9]+\.[0-9]+$`. The workflow relies on three structural
defenses to refuse tags that were not created by the operator's GPG
or SSH-signed `git tag -s`:

1. **`validate-tag-input`** (`.github/workflows/release.yml:47-65`) —
   the regex gate; rejects malformed tag names.
2. **`verify-tag-reachability`**
   (`.github/workflows/release.yml:72-139`) — the tag's commit must
   be an ancestor of `origin/main`; catches the "tag the branch tip
   then squash-merge" bug class and prevents a force-push from
   moving the tag onto an unrelated commit.
3. **`verify-tag-signature`**
   (`.github/workflows/release.yml:147-244`) — the tag must be
   signed by a key listed in `.github/trusted-signers`. The
   allow-list is fetched from `origin/main` (not the tagged tree),
   so revoking a key on main takes effect on the next release.

The three gates are defense in depth, not a substitute for the
release procedure in `README.md` (which is the load-bearing manual
control while the repo remains single-maintainer). Do not loosen the
regex, the ancestor check, or the signer allow-list expectation
without a security review. The full signer add/remove procedure is
documented in `README.md` under "Adding a new trusted signer" and
"Removing a signer".

## Questions?
Feel free to open an issue or a discussion on GitHub.
