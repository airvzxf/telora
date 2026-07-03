#!/usr/bin/env bash
# setup-env.sh - Smart dependency installer for validated distributions
set -euo pipefail

LOG_FILE=$(mktemp /tmp/telora-setup.XXXXXX.log)
echo "--> Logging to $LOG_FILE"
exec > >(tee -a "$LOG_FILE") 2>&1

echo "--> Starting Telora environment setup..."
echo "--> Date: $(date)"
echo "--> User: $(whoami)"

# Make sudo optional (e.g., for root-based init hooks)
SUDO=""
if command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
fi

if [ -f /etc/debian_version ]; then
    echo "--> Distribution: Debian/Ubuntu family"
    export DEBIAN_FRONTEND=noninteractive
    $SUDO apt-get update

    # Handle the libasound2 vs libasound2t64 transition
    ALSA_PKG="libasound2"
    if apt-cache show libasound2t64 >/dev/null 2>&1; then
        ALSA_PKG="libasound2t64"
    fi
    echo "--> Using ALSA package: $ALSA_PKG"

    $SUDO apt-get install -y \
        libgtk-4-1 \
        libadwaita-1-0 \
        libgtk4-layer-shell0 \
        "$ALSA_PKG" \
        libasound2-plugins \
        wl-clipboard \
        wtype \
        libgl1
    # wlrctl is optional; only needed for per-app paste shortcut overrides
    # on wlroots-based compositors (Sway, Hyprland, labwc, river, Wayfire).
    $SUDO apt-get install -y wlrctl || true

elif [ -f /etc/fedora-release ]; then
    echo "--> Distribution: Fedora family"
    $SUDO dnf install -y \
        gtk4 \
        libadwaita \
        gtk4-layer-shell \
        alsa-lib \
        pipewire-alsa \
        mesa-libGL \
        wl-clipboard \
        wtype
    # wlrctl is optional; only needed for per-app paste shortcut overrides
    # on wlroots-based compositors (Sway, Hyprland, labwc, river, Wayfire).
    $SUDO dnf install -y wlrctl || true

elif [ -f /etc/arch-release ]; then
    echo "--> Distribution: Arch Linux"
    $SUDO pacman -Syu --noconfirm \
        gtk4 \
        libadwaita \
        gtk4-layer-shell \
        alsa-lib \
        pipewire-alsa \
        wl-clipboard \
        wtype
    # wlrctl is optional; only needed for per-app paste shortcut overrides
    # on wlroots-based compositors (Sway, Hyprland, labwc, river, Wayfire).
    $SUDO pacman -S --noconfirm wlrctl || true

else
    echo "!! Error: Distribution not supported by setup-env.sh"
    echo "Please install dependencies manually: GTK4, libadwaita, ALSA, wl-clipboard, wtype."
    echo "Optional: wlrctl (for per-app paste shortcut overrides on wlroots compositors)."
    exit 1
fi

echo "--> Dependencies installed successfully."

# Add CUDA libraries to linker path if they exist
if [ -d "/opt/cuda/lib64" ]; then
    echo "--> Configuring CUDA library paths..."
    echo "/opt/cuda/lib64" | $SUDO tee /etc/ld.so.conf.d/cuda.conf > /dev/null
    $SUDO ldconfig
    echo "--> CUDA paths configured."
fi
