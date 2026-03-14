#!/usr/bin/env bash
# Benchmark: 2 russh configs × multiple sizes, using hyperfine for interleaved comparison.
#
# Builds each config binary once, starts both servers simultaneously on different ports,
# then runs hyperfine with both commands per file size. Hyperfine interleaves runs
# so CPU/memory/thermal conditions are equal for all configs.
#
# To reduce variance from background processes (e.g., plex), run with taskset:
#   taskset -c 0-3 ./benchmark-all.sh  # Pin benchmark to CPUs 0-3

set -euo pipefail

SFTP_DIR="/home/mjc/projects/sftp-s3-rs"
RESULTS_DIR="$SFTP_DIR/benchmark_results"
BINS_DIR="$SFTP_DIR/benchmark_bins"
LOG="$RESULTS_DIR/run-$(date +%Y%m%d-%H%M%S).log"
SFTP_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o Compression=no)

mkdir -p "$RESULTS_DIR"
exec > >(tee "$LOG") 2>&1
echo "Logging to $LOG"

# Scenarios: (label_suffix|client_source|server_backend)
# client_source: where client reads files from (disk directory or /dev/shm)
# server_backend: memory or local (local support WIP)
SCENARIOS=(
    "disk|$SFTP_DIR|memory"
    "shm|/dev/shm|memory"
)
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

# label|russh_branch|sftp_branch|extra_cargo_features (ports assigned dynamically below)
_CONFIGS_BASE=(
    "main-master|main|master|sftp-master"
    "main-deser|main|deserialize-bytes-optimization|"
    "reduce-master|reduce-mlock-usage|master|sftp-master"
    "reduce-deser|reduce-mlock-usage|deserialize-bytes-optimization|"
    "write-master|write-path-refactor|master|sftp-master"
    "write-deser|write-path-refactor|deserialize-bytes-optimization|"
)

# label:size_mb combos to skip — format: "label:size"
# main-* branches at 256MB+: mlock() exhaustion in russh main branch.
# CryptoVec is used for non-secret packet buffers (enc.write, PacketWriter) and mlock()s
# all backing memory but never munlock()s on clear(). The 256MB tier alone (25 runs × 5
# warmups) generates enough transfers to exceed RLIMIT_MEMLOCK (8MB), causing 17s±74s
# variance and eventual timeouts. At 1024MB, servers crash on first warmup.
# write-path-refactor and reduce-mlock-usage fix this.
#
# reduce-master at 1024MB: crashes during warmup. reduce-mlock-usage branch appears
# to have stability issues at large transfer sizes. reduce-deser works fine.
SKIP_AT=(
    "main-master:256"
    "main-master:1024"
    "main-deser:256"
    "main-deser:1024"
    "reduce-master:1024"
)

# label|russh_branch|sftp_branch|port|extra_cargo_features
CONFIGS=()
for _cfg in "${_CONFIGS_BASE[@]}"; do
    IFS='|' read -r _lbl _russh _sftp _feat <<< "$_cfg"
    CONFIGS+=("$_lbl|$_russh|$_sftp|$(pick_free_port)|$_feat")
done
unset _CONFIGS_BASE _cfg _lbl _russh _sftp _feat

# Iterations distributed across scenarios (total = iters / num_scenarios per scenario)
# With 3 scenarios, each config gets iters/3 runs per scenario
SIZES_ITERS=(
    "1:180:2"
    "32:45:3"
    "256:18:5"
    "1024:9:5"
)

mkdir -p "$BINS_DIR"

# Pre-create test files from /dev/urandom (compresses poorly, reflecting real data)
echo "=== Test files ==="
for si in "${SIZES_ITERS[@]}"; do
    size=${si%%:*}

    # Create in disk directory
    f="$SFTP_DIR/testfile_${size}mb.bin"
    if [[ ! -f "$f" ]]; then
        echo "  Creating ${size}MB (disk)..."
        dd if=/dev/urandom of="$f" bs=1M count="$size" status=none
    else
        echo "  ${size}MB (disk) exists"
    fi

    # Create in /dev/shm if available
    if [[ -w /dev/shm ]]; then
        f="/dev/shm/testfile_${size}mb.bin"
        if [[ ! -f "$f" ]]; then
            echo "  Creating ${size}MB (/dev/shm)..."
            dd if=/dev/urandom of="$f" bs=1M count="$size" status=none
        else
            echo "  ${size}MB (/dev/shm) exists"
        fi
    fi
