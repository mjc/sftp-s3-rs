#!/usr/bin/env bash
# Benchmark many varied-size small files against the memory backend.
# Usage: scripts/benchmark-small-files.sh [total_mb] [runs] [warmup]
#
# The benchmark creates/reuses a flat directory of varied-size files totalling
# total_mb, then each measured run performs one OpenSSH sftp session. This uses
# the SFTP subsystem requested by the `sftp` client, not legacy scp protocol:
#   mkdir remote-dir; cd remote-dir; put each file; get each file; rm each file; rmdir remote-dir

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

TOTAL_MB=${1:-1024}
RUNS=${2:-10}
WARMUP=${3:-1}
PORT=${PORT:-22230}
USER=${SFTP_USER:-benchmark}
DATASET_DIR=${DATASET_DIR:-"$ROOT/benchmark_data/small-files-${TOTAL_MB}mb"}
RESULTS_DIR=${RESULTS_DIR:-"$ROOT/benchmark_results/small-files-$(date +%Y%m%d-%H%M%S)"}
KEY_DIR=${KEY_DIR:-"/tmp/sftp-small-files-bench-keys"}
KEY="$KEY_DIR/id_ed25519"
BINARY="$ROOT/target/release/sftp-s3"

SFTP_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o Compression=no)

mkdir -p "$RESULTS_DIR"
exec > >(tee "$RESULTS_DIR/run.log") 2>&1

pick_port() {
    while nc -z localhost "$PORT" >/dev/null 2>&1; do
        PORT=$((PORT + 1))
    done
}

ensure_key() {
    mkdir -p "$KEY_DIR"
    chmod 700 "$KEY_DIR"
    if [[ ! -f "$KEY" ]]; then
        ssh-keygen -q -t ed25519 -N "" -f "$KEY" >/dev/null
    fi
    chmod 600 "$KEY"
    chmod 644 "$KEY.pub"
    export SFTP_IDENTITY_FILE="$KEY"
}

