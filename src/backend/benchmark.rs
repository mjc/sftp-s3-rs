use super::{
    current_timestamp, normalize_path, Backend, BackendCapabilities, BackendError, BackendResult,
    DirEntry, FileInfo, FileKind, ReadHandle, SetAttrs, WriteHandle,
};
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::RwLock;
use russh_sftp::protocol::DataPayload;
use std::collections::HashMap;
use std::sync::Arc;

const STATIC_ZERO_READ_CHUNK_SIZE: usize = 256 * 1024;
static STATIC_ZERO_READ_CHUNK: [u8; STATIC_ZERO_READ_CHUNK_SIZE] = [0; STATIC_ZERO_READ_CHUNK_SIZE];

#[derive(Debug, Clone)]
struct Entry {
    size: u64,
    permissions: u32,
    mtime: u32,
    atime: u32,
    uid: u32,
    gid: u32,
}

impl Entry {
    fn file(size: u64) -> Self {
        let now = current_timestamp();
        Self {
            size,
            permissions: 0o644,
            mtime: now,
            atime: now,
            uid: 1000,
            gid: 1000,
        }
    }

    fn to_info(&self) -> FileInfo {
        FileInfo {
            size: self.size,
            kind: FileKind::File,
            is_dir: false,
            permissions: self.permissions,
            mtime: self.mtime,
            atime: self.atime,
            uid: self.uid,
            gid: self.gid,
        }
    }

    fn apply(&mut self, attrs: &SetAttrs) {
        if let Some(size) = attrs.size {
            self.size = size;
        }
        if let Some(permissions) = attrs.permissions {
            self.permissions = permissions;
        }
        if let Some(atime) = attrs.atime {
            self.atime = atime;
        }
        if let Some(mtime) = attrs.mtime {
            self.mtime = mtime;
        }
        if let Some(uid) = attrs.uid {
            self.uid = uid;
        }
        if let Some(gid) = attrs.gid {
            self.gid = gid;
        }
    }
}

/// Protocol benchmark backend.
///
/// This backend is intentionally not durable: uploads record only final file
/// size and metadata, and reads synthesize zero-filled buffers. It is useful
/// for large protocol throughput runs where retaining 50-100GiB in memory would
/// distort or kill the benchmark process.
pub struct BenchmarkBackend {
    files: Arc<RwLock<HashMap<String, Entry>>>,
}

impl Default for BenchmarkBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl BenchmarkBackend {
    pub fn new() -> Self {
        Self {
            files: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl Backend for BenchmarkBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            symlinks: false,
            set_attrs: true,
            delegated_safe_streaming_fallback: true,
        }
    }

