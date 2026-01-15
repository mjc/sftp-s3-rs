use async_trait::async_trait;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod memory;
#[cfg(feature = "s3")]
pub mod s3;

pub use memory::MemoryBackend;
#[cfg(feature = "s3")]
pub use s3::{S3Backend, S3Config};

/// Result type for backend operations
pub type BackendResult<T> = Result<T, BackendError>;

/// Errors that can occur in backend operations
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("File or directory not found")]
    NotFound,
    #[error("Permission denied")]
    PermissionDenied,
    #[error("File already exists")]
    AlreadyExists,
    #[error("Not a directory")]
    NotADirectory,
    #[error("Is a directory")]
    IsADirectory,
    #[error("Directory not empty")]
    DirectoryNotEmpty,
    #[error("I/O error: {0}")]
    Io(String),
    #[error("Backend error: {0}")]
    Other(String),
}

/// Directory entry returned by list_dir
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub attrs: FileInfo,
}

/// File metadata information
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub size: u64,
    pub is_dir: bool,
    pub permissions: u32,
    pub mtime: u32,
    pub atime: u32,
    pub uid: u32,
    pub gid: u32,
}

impl FileInfo {
    /// Create FileInfo for a directory
    pub fn directory() -> Self {
        Self {
            size: 4096,
            is_dir: true,
            permissions: 0o755,
            mtime: current_timestamp(),
            atime: current_timestamp(),
            uid: 1000,
            gid: 1000,
        }
    }

    /// Create FileInfo for a directory with specific mtime
    pub fn directory_with_mtime(mtime: u32) -> Self {
        Self {
            size: 4096,
            is_dir: true,
            permissions: 0o755,
            mtime,
            atime: mtime,
            uid: 1000,
            gid: 1000,
        }
    }

    /// Create FileInfo for a regular file
    pub fn file(size: u64) -> Self {
        Self {
            size,
            is_dir: false,
            permissions: 0o644,
            mtime: current_timestamp(),
            atime: current_timestamp(),
            uid: 1000,
            gid: 1000,
        }
    }

    /// Create FileInfo for a regular file with specific mtime
    pub fn file_with_mtime(size: u64, mtime: u32) -> Self {
        Self {
            size,
            is_dir: false,
            permissions: 0o644,
            mtime,
            atime: mtime,
            uid: 1000,
            gid: 1000,
        }
    }
}

/// Backend trait for storage implementations
///
/// Implement this trait to create custom storage backends.
/// All paths are normalized strings without leading slashes.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// List directory contents
    ///
    /// Returns entries for the directory at `path`.
    /// Always includes "." and ".." entries.
    async fn list_dir(&self, path: &str) -> BackendResult<Vec<DirEntry>>;

    /// Get file or directory information
    async fn file_info(&self, path: &str) -> BackendResult<FileInfo>;

    /// Create a directory
    ///
    /// Creates the directory at `path`. Parent directories must exist.
    async fn make_dir(&self, path: &str) -> BackendResult<()>;

    /// Delete an empty directory
    async fn del_dir(&self, path: &str) -> BackendResult<()>;

    /// Delete a file
    async fn delete(&self, path: &str) -> BackendResult<()>;

    /// Rename/move a file or directory
    async fn rename(&self, src: &str, dst: &str) -> BackendResult<()>;

    /// Read entire file contents
    ///
    /// For the initial implementation, files are loaded entirely into memory.
    /// Future versions may support streaming for large files.
    async fn read_file(&self, path: &str) -> BackendResult<Vec<u8>>;

    /// Write file contents
    ///
    /// Creates or overwrites the file at `path` with `content`.
    async fn write_file(&self, path: &str, content: Vec<u8>) -> BackendResult<()>;
}

/// Normalize a path: trim leading/trailing slashes, handle empty as root
pub fn normalize_path(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        String::new()
    } else {
        trimmed.to_string()
    }
}

/// Get current Unix timestamp
pub fn current_timestamp() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}
