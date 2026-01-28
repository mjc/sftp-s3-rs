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

# Track temp files for cleanup
TEMP_FILES=()
cleanup() {
    for f in "${TEMP_FILES[@]}"; do
        rm -f "$f"
    done
}
trap cleanup EXIT

# Prepare arguments as array to handle spaces properly
ARGS=("--backend=$BACKEND" "--port=$PORT")

# Add users (required unless using authorized_keys for authentication)
if [ -n "$SFTP_USERS" ]; then
    ARGS+=("--user=$SFTP_USERS")
elif [ ! -f "$AUTHORIZED_KEYS_FILE" ] && [ -z "$AUTHORIZED_KEYS" ]; then
    echo -e "${RED}Error: Either SFTP_USERS or authorized_keys must be configured.${NC}"
    echo -e "${RED}Set SFTP_USERS environment variable or mount an authorized_keys file.${NC}"
    exit 1
fi

# Handle host key
if [ -f "$HOST_KEY_FILE" ]; then
    ARGS+=("--host-key-file=$HOST_KEY_FILE")
elif [ -n "$HOST_KEY" ]; then
    # If HOST_KEY env var is set, write it to temp file
    # Use printf to preserve backslashes in key content
    TEMP_KEY=$(mktemp)
    TEMP_FILES+=("$TEMP_KEY")
    printf '%s\n' "$HOST_KEY" > "$TEMP_KEY"
    chmod 600 "$TEMP_KEY"
    ARGS+=("--host-key-file=$TEMP_KEY")
else
    # Generate temporary key if none provided
    if command -v ssh-keygen >/dev/null 2>&1; then
        TEMP_KEY=$(mktemp -u)  # -u creates name only, doesn't create file
        TEMP_FILES+=("$TEMP_KEY" "${TEMP_KEY}.pub")
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
    # Use printf to preserve backslashes in key content
    TEMP_KEYS=$(mktemp)
    TEMP_FILES+=("$TEMP_KEYS")
    printf '%s\n' "$AUTHORIZED_KEYS" > "$TEMP_KEYS"
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
if [ -n "$SFTP_USERS" ]; then
    echo "  Auth: password (user: $(printf '%s' "$SFTP_USERS" | cut -d: -f1))"
else
    echo "  Auth: public key only"
fi
echo ""

# Export logging
export RUST_LOG="$RUST_LOG"

# Execute the SFTP server with proper argument expansion
exec sftp-s3 "${ARGS[@]}"
