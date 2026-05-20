# Custom Backends

This guide explains how to implement storage for `sftp-s3`.

## Backend Model

`sftp-s3` asks a backend to expose a filesystem-like contract. The underlying
storage can be a real directory, object storage, a database, an API, or a
service in your application.

Your backend does not need to be a real filesystem, but it should return
filesystem-like results for SFTP operations.

## Required Methods

Every backend implements:

- `list_dir`
- `file_info`
- `make_dir`
- `del_dir`
- `delete`
- `rename`
- `read_file`
- `write_file`
- `open_read`
- `open_write`

The whole-file methods are convenient for simple operations. The streaming
methods are used by the SFTP handler for normal file transfers.

## Minimal In-Memory Shape

```rust
use async_trait::async_trait;
use bytes::Bytes;
use sftp_s3::backend::{
    Backend, BackendError, BackendResult, BufferedReadHandle, DirEntry, FileInfo, ReadHandle,
    WriteHandle,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

struct ExampleBackend {
    files: Arc<Mutex<HashMap<String, Bytes>>>,
}

impl ExampleBackend {
    fn new() -> Self {
        Self {
            files: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl Backend for ExampleBackend {
    async fn list_dir(&self, _path: &str) -> BackendResult<Vec<DirEntry>> {
        Ok(vec![
            DirEntry {
                name: ".".into(),
                attrs: FileInfo::directory(),
            },
            DirEntry {
                name: "..".into(),
                attrs: FileInfo::directory(),
            },
        ])
    }

    async fn file_info(&self, path: &str) -> BackendResult<FileInfo> {
        if path.is_empty() || path == "/" {
            return Ok(FileInfo::directory());
        }

        let files = self.files.lock().await;
        let content = files.get(path).ok_or(BackendError::NotFound)?;
        Ok(FileInfo::file(content.len() as u64))
    }

    async fn make_dir(&self, _path: &str) -> BackendResult<()> {
        Ok(())
    }

    async fn del_dir(&self, _path: &str) -> BackendResult<()> {
        Ok(())
    }

    async fn delete(&self, path: &str) -> BackendResult<()> {
        self.files.lock().await.remove(path);
        Ok(())
    }

    async fn rename(&self, src: &str, dst: &str) -> BackendResult<()> {
        let mut files = self.files.lock().await;
        let content = files.remove(src).ok_or(BackendError::NotFound)?;
        files.insert(dst.to_string(), content);
        Ok(())
    }

    async fn read_file(&self, path: &str) -> BackendResult<Bytes> {
        self.files
            .lock()
            .await
            .get(path)
            .cloned()
            .ok_or(BackendError::NotFound)
    }

    async fn write_file(&self, path: &str, content: Bytes) -> BackendResult<()> {
        self.files.lock().await.insert(path.to_string(), content);
        Ok(())
    }

    async fn open_read(&self, path: &str) -> BackendResult<Box<dyn ReadHandle>> {
        Ok(Box::new(BufferedReadHandle::new(self.read_file(path).await?)))
    }

    async fn open_write(&self, path: &str) -> BackendResult<Box<dyn WriteHandle + Send>> {
        Ok(Box::new(ExampleWriteHandle {
            path: path.to_string(),
            files: self.files.clone(),
            buffer: Vec::new(),
        }))
    }
}

struct ExampleWriteHandle {
    path: String,
    files: Arc<Mutex<HashMap<String, Bytes>>>,
    buffer: Vec<u8>,
}

#[async_trait]
impl WriteHandle for ExampleWriteHandle {
    async fn write_at(&mut self, offset: u64, data: Bytes) -> BackendResult<()> {
        let offset = offset as usize;
        if self.buffer.len() < offset {
            self.buffer.resize(offset, 0);
        }
        if self.buffer.len() < offset + data.len() {
            self.buffer.resize(offset + data.len(), 0);
        }
        self.buffer[offset..offset + data.len()].copy_from_slice(&data);
        Ok(())
    }

    async fn finish(self: Box<Self>) -> BackendResult<()> {
        self.files
            .lock()
            .await
            .insert(self.path, Bytes::from(self.buffer));
        Ok(())
    }

    async fn abort(self: Box<Self>) -> BackendResult<()> {
        Ok(())
    }
}
```

## Path Handling

SFTP clients commonly send paths with leading slashes. Use `normalize_path` for
key-like stores:

```rust
use sftp_s3::backend::normalize_path;

let key = normalize_path("/folder/file.txt");
assert_eq!(key.as_ref(), "folder/file.txt");
```

For filesystem-backed stores, reject traversal before touching disk.
`LocalBackend` is the reference implementation for this pattern. For key-like
stores, decide whether `..` is a root path variant or a valid key segment and
document that choice.

## Directory Listings

`list_dir` should return immediate children only. Include `.` and `..` even if
your storage does not have real directory entries.

Object stores usually need to model directories by convention. The built-in S3
backend uses delimiter listings and `.keep` markers for empty directories.

## Error Mapping

Return the most specific `BackendError` you can:

- `NotFound` for missing files and directories
- `PermissionDenied` for auth, policy, or traversal failures
- `AlreadyExists` when creation cannot overwrite
- `NotADirectory` and `IsADirectory` for type mismatches
- `DirectoryNotEmpty` for non-empty directory deletion
- `Io` for local I/O errors
- `Other` for backend-specific failures

The SFTP handler maps these variants into client-visible SFTP status codes.

## Streaming Writes

`WriteHandle::write_at` may receive offsets. Some clients write sequentially,
but random, overlapping, or retry writes are possible. If your storage supports
random writes, make overlapping chunks overwrite the addressed byte range. If
your storage only supports append or multipart uploads, validate offsets and
return a stable error instead of silently corrupting data.

Callers commit uploads with `finish`. Use `abort` to clean up temporary files,
multipart uploads, or in-progress state.

## Post-Write Processing

If uploads need follow-up work, commit the file first and enqueue work after the
backend has accepted the data. From the SFTP client's perspective, a successful
write should mean the data is durably accepted by your storage boundary.
