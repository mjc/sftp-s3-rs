use super::{
    normalize_path, unix_secs_to_u32, Backend, BackendCapabilities, BackendError, BackendResult,
    DirEntry, FileInfo, FileKind, ReadHandle, SetAttrs, WriteHandle,
};
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use std::fs::File as StdFile;
use std::path::{Component, Path, PathBuf};
use tokio::fs::{self, File};
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

        let mut full_path = PathBuf::with_capacity(
            self.root.as_os_str().as_encoded_bytes().len() + normalized.len() + 1,
        );
        full_path.push(&self.root);
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

    fn resolve_symlink_target_path(
        &self,
        linkpath: &Path,
        targetpath: &str,
    ) -> BackendResult<PathBuf> {
        let target = Path::new(targetpath);
        let mut resolved = if target.is_absolute() {
            if !target.starts_with(&self.root) {
                return Err(BackendError::PermissionDenied);
            }
            self.root.clone()
        } else {
            linkpath
                .parent()
                .map_or_else(|| self.root.clone(), Path::to_path_buf)
        };

        for component in target.components() {
            match component {
                Component::Normal(part) => resolved.push(part),
                Component::CurDir => {}
                Component::ParentDir => {
                    if resolved == self.root || !resolved.pop() {
                        return Err(BackendError::PermissionDenied);
                    }
                }
                Component::RootDir => {
                    resolved = self.root.clone();
                }
                Component::Prefix(_) => return Err(BackendError::PermissionDenied),
            }
        }

        if !resolved.starts_with(&self.root) {
            return Err(BackendError::PermissionDenied);
        }

        Ok(resolved)
    }

    /// Convert `std::io::Error` to `BackendError`.
    fn map_io_error(err: std::io::Error) -> BackendError {
        match err.kind() {
            std::io::ErrorKind::NotFound => BackendError::NotFound,
            std::io::ErrorKind::PermissionDenied => BackendError::PermissionDenied,
            std::io::ErrorKind::AlreadyExists => BackendError::AlreadyExists,
            std::io::ErrorKind::DirectoryNotEmpty => BackendError::DirectoryNotEmpty,
            std::io::ErrorKind::NotADirectory => BackendError::NotADirectory,
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

        let kind = if metadata.file_type().is_symlink() {
            FileKind::Symlink
        } else if metadata.is_dir() {
            FileKind::Directory
        } else {
            FileKind::File
        };

        FileInfo {
            size: metadata.len(),
            kind,
            is_dir: kind == FileKind::Directory,
            permissions,
            mtime,
            atime,
            uid,
            gid,
        }
    }

    async fn lstat_path(&self, path: &str) -> BackendResult<FileInfo> {
        let full_path = self.full_path(path)?;
        let metadata = fs::symlink_metadata(&full_path)
            .await
            .map_err(Self::map_io_error)?;
        Ok(Self::metadata_to_info(&metadata))
    }
}

fn read_file_at(file: &StdFile, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_read(buf, offset)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let mut cloned = file.try_clone()?;
        std::io::Seek::seek(&mut cloned, std::io::SeekFrom::Start(offset))?;
        std::io::Read::read(&mut cloned, buf)
    }
}

fn write_all_file_at(file: &StdFile, mut data: &[u8], mut offset: u64) -> std::io::Result<()> {
    while !data.is_empty() {
        let written = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileExt;
                file.write_at(data, offset)?
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::FileExt;
                file.seek_write(data, offset)?
            }
            #[cfg(not(any(unix, windows)))]
            {
                let mut cloned = file.try_clone()?;
                std::io::Seek::seek(&mut cloned, std::io::SeekFrom::Start(offset))?;
                std::io::Write::write(&mut cloned, data)?
            }
        };

        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write whole buffer",
            ));
        }

        data = &data[written..];
        offset += written as u64;
    }

    Ok(())
}

