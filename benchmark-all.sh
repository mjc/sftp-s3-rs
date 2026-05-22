#!/usr/bin/env bash
# Benchmark the current sftp-s3-rs branch against a russh × russh-sftp branch matrix.
#
# Builds a clean temp worktree for each config, rewrites dependencies to remote git
# branches, then runs hyperfine across the configured file sizes/scenarios.
#
# To reduce variance from background processes (e.g., plex), run with taskset:
#   taskset -c 0-3 ./benchmark-all.sh  # Pin benchmark to CPUs 0-3

set -euo pipefail

SFTP_DIR="/home/mjc/projects/sftp-s3-rs"
SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
RESULTS_DIR="$SFTP_DIR/benchmark_results"
BINS_DIR="$SFTP_DIR/benchmark_bins"
LOG="$RESULTS_DIR/run-$(date +%Y%m%d-%H%M%S).log"
SFTP_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o Compression=no)
BENCH_CLIENT="openssh"
BENCH_CIPHERS="${SFTP_BENCH_CIPHERS:-}"
RUSSH_GIT_URL="https://github.com/mjc/russh.git"
RUSSH_SFTP_GIT_URL="https://github.com/mjc/russh-sftp.git"
BENCH_KEY_DIR="/tmp/sftp-bench-keys"
BENCH_KEY="$BENCH_KEY_DIR/id_ed25519"
BENCH_AUTHORIZED_KEYS="$BENCH_KEY.pub"

if [[ "${BENCHMARK_ALL_IN_ENV:-0}" != 1 ]] && command -v direnv >/dev/null 2>&1 && [[ -f "$SFTP_DIR/.envrc" ]]; then
    exec env BENCHMARK_ALL_IN_ENV=1 direnv exec "$SFTP_DIR" "$SCRIPT_PATH" "$@"
fi

while [[ $# -gt 0 ]]; do
    case "$1" in
        --client)
            BENCH_CLIENT=$2
            shift 2
            ;;
        --client=*)
            BENCH_CLIENT=${1#*=}
            shift
            ;;
        --ciphers)
            BENCH_CIPHERS=$2
            shift 2
            ;;
        --ciphers=*)
            BENCH_CIPHERS=${1#*=}
            shift
            ;;
        -h|--help)
            echo "Usage: $0 [--client openssh|rust] [--ciphers c1,c2]"
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            echo "Usage: $0 [--client openssh|rust] [--ciphers c1,c2]" >&2
            exit 2
            ;;
    esac
done

case "$BENCH_CLIENT" in
    openssh|rust) ;;
    *)
        echo "unknown benchmark client '$BENCH_CLIENT' (expected openssh or rust)" >&2
        exit 2
        ;;
esac

mkdir -p "$RESULTS_DIR"
exec > >(tee "$LOG") 2>&1
echo "Logging to $LOG"
echo "Benchmark client: $BENCH_CLIENT"
if [[ -n "$BENCH_CIPHERS" ]]; then
    echo "Benchmark ciphers: $BENCH_CIPHERS"
fi

# Scenarios: (label_suffix|client_source|server_backend)
# client_source: where client reads files from (disk directory or /dev/shm)
# server_backend: memory or local (local support WIP)
SCENARIOS=(
    "disk|$SFTP_DIR|memory"
    "shm|/dev/shm|memory"
)
if [[ "$BENCH_CLIENT" == "rust" ]]; then
    SCENARIOS=("generated|$SFTP_DIR|memory")
fi
export SFTP_BENCH_CIPHERS="$BENCH_CIPHERS"
# Uncomment to test all combinations (requires testing/debugging local backend)
#SCENARIOS=(
#    "disk|$SFTP_DIR|memory"
#    "shm|/dev/shm|memory"
#    "disk-local|$SFTP_DIR|local"
#    "shm-local|/dev/shm|local"
#)

pick_free_port() {
    local port
    while true; do
        port=$(( RANDOM % 16384 + 49152 ))
        nc -z localhost "$port" 2>/dev/null || { echo "$port"; return; }
    done
}

