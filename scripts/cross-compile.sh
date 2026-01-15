#!/bin/bash
# Cross-compile for multiple platforms
# Requires: rustup target add <target>
#
# For macOS cross-compilation from Linux, you may need:
#   - osxcross (for x86_64-apple-darwin, aarch64-apple-darwin)
#
# For Windows cross-compilation from Linux:
#   - mingw-w64 (apt install mingw-w64)

set -e

TARGETS=(
    "x86_64-unknown-linux-gnu"
    "x86_64-pc-windows-gnu"
    # Uncomment if you have osxcross configured:
    # "x86_64-apple-darwin"
    # "aarch64-apple-darwin"
)

OUTPUT_DIR="${1:-target/release-cross}"
mkdir -p "$OUTPUT_DIR"

echo "Cross-compiling for ${#TARGETS[@]} targets..."
echo "Output directory: $OUTPUT_DIR"
echo ""

for target in "${TARGETS[@]}"; do
    echo "=== Building for $target ==="

    # Check if target is installed
    if ! rustup target list --installed | grep -q "^$target$"; then
        echo "Installing target $target..."
        rustup target add "$target"
    fi

    # Build
    cargo build --release --target "$target"

    # Copy binary to output directory
    case "$target" in
        *-windows-*)
            BINARY="sftp-s3.exe"
            ;;
        *)
            BINARY="sftp-s3"
            ;;
    esac

    # Copy examples too
    SRC_DIR="target/$target/release"
    if [ -f "$SRC_DIR/examples/memory_server" ] || [ -f "$SRC_DIR/examples/memory_server.exe" ]; then
        mkdir -p "$OUTPUT_DIR/$target"
        cp "$SRC_DIR/examples/"* "$OUTPUT_DIR/$target/" 2>/dev/null || true
    fi

    echo "Built: $target"
    echo ""
done

echo "Cross-compilation complete!"
echo "Binaries are in: $OUTPUT_DIR"
ls -la "$OUTPUT_DIR"
