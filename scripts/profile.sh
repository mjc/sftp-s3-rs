#!/usr/bin/env bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

ROUNDS=${1:-8}
SIZE_MB=${2:-256}
PORT=2223

# Check deps
if ! command -v inferno-collapse-perf &> /dev/null; then
    echo "Installing inferno..."
    cargo install inferno
fi

# Fix perf permissions
echo 0 | sudo tee /proc/sys/kernel/kptr_restrict > /dev/null
echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid > /dev/null
echo 8192 | sudo tee /proc/sys/kernel/perf_event_mlock_kb > /dev/null

# Build with native CPU + frame pointers
RUSTFLAGS="-C target-cpu=native -C force-frame-pointers=yes" cargo build --profile profiling

# Start server under perf record; output goes to perf.data in project root
echo "Starting server under perf..."
perf record -F 997 -m 4096 -g --call-graph fp -o perf.data \
  "$PROJECT_DIR/target/profiling/sftp-s3" --backend memory --port $PORT --user "benchmark:benchmark" &
PERF_PID=$!

# Wait for server to be ready
for i in $(seq 1 20); do
    nc -z localhost $PORT 2>/dev/null && break
    sleep 0.2
done
echo "Server ready. Running $ROUNDS benchmark rounds (${SIZE_MB} MB each)..."

for i in $(seq 1 "$ROUNDS"); do
    bash scripts/run-benchmark.sh "$SIZE_MB" "r$i" 2>&1 | grep Roundtrip
done

echo ""
echo "Stopping perf..."
kill -INT "$PERF_PID"
wait "$PERF_PID" 2>/dev/null || true

echo "Generating flamegraph..."
perf script -i perf.data 2>/dev/null | inferno-collapse-perf > perf-folded.txt
inferno-flamegraph < perf-folded.txt > flamegraph.svg

echo ""
echo "Top leaf functions:"
awk '{n=split($0,a,";"); weight=$NF; leaf=a[n]; gsub(/ [0-9]+$/,"",leaf); counts[leaf]+=weight} END{for(f in counts) print counts[f], f}' \
    perf-folded.txt | sort -rn | head -30

echo ""
echo "Done: flamegraph.svg  perf-folded.txt"
