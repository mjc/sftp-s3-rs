use bytes::Bytes;

use crate::backend::{normalize_path, Backend, BackendError, FileInfo};
use crate::handle::{HandleInfo, HandleManager};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

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
        Ok(Version::new())
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        debug!(id, handle = %handle, "Closing handle");

        // If it's a write handle, finish it
        if let Some(write_handle_arc) = self.handles.take_write_handle(&handle) {
            let mut guard = write_handle_arc.lock().await;
            if let Some(write_handle) = guard.take() {
                write_handle.finish().await.map_err(StatusCode::from)?;
            }
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

        let (path, read_done) = self
            .handles
            .get_dir_handle(&handle)
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
        self.handles.mark_dir_read(&handle);

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
        debug!(id, path = %path, ?pflags, "Opening file");
        let normalized = normalize_path(&path);

        let handle = if pflags.contains(OpenFlags::WRITE) {
            // Write mode: open streaming write handle
            let write_handle = self
                .backend
                .open_write(&normalized)
                .await
                .map_err(StatusCode::from)?;
            self.handles
                .create_write_handle(normalized.into_owned(), write_handle)
        } else {
            // Read mode: open streaming read handle
            let read_handle = self
                .backend
                .open_read(&normalized)
                .await
                .map_err(StatusCode::from)?;
            self.handles
                .create_read_handle(normalized.into_owned(), read_handle)
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

        let (_path, read_handle, size) = self
            .handles
            .get_read_handle(&handle)
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
        handle: String,
        offset: u64,
        data: Bytes,
    ) -> Result<Status, Self::Error> {
        debug!(id, handle = %handle, offset, len = data.len(), "Writing file");

        let (_path, write_handle_arc) = self
            .handles
            .get_write_handle(&handle)
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

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let info = self
            .handles
            .get_handle_info(&handle)
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