CONFIGS=()
for _russh_kind_ref in tag:v0.57.0 branch:main branch:write-path-refactor; do
    IFS=: read -r _russh_kind _russh <<< "$_russh_kind_ref"
    for _sftp in master deserialize-bytes-optimization zero-copy-serialize; do
        case "$_sftp" in
            master)
                _sftp_label=master
                _feat="sftp-master"
                ;;
            deserialize-bytes-optimization)
                _sftp_label=deserialize
                _feat=
                ;;
            zero-copy-serialize)
                _sftp_label=zero-copy
                _feat=
                ;;
        esac
        config_entry="${_russh}-${_sftp_label}|${_russh_kind}|${_russh}|${_sftp}|$(pick_free_port)|${_feat}"
        CONFIGS+=("$config_entry")
    done
done
unset _russh_kind_ref _russh_kind _russh _sftp _sftp_label _feat

# Iterations distributed across scenarios (total = iters / num_scenarios per scenario)
# With 2 scenarios, each config gets iters/2 runs per scenario
SIZES_ITERS=(
    "1:180:2"
    "32:45:3"
    "256:18:5"
    "1024:9:5"
)

mkdir -p "$BINS_DIR"

# Pre-create test files on disk (reused across runs)
echo "=== Test files ==="
for si in "${SIZES_ITERS[@]}"; do
    size=${si%%:*}
    f="$SFTP_DIR/testfile_${size}mb.bin"
    if [[ ! -f "$f" ]]; then
        echo "  Creating ${size}MB (disk)..."
        dd if=/dev/urandom of="$f" bs=1M count="$size" status=none
    else
        echo "  ${size}MB (disk) exists"
    fi
done

create_shm_files() {
    if [[ ! -w /dev/shm ]]; then
        return 0
    fi
    echo "  Creating test files in /dev/shm..."
    for si in "${SIZES_ITERS[@]}"; do
        size=${si%%:*}
        f="/dev/shm/testfile_${size}mb.bin"
        dd if=/dev/urandom of="$f" bs=1M count="$size" status=none 2>/dev/null
    done
}

cleanup_shm_files() {
    if [[ ! -w /dev/shm ]]; then
        return 0
    fi
    echo "  Cleaning up /dev/shm test files..."
    for si in "${SIZES_ITERS[@]}"; do
        size=${si%%:*}
        f="/dev/shm/testfile_${size}mb.bin"
        rm -f "$f"
    done
}

ensure_benchmark_key() {
    mkdir -p "$BENCH_KEY_DIR"
    chmod 700 "$BENCH_KEY_DIR"
    if [[ ! -f "$BENCH_KEY" || ! -f "$BENCH_AUTHORIZED_KEYS" ]]; then
        ssh-keygen -q -t ed25519 -N "" -f "$BENCH_KEY" >/dev/null
    fi
    chmod 600 "$BENCH_KEY"
    chmod 644 "$BENCH_AUTHORIZED_KEYS"
    export SFTP_IDENTITY_FILE="$BENCH_KEY"
}

# Build all configs in isolated worktrees so dependency rewrites stay local to the benchmark.
echo ""
echo "=== Building ==="
echo "Using russh remote refs from $RUSSH_GIT_URL"
echo "Using russh-sftp remote branches from $RUSSH_SFTP_GIT_URL"
ensure_benchmark_key
echo "Using benchmark client key $BENCH_KEY"

if [[ "$BENCH_CLIENT" == "rust" ]]; then
    echo "Building Rust benchmark client..."
    (
        cd "$SFTP_DIR"
        unset NIX_ENFORCE_NO_NATIVE
        RUSTFLAGS="-C target-cpu=native" RUSTC_WRAPPER=sccache \
            cargo build --release --bin sftp-bench-client -q
    )
    export SFTP_BENCH_CLIENT_BIN="$SFTP_DIR/target/release/sftp-bench-client"
    echo "Using Rust benchmark client $SFTP_BENCH_CLIENT_BIN"