    async fn list_dir(&self, path: &str) -> BackendResult<Vec<DirEntry>> {
        let normalized = normalize_path(path);
        if !normalized.is_empty() {
            return Err(BackendError::NotFound);
        }

        let files = self.files.read();
        let mut entries = Vec::with_capacity(files.len() + 2);
        entries.push(DirEntry {
            name: ".".to_string(),
            attrs: FileInfo::directory(),
        });
        entries.push(DirEntry {
            name: "..".to_string(),
            attrs: FileInfo::directory(),
        });
        for (path, entry) in files.iter() {
            if !path.contains('/') {
                entries.push(DirEntry {
                    name: path.clone(),
                    attrs: entry.to_info(),
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
        self.files
            .read()
            .get(normalized.as_ref())
            .map(Entry::to_info)
            .ok_or(BackendError::NotFound)
    }

    async fn make_dir(&self, path: &str) -> BackendResult<()> {
        if normalize_path(path).is_empty() {
            Ok(())
        } else {
            Err(BackendError::Unsupported)
        }
    }

    async fn del_dir(&self, path: &str) -> BackendResult<()> {
        if normalize_path(path).is_empty() {
            Ok(())
        } else {
            Err(BackendError::Unsupported)
        }
    }

    async fn delete(&self, path: &str) -> BackendResult<()> {
        let normalized = normalize_path(path);
        self.files
            .write()
            .remove(normalized.as_ref())
            .map(|_| ())
            .ok_or(BackendError::NotFound)
    }

    async fn rename(&self, src: &str, dst: &str) -> BackendResult<()> {
        let src = normalize_path(src);
        let dst = normalize_path(dst);
        let mut files = self.files.write();
        let entry = files.remove(src.as_ref()).ok_or(BackendError::NotFound)?;
        files.insert(dst.into_owned(), entry);
        Ok(())
    }

    async fn read_file(&self, path: &str) -> BackendResult<Bytes> {
        let info = self.file_info(path).await?;
        Ok(Bytes::from(vec![0; info.size as usize]))
    }

    async fn write_file(&self, path: &str, content: Bytes) -> BackendResult<()> {
        let normalized = normalize_path(path);
        self.files
            .write()
            .insert(normalized.into_owned(), Entry::file(content.len() as u64));
        Ok(())
    }

    async fn open_read(&self, path: &str) -> BackendResult<Box<dyn ReadHandle>> {
        let info = self.file_info(path).await?;
        Ok(Box::new(BenchmarkReadHandle { size: info.size }))
    }

    async fn open_write(&self, path: &str) -> BackendResult<Box<dyn WriteHandle + Send>> {
        Ok(Box::new(BenchmarkWriteHandle {
            path: normalize_path(path).into_owned(),
            size: 0,
            files: Arc::clone(&self.files),
        }))
    }

    async fn set_attrs(&self, path: &str, attrs: SetAttrs) -> BackendResult<()> {
        let normalized = normalize_path(path);
        let mut files = self.files.write();
        let entry = files
            .get_mut(normalized.as_ref())
            .ok_or(BackendError::NotFound)?;
        entry.apply(&attrs);
        Ok(())
    }
}

struct BenchmarkReadHandle {
    size: u64,
}

#[async_trait]
impl ReadHandle for BenchmarkReadHandle {
    fn try_read_at(&self, offset: u64, len: u32) -> Option<BackendResult<DataPayload>> {
        if offset >= self.size {
            return Some(Ok(Bytes::new().into()));
        }
        let len = (self.size - offset).min(u64::from(len)) as usize;
        if len <= STATIC_ZERO_READ_CHUNK.len() {
            Some(Ok(Bytes::from_static(&STATIC_ZERO_READ_CHUNK[..len]).into()))
        } else {
            Some(Ok(Bytes::from(vec![0; len]).into()))
        }
    }

    async fn read_at(&self, offset: u64, len: u32) -> BackendResult<DataPayload> {
        if offset >= self.size {
            return Ok(Bytes::new().into());
        }
        let len = (self.size - offset).min(u64::from(len)) as usize;
        if len <= STATIC_ZERO_READ_CHUNK.len() {
            Ok(Bytes::from_static(&STATIC_ZERO_READ_CHUNK[..len]).into())
        } else {
            Ok(Bytes::from(vec![0; len]).into())
        }
    }

    fn size(&self) -> u64 {
        self.size
    }
}

struct BenchmarkWriteHandle {
    path: String,
    size: u64,
    files: Arc<RwLock<HashMap<String, Entry>>>,
}

#[async_trait]
impl WriteHandle for BenchmarkWriteHandle {
    fn try_write_at(&mut self, offset: u64, data: &Bytes) -> Option<BackendResult<()>> {
        let end = match offset.checked_add(data.len() as u64) {
            Some(end) => end,
            None => {
                return Some(Err(BackendError::Other(
                    "benchmark write length overflow".to_owned(),
                )));
            }
        };
        self.size = self.size.max(end);
        Some(Ok(()))
    }

    async fn write_at(&mut self, offset: u64, data: Bytes) -> BackendResult<()> {
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| BackendError::Other("benchmark write length overflow".to_owned()))?;
        self.size = self.size.max(end);
        Ok(())
    }

    async fn finish(self: Box<Self>) -> BackendResult<()> {
        self.files.write().insert(self.path, Entry::file(self.size));
        Ok(())
    }

    async fn abort(self: Box<Self>) -> BackendResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn benchmark_backend_records_size_without_storing_payload() {
        let backend = BenchmarkBackend::new();
        let mut writer = backend.open_write("large.bin").await.unwrap();
        writer
            .write_at(1024 * 1024, Bytes::from_static(b"x"))
            .await
            .unwrap();
        writer.finish().await.unwrap();

        let info = backend.file_info("large.bin").await.unwrap();
        assert_eq!(info.size, 1024 * 1024 + 1);

        let reader = backend.open_read("large.bin").await.unwrap();
        let data = reader.read_at(1024 * 1024, 8).await.unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data.as_ref(), &[0]);
    }

    #[tokio::test]
    async fn benchmark_backend_tracks_largest_written_end_offset() {
        let backend = BenchmarkBackend::new();
        let mut writer = backend.open_write("sparse.bin").await.unwrap();
        writer
            .write_at(0, Bytes::from_static(b"abc"))
            .await
            .unwrap();
        writer
            .write_at(4096, Bytes::from_static(b"z"))
            .await
            .unwrap();
        writer.finish().await.unwrap();

        let info = backend.file_info("sparse.bin").await.unwrap();
        assert_eq!(info.size, 4097);
    }

    #[tokio::test]
    async fn benchmark_backend_list_dir_includes_dot_entries() {
        let backend = BenchmarkBackend::new();
        backend
            .write_file("file.bin", Bytes::from_static(b"x"))
            .await
            .unwrap();

        let entries = backend.list_dir("").await.unwrap();
        assert_eq!(entries[0].name, ".");
        assert_eq!(entries[1].name, "..");
        assert!(entries.iter().any(|entry| entry.name == "file.bin"));
    }

    #[tokio::test]
    async fn benchmark_backend_delete_missing_file_returns_not_found() {
        let backend = BenchmarkBackend::new();
        let result = backend.delete("missing.bin").await;
        assert!(matches!(result, Err(BackendError::NotFound)));
    }
}
