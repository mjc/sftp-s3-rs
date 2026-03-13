#!/bin/bash
# Comprehensive benchmark: 6 configs × 5 sizes
# Server starts ONCE per (config, size), runs all iterations, then stops.
#
# Sizes & iterations:
#   1MB    × 10000
#   32MB   × 100
#   256MB  × 50
#   1024MB × 10
#   2048MB × 10

SFTP_DIR="/home/mjc/projects/sftp-s3-rs"
RUSSH_DIR="/home/mjc/projects/russh"
RUSSH_SFTP_DIR="/home/mjc/projects/russh-sftp"
RESULTS_DIR="$SFTP_DIR/benchmark_results"
RUN_BENCH="$SFTP_DIR/run-benchmark.sh"
PORT=2223
MAX_RETRIES=3

SIZES_ITERS=(
    "1:10000"
    "32:100"
    "256:50"
    "1024:10"
    "2048:10"
)

mkdir -p "$RESULTS_DIR"

# Pre-create all test files
echo "Pre-creating test files..."
for si in "${SIZES_ITERS[@]}"; do
    size=${si%%:*}
    f="$SFTP_DIR/testfile_${size}mb.bin"
    if [[ ! -f "$f" ]]; then
        echo "  Creating ${size}MB (random)..."
        dd if=/dev/urandom of="$f" bs=1M count=$size status=none
    else
        echo "  ${size}MB exists"
    fi
done
echo ""

build_config() {
    local russh_branch=$1 sftp_branch=$2
    echo "  Branches: russh=$russh_branch  russh-sftp=$sftp_branch"
    cd "$SFTP_DIR"
    # Swap branch names in Cargo.toml (both [dependencies] and [dev-dependencies])
    sed -i "s|github\.com/mjc/russh\.git\", branch = \"[^\"]*\"|github.com/mjc/russh.git\", branch = \"$russh_branch\"|g" Cargo.toml
    sed -i "s|github\.com/mjc/russh-sftp\.git\", branch = \"[^\"]*\"|github.com/mjc/russh-sftp.git\", branch = \"$sftp_branch\"|g" Cargo.toml
    cargo update -p russh -p russh-sftp --quiet 2>/dev/null
    unset NIX_ENFORCE_NO_NATIVE
    cargo clean -q
    RUSTFLAGS="-C target-cpu=native" RUSTC_WRAPPER=sccache cargo build --release -q
    echo "  Build done."
}

start_server() {
    # Kill any leftover
    fuser -k $PORT/tcp 2>/dev/null || true
    sleep 0.3

    "$SFTP_DIR/target/release/sftp-s3" --port $PORT --user "benchmark:benchmark" --backend memory >/dev/null 2>&1 &
    SERVER_PID=$!

    for i in {1..30}; do
        nc -z localhost $PORT 2>/dev/null && return 0
        kill -0 $SERVER_PID 2>/dev/null || { echo "    Server died on start" >&2; return 1; }
        sleep 0.2
    done
    echo "    Server start timeout" >&2
    return 1
}

stop_server() {
    if [[ -n "$SERVER_PID" ]]; then
        kill $SERVER_PID 2>/dev/null
        wait $SERVER_PID 2>/dev/null
        SERVER_PID=""
    fi
    fuser -k $PORT/tcp 2>/dev/null || true
    sleep 0.2
}

run_size() {
    local config_label=$1 size=$2 iters=$3
    local outfile="$RESULTS_DIR/${config_label}_${size}mb.txt"
    local testfile="$SFTP_DIR/testfile_${size}mb.bin"

    # Resume support
    local existing=0
    [[ -f "$outfile" ]] && existing=$(wc -l < "$outfile")
    if (( existing >= iters )); then
        echo "  ${size}MB: $existing/$iters done, skipping"
        print_stats "$outfile" "$config_label" "$size"
        return 0
    fi
    local start_iter=$((existing + 1))
    echo "  ${size}MB × ${iters} (starting at $start_iter)"

    # Start server once for all iterations of this size
    start_server || { echo "    FATAL: cannot start server for ${size}MB" >&2; return 1; }

    local progress_every=$((iters / 5))
    (( progress_every < 1 )) && progress_every=1

    for iter in $(seq $start_iter $iters); do
        local success=false
        for retry in $(seq 1 $MAX_RETRIES); do
            # Check server is still alive
            if ! kill -0 $SERVER_PID 2>/dev/null; then
                echo "    Server died at iter $iter, restarting..." >&2
                start_server || break
            fi

            result=$(bash "$RUN_BENCH" "$size" "$testfile" 2>&1)
            if [[ $? -eq 0 ]]; then
                upload=$(echo "$result"   | grep -m1 "^Upload:"    | grep -oP '[0-9]+\.[0-9]+(?= MB/s)')
                download=$(echo "$result" | grep -m1 "^Download:"  | grep -oP '[0-9]+\.[0-9]+(?= MB/s)')
                rt=$(echo "$result"       | grep -m1 "^Roundtrip:" | grep -oP '[0-9]+\.[0-9]+(?= MB/s)')
                if [[ -n "$upload" && -n "$download" && -n "$rt" ]]; then
                    echo "$iter,$upload,$download,$rt" >> "$outfile"
                    success=true
                    break
                fi
            fi
            echo "    WARN: iter $iter retry $retry failed" >&2
            sleep 0.5
        done
        $success || { echo "    ERROR: iter $iter failed, skipping" >&2; continue; }

        if (( iter % progress_every == 0 )); then
            echo "    [$iter/$iters] up=$upload down=$download rt=$rt MB/s"
        fi
    done

    # Stop server after all iterations for this size
    stop_server

    echo ""
    print_stats "$outfile" "$config_label" "$size"
}

