use bytes::{Buf, Bytes};

type SftpHandle = Bytes;
type SftpWriteData = Bytes;

fn handle_as_bytes(h: &SftpHandle) -> &[u8] {
    h.as_ref()
}

fn write_data_into_bytes(data: SftpWriteData) -> Bytes {
    data
}

type SftpResponseData = Bytes;

fn bytes_into_response_data(b: Bytes) -> SftpResponseData {
    b
}

use crate::backend::{normalize_path, Backend, BackendError, FileInfo};
use crate::handle::{HandleInfo, HandleManager};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, instrument, warn};

// Unix file type bits for SFTP
const S_IFREG: u32 = 0o100_000; // Regular file
const S_IFDIR: u32 = 0o040_000; // Directory

/// Convert `FileInfo` to `russh_sftp` `FileAttributes`.
fn to_file_attributes(info: &FileInfo) -> FileAttributes {
    // SFTP requires file type bits in permissions
    let file_type = if info.is_dir { S_IFDIR } else { S_IFREG };
    let permissions = file_type | (info.permissions & 0o7777);

    FileAttributes {
        size: Some(info.size),
        permissions: Some(permissions),
        mtime: Some(info.mtime),
        atime: Some(info.atime),
        uid: Some(info.uid),
        gid: Some(info.gid),
        ..Default::default()
    }
}

/// SFTP session handler that delegates to a backend
pub struct SftpHandler<B: Backend> {
    backend: Arc<B>,
    handles: HandleManager,
}

impl<B: Backend> SftpHandler<B> {
    pub fn new(backend: Arc<B>) -> Self {
        Self {
            backend,
            handles: HandleManager::new(),
        }
    }
}

/// Convert `BackendError` to SFTP `StatusCode`.
impl From<BackendError> for StatusCode {
    fn from(err: BackendError) -> Self {
        match err {
            BackendError::NotFound | BackendError::NotADirectory => StatusCode::NoSuchFile,
            BackendError::PermissionDenied => StatusCode::PermissionDenied,
            BackendError::AlreadyExists
            | BackendError::IsADirectory
            | BackendError::DirectoryNotEmpty
            | BackendError::Io(_)
            | BackendError::Other(_) => StatusCode::Failure,
        }
    }
}

fn ok_status(id: u32) -> Status {
    // TODO(perf): Status.error_message/language_tag are String; change to Cow<'static, str>
    // in russh-sftp fork to eliminate 2 small allocations per successful operation.
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_string(),
        language_tag: "en".to_string(),
    }
}

