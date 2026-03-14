#!/usr/bin/env bash
# Single upload+download against an already-running server.
# Hyperfine measures the wall time; no timing output here.
# Usage: run-one.sh <port> <testfile_path> [client_source_dir]
# client_source_dir: where client reads file from (default: dirname of testfile_path)

PORT=$1
TESTFILE=$2
CLIENT_SOURCE_DIR=${3:-.}  # default to current dir
USER="benchmark"
PASS="benchmark"
DLFILE="/tmp/sftp-dl-${PORT}.bin"
SFTPOPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o Compression=no"

# Use file from client_source_dir instead of original testfile location
FILENAME=$(basename "$TESTFILE")
CLIENT_FILE="$CLIENT_SOURCE_DIR/$FILENAME"

# Use 5 minute timeout for large transfers, set ConnectTimeout and batch mode
timeout 300 sshpass -p "$PASS" sftp -o ConnectTimeout=5 -o BatchMode=no $SFTPOPTS -P "$PORT" "$USER@localhost" <<EOF
put $CLIENT_FILE
bye
EOF
[[ $? -eq 0 ]] || exit 1

timeout 300 sshpass -p "$PASS" sftp -o ConnectTimeout=5 -o BatchMode=no $SFTPOPTS -P "$PORT" "$USER@localhost" <<EOF
get $FILENAME $DLFILE
bye
EOF
[[ $? -eq 0 ]] || exit 1

rm -f "$DLFILE"
