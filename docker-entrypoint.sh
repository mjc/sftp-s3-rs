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
# Note: SFTP_USERS has no default - must be explicitly set for security
RUST_LOG="${RUST_LOG:-sftp_s3=info}"

# Host key handling
HOST_KEY_FILE="${HOST_KEY_FILE:-/keys/ssh_host_ed25519_key}"
AUTHORIZED_KEYS_FILE="${AUTHORIZED_KEYS_FILE:-/config/authorized_keys}"

# Prepare arguments as array to handle spaces properly
ARGS=("--backend=$BACKEND" "--port=$PORT")

# Add users (required for security - must be explicitly set)
if [ -z "$SFTP_USERS" ]; then
    echo -e "${RED}Error: SFTP_USERS must be explicitly set. Do not use default credentials in production.${NC}"
    exit 1
fi
ARGS+=("--user=$SFTP_USERS")

# Handle host key
if [ -f "$HOST_KEY_FILE" ]; then
    ARGS+=("--host-key-file=$HOST_KEY_FILE")
elif [ -n "$HOST_KEY" ]; then
    # If HOST_KEY env var is set, write it to temp file
    TEMP_KEY=$(mktemp)
    trap "rm -f '$TEMP_KEY'" EXIT
    echo "$HOST_KEY" > "$TEMP_KEY"
    chmod 600 "$TEMP_KEY"
    ARGS+=("--host-key-file=$TEMP_KEY")
else
    # Generate temporary key if none provided
    if command -v ssh-keygen >/dev/null 2>&1; then
        TEMP_KEY=$(mktemp)
        trap "rm -f '$TEMP_KEY'" EXIT
        ssh-keygen -t ed25519 -f "$TEMP_KEY" -N "" -q
        ARGS+=("--host-key-file=$TEMP_KEY")
        echo -e "${YELLOW}Warning: Using generated temporary host key. SSH clients may warn about changed keys.${NC}"
        echo -e "${YELLOW}To avoid this, mount a persistent key at /keys/ssh_host_ed25519_key${NC}"
    else
        echo -e "${RED}Error: ssh-keygen is not available in this container image, and no host key was provided.${NC}"
        echo -e "${RED}Please either mount a host key at /keys/ssh_host_ed25519_key or set the HOST_KEY/HOST_KEY_FILE environment variable.${NC}"
        exit 1
    fi
fi

# Handle authorized keys
if [ -f "$AUTHORIZED_KEYS_FILE" ]; then
    ARGS+=("--authorized-keys-file=$AUTHORIZED_KEYS_FILE")
elif [ -n "$AUTHORIZED_KEYS" ]; then
    TEMP_KEYS=$(mktemp)
    trap "rm -f '$TEMP_KEYS'" EXIT
    echo "$AUTHORIZED_KEYS" > "$TEMP_KEYS"
    ARGS+=("--authorized-keys-file=$TEMP_KEYS")
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
        ARGS+=("--root=$LOCAL_ROOT")
        echo -e "${GREEN}Starting SFTP server with local backend (root: $LOCAL_ROOT)${NC}"
        ;;
    s3)
        if [ -z "$S3_BUCKET" ]; then
            echo -e "${RED}Error: S3_BUCKET must be set for S3 backend${NC}"
            exit 1
        fi
        ARGS+=("--bucket=$S3_BUCKET")
        [ -n "$S3_PREFIX" ] && ARGS+=("--prefix=$S3_PREFIX")
        [ -n "$S3_ENDPOINT" ] && ARGS+=("--endpoint=$S3_ENDPOINT")
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

# Execute the SFTP server with proper argument expansion
exec sftp-s3 "${ARGS[@]}"