print_stats() {
    local outfile=$1 config_label=$2 size=$3
    echo "  === ${config_label} | ${size}MB ==="
    awk -F, 'NF==4 {
        up+=$2; down+=$3; rt+=$4;
        up2+=$2*$2; down2+=$3*$3; rt2+=$4*$4;
        if(NR==1||$2<umin) umin=$2; if($2>umax) umax=$2;
        if(NR==1||$3<dmin) dmin=$3; if($3>dmax) dmax=$3;
        if(NR==1||$4<rmin) rmin=$4; if($4>rmax) rmax=$4;
        n++
    } END {
        um=up/n;   us=sqrt(up2/n   - um*um);
        dm=down/n; ds=sqrt(down2/n - dm*dm);
        rm=rt/n;   rs=sqrt(rt2/n   - rm*rm);
        printf "    upload     mean=%7.2f  stdev=%6.2f  cv=%4.1f%%  min=%7.2f  max=%7.2f  MB/s\n", um, us, us/um*100, umin, umax;
        printf "    download   mean=%7.2f  stdev=%6.2f  cv=%4.1f%%  min=%7.2f  max=%7.2f  MB/s\n", dm, ds, ds/dm*100, dmin, dmax;
        printf "    roundtrip  mean=%7.2f  stdev=%6.2f  cv=%4.1f%%  min=%7.2f  max=%7.2f  MB/s\n", rm, rs, rs/rm*100, rmin, rmax;
    }' "$outfile"
}

CONFIGS=(
    "1_russh-main_sftp-deserialize|main|deserialize-bytes-optimization"
    "2_russh-main_sftp-zerocopy|main|zero-copy-serialize"
    "3_russh-write_sftp-deserialize|write-path-refactor|deserialize-bytes-optimization"
    "4_russh-write_sftp-zerocopy|write-path-refactor|zero-copy-serialize"
)

echo "════════════════════════════════════════════════════════"
echo "  Benchmark Suite (server persists per size)"
echo "  1MB×10k  32MB×100  256MB×50  1GB×10  2GB×10"
echo "  4 configs  |  RUSTFLAGS: -C target-cpu=native"
echo "════════════════════════════════════════════════════════"
echo ""

for config in "${CONFIGS[@]}"; do
    IFS='|' read -r label russh_branch sftp_branch <<< "$config"
    echo ""
    echo "╔══════════════════════════════════════════╗"
    echo "║ CONFIG: $label"
    echo "╚══════════════════════════════════════════╝"
    build_config "$russh_branch" "$sftp_branch"

    for si in "${SIZES_ITERS[@]}"; do
        size=${si%%:*}
        iters=${si##*:}
        run_size "$label" "$size" "$iters"
    done
done

# Final summary
echo ""
echo "════════════════════════════════════════════════════════"
echo "  FINAL SUMMARY (mean MB/s)"
echo "════════════════════════════════════════════════════════"
printf "\n%-35s  %6s  %9s  %9s  %9s\n" "Config" "Size" "Upload" "Download" "Roundtrip"
printf '%0.s─' {1..80}; echo
for config in 1_russh-main_sftp-deserialize 2_russh-main_sftp-zerocopy 3_russh-write_sftp-deserialize 4_russh-write_sftp-zerocopy; do
    for si in "1:10000" "32:100" "256:50" "1024:10" "2048:10"; do
        size=${si%%:*}
        f="$RESULTS_DIR/${config}_${size}mb.txt"
        [[ -f "$f" ]] || continue
        awk -F, -v cfg="$config" -v sz="$size" '
            NF==4 { up+=$2; down+=$3; rt+=$4; n++ }
            END { if(n>0) printf "%-35s  %4dMB  %8.1f  %8.1f  %8.1f\n", cfg, sz, up/n, down/n, rt/n }
        ' "$f"
    done
done

echo ""
echo "Done. $(date)"
