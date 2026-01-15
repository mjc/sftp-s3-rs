use super::{normalize_path, Backend, BackendError, BackendResult, DirEntry, FileInfo};
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};

const KEEP_MARKER: &str = ".keep";

/// File data stored in memory
#[derive(Debug, Clone)]
struct FileData {
    content: Bytes,
    mtime: u32,
}

/// In-memory storage backend for testing and development
pub struct MemoryBackend {
    files: RwLock<HashMap<String, FileData>>,
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self {
            files: RwLock::new(HashMap::new()),
        }
    }

    /// Create with pre-populated files
    pub fn with_files(files: HashMap<String, impl Into<Bytes>>) -> Self {
        let mtime = super::current_timestamp();
        let files = files
            .into_iter()
            .map(|(k, content)| {
                (
                    k,
                    FileData {
                        content: content.into(),
                        mtime,
                    },
                )
            })
            .collect();
        Self {
            files: RwLock::new(files),
        }
    }
}

#[async_trait]
impl Backend for MemoryBackend {
    async fn list_dir(&self, path: &str) -> BackendResult<Vec<DirEntry>> {
        let normalized = normalize_path(path);
        let prefix = if normalized.is_empty() {
            String::new()
        } else {
            format!("{}/", normalized)
        };

        let files = self.files.read();
        let mut seen = HashSet::new();
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

        for (key, data) in files.iter() {
            let relative = if prefix.is_empty() {
                key.as_str()
            } else if let Some(stripped) = key.strip_prefix(&prefix) {
                stripped
            } else {
                continue;
            };

            // Get first path component
            let name = relative.split('/').next().unwrap_or(relative);

            if name.is_empty() || name == KEEP_MARKER {
                continue;
            }

            if seen.insert(name.to_string()) {
                let is_dir = relative.contains('/');
                let attrs = if is_dir {
                    FileInfo::directory_with_mtime(data.mtime)
                } else {
                    FileInfo::file_with_mtime(data.content.len() as u64, data.mtime)
                };

                entries.push(DirEntry {
                    name: name.to_string(),
                    attrs,
                });
            }
        }

        Ok(entries)
    }

    async fn file_info(&self, path: &str) -> BackendResult<FileInfo> {
        let normalized = normalize_path(path);

        if normalized.is_empty() {
            return Ok(FileInfo::directory());
        }

        let files = self.files.read();

        // Check if it's a file
        if let Some(data) = files.get(normalized.as_ref()) {
            return Ok(FileInfo::file_with_mtime(
                data.content.len() as u64,
                data.mtime,
            ));
        }

        // Check if it's a directory
        let prefix = format!("{}/", normalized);
        if files.keys().any(|k| k.starts_with(&prefix)) {
            return Ok(FileInfo::directory());
        }

        Err(BackendError::NotFound)
    }

    async fn make_dir(&self, path: &str) -> BackendResult<()> {
        let key = format!("{}/{}", normalize_path(path), KEEP_MARKER);
        self.files.write().insert(
            key,
            FileData {
                content: Bytes::new(),
                mtime: super::current_timestamp(),
            },
        );
        Ok(())
    }

    async fn del_dir(&self, path: &str) -> BackendResult<()> {
        let key = format!("{}/{}", normalize_path(path), KEEP_MARKER);
        self.files.write().remove(&key);
        Ok(())
    }

    async fn delete(&self, path: &str) -> BackendResult<()> {
        self.files.write().remove(normalize_path(path).as_ref());
        Ok(())
    }

    async fn rename(&self, src: &str, dst: &str) -> BackendResult<()> {
        let src_key = normalize_path(src);
        let dst_key = normalize_path(dst);

        let mut files = self.files.write();
        if let Some(data) = files.remove(src_key.as_ref()) {
            files.insert(dst_key.into_owned(), data);
        }
        Ok(())
    }

    async fn read_file(&self, path: &str) -> BackendResult<Bytes> {
        let normalized = normalize_path(path);
        self.files
            .read()
            .get(normalized.as_ref())
            .map(|d| d.content.clone()) // Bytes clone is O(1)
            .ok_or(BackendError::NotFound)
    }

    async fn write_file(&self, path: &str, content: Bytes) -> BackendResult<()> {
        self.files.write().insert(
            normalize_path(path).into_owned(),
            FileData {
                content,
                mtime: super::current_timestamp(),
            },
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::Arc;

    // Unit tests
    #[tokio::test]
    async fn test_write_and_read_file() {
        let backend = MemoryBackend::new();
        let content = Bytes::from_static(b"hello world");

        backend
            .write_file("test.txt", content.clone())
            .await
            .unwrap();
        let read = backend.read_file("test.txt").await.unwrap();

        assert_eq!(read, content);
    }

    #[tokio::test]
    async fn test_list_root() {
        let backend = MemoryBackend::new();
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
        let backend = MemoryBackend::new();
        backend.make_dir("subdir").await.unwrap();

        let entries = backend.list_dir("/").await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"subdir"));
    }

    #[tokio::test]
    async fn test_file_info() {
        let backend = MemoryBackend::new();
        backend
            .write_file("test.txt", Bytes::from_static(b"12345"))
            .await
            .unwrap();

        let info = backend.file_info("test.txt").await.unwrap();
        assert!(!info.is_dir);
        assert_eq!(info.size, 5);

        let root_info = backend.file_info("/").await.unwrap();
        assert!(root_info.is_dir);
    }

