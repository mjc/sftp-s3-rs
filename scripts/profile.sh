#!/usr/bin/env bash
set -euo pipefail

# CPU profiling for sftp-s3.
#
# Builds the server with frame pointers, runs it under perf, and generates a
# flamegraph. By default it uses the memory backend and authenticates clients
# with $HOME/.ssh/authorized_keys.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

ATTACH_PID=""
BACKEND="${PROFILE_BACKEND:-memory}"
PORT="${PORT:-2222}"
LOCAL_ROOT="${LOCAL_ROOT:-$PROJECT_DIR/profile-root}"
AUTHORIZED_KEYS_FILE="${AUTHORIZED_KEYS_FILE:-$HOME/.ssh/authorized_keys}"
RESULTS_DIR="${RESULTS_DIR:-"$PROJECT_DIR/benchmark_results/profile-cpu-$(date +%Y%m%d-%H%M%S)"}"
FREQUENCY="${PERF_FREQUENCY:-997}"
EXTRA_ARGS=()
GENERATED_FLAMEGRAPH=0
PERF_STATUS=0

generate_flamegraph() {
    if [[ "$GENERATED_FLAMEGRAPH" -eq 1 ]]; then
        return
    fi
    GENERATED_FLAMEGRAPH=1

    if [[ ! -s "$RESULTS_DIR/perf.data" ]]; then
        return
    fi

    echo ""
    echo "Generating flamegraph..."
    if perf script -i "$RESULTS_DIR/perf.data" 2>"$RESULTS_DIR/perf-script.err" |
        inferno-collapse-perf |
        inferno-flamegraph --title "sftp-s3 CPU" >"$RESULTS_DIR/flamegraph.svg"; then
        echo "Done: $RESULTS_DIR/flamegraph.svg"
    else
        echo "Error: failed to generate flamegraph; see $RESULTS_DIR/perf-script.err" >&2
    fi
}

finish() {
    local status=$?
    if [[ "$status" -eq 130 || "$status" -eq 143 || "$PERF_STATUS" -ne 0 ]]; then
        generate_flamegraph
    fi
}

trap finish EXIT
trap 'PERF_STATUS=130; exit 130' INT
trap 'PERF_STATUS=143; exit 143' TERM

usage() {
    cat <<EOF
Usage: $0 [OPTIONS] [-- EXTRA sftp-s3 ARGS...]

Profile sftp-s3 CPU usage with perf.

Options:
  --pid PID                    Attach to an already-running process
  --backend memory|local       Backend to launch when not attaching (default: $BACKEND)
  --port PORT                  Listen port when launching (default: $PORT)
  --root PATH                  Local backend root (default: $LOCAL_ROOT)
  --authorized-keys-file PATH  Authorized keys file (default: $AUTHORIZED_KEYS_FILE)
  --results-dir PATH           Output directory (default: benchmark_results/profile-cpu-...)
  -h, --help                   Show this help

Examples:
  nix develop -c scripts/profile.sh
  nix develop -c scripts/profile.sh --backend local --root /tmp/sftp-profile-root
  nix develop -c scripts/profile.sh --pid 12345

Stop the server with Ctrl-C to generate flamegraph.svg.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)
            usage
            exit 0
            ;;
        --pid)
            ATTACH_PID="$2"
            shift 2
            ;;
        --backend)
            BACKEND="$2"
            shift 2
            ;;
        --port)
            PORT="$2"
            shift 2
            ;;
        --root)
            LOCAL_ROOT="$2"
            shift 2
            ;;
        --authorized-keys-file)
            AUTHORIZED_KEYS_FILE="$2"
            shift 2
            ;;
        --results-dir)
            RESULTS_DIR="$2"
            shift 2
            ;;
        --)
            shift
            EXTRA_ARGS+=("$@")
            break
            ;;
        *)
            EXTRA_ARGS+=("$1")
            shift
            ;;
    esac
done

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "Error: perf profiling requires Linux" >&2
    exit 1
fi

if ! command -v perf >/dev/null 2>&1; then
    echo "Error: perf not found; run through nix develop" >&2
    exit 1
fi

if ! command -v inferno-collapse-perf >/dev/null 2>&1 ||
    ! command -v inferno-flamegraph >/dev/null 2>&1; then
    echo "Error: inferno tools not found; run through nix develop" >&2
    exit 1
fi

mkdir -p "$RESULTS_DIR"

if [[ -z "$ATTACH_PID" ]]; then
    case "$BACKEND" in
        memory|local) ;;
        *)
            echo "Error: --backend must be memory or local" >&2
            exit 1
            ;;
    esac

    if [[ ! -r "$AUTHORIZED_KEYS_FILE" ]]; then
        echo "Error: authorized keys file is not readable: $AUTHORIZED_KEYS_FILE" >&2
        exit 1
    fi

    if [[ "$BACKEND" == "local" ]]; then
        mkdir -p "$LOCAL_ROOT"
    fi

    echo "Building sftp-s3 profiling binary..."
    RUSTFLAGS="-C target-cpu=native -C force-frame-pointers=yes" \
        cargo build --profile profiling --bin sftp-s3
fi

echo 0 | sudo tee /proc/sys/kernel/kptr_restrict >/dev/null
echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid >/dev/null

printf '\033]0;perf: sftp-s3 CPU\007'

set +e
if [[ -n "$ATTACH_PID" ]]; then
    echo "Attaching to PID $ATTACH_PID..."
    echo "Press Ctrl-C to stop recording and generate flamegraph."
    perf record \
        -g --call-graph fp -F "$FREQUENCY" \
        -o "$RESULTS_DIR/perf.data" \
        -p "$ATTACH_PID"
    PERF_STATUS=$?
else
    SERVER_CMD=(
        "$PROJECT_DIR/target/profiling/sftp-s3"
        --backend "$BACKEND"
        --port "$PORT"
        --authorized-keys-file "$AUTHORIZED_KEYS_FILE"
    )

    if [[ "$BACKEND" == "local" ]]; then
        SERVER_CMD+=(--root "$LOCAL_ROOT")
    fi

    echo "Profiling: ${SERVER_CMD[*]} ${EXTRA_ARGS[*]}"
    echo "Results: $RESULTS_DIR"
    echo "Stop the server with Ctrl-C to generate flamegraph.svg."
    echo ""

    perf record \
        -g --call-graph fp -F "$FREQUENCY" \
        -o "$RESULTS_DIR/perf.data" \
        -- "${SERVER_CMD[@]}" "${EXTRA_ARGS[@]}"
    PERF_STATUS=$?
fi
set -e

if [[ ! -f "$RESULTS_DIR/perf.data" ]]; then
    echo "Error: perf.data not found" >&2
    exit 1
fi

generate_flamegraph
exit "$PERF_STATUS"
