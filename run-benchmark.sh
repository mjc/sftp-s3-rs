#!/usr/bin/env bash
# Single iteration: upload + download against an ALREADY RUNNING server.
# Usage: run-benchmark.sh <size_mb> <testfile_path>
# Server must already be listening on port 2223.

SIZE_MB=$1
TESTFILE=$2
PORT=2223
USER="benchmark"
PASS="benchmark"
SFTP_DIR="/home/mjc/projects/sftp-s3-rs"
DLFILE="$SFTP_DIR/dl_tmp.bin"

mono() { awk '{print $1}' /proc/uptime; }

# Upload
UPLOAD_START=$(mono)
sshpass -p "$PASS" sftp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o Compression=no -P $PORT "$USER@localhost" <<EOF 2>>/tmp/sftp-errors.log
put $TESTFILE
bye
EOF
UPLOAD_END=$(mono)
UPLOAD_TIME=$(echo "$UPLOAD_END - $UPLOAD_START" | bc)
UPLOAD_SPEED=$(echo "scale=2; $SIZE_MB / $UPLOAD_TIME" | bc)

# Download
REMOTE_FILE="${TESTFILE##*/}"
DOWNLOAD_START=$(mono)
sshpass -p "$PASS" sftp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o Compression=no -P $PORT "$USER@localhost" <<EOF 2>>/tmp/sftp-errors.log
get $REMOTE_FILE $DLFILE
bye
EOF
DOWNLOAD_END=$(mono)
DOWNLOAD_TIME=$(echo "$DOWNLOAD_END - $DOWNLOAD_START" | bc)
DOWNLOAD_SPEED=$(echo "scale=2; $SIZE_MB / $DOWNLOAD_TIME" | bc)

rm -f "$DLFILE"

TOTAL_TIME=$(echo "$UPLOAD_TIME + $DOWNLOAD_TIME" | bc)
ROUNDTRIP_SPEED=$(echo "scale=2; ($SIZE_MB * 2) / $TOTAL_TIME" | bc)

echo "Upload: ${UPLOAD_SPEED} MB/s"
echo "Download: ${DOWNLOAD_SPEED} MB/s"
echo "Roundtrip: ${ROUNDTRIP_SPEED} MB/s"
