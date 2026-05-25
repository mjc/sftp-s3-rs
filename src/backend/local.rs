use super::{
    normalize_path, unix_secs_to_u32, Backend, BackendCapabilities, BackendError, BackendResult,
    DirEntry, FileInfo, FileKind, ReadHandle, SetAttrs, WriteHandle,
};
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex as ParkingMutex;
use parking_lot::RwLock;
use russh::{ChannelData, ChannelDataRecycler, ReusableChannelData};
use russh_sftp::protocol::DataPayload;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs::File as StdFile;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use tokio::sync::oneshot;
use tracing::debug;

const MAX_LOCAL_READ_LEN: usize = 16 * 1024 * 1024;
const MAX_POOLED_READ_BUFFERS: usize = 128;
const LOCAL_READ_PREFIX_RESERVE: usize = 13;
pub(crate) const LOCAL_READ_MAX_SINGLE_CHANNEL_DATA: usize = 64 * 1024 - LOCAL_READ_PREFIX_RESERVE;

/// Local filesystem storage backend
pub struct LocalBackend {
    root: PathBuf,
    fs_pool: Arc<LocalFsPool>,
    metadata_cache: Arc<LocalMetadataCache>,
}

impl LocalBackend {
    /// Create a new local backend rooted at the given path
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            fs_pool: default_local_fs_pool(),
            metadata_cache: Arc::new(LocalMetadataCache::default()),
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
        if let Some(info) = self
            .metadata_cache
            .get(&full_path, MetadataCacheKind::Lstat)
        {
            return Ok(info);
        }

        let shard = self.fs_pool.shard_for_path(&full_path);
        let lookup_path = full_path.clone();
        let info = self
            .fs_pool
            .run_on_shard(shard, move || {
                let metadata =
                    std::fs::symlink_metadata(&lookup_path).map_err(Self::map_io_error)?;
                Ok(Self::metadata_to_info(&metadata))
            })
            .await?;
        self.metadata_cache
            .insert(full_path, MetadataCacheKind::Lstat, info.clone());
        Ok(info)
    }

    fn invalidate_path_and_parent(&self, path: &Path) {
        self.metadata_cache.invalidate_path(path);
        if let Some(parent) = path.parent() {
            self.metadata_cache.invalidate_path(parent);
        }
    }

    fn invalidate_paths<'a>(&self, paths: impl IntoIterator<Item = &'a Path>) {
        for path in paths {
            self.invalidate_path_and_parent(path);
        }
    }
}

type LocalFsTask = Box<dyn FnOnce() + Send + 'static>;

struct LocalFsPool {
    workers: Box<[mpsc::Sender<LocalFsTask>]>,
}

impl LocalFsPool {
    fn new(worker_count: usize) -> Self {
        let mut workers = Vec::with_capacity(worker_count);
        for worker_id in 0..worker_count {
            let (tx, rx) = mpsc::channel::<LocalFsTask>();
            std::thread::Builder::new()
                .name(format!("local-fs-worker-{worker_id}"))
                .spawn(move || {
                    while let Ok(task) = rx.recv() {
                        task();
                    }
                })
                .expect("failed to spawn local fs worker");
            workers.push(tx);
        }

        Self {
            workers: workers.into_boxed_slice(),
        }
    }

    fn shard_for_path(&self, path: &Path) -> usize {
        self.shard_for_key(path.to_string_lossy().as_ref())
    }

    fn shard_for_paths(&self, paths: &[&Path]) -> usize {
        let mut keys = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        keys.sort_unstable();

        let mut hasher = DefaultHasher::new();
        for key in keys {
            key.hash(&mut hasher);
        }
        (hasher.finish() as usize) % self.workers.len()
    }

    fn shard_for_key(&self, key: &str) -> usize {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        (hasher.finish() as usize) % self.workers.len()
    }

    async fn run_on_shard<T, F>(&self, shard: usize, task: F) -> BackendResult<T>
    where
        T: Send + 'static,
        F: FnOnce() -> BackendResult<T> + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.workers[shard]
            .send(Box::new(move || {
                let _ = reply_tx.send(task());
            }))
            .map_err(|err| BackendError::Other(err.to_string()))?;

