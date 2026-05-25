use async_trait::async_trait;
use bytes::Bytes;
use russh_sftp::protocol::DataPayload;
use std::borrow::Cow;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod benchmark;
pub mod delegated;
pub mod local;
pub mod memory;
#[cfg(feature = "s3")]
pub mod s3;

pub use benchmark::BenchmarkBackend;
pub use delegated::{BackendRequest, BackendResponse, DelegatedBackend, DelegatedBackendFn};
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
    #[error("Operation not supported by this backend")]
    Unsupported,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Backend error: {0}")]
    Other(String),
}

/// Directory entry returned by `list_dir`.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub attrs: FileInfo,
}

/// File metadata information
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

/// Settable file attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetAttrs {
    pub size: Option<u64>,
    pub permissions: Option<u32>,
    pub atime: Option<u32>,
    pub mtime: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

impl SetAttrs {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.size.is_none()
            && self.permissions.is_none()
            && self.atime.is_none()
            && self.mtime.is_none()
            && self.uid.is_none()
            && self.gid.is_none()
    }

    pub fn merge_from(&mut self, other: &Self) {
        self.size = other.size.or(self.size);
        self.permissions = other.permissions.or(self.permissions);
        self.atime = other.atime.or(self.atime);
        self.mtime = other.mtime.or(self.mtime);
        self.uid = other.uid.or(self.uid);
        self.gid = other.gid.or(self.gid);
    }
}

/// Backend capability flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilities {
    pub symlinks: bool,
    pub set_attrs: bool,
    pub delegated_safe_streaming_fallback: bool,
}

#[derive(Debug, Clone)]
#[must_use]
pub struct FileInfo {
    pub size: u64,
    pub kind: FileKind,
    pub is_dir: bool,
    pub permissions: u32,
    pub mtime: u32,
    pub atime: u32,
    pub uid: u32,
    pub gid: u32,
}

impl FileInfo {
    /// Create `FileInfo` for a directory.
    fn new(
        size: u64,
        kind: FileKind,
        permissions: u32,
        mtime: u32,
        atime: u32,
        uid: u32,
        gid: u32,
    ) -> Self {
        Self {
            size,
            kind,
            is_dir: kind == FileKind::Directory,
            permissions,
            mtime,
            atime,
            uid,
            gid,
        }
    }

    /// Create `FileInfo` for a directory.
    pub fn directory() -> Self {
        let now = current_timestamp();
        Self::new(4096, FileKind::Directory, 0o755, now, now, 1000, 1000)
    }

    /// Create `FileInfo` for a directory with specific mtime.
    pub fn directory_with_mtime(mtime: u32) -> Self {
        Self::new(4096, FileKind::Directory, 0o755, mtime, mtime, 1000, 1000)
    }

    /// Create `FileInfo` for a regular file.
    pub fn file(size: u64) -> Self {
        let now = current_timestamp();
        Self::new(size, FileKind::File, 0o644, now, now, 1000, 1000)
    }

    /// Create `FileInfo` for a regular file with specific mtime.
    pub fn file_with_mtime(size: u64, mtime: u32) -> Self {
        Self::new(size, FileKind::File, 0o644, mtime, mtime, 1000, 1000)
    }

    /// Create FileInfo for a symlink.
    pub fn symlink(size: u64) -> Self {
        let now = current_timestamp();
        Self::new(size, FileKind::Symlink, 0o777, now, now, 1000, 1000)
    }
}

/// Handle for reading file data in chunks
#[async_trait]
pub trait ReadHandle: Send + Sync {
    /// Fast path for handles that can satisfy the read synchronously without
    /// constructing the boxed `async_trait` future.
    fn try_read_at(&self, _offset: u64, _len: u32) -> Option<BackendResult<DataPayload>> {
        None
    }

    /// Read data at the given offset
    async fn read_at(&self, offset: u64, len: u32) -> BackendResult<DataPayload>;

