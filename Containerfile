# syntax=docker/dockerfile:1.7
#
# Telora multi-stage Containerfile.
#
# The prior pattern stubbed `src/main.rs` and `src/lib.rs` to populate
# the dependency-cache layer, which broke every time a `[[test]]`,
# `[[bench]]`, or `[[example]]` entry was added to any Cargo.toml —
# Cargo validates the manifest even for stubs and refuses to compile
# if a declared target path doesn't resolve. That brittleness is
# replaced here with `cargo chef`, the canonical Rust dependency-
# cache tool: it serialises the workspace's resolved dep graph into
# `recipe.json`, then `cargo chef cook` compiles only that graph in a
# stable, cacheable layer. Source-only changes leave the cook layer
# untouched; only Cargo.toml / Cargo.lock changes invalidate it.
#
# Stage map:
#   chef     → Rust toolchain + cargo-chef + gtk4-layer-shell + CUDA dev libs.
#               Invalidated only on base-image, Rust, cargo-chef, or
#               gtk4-layer-shell version bumps (all rare).
#   planner  → reads the workspace, emits recipe.json. Invalidated on
#               any source change, but the step is metadata-only and
#               finishes in seconds.
#   builder  → `cargo chef cook` (the heavy dep-compile layer, cacheable
#               per recipe.json) + `cargo build` (the four binaries).
#   runtime  → minimal nvidia/cuda runtime image with the four binaries
#               and the gtk4-layer-shell shared library.

# ─── Stage 0: chef ─────────────────────────────────────────────────
FROM docker.io/nvidia/cuda:12.9.1-cudnn-devel-ubuntu24.04 AS chef

ARG RUST_VERSION=stable
ENV DEBIAN_FRONTEND=noninteractive \
    PATH="/root/.cargo/bin:${PATH}" \
    CARGO_HOME=/root/.cargo \
    CARGO_TERM_COLOR=always

