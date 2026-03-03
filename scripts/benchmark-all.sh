#!/bin/bash
# Comprehensive benchmark harness
# 6 configs × 4 sizes × 100 iterations each
#
# Config matrix (russh × russh-sftp, sftp-s3-rs branch follows russh-sftp compat):
#   1. russh:main         + russh-sftp:master                   → sftp-s3-rs:baseline
#   2. russh:main         + russh-sftp:deserialize-bytes-optim  → sftp-s3-rs:baseline
#   3. russh:main         + russh-sftp:zero-copy-serialize      → sftp-s3-rs:main
#   4. russh:write-path   + russh-sftp:master                   → sftp-s3-rs:baseline
#   5. russh:write-path   + russh-sftp:deserialize-bytes-optim  → sftp-s3-rs:baseline
#   6. russh:write-path   + russh-sftp:zero-copy-serialize      → sftp-s3-rs:main

set -e

SFTP_DIR="/home/mjc/projects/sftp-s3-rs"
RUSSH_DIR="/home/mjc/projects/russh"
RUSSH_SFTP_DIR="/home/mjc/projects/russh-sftp"
SIZES=(1 256 512 1024)
ITERS=100
RESULTS_DIR="$SFTP_DIR/benchmark_results"

mkdir -p "$RESULTS_DIR"

build_config() {
    local s3_branch=$1
    local russh_branch=$2
    local sftp_branch=$3

    echo "  sftp-s3-rs=$s3_branch  russh=$russh_branch  russh-sftp=$sftp_branch"
    git -C "/home/mjc/projects/sftp-s3-rs"      checkout "$s3_branch"    --quiet
    git -C "/home/mjc/projects/russh"            checkout "$russh_branch" --quiet
    git -C "/home/mjc/projects/russh-sftp"       checkout "$sftp_branch"  --quiet

    cd "/home/mjc/projects/sftp-s3-rs"
    unset NIX_ENFORCE_NO_NATIVE
    RUSTFLAGS="-C target-cpu=native" cargo build --release -q
    echo "  Build done."
}

run_size() {
    local config_label=$1
    local size=$2
    local outfile="$RESULTS_DIR/${config_label}_${size}mb.txt"

    echo ""
    echo "  --- ${size}MB x ${ITERS} iters ---"
    > "$outfile"

    for iter in $(seq 1 $ITERS); do
        result=$(bash "$SFTP_DIR/scripts/run-benchmark.sh" "$size" "${config_label}" 2>&1)

        upload=$(echo "$result"   | grep -m1 "^Upload:"    | grep -oP '[0-9]+\.[0-9]+(?= MB/s)')
        download=$(echo "$result" | grep -m1 "^Download:"  | grep -oP '[0-9]+\.[0-9]+(?= MB/s)')
        rt=$(echo "$result"       | grep -m1 "^Roundtrip:" | grep -oP '[0-9]+\.[0-9]+(?= MB/s)')

        echo "$iter,$upload,$download,$rt" >> "$outfile"

        if (( iter % 20 == 0 )); then
            echo "    [$iter/$ITERS] up=${upload} down=${download} rt=${rt} MB/s"
        fi
    done

    echo ""
    echo "=== ${config_label} | ${size}MB ==="
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
        printf "  upload     mean=%7.2f  stdev=%6.2f  cv=%4.1f%%  min=%7.2f  max=%7.2f  MB/s\n", um, us, us/um*100, umin, umax;
        printf "  download   mean=%7.2f  stdev=%6.2f  cv=%4.1f%%  min=%7.2f  max=%7.2f  MB/s\n", dm, ds, ds/dm*100, dmin, dmax;
        printf "  roundtrip  mean=%7.2f  stdev=%6.2f  cv=%4.1f%%  min=%7.2f  max=%7.2f  MB/s\n", rm, rs, rs/rm*100, rmin, rmax;
    }' "$outfile"
}

# ── Configs ───────────────────────────────────────────────────────────────────
# Each entry: "label|s3_branch|russh_branch|sftp_branch"

CONFIGS=(
    "1_russh-main_sftp-master|baseline|main|master"
    "2_russh-main_sftp-deserialize|baseline|main|deserialize-bytes-optimization"
    "3_russh-main_sftp-zerocopy|main|main|zero-copy-serialize"
    "4_russh-write_sftp-master|baseline|write-path-refactor|master"
    "5_russh-write_sftp-deserialize|baseline|write-path-refactor|deserialize-bytes-optimization"
    "6_russh-write_sftp-zerocopy|main|write-path-refactor|zero-copy-serialize"
)

echo "=== Comprehensive Benchmark Suite ==="
echo "Sizes: ${SIZES[*]} MB  |  Iterations: $ITERS  |  RUSTFLAGS: -C target-cpu=native"
echo "Results: $RESULTS_DIR"
echo ""

for config in "${CONFIGS[@]}"; do
    IFS='|' read -r label s3_branch russh_branch sftp_branch <<< "$config"

    echo ""
    echo "══════════════════════════════════════════════"
    echo "CONFIG: $label"
    build_config "$s3_branch" "$russh_branch" "$sftp_branch"

    for size in "${SIZES[@]}"; do
        run_size "$label" "$size"
    done
done

# ── Final summary table ───────────────────────────────────────────────────────
echo ""
echo "=== FINAL SUMMARY ==="
printf "\n%-35s  %6s  %9s  %9s  %9s\n" "Config" "Size" "Upload" "Download" "Roundtrip"
printf '%0.s-' {1..80}; echo
for config in 1_russh-main_sftp-master 2_russh-main_sftp-deserialize 3_russh-main_sftp-zerocopy 4_russh-write_sftp-master 5_russh-write_sftp-deserialize 6_russh-write_sftp-zerocopy; do
    for size in 1 256 512 1024; do
        f="$RESULTS_DIR/${config}_${size}mb.txt"
        [[ -f "$f" ]] || continue
        awk -F, -v cfg="$config" -v sz="$size" '
            NF==4 { up+=$2; down+=$3; rt+=$4; n++ }
            END { if(n>0) printf "%-35s  %4dMB  %8.1f  %8.1f  %8.1f\n", cfg, sz, up/n, down/n, rt/n }
        ' "$f"
    done
done

# Restore to fully-optimized state
echo ""
echo "Restoring optimized branches..."
git -C "/home/mjc/projects/sftp-s3-rs"   checkout main                        --quiet
git -C "/home/mjc/projects/russh"         checkout write-path-refactor          --quiet
git -C "/home/mjc/projects/russh-sftp"    checkout zero-copy-serialize          --quiet
cd "/home/mjc/projects/sftp-s3-rs"
unset NIX_ENFORCE_NO_NATIVE
RUSTFLAGS="-C target-cpu=native" cargo build --release -q
echo "Done."
