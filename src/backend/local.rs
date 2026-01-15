use super::{normalize_path, Backend, BackendError, BackendResult, DirEntry, FileInfo};
use async_trait::async_trait;
use bytes::Bytes;
use std::path::{Path, PathBuf};
use tokio::fs;
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

    /// Get the full filesystem path for a normalized SFTP path
    fn full_path(&self, path: &str) -> PathBuf {
        if path.is_empty() {
            self.root.clone()
        } else {
            self.root.join(path)
        }
    }

    /// Convert std::io::Error to BackendError
    fn map_io_error(err: std::io::Error) -> BackendError {
        match err.kind() {
            std::io::ErrorKind::NotFound => BackendError::NotFound,
            std::io::ErrorKind::PermissionDenied => BackendError::PermissionDenied,
            std::io::ErrorKind::AlreadyExists => BackendError::AlreadyExists,
            std::io::ErrorKind::DirectoryNotEmpty => BackendError::DirectoryNotEmpty,
            std::io::ErrorKind::IsADirectory => BackendError::IsADirectory,
            _ => BackendError::Io(err.to_string()),
        }
    }

    /// Convert filesystem metadata to FileInfo
    fn metadata_to_info(metadata: &std::fs::Metadata) -> FileInfo {
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);

        let atime = metadata
            .accessed()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as u32)
            .unwrap_or(mtime);

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
        let normalized = normalize_path(path);
        let full_path = self.full_path(&normalized);

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
        let normalized = normalize_path(path);
        let full_path = self.full_path(&normalized);

        debug!(path = %full_path.display(), "Getting file info");

        let metadata = fs::metadata(&full_path).await.map_err(Self::map_io_error)?;
        Ok(Self::metadata_to_info(&metadata))
    }

    async fn make_dir(&self, path: &str) -> BackendResult<()> {
        let normalized = normalize_path(path);
        let full_path = self.full_path(&normalized);

        debug!(path = %full_path.display(), "Creating directory");

        fs::create_dir(&full_path).await.map_err(Self::map_io_error)
    }

    async fn del_dir(&self, path: &str) -> BackendResult<()> {
        let normalized = normalize_path(path);
        let full_path = self.full_path(&normalized);

        debug!(path = %full_path.display(), "Removing directory");

        fs::remove_dir(&full_path).await.map_err(Self::map_io_error)
    }

    async fn delete(&self, path: &str) -> BackendResult<()> {
        let normalized = normalize_path(path);
        let full_path = self.full_path(&normalized);

        debug!(path = %full_path.display(), "Deleting file");

        fs::remove_file(&full_path)
            .await
            .map_err(Self::map_io_error)
    }

    async fn rename(&self, src: &str, dst: &str) -> BackendResult<()> {
        let src_path = self.full_path(&normalize_path(src));
        let dst_path = self.full_path(&normalize_path(dst));

        debug!(from = %src_path.display(), to = %dst_path.display(), "Renaming");

        fs::rename(&src_path, &dst_path)
            .await
            .map_err(Self::map_io_error)
    }

    async fn read_file(&self, path: &str) -> BackendResult<Bytes> {
        let normalized = normalize_path(path);
        let full_path = self.full_path(&normalized);

        debug!(path = %full_path.display(), "Reading file");

        let content = fs::read(&full_path).await.map_err(Self::map_io_error)?;
        Ok(Bytes::from(content))
    }

    async fn write_file(&self, path: &str, content: Bytes) -> BackendResult<()> {
        let normalized = normalize_path(path);
        let full_path = self.full_path(&normalized);

        debug!(path = %full_path.display(), len = content.len(), "Writing file");

        fs::write(&full_path, &content)
            .await
            .map_err(Self::map_io_error)
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
