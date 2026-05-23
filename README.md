# sftp-s3

A pluggable SFTP server with S3 and custom backend support, written in Rust.

## Project Status / Parity Target

`sftp-s3-rs` is the maintained implementation target for this project family. The Elixir
`sftpd-s3` codebase remains a useful behavioral reference, but Rust is the canonical target for
new lifecycle APIs, backend semantics, protocol behavior, and documentation.

## Features

- SFTP server using [russh](https://github.com/Eugeny/russh)
- SCP send and receive support for files and recursive directories
- Pluggable backend trait for custom storage implementations
- Built-in backends:
  - **Local** - Local filesystem backend with symlink and metadata mutation support
  - **Memory** - In-memory storage for testing/development
  - **S3** - Amazon S3 or S3-compatible storage (LocalStack, MinIO)
- Delegated/process-backed backend adapter for actor-style integrations
- Password authentication
- Public-key authentication
- Graceful lifecycle API with `serve()` and `ServerHandle`
- Optional SSH connection limits with `max_connections`
- Async/await with Tokio

## Guides

- [BACKENDS.md](BACKENDS.md) for backend tradeoffs and built-in backend behavior
- [CUSTOM_BACKENDS.md](CUSTOM_BACKENDS.md) for implementing your own backend
- [TELEMETRY.md](TELEMETRY.md) for tracing, span fields, and operational logging

## Capability Matrix

| Capability | `sftp-s3-rs` | `sftpd-s3` reference | Notes |
|----------|----------|----------|----------|
| Password auth | Yes | Yes | Rust CLI and embedded API |
| Public-key auth | Yes | Yes | Authorized keys and callback-based auth |
| SFTP | Yes | Yes | Rust is the protocol reference target |
| SCP receive/send | Yes | Yes | Rust supports file and recursive directory transfer |
| Local backend | Yes | Yes | Rust local backend supports symlinks and metadata mutation |
| Memory backend | Yes | Yes | Rust memory backend is the protocol semantics reference backend |
| S3 backend | Yes | Yes | Uses `.keep` directory markers |
| Session limits | Yes | Yes | `ServerConfig::with_max_connections` / `MAX_CONNECTIONS` |
| Graceful shutdown API | Yes | Yes | `serve()`, `serve_on_socket()`, `ServerHandle` |
| Delegated backends | Yes | Yes | Rust `DelegatedBackend` is the process-backed equivalent |
| Symlink support | Local, memory, delegated | Reference support | S3 returns explicit `OpUnsupported` |
| Metadata mutation by backend | Local, memory, delegated | Reference support | S3 returns explicit `OpUnsupported`; empty `setstat` remains `Ok` |

## Migration Note

If you are moving from `sftpd-s3`, treat `sftp-s3-rs` as the canonical implementation target.
Match behavior against Rust first. The Rust API now exposes explicit lifecycle control,
connection limits, delegated backends, and per-backend capability differences instead of silently
acknowledging unsupported operations.
## Quick Start

```rust
use sftp_s3::{Server, ServerConfig, MemoryBackend};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let backend = MemoryBackend::new();
    let config = ServerConfig::new()
        .port(2222)
        .with_generated_key();

    let handle = Server::new(backend)
        .config(config)
        .with_users(vec![("user".into(), "pass".into())])
        .serve()
        .await?;

    handle.wait().await
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

Implement the `Backend` trait for custom storage. The full guide is in
[CUSTOM_BACKENDS.md](CUSTOM_BACKENDS.md).

```rust
use async_trait::async_trait;
use bytes::Bytes;
use sftp_s3::backend::{
    Backend, BackendResult, BufferedReadHandle, DirEntry, FileInfo, ReadHandle, WriteHandle,
};

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

    async fn read_file(&self, path: &str) -> BackendResult<Bytes> {
        // Implementation
    }

    async fn write_file(&self, path: &str, content: Bytes) -> BackendResult<()> {
        // Implementation
    }

    async fn open_read(&self, path: &str) -> BackendResult<Box<dyn ReadHandle>> {
        Ok(Box::new(BufferedReadHandle::new(self.read_file(path).await?)))
    }

    async fn open_write(&self, path: &str) -> BackendResult<Box<dyn WriteHandle + Send>> {
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
S3_BUCKET=my-bucket cargo run --example s3_server
```

Connect with an SFTP client:

```bash
sftp -P 2222 user@localhost
```

## Testing

Run the default suite through the Nix development shell:

```bash
nix develop -c cargo test --all-features
```

The MinIO-backed S3 end-to-end test is ignored by default because it starts a
local MinIO process. `minio` is provided by the flake:

```bash
nix develop -c cargo test --all-features --test s3_minio_integration -- --ignored --nocapture
```

## Performance

OpenSSH `sftp` round-trip benchmark against the memory backend, measured on May
22, 2026 with release binaries. The client requests the SSH `sftp` subsystem
(`sftp -vv` reports `Sending subsystem: sftp` and remote SFTP version 3); it is
not using legacy scp protocol. Each row is 10 measured runs after 2 warmups using
public-key authentication, `Compression=no`, `-R 16`, and `-B 131072`. The
single-file rows were constrained to `aes256-gcm@openssh.com`. Throughput is
calculated as upload plus download bytes divided by wall time.

| Client file source | Size | Cipher | main throughput | main time | current throughput | current time | Change |
|--------------------|------|--------|-----------------|-----------|--------------------|--------------|--------|
| disk | 32MB | aes256-gcm | 463.5 MB/s | 0.138s +/- 0.007s | 550.8 MB/s | 0.116s +/- 0.011s | +18.8% |
| disk | 256MB | aes256-gcm | 873.7 MB/s | 0.586s +/- 0.044s | 1203.9 MB/s | 0.425s +/- 0.015s | +37.8% |
| disk | 1024MB | aes256-gcm | 937.7 MB/s | 2.184s +/- 0.081s | 1304.0 MB/s | 1.571s +/- 0.020s | +39.1% |
| `/dev/shm` | 32MB | aes256-gcm | 484.3 MB/s | 0.132s +/- 0.005s | 559.4 MB/s | 0.114s +/- 0.005s | +15.5% |
| `/dev/shm` | 256MB | aes256-gcm | 862.8 MB/s | 0.593s +/- 0.029s | 1230.8 MB/s | 0.416s +/- 0.021s | +42.7% |
| `/dev/shm` | 1024MB | aes256-gcm | 988.5 MB/s | 2.072s +/- 0.118s | 1376.1 MB/s | 1.488s +/- 0.087s | +39.2% |

The many-small-files benchmark transfers a 1GiB flat directory made of 10,251
varied-size files, then downloads the files back in the same OpenSSH `sftp`
session. The batch lists each file explicitly to measure per-file transfer
overhead without relying on SFTP glob expansion.

| Workload | Files | Payload | Cipher | main throughput | main file ops | main time | current throughput | current file ops | current time | Change |
|----------|-------|---------|--------|-----------------|---------------|-----------|--------------------|------------------|--------------|--------|
| varied small files | 10,251 | 1GiB upload + 1GiB download | default OpenSSH | 224.0 MB/s | 2,242.7 files/s | 9.142s +/- 0.189s | 269.0 MB/s | 2,692.8 files/s | 7.614s +/- 0.126s | +20.1% |
| varied small files | 10,251 | 1GiB upload + 1GiB download | aes256-gcm | 256.1 MB/s | 2,563.4 files/s | 7.998s +/- 0.131s | 334.6 MB/s | 3,349.8 files/s | 6.120s +/- 0.077s | +30.7% |
## Benchmark Client

The `sftp-bench-client` binary is a Rust SFTP client for measuring this server without relying on
the system `sftp` command:

```bash
nix develop -c cargo run --release --bin sftp-bench-client -- \
  --host 127.0.0.1 \
  --port 2222 \
  --user benchmark \
  --password benchmark \
  --operation roundtrip \
  --size 256MiB \
  --iterations 5
```

It supports `upload`, `download`, and `roundtrip` modes. Download mode uploads fixture files before
measurement, then times repeated reads. By default benchmark files are removed after each run; pass
`--keep-files` to inspect them on the server.

## Benchmark Matrix

Run the russh/russh-sftp benchmark matrix from a Nix development shell:

```bash
nix develop -c ./benchmark-all.sh --client rust --ciphers aes256-gcm
```

To run only selected sizes, pass `--sizes` with MiB values:

```bash
nix develop -c ./benchmark-all.sh --client rust --ciphers aes256-gcm --sizes 1024,10240
```

The matrix builds isolated server binaries for a 2x2 comparison: upstream/current `russh` + `russh-sftp`
(`main` + `master`) versus the pinned MJC branches (`mjc/own-inbound-channel-payloads` +
`deserialize-bytes-optimization`). It then runs roundtrip transfer benchmarks. For the Rust client
matrix, the server uses the `benchmark` backend: it records file sizes and metadata, discards
uploaded bytes, and synthesizes zero-filled reads so large 50-100GiB protocol runs do not exhaust
RAM. The Rust benchmark client is built once from the current checkout and reused across the matrix.

Results below were measured on a Darwin arm64 Apple Silicon machine with the Rust benchmark client,
the `benchmark` backend, and the 2x2 matrix:

- `current-current` = `russh main` + `russh-sftp master`
- `current-mjc` = `russh main` + `russh-sftp deserialize-bytes-optimization`
- `mjc-current` = `russh mjc/own-inbound-channel-payloads` + `russh-sftp master`
- `mjc-mjc` = `russh mjc/own-inbound-channel-payloads` + `russh-sftp deserialize-bytes-optimization`

### Darwin arm64 matrix results

#### `aes256-gcm`

| Config | 1024MB MB/s | 1024MB mean | 10240MB MB/s | 10240MB mean |
| --- | ---: | ---: | ---: | ---: |
| current-current | 345.9 | 5.921s | 332.3 | 61.626s |
| current-mjc | 1040.1 | 1.969s | 998.5 | 20.511s |
| mjc-current | 343.2 | 5.968s | 328.8 | 62.293s |
| mjc-mjc | 936.9 | 2.186s | 909.0 | 22.530s |

Under `aes256-gcm`, the `russh-sftp` MJC branch is carrying nearly all of the win on this machine:
`current-mjc` is about 3x faster than `current-current`, while `mjc-current` stays essentially flat
against upstream/current. Pairing both MJC branches together remains much faster than upstream, but
slightly behind `current-mjc`, so the `russh` branch is not where the large gain is coming from in
this cipher on Apple Silicon.

#### `chacha20-poly1305`

| Config | 1024MB MB/s | 1024MB mean | 10240MB MB/s | 10240MB mean |
| --- | ---: | ---: | ---: | ---: |
| current-current | 225.3 | 9.091s | 231.6 | 88.446s |
| current-mjc | 412.1 | 4.970s | 394.5 | 51.910s |
| mjc-current | 247.0 | 8.293s | 252.4 | 81.136s |
| mjc-mjc | 491.4 | 4.168s | 491.7 | 41.653s |

Under `chacha20-poly1305`, both MJC branches help and `mjc-mjc` is the best combination. The
`russh-sftp` branch still contributes most of the improvement, but unlike the GCM run, the `russh`
branch also moves the result in the right direction. On this M1 machine, `aes256-gcm` still
outperforms `chacha20-poly1305` across all four configs, which matches the expectation that Apple
Silicon's AES acceleration makes GCM especially strong here.

Default transfer sizes are:

```text
1MiB, 32MiB, 256MiB, 1GiB, 10GiB, 50GiB, 100GiB
```

Existing valid result JSON files are skipped, so interrupted runs can be resumed by running the same
command again. Empty or malformed result files are treated as missing and rerun.

Results are written under `benchmark_results/`:

- `run-*.log` captures the full matrix output
- `<size>mb-<scenario>-<config>.json` stores hyperfine JSON for each completed case
- `server-<config>.log` captures server output for the most recent run of a config

Large disk fixtures are created sparsely with `truncate`; the Rust benchmark client generates its
payload in memory from the requested size. Transfer commands are not wrapped in a timeout, so large
cases run until the client finishes or fails on its own.

Benchmark dependency patches live in `benchmark_patches/`. The matrix applies matching patches to
temporary dependency checkouts before building:

```text
benchmark_patches/<component>/all/*.patch
benchmark_patches/<component>/<ref-name>/*.patch
benchmark_patches/<component>/<matrix-label>/*.patch
```

Use `SFTP_BENCH_CHUNK_SIZE` to override the Rust client's request size. The default is `64KiB`, which
keeps the Rust client compatible with older server refs that reject larger SFTP packets.
Use `SFTP_BENCH_SIZES` as an environment alternative to `--sizes`.
## Docker Deployment

### Quick Start

#### Memory Backend (Testing)

```bash
# Set up directories and host key
./scripts/docker-setup.sh

# Start the memory backend (uses default credentials from docker-compose.yml)
docker-compose up -d sftp-memory

# Connect with default credentials
sftp -P 2222 user@localhost
# password: changeme
```

To use custom credentials, either:
1. Edit `docker-compose.yml` and set the SFTP_USERS variable for sftp-memory
2. Or use environment variables: `SFTP_USERS=myuser:mypass docker-compose up -d sftp-memory`

#### Local Filesystem Backend

```bash
# Set up directories and host key (if not already done)
./scripts/docker-setup.sh

docker-compose up -d sftp-local

# Files are stored in the 'sftp-data' Docker volume
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
| `MAX_CONNECTIONS` | - | All | Maximum concurrent SSH connections |
| `SFTP_USERS` | - | All | Comma-separated user:password pairs (required unless using authorized_keys) |
| `RUST_LOG` | `sftp_s3=info` | All | Logging level |
| `HOST_KEY_FILE` | `/keys/ssh_host_ed25519_key` | All | Path to SSH host key |
| `AUTHORIZED_KEYS_FILE` | `/config/authorized_keys` | All | Path to authorized public keys |
| `LOCAL_ROOT` | `.` (optional) | local | Root directory for local filesystem backend |
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

1. **Credentials**: The example docker-compose.yml includes test credentials (`user:changeme`) for development. **Change these before production use** or use public key authentication.
2. **Public Key Auth**: When using `authorized_keys`, SFTP_USERS can be omitted entirely for password-less authentication.
3. **Persistent Host Keys**: Use `./scripts/docker-setup.sh` to generate host keys. Ensures consistent server identity.
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
