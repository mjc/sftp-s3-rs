use crate::backend::{normalize_path, Backend, BackendError, FileInfo};
use crate::handle::{HandleManager, HandleType};
use bytes::Bytes;
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

/// Convert FileInfo to russh_sftp FileAttributes
fn to_file_attributes(info: &FileInfo) -> FileAttributes {
    FileAttributes {
        size: Some(info.size),
        permissions: Some(info.permissions),
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
        Ok(Version::new())
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        debug!(id, handle = %handle, "Closing handle");

        // If it's a write handle, flush the buffer to backend
        if let Some(HandleType::Write { path, buffer }) = self.handles.get(&handle) {
            self.backend
                .write_file(&path, Bytes::from(buffer))
                .await
                .map_err(StatusCode::from)?;
        }

        self.handles.remove(&handle);
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

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        debug!(id, handle = %handle, "Reading directory");

        let handle_data = self.handles.get(&handle).ok_or(StatusCode::Failure)?;

        match handle_data {
            HandleType::Dir { path, read_done } => {
                if read_done {
                    return Err(StatusCode::Eof);
                }

                let entries = self
                    .backend
                    .list_dir(&path)
                    .await
                    .map_err(StatusCode::from)?;

                // Mark as read
                self.handles.update(
                    &handle,
                    HandleType::Dir {
                        path,
                        read_done: true,
                    },
                );

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
            _ => Err(StatusCode::Failure),
        }
    }

    async fn open(
        &mut self,
        id: u32,
        path: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        debug!(id, path = %path, ?pflags, "Opening file");
        let normalized = normalize_path(&path);

        let handle = if pflags.contains(OpenFlags::WRITE) {
            // Write mode: create empty buffer
            self.handles.create_write_handle(normalized.into_owned())
        } else {
            // Read mode: load file content (returns Bytes)
            let content = self
                .backend
                .read_file(&normalized)
                .await
                .map_err(StatusCode::from)?;
            self.handles
                .create_read_handle(normalized.into_owned(), content)
        };

        Ok(Handle { id, handle })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        debug!(id, handle = %handle, offset, len, "Reading file");

        let handle_data = self.handles.get(&handle).ok_or(StatusCode::Failure)?;

        match handle_data {
            HandleType::Read { content, .. } => {
                let start = offset as usize;
                if start >= content.len() {
                    return Err(StatusCode::Eof);
                }

                let end = std::cmp::min(start + len as usize, content.len());
                // Use Bytes::slice for efficient sub-range, then convert to Vec for protocol
                let data = content.slice(start..end).to_vec();

                Ok(Data { id, data })
            }
            _ => Err(StatusCode::Failure),
        }
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        debug!(id, handle = %handle, offset, len = data.len(), "Writing file");

        let handle_data = self.handles.get(&handle).ok_or(StatusCode::Failure)?;

        match handle_data {
            HandleType::Write { path, mut buffer } => {
                // Handle writes at offset
                let start = offset as usize;
                if start > buffer.len() {
                    buffer.resize(start, 0);
                }
                if start == buffer.len() {
                    buffer.extend_from_slice(&data);
                } else {
                    let end = start + data.len();
                    if end > buffer.len() {
                        buffer.resize(end, 0);
                    }
                    buffer[start..end].copy_from_slice(&data);
                }

                self.handles
                    .update(&handle, HandleType::Write { path, buffer });

                Ok(ok_status(id))
            }
            _ => Err(StatusCode::Failure),
        }
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

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let handle_data = self.handles.get(&handle).ok_or(StatusCode::Failure)?;

        let (path, size) = match handle_data {
            HandleType::Read { path, content } => (path, content.len() as u64),
            HandleType::Write { path, buffer } => (path, buffer.len() as u64),
            HandleType::Dir { .. } => {
                return Ok(Attrs {
                    id,
                    attrs: to_file_attributes(&FileInfo::directory()),
                });
            }
        };

        let mut info = self
            .backend
            .file_info(&path)
            .await
            .unwrap_or_else(|_| FileInfo::file(size));
        info.size = size;

        Ok(Attrs {
            id,
            attrs: to_file_attributes(&info),
        })
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
        self.backend
            .rename(&normalize_path(&oldpath), &normalize_path(&newpath))
            .await
            .map_err(StatusCode::from)?;

        Ok(ok_status(id))
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
        _handle: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        // S3 doesn't support setting attributes, just acknowledge
        Ok(ok_status(id))
    }
}
