#!/usr/bin/env bash
set -euo pipefail

# Latency-focused profiling for small file transfers
# Unlike flamegraph profiling, this shows WHERE TIME IS SPENT WAITING
#
# Outputs:
#   - strace.log: syscall timing data
#   - server.log: application tracing with span durations
#   - flamegraph-offcpu.svg: off-CPU flamegraph (what the server waits on)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

MODE="${1:-strace}"
shift || true
RESULTS_DIR="${RESULTS_DIR:-"$PROJECT_DIR/benchmark_results/profile-${MODE}-$(date +%Y%m%d-%H%M%S)"}"
PORT="${PORT:-2223}"
PROFILE_BACKEND="${PROFILE_BACKEND:-memory}"
PROFILE_ROOT="${PROFILE_ROOT:-.}"

mkdir -p "$RESULTS_DIR"

cleanup_server() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
}

cleanup_bpftrace() {
    if [[ -n "${BPFTRACE_PID:-}" ]]; then
        local children
        children="$(pgrep -P "$BPFTRACE_PID" 2>/dev/null || true)"
        if [[ -n "$children" ]]; then
            sudo kill -INT $children 2>/dev/null || kill -INT $children 2>/dev/null || true
        fi
        kill -INT "$BPFTRACE_PID" 2>/dev/null || true
        wait "$BPFTRACE_PID" 2>/dev/null || true
        BPFTRACE_PID=
    fi
}

cleanup() {
    cleanup_bpftrace
    cleanup_server
}

trap 'echo ""; echo "Stopping profiler, generating reports..."; cleanup_bpftrace' INT
trap cleanup EXIT

echo "=== Latency Profile Mode: $MODE ==="
echo "Results: $RESULTS_DIR"

# Fix perf permissions
echo 0 | sudo tee /proc/sys/kernel/kptr_restrict > /dev/null
echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid > /dev/null
# Enable kernel tracing access for sched:sched_switch tracepoint
sudo chmod -R a+rx /sys/kernel/tracing 2>/dev/null || true
sudo chmod -R a+rx /sys/kernel/debug/tracing 2>/dev/null || true

# Build with profiling profile
RUSTFLAGS="-C target-cpu=native -C force-frame-pointers=yes" cargo build --profile profiling

# Build span-stats tool if needed
if [ ! -x "$SCRIPT_DIR/span-stats" ] || [ "$SCRIPT_DIR/span-stats.rs" -nt "$SCRIPT_DIR/span-stats" ]; then
    echo "Building span-stats..."
    rustc -O "$SCRIPT_DIR/span-stats.rs" -o "$SCRIPT_DIR/span-stats"
fi

if [ ! -x "$SCRIPT_DIR/collapse-bpftrace-offcpu" ] || [ "$SCRIPT_DIR/collapse-bpftrace-offcpu.rs" -nt "$SCRIPT_DIR/collapse-bpftrace-offcpu" ]; then
    echo "Building collapse-bpftrace-offcpu..."
    rustc -O "$SCRIPT_DIR/collapse-bpftrace-offcpu.rs" -o "$SCRIPT_DIR/collapse-bpftrace-offcpu"
fi

# Enable debug logging to capture span timings
export RUST_LOG="${RUST_LOG:-sftp_s3=debug}"

SERVER_CMD=(
    "$PROJECT_DIR/target/profiling/sftp-s3"
    --backend "$PROFILE_BACKEND"
    --port "$PORT"
    --root "$PROFILE_ROOT"
)

# Function to generate stats from server.log using Rust binary
generate_span_stats() {
    if [ ! -f "$RESULTS_DIR/server.log" ]; then
        return
    fi

    if [ -x "$SCRIPT_DIR/span-stats" ]; then
        echo ""
        "$SCRIPT_DIR/span-stats" < "$RESULTS_DIR/server.log"
    else
        echo "(span-stats not found)"
    fi
}

wait_for_server() {
    local ready=0
    for _ in {1..100}; do
        if nc -z localhost "$PORT" >/dev/null 2>&1; then
            ready=1
            break
        fi
        if [[ -n "${SERVER_PID:-}" ]] && ! kill -0 "$SERVER_PID" 2>/dev/null; then
            echo "Server exited during startup; log follows:" >&2
            sed -n '1,120p' "$RESULTS_DIR/server.log" >&2
            exit 1
        fi
        sleep 0.1
    done

    if [[ "$ready" != 1 ]]; then
        echo "Server did not become ready on port $PORT" >&2
        exit 1
    fi
}

