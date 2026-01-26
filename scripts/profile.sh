#!/usr/bin/env bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

# Trap Ctrl-C and continue with flamegraph generation
trap 'echo ""; echo "Stopping perf record, generating flamegraph..."' INT

# Check deps
if ! command -v inferno-collapse-perf &> /dev/null; then
    echo "Installing inferno..."
    cargo install inferno
fi

# Fix perf permissions
echo 0 | sudo tee /proc/sys/kernel/kptr_restrict > /dev/null
echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid > /dev/null

# Build with native CPU + frame pointers
RUSTFLAGS="-C target-cpu=native -C force-frame-pointers=yes" cargo build --profile profiling

# Record using frame pointers (not DWARF)
# Disable set -e for perf since Ctrl-C causes non-zero exit
set +e
echo -ne "\033]0;sftp-s3 READY (profile CPU)\007"  # terminal title
echo -ne "\033ksftp-s3 READY\033\\"                 # tmux window name
perf record -g --call-graph fp -F 997 \
  "$PROJECT_DIR/target/profiling/sftp-s3" --backend memory --port 2223 --root .
set -e

echo ""
echo "Generating flamegraph from perf.data..."

# Generate flamegraph
if [ ! -f perf.data ]; then
    echo "Error: perf.data not found"
    exit 1
fi

perf script 2>/dev/null | inferno-collapse-perf | inferno-flamegraph > flamegraph.svg

echo "Done: flamegraph.svg"
