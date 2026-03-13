#!/usr/bin/env bash
# Single upload+download against an already-running server.
# Hyperfine measures the wall time; no timing output here.
# Usage: run-one.sh <port> <testfile_path>

PORT=$1
TESTFILE=$2
USER="benchmark"
PASS="benchmark"
DLFILE="/tmp/sftp-dl-${PORT}.bin"
SFTPOPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o Compression=no"

sshpass -p "$PASS" sftp -b - $SFTPOPTS -P "$PORT" "$USER@localhost" 2>/dev/null <<< "put $TESTFILE" || exit 1
sshpass -p "$PASS" sftp -b - $SFTPOPTS -P "$PORT" "$USER@localhost" 2>/dev/null <<< "get ${TESTFILE##*/} $DLFILE" || exit 1
rm -f "$DLFILE"
