use super::{
    current_timestamp, normalize_path, Backend, BackendCapabilities, BackendError, BackendResult,
    BufferedReadHandle, DirEntry, FileInfo, FileKind, ReadHandle, SetAttrs, WriteHandle,
};
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

#[derive(Debug, Clone)]
struct EntryMeta {
    permissions: u32,
    atime: u32,
    mtime: u32,
    uid: u32,
    gid: u32,
}

impl EntryMeta {
    fn file_default() -> Self {
        let now = current_timestamp();
        Self {
            permissions: 0o644,
            atime: now,
            mtime: now,
            uid: 1000,
            gid: 1000,
        }
    }

    fn dir_default() -> Self {
        let now = current_timestamp();
        Self {
            permissions: 0o755,
            atime: now,
            mtime: now,
            uid: 1000,
            gid: 1000,
        }
    }

    fn symlink_default() -> Self {
        let now = current_timestamp();
        Self {
            permissions: 0o777,
            atime: now,
            mtime: now,
            uid: 1000,
            gid: 1000,
        }
    }

    fn apply(&mut self, attrs: &SetAttrs) {
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

    fn to_info(&self, size: u64, kind: FileKind) -> FileInfo {
        FileInfo {
            size,
            kind,
            is_dir: kind == FileKind::Directory,
            permissions: self.permissions,
            mtime: self.mtime,
            atime: self.atime,
            uid: self.uid,
            gid: self.gid,
        }
    }
}

#[derive(Debug, Clone)]
enum EntryData {
    File { content: Bytes, meta: EntryMeta },
    Symlink { target: String, meta: EntryMeta },
    DirMarker { meta: EntryMeta },
}

impl EntryData {
    fn lstat_info(&self) -> FileInfo {
        match self {
            Self::File { content, meta } => meta.to_info(content.len() as u64, FileKind::File),
            Self::Symlink { target, meta } => meta.to_info(target.len() as u64, FileKind::Symlink),
            Self::DirMarker { meta } => meta.to_info(4096, FileKind::Directory),
        }
    }
}

/// In-memory storage backend for testing and development.
#[must_use]
pub struct MemoryBackend {
    entries: Arc<RwLock<HashMap<String, EntryData>>>,
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create with pre-populated files.
    pub fn with_files(files: HashMap<String, impl Into<Bytes>>) -> Self {
        let entries = files
            .into_iter()
            .map(|(path, content)| {
                (
                    normalize_path(&path).into_owned(),
                    EntryData::File {
                        content: content.into(),
                        meta: EntryMeta::file_default(),
                    },
                )
            })
            .collect();

        Self {
            entries: Arc::new(RwLock::new(entries)),
        }
    }

    fn dir_exists(entries: &HashMap<String, EntryData>, path: &str) -> bool {
        if path.is_empty() {
            return true;
        }

        matches!(entries.get(path), Some(EntryData::DirMarker { .. }))
            || Self::has_descendants(entries, path)
    }

    fn has_descendants(entries: &HashMap<String, EntryData>, path: &str) -> bool {
        let prefix = format!("{path}/");
        entries.keys().any(|key| key.starts_with(&prefix))
    }

    fn resolve_link_target(link_path: &str, target: &str) -> String {
        if target.starts_with('/') {
            return normalize_virtual_path(target);
        }

        let parent = link_path
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("");
        if parent.is_empty() {
            normalize_virtual_path(target)
        } else {
            normalize_virtual_path(&format!("{parent}/{target}"))
        }
    }

