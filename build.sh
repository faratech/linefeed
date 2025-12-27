#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Use all available CPU cores for compilation
export CARGO_BUILD_JOBS=$(nproc)

# Use sccache for faster rebuilds (if available)
if command -v sccache &> /dev/null; then
    export RUSTC_WRAPPER=sccache
fi

echo "=== fmIRC Build Script ==="
echo "Using $CARGO_BUILD_JOBS parallel jobs"
[ -n "$RUSTC_WRAPPER" ] && echo "Using sccache for caching"
echo ""

# Update dependencies to latest
echo "[0/3] Checking dependencies..."
python3 tools/update-deps.py --pin
echo ""

# Output directory
OUT_DIR="./dist"
mkdir -p "$OUT_DIR"

# Build Linux (native)
echo "[1/3] Building Linux (native)..."
cargo build --profile dist
cp target/dist/fmirc "$OUT_DIR/fmirc_linux"
echo "      -> $OUT_DIR/fmirc_linux"

# Build Windows ARM64
echo "[2/3] Building Windows ARM64..."
cargo build --profile dist --target aarch64-pc-windows-gnullvm
cp target/aarch64-pc-windows-gnullvm/dist/fmirc.exe "$OUT_DIR/fmirc_arm64.exe"
echo "      -> $OUT_DIR/fmirc_arm64.exe"

# Build Windows x86
echo "[3/3] Building Windows x86..."
cargo build --profile dist --target i686-pc-windows-gnullvm
cp target/i686-pc-windows-gnullvm/dist/fmirc.exe "$OUT_DIR/fmirc_x86.exe"
echo "      -> $OUT_DIR/fmirc_x86.exe"

# Compress with UPX
echo ""
echo "[4/4] Compressing with UPX..."
if command -v upx &> /dev/null; then
    echo "      Compressing Linux..."
    upx --best -q "$OUT_DIR/fmirc_linux"
    echo "      Compressing Windows x86..."
    upx --best -q "$OUT_DIR/fmirc_x86.exe"
    echo "      Skipping ARM64 (UPX doesn't support win64/arm64 yet)"
else
    echo "      UPX not found, skipping compression"
fi

echo ""
echo "=== Build Complete ==="
ls -lh "$OUT_DIR"