done

# Build all configs in parallel using git worktrees to avoid Cargo.toml patching races
echo ""
echo "=== Building (parallel) ==="

_build_one() {
    local label=$1 russh=$2 sftp=$3 features=$4
    local wt="/tmp/bench-wt-$label"
    local tgt="/tmp/bench-target-$label"
    local bin="$BINS_DIR/sftp-s3-$label"
    local logfile="$RESULTS_DIR/build-$label.log"

    git -C "$SFTP_DIR" worktree remove --force "$wt" 2>/dev/null || true
    rm -rf "$wt"
    git -C "$SFTP_DIR" worktree add -q "$wt" HEAD

    sed -i "s|github\.com/mjc/russh\.git\", branch = \"[^\"]*\"|github.com/mjc/russh.git\", branch = \"$russh\"|g" "$wt/Cargo.toml"
    sed -i "s|github\.com/mjc/russh-sftp\.git\", branch = \"[^\"]*\"|github.com/mjc/russh-sftp.git\", branch = \"$sftp\"|g" "$wt/Cargo.toml"

    echo "  $label (russh=$russh sftp=$sftp${features:+ features=$features})"
    (
        cd "$wt"
        cargo update --quiet >>"$logfile" 2>&1 || true
        unset NIX_ENFORCE_NO_NATIVE
        CARGO_TARGET_DIR="$tgt" RUSTFLAGS="-C target-cpu=native" RUSTC_WRAPPER=sccache \
            cargo build --release ${features:+--features $features} -q >>"$logfile" 2>&1
    )
    cp "$tgt/release/sftp-s3" "$bin"
    git -C "$SFTP_DIR" worktree remove --force "$wt" 2>/dev/null || true
    echo "    -> $bin"
}

declare -a _build_pids=()
for config in "${CONFIGS[@]}"; do
    IFS='|' read -r label russh sftp _port features <<< "$config"
    _build_one "$label" "$russh" "$sftp" "$features" &
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

start_servers() {
    local backend=${1:-memory}
    stop_servers
    for config in "${CONFIGS[@]}"; do
        IFS='|' read -r label _russh _sftp port _feat <<< "$config"
        if [[ "$backend" == "local" ]]; then
            local root="/tmp/sftp-bench-root-$label"
            mkdir -p "$root"
            "$BINS_DIR/sftp-s3-$label" --port "$port" --user "benchmark:benchmark" --backend local --root "$root" \
                >"$RESULTS_DIR/server-$label.log" 2>&1 &
        else
            "$BINS_DIR/sftp-s3-$label" --port "$port" --user "benchmark:benchmark" --backend memory \
                >"$RESULTS_DIR/server-$label.log" 2>&1 &
        fi
        echo $! > "/tmp/sftp-bench-$port.pid"
    done
    for config in "${CONFIGS[@]}"; do
        IFS='|' read -r label _russh _sftp port _feat <<< "$config"
        for i in {1..50}; do
            if sshpass -p "benchmark" sftp -q "${SFTP_OPTS[@]}" -P "$port" "benchmark@localhost" <<< "bye" >/dev/null 2>&1; then
                break
            fi
            if [[ -f "/tmp/sftp-bench-$port.pid" ]] && ! kill -0 "$(cat "/tmp/sftp-bench-$port.pid")" 2>/dev/null; then
                echo "ERROR: server $label ($port) exited during startup; see $RESULTS_DIR/server-$label.log" >&2
                return 1
            fi
            sleep 0.1
        done
        if ! sshpass -p "benchmark" sftp -q "${SFTP_OPTS[@]}" -P "$port" "benchmark@localhost" <<< "bye" >/dev/null 2>&1; then
            echo "ERROR: server $label ($port) failed to accept SFTP connections; see $RESULTS_DIR/server-$label.log" >&2
            return 1
        fi
    done
    echo "  Servers up: $(for c in "${CONFIGS[@]}"; do IFS='|' read -r lbl _ _ p _ <<< "$c"; echo -n "$lbl:$p "; done)"
}