    fn resolve_info(
        entries: &HashMap<String, EntryData>,
        path: &str,
        follow_symlinks: bool,
        depth: usize,
    ) -> BackendResult<FileInfo> {
        if depth > 40 {
            return Err(BackendError::Other(
                "too many levels of symbolic links".into(),
            ));
        }

        if path.is_empty() {
            return Ok(FileInfo::directory());
        }

        match entries.get(path) {
            Some(EntryData::File { .. }) | Some(EntryData::DirMarker { .. }) => {
                Ok(entries.get(path).expect("entry exists").lstat_info())
            }
            Some(EntryData::Symlink { target, .. }) if follow_symlinks => {
                let resolved = Self::resolve_link_target(path, target);
                Self::resolve_info(entries, &resolved, true, depth + 1)
            }
            Some(EntryData::Symlink { .. }) => {
                Ok(entries.get(path).expect("entry exists").lstat_info())
            }
            None if Self::has_descendants(entries, path) => Ok(FileInfo::directory()),
            None => Err(BackendError::NotFound),
        }
    }

    fn read_file_resolved(
        entries: &HashMap<String, EntryData>,
        path: &str,
        depth: usize,
    ) -> BackendResult<Bytes> {
        if depth > 40 {
            return Err(BackendError::Other(
                "too many levels of symbolic links".into(),
            ));
        }

        match entries.get(path) {
            Some(EntryData::File { content, .. }) => Ok(content.clone()),
            Some(EntryData::Symlink { target, .. }) => {
                let resolved = Self::resolve_link_target(path, target);
                Self::read_file_resolved(entries, &resolved, depth + 1)
            }
            Some(EntryData::DirMarker { .. }) => Err(BackendError::IsADirectory),
            None if Self::has_descendants(entries, path) => Err(BackendError::IsADirectory),
            None => Err(BackendError::NotFound),
        }
    }

    fn immediate_children(
        entries: &HashMap<String, EntryData>,
        path: &str,
    ) -> BackendResult<Vec<DirEntry>> {
        if !path.is_empty() {
            match entries.get(path) {
                Some(EntryData::DirMarker { .. }) => {}
                Some(_) => return Err(BackendError::NotADirectory),
                None if !Self::has_descendants(entries, path) => {
                    return Err(BackendError::NotFound)
                }
                None => {}
            }
        }

        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };

        let mut seen = HashSet::new();
        let mut children = BTreeMap::new();

        for (key, entry) in entries {
            let relative = if prefix.is_empty() {
                key.as_str()
            } else if let Some(stripped) = key.strip_prefix(&prefix) {
                stripped
            } else {
                continue;
            };

            if relative.is_empty() {
                continue;
            }

            let name = relative.split('/').next().unwrap_or(relative);
            if !seen.insert(name.to_string()) {
                continue;
            }

            let child_path = if path.is_empty() {
                name.to_string()
            } else {
                format!("{path}/{name}")
            };

            let attrs = if relative.contains('/') {
                match entries.get(&child_path) {
                    Some(EntryData::DirMarker { .. }) => FileInfo::directory(),
                    Some(other) => other.lstat_info(),
                    None => FileInfo::directory(),
                }
            } else {
                entry.lstat_info()
            };

            children.insert(
                name.to_string(),
                DirEntry {
                    name: name.to_string(),
                    attrs,
                },
            );
        }

        let mut result = vec![
            DirEntry {
                name: ".".to_string(),
                attrs: FileInfo::directory(),
            },
            DirEntry {
                name: "..".to_string(),
                attrs: FileInfo::directory(),
            },
        ];
        result.extend(children.into_values());
        Ok(result)
    }
}

