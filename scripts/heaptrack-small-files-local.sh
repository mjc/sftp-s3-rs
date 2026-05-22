#!/usr/bin/env bash
# Heaptrack the local-disk many-small-files benchmark.
# Usage: scripts/heaptrack-small-files-local.sh [total_mb]

set -euo pipefail

find_heaptrack_data() {
    local results_dir=$1
    find "$results_dir" -maxdepth 1 -type f \
        \( -name "heaptrack*.zst" -o -name "heaptrack*.gz" \) \
        ! -name "heaptrack-print.txt" \
        | sort \
        | head -n1
}

self_test() {
    local tmp
    tmp=$(mktemp -d)
    SELF_TEST_TMP=$tmp
    trap 'rm -rf "$SELF_TEST_TMP"' EXIT

    if [[ -n "$(find_heaptrack_data "$tmp")" ]]; then
        echo "expected no heaptrack data in empty directory" >&2
        exit 1
    fi

    touch "$tmp/heaptrack.zst"
    touch "$tmp/heaptrack.log"
    if [[ "$(find_heaptrack_data "$tmp")" != "$tmp/heaptrack.zst" ]]; then
        echo "failed to find heaptrack.zst" >&2
        exit 1
    fi

    rm "$tmp/heaptrack.zst"
    touch "$tmp/heaptrack.123.gz.zst"
    touch "$tmp/heaptrack-print.txt"
    if [[ "$(find_heaptrack_data "$tmp")" != "$tmp/heaptrack.123.gz.zst" ]]; then
        echo "failed to find heaptrack.*.gz.zst" >&2
        exit 1
    fi
}

if [[ "${1:-}" == "--self-test" ]]; then
    self_test
    exit 0
fi

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TOTAL_MB=${1:-1024}
DATASET_DIR=${DATASET_DIR:-"$ROOT/benchmark_data/small-files-${TOTAL_MB}mb"}
RESULTS_DIR=${RESULTS_DIR:-"$ROOT/benchmark_results/small-files-local-heaptrack-hotpath-$(date +%Y%m%d-%H%M%S)"}
KEY_DIR=${KEY_DIR:-/tmp/sftp-small-files-bench-keys}
KEY="$KEY_DIR/id_ed25519"
PORT=${PORT:-22230}
USER=${SFTP_USER:-benchmark}
SFTP_REQUESTS=${SFTP_REQUESTS:-}
SFTP_JOBS=${SFTP_JOBS:-1}
SFTP_CIPHER=${SFTP_CIPHER:-}
SFTP_BUFFER=${SFTP_BUFFER-131072}
BINARY="$ROOT/target/profiling/sftp-s3"

mkdir -p "$RESULTS_DIR" "$KEY_DIR"
exec > >(tee "$RESULTS_DIR/run.log") 2>&1

while nc -z localhost "$PORT" >/dev/null 2>&1; do
    PORT=$((PORT + 1))
done

if [[ ! -f "$KEY" ]]; then
    ssh-keygen -q -t ed25519 -N "" -f "$KEY" >/dev/null
fi
chmod 600 "$KEY"
chmod 644 "$KEY.pub"
export SFTP_IDENTITY_FILE="$KEY"

if [[ ! -f "$DATASET_DIR/.manifest" ]]; then
    echo "Missing dataset manifest: $DATASET_DIR/.manifest" >&2
    echo "Run scripts/benchmark-small-files.sh $TOTAL_MB 1 0 once to create it." >&2
    exit 1
fi

FILE_COUNT=$(awk -F= '$1 == "files" { print $2 }' "$DATASET_DIR/.manifest")
TRANSFER_MB=$((TOTAL_MB * 2))
TRANSFER_FILES=$((FILE_COUNT * 2))
LOCAL_ROOT="$RESULTS_DIR/local-root"
DOWNLOAD_ROOT="$RESULTS_DIR/downloads"
RUN_SCRIPT="$RESULTS_DIR/run-one-small-files.sh"

mkdir -p "$LOCAL_ROOT" "$DOWNLOAD_ROOT"

cat > "$RUN_SCRIPT" <<'RUNONE'
#!/usr/bin/env bash
set -euo pipefail

PORT=$1
DATASET_DIR=$2
DOWNLOAD_ROOT=$3
USER=$4
SFTP_REQUESTS=${5:-}
SFTP_JOBS=${6:-1}
SFTP_CIPHER=${7:-}
SFTP_BUFFER=${8:-}

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
export SFTP_REQUESTS SFTP_JOBS SFTP_CIPHER SFTP_BUFFER

cleanup() {
    rm -f /tmp/sftp-small-files-batch-$$-*.txt
    rm -f /tmp/sftp-small-files-stderr-$$-*.txt
    rm -rf "$DOWNLOAD_ROOT"/small-files-"$USER"-$$-*
}
trap cleanup EXIT

