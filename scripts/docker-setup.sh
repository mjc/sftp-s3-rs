#!/bin/bash

# Script to set up Docker environment for SFTP-S3 server

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${BLUE}=== SFTP-S3 Docker Setup ===${NC}"
echo ""

# Create necessary directories
echo "Creating directories..."
mkdir -p "$PROJECT_ROOT/keys"
mkdir -p "$PROJECT_ROOT/config"

# Generate host key if it doesn't exist
if [ ! -f "$PROJECT_ROOT/keys/ssh_host_ed25519_key" ]; then
    echo "Generating SSH host key..."
    bash "$SCRIPT_DIR/generate-host-key.sh"
else
    echo -e "${GREEN}Host key already exists${NC}"
fi

echo ""

# Create example .env file if it doesn't exist
ENV_FILE="$PROJECT_ROOT/.env"
if [ ! -f "$ENV_FILE" ]; then
    echo "Creating example .env file..."
    cat > "$ENV_FILE" << 'EOF'
# SFTP User credentials (REQUIRED for sftp-s3 service only)
# Format: username:password
# IMPORTANT: Change this to a strong password before production use!
SFTP_USERS=user:change-me-to-strong-password

# AWS S3 Configuration (REQUIRED for sftp-s3 service only)
# Replace with your actual bucket and credentials
S3_BUCKET=my-bucket
S3_PREFIX=sftp/
AWS_REGION=us-east-1
AWS_ACCESS_KEY_ID=replace-with-your-access-key
AWS_SECRET_ACCESS_KEY=replace-with-your-secret-key

# Optional: Use custom S3-compatible endpoint (LocalStack, MinIO, etc.)
# Uncomment and set if using non-AWS S3:
# S3_ENDPOINT=http://localstack:4566
EOF
    echo -e "${GREEN}Created $ENV_FILE${NC}"
    echo "  Note: .env is only needed for the sftp-s3 (AWS S3) service"
    echo "  Edit with your AWS credentials before using 'docker-compose up sftp-s3'"
else
    echo -e "${YELLOW}.env file already exists${NC}"
fi

echo ""

# Create example authorized_keys file if it doesn't exist
AUTH_KEYS="$PROJECT_ROOT/config/authorized_keys"
if [ ! -f "$AUTH_KEYS" ]; then
    echo "Creating example authorized_keys file..."
    cat > "$AUTH_KEYS" << 'EOF'
# Add your SSH public keys here for key-based authentication
# Example format:
# ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILZfg... user@example.com
EOF
    chmod 600 "$AUTH_KEYS"
    echo -e "${GREEN}Created $AUTH_KEYS${NC}"
    echo "  Add your SSH public keys to this file for key-based authentication"
else
    echo -e "${YELLOW}authorized_keys already exists${NC}"
fi

echo ""
echo -e "${GREEN}Setup complete!${NC}"
echo ""
echo -e "${BLUE}Next steps:${NC}"
echo "1. Start services:"
echo "   docker-compose up -d"
echo ""
echo "2. Test memory backend:"
echo "   sftp -P 2222 user@localhost  # password: changeme"
echo ""
echo "3. Test local backend:"
echo "   sftp -P 2223 user@localhost  # password: changeme"
echo "   # Files are stored in the 'sftp-data' Docker volume"
echo ""
echo "4. Test S3 with LocalStack:"
echo "   docker-compose up -d localstack sftp-s3-local"
echo "   bash $SCRIPT_DIR/localstack-init.sh"
echo "   sftp -P 2225 user@localhost  # password: localstacktest"
echo ""
echo "5. Use real AWS S3:"
echo "   Edit .env with your AWS credentials and SFTP_USERS"
echo "   docker-compose up -d sftp-s3"
echo "   sftp -P 2224 user@localhost  # uses password from .env"
echo ""
echo "6. View logs:"
echo "   docker-compose logs -f [service-name]"
echo ""
echo "7. Stop services:"
echo "   docker-compose down"
echo ""
echo "For more information, see README.md"