        reply_rx
            .await
            .map_err(|err| BackendError::Other(err.to_string()))?
    }
}

fn default_local_fs_pool() -> Arc<LocalFsPool> {
    static POOL: OnceLock<Arc<LocalFsPool>> = OnceLock::new();
    Arc::clone(POOL.get_or_init(|| Arc::new(LocalFsPool::new(default_local_fs_worker_count()))))
}

fn default_local_fs_worker_count() -> usize {
    std::thread::available_parallelism().map_or(2, |count| count.get().clamp(2, 4))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataCacheKind {
    Stat,
    Lstat,
}

#[derive(Debug, Clone, Default)]
struct CachedMetadata {
    stat: Option<FileInfo>,
    lstat: Option<FileInfo>,
}

#[derive(Default)]
struct LocalMetadataCache {
    entries: RwLock<HashMap<PathBuf, CachedMetadata>>,
}

impl LocalMetadataCache {
    fn get(&self, path: &Path, kind: MetadataCacheKind) -> Option<FileInfo> {
        let entries = self.entries.read();
        let entry = entries.get(path)?;
        match kind {
            MetadataCacheKind::Stat => entry.stat.clone(),
            MetadataCacheKind::Lstat => entry.lstat.clone(),
        }
    }

    fn insert(&self, path: PathBuf, kind: MetadataCacheKind, info: FileInfo) {
        let mut entries = self.entries.write();
        let entry = entries.entry(path).or_default();
        match kind {
            MetadataCacheKind::Stat => entry.stat = Some(info),
            MetadataCacheKind::Lstat => entry.lstat = Some(info),
        }
    }

    fn insert_from_metadata(&self, path: PathBuf, metadata: &std::fs::Metadata) -> FileInfo {
        let info = LocalBackend::metadata_to_info(metadata);
        let mut entries = self.entries.write();
        let entry = entries.entry(path).or_default();
        entry.lstat = Some(info.clone());
        if !metadata.file_type().is_symlink() {
            entry.stat = Some(info.clone());
        }
        info
    }

    fn invalidate_path(&self, path: &Path) {
        self.entries.write().remove(path);
    }
}

#[cfg(not(unix))]
fn read_file_at(file: &StdFile, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
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

#[cfg(unix)]
fn read_file_at_uninit(
    file: &StdFile,
    buf: &mut [std::mem::MaybeUninit<u8>],
    offset: u64,
) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;

    let offset = nix::libc::off_t::try_from(offset).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file offset does not fit in off_t",
        )
    })?;

    let bytes_read =
        unsafe { nix::libc::pread(file.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), offset) };

    if bytes_read < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(bytes_read as usize)
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
        let metadata_cache = Arc::clone(&self.metadata_cache);
        let shard = self.fs_pool.shard_for_path(&full_path);
        let dir_path = full_path.clone();

        debug!(path = %full_path.display(), "Listing directory");

        self.fs_pool
            .run_on_shard(shard, move || {
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

                for entry in std::fs::read_dir(&dir_path).map_err(Self::map_io_error)? {
                    let entry = entry.map_err(Self::map_io_error)?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    let entry_path = entry.path();
                    let metadata =
                        std::fs::symlink_metadata(&entry_path).map_err(Self::map_io_error)?;
                    let attrs = metadata_cache.insert_from_metadata(entry_path, &metadata);
                    entries.push(DirEntry { name, attrs });
                }

                Ok(entries)
            })
            .await
    }

    async fn file_info(&self, path: &str) -> BackendResult<FileInfo> {
        let full_path = self.full_path(path)?;

        debug!(path = %full_path.display(), "Getting file info");

        if let Some(info) = self.metadata_cache.get(&full_path, MetadataCacheKind::Stat) {
            return Ok(info);
        }

        let shard = self.fs_pool.shard_for_path(&full_path);
        let lookup_path = full_path.clone();
        let info = self
            .fs_pool
            .run_on_shard(shard, move || {
                let metadata = std::fs::metadata(&lookup_path).map_err(Self::map_io_error)?;
                Ok(Self::metadata_to_info(&metadata))
            })
            .await?;
        self.metadata_cache
            .insert(full_path, MetadataCacheKind::Stat, info.clone());
        Ok(info)
    }

    async fn lstat(&self, path: &str) -> BackendResult<FileInfo> {
        let normalized = normalize_path(path);
        self.lstat_path(normalized.as_ref()).await
    }

    async fn make_dir(&self, path: &str) -> BackendResult<()> {
        let full_path = self.full_path(path)?;
        let shard = self.fs_pool.shard_for_path(&full_path);
        let create_path = full_path.clone();

        debug!(path = %full_path.display(), "Creating directory");

        self.fs_pool
            .run_on_shard(shard, move || {
                std::fs::create_dir(&create_path).map_err(Self::map_io_error)
            })
            .await?;
        self.invalidate_path_and_parent(&full_path);
        Ok(())
    }

    async fn del_dir(&self, path: &str) -> BackendResult<()> {
        let full_path = self.full_path(path)?;
        let shard = self.fs_pool.shard_for_path(&full_path);
        let remove_path = full_path.clone();

        debug!(path = %full_path.display(), "Removing directory");

        self.fs_pool
            .run_on_shard(shard, move || {
                std::fs::remove_dir(&remove_path).map_err(Self::map_io_error)
            })
            .await?;
        self.invalidate_path_and_parent(&full_path);
        Ok(())
    }

    async fn delete(&self, path: &str) -> BackendResult<()> {
        let full_path = self.full_path(path)?;
        let shard = self.fs_pool.shard_for_path(&full_path);
        let delete_path = full_path.clone();

        debug!(path = %full_path.display(), "Deleting file");

        self.fs_pool
            .run_on_shard(shard, move || match std::fs::remove_file(&delete_path) {
                Ok(()) => Ok(()),
                Err(err) => match err.kind() {
                    std::io::ErrorKind::IsADirectory | std::io::ErrorKind::PermissionDenied => {
                        let metadata =
                            std::fs::symlink_metadata(&delete_path).map_err(Self::map_io_error)?;
                        if Self::metadata_to_info(&metadata).kind == FileKind::Directory {
                            Err(BackendError::IsADirectory)
                        } else {
                            Err(Self::map_io_error(err))
                        }
                    }
                    _ => Err(Self::map_io_error(err)),
                },
            })
            .await?;
        self.invalidate_path_and_parent(&full_path);
        Ok(())
    }

    async fn rename(&self, src: &str, dst: &str) -> BackendResult<()> {
        let src_path = self.full_path(src)?;
        let dst_path = self.full_path(dst)?;
        let shard = self
            .fs_pool
            .shard_for_paths(&[src_path.as_path(), dst_path.as_path()]);
        let rename_src = src_path.clone();
        let rename_dst = dst_path.clone();

        debug!(from = %src_path.display(), to = %dst_path.display(), "Renaming");

        self.fs_pool
            .run_on_shard(shard, move || {
                std::fs::rename(&rename_src, &rename_dst).map_err(Self::map_io_error)
            })
            .await?;
        self.invalidate_paths([src_path.as_path(), dst_path.as_path()]);
        Ok(())
    }

    async fn read_file(&self, path: &str) -> BackendResult<Bytes> {
        let full_path = self.full_path(path)?;
        let shard = self.fs_pool.shard_for_path(&full_path);
        let read_path = full_path.clone();

        debug!(path = %full_path.display(), "Reading file");

        self.fs_pool
            .run_on_shard(shard, move || {
                let content = std::fs::read(&read_path).map_err(Self::map_io_error)?;
                Ok(Bytes::from(content))
            })
            .await
    }

    async fn write_file(&self, path: &str, content: Bytes) -> BackendResult<()> {
        let full_path = self.full_path(path)?;
        let shard = self.fs_pool.shard_for_path(&full_path);
        let write_path = full_path.clone();

        debug!(path = %full_path.display(), len = content.len(), "Writing file");

        self.fs_pool
            .run_on_shard(shard, move || {
                std::fs::write(&write_path, &content).map_err(Self::map_io_error)
            })
            .await?;
        self.invalidate_path_and_parent(&full_path);
        Ok(())
    }

    async fn open_read(&self, path: &str) -> BackendResult<Box<dyn ReadHandle>> {
        let full_path = self.full_path(path)?;

        debug!(path = %full_path.display(), "Opening file for read");

        let shard = self.fs_pool.shard_for_path(&full_path);
        let open_path = full_path.clone();
        let (file, size) = self
            .fs_pool
            .run_on_shard(shard, move || {
                let file = StdFile::open(&open_path).map_err(Self::map_io_error)?;
                let size = file.metadata().map_err(Self::map_io_error)?.len();
                Ok((Arc::new(file), size))
            })
            .await?;

        Ok(Box::new(LocalReadHandle {
            file,
            size,
            read_buffers: Arc::new(LocalReadBufferPool::default()),
        }))
    }

    async fn open_write(&self, path: &str) -> BackendResult<Box<dyn WriteHandle + Send>> {
        let full_path = self.full_path(path)?;

        debug!(path = %full_path.display(), "Opening file for write");

        let shard = self.fs_pool.shard_for_path(&full_path);
        let open_path = full_path.clone();
        let file = self
            .fs_pool
            .run_on_shard(shard, move || {
                StdFile::create(&open_path)
                    .map(Arc::new)
                    .map_err(Self::map_io_error)
            })
            .await?;
        self.invalidate_path_and_parent(&full_path);

        Ok(Box::new(LocalWriteHandle {
            pool: Arc::clone(&self.fs_pool),
            metadata_cache: Arc::clone(&self.metadata_cache),
            shard,
            file,
            path: full_path,
        }))
    }

    async fn read_link(&self, path: &str) -> BackendResult<String> {
        let normalized = normalize_path(path);
        let full_path = self.full_path(&normalized)?;
        let shard = self.fs_pool.shard_for_path(&full_path);
        let link_path = full_path.clone();
        self.fs_pool
            .run_on_shard(shard, move || {
                let target = std::fs::read_link(&link_path).map_err(Self::map_io_error)?;
                Ok(target.to_string_lossy().to_string())
            })
            .await
    }

    async fn symlink(&self, linkpath: &str, targetpath: &str) -> BackendResult<()> {
        let linkpath = self.full_path(&normalize_path(linkpath))?;
        let targetpath_owned = targetpath.to_string();
        let _ = self.resolve_symlink_target_path(&linkpath, &targetpath_owned)?;
        #[cfg(windows)]
        let target_exists_path = self.resolve_symlink_target_path(&linkpath, &targetpath_owned)?;
        let shard = self.fs_pool.shard_for_path(&linkpath);
        let symlink_path = linkpath.clone();

        self.fs_pool
            .run_on_shard(shard, move || {
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&targetpath_owned, &symlink_path)
                        .map_err(Self::map_io_error)?;
                    Ok(())
                }

                #[cfg(windows)]
                {
                    let target_is_dir =
                        target_exists_path.is_dir() || targetpath_owned.ends_with('/');
                    if target_is_dir {
                        std::os::windows::fs::symlink_dir(&targetpath_owned, &symlink_path)
                    } else {
                        std::os::windows::fs::symlink_file(&targetpath_owned, &symlink_path)
                    }
                    .map_err(Self::map_io_error)?;
                    Ok(())
                }
            })
            .await?;

        self.invalidate_path_and_parent(&linkpath);
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

        let shard = self.fs_pool.shard_for_path(&full_path);
        let attr_path = full_path.clone();
        self.fs_pool
            .run_on_shard(shard, move || {
                if let Some(size) = attrs.size {
                    let file = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&attr_path)
                        .map_err(Self::map_io_error)?;
                    file.set_len(size).map_err(Self::map_io_error)?;
                }

                if let Some(mode) = attrs.permissions {
                    let metadata = std::fs::metadata(&attr_path).map_err(Self::map_io_error)?;
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

                    std::fs::set_permissions(&attr_path, permissions)
                        .map_err(Self::map_io_error)?;
                }

                if attrs.atime.is_some() || attrs.mtime.is_some() {
                    let atime = filetime::FileTime::from_unix_time(
                        attrs.atime.unwrap_or(lstat.atime) as i64,
                        0,
                    );
                    let mtime = filetime::FileTime::from_unix_time(
                        attrs.mtime.unwrap_or(lstat.mtime) as i64,
                        0,
                    );
                    filetime::set_file_times(&attr_path, atime, mtime)
                        .map_err(Self::map_io_error)?;
                }

                #[cfg(unix)]
                if attrs.uid.is_some() || attrs.gid.is_some() {
                    let uid = attrs.uid.map(nix::unistd::Uid::from_raw);
                    let gid = attrs.gid.map(nix::unistd::Gid::from_raw);
                    nix::unistd::chown(&attr_path, uid, gid)
                        .map_err(|err| BackendError::Other(err.to_string()))?;
                }

                #[cfg(not(unix))]
                if attrs.uid.is_some() || attrs.gid.is_some() {
                    return Err(BackendError::Unsupported);
                }

                Ok(())
            })
            .await?;
        self.invalidate_path_and_parent(&full_path);
        Ok(())
    }
}