#[async_trait]
impl Backend for MemoryBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            symlinks: true,
            set_attrs: true,
            delegated_safe_streaming_fallback: true,
        }
    }

    async fn list_dir(&self, path: &str) -> BackendResult<Vec<DirEntry>> {
        let normalized = normalize_path(path);
        let entries = self.entries.read();
        Self::immediate_children(&entries, normalized.as_ref())
    }

    async fn file_info(&self, path: &str) -> BackendResult<FileInfo> {
        let normalized = normalize_path(path);
        let entries = self.entries.read();
        Self::resolve_info(&entries, normalized.as_ref(), true, 0)
    }

    async fn lstat(&self, path: &str) -> BackendResult<FileInfo> {
        let normalized = normalize_path(path);
        let entries = self.entries.read();
        Self::resolve_info(&entries, normalized.as_ref(), false, 0)
    }

    async fn make_dir(&self, path: &str) -> BackendResult<()> {
        let normalized = normalize_path(path).into_owned();
        if normalized.is_empty() {
            return Ok(());
        }

        let mut entries = self.entries.write();
        match entries.get(normalized.as_str()) {
            Some(EntryData::DirMarker { .. }) => return Err(BackendError::AlreadyExists),
            Some(_) => return Err(BackendError::AlreadyExists),
            None if Self::has_descendants(&entries, &normalized) => {
                return Err(BackendError::AlreadyExists)
            }
            None => {}
        }

        entries.insert(
            normalized,
            EntryData::DirMarker {
                meta: EntryMeta::dir_default(),
            },
        );
        Ok(())
    }

    async fn del_dir(&self, path: &str) -> BackendResult<()> {
        let normalized = normalize_path(path).into_owned();
        if normalized.is_empty() {
            return Err(BackendError::PermissionDenied);
        }

        let mut entries = self.entries.write();
        match entries.get(normalized.as_str()) {
            Some(EntryData::DirMarker { .. }) => {
                if Self::has_descendants(&entries, &normalized) {
                    return Err(BackendError::DirectoryNotEmpty);
                }
                entries.remove(&normalized);
                Ok(())
            }
            Some(_) => Err(BackendError::NotADirectory),
            None if Self::has_descendants(&entries, &normalized) => {
                Err(BackendError::DirectoryNotEmpty)
            }
            None => Err(BackendError::NotFound),
        }
    }

    async fn delete(&self, path: &str) -> BackendResult<()> {
        let normalized = normalize_path(path).into_owned();
        let mut entries = self.entries.write();

        match entries.get(normalized.as_str()) {
            Some(EntryData::DirMarker { .. }) => Err(BackendError::IsADirectory),
            Some(_) => {
                entries.remove(&normalized);
                Ok(())
            }
            None if Self::has_descendants(&entries, &normalized) => Err(BackendError::IsADirectory),
            None => Err(BackendError::NotFound),
        }
    }

    async fn rename(&self, src: &str, dst: &str) -> BackendResult<()> {
        let src = normalize_path(src).into_owned();
        let dst = normalize_path(dst).into_owned();
        let mut entries = self.entries.write();

        if dst == src || dst.starts_with(&(src.clone() + "/")) {
            return Err(BackendError::PermissionDenied);
        }

        if matches!(
            entries.get(src.as_str()),
            Some(EntryData::File { .. }) | Some(EntryData::Symlink { .. })
        ) {
            let Some(entry) = entries.remove(src.as_str()) else {
                return Err(BackendError::NotFound);
            };
            entries.insert(dst, entry);
            return Ok(());
        }

        let src_prefix = format!("{src}/");
        if dst != src && dst.starts_with(&src_prefix) {
            return Err(BackendError::Other(
                "cannot rename a directory into its own subtree".into(),
            ));
        }
        let mut moved = Vec::new();
        for key in entries.keys() {
            if key == &src || key.starts_with(&src_prefix) {
                moved.push(key.clone());
            }
        }

        if moved.is_empty() {
            return Err(BackendError::NotFound);
        }

        for old_key in moved {
            let entry = entries.remove(old_key.as_str()).expect("entry exists");
            let new_key = if old_key == src {
                dst.clone()
            } else {
                format!("{dst}/{}", old_key.strip_prefix(&src_prefix).unwrap_or(""))
            };
            entries.insert(new_key, entry);
        }

        Ok(())
    }

    async fn read_file(&self, path: &str) -> BackendResult<Bytes> {
        let normalized = normalize_path(path);
        let entries = self.entries.read();
        Self::read_file_resolved(&entries, normalized.as_ref(), 0)
    }

    async fn write_file(&self, path: &str, content: Bytes) -> BackendResult<()> {
        let normalized = normalize_path(path).into_owned();
        let mut entries = self.entries.write();

        if Self::dir_exists(&entries, &normalized) {
            return Err(BackendError::IsADirectory);
        }

        entries.insert(
            normalized,
            EntryData::File {
                content,
                meta: EntryMeta::file_default(),
            },
        );
        Ok(())
    }

    async fn open_read(&self, path: &str) -> BackendResult<Box<dyn ReadHandle>> {
        let content = self.read_file(path).await?;
        Ok(Box::new(BufferedReadHandle::new(content)))
    }

    async fn open_write(&self, path: &str) -> BackendResult<Box<dyn WriteHandle + Send>> {
        let normalized = normalize_path(path).into_owned();
        Ok(Box::new(MemoryWriteHandle::new(
            normalized,
            self.entries.clone(),
        )))
    }

    async fn read_link(&self, path: &str) -> BackendResult<String> {
        let normalized = normalize_path(path);
        let entries = self.entries.read();
        match entries.get(normalized.as_ref()) {
            Some(EntryData::Symlink { target, .. }) => Ok(target.clone()),
            Some(_) => Err(BackendError::Other("path is not a symlink".into())),
            None => Err(BackendError::NotFound),
        }
    }

    async fn symlink(&self, linkpath: &str, targetpath: &str) -> BackendResult<()> {
        let linkpath = normalize_path(linkpath).into_owned();
        let mut entries = self.entries.write();

        if matches!(
            entries.get(linkpath.as_str()),
            Some(EntryData::DirMarker { .. })
        ) || Self::has_descendants(&entries, &linkpath)
        {
            return Err(BackendError::IsADirectory);
        }
        if entries.contains_key(linkpath.as_str()) {
            return Err(BackendError::AlreadyExists);
        }

        entries.insert(
            linkpath,
            EntryData::Symlink {
                target: targetpath.to_string(),
                meta: EntryMeta::symlink_default(),
            },
        );
        Ok(())
    }

    async fn set_attrs(&self, path: &str, attrs: SetAttrs) -> BackendResult<()> {
        let normalized = normalize_path(path).into_owned();
        let mut entries = self.entries.write();

        if let Some(entry) = entries.get_mut(normalized.as_str()) {
            match entry {
                EntryData::File { content, meta } => {
                    if let Some(size) = attrs.size {
                        let mut bytes = content.to_vec();
                        bytes.resize(size as usize, 0);
                        *content = Bytes::from(bytes);
                    }
                    meta.apply(&attrs);
                    Ok(())
                }
                EntryData::Symlink { meta, .. } => {
                    if attrs.size.is_some() {
                        return Err(BackendError::Unsupported);
                    }
                    meta.apply(&attrs);
                    Ok(())
                }
                EntryData::DirMarker { meta } => {
                    if attrs.size.is_some() {
                        return Err(BackendError::Unsupported);
                    }
                    meta.apply(&attrs);
                    Ok(())
                }
            }
        } else if Self::has_descendants(&entries, &normalized) {
            if attrs.size.is_some() {
                return Err(BackendError::Unsupported);
            }

            let mut meta = EntryMeta::dir_default();
            meta.apply(&attrs);
            entries.insert(normalized, EntryData::DirMarker { meta });
            Ok(())
        } else {
            Err(BackendError::NotFound)
        }
    }
}

