#!/usr/bin/env sh
set -eu

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="${PMUX_BIN_DIR:-$HOME/.local/bin}"
BIN_NAME="pmux"

cd "$REPO_DIR"

echo "[pmux] Building release binary..."
cargo build --release

echo "[pmux] Installing to $BIN_DIR/$BIN_NAME"
mkdir -p "$BIN_DIR"
install -m 0755 "target/release/$BIN_NAME" "$BIN_DIR/$BIN_NAME"

echo "[pmux] Reloading pmux server config..."
"$BIN_DIR/$BIN_NAME" restart || true

echo "[pmux] Reinstall complete"
echo "[pmux] Binary: $BIN_DIR/$BIN_NAME"
