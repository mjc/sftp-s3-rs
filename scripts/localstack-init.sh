#!/bin/bash

# Script to initialize LocalStack with S3 bucket for SFTP testing

set -e

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m' # No Color

BUCKET_NAME="test-bucket"
LOCALSTACK_ENDPOINT="http://localhost:4566"

echo -e "${BLUE}=== LocalStack S3 Initialization ===${NC}"
echo ""

# Check if localstack is running
echo "Checking LocalStack health..."
if ! curl -s "$LOCALSTACK_ENDPOINT/health" > /dev/null 2>&1; then
    echo -e "${RED}Error: LocalStack is not running at $LOCALSTACK_ENDPOINT${NC}"
    echo "Start LocalStack first:"
    echo "  docker-compose up -d localstack"
    exit 1
fi

echo -e "${GREEN}LocalStack is healthy${NC}"
echo ""

# Set up AWS CLI environment
export AWS_ACCESS_KEY_ID=test
export AWS_SECRET_ACCESS_KEY=test
export AWS_DEFAULT_REGION=us-east-1

# Check if bucket already exists
echo "Checking for existing bucket..."
if aws s3 ls "s3://$BUCKET_NAME" --endpoint-url="$LOCALSTACK_ENDPOINT" > /dev/null 2>&1; then
    echo -e "${YELLOW}Bucket '$BUCKET_NAME' already exists${NC}"
else
    echo "Creating S3 bucket '$BUCKET_NAME'..."
    aws s3 mb "s3://$BUCKET_NAME" --endpoint-url="$LOCALSTACK_ENDPOINT" --region us-east-1
    echo -e "${GREEN}Bucket created successfully${NC}"
fi

echo ""

# List buckets
echo "Available buckets:"
aws s3 ls --endpoint-url="$LOCALSTACK_ENDPOINT"

echo ""
echo -e "${GREEN}LocalStack is ready!${NC}"
echo ""
echo "You can now:"
echo "1. Connect via SFTP:"
echo "   sftp -P 2225 user@localhost  # password: localstacktest"
echo ""
echo "2. Upload files through SFTP and verify in LocalStack:"
echo "   aws s3 ls s3://$BUCKET_NAME/sftp/ --endpoint-url=\"$LOCALSTACK_ENDPOINT\""
echo ""
echo "3. Clean up:"
echo "   docker-compose down -v"