case "$MODE" in
  strace)
    echo ""
    echo "Recording syscall latency with strace..."
    echo "Server debug output -> server.log"
    echo "Press Ctrl-C to stop and generate report"
    echo ""

    # -T: show time spent in syscall
    # -f: follow forks
    # -tt: microsecond timestamps
    # -o: output file (strace output goes here, not console)
    echo -ne "\033]0;sftp-s3 READY (strace)\007"
    echo -ne "\033ksftp-s3 READY\033\\"
    strace -T -f -tt -e read,write,recvfrom,sendto,poll,epoll_wait,accept4,close \
      -o "$RESULTS_DIR/strace.log" \
      "${SERVER_CMD[@]}" "$@" > "$RESULTS_DIR/server.log" 2>&1 || true

    echo ""
    echo "=== Syscall Summary ==="
    echo ""

    # Summarize syscall times
    echo "Top syscalls by total time:"
    grep -oP '<[0-9.]+>' "$RESULTS_DIR/strace.log" | tr -d '<>' | \
      awk '{sum+=$1; count++} END {printf "Total: %.3fs across %d calls (avg %.3fms)\n", sum, count, (sum/count)*1000}'

    echo ""
    echo "Breakdown by syscall type:"
    for syscall in read write recvfrom sendto poll epoll_wait accept4; do
      if grep -q "^[0-9].*$syscall(" "$RESULTS_DIR/strace.log" 2>/dev/null; then
        grep "$syscall(" "$RESULTS_DIR/strace.log" | grep -oP '<[0-9.]+>' | tr -d '<>' | \
          awk -v name="$syscall" '{sum+=$1; count++} END {if(count>0) printf "  %-12s: %.3fs total, %6d calls, avg %.3fms\n", name, sum, count, (sum/count)*1000}'
      fi
    done

    echo ""
    echo "Slowest individual syscalls (>1ms):"
    grep -oP '^[0-9]+\s+[0-9:.]+\s+\S+\(.*<[0-9.]+>' "$RESULTS_DIR/strace.log" | \
      awk -F'<' '{time=$2; gsub(/>.*/, "", time); if(time+0 > 0.001) print time, $1}' | \
      sort -rn | head -20

    generate_span_stats

    echo ""
    echo "Full logs: $RESULTS_DIR/strace.log, $RESULTS_DIR/server.log"
    ;;

  offcpu)
    echo ""
    echo "Recording off-CPU time (what the server waits on)..."
    echo "Server debug output -> server.log"
    echo "Press Ctrl-C to stop and generate flamegraph"
    echo ""

    # Try different methods for off-CPU profiling
    OFFCPU_METHOD=""

    # Method 1: bpftrace sched_switch with perf-style user stacks. This is the
    # real off-CPU path; profile:hz samples CPU time and produces misleading
    # client-heavy flamegraphs.
    if command -v bpftrace &> /dev/null &&
       [[ -x "$SCRIPT_DIR/collapse-bpftrace-offcpu" ]] &&
       command -v inferno-flamegraph &> /dev/null; then
      echo "Using bpftrace for off-CPU profiling..."
      OFFCPU_METHOD="bpftrace"
    # Method 2: Try perf with sched tracepoint
    elif perf record -e sched:sched_switch -a -- sleep 0.01 2>/dev/null; then
      rm -f perf.data
      echo "Using perf sched:sched_switch..."
      OFFCPU_METHOD="perf-sched"
    # Method 3: Try perf with software event (less accurate but works without tracepoints)
    elif command -v inferno-collapse-perf &>/dev/null &&
         perf record -e cpu-clock -a -- sleep 0.01 2>/dev/null; then
      rm -f perf.data
      echo "Using perf cpu-clock (less accurate, shows on-CPU not off-CPU)..."
      echo "Note: This won't show true off-CPU time, just CPU sampling"
      OFFCPU_METHOD="perf-cpu"
    else
      echo "Error: No off-CPU profiling method available"
      echo ""
      echo "Try installing bpftrace, or fix perf permissions:"
      echo "  sudo sh -c 'echo 0 > /proc/sys/kernel/perf_event_paranoid'"
      echo "  sudo chmod -R a+rx /sys/kernel/tracing"
      exit 1
    fi

    set +e
    case "$OFFCPU_METHOD" in
      bpftrace)
        # Run server in background with output to file
        echo -ne "\033]0;sftp-s3 READY (offcpu-bpftrace)\007"
        echo -ne "\033ksftp-s3 READY\033\\"
        "${SERVER_CMD[@]}" "$@" > "$RESULTS_DIR/server.log" 2>&1 &
        SERVER_PID=$!
        wait_for_server

        cat > "$RESULTS_DIR/offcpu.bt" <<'BPFTRACE'
BEGIN
{
  printf("Tracing off-CPU waits for target process. Press Ctrl-C to stop.\n");
}

tracepoint:sched:sched_switch
/pid == TARGET_PID && args->prev_state != 0/
{
  $tid = (uint64)tid;
  @start[$tid] = nsecs;
  @stack[$tid] = ustack(perf);
}

tracepoint:sched:sched_switch
/@start[(uint64)args->next_pid]/
{
  $tid = (uint64)args->next_pid;
  @offcpu[@stack[$tid]] = sum((nsecs - @start[$tid]) / 1000);
  delete(@start[$tid]);
  delete(@stack[$tid]);
}

