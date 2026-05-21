use super::{
    normalize_path, unix_secs_to_u32, Backend, BackendError, BackendResult, DirEntry, FileInfo,
    ReadHandle, WriteHandle,
};
use async_trait::async_trait;
use bytes::{BufMut, Bytes};
use std::path::{Component, Path, PathBuf};
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tracing::debug;

/// Local filesystem storage backend
pub struct LocalBackend {
    root: PathBuf,
}

impl LocalBackend {
    /// Create a new local backend rooted at the given path
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// Get the full filesystem path for an SFTP path, rejecting traversal
    /// outside the configured root.
    fn full_path(&self, path: &str) -> BackendResult<PathBuf> {
        Self::check_root_traversal(path)?;

        let normalized = normalize_path(path);
        if normalized.is_empty() {
            return Ok(self.root.clone());
        }

        let mut full_path = self.root.clone();
        for component in Path::new(normalized.as_ref()).components() {
            match component {
                Component::Normal(part) => full_path.push(part),
                Component::CurDir => {}
                Component::ParentDir => {
                    if !full_path.pop() {
                        return Err(BackendError::PermissionDenied);
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(BackendError::PermissionDenied);
                }
            }
        }

        Ok(full_path)
    }

    fn check_root_traversal(path: &str) -> BackendResult<()> {
        let mut depth = 0usize;
        let mut escaped_root = false;

        for component in Path::new(path).components() {
            match component {
                Component::Normal(_) => {
                    if escaped_root {
                        return Err(BackendError::PermissionDenied);
                    }
                    depth += 1;
                }
                Component::CurDir | Component::RootDir => {}
                Component::ParentDir => {
                    if depth == 0 {
                        escaped_root = true;
                    } else {
                        depth -= 1;
                    }
                }
                Component::Prefix(_) => return Err(BackendError::PermissionDenied),
            }
        }

        Ok(())
    }

    /// Convert `std::io::Error` to `BackendError`.
    fn map_io_error(err: std::io::Error) -> BackendError {
        match err.kind() {
            std::io::ErrorKind::NotFound => BackendError::NotFound,
            std::io::ErrorKind::PermissionDenied => BackendError::PermissionDenied,
            std::io::ErrorKind::AlreadyExists => BackendError::AlreadyExists,
            std::io::ErrorKind::DirectoryNotEmpty => BackendError::DirectoryNotEmpty,
            std::io::ErrorKind::IsADirectory => BackendError::IsADirectory,
            _ => BackendError::Io(err),
        }
    }

    /// Convert filesystem metadata to `FileInfo`.
    fn metadata_to_info(metadata: &std::fs::Metadata) -> FileInfo {
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| unix_secs_to_u32(d.as_secs()));

        let atime = metadata
            .accessed()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(mtime, |d| unix_secs_to_u32(d.as_secs()));

        #[cfg(unix)]
        let (permissions, uid, gid) = {
            use std::os::unix::fs::MetadataExt;
            (metadata.mode(), metadata.uid(), metadata.gid())
        };

        #[cfg(not(unix))]
        let (permissions, uid, gid) = {
            if metadata.is_dir() {
                (0o755, 1000, 1000)
            } else {
                (0o644, 1000, 1000)
            }
        };

        FileInfo {
            size: metadata.len(),
            is_dir: metadata.is_dir(),
            permissions,
            mtime,
            atime,
            uid,
            gid,
        }
    }
}

#[async_trait]
impl Backend for LocalBackend {
    async fn list_dir(&self, path: &str) -> BackendResult<Vec<DirEntry>> {
        let full_path = self.full_path(path)?;

        debug!(path = %full_path.display(), "Listing directory");

        let mut entries = vec![
            DirEntry {
                name: ".".to_string(),
                attrs: FileInfo::directory(),
            },
            DirEntry {
                name: "..".to_string(),
                attrs: FileInfo::directory(),
            },
        ];

        let mut read_dir = fs::read_dir(&full_path).await.map_err(Self::map_io_error)?;

        while let Some(entry) = read_dir.next_entry().await.map_err(Self::map_io_error)? {
            let name = entry.file_name().to_string_lossy().to_string();
            let metadata = entry.metadata().await.map_err(Self::map_io_error)?;
            let attrs = Self::metadata_to_info(&metadata);

            entries.push(DirEntry { name, attrs });
        }

        Ok(entries)
    }

