#!/bin/bash
set -e

echo "=== fmIRC Build Script ==="
echo ""

# Output directory
OUT_DIR="./dist"
mkdir -p "$OUT_DIR"

# Build Linux (native)
echo "[1/3] Building Linux (native)..."
cargo build --release
cp target/release/fmirc "$OUT_DIR/fmirc_linux"
echo "      -> $OUT_DIR/fmirc_linux"

# Build Windows ARM64
echo "[2/3] Building Windows ARM64..."
cargo build --release --target aarch64-pc-windows-gnullvm
cp target/aarch64-pc-windows-gnullvm/release/fmirc.exe "$OUT_DIR/fmirc_arm64.exe"
echo "      -> $OUT_DIR/fmirc_arm64.exe"

# Build Windows x86
echo "[3/3] Building Windows x86..."
cargo build --release --target i686-pc-windows-gnullvm
cp target/i686-pc-windows-gnullvm/release/fmirc.exe "$OUT_DIR/fmirc_x86.exe"
echo "      -> $OUT_DIR/fmirc_x86.exe"

echo ""
echo "=== Build Complete ==="
ls -lh "$OUT_DIR"