/// Read handle for local filesystem - uses seek + read for random access.
struct LocalReadHandle {
    file: Arc<StdFile>,
    size: u64,
    read_buffers: Arc<LocalReadBufferPool>,
}

impl LocalReadHandle {
    fn read_at_sync(&self, offset: u64, len: u32) -> BackendResult<DataPayload> {
        let len = usize::try_from(len).map_err(|_| {
            BackendError::Other("requested read length does not fit in usize".to_string())
        })?;
        if len > MAX_LOCAL_READ_LEN {
            return Err(BackendError::Other(format!(
                "requested read length {len} exceeds local backend limit {MAX_LOCAL_READ_LEN}"
            )));
        }
        let len = len.min(LOCAL_READ_MAX_SINGLE_CHANNEL_DATA);
        let capacity = len
            .checked_add(LOCAL_READ_PREFIX_RESERVE)
            .ok_or_else(|| BackendError::Other("requested read length overflow".to_string()))?;
        let mut buf = self.read_buffers.take(capacity);
        buf.resize(LOCAL_READ_PREFIX_RESERVE, 0);

        #[cfg(unix)]
        let bytes_read = {
            let spare = buf.spare_capacity_mut();
            read_file_at_uninit(&self.file, &mut spare[..len], offset)
                .map_err(LocalBackend::map_io_error)?
        };

        #[cfg(unix)]
        unsafe {
            buf.set_len(LOCAL_READ_PREFIX_RESERVE + bytes_read);
        }

        #[cfg(not(unix))]
        buf.resize(capacity, 0);

        #[cfg(not(unix))]
        let bytes_read = read_file_at(
            self.file.as_ref(),
            &mut buf[LOCAL_READ_PREFIX_RESERVE..capacity],
            offset,
        )
        .map_err(LocalBackend::map_io_error)?;

        #[cfg(not(unix))]
        buf.truncate(LOCAL_READ_PREFIX_RESERVE + bytes_read);

        let recycler: Arc<dyn ChannelDataRecycler> = self.read_buffers.clone();
        let data = ReusableChannelData::try_new_with_range(
            buf,
            LOCAL_READ_PREFIX_RESERVE,
            bytes_read,
            recycler,
        )
        .ok_or_else(|| BackendError::Other("invalid local read buffer range".to_string()))?;
        Ok(ChannelData::Reusable(data).into())
    }
}