/// Write handle for memory backend using sparse chunk storage.
struct MemoryWriteHandle {
    path: String,
    chunks: BTreeMap<u64, Bytes>,
    entries: Arc<RwLock<HashMap<String, EntryData>>>,
}

impl MemoryWriteHandle {
    fn new(path: String, entries: Arc<RwLock<HashMap<String, EntryData>>>) -> Self {
        Self {
            path,
            chunks: BTreeMap::new(),
            entries,
        }
    }
}

#[async_trait]
impl WriteHandle for MemoryWriteHandle {
    async fn write_at(&mut self, offset: u64, data: Bytes) -> BackendResult<()> {
        self.chunks.insert(offset, data);
        Ok(())
    }

    async fn finish(self: Box<Self>) -> BackendResult<()> {
        let content = if self.chunks.is_empty() {
            Bytes::new()
        } else if self.chunks.len() == 1 {
            let (offset, data) = self.chunks.into_iter().next().expect("single chunk");
            if offset == 0 {
                data
            } else {
                let offset = usize::try_from(offset).map_err(|_| {
                    BackendError::Other("file offset too large for this platform".into())
                })?;
                let mut merged =
                    Vec::with_capacity(offset.checked_add(data.len()).ok_or_else(|| {
                        BackendError::Other("file size too large for this platform".into())
                    })?);
                merged.resize(offset, 0);
                merged.extend_from_slice(&data);
                Bytes::from(merged)
            }
        } else {
            let total_size_u64 = self.chunks.iter().fold(0u64, |max, (offset, data)| {
                max.max(offset.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX)))
            });
            let total_size = usize::try_from(total_size_u64)
                .map_err(|_| BackendError::Other("file too large for this platform".into()))?;

