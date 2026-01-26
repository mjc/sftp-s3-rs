#!/usr/bin/env bash
set -e

# Latency-focused profiling for small file transfers
# Unlike flamegraph profiling, this shows WHERE TIME IS SPENT WAITING
#
# Outputs:
#   - strace.log: syscall timing data
#   - server.log: application tracing with span durations
#   - flamegraph-offcpu.svg: off-CPU flamegraph (what we're waiting on)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

MODE="${1:-strace}"
shift || true

# Trap Ctrl-C to continue with report generation
trap 'echo ""; echo "Stopping server, generating reports..."' INT

echo "=== Latency Profile Mode: $MODE ==="

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

# Enable debug logging to capture span timings
export RUST_LOG="${RUST_LOG:-sftp_s3=debug}"

# Server command with output logging
SERVER_CMD="$PROJECT_DIR/target/profiling/sftp-s3 --backend memory --port 2223 --root ."

# Function to generate stats from server.log using Rust binary
generate_span_stats() {
    if [ ! -f server.log ]; then
        return
    fi

    SCRIPT_DIR="$(dirname "$0")"
    if [ -x "$SCRIPT_DIR/span-stats" ]; then
        echo ""
        "$SCRIPT_DIR/span-stats" < server.log
    else
        echo "(span-stats not found)"
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
      -o strace.log \
      $SERVER_CMD "$@" > server.log 2>&1 || true

    echo ""
    echo "=== Syscall Summary ==="
    echo ""

    # Summarize syscall times
    echo "Top syscalls by total time:"
    grep -oP '<[0-9.]+>' strace.log | tr -d '<>' | \
      awk '{sum+=$1; count++} END {printf "Total: %.3fs across %d calls (avg %.3fms)\n", sum, count, (sum/count)*1000}'

    echo ""
    echo "Breakdown by syscall type:"
    for syscall in read write recvfrom sendto poll epoll_wait accept4; do
      if grep -q "^[0-9].*$syscall(" strace.log 2>/dev/null; then
        grep "$syscall(" strace.log | grep -oP '<[0-9.]+>' | tr -d '<>' | \
          awk -v name="$syscall" '{sum+=$1; count++} END {if(count>0) printf "  %-12s: %.3fs total, %6d calls, avg %.3fms\n", name, sum, count, (sum/count)*1000}'
      fi
    done

    echo ""
    echo "Slowest individual syscalls (>1ms):"
    grep -oP '^[0-9]+\s+[0-9:.]+\s+\S+\(.*<[0-9.]+>' strace.log | \
      awk -F'<' '{time=$2; gsub(/>.*/, "", time); if(time+0 > 0.001) print time, $1}' | \
      sort -rn | head -20

    generate_span_stats

    echo ""
    echo "Full logs: strace.log, server.log"
    ;;

  offcpu)
    echo ""
    echo "Recording off-CPU time (what we're waiting on)..."
    echo "Server debug output -> server.log"
    echo "Press Ctrl-C to stop and generate flamegraph"
    echo ""

    # Try different methods for off-CPU profiling
    OFFCPU_METHOD=""

    # Method 1: Try bpftrace offcputime (most reliable if available)
    if command -v bpftrace &> /dev/null; then
      echo "Using bpftrace for off-CPU profiling..."
      OFFCPU_METHOD="bpftrace"
    # Method 2: Try perf with sched tracepoint
    elif perf record -e sched:sched_switch -a -- sleep 0.01 2>/dev/null; then
      rm -f perf.data
      echo "Using perf sched:sched_switch..."
      OFFCPU_METHOD="perf-sched"
    # Method 3: Try perf with software event (less accurate but works without tracepoints)
    elif perf record -e cpu-clock -a -- sleep 0.01 2>/dev/null; then
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
        $SERVER_CMD "$@" > server.log 2>&1 &
        SERVER_PID=$!
        sleep 1  # Let server start

        # Record off-CPU stacks
        echo "Recording... (press Ctrl-C to stop)"
        sudo bpftrace -e '
          profile:hz:99 /pid == '"$SERVER_PID"'/ { @[ustack] = count(); }
          END { print(@); }
        ' > offcpu-stacks.txt 2>/dev/null || true

        kill $SERVER_PID 2>/dev/null || true
        wait $SERVER_PID 2>/dev/null || true

        # Convert to flamegraph format
        if [ -s offcpu-stacks.txt ]; then
          cat offcpu-stacks.txt | inferno-collapse-guess | inferno-flamegraph --title "Off-CPU Time" > flamegraph-offcpu.svg
        fi
        ;;

      perf-sched)
        # Use perf sched for proper scheduler analysis
        echo -ne "\033]0;sftp-s3 READY (offcpu-sched)\007"
        echo -ne "\033ksftp-s3 READY\033\\"
        perf sched record -o perf-offcpu.data \
          sh -c "$SERVER_CMD $* > server.log 2>&1" || true
        ;;

      perf-cpu)
        echo -ne "\033]0;sftp-s3 READY (offcpu-cpu)\007"
        echo -ne "\033ksftp-s3 READY\033\\"
        perf record -e cpu-clock -g --call-graph fp -F 997 -o perf-offcpu.data \
          sh -c "$SERVER_CMD $* > server.log 2>&1" || true
        ;;
    esac
    set -e

    echo ""
    echo "Generating reports..."

    if [ -f perf-offcpu.data ]; then
      if [ "$OFFCPU_METHOD" = "perf-sched" ]; then
        # perf sched gives latency report, not flamegraph
        echo ""
        echo "=== Scheduler Latency Report ==="
        perf sched latency -i perf-offcpu.data 2>/dev/null | head -50
        echo ""
        echo "Full report: perf sched latency -i perf-offcpu.data"
      else
        # cpu-clock can make a flamegraph
        perf script -i perf-offcpu.data 2>/dev/null | \
          inferno-collapse-perf | \
          inferno-flamegraph --title "CPU Time Profile" > flamegraph-offcpu.svg
        echo "Done: flamegraph-offcpu.svg"
      fi
    elif [ -f flamegraph-offcpu.svg ]; then
      echo "Done: flamegraph-offcpu.svg"
    else
      echo "Warning: Could not generate flamegraph/report"
    fi

    generate_span_stats

    echo ""
    echo "Full logs: server.log"
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