END
{
  print(@offcpu);
  clear(@start);
  clear(@stack);
}
BPFTRACE

        sed -i "s/TARGET_PID/$SERVER_PID/g" "$RESULTS_DIR/offcpu.bt"

        # Keep symbol resolution per process. Without this bpftrace often emits
        # mostly raw or unknown user frames for optimized Rust binaries.
        export BPFTRACE_CACHE_USER_SYMBOLS="${BPFTRACE_CACHE_USER_SYMBOLS:-PER_PID}"
        export BPFTRACE_MAX_MAP_KEYS="${BPFTRACE_MAX_MAP_KEYS:-65536}"

        # Record off-CPU stacks. The values are microseconds, so the flamegraph
        # width represents time spent blocked, not sample count.
        echo "Recording... (press Ctrl-C to stop)"
        BPFTRACE_BIN=$(command -v bpftrace)
        sudo env \
          BPFTRACE_CACHE_USER_SYMBOLS="$BPFTRACE_CACHE_USER_SYMBOLS" \
          BPFTRACE_MAX_MAP_KEYS="$BPFTRACE_MAX_MAP_KEYS" \
          "$BPFTRACE_BIN" "$RESULTS_DIR/offcpu.bt" \
          > "$RESULTS_DIR/offcpu-stacks.txt" \
          2> "$RESULTS_DIR/offcpu-bpftrace.log" &
        BPFTRACE_PID=$!
        wait "$BPFTRACE_PID" || true
        BPFTRACE_PID=

        cleanup_server

        # Convert to flamegraph format
        if grep -q '^@offcpu\[' "$RESULTS_DIR/offcpu-stacks.txt"; then
          "$SCRIPT_DIR/collapse-bpftrace-offcpu" \
            < "$RESULTS_DIR/offcpu-stacks.txt" \
            > "$RESULTS_DIR/offcpu-folded.txt"
          if [ ! -s "$RESULTS_DIR/offcpu-folded.txt" ]; then
            echo "Error: bpftrace produced output, but no folded stacks were generated" >&2
            sed -n '1,120p' "$RESULTS_DIR/offcpu-stacks.txt" >&2
            sed -n '1,120p' "$RESULTS_DIR/offcpu-bpftrace.log" >&2
            exit 1
          fi
          inferno-flamegraph \
            --title "sftp-s3 Off-CPU Time" \
            --countname us \
            "$RESULTS_DIR/offcpu-folded.txt" > "$RESULTS_DIR/flamegraph-offcpu.svg"
        else
          echo "Error: bpftrace did not capture any off-CPU stacks" >&2
          echo "Run a client workload while the profiler is recording, then press Ctrl-C." >&2
          sed -n '1,120p' "$RESULTS_DIR/offcpu-stacks.txt" >&2
          sed -n '1,120p' "$RESULTS_DIR/offcpu-bpftrace.log" >&2
          exit 1
        fi
        ;;

      perf-sched)
        # Use perf sched for proper scheduler analysis
        echo -ne "\033]0;sftp-s3 READY (offcpu-sched)\007"
        echo -ne "\033ksftp-s3 READY\033\\"
        perf sched record -o "$RESULTS_DIR/perf-offcpu.data" \
          -- "${SERVER_CMD[@]}" "$@" > "$RESULTS_DIR/server.log" 2>&1 || true
        ;;

      perf-cpu)
        echo -ne "\033]0;sftp-s3 READY (offcpu-cpu)\007"
        echo -ne "\033ksftp-s3 READY\033\\"
        perf record -e cpu-clock -g --call-graph fp -F 997 \
          -o "$RESULTS_DIR/perf-offcpu.data" \
          -- "${SERVER_CMD[@]}" "$@" > "$RESULTS_DIR/server.log" 2>&1 || true
        ;;
    esac
    set -e

    echo ""
    echo "Generating reports..."

    if [ -f "$RESULTS_DIR/perf-offcpu.data" ]; then
      if [ "$OFFCPU_METHOD" = "perf-sched" ]; then
        # perf sched gives latency report, not flamegraph
        echo ""
        echo "=== Scheduler Latency Report ==="
        perf sched latency -i "$RESULTS_DIR/perf-offcpu.data" 2>/dev/null | head -50
        echo ""
        echo "Full report: perf sched latency -i $RESULTS_DIR/perf-offcpu.data"
      else
        # cpu-clock can make a flamegraph
        perf script -i "$RESULTS_DIR/perf-offcpu.data" 2>/dev/null | \
          inferno-collapse-perf | \
          inferno-flamegraph --title "CPU Time Profile" > "$RESULTS_DIR/flamegraph-offcpu.svg"
        echo "Done: $RESULTS_DIR/flamegraph-offcpu.svg"
      fi
    elif [ -f "$RESULTS_DIR/flamegraph-offcpu.svg" ]; then
      echo "Done: $RESULTS_DIR/flamegraph-offcpu.svg"
    else
      echo "Warning: Could not generate flamegraph/report"
    fi

    generate_span_stats

    echo ""
    echo "Full logs: $RESULTS_DIR/server.log"
    ;;

  *)
    echo "Usage: $0 [strace|offcpu] [extra sftp-s3 args...]"
    echo ""
    echo "Modes:"
    echo "  strace  - Record syscall latency (default)"
    echo "  offcpu  - Off-CPU flamegraph (tries bpftrace, then perf)"
    exit 1
    ;;
esac
