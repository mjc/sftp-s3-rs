use async_trait::async_trait;
use bytes::Bytes;
use std::borrow::Cow;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod local;
pub mod memory;
#[cfg(feature = "s3")]
pub mod s3;

pub use local::LocalBackend;
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
    Io(#[from] std::io::Error),
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

/// Handle for reading file data in chunks
#[async_trait]
pub trait ReadHandle: Send + Sync {
    /// Read data at the given offset
    async fn read_at(&self, offset: u64, len: u32) -> BackendResult<Bytes>;

    /// Get the total file size
    fn size(&self) -> u64;
}

/// Handle for writing file data in chunks (supports streaming uploads)
#[async_trait]
pub trait WriteHandle: Send {
    /// Write data at the given offset. Takes Bytes to support zero-copy passing
    /// from the SFTP protocol layer, avoiding unnecessary copies in the hot path.
    async fn write_at(&mut self, offset: u64, data: Bytes) -> BackendResult<()>;

    /// Finish the write and flush all data
    async fn finish(self: Box<Self>) -> BackendResult<()>;

    /// Abort the write and clean up
    async fn abort(self: Box<Self>) -> BackendResult<()>;
}

/// Default read handle that buffers entire file in memory
pub struct BufferedReadHandle {
    content: Bytes,
}

impl BufferedReadHandle {
    pub fn new(content: Bytes) -> Self {
        Self { content }
    }
}

#[async_trait]
impl ReadHandle for BufferedReadHandle {
    async fn read_at(&self, offset: u64, len: u32) -> BackendResult<Bytes> {
        let start = usize::try_from(offset)
            .map_err(|_| BackendError::Other("read offset too large for this platform".into()))?;
        if start >= self.content.len() {
            return Ok(Bytes::new());
        }
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        let end = std::cmp::min(start.saturating_add(len), self.content.len());
        Ok(self.content.slice(start..end))
    }

    fn size(&self) -> u64 {
        self.content.len() as u64
    }
}

/// Write handle that buffers in memory and returns data on finish
/// Used by simple backends that don't support streaming
pub struct BufferedWriteHandle {
    buffer: Vec<u8>,
}

impl BufferedWriteHandle {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Get the buffered data (for backends to write after finish)
    pub fn into_bytes(self) -> Bytes {
        Bytes::from(self.buffer)
    }
}

impl Default for BufferedWriteHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WriteHandle for BufferedWriteHandle {
    async fn write_at(&mut self, offset: u64, data: Bytes) -> BackendResult<()> {
        let start = usize::try_from(offset)
            .map_err(|_| BackendError::Other("write offset too large for this platform".into()))?;
        if start > self.buffer.len() {
            self.buffer.resize(start, 0);
        }
        if start == self.buffer.len() {
            self.buffer.extend_from_slice(&data);
        } else {
            let end = start + data.len();
            if end > self.buffer.len() {
                self.buffer.resize(end, 0);
            }
            self.buffer[start..end].copy_from_slice(&data);
        }
        Ok(())
    }

    async fn finish(self: Box<Self>) -> BackendResult<()> {
        // BufferedWriteHandle doesn't write - caller must handle data
        // This is for S3-style handles that write on finish
        Ok(())
    }

    async fn abort(self: Box<Self>) -> BackendResult<()> {
        Ok(())
    }
}

/// Wrapper that writes buffered data to backend on finish
pub struct BufferedWriteWithBackend<B: Backend> {
    inner: BufferedWriteHandle,
    path: String,
    backend: Arc<B>,
}

impl<B: Backend> BufferedWriteWithBackend<B> {
    pub fn new(path: String, backend: Arc<B>) -> Self {
        Self {
            inner: BufferedWriteHandle::new(),
            path,
            backend,
        }
    }
}

#[async_trait]
impl<B: Backend> WriteHandle for BufferedWriteWithBackend<B> {
    async fn write_at(&mut self, offset: u64, data: Bytes) -> BackendResult<()> {
        self.inner.write_at(offset, data).await
    }

    async fn finish(self: Box<Self>) -> BackendResult<()> {
        let data = Bytes::from(self.inner.buffer);
        self.backend.write_file(&self.path, data).await
    }

    async fn abort(self: Box<Self>) -> BackendResult<()> {
        Ok(())
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

    /// Read entire file contents (simple API for small files)
    async fn read_file(&self, path: &str) -> BackendResult<Bytes>;

    /// Write file contents (simple API for small files)
    async fn write_file(&self, path: &str, content: Bytes) -> BackendResult<()>;

    /// Open a file for streaming reads
    ///
    /// Returns a ReadHandle for reading file data in chunks.
    /// For backends that support range requests (e.g., S3), this enables
    /// reading large files without loading them entirely into memory.
    async fn open_read(&self, path: &str) -> BackendResult<Box<dyn ReadHandle>>;

    /// Open a file for streaming writes
    ///
    /// Returns a WriteHandle for writing file data in chunks.
    /// For backends that support multipart uploads (e.g., S3), this enables
    /// uploading large files without buffering them entirely in memory.
    async fn open_write(&self, path: &str) -> BackendResult<Box<dyn WriteHandle + Send>>;
}

/// Normalize a path: trim leading/trailing slashes, handle empty as root.
/// Returns Cow::Borrowed when input is already normalized, avoiding allocation.
pub fn normalize_path(path: &str) -> Cow<'_, str> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        Cow::Borrowed("")
    } else if trimmed.len() == path.len() {
        // No slashes were trimmed, return borrowed
        Cow::Borrowed(path)
    } else {
        Cow::Owned(trimmed.to_string())
    }
}

/// Get current Unix timestamp
pub fn current_timestamp() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| unix_secs_to_u32(d.as_secs()))
        .unwrap_or(0)
}

pub(crate) fn unix_secs_to_u32(secs: u64) -> u32 {
    u32::try_from(secs).unwrap_or(u32::MAX)
}
