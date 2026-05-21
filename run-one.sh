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
SFTPOPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o Compression=no)

if [[ -n "${SFTP_IDENTITY_FILE:-}" ]]; then
    SFTP_CMD=(sftp -o BatchMode=yes -o IdentityFile="$SFTP_IDENTITY_FILE")
else
    SFTP_CMD=(sshpass -p "$PASS" sftp -o BatchMode=no -o PreferredAuthentications=password -o PubkeyAuthentication=no)
fi

# Use file from client_source_dir instead of original testfile location
FILENAME=$(basename "$TESTFILE")
CLIENT_FILE="$CLIENT_SOURCE_DIR/$FILENAME"

# Create batch files to avoid rekey issues with heredoc
BATCH_UP="/tmp/sftp_batch_up_$$.txt"
BATCH_DL="/tmp/sftp_batch_dl_$$.txt"

echo "put $CLIENT_FILE" > "$BATCH_UP"
echo "quit" >> "$BATCH_UP"

echo "get $FILENAME $DLFILE" > "$BATCH_DL"
echo "quit" >> "$BATCH_DL"

# Use 5 minute timeout for large transfers with -b batch mode
if [[ -n "${SFTP_IDENTITY_FILE:-}" ]]; then
    timeout 300 "${SFTP_CMD[@]}" -b "$BATCH_UP" "${SFTPOPTS[@]}" -P "$PORT" "$USER@localhost" >/dev/null 2>&1
else
    timeout 300 "${SFTP_CMD[@]}" "${SFTPOPTS[@]}" -P "$PORT" "$USER@localhost" < "$BATCH_UP" >/dev/null 2>&1
fi
[[ $? -eq 0 ]] || { rm -f "$BATCH_UP" "$BATCH_DL"; exit 1; }

if [[ -n "${SFTP_IDENTITY_FILE:-}" ]]; then
    timeout 300 "${SFTP_CMD[@]}" -b "$BATCH_DL" "${SFTPOPTS[@]}" -P "$PORT" "$USER@localhost" >/dev/null 2>&1
else
    timeout 300 "${SFTP_CMD[@]}" "${SFTPOPTS[@]}" -P "$PORT" "$USER@localhost" < "$BATCH_DL" >/dev/null 2>&1
fi
[[ $? -eq 0 ]] || { rm -f "$BATCH_UP" "$BATCH_DL"; exit 1; }

rm -f "$DLFILE" "$BATCH_UP" "$BATCH_DL"
