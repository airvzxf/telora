# --- Stage 1: Build ---
FROM docker.io/nvidia/cuda:12.9.1-cudnn-devel-ubuntu24.04 AS builder

ARG CUDA_ARCH=61
ENV DEBIAN_FRONTEND=noninteractive \
    PATH="/root/.cargo/bin:${PATH}" \
    CMAKE_CUDA_ARCHITECTURES=${CUDA_ARCH}

# 1. SETUP: System Deps + GTK4 Layer Shell + Rust
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    pkg-config \
    libssl-dev \
    libclang-dev \
    cmake \
    curl \
    git \
    libasound2-dev \
    libgtk-4-dev \
    meson \
    ninja-build \
    gobject-introspection \
    libgirepository1.0-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    # Build gtk4-layer-shell
    && git clone --depth 1 --branch v1.3.0 https://github.com/wmww/gtk4-layer-shell.git /tmp/gtk4-layer-shell \
    && cd /tmp/gtk4-layer-shell \
    && meson setup build --prefix=/usr -Dvapi=false \
    && ninja -C build \
    && ninja -C build install \
    && cd / \
    && rm -rf /tmp/gtk4-layer-shell \
    # Install Rust
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    && rustup component add rustfmt clippy

WORKDIR /app

# 2. DEPENDENCIES: Manifests
COPY Cargo.toml Cargo.lock ./
COPY telora-daemon/Cargo.toml ./telora-daemon/
COPY telora-gui/Cargo.toml ./telora-gui/
COPY telora-ctl/Cargo.toml ./telora-ctl/
COPY telora-models/Cargo.toml ./telora-models/

# 3. CACHE: Compile dependencies with dummy sources
RUN mkdir -p telora-daemon/src telora-gui/src telora-ctl/src telora-models/src && \
    echo "fn main() {}" > telora-daemon/src/main.rs && \
    echo "fn main() {}" > telora-gui/src/main.rs && \
    echo "fn main() {}" > telora-ctl/src/main.rs && \
    echo "fn main() {}" > telora-models/src/main.rs && \
    cargo build --release --workspace && \
    rm -rf telora-daemon/src telora-gui/src telora-ctl/src telora-models/src

# 4. SOURCE: Copy entire project context
COPY . .

# 5. BUILD: Final compilation
RUN touch telora-daemon/src/main.rs telora-gui/src/main.rs telora-ctl/src/main.rs telora-models/src/main.rs && \
    cargo clippy --release --workspace -- -D warnings && \
    cargo build --release --workspace

# --- Stage 2: Runtime ---
FROM docker.io/nvidia/cuda:12.9.1-cudnn-runtime-ubuntu24.04

WORKDIR /app

# 6. RUNTIME ENV: Install libs + Configure
RUN apt-get update && apt-get install -y --no-install-recommends \
    libasound2t64 \
    libasound2-plugins \
    libgtk-4-1 \
    && rm -rf /var/lib/apt/lists/* \
    && echo 'pcm.!default { type pulse }' > /etc/asound.conf \
    && echo 'ctl.!default { type pulse }' >> /etc/asound.conf

# 7. ARTIFACTS: Gather all artifacts in one go
COPY --from=builder \
    /usr/lib/x86_64-linux-gnu/libgtk4-layer-shell.so* \
    /usr/lib/x86_64-linux-gnu/girepository-1.0/Gtk4LayerShell-1.0.typelib \
    /app/target/release/telora-daemon \
    /app/target/release/telora-gui \
    /app/target/release/telora \
    /app/target/release/telora-models \
    /tmp/artifacts/

# 8. INSTALL: Move artifacts to final locations
RUN mkdir -p /usr/lib/x86_64-linux-gnu/girepository-1.0/ && \
    mv /tmp/artifacts/libgtk4-layer-shell* /usr/lib/x86_64-linux-gnu/ && \
    mv /tmp/artifacts/Gtk4LayerShell-1.0.typelib /usr/lib/x86_64-linux-gnu/girepository-1.0/ && \
    mv /tmp/artifacts/telora* /usr/bin/ && \
    rm -rf /tmp/artifacts

ENTRYPOINT ["/usr/bin/telora-daemon"]
