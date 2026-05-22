#!/usr/bin/env bash
# Benchmark one large file against the memory backend.
# Usage: scripts/benchmark-single-file.sh [total_mb] [runs] [warmup]
#
# Set SFTP_REQUESTS to pass OpenSSH sftp -R and control in-flight requests.
# Set SFTP_CIPHER to pass OpenSSH sftp -c and force a cipher.
# Set SFTP_BUFFER to pass OpenSSH sftp -B and control transfer buffer size.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

TOTAL_MB=${1:-1024}
RUNS=${2:-10}
WARMUP=${3:-1}
PORT=${PORT:-22230}
USER=${SFTP_USER:-benchmark}
SFTP_REQUESTS=${SFTP_REQUESTS-16}
SFTP_CIPHER=${SFTP_CIPHER-aes128-gcm@openssh.com}
SFTP_BUFFER=${SFTP_BUFFER-131072}
DATASET=${DATASET:-"$ROOT/benchmark_data/testfile_${TOTAL_MB}mb.bin"}
RESULTS_DIR=${RESULTS_DIR:-"$ROOT/benchmark_results/single-file-$(date +%Y%m%d-%H%M%S)"}
KEY_DIR=${KEY_DIR:-"/tmp/sftp-small-files-bench-keys"}
KEY="$KEY_DIR/id_ed25519"
BINARY=${BINARY:-"$ROOT/target/release/sftp-s3"}

SFTP_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o Compression=no)
if [[ -n "$SFTP_REQUESTS" ]]; then
    SFTP_OPTS+=(-R "$SFTP_REQUESTS")
fi
if [[ -n "$SFTP_CIPHER" ]]; then
    SFTP_OPTS+=(-c "$SFTP_CIPHER")
fi
if [[ -n "$SFTP_BUFFER" ]]; then
    SFTP_OPTS+=(-B "$SFTP_BUFFER")
fi
export SFTP_REQUESTS SFTP_CIPHER SFTP_BUFFER

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
    if [[ -f "$DATASET" && "$(stat -c%s "$DATASET")" == "$((TOTAL_MB * 1024 * 1024))" ]]; then
        echo "Reusing dataset: $DATASET ($TOTAL_MB MiB)"
        return
    fi

    echo "Creating dataset: $DATASET ($TOTAL_MB MiB)"
    mkdir -p "$(dirname "$DATASET")"
    dd if=/dev/zero of="$DATASET" bs=1M count="$TOTAL_MB" status=none
}

write_run_script() {
    local run_script="$RESULTS_DIR/run-one-single-file.sh"
    cat > "$run_script" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

PORT=$1
DATASET=$2
DOWNLOAD_ROOT=$3
USER=$4

REMOTE_DIR="single-file-$USER-$$-$RANDOM"
DOWNLOAD_DIR="$DOWNLOAD_ROOT/$REMOTE_DIR"
BATCH_FILE="/tmp/sftp-single-file-batch-$$.txt"
STDERR_FILE="/tmp/sftp-single-file-stderr-$$.txt"
SFTP_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o Compression=no)
if [[ -n "${SFTP_REQUESTS:-}" ]]; then
    SFTP_OPTS+=(-R "$SFTP_REQUESTS")
fi
if [[ -n "${SFTP_CIPHER:-}" ]]; then
    SFTP_OPTS+=(-c "$SFTP_CIPHER")
fi
if [[ -n "${SFTP_BUFFER:-}" ]]; then
    SFTP_OPTS+=(-B "$SFTP_BUFFER")
fi

cleanup() {
    rm -f "$BATCH_FILE" "$STDERR_FILE"
    rm -rf "$DOWNLOAD_DIR"
}
trap cleanup EXIT

mkdir -p "$DOWNLOAD_DIR"

{
    echo "mkdir $REMOTE_DIR"
    echo "cd $REMOTE_DIR"
    echo "put $DATASET testfile.bin"
    echo "lcd $DOWNLOAD_DIR"
    echo "get testfile.bin"
    echo "rm testfile.bin"
    echo "cd /"
    echo "rmdir $REMOTE_DIR"
    echo "bye"
} > "$BATCH_FILE"

status=0
timeout 900 sftp -q -o BatchMode=yes -o IdentityFile="$SFTP_IDENTITY_FILE" \
    "${SFTP_OPTS[@]}" -b "$BATCH_FILE" -P "$PORT" "$USER@localhost" \
    >/dev/null 2>"$STDERR_FILE" || status=$?
if [[ "$status" != 0 || -s "$STDERR_FILE" ]]; then
    cat "$STDERR_FILE" >&2
    exit 1
fi
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
if [[ ! -x "$BINARY" ]]; then
    cargo build --release --bin sftp-s3 -q
fi

echo "Starting memory server"
RUST_LOG=error "$BINARY" --backend memory --port "$PORT" \
    --authorized-keys-file "$KEY.pub" >"$RESULTS_DIR/server.log" 2>&1 &
SERVER_PID=$!
wait_for_server

RUN_SCRIPT=$(write_run_script)
DOWNLOAD_ROOT="$RESULTS_DIR/downloads"
mkdir -p "$DOWNLOAD_ROOT"

TRANSFER_MB=$((TOTAL_MB * 2))

echo ""
echo "=== Single File Benchmark ==="
echo "Dataset:       $DATASET"
echo "Payload:       $TOTAL_MB MiB upload + $TOTAL_MB MiB download"
echo "Runs:          $RUNS measured, $WARMUP warmup"
echo "SFTP requests: ${SFTP_REQUESTS:-default}"
echo "SFTP cipher:   ${SFTP_CIPHER:-default}"
echo "SFTP buffer:   ${SFTP_BUFFER:-default}"
echo "Results:       $RESULTS_DIR"
echo ""

hyperfine \
    --warmup "$WARMUP" \
    --runs "$RUNS" \
    --export-json "$RESULTS_DIR/results.json" \
    --command-name "single-file-${TOTAL_MB}mb-r${SFTP_REQUESTS:-default}-${SFTP_CIPHER:-default}-b${SFTP_BUFFER:-default}" \
    "bash '$RUN_SCRIPT' '$PORT' '$DATASET' '$DOWNLOAD_ROOT' '$USER'"

echo ""
echo "Summary"
jq -r \
    --argjson mb "$TRANSFER_MB" \
    '.results[0]
     | "  throughput: \((($mb / .mean) * 10 | round / 10)) MB/s\n"
       + "  mean:       \((.mean * 1000 | round / 1000))s\n"
       + "  stddev:     \((((.stddev // 0) * 1000) | round / 1000))s\n"
       + "  range:      \((.min * 1000 | round / 1000))s - \((.max * 1000 | round / 1000))s"' \
    "$RESULTS_DIR/results.json"

echo "JSON: $RESULTS_DIR/results.json"