    /// Get the total file size
    fn size(&self) -> u64;
}

/// Handle for writing file data in chunks (supports streaming uploads)
#[async_trait]
pub trait WriteHandle: Send {
    /// Fast path for handles that can accept a write synchronously without
    /// constructing the boxed `async_trait` future.
    fn try_write_at(&mut self, _offset: u64, _data: &Bytes) -> Option<BackendResult<()>> {
        None
    }

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
    fn try_read_at(&self, offset: u64, len: u32) -> Option<BackendResult<DataPayload>> {
        let start = match usize::try_from(offset) {
            Ok(start) => start,
            Err(_) => {
                return Some(Err(BackendError::Other(
                    "read offset too large for this platform".into(),
                )));
            }
        };
        if start >= self.content.len() {
            return Some(Ok(Bytes::new().into()));
        }
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        let end = std::cmp::min(start.saturating_add(len), self.content.len());
        Some(Ok(self.content.slice(start..end).into()))
    }

    async fn read_at(&self, offset: u64, len: u32) -> BackendResult<DataPayload> {
        let start = usize::try_from(offset)
            .map_err(|_| BackendError::Other("read offset too large for this platform".into()))?;
        if start >= self.content.len() {
            return Ok(Bytes::new().into());
        }
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        let end = std::cmp::min(start.saturating_add(len), self.content.len());
        Ok(self.content.slice(start..end).into())
    }

    fn size(&self) -> u64 {
        self.content.len() as u64
    }
}

/// Write handle that buffers in memory and returns data on finish
/// Used by simple backends that don't support streaming
#[must_use]
pub struct BufferedWriteHandle {
    buffer: Vec<u8>,
}

impl BufferedWriteHandle {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Get the buffered data (for backends to write after finish)
    #[must_use]
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
        let end = start
            .checked_add(data.len())
            .ok_or_else(|| BackendError::Other("write range too large for this platform".into()))?;
        if start > self.buffer.len() {
            self.buffer.resize(start, 0);
        }
        if start == self.buffer.len() {
            self.buffer.extend_from_slice(&data);
        } else {
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
    backend: B,
}

impl<B: Backend> BufferedWriteWithBackend<B> {
    pub fn new(path: String, backend: B) -> Self {
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
    /// Return backend capability flags.
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            symlinks: false,
            set_attrs: false,
            delegated_safe_streaming_fallback: true,
        }
    }

    /// List directory contents
    ///
    /// Returns entries for the directory at `path`.
    /// Always includes "." and ".." entries.
    async fn list_dir(&self, path: &str) -> BackendResult<Vec<DirEntry>>;

    /// Get file or directory information
    async fn file_info(&self, path: &str) -> BackendResult<FileInfo>;

    /// Get file or directory information without following symlinks.
    async fn lstat(&self, path: &str) -> BackendResult<FileInfo> {
        self.file_info(path).await
    }

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
    /// Returns a `ReadHandle` for reading file data in chunks.
    /// For backends that support range requests (e.g., S3), this enables
    /// reading large files without loading them entirely into memory.
    async fn open_read(&self, path: &str) -> BackendResult<Box<dyn ReadHandle>>;

    /// Open a file for streaming writes
    ///
    /// Returns a `WriteHandle` for writing file data in chunks.
    /// For backends that support multipart uploads (e.g., S3), this enables
    /// uploading large files without buffering them entirely in memory.
    async fn open_write(&self, path: &str) -> BackendResult<Box<dyn WriteHandle + Send>>;

    /// Read the target of a symlink.
    async fn read_link(&self, _path: &str) -> BackendResult<String> {
        Err(BackendError::Unsupported)
    }

    /// Create a symlink.
    async fn symlink(&self, _linkpath: &str, _targetpath: &str) -> BackendResult<()> {
        Err(BackendError::Unsupported)
    }

    /// Apply supported metadata mutations to a path.
    async fn set_attrs(&self, _path: &str, _attrs: SetAttrs) -> BackendResult<()> {
        Err(BackendError::Unsupported)
    }
}

