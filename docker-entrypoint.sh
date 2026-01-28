#!/bin/bash
set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Configuration defaults
BACKEND="${BACKEND:-memory}"
PORT="${PORT:-2222}"
SFTP_USERS="${SFTP_USERS:-user:pass}"
RUST_LOG="${RUST_LOG:-sftp_s3=info}"

# Host key handling
HOST_KEY_FILE="${HOST_KEY_FILE:-/keys/ssh_host_ed25519_key}"
AUTHORIZED_KEYS_FILE="${AUTHORIZED_KEYS_FILE:-/config/authorized_keys}"

# Prepare arguments
ARGS="--backend=$BACKEND --port=$PORT"

# Add users
if [ -n "$SFTP_USERS" ]; then
    ARGS="$ARGS --users=$SFTP_USERS"
fi

# Handle host key
if [ -f "$HOST_KEY_FILE" ]; then
    ARGS="$ARGS --host-key=$HOST_KEY_FILE"
elif [ -n "$HOST_KEY" ]; then
    # If HOST_KEY env var is set, write it to temp file
    TEMP_KEY=$(mktemp)
    trap "rm -f $TEMP_KEY" EXIT
    echo "$HOST_KEY" > "$TEMP_KEY"
    chmod 600 "$TEMP_KEY"
    ARGS="$ARGS --host-key=$TEMP_KEY"
else
    # Generate temporary key if none provided
    TEMP_KEY=$(mktemp)
    trap "rm -f $TEMP_KEY" EXIT
    ssh-keygen -t ed25519 -f "$TEMP_KEY" -N "" -q
    ARGS="$ARGS --host-key=$TEMP_KEY"
    echo -e "${YELLOW}Warning: Using generated temporary host key. SSH clients may warn about changed keys.${NC}"
    echo -e "${YELLOW}To avoid this, mount a persistent key at /keys/ssh_host_ed25519_key${NC}"
fi

# Handle authorized keys
if [ -f "$AUTHORIZED_KEYS_FILE" ]; then
    ARGS="$ARGS --authorized-keys=$AUTHORIZED_KEYS_FILE"
elif [ -n "$AUTHORIZED_KEYS" ]; then
    TEMP_KEYS=$(mktemp)
    trap "rm -f $TEMP_KEYS" EXIT
    echo "$AUTHORIZED_KEYS" > "$TEMP_KEYS"
    ARGS="$ARGS --authorized-keys=$TEMP_KEYS"
fi

# Backend-specific configuration
case "$BACKEND" in
    memory)
        echo -e "${GREEN}Starting SFTP server with memory backend${NC}"
        ;;
    local)
        if [ -z "$LOCAL_ROOT" ]; then
            echo -e "${RED}Error: LOCAL_ROOT must be set for local backend${NC}"
            exit 1
        fi
        ARGS="$ARGS --local-root=$LOCAL_ROOT"
        echo -e "${GREEN}Starting SFTP server with local backend (root: $LOCAL_ROOT)${NC}"
        ;;
    s3)
        if [ -z "$S3_BUCKET" ]; then
            echo -e "${RED}Error: S3_BUCKET must be set for S3 backend${NC}"
            exit 1
        fi
        ARGS="$ARGS --s3-bucket=$S3_BUCKET"
        [ -n "$S3_PREFIX" ] && ARGS="$ARGS --s3-prefix=$S3_PREFIX"
        [ -n "$S3_ENDPOINT" ] && ARGS="$ARGS --s3-endpoint=$S3_ENDPOINT"
        echo -e "${GREEN}Starting SFTP server with S3 backend (bucket: $S3_BUCKET)${NC}"
        ;;
    *)
        echo -e "${RED}Error: Unknown backend '$BACKEND'. Use: memory, local, or s3${NC}"
        exit 1
        ;;
esac

echo "Configuration:"
echo "  Backend: $BACKEND"
echo "  Port: $PORT"
echo "  Users: $(echo "$SFTP_USERS" | cut -d: -f1)"
echo ""

# Export logging
export RUST_LOG="$RUST_LOG"

# Execute the SFTP server
exec sftp-s3 $ARGS
