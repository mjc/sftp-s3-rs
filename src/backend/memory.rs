use super::{normalize_path, Backend, BackendError, BackendResult, DirEntry, FileInfo};
use async_trait::async_trait;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};

const KEEP_MARKER: &str = ".keep";

/// File data stored in memory
#[derive(Debug, Clone)]
struct FileData {
    content: Vec<u8>,
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
    pub fn with_files(files: HashMap<String, Vec<u8>>) -> Self {
        let mtime = super::current_timestamp();
        let files = files
            .into_iter()
            .map(|(k, content)| (k, FileData { content, mtime }))
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
        if let Some(data) = files.get(&normalized) {
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
                content: Vec::new(),
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
        self.files.write().remove(&normalize_path(path));
        Ok(())
    }

    async fn rename(&self, src: &str, dst: &str) -> BackendResult<()> {
        let src_key = normalize_path(src);
        let dst_key = normalize_path(dst);

        let mut files = self.files.write();
        if let Some(data) = files.remove(&src_key) {
            files.insert(dst_key, data);
        }
        Ok(())
    }

    async fn read_file(&self, path: &str) -> BackendResult<Vec<u8>> {
        let normalized = normalize_path(path);
        self.files
            .read()
            .get(&normalized)
            .map(|d| d.content.clone())
            .ok_or(BackendError::NotFound)
    }

    async fn write_file(&self, path: &str, content: Vec<u8>) -> BackendResult<()> {
        self.files.write().insert(
            normalize_path(path),
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

    #[tokio::test]
    async fn test_write_and_read_file() {
        let backend = MemoryBackend::new();
        let content = b"hello world".to_vec();

        backend.write_file("test.txt", content.clone()).await.unwrap();
        let read = backend.read_file("test.txt").await.unwrap();

        assert_eq!(read, content);
    }

    #[tokio::test]
    async fn test_list_root() {
        let backend = MemoryBackend::new();
        backend.write_file("file1.txt", b"a".to_vec()).await.unwrap();
        backend.write_file("file2.txt", b"b".to_vec()).await.unwrap();

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
        backend.write_file("test.txt", b"12345".to_vec()).await.unwrap();

        let info = backend.file_info("test.txt").await.unwrap();
        assert!(!info.is_dir);
        assert_eq!(info.size, 5);

        let root_info = backend.file_info("/").await.unwrap();
        assert!(root_info.is_dir);
    }
}
