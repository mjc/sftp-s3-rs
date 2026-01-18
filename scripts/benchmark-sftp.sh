#!/usr/bin/env bash
# Benchmark SFTP transfer performance
# Usage: ./scripts/benchmark-sftp.sh [size_mb]

set -e

SIZE_MB=${1:-1024}  # Default 1GB
PORT=2223
USER="benchmark"
PASS="benchmark"
TESTFILE="testfile_${SIZE_MB}mb.bin"

echo "=== SFTP Benchmark: ${SIZE_MB}MB file ==="
echo ""

# Check if server is already running
if lsof -i :$PORT >/dev/null 2>&1; then
    echo "Error: Port $PORT is already in use"
    exit 1
fi

# Create test data
echo "Creating ${SIZE_MB}MB test file..."
dd if=/dev/urandom of="/tmp/$TESTFILE" bs=1M count=$SIZE_MB status=progress 2>&1 | tail -1

# Start the memory server in background
echo ""
echo "Starting SFTP server on port $PORT..."
cargo run --release --bin sftp-s3 -- --port $PORT --user "$USER:$PASS" memory &
SERVER_PID=$!

# Wait for server to start (may need time to compile on first run)
echo "Waiting for server to start..."
for i in {1..60}; do
    if nc -z localhost $PORT 2>/dev/null; then
        echo "Server is ready!"
        break
    fi
    sleep 1
done

# Verify server is running
if ! kill -0 $SERVER_PID 2>/dev/null; then
    echo "Error: Server failed to start"
    exit 1
fi

cleanup() {
    echo ""
    echo "Cleaning up..."
    kill $SERVER_PID 2>/dev/null || true
    rm -f "/tmp/$TESTFILE" "/tmp/${TESTFILE}.downloaded"
}
trap cleanup EXIT

# Run upload benchmark
echo ""
echo "=== Upload Benchmark ==="
UPLOAD_START=$(date +%s.%N)
sshpass -p "$PASS" sftp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -P $PORT "$USER@localhost" <<EOF
put /tmp/$TESTFILE
bye
EOF
UPLOAD_END=$(date +%s.%N)
UPLOAD_TIME=$(echo "$UPLOAD_END - $UPLOAD_START" | bc)
UPLOAD_SPEED=$(echo "scale=2; $SIZE_MB / $UPLOAD_TIME" | bc)
echo "Upload: ${SIZE_MB}MB in ${UPLOAD_TIME}s = ${UPLOAD_SPEED} MB/s"

# Run download benchmark
echo ""
echo "=== Download Benchmark ==="
DOWNLOAD_START=$(date +%s.%N)
sshpass -p "$PASS" sftp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -P $PORT "$USER@localhost" <<EOF
get $TESTFILE /tmp/${TESTFILE}.downloaded
bye
EOF
DOWNLOAD_END=$(date +%s.%N)
DOWNLOAD_TIME=$(echo "$DOWNLOAD_END - $DOWNLOAD_START" | bc)
DOWNLOAD_SPEED=$(echo "scale=2; $SIZE_MB / $DOWNLOAD_TIME" | bc)
echo "Download: ${SIZE_MB}MB in ${DOWNLOAD_TIME}s = ${DOWNLOAD_SPEED} MB/s"

# Summary
echo ""
echo "=== Summary ==="
echo "File size: ${SIZE_MB}MB"
echo "Upload:    ${UPLOAD_SPEED} MB/s (${UPLOAD_TIME}s)"
echo "Download:  ${DOWNLOAD_SPEED} MB/s (${DOWNLOAD_TIME}s)"
TOTAL_TIME=$(echo "$UPLOAD_TIME + $DOWNLOAD_TIME" | bc)
ROUNDTRIP_SPEED=$(echo "scale=2; ($SIZE_MB * 2) / $TOTAL_TIME" | bc)
echo "Roundtrip: ${ROUNDTRIP_SPEED} MB/s"