fi

_build_one() {
    local label=$1 russh_kind=$2 russh_ref=$3 sftp=$4 features=$5
    local wt="/tmp/bench-wt-$label"
    local tgt="/tmp/bench-target-$label"
    local bin="$BINS_DIR/sftp-s3-$label"
    local logfile="$RESULTS_DIR/build-$label.log"
    local russh_commit russh_spec

    : >"$logfile"

    git -C "$SFTP_DIR" worktree remove --force "$wt" 2>/dev/null || true
    rm -rf "$wt"
    git -C "$SFTP_DIR" worktree add -q "$wt" HEAD

    case "$russh_kind" in
        branch)
            russh_commit=$(git ls-remote "$RUSSH_GIT_URL" "refs/heads/$russh_ref" | awk '{print $1}')
            ;;
        tag)
            russh_commit=$(git ls-remote "$RUSSH_GIT_URL" "refs/tags/$russh_ref" | awk '{print $1}')
            ;;
        *)
            echo "ERROR: unsupported russh ref kind '$russh_kind' for $label" >&2
            return 1
            ;;
    esac
    if [[ -z "$russh_commit" ]]; then
        echo "ERROR: failed to resolve russh $russh_kind '$russh_ref' for $label" >&2
        return 1
    fi
    russh_spec="rev = \"$russh_commit\""

    sed -i \
        -e "s|^russh = .*|russh = { git = \"$RUSSH_GIT_URL\", $russh_spec, default-features = false, features = [\"aws-lc-rs\", \"flate2\"] }|" \
        -e "s|^russh-sftp = .*|russh-sftp = { git = \"$RUSSH_SFTP_GIT_URL\", branch = \"$sftp\" }|" \
        "$wt/Cargo.toml"

    # Each matrix entry intentionally rewrites core git dependencies. Reusing
    # HEAD's lockfile can force incompatible transitive versions onto older
    # russh/russh-sftp combinations, so resolve the rewritten graph directly.
    rm -f "$wt/Cargo.lock"

    echo "  $label (russh $russh_kind=$russh_ref sftp=$sftp${features:+ features=$features})"
    (
        cd "$wt"
        cargo update --quiet >>"$logfile" 2>&1 || true
        unset NIX_ENFORCE_NO_NATIVE
        CARGO_TARGET_DIR="$tgt" RUSTFLAGS="-C target-cpu=native" RUSTC_WRAPPER=sccache \
            cargo build --release --bin sftp-s3 ${features:+--features $features} -q >>"$logfile" 2>&1
    )
    cp "$tgt/release/sftp-s3" "$bin"
    git -C "$SFTP_DIR" worktree remove --force "$wt" 2>/dev/null || true
    echo "    -> $bin"
}

declare -a _build_pids=()
for config in "${CONFIGS[@]}"; do
    IFS='|' read -r label russh_kind russh_ref sftp _port features <<< "$config"
    _build_one "$label" "$russh_kind" "$russh_ref" "$sftp" "$features" &
    _build_pids+=($!)
done

_build_ok=true
for _pid in "${_build_pids[@]}"; do
    wait "$_pid" || _build_ok=false
done
$_build_ok || { echo "ERROR: one or more builds failed; see $RESULTS_DIR/build-*.log" >&2; exit 1; }
echo "All builds done."

port_responding() {
    local port=$1
    nc -z localhost "$port" >/dev/null 2>&1
}

wait_for_port_release() {
    local label=$1
    local port=$2
    for _ in {1..50}; do
        if ! port_responding "$port"; then
            return 0
        fi
        sleep 0.1
    done
    echo "ERROR: port $port for $label is still busy after cleanup" >&2
    return 1
}

