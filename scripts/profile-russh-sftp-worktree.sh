#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUSSH_REPO="${RUSSH_REPO:-$ROOT_DIR/../russh}"
RUSSH_SFTP_REPO="${RUSSH_SFTP_REPO:-$ROOT_DIR/../russh-sftp}"
SFTP_S3_REPO="${SFTP_S3_REPO:-$ROOT_DIR}"
RUSSH_REF="${RUSSH_REF:-fix-openssh-rekey}"
SFTP_S3_REF="${SFTP_S3_REF:-main}"
BASELINE_REF="${BASELINE_REF:-upstream/master}"
CANDIDATE_REF="${CANDIDATE_REF:-HEAD}"
BASELINE_FEATURES="${BASELINE_FEATURES:-sftp-master}"
CANDIDATE_FEATURES="${CANDIDATE_FEATURES:-}"
ROUNDS="${ROUNDS:-4}"
SIZE_MB="${SIZE_MB:-256}"
OUT_DIR="${OUT_DIR:-$ROOT_DIR/benchmark_results/russh-sftp-profile}"
PERF_MMAP_PAGES="${PERF_MMAP_PAGES:-64}"

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "missing required command: $1" >&2
        exit 1
    }
}

for cmd in git cargo perf nc sshpass inferno-collapse-perf inferno-flamegraph mktemp perl dd awk rg; do
    need_cmd "$cmd"
done

if [[ ! -d "$RUSSH_SFTP_REPO" ]]; then
    echo "set RUSSH_SFTP_REPO to a local russh-sftp checkout (default tried: $RUSSH_SFTP_REPO)" >&2
    exit 1
fi

if [[ ! -d "$RUSSH_REPO" ]]; then
    echo "set RUSSH_REPO to a local russh checkout (default tried: $RUSSH_REPO)" >&2
    exit 1
fi

mkdir -p "$OUT_DIR"
TMP_ROOT="$(mktemp -d /tmp/russh-sftp-profile.XXXXXX)"
trap 'git -C "$RUSSH_SFTP_REPO" worktree remove --force "$TMP_ROOT/russh-sftp-baseline" 2>/dev/null || true; \
      git -C "$RUSSH_SFTP_REPO" worktree remove --force "$TMP_ROOT/russh-sftp-candidate" 2>/dev/null || true; \
      git -C "$RUSSH_REPO" worktree remove --force "$TMP_ROOT/russh-baseline" 2>/dev/null || true; \
      git -C "$RUSSH_REPO" worktree remove --force "$TMP_ROOT/russh-candidate" 2>/dev/null || true; \
      git -C "$SFTP_S3_REPO" worktree remove --force "$TMP_ROOT/sftp-s3-baseline" 2>/dev/null || true; \
      git -C "$SFTP_S3_REPO" worktree remove --force "$TMP_ROOT/sftp-s3-candidate" 2>/dev/null || true; \
      rm -rf "$TMP_ROOT"' EXIT

pick_free_port() {
    local port
    while true; do
        port=$(( RANDOM % 16384 + 49152 ))
        nc -z 127.0.0.1 "$port" >/dev/null 2>&1 || {
            echo "$port"
            return
        }
    done
}

prepare_worktree_pair() {
    local label="$1"
    local sftp_ref="$2"
    local sftp_features="$3"
    local russh_wt="$TMP_ROOT/russh-$label"
    local sftp_wt="$TMP_ROOT/russh-sftp-$label"
    local s3_wt="$TMP_ROOT/sftp-s3-$label"

    git -C "$RUSSH_REPO" worktree add --detach "$russh_wt" "$RUSSH_REF" >/dev/null
    git -C "$RUSSH_SFTP_REPO" worktree add --detach "$sftp_wt" "$sftp_ref" >/dev/null
    git -C "$SFTP_S3_REPO" worktree add --detach "$s3_wt" "$SFTP_S3_REF" >/dev/null

    perl -0pi -e "s|^russh = .*|russh = { path = \"$russh_wt/russh\", default-features = false, features = [\"aws-lc-rs\", \"flate2\"] }|mg" "$s3_wt/Cargo.toml"
    perl -0pi -e "s|^russh-sftp = .*|russh-sftp = { path = \"$sftp_wt\" }|mg" "$s3_wt/Cargo.toml"
    perl -0pi -e 's/Ok\(\s*Handle\s*\{\s*id\s*,\s*handle\s*\}\s*\)/Ok(Handle { id, handle: handle.into() })/g' \
        "$s3_wt/src/sftp_handler.rs"

    {
        echo "label=$label"
        echo "russh_ref=$RUSSH_REF"
        echo "russh_sftp_ref=$sftp_ref"
        echo "sftp_s3_ref=$SFTP_S3_REF"
        echo "features=${sftp_features:-<none>}"
        echo "russh_worktree=$russh_wt"
        echo "russh_sftp_worktree=$sftp_wt"
        echo "sftp_s3_worktree=$s3_wt"
    } > "$OUT_DIR/$label.meta"
}