/// Normalize a path: trim leading/trailing slashes, handle empty as root,
/// and collapse `.` / `..` path components without escaping above root.
/// Returns `Cow::Borrowed` when input is already normalized, avoiding allocation.
#[must_use]
pub fn normalize_path(path: &str) -> Cow<'_, str> {
    if !path.as_bytes().contains(&b'/') {
        return match path {
            "" | "." | ".." => Cow::Borrowed(""),
            _ => Cow::Borrowed(path),
        };
    }

    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Cow::Borrowed("");
    }

    let needs_component_normalization = trimmed
        .split('/')
        .any(|component| matches!(component, "" | "." | ".."));

    if !needs_component_normalization {
        return Cow::Borrowed(trimmed);
    }

    let mut components = Vec::new();
    for component in trimmed.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            component => components.push(component),
        }
    }

    if components.is_empty() {
        Cow::Borrowed("")
    } else {
        Cow::Owned(components.join("/"))
    }
}

/// Get current Unix timestamp
#[must_use]
pub fn current_timestamp() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| unix_secs_to_u32(d.as_secs()))
        .unwrap_or(0)
}

pub(crate) fn unix_secs_to_u32(secs: u64) -> u32 {
    u32::try_from(secs).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_trimmed_paths_borrow() {
        assert!(matches!(
            normalize_path("/normal/file.txt/"),
            Cow::Borrowed("normal/file.txt")
        ));
    }

    #[test]
    fn test_normalize_plain_filename_borrows() {
        assert!(matches!(
            normalize_path("plain-file.txt"),
            Cow::Borrowed("plain-file.txt")
        ));
    }

    #[tokio::test]
    async fn test_buffered_read_rejects_offset_too_large_for_platform() {
        let handle = BufferedReadHandle::new(Bytes::from_static(b"data"));

        let result = handle.read_at(u64::MAX, 1).await;

        if usize::try_from(u64::MAX).is_ok() {
            assert!(matches!(result, Ok(bytes) if bytes.is_empty()));
        } else {
            assert!(matches!(result, Err(BackendError::Other(msg)) if msg.contains("offset")));
        }
    }

    #[tokio::test]
    async fn test_buffered_read_saturates_large_lengths() {
        let handle = BufferedReadHandle::new(Bytes::from_static(b"abcdef"));

        let bytes = handle.read_at(2, u32::MAX).await.unwrap();

        assert_eq!(bytes, Bytes::from_static(b"cdef"));
    }

    #[tokio::test]
    async fn test_buffered_write_rejects_offset_too_large_for_platform() {
        let mut handle = BufferedWriteHandle::new();

        let result = handle.write_at(u64::MAX, Bytes::from_static(b"x")).await;

        assert!(
            matches!(result, Err(BackendError::Other(msg)) if msg.contains("offset") || msg.contains("range"))
        );
    }

    #[tokio::test]
    async fn test_buffered_write_overwrites_existing_range() {
        let mut handle = BufferedWriteHandle::new();

        handle
            .write_at(0, Bytes::from_static(b"abcdef"))
            .await
            .unwrap();
        handle
            .write_at(2, Bytes::from_static(b"XYZ"))
            .await
            .unwrap();

        assert_eq!(handle.into_bytes(), Bytes::from_static(b"abXYZf"));
    }

    #[tokio::test]
    async fn test_buffered_write_preserves_sparse_gaps() {
        let mut handle = BufferedWriteHandle::new();

        handle.write_at(3, Bytes::from_static(b"z")).await.unwrap();

        assert_eq!(handle.into_bytes(), Bytes::from_static(b"\0\0\0z"));
    }

    #[test]
    fn test_unix_secs_to_u32_saturates() {
        assert_eq!(unix_secs_to_u32(42), 42);
        assert_eq!(unix_secs_to_u32(u64::MAX), u32::MAX);
    }
}