ensure_dataset() {
    local manifest="$DATASET_DIR/.manifest"
    local expected_total=$((TOTAL_MB * 1024 * 1024))

    if [[ -f "$manifest" ]]; then
        local manifest_total manifest_files
        manifest_total=$(awk -F= '$1 == "total_bytes" { print $2 }' "$manifest")
        manifest_files=$(awk -F= '$1 == "files" { print $2 }' "$manifest")
        if [[ "$manifest_total" == "$expected_total" && -n "$manifest_files" ]]; then
            echo "Reusing dataset: $DATASET_DIR ($manifest_files files, $TOTAL_MB MiB)"
            return
        fi
    fi

    echo "Creating dataset: $DATASET_DIR (${TOTAL_MB} MiB varied-size files)"
    rm -rf "$DATASET_DIR"
    mkdir -p "$DATASET_DIR"

    # Repeating powers-of-two-ish small sizes gives a broad mix while keeping
    # the file count high enough to exercise per-file SFTP overhead.
    local sizes=(1024 2048 4096 8192 16384 32768 65536 131072 262144 524288)
    local remaining=$expected_total
    local i=0
    local file_count=0

    while (( remaining > 0 )); do
        local size=${sizes[$((i % ${#sizes[@]}))]}
        if (( size > remaining )); then
            size=$remaining
        fi
        printf -v name "file_%05d_%06d.bin" "$file_count" "$size"
        dd if=/dev/zero of="$DATASET_DIR/$name" bs="$size" count=1 status=none
        remaining=$((remaining - size))
        i=$((i + 1))
        file_count=$((file_count + 1))
    done

    {
        echo "total_bytes=$expected_total"
        echo "total_mb=$TOTAL_MB"
        echo "files=$file_count"
        echo "size_pattern=${sizes[*]}"
    } > "$manifest"
    echo "Created $file_count files"
}

write_run_script() {
    local run_script="$RESULTS_DIR/run-one-small-files.sh"
    cat > "$run_script" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

PORT=$1
DATASET_DIR=$2
DOWNLOAD_ROOT=$3
USER=$4

REMOTE_DIR="small-files-$USER-$$-$RANDOM"
DOWNLOAD_DIR="$DOWNLOAD_ROOT/$REMOTE_DIR"
BATCH_FILE="/tmp/sftp-small-files-batch-$$.txt"
SFTP_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o Compression=no)

cleanup() {
    rm -f "$BATCH_FILE"
    rm -rf "$DOWNLOAD_DIR"
}
trap cleanup EXIT

mkdir -p "$DOWNLOAD_DIR"

{
    echo "mkdir $REMOTE_DIR"
    echo "cd $REMOTE_DIR"
    echo "lcd $DATASET_DIR"
    find "$DATASET_DIR" -maxdepth 1 -type f ! -name '.manifest' -printf '%f\n' |
        sort |
        while IFS= read -r file; do
            echo "put $file"
        done
    echo "lcd $DOWNLOAD_DIR"
    find "$DATASET_DIR" -maxdepth 1 -type f ! -name '.manifest' -printf '%f\n' |
        sort |
        while IFS= read -r file; do
            echo "get $file"
        done
    find "$DATASET_DIR" -maxdepth 1 -type f ! -name '.manifest' -printf '%f\n' |
        sort |
        while IFS= read -r file; do
            echo "rm $file"
        done
    echo "cd .."
    echo "rmdir $REMOTE_DIR"
    echo "bye"
} > "$BATCH_FILE"

timeout 900 sftp -q -o BatchMode=yes -o IdentityFile="$SFTP_IDENTITY_FILE" \
    "${SFTP_OPTS[@]}" -b "$BATCH_FILE" -P "$PORT" "$USER@localhost" >/dev/null
EOF
    chmod +x "$run_script"
    echo "$run_script"
}

wait_for_server() {
    for _ in {1..100}; do
        if sftp -q -o BatchMode=yes -o IdentityFile="$KEY" "${SFTP_OPTS[@]}" \
            -P "$PORT" "$USER@localhost" <<< "bye" >/dev/null 2>&1; then
            echo "Server ready on port $PORT"
            return 0
        fi
        if ! kill -0 "$SERVER_PID" 2>/dev/null; then
            echo "Server exited during startup; log follows:" >&2
            sed -n '1,120p' "$RESULTS_DIR/server.log" >&2
            return 1
        fi
        sleep 0.1
    done
    echo "Server did not become ready" >&2
    return 1
}

cleanup_server() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
}
trap cleanup_server EXIT INT TERM

cd "$ROOT"
pick_port
ensure_key
ensure_dataset

echo "Building release binary"
cargo build --release --bin sftp-s3 -q

echo "Starting memory server"
RUST_LOG=error "$BINARY" --backend memory --port "$PORT" \
    --authorized-keys-file "$KEY.pub" >"$RESULTS_DIR/server.log" 2>&1 &
SERVER_PID=$!
wait_for_server

RUN_SCRIPT=$(write_run_script)
DOWNLOAD_ROOT="$RESULTS_DIR/downloads"
mkdir -p "$DOWNLOAD_ROOT"

FILE_COUNT=$(awk -F= '$1 == "files" { print $2 }' "$DATASET_DIR/.manifest")
TOTAL_BYTES=$((TOTAL_MB * 1024 * 1024))
TRANSFER_MB=$((TOTAL_MB * 2))
TRANSFER_FILES=$((FILE_COUNT * 2))

echo ""
echo "=== Many Small Files Benchmark ==="
echo "Dataset:       $DATASET_DIR"
echo "Files:         $FILE_COUNT"
echo "Payload:       $TOTAL_MB MiB upload + $TOTAL_MB MiB download"
echo "Runs:          $RUNS measured, $WARMUP warmup"
echo "Results:       $RESULTS_DIR"
echo ""

hyperfine \
    --warmup "$WARMUP" \
    --runs "$RUNS" \
    --export-json "$RESULTS_DIR/results.json" \
    --command-name "small-files-${TOTAL_MB}mb" \
    "bash '$RUN_SCRIPT' '$PORT' '$DATASET_DIR' '$DOWNLOAD_ROOT' '$USER'"

echo ""
echo "Summary"
jq -r \
    --argjson mb "$TRANSFER_MB" \
    --argjson files "$TRANSFER_FILES" \
    '.results[0]
     | "  throughput: \((($mb / .mean) * 10 | round / 10)) MB/s\n"
       + "  file ops:   \((($files / .mean) * 10 | round / 10)) files/s\n"
       + "  mean:       \((.mean * 1000 | round / 1000))s\n"
       + "  stddev:     \((((.stddev // 0) * 1000) | round / 1000))s\n"
       + "  range:      \((.min * 1000 | round / 1000))s - \((.max * 1000 | round / 1000))s"' \
    "$RESULTS_DIR/results.json"

echo "JSON: $RESULTS_DIR/results.json"