run_job() {
    local job=$1
    local remote_dir="small-files-$USER-$$-$job-$RANDOM"
    local download_dir="$DOWNLOAD_ROOT/$remote_dir"
    local batch_file="/tmp/sftp-small-files-batch-$$-$job.txt"
    local stderr_file="/tmp/sftp-small-files-stderr-$$-$job.txt"

    mkdir -p "$download_dir"

    {
        echo "mkdir $remote_dir"
        echo "cd $remote_dir"
        echo "lcd $DATASET_DIR"
        find "$DATASET_DIR" -maxdepth 1 -type f ! -name .manifest -printf "%f\n" |
            sort |
            awk -v jobs="$SFTP_JOBS" -v job="$job" 'NR % jobs == job' |
            while IFS= read -r file; do
                echo "put $file"
            done
        echo "lcd $download_dir"
        find "$DATASET_DIR" -maxdepth 1 -type f ! -name .manifest -printf "%f\n" |
            sort |
            awk -v jobs="$SFTP_JOBS" -v job="$job" 'NR % jobs == job' |
            while IFS= read -r file; do
                echo "get $file"
            done
        find "$DATASET_DIR" -maxdepth 1 -type f ! -name .manifest -printf "%f\n" |
            sort |
            awk -v jobs="$SFTP_JOBS" -v job="$job" 'NR % jobs == job' |
            while IFS= read -r file; do
                echo "rm $file"
            done
        echo "cd /"
        echo "rmdir $remote_dir"
        echo "bye"
    } > "$batch_file"

    local status=0
    timeout 900 sftp -q -o BatchMode=yes -o IdentityFile="$SFTP_IDENTITY_FILE" \
        "${SFTP_OPTS[@]}" -b "$batch_file" -P "$PORT" "$USER@localhost" \
        >/dev/null 2>"$stderr_file" || status=$?
    if [[ "$status" != 0 || -s "$stderr_file" ]]; then
        cat "$stderr_file" >&2
        return 1
    fi
}

pids=()
for ((job = 0; job < SFTP_JOBS; job++)); do
    run_job "$job" &
    pids+=("$!")
done
for pid in "${pids[@]}"; do
    wait "$pid"
done
RUNONE
chmod +x "$RUN_SCRIPT"

cd "$ROOT"
echo "Building profiling binary"
cargo build --profile profiling --bin sftp-s3 -q

echo "Starting local disk server under heaptrack on port $PORT"
RUST_LOG=error heaptrack \
    --output "$RESULTS_DIR/heaptrack" \
    "$BINARY" \
    --backend local \
    --root "$LOCAL_ROOT" \
    --port "$PORT" \
    --authorized-keys-file "$KEY.pub" \
    >"$RESULTS_DIR/server.log" \
    2>"$RESULTS_DIR/heaptrack.log" &
SERVER_PID=$!

cleanup_server() {
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
}
trap cleanup_server EXIT INT TERM

ready=0
for _ in {1..150}; do
    if sftp -q -o BatchMode=yes -o IdentityFile="$KEY" \
        -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null \
        -o Compression=no \
        -P "$PORT" "$USER@localhost" <<< bye >/dev/null 2>&1; then
        ready=1
        echo "Server ready"
        break
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "Server exited"
        sed -n '1,120p' "$RESULTS_DIR/server.log"
        sed -n '1,120p' "$RESULTS_DIR/heaptrack.log"
        exit 1
    fi
    sleep 0.1
done
if [[ "$ready" != 1 ]]; then
    echo "Server did not become ready"
    exit 1
fi

echo "Heaptracking local-disk many-small-files benchmark"
echo "Results: $RESULTS_DIR"
echo "SFTP requests: ${SFTP_REQUESTS:-default}"
echo "SFTP jobs: $SFTP_JOBS"
echo "SFTP cipher: ${SFTP_CIPHER:-default}"
echo "SFTP buffer: ${SFTP_BUFFER:-default}"
hyperfine \
    --warmup 1 \
    --runs 10 \
    --export-json "$RESULTS_DIR/results.json" \
    --command-name "small-files-${TOTAL_MB}mb-local-heaptrack-hotpath-r${SFTP_REQUESTS:-default}-j${SFTP_JOBS}-${SFTP_CIPHER:-default}-b${SFTP_BUFFER:-default}" \
    "bash '$RUN_SCRIPT' '$PORT' '$DATASET_DIR' '$DOWNLOAD_ROOT' '$USER' '${SFTP_REQUESTS}' '$SFTP_JOBS' '$SFTP_CIPHER' '${SFTP_BUFFER}'"

echo "Stopping heaptracked server"
cleanup_server
trap - EXIT

HEAP_FILE=$(find_heaptrack_data "$RESULTS_DIR")
if [[ -z "$HEAP_FILE" ]]; then
    echo "No heaptrack data file found in $RESULTS_DIR" >&2
    exit 1
fi
echo "Heaptrack data: $HEAP_FILE"
echo "Writing heaptrack summary"
heaptrack_print "$HEAP_FILE" > "$RESULTS_DIR/heaptrack-print.txt"

jq -r \
    --argjson mb "$TRANSFER_MB" \
    --argjson files "$TRANSFER_FILES" \
    '.results[0]
     | "Summary\n"
       + "  throughput: \((($mb / .mean) * 10 | round / 10)) MB/s\n"
       + "  file ops:   \((($files / .mean) * 10 | round / 10)) files/s\n"
       + "  mean:       \((.mean * 1000 | round / 1000))s\n"
       + "  stddev:     \((((.stddev // 0) * 1000) | round / 1000))s\n"
       + "  range:      \((.min * 1000 | round / 1000))s - \((.max * 1000 | round / 1000))s"' \
    "$RESULTS_DIR/results.json"
echo "HEAPTRACK_RESULTS=$RESULTS_DIR"