#[async_trait]
impl Backend for LocalBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            symlinks: true,
            set_attrs: true,
            delegated_safe_streaming_fallback: true,
        }
    }

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
            let metadata = fs::symlink_metadata(entry.path())
                .await
                .map_err(Self::map_io_error)?;
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

    async fn lstat(&self, path: &str) -> BackendResult<FileInfo> {
        let normalized = normalize_path(path);
        self.lstat_path(normalized.as_ref()).await
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
        let normalized = normalize_path(path);
        let full_path = self.full_path(path)?;

        debug!(path = %full_path.display(), "Deleting file");

        match fs::remove_file(&full_path).await {
            Ok(()) => Ok(()),
            Err(err) => match err.kind() {
                std::io::ErrorKind::IsADirectory | std::io::ErrorKind::PermissionDenied => {
                    if self.lstat_path(normalized.as_ref()).await?.kind == FileKind::Directory {
                        Err(BackendError::IsADirectory)
                    } else {
                        Err(Self::map_io_error(err))
                    }
                }
                _ => Err(Self::map_io_error(err)),
            },
        }
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

        let (file, size) = tokio::task::spawn_blocking(move || -> BackendResult<(StdFile, u64)> {
            let file = StdFile::open(&full_path).map_err(Self::map_io_error)?;
            let size = file.metadata().map_err(Self::map_io_error)?.len();
            Ok((file, size))
        })
        .await
        .map_err(|err| BackendError::Other(err.to_string()))??;

        Ok(Box::new(LocalReadHandle {
            file,
            buf: Mutex::new(bytes::BytesMut::with_capacity(64 * 1024)),
            size,
        }))
    }

    async fn open_write(&self, path: &str) -> BackendResult<Box<dyn WriteHandle + Send>> {
        let full_path = self.full_path(path)?;

        debug!(path = %full_path.display(), "Opening file for write");

        let file = tokio::task::spawn_blocking(move || -> BackendResult<StdFile> {
            StdFile::create(&full_path).map_err(Self::map_io_error)
        })
        .await
        .map_err(|err| BackendError::Other(err.to_string()))??;

        Ok(Box::new(LocalWriteHandle { file }))
    }

    async fn read_link(&self, path: &str) -> BackendResult<String> {
        let normalized = normalize_path(path);
        let full_path = self.full_path(&normalized)?;
        let target = fs::read_link(&full_path)
            .await
            .map_err(Self::map_io_error)?;
        Ok(target.to_string_lossy().to_string())
    }

    async fn symlink(&self, linkpath: &str, targetpath: &str) -> BackendResult<()> {
        let linkpath = self.full_path(&normalize_path(linkpath))?;
        let targetpath_owned = targetpath.to_string();
        let _ = self.resolve_symlink_target_path(&linkpath, &targetpath_owned)?;
        #[cfg(windows)]
        let target_exists_path = self.resolve_symlink_target_path(&linkpath, &targetpath_owned)?;

        tokio::task::spawn_blocking(move || {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&targetpath_owned, &linkpath)
            }

            #[cfg(windows)]
            {
                let target_is_dir = target_exists_path.is_dir() || targetpath_owned.ends_with('/');
                if target_is_dir {
                    std::os::windows::fs::symlink_dir(&targetpath_owned, &linkpath)
                } else {
                    std::os::windows::fs::symlink_file(&targetpath_owned, &linkpath)
                }
            }
        })
        .await
        .map_err(|err| BackendError::Other(err.to_string()))?
        .map_err(Self::map_io_error)?;

        Ok(())
    }

    async fn set_attrs(&self, path: &str, attrs: SetAttrs) -> BackendResult<()> {
        let normalized = normalize_path(path);
        let full_path = self.full_path(&normalized)?;
        let lstat = self.lstat_path(normalized.as_ref()).await?;

        #[cfg(not(unix))]
        if attrs.uid.is_some() || attrs.gid.is_some() {
            return Err(BackendError::Unsupported);
        }

        if lstat.kind == FileKind::Symlink {
            return Err(BackendError::Unsupported);
        }
        if lstat.kind == FileKind::Directory && attrs.size.is_some() {
            return Err(BackendError::Unsupported);
        }

        if let Some(size) = attrs.size {
            let file = File::options()
                .write(true)
                .open(&full_path)
                .await
                .map_err(Self::map_io_error)?;
            file.set_len(size).await.map_err(Self::map_io_error)?;
        }

        if let Some(mode) = attrs.permissions {
            let metadata = fs::metadata(&full_path).await.map_err(Self::map_io_error)?;
            let mut permissions = metadata.permissions();

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                permissions.set_mode(mode);
            }

            #[cfg(not(unix))]
            {
                permissions.set_readonly(mode & 0o222 == 0);
            }

            fs::set_permissions(&full_path, permissions)
                .await
                .map_err(Self::map_io_error)?;
        }

        if attrs.atime.is_some() || attrs.mtime.is_some() {
            let atime =
                filetime::FileTime::from_unix_time(attrs.atime.unwrap_or(lstat.atime) as i64, 0);
            let mtime =
                filetime::FileTime::from_unix_time(attrs.mtime.unwrap_or(lstat.mtime) as i64, 0);
            let path = full_path.clone();
            tokio::task::spawn_blocking(move || filetime::set_file_times(&path, atime, mtime))
                .await
                .map_err(|err| BackendError::Other(err.to_string()))?
                .map_err(Self::map_io_error)?;
        }

        #[cfg(unix)]
        if attrs.uid.is_some() || attrs.gid.is_some() {
            let uid = attrs.uid.map(nix::unistd::Uid::from_raw);
            let gid = attrs.gid.map(nix::unistd::Gid::from_raw);
            let path = full_path.clone();
            tokio::task::spawn_blocking(move || nix::unistd::chown(&path, uid, gid))
                .await
                .map_err(|err| BackendError::Other(err.to_string()))?
                .map_err(|err| BackendError::Other(err.to_string()))?;
        }

        #[cfg(not(unix))]
        if attrs.uid.is_some() || attrs.gid.is_some() {
            return Err(BackendError::Unsupported);
        }

        Ok(())
    }
}