impl<B: Backend> russh_sftp::server::Handler for SftpHandler<B> {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        debug!(version, "SFTP init");
        // Advertise extensions for better client compatibility
        let mut v = Version::new();
        v.extensions
            .insert("posix-rename@openssh.com".to_string(), "1".to_string());
        v.extensions
            .insert("fsync@openssh.com".to_string(), "1".to_string());
        v.extensions
            .insert("statvfs@openssh.com".to_string(), "2".to_string());
        Ok(v)
    }

    #[instrument(level = "debug", skip(self, handle), fields(handle = %String::from_utf8_lossy(&handle)))]
    async fn close(&mut self, id: u32, handle: SftpHandle) -> Result<Status, Self::Error> {
        let hb = handle_as_bytes(&handle);
        // If it's a write handle, finish it
        if let Some(write_handle_arc) = self.handles.take_write_handle(hb) {
            let mut guard = write_handle_arc.lock().await;
            if let Some(write_handle) = guard.take() {
                write_handle.finish().await.map_err(StatusCode::from)?;
            }
        }

        self.handles.remove(hb);
        Ok(ok_status(id))
    }

    #[instrument(level = "debug", skip(self))]
    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let normalized = normalize_path(&path);

        // Verify it's a directory
        let info = self
            .backend
            .file_info(&normalized)
            .await
            .map_err(StatusCode::from)?;

        if !info.is_dir {
            return Err(StatusCode::NoSuchFile);
        }

        let handle = self.handles.create_dir_handle(normalized.as_ref());
        Ok(Handle {
            id,
            handle: handle.into(),
        })
    }

    #[instrument(level = "debug", skip(self, handle), fields(handle = %String::from_utf8_lossy(&handle)))]
    async fn readdir(&mut self, id: u32, handle: SftpHandle) -> Result<Name, Self::Error> {
        let hb = handle_as_bytes(&handle);
        let (path, read_done) = self.handles.get_dir_handle(hb).ok_or(StatusCode::Failure)?;

        if read_done {
            return Err(StatusCode::Eof);
        }

        let entries = self
            .backend
            .list_dir(&path)
            .await
            .map_err(StatusCode::from)?;

        // Mark as read
        self.handles.mark_dir_read(hb);

        let files: Vec<File> = entries
            .into_iter()
            .map(|entry| File {
                filename: entry.name,
                longname: String::new(),
                attrs: to_file_attributes(&entry.attrs),
            })
            .collect();

        Ok(Name { id, files })
    }

    #[instrument(level = "debug", skip(self, _attrs), fields(write = pflags.contains(OpenFlags::WRITE), create = pflags.contains(OpenFlags::CREATE)))]
    async fn open(
        &mut self,
        id: u32,
        path: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let normalized = normalize_path(&path);

        // Treat CREATE as implying write mode (some clients send CREATE without WRITE)
        let is_write = pflags.contains(OpenFlags::WRITE) || pflags.contains(OpenFlags::CREATE);

        let handle = if is_write {
            // Write mode: open streaming write handle
            match self.backend.open_write(&normalized).await {
                Ok(write_handle) => {
                    let h = self
                        .handles
                        .create_write_handle(normalized.as_ref(), write_handle);
                    debug!(id, path = %path, handle = %h, "Opened file for write");
                    h
                }
                Err(e) => {
                    warn!(id, path = %path, error = %e, "Failed to open file for write");
                    return Err(StatusCode::from(e));
                }
            }
        } else {
            // Read mode: open streaming read handle
            match self.backend.open_read(&normalized).await {
                Ok(read_handle) => {
                    let h = self
                        .handles
                        .create_read_handle(normalized.as_ref(), read_handle);
                    debug!(id, path = %path, handle = %h, "Opened file for read");
                    h
                }
                Err(e) => {
                    warn!(id, path = %path, error = %e, "Failed to open file for read");
                    return Err(StatusCode::from(e));
                }
            }
        };

        Ok(Handle {
            id,
            handle: handle.into(),
        })
    }

    #[instrument(level = "debug", skip(self, handle), fields(handle = %String::from_utf8_lossy(&handle)))]
    async fn read(
        &mut self,
        id: u32,
        handle: SftpHandle,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let (_path, read_handle, size) = self
            .handles
            .get_read_handle(handle_as_bytes(&handle))
            .ok_or(StatusCode::Failure)?;

        if offset >= size {
            return Err(StatusCode::Eof);
        }

        let guard = read_handle.lock().await;
        let data = guard.read_at(offset, len).await.map_err(StatusCode::from)?;

        if data.is_empty() {
            return Err(StatusCode::Eof);
        }

        Ok(Data {
            id,
            data: bytes_into_response_data(data),
        })
    }

    #[instrument(level = "debug", skip(self, handle, data), fields(handle = %String::from_utf8_lossy(&handle), len = data.len()))]
    async fn write(
        &mut self,
        id: u32,
        handle: SftpHandle,
        offset: u64,
        data: SftpWriteData,
    ) -> Result<Status, Self::Error> {
        let (_path, write_handle_arc) = self
            .handles
            .get_write_handle(handle_as_bytes(&handle))
            .ok_or(StatusCode::Failure)?;

        let mut guard = write_handle_arc.lock().await;
        if let Some(ref mut write_handle) = *guard {
            write_handle
                .write_at(offset, write_data_into_bytes(data))
                .await
                .map_err(StatusCode::from)?;
        } else {
            return Err(StatusCode::Failure);
        }

        Ok(ok_status(id))
    }

    #[instrument(level = "debug", skip(self))]
    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let info = self
            .backend
            .file_info(&normalize_path(&path))
            .await
            .map_err(StatusCode::from)?;

        Ok(Attrs {
            id,
            attrs: to_file_attributes(&info),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        // No symlink support, same as stat
        self.stat(id, path).await
    }

    async fn fstat(&mut self, id: u32, handle: SftpHandle) -> Result<Attrs, Self::Error> {
        let info = self
            .handles
            .get_handle_info(handle_as_bytes(&handle))
            .ok_or(StatusCode::Failure)?;

        let attrs = match info {
            HandleInfo::Dir { .. } => to_file_attributes(&FileInfo::directory()),
            HandleInfo::Read { path, size } => {
                let mut file_info = self
                    .backend
                    .file_info(&path)
                    .await
                    .unwrap_or_else(|_| FileInfo::file(size));
                file_info.size = size;
                to_file_attributes(&file_info)
            }
            HandleInfo::Write { path } => {
                // For write handles, we don't know the final size yet
                self.backend.file_info(&path).await.map_or_else(
                    |_| to_file_attributes(&FileInfo::file(0)),
                    |i| to_file_attributes(&i),
                )
            }
        };

        Ok(Attrs { id, attrs })
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let normalized = normalize_path(&path);
        let absolute = if normalized.is_empty() {
            "/".to_string()
        } else {
            format!("/{normalized}")
        };

        Ok(Name {
            id,
            files: vec![File::dummy(&absolute)],
        })
    }

    #[instrument(level = "debug", skip(self, _attrs))]
    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.backend
            .make_dir(&normalize_path(&path))
            .await
            .map_err(StatusCode::from)?;

        Ok(ok_status(id))
    }

    #[instrument(level = "debug", skip(self))]
    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        self.backend
            .del_dir(&normalize_path(&path))
            .await
            .map_err(StatusCode::from)?;

        Ok(ok_status(id))
    }

    #[instrument(level = "debug", skip(self))]
    async fn remove(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        self.backend
            .delete(&normalize_path(&path))
            .await
            .map_err(StatusCode::from)?;

        Ok(ok_status(id))
    }

    #[instrument(level = "debug", skip(self))]
    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        match self
            .backend
            .rename(&normalize_path(&oldpath), &normalize_path(&newpath))
            .await
        {
            Ok(()) => Ok(ok_status(id)),
            Err(e) => {
                warn!(id, from = %oldpath, to = %newpath, error = %e, "Rename failed");
                Err(StatusCode::from(e))
            }
        }
    }

    async fn setstat(
        &mut self,
        id: u32,
        _path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        // S3 doesn't support setting attributes, just acknowledge
        Ok(ok_status(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        _handle: SftpHandle,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        // S3 doesn't support setting attributes, just acknowledge
        Ok(ok_status(id))
    }

    async fn extended(
        &mut self,
        id: u32,
        request: String,
        data: Vec<u8>,
    ) -> Result<russh_sftp::protocol::Packet, Self::Error> {
        debug!(id, request = %request, data_len = data.len(), "Extended request");

        match request.as_str() {
            "posix-rename@openssh.com" => {
                // Parse posix-rename data: two strings (oldpath, newpath)
                let mut bytes = Bytes::from(data);
                let oldpath = read_string(&mut bytes).ok_or(StatusCode::BadMessage)?;
                let newpath = read_string(&mut bytes).ok_or(StatusCode::BadMessage)?;

                debug!(id, from = %oldpath, to = %newpath, "posix-rename");

                match self
                    .backend
                    .rename(&normalize_path(&oldpath), &normalize_path(&newpath))
                    .await
                {
                    Ok(()) => Ok(russh_sftp::protocol::Packet::Status(ok_status(id))),
                    Err(e) => {
                        warn!(id, from = %oldpath, to = %newpath, error = %e, "posix-rename failed");
                        Err(StatusCode::from(e))
                    }
                }
            }
            "fsync@openssh.com" => {
                // Parse fsync data: handle string
                let mut bytes = Bytes::from(data);
                let handle = read_string(&mut bytes).ok_or(StatusCode::BadMessage)?;

                debug!(id, handle = %handle, "fsync");

                // For virtual filesystems, fsync is a no-op - just acknowledge
                Ok(russh_sftp::protocol::Packet::Status(ok_status(id)))
            }
            "statvfs@openssh.com" => {
                // Parse statvfs data: path string
                let mut bytes = Bytes::from(data);
                let path = read_string(&mut bytes).ok_or(StatusCode::BadMessage)?;

                debug!(id, path = %path, "statvfs");

                // Return virtual filesystem stats
                let statvfs = russh_sftp::extensions::Statvfs {
                    block_size: 4096,
                    fragment_size: 4096,
                    blocks: u64::MAX / 4096, // Effectively unlimited
                    blocks_free: u64::MAX / 4096,
                    blocks_avail: u64::MAX / 4096,
                    inodes: u64::MAX,
                    inodes_free: u64::MAX,
                    inodes_avail: u64::MAX,
                    fs_id: 0,
                    flags: 0,
                    name_max: 255,
                };

                Ok(russh_sftp::protocol::Packet::ExtendedReply(
                    russh_sftp::protocol::ExtendedReply {
                        id,
                        data: russh_sftp::ser::to_bytes(&statvfs)
                            .map_err(|_| StatusCode::Failure)?
                            .to_vec(),
                    },
                ))
            }
            _ => {
                debug!(id, request = %request, "Unsupported extended request");
                Err(StatusCode::OpUnsupported)
            }
        }
    }
}

/// Helper to read a length-prefixed string from bytes.
/// Tries valid UTF-8 first (owned), falls back to lossy conversion.
fn read_string(bytes: &mut Bytes) -> Option<String> {
    if bytes.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    bytes.advance(4);
    if bytes.len() < len {
        return None;
    }
    let raw = bytes.split_to(len);
    let s = match String::from_utf8(raw.to_vec()) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MemoryBackend;
    use proptest::prelude::*;
    use russh_sftp::protocol::{FileAttributes, OpenFlags};
    use russh_sftp::server::Handler as SftpHandlerTrait;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn make_handler() -> SftpHandler<MemoryBackend> {
        SftpHandler::new(Arc::new(MemoryBackend::new()))
    }

    fn to_sftp_handle(s: impl Into<Bytes>) -> SftpHandle {
        s.into()
    }

    fn to_sftp_data(v: impl Into<Vec<u8>>) -> SftpWriteData {
        Bytes::from(v.into())
    }

    // Helper: init the SFTP session
    async fn init_handler(handler: &mut SftpHandler<MemoryBackend>) {
        handler.init(3, HashMap::new()).await.expect("init failed");
    }

    #[tokio::test]
    async fn test_init_returns_version_with_extensions() {
        let mut handler = make_handler();
        let version = handler.init(3, HashMap::new()).await.unwrap();
        assert!(version.extensions.contains_key("posix-rename@openssh.com"));
        assert!(version.extensions.contains_key("fsync@openssh.com"));
        assert!(version.extensions.contains_key("statvfs@openssh.com"));
    }

    #[tokio::test]
    async fn test_open_write_close_open_read_roundtrip() {
        let mut handler = make_handler();
        init_handler(&mut handler).await;

        // Write file
        let write_handle = handler
            .open(
                1,
                "/test.txt".to_string(),
                OpenFlags::WRITE | OpenFlags::CREATE,
                FileAttributes::default(),
            )
            .await
            .unwrap();

        let handle_bytes = to_sftp_handle(write_handle.handle.clone());
        handler
            .write(1, handle_bytes.clone(), 0, to_sftp_data(b"hello world"))
            .await
            .unwrap();
        handler.close(1, handle_bytes).await.unwrap();

        // Read file back
        let read_handle = handler
            .open(
                2,
                "/test.txt".to_string(),
                OpenFlags::READ,
                FileAttributes::default(),
            )
            .await
            .unwrap();

        let handle_bytes = to_sftp_handle(read_handle.handle);
        let data = handler.read(2, handle_bytes.clone(), 0, 64).await.unwrap();
        assert_eq!(data.data, Bytes::from_static(b"hello world"));

        handler.close(2, handle_bytes).await.unwrap();
    }

    #[tokio::test]
    async fn test_stat_nonexistent_returns_error() {
        let mut handler = make_handler();
        let result = handler.stat(1, "/nonexistent.txt".to_string()).await;
        assert!(matches!(result, Err(StatusCode::NoSuchFile)));
    }

    #[tokio::test]
    async fn test_mkdir_opendir_readdir_sees_entry() {
        let mut handler = make_handler();
        init_handler(&mut handler).await;

        // Create a dir and a file in it
        handler
            .mkdir(1, "/mydir".to_string(), FileAttributes::default())
            .await
            .unwrap();

        // Write a file into the dir
        let wh = handler
            .open(
                2,
                "/mydir/file.txt".to_string(),
                OpenFlags::WRITE | OpenFlags::CREATE,
                FileAttributes::default(),
            )
            .await
            .unwrap();
        let wh_bytes = to_sftp_handle(wh.handle);
        handler
            .write(2, wh_bytes.clone(), 0, to_sftp_data(b"data"))
            .await
            .unwrap();
        handler.close(2, wh_bytes).await.unwrap();

        // Open dir and read entries
        let dir_handle = handler.opendir(3, "/mydir".to_string()).await.unwrap();
        let dir_bytes = to_sftp_handle(dir_handle.handle);

        let entries = handler.readdir(3, dir_bytes.clone()).await.unwrap();
        let names: Vec<&str> = entries.files.iter().map(|f| f.filename.as_str()).collect();
        assert!(names.contains(&"."));
        assert!(names.contains(&".."));
        assert!(names.contains(&"file.txt"));

        // Second readdir should return EOF
        let eof = handler.readdir(3, dir_bytes).await;
        assert!(matches!(eof, Err(StatusCode::Eof)));
    }

    #[tokio::test]
    async fn test_remove_then_stat_fails() {
        let mut handler = make_handler();

        // Write a file then remove it
        let wh = handler
            .open(
                1,
                "/todel.txt".to_string(),
                OpenFlags::WRITE | OpenFlags::CREATE,
                FileAttributes::default(),
            )
            .await
            .unwrap();
        let wh_bytes = to_sftp_handle(wh.handle);
        handler
            .write(1, wh_bytes.clone(), 0, to_sftp_data(b"x"))
            .await
            .unwrap();
        handler.close(1, wh_bytes).await.unwrap();

        handler.remove(2, "/todel.txt".to_string()).await.unwrap();

        let result = handler.stat(3, "/todel.txt".to_string()).await;
        assert!(matches!(result, Err(StatusCode::NoSuchFile)));
    }

    #[tokio::test]
    async fn test_rename_preserves_content() {
        let mut handler = make_handler();

        let wh = handler
            .open(
                1,
                "/src.txt".to_string(),
                OpenFlags::WRITE | OpenFlags::CREATE,
                FileAttributes::default(),
            )
            .await
            .unwrap();
        let wh_bytes = to_sftp_handle(wh.handle);
        handler
            .write(1, wh_bytes.clone(), 0, to_sftp_data(b"content"))
            .await
            .unwrap();
        handler.close(1, wh_bytes).await.unwrap();

        handler
            .rename(2, "/src.txt".to_string(), "/dst.txt".to_string())
            .await
            .unwrap();

        // dst exists with content
        let rh = handler
            .open(
                3,
                "/dst.txt".to_string(),
                OpenFlags::READ,
                FileAttributes::default(),
            )
            .await
            .unwrap();
        let rh_bytes = to_sftp_handle(rh.handle);
        let data = handler.read(3, rh_bytes.clone(), 0, 64).await.unwrap();
        assert_eq!(data.data, Bytes::from_static(b"content"));
        handler.close(3, rh_bytes).await.unwrap();

        // src is gone
        let result = handler.stat(4, "/src.txt".to_string()).await;
        assert!(matches!(result, Err(StatusCode::NoSuchFile)));
    }

    #[tokio::test]
    async fn test_realpath_normalizes() {
        let mut handler = make_handler();

        let result = handler.realpath(1, "/".to_string()).await.unwrap();
        assert_eq!(result.files[0].filename, "/");

        let result = handler.realpath(2, "foo/bar".to_string()).await.unwrap();
        assert_eq!(result.files[0].filename, "/foo/bar");

        let result = handler.realpath(3, String::new()).await.unwrap();
        assert_eq!(result.files[0].filename, "/");

        let result = handler.realpath(4, "/..".to_string()).await.unwrap();
        assert_eq!(result.files[0].filename, "/");
    }

    #[tokio::test]
    async fn test_extended_posix_rename() {
        let mut handler = make_handler();

        // Write a file first
        let wh = handler
            .open(
                1,
                "/old.txt".to_string(),
                OpenFlags::WRITE | OpenFlags::CREATE,
                FileAttributes::default(),
            )
            .await
            .unwrap();
        let wh_bytes = to_sftp_handle(wh.handle);
        handler
            .write(1, wh_bytes.clone(), 0, to_sftp_data(b"data"))
            .await
            .unwrap();
        handler.close(1, wh_bytes).await.unwrap();

        // Build posix-rename payload: [4-byte len][old][4-byte len][new]
        let mut payload = Vec::new();
        let old = b"/old.txt";
        let new = b"/new.txt";
        payload.extend_from_slice(&(old.len() as u32).to_be_bytes());
        payload.extend_from_slice(old);
        payload.extend_from_slice(&(new.len() as u32).to_be_bytes());
        payload.extend_from_slice(new);

        let result = handler
            .extended(2, "posix-rename@openssh.com".to_string(), payload)
            .await
            .unwrap();
        assert!(matches!(result, russh_sftp::protocol::Packet::Status(_)));

        // old is gone, new exists
        assert!(matches!(
            handler.stat(3, "/old.txt".to_string()).await,
            Err(StatusCode::NoSuchFile)
        ));
        assert!(handler.stat(4, "/new.txt".to_string()).await.is_ok());
    }

    #[tokio::test]
    async fn test_extended_fsync() {
        let mut handler = make_handler();

        // fsync just needs a handle string payload
        let handle = b"1";
        let mut payload = Vec::new();
        payload.extend_from_slice(&(handle.len() as u32).to_be_bytes());
        payload.extend_from_slice(handle);

        let result = handler
            .extended(1, "fsync@openssh.com".to_string(), payload)
            .await
            .unwrap();
        assert!(matches!(result, russh_sftp::protocol::Packet::Status(_)));
    }

    #[tokio::test]
    async fn test_extended_statvfs() {
        let mut handler = make_handler();

        let path = b"/";
        let mut payload = Vec::new();
        payload.extend_from_slice(&(path.len() as u32).to_be_bytes());
        payload.extend_from_slice(path);

        let result = handler
            .extended(1, "statvfs@openssh.com".to_string(), payload)
            .await
            .unwrap();
        assert!(matches!(
            result,
            russh_sftp::protocol::Packet::ExtendedReply(_)
        ));
    }

    #[tokio::test]
    async fn test_extended_unsupported() {
        let mut handler = make_handler();
        let result = handler
            .extended(1, "unknown@example.com".to_string(), vec![])
            .await;
        assert!(matches!(result, Err(StatusCode::OpUnsupported)));
    }

    #[tokio::test]
    async fn test_fstat_read_handle_returns_size() {
        let mut handler = make_handler();

        // Write a known file
        let wh = handler
            .open(
                1,
                "/sized.txt".to_string(),
                OpenFlags::WRITE | OpenFlags::CREATE,
                FileAttributes::default(),
            )
            .await
            .unwrap();
        let wh_bytes = to_sftp_handle(wh.handle);
        handler
            .write(1, wh_bytes.clone(), 0, to_sftp_data(b"12345"))
            .await
            .unwrap();
        handler.close(1, wh_bytes).await.unwrap();

        // Open for read and fstat
        let rh = handler
            .open(
                2,
                "/sized.txt".to_string(),
                OpenFlags::READ,
                FileAttributes::default(),
            )
            .await
            .unwrap();
        let rh_bytes = to_sftp_handle(rh.handle.clone());

        let attrs = handler.fstat(2, rh_bytes.clone()).await.unwrap();
        assert_eq!(attrs.attrs.size, Some(5));

        handler.close(2, rh_bytes).await.unwrap();
    }

    #[tokio::test]
    async fn test_readdir_second_call_returns_eof() {
        let mut handler = make_handler();

        let dir_handle = handler.opendir(1, "/".to_string()).await.unwrap();
        let dir_bytes = to_sftp_handle(dir_handle.handle);

        // First call succeeds (may return empty list with just . and ..)
        let _ = handler.readdir(1, dir_bytes.clone()).await.unwrap();

        // Second call returns EOF
        let result = handler.readdir(1, dir_bytes).await;
        assert!(matches!(result, Err(StatusCode::Eof)));
    }

    // --- Property tests ---

    proptest! {
        #[test]
        fn prop_write_read_roundtrip(
            // Simple paths: no slashes so we don't create implicit directories
            filename in "[a-z][a-z0-9_]{0,12}\\.[a-z]{1,4}",
            content in proptest::collection::vec(any::<u8>(), 0..4096)
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut handler = make_handler();

                let path = format!("/{}", filename);
                // Write
                let wh = handler
                    .open(1, path.clone(), OpenFlags::WRITE | OpenFlags::CREATE, FileAttributes::default())
                    .await
                    .unwrap();
                let wh_bytes = to_sftp_handle(wh.handle);
                if !content.is_empty() {
                    handler.write(1, wh_bytes.clone(), 0, to_sftp_data(content.clone())).await.unwrap();
                }
                handler.close(1, wh_bytes).await.unwrap();

                // Read back
                let rh = handler
                    .open(2, path, OpenFlags::READ, FileAttributes::default())
                    .await
                    .unwrap();
                let rh_bytes = to_sftp_handle(rh.handle);
                let mut read_data = Vec::new();
                let mut offset = 0u64;
                loop {
                    match handler.read(2, rh_bytes.clone(), offset, 32768).await {
                        Ok(d) => {
                            offset += d.data.len() as u64;
                            read_data.extend_from_slice(&d.data);
                        }
                        Err(StatusCode::Eof) => break,
                        Err(e) => panic!("read error: {:?}", e),
                    }
                }
                handler.close(2, rh_bytes).await.unwrap();

                prop_assert_eq!(read_data, content);
                Ok(())
            })?
        }

        #[test]
        fn prop_mkdir_readdir_consistency(
            dirname in "[a-z][a-z0-9]{0,12}"
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut handler = make_handler();

                // Create dir
                handler
                    .mkdir(1, format!("/{}", dirname), FileAttributes::default())
                    .await
                    .unwrap();

                // Open root dir and read
                let dh = handler.opendir(2, "/".to_string()).await.unwrap();
                let dh_bytes = to_sftp_handle(dh.handle);
                let entries = handler.readdir(2, dh_bytes).await.unwrap();
                let names: Vec<&str> = entries.files.iter().map(|f| f.filename.as_str()).collect();

                prop_assert!(names.contains(&dirname.as_str()), "dirname {} not found in {:?}", dirname, names);
                Ok(())
            })?
        }
    }

    #[tokio::test]
    async fn test_read_past_eof_returns_eof() {
        let mut handler = make_handler();

        // Write 5 bytes
        let wh = handler
            .open(
                1,
                "/small.txt".to_string(),
                OpenFlags::WRITE | OpenFlags::CREATE,
                FileAttributes::default(),
            )
            .await
            .unwrap();
        let wh_bytes = to_sftp_handle(wh.handle);
        handler
            .write(1, wh_bytes.clone(), 0, to_sftp_data(b"hello"))
            .await
            .unwrap();
        handler.close(1, wh_bytes).await.unwrap();

        // Open and read past end
        let rh = handler
            .open(
                2,
                "/small.txt".to_string(),
                OpenFlags::READ,
                FileAttributes::default(),
            )
            .await
            .unwrap();
        let rh_bytes = to_sftp_handle(rh.handle);

        let result = handler.read(2, rh_bytes.clone(), 100, 64).await;
        assert!(matches!(result, Err(StatusCode::Eof)));

        handler.close(2, rh_bytes).await.unwrap();
    }
}