    async fn file_info(&self, path: &str) -> BackendResult<FileInfo> {
        let full_path = self.full_path(path)?;

        debug!(path = %full_path.display(), "Getting file info");

        let metadata = fs::metadata(&full_path).await.map_err(Self::map_io_error)?;
        Ok(Self::metadata_to_info(&metadata))
    }

    async fn make_dir(&self, path: &str) -> BackendResult<()> {
        let full_path = self.full_path(path)?;

        debug!(path = %full_path.display(), "Creating directory");

        fs::create_dir(&full_path).await.map_err(Self::map_io_error)
    }

    async fn del_dir(&self, path: &str) -> BackendResult<()> {
        let full_path = self.full_path(path)?;

        debug!(path = %full_path.display(), "Removing directory");

        fs::remove_dir(&full_path).await.map_err(Self::map_io_error)
    }

    async fn delete(&self, path: &str) -> BackendResult<()> {
        let full_path = self.full_path(path)?;

        debug!(path = %full_path.display(), "Deleting file");

        fs::remove_file(&full_path)
            .await
            .map_err(Self::map_io_error)
    }

    async fn rename(&self, src: &str, dst: &str) -> BackendResult<()> {
        let src_path = self.full_path(src)?;
        let dst_path = self.full_path(dst)?;

        debug!(from = %src_path.display(), to = %dst_path.display(), "Renaming");

        fs::rename(&src_path, &dst_path)
            .await
            .map_err(Self::map_io_error)
    }

    async fn read_file(&self, path: &str) -> BackendResult<Bytes> {
        let full_path = self.full_path(path)?;

        debug!(path = %full_path.display(), "Reading file");

        let content = fs::read(&full_path).await.map_err(Self::map_io_error)?;
        Ok(Bytes::from(content))
    }

    async fn write_file(&self, path: &str, content: Bytes) -> BackendResult<()> {
        let full_path = self.full_path(path)?;

        debug!(path = %full_path.display(), len = content.len(), "Writing file");

        fs::write(&full_path, &content)
            .await
            .map_err(Self::map_io_error)
    }

    async fn open_read(&self, path: &str) -> BackendResult<Box<dyn ReadHandle>> {
        let full_path = self.full_path(path)?;

        debug!(path = %full_path.display(), "Opening file for read");

        let file = File::open(&full_path).await.map_err(Self::map_io_error)?;
        let metadata = file.metadata().await.map_err(Self::map_io_error)?;
        let size = metadata.len();

        Ok(Box::new(LocalReadHandle {
            inner: Mutex::new(LocalReadHandleInner {
                file,
                buf: bytes::BytesMut::with_capacity(64 * 1024), // Pre-allocate typical read size
            }),
            size,
        }))
    }

    async fn open_write(&self, path: &str) -> BackendResult<Box<dyn WriteHandle + Send>> {
        let full_path = self.full_path(path)?;

        debug!(path = %full_path.display(), "Opening file for write");

        let file = File::create(&full_path).await.map_err(Self::map_io_error)?;

        Ok(Box::new(LocalWriteHandle { file }))
    }
}

/// Read handle for local filesystem - uses seek + read for random access.
/// Includes a reusable buffer to avoid repeated allocations and page faults.
struct LocalReadHandle {
    /// File and buffer are combined in the mutex to ensure synchronized access.
    inner: Mutex<LocalReadHandleInner>,
    size: u64,
}

struct LocalReadHandleInner {
    file: File,
    /// Reusable read buffer - cleared between reads but capacity is retained.
    buf: bytes::BytesMut,
}

