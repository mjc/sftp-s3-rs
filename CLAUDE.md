# Claude Development Guide

## Build Commands
- **Check compilation**: `cargo check` (preferred over `cargo build` for faster feedback)
- **Run tests**: `cargo test`
- **Format code**: `cargo fmt`
- **Lint**: `cargo clippy`
- **Build release**: `cargo build --release`

## Project Structure
- `src/backend/` - Storage backend trait and implementations
  - `mod.rs` - Backend trait, BackendError, normalize_path helper
  - `memory.rs` - In-memory backend for testing
  - `s3.rs` - AWS S3 backend
- `src/handle/` - SFTP file handle management
- `src/sftp_handler.rs` - SFTP protocol implementation
- `src/ssh_handler.rs` - SSH session management
- `src/server.rs` - Server configuration and startup
- `examples/` - Usage examples

## Architecture
- Pluggable backend via `Backend` trait
- Uses `Bytes` for file content (cheap clones, zero-copy where possible)
- Uses `Cow<str>` for path normalization (avoids allocation when input already normalized)
- Numeric handles (u64) instead of UUID strings
- Async/await with Tokio runtime

## Testing
- Unit tests inline in modules
- Property tests with proptest in backend modules
- Run specific test: `cargo test test_name`

## Code Style
- Minimize allocations - use references, Cow, Bytes where appropriate
- Functional style with iterators
- Error handling with thiserror
- Logging with tracing
