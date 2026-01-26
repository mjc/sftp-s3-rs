use bytes::{Buf, Bytes};

use crate::backend::{normalize_path, Backend, BackendError, FileInfo};
use crate::handle::{HandleInfo, HandleManager};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, warn};

// Unix file type bits for SFTP
const S_IFREG: u32 = 0o100000; // Regular file
const S_IFDIR: u32 = 0o040000; // Directory

/// Convert FileInfo to russh_sftp FileAttributes
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

/// Convert BackendError to SFTP StatusCode
impl From<BackendError> for StatusCode {
    fn from(err: BackendError) -> Self {
        match err {
            BackendError::NotFound => StatusCode::NoSuchFile,
            BackendError::PermissionDenied => StatusCode::PermissionDenied,
            BackendError::AlreadyExists => StatusCode::Failure,
            BackendError::NotADirectory => StatusCode::NoSuchFile,
            BackendError::IsADirectory => StatusCode::Failure,
            BackendError::DirectoryNotEmpty => StatusCode::Failure,
            BackendError::Io(_) => StatusCode::Failure,
            BackendError::Other(_) => StatusCode::Failure,
        }
    }
}

fn ok_status(id: u32) -> Status {
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

    async fn close(&mut self, id: u32, handle: Bytes) -> Result<Status, Self::Error> {
        let handle_str = String::from_utf8_lossy(&handle);
        debug!(id, handle = %handle_str, "Closing handle");

        // If it's a write handle, finish it
        if let Some(write_handle_arc) = self.handles.take_write_handle(&handle_str) {
            let mut guard = write_handle_arc.lock().await;
            if let Some(write_handle) = guard.take() {
                write_handle.finish().await.map_err(StatusCode::from)?;
            }
        }

        self.handles.remove(&handle_str);
        Ok(ok_status(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        debug!(id, path = %path, "Opening directory");
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

        let handle = self.handles.create_dir_handle(normalized.into_owned());
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: Bytes) -> Result<Name, Self::Error> {
        let handle_str = String::from_utf8_lossy(&handle);
        debug!(id, handle = %handle_str, "Reading directory");

        let (path, read_done) = self
            .handles
            .get_dir_handle(&handle_str)
            .ok_or(StatusCode::Failure)?;

        if read_done {
            return Err(StatusCode::Eof);
        }

        let entries = self
            .backend
            .list_dir(&path)
            .await
            .map_err(StatusCode::from)?;

        // Mark as read
        self.handles.mark_dir_read(&handle_str);

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

    async fn open(
        &mut self,
        id: u32,
        path: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        debug!(id, path = %path, ?pflags, write = pflags.contains(OpenFlags::WRITE),
               create = pflags.contains(OpenFlags::CREATE), "Opening file");
        let normalized = normalize_path(&path);

        // Treat CREATE as implying write mode (some clients send CREATE without WRITE)
        let is_write = pflags.contains(OpenFlags::WRITE) || pflags.contains(OpenFlags::CREATE);

        let handle = if is_write {
            // Write mode: open streaming write handle
            match self.backend.open_write(&normalized).await {
                Ok(write_handle) => {
                    let h = self
                        .handles
                        .create_write_handle(normalized.into_owned(), write_handle);
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
                        .create_read_handle(normalized.into_owned(), read_handle);
                    debug!(id, path = %path, handle = %h, "Opened file for read");
                    h
                }
                Err(e) => {
                    warn!(id, path = %path, error = %e, "Failed to open file for read");
                    return Err(StatusCode::from(e));
                }
            }
        };

        Ok(Handle { id, handle })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: Bytes,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let handle_str = String::from_utf8_lossy(&handle);
        debug!(id, handle = %handle_str, offset, len, "Reading file");

        let (_path, read_handle, size) = self
            .handles
            .get_read_handle(&handle_str)
            .ok_or(StatusCode::Failure)?;

        if offset >= size {
            return Err(StatusCode::Eof);
        }

        let guard = read_handle.lock().await;
        let data = guard.read_at(offset, len).await.map_err(StatusCode::from)?;

        if data.is_empty() {
            return Err(StatusCode::Eof);
        }

        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: Bytes,
        offset: u64,
        data: Bytes,
    ) -> Result<Status, Self::Error> {
        let handle_str = String::from_utf8_lossy(&handle);
        debug!(id, handle = %handle_str, offset, len = data.len(), "Writing file");

        let (_path, write_handle_arc) = self
            .handles
            .get_write_handle(&handle_str)
            .ok_or(StatusCode::Failure)?;

        let mut guard = write_handle_arc.lock().await;
        if let Some(ref mut write_handle) = *guard {
            write_handle
                .write_at(offset, &data)
                .await
                .map_err(StatusCode::from)?;
        } else {
            return Err(StatusCode::Failure);
        }

        Ok(ok_status(id))
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        debug!(id, path = %path, "Getting file stats");
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

    async fn fstat(&mut self, id: u32, handle: Bytes) -> Result<Attrs, Self::Error> {
        let handle_str = String::from_utf8_lossy(&handle);
        let info = self
            .handles
            .get_handle_info(&handle_str)
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
                self.backend
                    .file_info(&path)
                    .await
                    .map(|i| to_file_attributes(&i))
                    .unwrap_or_else(|_| to_file_attributes(&FileInfo::file(0)))
            }
        };

        Ok(Attrs { id, attrs })
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let normalized = normalize_path(&path);
        let absolute = if normalized.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", normalized)
        };

        Ok(Name {
            id,
            files: vec![File::dummy(&absolute)],
        })
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        debug!(id, path = %path, "Creating directory");
        self.backend
            .make_dir(&normalize_path(&path))
            .await
            .map_err(StatusCode::from)?;

        Ok(ok_status(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        debug!(id, path = %path, "Removing directory");
        self.backend
            .del_dir(&normalize_path(&path))
            .await
            .map_err(StatusCode::from)?;

        Ok(ok_status(id))
    }

    async fn remove(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        debug!(id, path = %path, "Removing file");
        self.backend
            .delete(&normalize_path(&path))
            .await
            .map_err(StatusCode::from)?;

        Ok(ok_status(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        debug!(id, from = %oldpath, to = %newpath, "Renaming");
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
        _handle: Bytes,
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

/// Helper to read a length-prefixed string from bytes
fn read_string(bytes: &mut Bytes) -> Option<String> {
    if bytes.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    bytes.advance(4);
    if bytes.len() < len {
        return None;
    }
    let s = String::from_utf8_lossy(&bytes[..len]).to_string();
    bytes.advance(len);
    Some(s)
}