#[derive(Default)]
struct LocalReadBufferPool {
    buffers: ParkingMutex<Vec<Vec<u8>>>,
}

impl LocalReadBufferPool {
    fn take(&self, len: usize) -> Vec<u8> {
        let mut buffers = self.buffers.lock();
        let index = buffers.iter().position(|buf| buf.capacity() >= len);
        let mut buf = index
            .map(|index| buffers.swap_remove(index))
            .unwrap_or_else(|| Vec::with_capacity(len));
        buf.clear();
        buf
    }

    fn put(&self, mut buf: Vec<u8>) {
        if buf.capacity() > MAX_LOCAL_READ_LEN {
            return;
        }

        buf.clear();
        let mut buffers = self.buffers.lock();
        if buffers.len() < MAX_POOLED_READ_BUFFERS {
            buffers.push(buf);
        }
    }
}

impl ChannelDataRecycler for LocalReadBufferPool {
    fn recycle(&self, data: Vec<u8>) {
        self.put(data);
    }
}

#[async_trait]
impl ReadHandle for LocalReadHandle {
    fn try_read_at(&self, offset: u64, len: u32) -> Option<BackendResult<DataPayload>> {
        Some(self.read_at_sync(offset, len))
    }

    async fn read_at(&self, offset: u64, len: u32) -> BackendResult<DataPayload> {
        self.read_at_sync(offset, len)
    }

