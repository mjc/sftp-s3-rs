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

sshpass -p "$PASS" sftp $SFTPOPTS -P "$PORT" "$USER@localhost" 2>/dev/null <<< "put $CLIENT_FILE" || exit 1
sshpass -p "$PASS" sftp $SFTPOPTS -P "$PORT" "$USER@localhost" 2>/dev/null <<< "get $FILENAME $DLFILE" || exit 1
rm -f "$DLFILE"
