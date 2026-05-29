#!/usr/bin/env bash
set -euo pipefail

sudo apt-get update
sudo apt-get install -y \
    build-essential \
    pkg-config \
    libgtk-4-dev \
    libadwaita-1-dev \
    libglib2.0-dev \
    libglib2.0-bin \
    libsofia-sip-ua-dev \
    libsofia-sip-ua-glib-dev \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    libsecret-1-dev

# Install Rust if not present
if ! command -v cargo &>/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
fi

echo "All dependencies installed."
echo "Build:  cargo build"
echo "Run:    cargo run"