    // Concurrent access test
    #[tokio::test]
    async fn test_concurrent_writes() {
        use futures::future::join_all;

        let backend = Arc::new(MemoryBackend::new());
        let tasks: Vec<_> = (0..100)
            .map(|i| {
                let b = backend.clone();
                tokio::spawn(async move {
                    let content = Bytes::from(vec![i as u8; 100]);
                    b.write_file(&format!("file{}", i), content).await
                })
            })
            .collect();

        let results = join_all(tasks).await;
        assert!(results.iter().all(|r| r.is_ok()));

        // Verify all files exist with correct content
        for i in 0..100u8 {
            let content = backend.read_file(&format!("file{}", i)).await.unwrap();
            assert_eq!(content.as_ref(), &vec![i; 100]);
        }
    }

    // Property tests
    proptest! {
        // Path normalization: idempotent
        #[test]
        fn prop_normalize_idempotent(path in ".*") {
            let once = normalize_path(&path);
            let twice = normalize_path(&once);
            prop_assert_eq!(once.as_ref(), twice.as_ref());
        }

        // Path normalization: no leading slash
        #[test]
        fn prop_normalize_no_leading_slash(path in ".*") {
            let result = normalize_path(&path);
            prop_assert!(!result.starts_with('/') || result.is_empty());
        }

        // Path normalization: no trailing slash
        #[test]
        fn prop_normalize_no_trailing_slash(path in ".*") {
            let result = normalize_path(&path);
            prop_assert!(!result.ends_with('/') || result.is_empty());
        }

        // Path normalization: root variants normalize to empty
        #[test]
        fn prop_root_normalizes_empty(slashes in "/+") {
            let result = normalize_path(&slashes);
            prop_assert!(result.is_empty());
        }

        // Write-then-read roundtrip
        #[test]
        fn prop_write_read_roundtrip(
            path in "[a-z][a-z0-9_]{0,15}(\\.[a-z]{1,4})?",
            content in prop::collection::vec(any::<u8>(), 0..1024)
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let backend = MemoryBackend::new();
                let bytes = Bytes::from(content.clone());
                backend.write_file(&path, bytes).await.unwrap();
                let read = backend.read_file(&path).await.unwrap();
                prop_assert_eq!(read.as_ref(), content.as_slice());
                Ok(())
            })?
        }

        // Delete then not found
        #[test]
        fn prop_delete_then_not_found(
            path in "[a-z][a-z0-9_]{0,15}",
            content in prop::collection::vec(any::<u8>(), 1..100)
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let backend = MemoryBackend::new();
                backend.write_file(&path, Bytes::from(content)).await.unwrap();
                backend.delete(&path).await.unwrap();
                let result = backend.read_file(&path).await;
                prop_assert!(matches!(result, Err(BackendError::NotFound)));
                Ok(())
            })?
        }

        // Rename preserves content
        #[test]
        fn prop_rename_preserves_content(
            src in "[a-z][a-z0-9]{0,10}",
            dst in "[a-z][a-z0-9]{0,10}",
            content in prop::collection::vec(any::<u8>(), 0..100)
        ) {
            prop_assume!(src != dst);
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let backend = MemoryBackend::new();
                backend.write_file(&src, Bytes::from(content.clone())).await.unwrap();
                backend.rename(&src, &dst).await.unwrap();
                let read = backend.read_file(&dst).await.unwrap();
                prop_assert_eq!(read.as_ref(), content.as_slice());
                let src_result = backend.read_file(&src).await;
                prop_assert!(matches!(src_result, Err(BackendError::NotFound)));
                Ok(())
            })?
        }

        // mkdir then appears in listing
        #[test]
        fn prop_mkdir_appears_in_listing(dirname in "[a-z][a-z0-9]{0,15}") {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let backend = MemoryBackend::new();
                backend.make_dir(&dirname).await.unwrap();
                let entries = backend.list_dir("/").await.unwrap();
                let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
                prop_assert!(names.contains(&dirname.as_str()));
                Ok(())
            })?
        }

        // deldir removes from listing
        #[test]
        fn prop_deldir_removes_from_listing(dirname in "[a-z][a-z0-9]{0,15}") {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let backend = MemoryBackend::new();
                backend.make_dir(&dirname).await.unwrap();
                backend.del_dir(&dirname).await.unwrap();
                let entries = backend.list_dir("/").await.unwrap();
                let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
                prop_assert!(!names.contains(&dirname.as_str()));
                Ok(())
            })?
        }

        // file_info size matches content
        #[test]
        fn prop_file_info_size(
            path in "[a-z][a-z0-9_]{0,15}",
            content in prop::collection::vec(any::<u8>(), 0..1024)
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let backend = MemoryBackend::new();
                let size = content.len() as u64;
                backend.write_file(&path, Bytes::from(content)).await.unwrap();
                let info = backend.file_info(&path).await.unwrap();
                prop_assert_eq!(info.size, size);
                prop_assert!(!info.is_dir);
                Ok(())
            })?
        }
    }
}
