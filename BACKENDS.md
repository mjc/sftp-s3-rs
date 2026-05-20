# Backends

`sftp-s3` separates the SSH/SFTP runtime from storage through the `Backend`
trait. A backend presents a filesystem-like interface over whatever storage you
want to expose to SFTP clients.

## Choosing a Backend

| Need | Use |
| --- | --- |
| Tests, demos, and local development | `MemoryBackend` |
| A real directory on disk | `LocalBackend` |
| Amazon S3, MinIO, LocalStack, or another S3-compatible service | `S3Backend` |
| A database, API, queue, or application-specific store | A custom `Backend` implementation |
| Large-file transfers without whole-file buffering | Implement `open_read` and `open_write` with streaming handles |

## Built-In Backends

### `MemoryBackend`

The memory backend stores file contents in process memory and is best for:

- unit tests
- examples
- local experiments
- embedding a server without external services

It supports the full `Backend` trait. Reads use a buffered read handle and
writes collect chunks before committing the final file.

```rust
use sftp_s3::{MemoryBackend, Server, ServerConfig};

# async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
Server::new(MemoryBackend::new())
    .config(ServerConfig::new().with_generated_key())
    .with_users(vec![("dev".into(), "dev".into())])
    .run()
    .await?;
# Ok(())
# }
```

### `LocalBackend`

The local backend maps SFTP paths into a configured root directory on disk. It
rejects parent-directory traversal and only resolves paths under that root.

```rust
use sftp_s3::{LocalBackend, Server, ServerConfig};

# async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
Server::new(LocalBackend::new("/srv/sftp"))
    .config(ServerConfig::new().with_generated_key())
    .with_users(vec![("user".into(), "pass".into())])
    .run()
    .await?;
# Ok(())
# }
```

### `S3Backend`

The S3 backend maps SFTP operations onto object storage. It is available behind
the default `s3` feature and supports:

- AWS S3 and S3-compatible endpoints
- optional object key prefixes for tenancy or namespacing
- delimiter-based directory listings
- paginated listings
- range reads
- lazy multipart streaming writes
- direct `PutObject` writes for small files
- `.keep` marker objects for empty directories

```rust
use sftp_s3::{S3Backend, S3Config, Server, ServerConfig};

# async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
let backend = S3Backend::from_env(S3Config::new("my-bucket").with_prefix("tenant-a/")).await;

Server::new(backend)
    .config(ServerConfig::new().with_generated_key())
    .with_users(vec![("user".into(), "pass".into())])
    .run()
    .await?;
# Ok(())
# }
```

Use `S3Backend::with_endpoint` for MinIO, LocalStack, or another compatible
service:

```rust
# use sftp_s3::{S3Backend, S3Config};
# async fn build() {
let backend = S3Backend::with_endpoint(
    S3Config::new("test-bucket").with_prefix("sftp/"),
    "http://localhost:4566",
    "us-east-1",
)
.await;
# let _ = backend;
# }
```

## Backend Semantics

Backends should behave like a filesystem from an SFTP client's point of view,
even when the storage system is not a filesystem.

Important conventions:

- `list_dir` should include `.` and `..`.
- `file_info("/")` and equivalent root paths should return directory metadata.
- `FileInfo::file`, `FileInfo::file_with_mtime`, `FileInfo::directory`, and
  `FileInfo::directory_with_mtime` build compatible metadata.
- `normalize_path` removes leading and trailing slashes and treats `.`, `..`,
  and empty paths as root.
- Use stable `BackendError` variants so clients receive predictable SFTP status
  codes.

## Streaming

The trait includes both whole-file methods and streaming methods:

- `read_file` and `write_file` are simple and useful for small objects.
- `open_read` returns a `ReadHandle` for range-like reads.
- `open_write` returns a `WriteHandle` for chunked writes.

For large object stores, implement the streaming methods directly. For simpler
stores, it is fine to buffer in memory and commit on `finish`.

S3 multipart uploads are append-only, so `S3Backend` requires sequential write
offsets. It starts multipart uploads lazily and uses direct `PutObject` for
small files. Backends that support random writes should make overwrite behavior
explicit and test overlapping chunks.

## Testing Recommendations

At minimum, test:

- root listings include `.` and `..`
- missing path behavior
- file metadata size and directory flags
- write then read round trips
- rename semantics
- directory creation and deletion
- path normalization and traversal rejection for filesystem-backed stores
- streaming read offsets and write finalization
- non-sequential or overlapping write offsets when the backend supports them

For S3-compatible backends, also test prefix handling, delimiter listings,
pagination, `.keep` marker filtering, range reads, multipart completion, and
abort behavior.
