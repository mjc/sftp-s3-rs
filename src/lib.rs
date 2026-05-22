//! # sftp-s3
//!
//! A pluggable SFTP/SCP server with local, memory, S3, and delegated backend support.
//!
//! `sftp-s3-rs` is the maintained parity target for the broader project family. The Rust
//! implementation is the reference point for lifecycle control, backend semantics, and protocol
//! behavior.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use sftp_s3::{MemoryBackend, Server, ServerConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
//!     let backend = MemoryBackend::new();
//!     let config = ServerConfig::new()
//!         .port(2222)
//!         .with_generated_key();
//!
//!     let handle = Server::new(backend)
//!         .config(config)
//!         .with_users(vec![("user".into(), "pass".into())])
//!         .serve()
//!         .await?;
//!
//!     handle.wait().await
//! }
//! ```
//!
//! ## S3 Backend
//!
//! ```rust,ignore
//! use sftp_s3::{S3Backend, S3Config, Server, ServerConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
//!     let s3_config = S3Config::new("my-bucket")
//!         .with_prefix("sftp/");
//!     let backend = S3Backend::from_env(s3_config).await;
//!
//!     let handle = Server::new(backend)
//!         .config(ServerConfig::new().with_generated_key())
//!         .with_users(vec![("user".into(), "pass".into())])
//!         .serve()
//!         .await?;
//!
//!     handle.wait().await
//! }
//! ```
//!
//! ## Custom Backend
//!
//! Implement the `Backend` trait for custom storage:
//!
//! ```rust,ignore
//! use sftp_s3::backend::{Backend, BackendResult, DirEntry, FileInfo};
//! use async_trait::async_trait;
//!
//! struct MyBackend;
//!
//! #[async_trait]
//! impl Backend for MyBackend {
//!     async fn list_dir(&self, path: &str) -> BackendResult<Vec<DirEntry>> {
//!         // Implementation
//!         todo!()
//!     }
//!     // ... other methods
//! }
//! ```

pub mod backend;
pub mod error;
pub mod handle;
pub mod scp_handler;
pub mod server;
pub mod sftp_handler;
pub mod ssh_handler;

// Re-exports for convenience
pub use backend::local::LocalBackend;
pub use backend::memory::MemoryBackend;
pub use backend::{
    Backend, BackendCapabilities, BackendError, BackendRequest, BackendResponse, BackendResult,
    BenchmarkBackend, DelegatedBackend, DelegatedBackendFn, DirEntry, FileInfo, FileKind, SetAttrs,
};
#[cfg(feature = "s3")]
pub use backend::{S3Backend, S3Config};

pub use error::Error;
pub use server::{parse_cipher, Server, ServerConfig, ServerHandle, AVAILABLE_CIPHERS};