    fn size(&self) -> u64 {
        self.size
    }
}

/// Write handle for local filesystem - writes directly to file
struct LocalWriteHandle {
    pool: Arc<LocalFsPool>,
    metadata_cache: Arc<LocalMetadataCache>,
    shard: usize,
    file: Arc<StdFile>,
    path: PathBuf,
}

#[async_trait]
impl WriteHandle for LocalWriteHandle {
    async fn write_at(&mut self, offset: u64, data: Bytes) -> BackendResult<()> {
        let file = Arc::clone(&self.file);
        self.pool
            .run_on_shard(self.shard, move || {
                write_all_file_at(file.as_ref(), &data, offset).map_err(LocalBackend::map_io_error)
            })
            .await
    }

    async fn finish(self: Box<Self>) -> BackendResult<()> {
        self.metadata_cache.invalidate_path(&self.path);
        Ok(())
    }

    async fn abort(self: Box<Self>) -> BackendResult<()> {
        // File will be closed on drop, but it may have partial content
        // For a cleaner abort, we'd need to track the path and delete the file
        self.metadata_cache.invalidate_path(&self.path);
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
        let content = Bytes::from(vec![42; LOCAL_READ_MAX_SINGLE_CHANNEL_DATA]);

        backend
            .write_file("test.bin", content.clone())
            .await
            .unwrap();

        let handle = backend.open_read("test.bin").await.unwrap();
        let read = handle
            .read_at(0, LOCAL_READ_MAX_SINGLE_CHANNEL_DATA as u32)
            .await
            .unwrap();

        assert_eq!(read.len(), LOCAL_READ_MAX_SINGLE_CHANNEL_DATA);
        assert_eq!(
            read.as_ref(),
            &content[..LOCAL_READ_MAX_SINGLE_CHANNEL_DATA]
        );
    }