run_profile() {
    local label="$1"
    local features="$2"
    local s3_wt="$TMP_ROOT/sftp-s3-$label"
    local target_dir="$TMP_ROOT/target-$label"
    local port
    local perf_pid
    local testfile="$OUT_DIR/testfile_${SIZE_MB}mb.bin"
    local perf_data="$OUT_DIR/$label.perf.data"
    local folded="$OUT_DIR/$label.perf-folded.txt"
    local flamegraph="$OUT_DIR/$label.flamegraph.svg"
    local leafs="$OUT_DIR/$label.top-leaf.txt"
    local log="$OUT_DIR/$label.profile.log"
    local binary="$target_dir/profiling/sftp-s3"
    local -a cargo_args=(cargo build --profile profiling)

    port="$(pick_free_port)"

    if [[ ! -f "$testfile" ]]; then
        dd if=/dev/zero of="$testfile" bs=1M count="$SIZE_MB" status=none
    fi

    (
        cd "$s3_wt"
        cargo update -p russh --quiet || true
        cargo update -p russh-sftp --quiet || true
        if [[ -n "$features" ]]; then
            cargo_args+=(--features "$features")
        fi
        CARGO_TARGET_DIR="$target_dir" \
        RUSTFLAGS="-C target-cpu=native -C force-frame-pointers=yes" \
        "${cargo_args[@]}" >/dev/null
    )

    perf record -F 997 -m "$PERF_MMAP_PAGES" -g --call-graph fp -o "$perf_data" \
        "$binary" --backend memory --port "$port" --user "benchmark:benchmark" >"$log" 2>&1 &
    perf_pid=$!

    for _ in $(seq 1 50); do
        if nc -z 127.0.0.1 "$port" >/dev/null 2>&1; then
            break
        fi
        if ! kill -0 "$perf_pid" >/dev/null 2>&1; then
            echo "profiled server exited early for $label" >&2
            cat "$log" >&2 || true
            exit 1
        fi
        sleep 0.2
    done

    for round in $(seq 1 "$ROUNDS"); do
        local remote_file
        remote_file="$(basename "$testfile")"
        sshpass -p benchmark sftp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
            -P "$port" "benchmark@127.0.0.1" <<EOF >/dev/null
put $testfile
get $remote_file $OUT_DIR/${label}.${round}.downloaded
bye
EOF
        rm -f "$OUT_DIR/${label}.${round}.downloaded"
    done

    kill -INT "$perf_pid"
    wait "$perf_pid" 2>/dev/null || true

    perf script -i "$perf_data" 2>/dev/null | inferno-collapse-perf > "$folded"
    inferno-flamegraph < "$folded" > "$flamegraph"
    awk '{n=split($0,a,";"); weight=$NF; leaf=a[n]; gsub(/ [0-9]+$/,"",leaf); counts[leaf]+=weight} END{for(f in counts) print counts[f], f}' \
        "$folded" | sort -rn | sed -n '1,30p' > "$leafs"

    {
        echo "== $label =="
        echo "port=$port"
        echo "features=${features:-<none>}"
        echo "perf_data=$perf_data"
        echo "folded=$folded"
        echo "flamegraph=$flamegraph"
        echo "top_leaf=$leafs"
        echo
        rg "Packet::try_from|::from_bytes|Name::from_bytes|Status::from_bytes|Handle::from_bytes|Open::from_bytes" "$folded" | sed -n '1,40p' || true
    } > "$OUT_DIR/$label.summary"
}

prepare_worktree_pair baseline "$BASELINE_REF" "$BASELINE_FEATURES"
prepare_worktree_pair candidate "$CANDIDATE_REF" "$CANDIDATE_FEATURES"

run_profile baseline "$BASELINE_FEATURES"
run_profile candidate "$CANDIDATE_FEATURES"

echo "profiles written to $OUT_DIR"
