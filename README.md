# sftp-s3

A pluggable SFTP server with S3 and custom backend support, written in Rust.

## Features

- SFTP server using [russh](https://github.com/Eugeny/russh)
- Pluggable backend trait for custom storage implementations
- Built-in backends:
  - **Memory** - In-memory storage for testing/development
  - **S3** - Amazon S3 or S3-compatible storage (LocalStack, MinIO)
- Password authentication
- Async/await with Tokio

## Quick Start

```rust
use sftp_s3::{Server, ServerConfig, MemoryBackend};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let backend = MemoryBackend::new();
    let config = ServerConfig::new()
        .port(2222)
        .with_generated_key();

    Server::new(backend)
        .config(config)
        .with_users(vec![("user".into(), "pass".into())])
        .run()
        .await
}
```

## S3 Backend

```rust
use sftp_s3::{Server, ServerConfig, S3Backend, S3Config};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let s3_config = S3Config::new("my-bucket")
        .with_prefix("sftp/");
    let backend = S3Backend::from_env(s3_config).await;

    Server::new(backend)
        .config(ServerConfig::new().with_generated_key())
        .with_users(vec![("user".into(), "pass".into())])
        .run()
        .await
}
```

Configure AWS credentials via environment variables:
- `AWS_ACCESS_KEY_ID`
- `AWS_SECRET_ACCESS_KEY`
- `AWS_REGION` (or `AWS_DEFAULT_REGION`)
- `AWS_ENDPOINT_URL` (for LocalStack/MinIO)

## Custom Backend

Implement the `Backend` trait for custom storage:

```rust
use sftp_s3::backend::{Backend, BackendResult, DirEntry, FileInfo};
use async_trait::async_trait;

struct MyBackend;

#[async_trait]
impl Backend for MyBackend {
    async fn list_dir(&self, path: &str) -> BackendResult<Vec<DirEntry>> {
        // Implementation
    }

    async fn file_info(&self, path: &str) -> BackendResult<FileInfo> {
        // Implementation
    }

    async fn make_dir(&self, path: &str) -> BackendResult<()> {
        // Implementation
    }

    async fn del_dir(&self, path: &str) -> BackendResult<()> {
        // Implementation
    }

    async fn delete(&self, path: &str) -> BackendResult<()> {
        // Implementation
    }

    async fn rename(&self, src: &str, dst: &str) -> BackendResult<()> {
        // Implementation
    }

    async fn read_file(&self, path: &str) -> BackendResult<Vec<u8>> {
        // Implementation
    }

    async fn write_file(&self, path: &str, content: Vec<u8>) -> BackendResult<()> {
        // Implementation
    }
}
```

## Examples

Run the memory backend example:

```bash
cargo run --example memory_server
```

Run the S3 backend example:

```bash
SFTP_BUCKET=my-bucket cargo run --example s3_server
```

Connect with an SFTP client:

```bash
sftp -P 2222 user@localhost
```

## Docker Deployment

### Quick Start

#### Memory Backend (Testing)

```bash
# Set up directories and host key
./scripts/docker-setup.sh

# Update docker-compose.yml with your desired SFTP credentials
# Edit sftp-memory service SFTP_USERS variable

# Start the memory backend
docker-compose up -d sftp-memory

# Connect
sftp -P 2222 user@localhost
```

#### Local Filesystem Backend

```bash
# Set up directories and host key (if not already done)
./scripts/docker-setup.sh

docker-compose up -d sftp-local

# Files are stored in ./data/ directory
sftp -P 2223 user@localhost  # password: changeme
```

#### AWS S3 Backend

```bash
# Set up directories and host key (if not already done)
./scripts/docker-setup.sh

# Edit .env with your AWS credentials and SFTP_USERS
nano .env

# Start the S3 backend
docker-compose up -d sftp-s3

sftp -P 2224 user@localhost  # uses password from .env SFTP_USERS
```

#### LocalStack Testing (Local S3)

```bash
# Set up directories and host key (if not already done)
./scripts/docker-setup.sh

# Start LocalStack and SFTP
docker-compose up -d localstack sftp-s3-local

# Initialize LocalStack bucket
./scripts/localstack-init.sh

# Connect via SFTP
sftp -P 2225 user@localhost  # password: localstacktest

# Verify files in LocalStack S3
aws s3 ls s3://test-bucket/sftp/ --endpoint-url="http://localhost:4566"
```

### Docker Configuration

#### Environment Variables

| Variable | Default | Backend | Purpose |
|----------|---------|---------|---------|
| `BACKEND` | `memory` | All | Storage backend: `memory`, `local`, or `s3` |
| `PORT` | `2222` | All | SFTP listening port |
| `SFTP_USERS` | - | All | Comma-separated user:password pairs (required unless using authorized_keys) |
| `RUST_LOG` | `sftp_s3=info` | All | Logging level |
| `HOST_KEY_FILE` | `/keys/ssh_host_ed25519_key` | All | Path to SSH host key |
| `AUTHORIZED_KEYS_FILE` | `/config/authorized_keys` | All | Path to authorized public keys |
| `LOCAL_ROOT` | `.` | local | Root directory for local filesystem backend |
| `S3_BUCKET` | - | s3 | AWS S3 bucket name |
| `S3_PREFIX` | (empty) | s3 | Prefix for objects in S3 |
| `S3_ENDPOINT` | - | s3 | Custom S3-compatible endpoint (LocalStack, MinIO) |
| `AWS_REGION` | `us-east-1` | s3 | AWS region |
| `AWS_ACCESS_KEY_ID` | - | s3 | AWS access key |
| `AWS_SECRET_ACCESS_KEY` | - | s3 | AWS secret key |

#### Volume Mounts

| Path | Purpose | Mode |
|------|---------|------|
| `/data` | Local backend storage | Read-Write |
| `/keys` | SSH host keys | Read-Only |
| `/config` | Config files (authorized_keys) | Read-Only |

### Building Images

#### Standard Debian Build

```bash
docker build -t sftp-s3:latest .
```

#### Alpine Build (Minimal Size)

```bash
docker build -f Dockerfile.alpine -t sftp-s3:alpine .
```

### Image Sizes

- **Standard (Debian)**: ~20MB
- **Alpine**: ~8MB

### Host Key Management

#### Using docker-setup.sh (Recommended)

```bash
./scripts/docker-setup.sh
# Generates key at: ./keys/ssh_host_ed25519_key
```

#### Manual Generation

```bash
./scripts/generate-host-key.sh
```

#### Using Existing Host Key

If you have an existing SSH **host key** (not your personal key), you can use it:

```bash
# Copy an existing HOST key (not your personal ~/.ssh/id_ed25519!)
cp /path/to/existing/host_key ./keys/ssh_host_ed25519_key
chmod 600 ./keys/ssh_host_ed25519_key
```

**Warning:** Never use your personal SSH key as a host key. Always generate a dedicated host key using `./scripts/generate-host-key.sh`.

### Authentication

#### Password Authentication

Configure SFTP user credentials in `docker-compose.yml` or `.env`:

```yaml
environment:
  SFTP_USERS: "myuser:strong-password"
```

Then connect:

```bash
sftp -P 2222 myuser@localhost
```

#### Public Key Authentication

1. Add your public key to `./config/authorized_keys`:

```bash
cat ~/.ssh/id_ed25519.pub >> ./config/authorized_keys
chmod 600 ./config/authorized_keys
```

2. Connect without password:

```bash
sftp -P 2222 user@localhost
```

### Production Deployment

#### Docker Secrets (for credentials)

```bash
# Create secrets
echo "my-bucket" | docker secret create s3_bucket -
echo "us-east-1" | docker secret create aws_region -
echo "AKIA..." | docker secret create aws_access_key -
echo "..." | docker secret create aws_secret_key -

# Update docker-compose.yml to use secrets
```

#### Health Checks

All services include TCP health checks on the SFTP port. Verify status:

```bash
docker-compose ps
# Status should show "healthy"
```

#### Logging

View logs from specific service:

```bash
docker-compose logs -f sftp-s3

# Set log level
docker-compose run -e RUST_LOG=sftp_s3=debug sftp-s3
```

#### Resource Limits

Add to docker-compose.yml service:

```yaml
services:
  sftp-s3:
    deploy:
      resources:
        limits:
          cpus: '1'
          memory: 512M
        reservations:
          cpus: '0.5'
          memory: 256M
```

### Security Considerations

1. **SFTP_USERS**: Must be explicitly set in docker-compose.yml or environment. No default credentials.
2. **Persistent Host Keys**: Use `./scripts/docker-setup.sh` to generate host keys. Ensures consistent server identity.
3. **SSH Key Authentication**: Preferred over passwords. Add public keys to `./config/authorized_keys`.
4. **Non-root Execution**: All containers run as UID 1000 for reduced attack surface.
5. **Read-only Config**: Host keys and authorized_keys mounted as read-only to prevent tampering.

### Troubleshooting

#### Connection Refused

```bash
# Check if service is running and healthy
docker-compose ps

# Check logs
docker-compose logs sftp-s3

# Test TCP connection
nc -zv localhost 2224
```

#### SFTP_USERS Not Set Error

```bash
# Error: SFTP_USERS must be explicitly set
# Solution: Set SFTP_USERS in docker-compose.yml before starting
docker-compose up -d
```

#### Permission Denied

- Verify host key file exists: `ls -la ./keys/ssh_host_ed25519_key`
- Check permissions: should be `600`
- Verify directory permissions: `ls -la ./keys/`

#### S3 Errors

- Verify credentials in `.env` file
- Check AWS region matches your bucket
- For LocalStack, verify it's healthy: `docker-compose ps localstack`
- Check S3 endpoint is accessible from container

### Cleanup

```bash
# Stop all services
docker-compose down

# Remove volumes (including persistent data)
docker-compose down -v

# Remove images
docker rmi sftp-s3:latest
```

## License

Apache 2.0