# One apt layer to keep the chef image small; everything below
# (`apt-get update`, gtk4-layer-shell source build, rustup install,
# cargo-chef install, component add) chains in a single `RUN` so the
# final layer only retains the artefacts.
#
# `gcc-14` / `g++-14` are pinned because the workspace's local-only
# `.cargo/config.toml` (gitignored, see `.gitignore:22`) propagates
# `NVCC_CCBIN=/usr/bin/gcc-14` to every cargo invocation. The base
# image only ships `gcc-13` via `build-essential`; without gcc-14
# bindgen_cuda invokes nvcc with `-ccbin /usr/bin/gcc-14`, which
# nvcc then fails to find. This matches the developer contract in
# CONTRIBUTING.md ("`gcc-14` (or any `gcc ≤ 15`) is the host
# compiler nvcc 12.x speaks natively").
#
# `RUST_VERSION` defaults to `stable` so the Containerfile build
# uses the same toolchain as `.github/workflows/{ci,release}.yml`
# (`dtolnay/rust-toolchain@stable`). Pinning to a fixed release
# would let clippy-version drift silently break `cargo clippy
# -D warnings` (the gate inside this file) when pedantic lints
# toggle across toolchain bumps. Operators on slow-moving distros
# can override at build time with `--build-arg RUST_VERSION=1.88`
# if they need to verify the MSRV.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        gcc-14 \
        g++-14 \
        pkg-config \
        libssl-dev \
        libclang-dev \
        cmake \
        curl \
        ca-certificates \
        git \
        libasound2-dev \
        libgtk-4-dev \
        libwayland-dev \
        wayland-protocols \
        meson \
        ninja-build \
        gobject-introspection \
        libgirepository1.0-dev \
    # gtk4-layer-shell: Ubuntu 24.04 only ships the GTK3 variant
    # (`libgtk-layer-shell-0-dev`); the Rust crate `gtk4-layer-shell
    # 0.7` with feature `v1_3` requires >= 1.3, so we build the C
    # library from source at the matching tag.
    && git clone --depth 1 --branch v1.3.0 \
        https://github.com/wmww/gtk4-layer-shell.git /tmp/gtk4-layer-shell \
    && cd /tmp/gtk4-layer-shell \
    && meson setup build --prefix=/usr -Dvapi=false \
    && ninja -C build \
    && ninja -C build install \
    && cd / \
    && rm -rf /tmp/gtk4-layer-shell \
    # Rust toolchain: pinned via `rust-version` in the workspace
    # `Cargo.toml`; the ARG here lets CI override without editing
    # the Containerfile.
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain "${RUST_VERSION}" --profile minimal \
    && cargo install cargo-chef --locked --version ^0.1 \
    && rustup component add rustfmt clippy \
    && rm -rf /var/lib/apt/lists/* /root/.cargo/registry/cache/*

# ─── Stage 1: planner ─────────────────────────────────────────────
FROM chef AS planner

WORKDIR /app
# `COPY . .` brings in everything .dockerignore allows: sources,
# tests, fixtures, manifests, scripts, docs. The build context stays
# small because target/, bin/, pkg/, models/, .git/, README, etc. are
# excluded at the docker daemon level. The full source tree is
# required here so `cargo chef prepare` sees every path any Cargo.toml
# references (including `tests/minimax_live_jfk.rs`) and can serialise
# the dep graph faithfully.
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ─── Stage 2: builder ─────────────────────────────────────────────
FROM chef AS builder

ARG CMAKE_CUDA_ARCHITECTURES=61
# candle-kernels (pulled in transitively via cuda-qwen3asr) calls
# `nvidia-smi` to auto-detect the GPU compute capability and panics
# if it isn't found. The local `.cargo/config.toml` propagates
# `CUDA_COMPUTE_CAP` from cargo's `[env]`, but it is gitignored
# and not present in CI or in fresh clones, so we hardcode the
# arch here: sm_80 (Ampere) is the lowest arch candle's WMMA BF16
# kernels compile against and is supported by the CUDA 12.9
# toolkit the builder stage ships. The binary is not executed
# during CI; only the compile path matters. Mirrors the same fix
# in `.github/workflows/release.yml:281`.
ENV CUDA_COMPUTE_CAP=80

WORKDIR /app

# Cook layer: compiles ONLY the dependency graph needed for the four
# production binaries. The cache key is recipe.json, so this layer
# is reused across source-only changes — only Cargo.toml /
# Cargo.lock edits invalidate it. `--bin` per binary keeps the
# cooked closure tight; the later `cargo build --workspace` reuses
# this cache for the bin targets.
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json \
        --bin telora-daemon --bin telora-gui --bin telora --bin telora-models

# Real source tree. The planner stage also copied it; the second
# COPY here re-layers the actual code on top of the cooked deps.
COPY . .
# Touch the source roots so cargo's incremental cache sees the new
# mtimes and re-links the four bins from the freshly-copied sources.
RUN touch telora-common/src/lib.rs \
        telora-daemon/src/main.rs \
        telora-gui/src/main.rs \
        telora-ctl/src/main.rs \
        telora-models/src/main.rs

# Lint-clean release build (matches the prior contract — clippy
# warnings fail the container build). CI's `lint` Makefile target
# uses the same flag set, so a Containerfile build that passes here
# also passes `make lint` locally.
RUN cargo clippy --release --workspace --locked -- -D warnings \
 && cargo build --release --workspace --locked

# ─── Stage 3: runtime ─────────────────────────────────────────────
FROM docker.io/nvidia/cuda:12.9.1-cudnn-runtime-ubuntu24.04

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
        libasound2t64 \
        libasound2-plugins \
        libgtk-4-1 \
    && rm -rf /var/lib/apt/lists/* \
    && echo 'pcm.!default { type pulse }' > /etc/asound.conf \
    && echo 'ctl.!default { type pulse }' >> /etc/asound.conf

# Pull the four binaries + the gtk4-layer-shell shared library +
# its typelib in one COPY, then move them into their final paths.
# Using a staging dir (`/tmp/artifacts`) lets the COPY layer use a
# single glob; the move is one RUN so the final image only carries
# the installed files.
COPY --from=builder \
    /usr/lib/x86_64-linux-gnu/libgtk4-layer-shell.so* \
    /usr/lib/x86_64-linux-gnu/girepository-1.0/Gtk4LayerShell-1.0.typelib \
    /app/target/release/telora-daemon \
    /app/target/release/telora-gui \
    /app/target/release/telora \
    /app/target/release/telora-models \
    /tmp/artifacts/

RUN mkdir -p /usr/lib/x86_64-linux-gnu/girepository-1.0/ && \
    mv /tmp/artifacts/libgtk4-layer-shell* /usr/lib/x86_64-linux-gnu/ && \
    mv /tmp/artifacts/Gtk4LayerShell-1.0.typelib /usr/lib/x86_64-linux-gnu/girepository-1.0/ && \
    mv /tmp/artifacts/telora* /usr/bin/ && \
    rm -rf /tmp/artifacts

ENTRYPOINT ["/usr/bin/telora-daemon"]
