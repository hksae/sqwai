#!/usr/bin/env bash
# Install/build script for Unix-like systems.
# Builds sqwai in release mode and copies the binary so `sqwai` is callable
# from any directory. Default install dir is ~/.cargo/bin (on PATH with
# rustup); override with $SQWAI_INSTALL_DIR.
#
# Usage (from the repository root):
#   ./install.sh

set -euo pipefail

if [ ! -f Cargo.toml ]; then
    echo "error: run this script from the sqwai repository root" >&2
    exit 1
fi

BIN="${SQWAI_INSTALL_DIR:-$HOME/.cargo/bin}"

# Fail with a sentence, not with "cargo: command not found" from the middle of
# a build. `--locked` matches CI: an install that resolves a different
# dependency set than the one that passed the suite is a silent divergence.
if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found — install a Rust toolchain (https://rustup.rs) and rerun" >&2
    exit 1
fi

echo "building sqwai (release, locked dependencies)..."
cargo build --release --locked

mkdir -p "$BIN"
cp "target/release/sqwai" "$BIN/sqwai"

echo "installed to $BIN/sqwai"
"$BIN/sqwai" --version
echo ""
echo "sqwai is now available from any directory; run it inside a project with:"
echo "  sqwai"
