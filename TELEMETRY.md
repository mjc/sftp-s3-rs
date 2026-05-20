# Telemetry

`sftp-s3` uses `tracing` for logs and operation spans. The binary initializes a
`tracing_subscriber` formatter and enables close events for spans, which makes
operation durations visible when debug spans are enabled.

## Enabling Logs

Use `RUST_LOG` to choose verbosity:

```bash
RUST_LOG=sftp_s3=info cargo run --example memory_server
RUST_LOG=sftp_s3=debug cargo run --example memory_server
RUST_LOG=sftp_s3=trace,russh=warn cargo run --example memory_server
```

For the packaged binary and Docker image, `RUST_LOG` works the same way:

```bash
RUST_LOG=sftp_s3=debug docker-compose up sftp-memory
```

## Instrumented Areas

The server emits events or spans around:

- server startup
- authentication attempts
- SSH channel and subsystem handling
- SFTP operations such as open, close, read, write, mkdir, rename, remove, stat,
  and readdir
- backend operations in the built-in local and S3 backends
- SCP command parsing and file operations

Many SFTP handler methods use `#[instrument]`, so debug-level span close events
include elapsed time for the operation.

## Useful Fields

Common fields include:

- `path`
- `handle`
- `offset`
- `len`
- `read_len`
- `write`
- `create`
- `prefix`
- S3 multipart `key`, `upload_id`, `part`, and `size`

Field availability depends on the operation and on whether the value would be
safe and useful to log.

## Production Notes

- Prefer `info` for normal operation.
- Use `debug` while diagnosing SFTP client behavior, latency, or backend calls.
- Be careful with path logging in multi-tenant systems; paths may contain
  customer identifiers or filenames.
- Avoid slow log sinks on hot paths. SFTP reads and writes can be frequent.

## Custom Backends

Custom backends can use `tracing` directly:

```rust
use tracing::{debug, instrument};

#[instrument(level = "debug", skip(self, content), fields(path = %path, len = content.len()))]
async fn write_file(&self, path: &str, content: bytes::Bytes) -> sftp_s3::BackendResult<()> {
    debug!("writing file");
    # let _ = (path, content);
    Ok(())
}
```

Follow the built-in backends' convention: include enough fields to diagnose
storage behavior, but skip large payloads and secrets.
