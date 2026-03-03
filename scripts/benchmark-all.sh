#!/bin/bash
# Comprehensive benchmark harness
# Tests 4 configs × 4 sizes × 100 iterations each
# Configs:
#   1. russh:main        + russh-sftp:master
#   2. russh:main        + russh-sftp:zero-copy-serialize
#   3. russh:write-path  + russh-sftp:master
#   4. russh:write-path  + russh-sftp:zero-copy-serialize (full optimized)

set -e

SFTP_DIR="/home/mjc/projects/sftp-s3-rs"
RUSSH_DIR="/home/mjc/projects/russh"
RUSSH_SFTP_DIR="/home/mjc/projects/russh-sftp"
SIZES=(1 256 512 1024)
ITERS=100
RESULTS_DIR="$SFTP_DIR/benchmark_results"

mkdir -p "$RESULTS_DIR"

build_config() {
    local russh_branch=$1
    local sftp_branch=$2

    echo "  Switching russh -> $russh_branch ..."
    git -C "$RUSSH_DIR" checkout "$russh_branch" --quiet

    echo "  Switching russh-sftp -> $sftp_branch ..."
    git -C "$RUSSH_SFTP_DIR" checkout "$sftp_branch" --quiet

    echo "  Building sftp-s3-rs with target-cpu=native ..."
    cd "$SFTP_DIR"
    RUSTFLAGS="-C target-cpu=native" cargo build --release -q 2>&1
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

        upload=$(echo "$result"   | grep "^Upload:"    | grep -oP '[0-9]+\.[0-9]+(?= MB/s)')
        download=$(echo "$result" | grep "^Download:"  | grep -oP '[0-9]+\.[0-9]+(?= MB/s)')
        rt=$(echo "$result"       | grep "^Roundtrip:" | grep -oP '[0-9]+\.[0-9]+(?= MB/s)')

        echo "$iter,$upload,$download,$rt" >> "$outfile"

        if (( iter % 20 == 0 )); then
            echo "    [$iter/$ITERS] up=${upload} down=${download} rt=${rt} MB/s"
        fi
    done

    python3 - "$outfile" "$config_label" "$size" <<'PY'
import sys, statistics

path, label, size = sys.argv[1], sys.argv[2], sys.argv[3]
rows = [line.strip().split(",") for line in open(path) if line.strip()]
ups   = [float(r[1]) for r in rows if len(r)==4]
downs = [float(r[2]) for r in rows if len(r)==4]
rts   = [float(r[3]) for r in rows if len(r)==4]

def fmt(name, vals):
    m = statistics.mean(vals)
    s = statistics.stdev(vals) if len(vals)>1 else 0
    cv = s/m*100 if m else 0
    print(f"  {name:12s}  mean={m:7.2f}  stdev={s:6.2f}  cv={cv:4.1f}%  min={min(vals):7.2f}  max={max(vals):7.2f}  MB/s")

print(f"\n=== {label} | {size}MB | n={len(rts)} ===")
fmt("upload",    ups)
fmt("download",  downs)
fmt("roundtrip", rts)
PY
}

# ── Configs ───────────────────────────────────────────────────────────────────

ORDERED=(
    "1_russh-main_sftp-master"
    "2_russh-main_sftp-zerocopy"
    "3_russh-branch_sftp-master"
    "4_russh-branch_sftp-zerocopy"
)

declare -A RUSSH_BRANCH=(
    ["1_russh-main_sftp-master"]="main"
    ["2_russh-main_sftp-zerocopy"]="main"
    ["3_russh-branch_sftp-master"]="write-path-refactor"
    ["4_russh-branch_sftp-zerocopy"]="write-path-refactor"
)

declare -A SFTP_BRANCH=(
    ["1_russh-main_sftp-master"]="master"
    ["2_russh-main_sftp-zerocopy"]="zero-copy-serialize"
    ["3_russh-branch_sftp-master"]="master"
    ["4_russh-branch_sftp-zerocopy"]="zero-copy-serialize"
)

echo "=== Comprehensive Benchmark Suite ==="
echo "Sizes: ${SIZES[*]} MB  |  Iterations: $ITERS  |  RUSTFLAGS: -C target-cpu=native"
echo "Results: $RESULTS_DIR"
echo ""

for config_label in "${ORDERED[@]}"; do
    russh_branch="${RUSSH_BRANCH[$config_label]}"
    sftp_branch="${SFTP_BRANCH[$config_label]}"

    echo ""
    echo "══════════════════════════════════════════════"
    echo "CONFIG: $config_label"
    echo "  russh=$russh_branch  russh-sftp=$sftp_branch"
    echo "══════════════════════════════════════════════"

    build_config "$russh_branch" "$sftp_branch"

    for size in "${SIZES[@]}"; do
        run_size "$config_label" "$size"
    done
done

# ── Final summary table ───────────────────────────────────────────────────────
echo ""
echo "=== FINAL SUMMARY ==="
python3 - "$RESULTS_DIR" <<PY
import sys, os, statistics

results_dir = sys.argv[1]

configs = [
    "1_russh-main_sftp-master",
    "2_russh-main_sftp-zerocopy",
    "3_russh-branch_sftp-master",
    "4_russh-branch_sftp-zerocopy",
]
sizes = [1, 256, 512, 1024]

print(f"\n{'Config':<40}  {'Size':>6}  {'Upload':>9}  {'Download':>9}  {'Roundtrip':>9}")
print("-" * 85)

for config in configs:
    for size in sizes:
        path = os.path.join(results_dir, f"{config}_{size}mb.txt")
        if not os.path.exists(path):
            continue
        rows = [line.strip().split(",") for line in open(path) if line.strip()]
        ups   = [float(r[1]) for r in rows if len(r)==4]
        downs = [float(r[2]) for r in rows if len(r)==4]
        rts   = [float(r[3]) for r in rows if len(r)==4]
        if not rts:
            continue
        print(f"{config:<40}  {size:>4}MB  {statistics.mean(ups):>8.1f}  {statistics.mean(downs):>8.1f}  {statistics.mean(rts):>8.1f}")
PY

# Restore to optimized branches
echo ""
echo "Restoring to optimized branches..."
git -C "$RUSSH_DIR" checkout write-path-refactor --quiet
git -C "$RUSSH_SFTP_DIR" checkout zero-copy-serialize --quiet
cd "$SFTP_DIR"
RUSTFLAGS="-C target-cpu=native" cargo build --release -q 2>&1
echo "Done."
