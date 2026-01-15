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

## License

Apache 2.0