stop_servers() {
    for config in "${CONFIGS[@]}"; do
        IFS='|' read -r label _russh _sftp port _feat <<< "$config"
        if [[ -f "/tmp/sftp-bench-$port.pid" ]]; then
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

# Run benchmarks for each scenario
for scenario in "${SCENARIOS[@]}"; do
    IFS='|' read -r scenario_label client_source server_backend <<< "$scenario"
    echo ""
    echo "### Scenario: client=$scenario_label, backend=$server_backend ###"

    # Ensure servers from previous scenario are stopped
    stop_servers
    sleep 0.5

for si in "${SIZES_ITERS[@]}"; do
    size=${si%%:*}
    rest=${si#*:}
    iters=${rest%%:*}
    warmup=${rest##*:}
    testfile="$SFTP_DIR/testfile_${size}mb.bin"
    outjson="$RESULTS_DIR/${size}mb-${scenario_label}.json"

    echo ""
    echo "--- ${size}MB × ${iters} runs (${warmup} warmup) ---"
    start_servers "$server_backend" || { echo "Skipping ${size}MB" >&2; continue; }

    cmd_names=()
    cmds=()
    for config in "${CONFIGS[@]}"; do
        IFS='|' read -r label _russh _sftp port _feat <<< "$config"
        if [[ " ${SKIP_AT[*]} " == *" $label:$size "* ]]; then
            echo "  SKIP $label at ${size}MB (see SKIP_AT)" >&2
            continue
        fi
        cmd_names+=(--command-name "$label")
        cmds+=("bash $SFTP_DIR/run-one.sh $port $testfile $client_source")
    done

    if [[ ${#cmds[@]} -eq 0 ]]; then
        echo "Skipping ${size}MB (all configs filtered)" >&2
        continue
    fi

    if ! hyperfine \
        --warmup "$warmup" \
        --runs "$iters" \
        --export-json "$outjson" \
        "${cmd_names[@]}" \
        "${cmds[@]}"; then
        echo "ERROR: benchmark failed for ${size}MB; rerun the printed command with --show-output if needed." >&2
        stop_servers
        continue
    fi

    stop_servers

    echo "  Throughput (roundtrip = upload+download / wall time):"
    jq -r --argjson mb "$((size * 2))" \
        '.results[] | "    \(.command): \($mb / .mean | . * 10 | round / 10) MB/s  (±\($mb * .stddev / (.mean * .mean) | . * 10 | round / 10))"' \
        "$outjson"
done
done  # End scenario loop

echo ""
echo "=== FINAL SUMMARY (by scenario) ==="
for scenario in "${SCENARIOS[@]}"; do
    IFS='|' read -r scenario_label _ _ <<< "$scenario"
    echo ""
    echo "Scenario: $scenario_label"
    printf "%-15s" "Config"
    for si in "${SIZES_ITERS[@]}"; do
        size=${si%%:*}
        printf "  %8s" "${size}MB"
    done
    echo ""
    printf '%0.s─' {1..60}; echo

    for config in "${CONFIGS[@]}"; do
        IFS='|' read -r label _ _ _ _ <<< "$config"
        printf "%-15s" "$label"
        for si in "${SIZES_ITERS[@]}"; do
            size_str=${si%%:*}
            outjson="$RESULTS_DIR/${size_str}mb-${scenario_label}.json"
            if [[ -f "$outjson" ]]; then
                speed=$(jq -r --argjson mb "$((size_str * 2))" --arg lbl "$label" \
                    '.results[] | select(.command == $lbl) | $mb / .mean | . * 10 | round / 10' \
                    "$outjson" 2>/dev/null)
                printf "  %7s" "${speed:-n/a}"
            else
                printf "  %7s" "n/a"
            fi
        done
        echo ""
    done
done

echo ""
echo "Done. $(date)"