start_server() {
    local label=$1 backend=${2:-memory}
    stop_server "$label"

    # Find config for this label
    local config port russh_kind russh_ref sftp features
    for c in "${CONFIGS[@]}"; do
        IFS='|' read -r lbl russh_kind russh_ref sftp port features <<< "$c"
        if [[ "$lbl" == "$label" ]]; then
            config="$c"
            break
        fi
    done
    [[ -z "$config" ]] && { echo "ERROR: config not found for $label" >&2; return 1; }
    IFS='|' read -r label russh_kind russh_ref sftp port features <<< "$config"

    if [[ "$backend" == "local" ]]; then
        local root="/tmp/sftp-bench-root-$label"
        mkdir -p "$root"
        "$BINS_DIR/sftp-s3-$label" --port "$port" --authorized-keys-file "$BENCH_AUTHORIZED_KEYS" --user benchmark:benchmark ${BENCH_CIPHERS:+--ciphers "$BENCH_CIPHERS"} --backend local --root "$root" \
            >"$RESULTS_DIR/server-$label.log" 2>&1 &
    else
        "$BINS_DIR/sftp-s3-$label" --port "$port" --authorized-keys-file "$BENCH_AUTHORIZED_KEYS" --user benchmark:benchmark ${BENCH_CIPHERS:+--ciphers "$BENCH_CIPHERS"} --backend memory \
            >"$RESULTS_DIR/server-$label.log" 2>&1 &
    fi
    echo $! > "/tmp/sftp-bench-$port.pid"

    # Wait for server to be ready
    for _ in {1..50}; do
        if sftp -q -o BatchMode=yes -o IdentityFile="$BENCH_KEY" "${SFTP_OPTS[@]}" -P "$port" "benchmark@localhost" <<< "bye" >/dev/null 2>&1; then
            echo "  Server $label up on port $port"
            return 0
        fi
        if [[ -f "/tmp/sftp-bench-$port.pid" ]] && ! kill -0 "$(cat "/tmp/sftp-bench-$port.pid")" 2>/dev/null; then
            echo "ERROR: server $label ($port) exited during startup; see $RESULTS_DIR/server-$label.log" >&2
            return 1
        fi
        sleep 0.1
    done
    echo "ERROR: server $label ($port) failed to accept SFTP connections; see $RESULTS_DIR/server-$label.log" >&2
    return 1
}

stop_server() {
    local label=$1
    # Find port for this label
    local port
    for c in "${CONFIGS[@]}"; do
        IFS='|' read -r lbl _ _ _ p _ <<< "$c"
        if [[ "$lbl" == "$label" ]]; then
            port=$p
            break
        fi
    done
    [[ -z "$port" ]] && return 0

    if [[ -f "/tmp/sftp-bench-$port.pid" ]]; then
        local pid
        pid=$(cat "/tmp/sftp-bench-$port.pid")
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
        rm -f "/tmp/sftp-bench-$port.pid"
    fi
    fuser -k "$port/tcp" 2>/dev/null || true
    wait_for_port_release "$label" "$port" || return 1
}

# shellcheck disable=SC2329
stop_servers() {
    for config in "${CONFIGS[@]}"; do
        IFS='|' read -r label _russh_kind _russh_ref _sftp port _feat <<< "$config"
        if [[ -f "/tmp/sftp-bench-$port.pid" ]]; then
            local pid
            pid=$(cat "/tmp/sftp-bench-$port.pid")
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
            rm -f "/tmp/sftp-bench-$port.pid"
        fi
        fuser -k "$port/tcp" 2>/dev/null || true
        wait_for_port_release "$label" "$port" || return 1
    done
}

trap stop_servers EXIT INT TERM

echo ""
echo "=== Benchmarks ==="