    #[tokio::test]
    async fn test_open_read_rejects_oversized_request_length() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend
            .write_file("test.bin", Bytes::from_static(b"data"))
            .await
            .unwrap();

        let handle = backend.open_read("test.bin").await.unwrap();
        let result = handle.read_at(0, (MAX_LOCAL_READ_LEN + 1) as u32).await;

        assert!(matches!(result, Err(BackendError::Other(_))));
    }

    #[tokio::test]
    async fn test_open_read_caps_payload_to_single_channel_packet() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());
        let content = Bytes::from(vec![7; LOCAL_READ_MAX_SINGLE_CHANNEL_DATA + 1024]);

        backend.write_file("test.bin", content).await.unwrap();

        let handle = backend.open_read("test.bin").await.unwrap();
        let read = handle
            .read_at(0, (LOCAL_READ_MAX_SINGLE_CHANNEL_DATA + 1024) as u32)
            .await
            .unwrap();

        assert_eq!(read.len(), LOCAL_READ_MAX_SINGLE_CHANNEL_DATA);
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
    async fn test_file_info_cache_invalidates_on_rewrite() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend
            .write_file("test.txt", Bytes::from_static(b"old"))
            .await
            .unwrap();
        let original = backend.file_info("test.txt").await.unwrap();
        assert_eq!(original.size, 3);

        backend
            .write_file("test.txt", Bytes::from_static(b"newer-data"))
            .await
            .unwrap();
        let rewritten = backend.file_info("test.txt").await.unwrap();
        assert_eq!(rewritten.size, 10);
    }

    #[tokio::test]
    async fn test_rename_invalidates_cached_paths() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path());

        backend
            .write_file("before.txt", Bytes::from_static(b"data"))
            .await
            .unwrap();
        let original = backend.file_info("before.txt").await.unwrap();
        assert_eq!(original.size, 4);

        backend.rename("before.txt", "after.txt").await.unwrap();

        let old = backend.file_info("before.txt").await;
        assert!(matches!(old, Err(BackendError::NotFound)));

        let renamed = backend.file_info("after.txt").await.unwrap();
        assert_eq!(renamed.size, 4);
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
