#!/bin/bash

# Script to generate SSH Ed25519 host key for SFTP container

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
KEYS_DIR="$PROJECT_ROOT/keys"

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Create keys directory if it doesn't exist
mkdir -p "$KEYS_DIR"

KEY_FILE="$KEYS_DIR/ssh_host_ed25519_key"
PUB_FILE="$KEYS_DIR/ssh_host_ed25519_key.pub"

if [ -f "$KEY_FILE" ]; then
    echo -e "${YELLOW}Host key already exists at $KEY_FILE${NC}"
    echo "Fingerprint:"
    ssh-keygen -l -f "$KEY_FILE"
    exit 0
fi

echo "Generating SSH Ed25519 host key..."
ssh-keygen -t ed25519 -f "$KEY_FILE" -N "" -q -C "sftp-s3-server"

# Set proper permissions
chmod 600 "$KEY_FILE"
chmod 644 "$PUB_FILE"

echo -e "${GREEN}Host key generated successfully!${NC}"
echo "Private key: $KEY_FILE (chmod 600)"
echo "Public key: $PUB_FILE (chmod 644)"
echo ""
echo "Fingerprint:"
ssh-keygen -l -f "$KEY_FILE"
echo ""
echo "Key type: Ed25519"