/// Read handle for local filesystem - uses seek + read for random access.
/// Includes a reusable buffer to avoid repeated allocations and page faults.
struct LocalReadHandle {
    file: StdFile,
    /// Reusable read buffer retained across reads to avoid repeated allocations.
    buf: Mutex<bytes::BytesMut>,
    size: u64,
}

#[async_trait]
impl ReadHandle for LocalReadHandle {
    fn try_read_at(&self, offset: u64, len: u32) -> Option<BackendResult<Bytes>> {
        let mut buf = self.buf.lock();
        buf.clear();
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        buf.resize(len, 0);

        let bytes_read = match read_file_at(&self.file, &mut buf[..], offset) {
            Ok(bytes_read) => bytes_read,
            Err(err) => return Some(Err(LocalBackend::map_io_error(err))),
        };

        buf.truncate(bytes_read);
        Some(Ok(buf.split().freeze()))
    }

    async fn read_at(&self, offset: u64, len: u32) -> BackendResult<Bytes> {
        self.try_read_at(offset, len).unwrap()
    }

    fn size(&self) -> u64 {
        self.size
    }
}

/// Write handle for local filesystem - writes directly to file
struct LocalWriteHandle {
    file: StdFile,
}

#[async_trait]
impl WriteHandle for LocalWriteHandle {
    fn try_write_at(&mut self, offset: u64, data: &Bytes) -> Option<BackendResult<()>> {
        Some(write_all_file_at(&self.file, data, offset).map_err(LocalBackend::map_io_error))
    }

    async fn write_at(&mut self, offset: u64, data: Bytes) -> BackendResult<()> {
        self.try_write_at(offset, &data).unwrap()
    }