#[async_trait]
impl ReadHandle for LocalReadHandle {
    async fn read_at(&self, offset: u64, len: u32) -> BackendResult<Bytes> {
        let mut inner = self.inner.lock().await;
        let LocalReadHandleInner { file, buf } = &mut *inner;

        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(LocalBackend::map_io_error)?;

        buf.clear();
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        buf.reserve(len);

        let bytes_read = file
            .read_buf(&mut buf.limit(len))
            .await
            .map_err(LocalBackend::map_io_error)?;

        Ok(buf.split_to(bytes_read).freeze())
    }

    fn size(&self) -> u64 {
        self.size
    }
}

/// Write handle for local filesystem - writes directly to file
struct LocalWriteHandle {
    file: File,
}

#[async_trait]
impl WriteHandle for LocalWriteHandle {
    async fn write_at(&mut self, offset: u64, data: Bytes) -> BackendResult<()> {
        self.file
            .seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(LocalBackend::map_io_error)?;
        self.file
            .write_all(&data)
            .await
            .map_err(LocalBackend::map_io_error)?;
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> BackendResult<()> {
        self.file
            .flush()
            .await
            .map_err(LocalBackend::map_io_error)?;
        Ok(())
    }

    async fn abort(self: Box<Self>) -> BackendResult<()> {
        // File will be closed on drop, but it may have partial content
        // For a cleaner abort, we'd need to track the path and delete the file
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_write_and_read_file() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        let content = Bytes::from_static(b"hello world");
        backend
            .write_file("test.txt", content.clone())
            .await
            .unwrap();
        let read = backend.read_file("test.txt").await.unwrap();
        assert_eq!(read, content);
    }

    #[tokio::test]
    async fn test_open_read_respects_requested_length() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());
        let content = Bytes::from(vec![42; 65_536]);

        backend
            .write_file("test.bin", content.clone())
            .await
            .unwrap();

        let handle = backend.open_read("test.bin").await.unwrap();
        let read = handle.read_at(0, 32_768).await.unwrap();

