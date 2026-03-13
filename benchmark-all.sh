#!/usr/bin/env bash
# Benchmark: 2 russh configs × multiple sizes, using hyperfine for interleaved comparison.
#
# Builds each config binary once, starts both servers simultaneously on different ports,
# then runs hyperfine with both commands per file size. Hyperfine interleaves runs
# so CPU/memory/thermal conditions are equal for all configs.

SFTP_DIR="/home/mjc/projects/sftp-s3-rs"
RESULTS_DIR="$SFTP_DIR/benchmark_results"
BINS_DIR="$SFTP_DIR/benchmark_bins"
LOG="$RESULTS_DIR/run-$(date +%Y%m%d-%H%M%S).log"

mkdir -p "$RESULTS_DIR"
exec > >(tee "$LOG") 2>&1
echo "Logging to $LOG"

# label|russh_branch|sftp_branch|port|extra_cargo_features
CONFIGS=(
    "main-master|main|master|2223|sftp-master"
    "main-deser|main|deserialize-bytes-optimization|2224|"
    "write-master|write-path-refactor|master|2225|sftp-master"
    "write-deser|write-path-refactor|deserialize-bytes-optimization|2226|"
)

SIZES_ITERS=(
    "1:200"
    "32:50"
    "256:20"
    "1024:10"
)

mkdir -p "$BINS_DIR"

# Pre-create test files from /dev/urandom (compresses poorly, reflecting real data)
echo "=== Test files ==="
for si in "${SIZES_ITERS[@]}"; do
    size=${si%%:*}
    f="$SFTP_DIR/testfile_${size}mb.bin"
    if [[ ! -f "$f" ]]; then
        echo "  Creating ${size}MB..."
        dd if=/dev/urandom of="$f" bs=1M count="$size" status=none
    else
        echo "  ${size}MB exists"
    fi
done

# Build all configs
echo ""
echo "=== Building ==="
for config in "${CONFIGS[@]}"; do
    IFS='|' read -r label russh sftp _port features <<< "$config"
    bin="$BINS_DIR/sftp-s3-$label"
    echo "  $label (russh=$russh sftp=$sftp${features:+ features=$features})"
    cd "$SFTP_DIR"
    sed -i "s|github\.com/mjc/russh\.git\", branch = \"[^\"]*\"|github.com/mjc/russh.git\", branch = \"$russh\"|g" Cargo.toml
    sed -i "s|github\.com/mjc/russh-sftp\.git\", branch = \"[^\"]*\"|github.com/mjc/russh-sftp.git\", branch = \"$sftp\"|g" Cargo.toml
    cargo update -p russh -p russh-sftp --quiet 2>/dev/null || true
    unset NIX_ENFORCE_NO_NATIVE
    cargo clean -q
    CARGO_FEATURES="${features:+--features $features}"
    RUSTFLAGS="-C target-cpu=native" RUSTC_WRAPPER=sccache cargo build --release $CARGO_FEATURES -q
    cp target/release/sftp-s3 "$bin"
    echo "    -> $bin"
done
echo "All builds done."

start_servers() {
    for config in "${CONFIGS[@]}"; do
        IFS='|' read -r _label _russh _sftp port _feat <<< "$config"
        fuser -k "$port/tcp" 2>/dev/null || true
    done
    sleep 0.3
    for config in "${CONFIGS[@]}"; do
        IFS='|' read -r label _russh _sftp port _feat <<< "$config"
        "$BINS_DIR/sftp-s3-$label" --port "$port" --user "benchmark:benchmark" --backend memory >/dev/null 2>&1 &
        echo $! > "/tmp/sftp-bench-$port.pid"
    done
    for config in "${CONFIGS[@]}"; do
        IFS='|' read -r label _russh _sftp port _feat <<< "$config"
        for i in {1..50}; do
            nc -z localhost "$port" 2>/dev/null && break
            sleep 0.1
        done
        if ! nc -z localhost "$port" 2>/dev/null; then
            echo "ERROR: server $label ($port) failed to start" >&2
            return 1
        fi
    done
    echo "  Servers up: $(for c in "${CONFIGS[@]}"; do IFS='|' read -r lbl _ _ p _ <<< "$c"; echo -n "$lbl:$p "; done)"
}

stop_servers() {
    for config in "${CONFIGS[@]}"; do
        IFS='|' read -r _label _russh _sftp port _feat <<< "$config"
        if [[ -f "/tmp/sftp-bench-$port.pid" ]]; then
            kill "$(cat "/tmp/sftp-bench-$port.pid")" 2>/dev/null || true
            rm -f "/tmp/sftp-bench-$port.pid"
        fi
        fuser -k "$port/tcp" 2>/dev/null || true
    done
    sleep 0.2
}

echo ""
echo "=== Benchmarks ==="

for si in "${SIZES_ITERS[@]}"; do
    size=${si%%:*}
    iters=${si##*:}
    testfile="$SFTP_DIR/testfile_${size}mb.bin"
    outjson="$RESULTS_DIR/${size}mb.json"

    echo ""
    echo "--- ${size}MB × ${iters} runs ---"
    start_servers || { echo "Skipping ${size}MB" >&2; continue; }

    cmd_names=()
    cmds=()
    for config in "${CONFIGS[@]}"; do
        IFS='|' read -r label _russh _sftp port _feat <<< "$config"
        cmd_names+=(--command-name "$label")
        cmds+=("bash $SFTP_DIR/run-one.sh $port $testfile")
    done

    hyperfine \
        --warmup 3 \
        --runs "$iters" \
        --export-json "$outjson" \
        "${cmd_names[@]}" \
        "${cmds[@]}"

    stop_servers

    echo "  Throughput (roundtrip = upload+download / wall time):"
    jq -r --argjson mb "$((size * 2))" \
        '.results[] | "    \(.command): \($mb / .mean | . * 10 | round / 10) MB/s  (±\($mb * .stddev / (.mean * .mean) | . * 10 | round / 10))"' \
        "$outjson"
done

echo ""
echo "=== FINAL SUMMARY ==="
printf "\n%-15s" "Config"
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
        size=${si%%:*}
        outjson="$RESULTS_DIR/${size}mb.json"
        if [[ -f "$outjson" ]]; then
            speed=$(jq -r --argjson mb "$((size * 2))" --arg lbl "$label" \
                '.results[] | select(.command == $lbl) | $mb / .mean | . * 10 | round / 10' \
                "$outjson" 2>/dev/null)
            printf "  %7s" "${speed:-n/a}"
        else
            printf "  %7s" "n/a"
        fi
    done
    echo ""
done

echo ""
echo "Done. $(date)"