    async fn finish(self: Box<Self>) -> BackendResult<()> {
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
    async fn test_open_write_supports_random_access_writes() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        let mut handle = backend.open_write("test.bin").await.unwrap();
        handle
            .write_at(4, Bytes::from_static(b"tail"))
            .await
            .unwrap();
        handle
            .write_at(0, Bytes::from_static(b"head"))
            .await
            .unwrap();
        handle.finish().await.unwrap();

        let read = backend.read_file("test.bin").await.unwrap();
        assert_eq!(read.as_ref(), b"headtail");
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
        assert_eq!(info.kind, FileKind::Directory);
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

    #[tokio::test]
    async fn test_symlink_readlink_and_stat() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend
            .write_file("target.txt", Bytes::from_static(b"hello"))
            .await
            .unwrap();
        backend.symlink("link.txt", "target.txt").await.unwrap();

        assert_eq!(backend.read_link("link.txt").await.unwrap(), "target.txt");
        assert_eq!(
            backend.lstat("link.txt").await.unwrap().kind,
            FileKind::Symlink
        );
        assert_eq!(
            backend.file_info("link.txt").await.unwrap().kind,
            FileKind::File
        );
    }

    #[tokio::test]
    async fn test_broken_symlink_stat_fails_lstat_succeeds() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend.symlink("broken", "missing").await.unwrap();

        assert_eq!(
            backend.lstat("broken").await.unwrap().kind,
            FileKind::Symlink
        );
        assert!(matches!(
            backend.file_info("broken").await,
            Err(BackendError::NotFound)
        ));
    }

    #[tokio::test]
    async fn test_symlink_rejects_targets_that_escape_root() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        let absolute = backend.symlink("abs-link", "/etc/passwd").await;
        let traversal = backend.symlink("rel-link", "../../etc/passwd").await;

        assert!(matches!(absolute, Err(BackendError::PermissionDenied)));
        assert!(matches!(traversal, Err(BackendError::PermissionDenied)));
    }

    #[tokio::test]
    async fn test_set_attrs_updates_permissions_times_and_size() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend
            .write_file("data.bin", Bytes::from_static(b"abc"))
            .await
            .unwrap();
        backend
            .set_attrs(
                "data.bin",
                SetAttrs {
                    size: Some(5),
                    permissions: Some(0o600),
                    atime: Some(100),
                    mtime: Some(200),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let info = backend.file_info("data.bin").await.unwrap();
        let content = backend.read_file("data.bin").await.unwrap();
        assert_eq!(info.size, 5);
        assert_eq!(info.permissions & 0o777, 0o600);
        assert_eq!(info.atime, 100);
        assert_eq!(info.mtime, 200);
        assert_eq!(content.as_ref(), b"abc\0\0");
    }

    #[tokio::test]
    async fn test_set_attrs_size_on_symlink_unsupported() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend
            .write_file("target.txt", Bytes::from_static(b"x"))
            .await
            .unwrap();
        backend.symlink("link.txt", "target.txt").await.unwrap();

        let result = backend
            .set_attrs(
                "link.txt",
                SetAttrs {
                    size: Some(1),
                    ..Default::default()
                },
            )
            .await;

        assert!(matches!(result, Err(BackendError::Unsupported)));
    }

    #[tokio::test]
    async fn test_directory_delete_semantics() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend.make_dir("dir").await.unwrap();
        backend
            .write_file("file.txt", Bytes::from_static(b"x"))
            .await
            .unwrap();
        backend
            .write_file("dir/child.txt", Bytes::from_static(b"x"))
            .await
            .unwrap();

        assert!(matches!(
            backend.delete("dir").await,
            Err(BackendError::IsADirectory)
        ));
        assert!(matches!(
            backend.del_dir("file.txt").await,
            Err(BackendError::NotADirectory)
        ));
        assert!(matches!(
            backend.del_dir("dir").await,
            Err(BackendError::DirectoryNotEmpty)
        ));
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