        assert_eq!(read.len(), 32_768);
        assert_eq!(read.as_ref(), &content[..32_768]);
    }

    #[tokio::test]
    async fn test_list_dir() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend
            .write_file("file1.txt", Bytes::from_static(b"a"))
            .await
            .unwrap();
        backend
            .write_file("file2.txt", Bytes::from_static(b"b"))
            .await
            .unwrap();

        let entries = backend.list_dir("/").await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"."));
        assert!(names.contains(&".."));
        assert!(names.contains(&"file1.txt"));
        assert!(names.contains(&"file2.txt"));
    }

    #[tokio::test]
    async fn test_make_and_list_dir() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend.make_dir("subdir").await.unwrap();

        let entries = backend.list_dir("/").await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"subdir"));

        let info = backend.file_info("subdir").await.unwrap();
        assert!(info.is_dir);
    }

    #[tokio::test]
    async fn test_delete_file() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend
            .write_file("test.txt", Bytes::from_static(b"data"))
            .await
            .unwrap();
        backend.delete("test.txt").await.unwrap();
        let result = backend.read_file("test.txt").await;
        assert!(matches!(result, Err(BackendError::NotFound)));
    }

    #[tokio::test]
    async fn test_rejects_parent_directory_traversal() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());
        let outside_name = format!(
            "{}-outside.txt",
            temp_dir.path().file_name().unwrap().to_string_lossy()
        );

        let result = backend
            .write_file(&format!("../{outside_name}"), Bytes::from_static(b"nope"))
            .await;

        assert!(matches!(result, Err(BackendError::PermissionDenied)));
        assert!(
            !temp_dir
                .path()
                .parent()
                .unwrap()
                .join(outside_name)
                .exists(),
            "traversal must not create files outside the backend root"
        );
    }

    #[tokio::test]
    async fn test_rejects_traversal_after_normal_component() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend.make_dir("safe").await.unwrap();
        let result = backend
            .write_file("safe/../../outside.txt", Bytes::from_static(b"nope"))
            .await;

        assert!(matches!(result, Err(BackendError::PermissionDenied)));
    }

    #[tokio::test]
    async fn test_allows_parent_directory_segments_within_root() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend.make_dir("safe").await.unwrap();
        backend
            .write_file("safe/../root.txt", Bytes::from_static(b"ok"))
            .await
            .unwrap();

        let read = backend.read_file("root.txt").await.unwrap();
        assert_eq!(read, Bytes::from_static(b"ok"));
    }

    #[tokio::test]
    async fn test_allows_multiple_in_root_parent_segments() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend.make_dir("a").await.unwrap();
        backend.make_dir("a/b").await.unwrap();
        backend
            .write_file("a/b/../c.txt", Bytes::from_static(b"nested"))
            .await
            .unwrap();

        let read = backend.read_file("a/c.txt").await.unwrap();
        assert_eq!(read, Bytes::from_static(b"nested"));
    }

    #[tokio::test]
    async fn test_current_dir_segments_are_ignored() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend.make_dir("safe").await.unwrap();
        backend
            .write_file("safe/./file.txt", Bytes::from_static(b"dot"))
            .await
            .unwrap();

        let read = backend.read_file("safe/file.txt").await.unwrap();
        assert_eq!(read, Bytes::from_static(b"dot"));
    }

    #[tokio::test]
    async fn test_rejects_traversal_with_multiple_parent_segments() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend.make_dir("a").await.unwrap();
        backend.make_dir("a/b").await.unwrap();
        let result = backend
            .write_file("a/b/../../../outside.txt", Bytes::from_static(b"nope"))
            .await;

        assert!(matches!(result, Err(BackendError::PermissionDenied)));
        assert!(!temp_dir.path().join("outside.txt").exists());
    }

    #[tokio::test]
    async fn test_rename_with_in_root_parent_segments() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend.make_dir("a").await.unwrap();
        backend.make_dir("a/b").await.unwrap();
        backend
            .write_file("a/b/source.txt", Bytes::from_static(b"rename"))
            .await
            .unwrap();

        backend
            .rename("a/b/source.txt", "a/b/../dest.txt")
            .await
            .unwrap();

        assert!(matches!(
            backend.read_file("a/b/source.txt").await,
            Err(BackendError::NotFound)
        ));
        let read = backend.read_file("a/dest.txt").await.unwrap();
        assert_eq!(read, Bytes::from_static(b"rename"));
    }

    #[tokio::test]
    async fn test_list_dir_with_in_root_parent_segment() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend.make_dir("a").await.unwrap();
        backend.make_dir("a/b").await.unwrap();
        backend
            .write_file("a/file.txt", Bytes::from_static(b"list"))
            .await
            .unwrap();

        let entries = backend.list_dir("a/b/..").await.unwrap();
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains(&"file.txt"));
        assert!(names.contains(&"b"));
    }

    #[tokio::test]
    async fn test_rename() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        let content = Bytes::from_static(b"data");
        backend
            .write_file("old.txt", content.clone())
            .await
            .unwrap();
        backend.rename("old.txt", "new.txt").await.unwrap();

        let read = backend.read_file("new.txt").await.unwrap();
        assert_eq!(read, content);

        let old_result = backend.read_file("old.txt").await;
        assert!(matches!(old_result, Err(BackendError::NotFound)));
    }

    proptest! {
        #[test]
        fn prop_write_read_roundtrip(
            filename in "[a-z][a-z0-9_]{0,10}\\.txt",
            content in prop::collection::vec(any::<u8>(), 0..1024)
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let temp_dir = TempDir::new().unwrap();
                let backend = LocalBackend::new(temp_dir.path());
                let bytes = Bytes::from(content.clone());
                backend.write_file(&filename, bytes).await.unwrap();
                let read = backend.read_file(&filename).await.unwrap();
                prop_assert_eq!(read.as_ref(), content.as_slice());
                Ok(())
            })?
        }

        #[test]
        fn prop_mkdir_appears_in_listing(dirname in "[a-z][a-z0-9]{0,10}") {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let temp_dir = TempDir::new().unwrap();
                let backend = LocalBackend::new(temp_dir.path());
                backend.make_dir(&dirname).await.unwrap();
                let entries = backend.list_dir("/").await.unwrap();
                let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
                prop_assert!(names.contains(&dirname.as_str()));
                Ok(())
            })?
        }
    }
}