# Run benchmarks sequentially: one config at a time to avoid resource contention
for scenario in "${SCENARIOS[@]}"; do
    IFS='|' read -r scenario_label client_source server_backend <<< "$scenario"
    echo ""
    echo "### Scenario: client=$scenario_label, backend=$server_backend ###"

    # Create /dev/shm files if needed
    if [[ "$scenario_label" == "shm"* ]]; then
        create_shm_files
    fi

    for config in "${CONFIGS[@]}"; do
        IFS='|' read -r label _russh_kind _russh_ref _sftp port _feat <<< "$config"

        for si in "${SIZES_ITERS[@]}"; do
            size=${si%%:*}
            rest=${si#*:}
            iters=${rest%%:*}
            warmup=${rest##*:}
            testfile="$SFTP_DIR/testfile_${size}mb.bin"
            outjson="$RESULTS_DIR/${size}mb-${scenario_label}-${label}.json"

            if [[ "$size" -eq 1024 && ( "$_russh_ref" == "main" || "$_russh_ref" == "v0.57.0" ) ]]; then
                echo ""
                echo "--- $label ${size}MB skipped ---"
                echo "  Skipping ${size}MB benchmark for $label: ${_russh_ref} is expected to fail this case"
                continue
            fi

            echo ""
            echo "--- $label ${size}MB × ${iters} runs (${warmup} warmup) ---"

            start_server "$label" "$server_backend" || { echo "Skipping $label at ${size}MB" >&2; continue; }

            if ! hyperfine \
                --warmup "$warmup" \
                --runs "$iters" \
                --export-json "$outjson" \
                --command-name "$label" \
                "bash $SFTP_DIR/run-one.sh --client $BENCH_CLIENT ${BENCH_CIPHERS:+--ciphers $BENCH_CIPHERS} $port $testfile $client_source"; then
                echo "ERROR: benchmark failed for $label at ${size}MB; see $RESULTS_DIR/server-$label.log" >&2
                stop_server "$label"
                continue
            fi

            stop_server "$label"

            echo "  Throughput (roundtrip = upload+download / wall time):"
            jq -r --argjson mb "$((size * 2))" \
                '.results[] | "    \(.command): \($mb / .mean | . * 10 | round / 10) MB/s  (±\($mb * .stddev / (.mean * .mean) | . * 10 | round / 10))"' \
                "$outjson"
        done
    done

    # Cleanup /dev/shm files after scenario
    if [[ "$scenario_label" == "shm"* ]]; then
        cleanup_shm_files
    fi
done  # End scenario loop

echo ""
echo "=== FINAL SUMMARY (by scenario) ==="

config_width=${#"Config"}
for config in "${CONFIGS[@]}"; do
    IFS='|' read -r label _ _ _ _ _ <<< "$config"
    if (( ${#label} > config_width )); then
        config_width=${#label}
    fi
done
if (( config_width < 28 )); then
    config_width=28
fi

size_width=8
separator_width=$config_width
for _ in "${SIZES_ITERS[@]}"; do
    separator_width=$((separator_width + 2 + size_width))
done

for scenario in "${SCENARIOS[@]}"; do
    IFS='|' read -r scenario_label _ _ <<< "$scenario"
    echo ""
    echo "Scenario: $scenario_label"
    printf "%-*s" "$config_width" "Config"
    for si in "${SIZES_ITERS[@]}"; do
        size=${si%%:*}
        printf "  %${size_width}s" "${size}MB"
    done
    echo ""
    printf '%*s\n' "$separator_width" '' | tr ' ' '─'

    for config in "${CONFIGS[@]}"; do
        IFS='|' read -r label _ _ _ _ <<< "$config"
        printf "%-*s" "$config_width" "$label"
        for si in "${SIZES_ITERS[@]}"; do
            size_str=${si%%:*}
            outjson="$RESULTS_DIR/${size_str}mb-${scenario_label}-${label}.json"
            if [[ -f "$outjson" ]]; then
                speed=$(jq -r --argjson mb "$((size_str * 2))" \
                    '$mb / .results[0].mean | . * 10 | round / 10' \
                    "$outjson" 2>/dev/null)
                printf "  %${size_width}s" "${speed:-n/a}"
            else
                printf "  %${size_width}s" "n/a"
            fi
        done
        echo ""
    done
done

echo ""
echo "Done. $(date)"