            let mut merged = Vec::with_capacity(total_size);
            for (offset, data) in self.chunks {
                let offset = usize::try_from(offset).map_err(|_| {
                    BackendError::Other("file offset too large for this platform".into())
                })?;
                if merged.len() < offset {
                    merged.resize(offset, 0);
                }
                let end = offset.checked_add(data.len()).ok_or_else(|| {
                    BackendError::Other("file size too large for this platform".into())
                })?;
                if merged.len() < end {
                    merged.resize(end, 0);
                }
                merged[offset..end].copy_from_slice(&data);
            }

            Bytes::from(merged)
        };

        self.entries.write().insert(
            self.path,
            EntryData::File {
                content,
                meta: EntryMeta::file_default(),
            },
        );
        Ok(())
    }

    async fn abort(self: Box<Self>) -> BackendResult<()> {
        Ok(())
    }
}

fn normalize_virtual_path(path: &str) -> String {
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            other => components.push(other),
        }
    }
    components.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::Arc;

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

    #[test]
    fn test_normalize_root_variants() {
        for path in ["", "/", ".", "/.", "..", "/.."] {
            assert_eq!(normalize_path(path).as_ref(), "", "{path:?}");
        }
    }

    #[test]
    fn test_normalize_parent_and_current_dir_segments() {
        for (path, expected) in [
            ("/debugdir/..", ""),
            ("a/b/../c.txt", "a/c.txt"),
            ("a/./b.txt", "a/b.txt"),
            ("a//b///c.txt", "a/b/c.txt"),
            ("a/b/../../root.txt", "root.txt"),
        ] {
            assert_eq!(normalize_path(path).as_ref(), expected, "{path:?}");
        }
    }

    #[tokio::test]
    async fn test_list_root() {
        let backend = MemoryBackend::new();
        backend
            .write_file("file1.txt", Bytes::from_static(b"a"))
            .await
            .unwrap();
        backend
            .write_file("dir/file2.txt", Bytes::from_static(b"b"))
            .await
            .unwrap();

        let entries = backend.list_dir("/").await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"."));
        assert!(names.contains(&".."));
        assert!(names.contains(&"file1.txt"));
        assert!(names.contains(&"dir"));
    }

    #[tokio::test]
    async fn test_make_and_list_dir() {
        let backend = MemoryBackend::new();
        backend.make_dir("subdir").await.unwrap();

        let entries = backend.list_dir("/").await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"subdir"));
        assert_eq!(
            backend.file_info("subdir").await.unwrap().kind,
            FileKind::Directory
        );
    }

    #[tokio::test]
    async fn test_list_dir_returns_sorted_immediate_entries() {
        let backend = MemoryBackend::new();
        backend
            .write_file("z.txt", Bytes::from_static(b"z"))
            .await
            .unwrap();
        backend
            .write_file("dir/file.txt", Bytes::from_static(b"nested"))
            .await
            .unwrap();
        backend
            .write_file("a.txt", Bytes::from_static(b"a"))
            .await
            .unwrap();

        let entries = backend.list_dir("/").await.unwrap();
        let names: Vec<_> = entries.into_iter().map(|entry| entry.name).collect();

        assert_eq!(names, vec![".", "..", "a.txt", "dir", "z.txt"]);
    }

    #[tokio::test]
    async fn test_file_info() {
        let backend = MemoryBackend::new();
        backend
            .write_file("test.txt", Bytes::from_static(b"12345"))
            .await
            .unwrap();

        let info = backend.file_info("test.txt").await.unwrap();
        assert_eq!(info.kind, FileKind::File);
        assert!(!info.is_dir);
        assert_eq!(info.size, 5);

        let root_info = backend.file_info("/").await.unwrap();
        assert_eq!(root_info.kind, FileKind::Directory);
        assert!(root_info.is_dir);
    }

    #[tokio::test]
    async fn test_root_path_variants() {
        let backend = MemoryBackend::new();
        backend
            .write_file("test.txt", Bytes::from_static(b"hello"))
            .await
            .unwrap();

        for path in [".", "", "/", "..", "/.."] {
            let info = backend.file_info(path).await.unwrap();
            assert!(info.is_dir, "{path:?} should be root directory");

            let entries = backend.list_dir(path).await.unwrap();
            let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
            assert!(names.contains(&"test.txt"), "{path:?} should list root");
        }
    }

    #[tokio::test]
    async fn test_del_dir_rejects_non_empty_directory() {
        let backend = MemoryBackend::new();
        backend.make_dir("mydir").await.unwrap();
        backend
            .write_file("mydir/file.txt", Bytes::from_static(b"content"))
            .await
            .unwrap();

        let result = backend.del_dir("mydir").await;

        assert!(matches!(result, Err(BackendError::DirectoryNotEmpty)));
        let entries = backend.list_dir("mydir").await.unwrap();
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains(&"file.txt"));
    }

    #[tokio::test]
    async fn test_list_dir_uses_directory_attrs_for_implicit_child_dirs() {
        let backend = MemoryBackend::new();
        backend
            .write_file("dir/child.txt", Bytes::from_static(b"content"))
            .await
            .unwrap();

        let entries = backend.list_dir(".").await.unwrap();
        let dir = entries
            .into_iter()
            .find(|entry| entry.name == "dir")
            .expect("dir entry");

        assert_eq!(dir.attrs.kind, FileKind::Directory);
        assert!(dir.attrs.is_dir);
    }

    #[tokio::test]
    async fn test_rename_rejects_directory_into_own_subtree() {
        let backend = MemoryBackend::new();
        backend.make_dir("a").await.unwrap();
        backend
            .write_file("a/file.txt", Bytes::from_static(b"content"))
            .await
            .unwrap();

        let result = backend.rename("a", "a/b").await;
        assert!(matches!(result, Err(BackendError::PermissionDenied)));

        assert!(backend.file_info("a/file.txt").await.is_ok());
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
                    b.write_file(&format!("file{i}"), content).await
                })
            })
            .collect();

        let results = join_all(tasks).await;
        assert!(results.iter().all(|r| r.is_ok()));

        for i in 0..100u8 {
            let content = backend.read_file(&format!("file{i}")).await.unwrap();
            assert_eq!(content.as_ref(), &vec![i; 100]);
        }
    }

    #[tokio::test]
    async fn test_symlink_readlink_and_stat() {
        let backend = MemoryBackend::new();
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
        let backend = MemoryBackend::new();
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
    async fn test_set_attrs_resize_file() {
        let backend = MemoryBackend::new();
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
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let info = backend.file_info("data.bin").await.unwrap();
        let content = backend.read_file("data.bin").await.unwrap();
        assert_eq!(info.size, 5);
        assert_eq!(info.permissions, 0o600);
        assert_eq!(content.as_ref(), b"abc\0\0");
    }

    #[tokio::test]
    async fn test_set_attrs_size_on_symlink_unsupported() {
        let backend = MemoryBackend::new();
        backend.symlink("link", "target").await.unwrap();

        let result = backend
            .set_attrs(
                "link",
                SetAttrs {
                    size: Some(1),
                    ..Default::default()
                },
            )
            .await;

        assert!(matches!(result, Err(BackendError::Unsupported)));
    }

    #[tokio::test]
    async fn test_delete_rejects_directory() {
        let backend = MemoryBackend::new();
        backend.make_dir("dir").await.unwrap();

        let result = backend.delete("dir").await;
        assert!(matches!(result, Err(BackendError::IsADirectory)));
    }

    #[tokio::test]
    async fn test_rename_directory_moves_descendants() {
        let backend = MemoryBackend::new();
        backend.make_dir("src").await.unwrap();
        backend
            .write_file("src/nested/file.txt", Bytes::from_static(b"data"))
            .await
            .unwrap();
        backend
            .symlink("src/link", "nested/file.txt")
            .await
            .unwrap();

        backend.rename("src", "dst").await.unwrap();

        assert_eq!(
            backend.read_file("dst/nested/file.txt").await.unwrap(),
            Bytes::from_static(b"data")
        );
        assert_eq!(
            backend.read_link("dst/link").await.unwrap(),
            "nested/file.txt"
        );
        assert!(matches!(
            backend.file_info("src").await,
            Err(BackendError::NotFound)
        ));
    }

    proptest! {
        #[test]
        fn prop_normalize_idempotent(path in ".*") {
            let once = normalize_path(&path);
            let twice = normalize_path(&once);
            prop_assert_eq!(once.as_ref(), twice.as_ref());
        }

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
    }

    #[tokio::test]
    async fn test_rsync_temp_file_pattern() {
        let backend = MemoryBackend::new();
        let temp_path = ".file_123.bin.wU0ylU";
        let final_path = "file_123.bin";
        let content = Bytes::from(vec![0xABu8; 1024]);

        let mut handle = backend.open_write(temp_path).await.unwrap();
        handle.write_at(0, content).await.unwrap();
        handle.finish().await.unwrap();

        let read = backend.read_file(temp_path).await.unwrap();
        assert_eq!(read.len(), 1024);

        backend.rename(temp_path, final_path).await.unwrap();

        let final_read = backend.read_file(final_path).await.unwrap();
        assert_eq!(final_read.len(), 1024);

        let temp_result = backend.read_file(temp_path).await;
        assert!(matches!(temp_result, Err(BackendError::NotFound)));
    }

    #[tokio::test]
    async fn test_single_chunk_write_finishes_without_copying() {
        let backend = MemoryBackend::new();
        let content = Bytes::from(vec![0xABu8; 1024]);
        let content_ptr = content.as_ptr();
        let mut handle = backend.open_write("single.bin").await.unwrap();

        handle.write_at(0, content).await.unwrap();
        handle.finish().await.unwrap();

        let read = backend.read_file("single.bin").await.unwrap();
        assert_eq!(read.len(), 1024);
        assert_eq!(read.as_ptr(), content_ptr);
    }

    #[tokio::test]
    async fn test_sparse_write_handle_overwrites_existing_bytes() {
        let backend = MemoryBackend::new();
        let mut handle = backend.open_write("overlap.txt").await.unwrap();

        handle
            .write_at(0, Bytes::from_static(b"abcdef"))
            .await
            .unwrap();
        handle.write_at(2, Bytes::from_static(b"ZZ")).await.unwrap();
        handle.finish().await.unwrap();

        let content = backend.read_file("overlap.txt").await.unwrap();
        assert_eq!(content, Bytes::from_static(b"abZZef"));
    }

    #[tokio::test]
    async fn test_sparse_write_handle_preserves_gaps() {
        let backend = MemoryBackend::new();
        let mut handle = backend.open_write("sparse.bin").await.unwrap();

        handle
            .write_at(3, Bytes::from_static(b"abc"))
            .await
            .unwrap();
        handle.finish().await.unwrap();

        let content = backend.read_file("sparse.bin").await.unwrap();
        assert_eq!(content.as_ref(), b"\0\0\0abc");
    }
}
