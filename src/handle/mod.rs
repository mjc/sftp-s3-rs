use crate::backend::{ReadHandle, SetAttrs, WriteHandle};
use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

const HANDLE_MAGIC: u8 = b'H';
const HANDLE_LEN: usize = 18;

#[derive(Clone, Copy, PartialEq, Eq)]
enum HandleKind {
    Dir = 1,
    Read = 2,
    Write = 3,
}

/// Type alias for shared read handle
pub type SharedReadHandle = Arc<Mutex<Box<dyn ReadHandle>>>;

/// Type alias for shared write handle (Option for take semantics)
pub type SharedWriteHandle = Arc<Mutex<Option<Box<dyn WriteHandle + Send>>>>;

/// Types of file handles
pub enum HandleType {
    /// Directory handle for listing
    Dir { path: Arc<str>, read_done: bool },
    /// Read handle (streaming)
    Read {
        path: Arc<str>,
        handle: SharedReadHandle,
        size: u64,
    },
    /// Write handle (streaming)
    Write {
        path: Arc<str>,
        handle: SharedWriteHandle,
        pending_attrs: SetAttrs,
    },
}

/// Manages file handles for SFTP sessions using numeric IDs
#[must_use]
pub struct HandleManager {
    handles: RwLock<HashMap<u64, HandleType>>,
    next_id: AtomicU64,
    session_cookie: u64,
}

impl HandleManager {
    pub fn new() -> Self {
        let mut session_cookie = [0; 8];
        getrandom::fill(&mut session_cookie).expect("failed to generate SFTP handle cookie");

        Self {
            handles: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            session_cookie: u64::from_be_bytes(session_cookie),
        }
    }

    fn generate_handle(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn encode_handle(&self, kind: HandleKind, id: u64) -> Bytes {
        let mut encoded = [0; HANDLE_LEN];
        encoded[0] = HANDLE_MAGIC;
        encoded[1] = kind as u8;
        encoded[2..10].copy_from_slice(&id.to_be_bytes());
        encoded[10..18].copy_from_slice(&self.session_cookie.to_be_bytes());
        Bytes::copy_from_slice(&encoded)
    }

    /// Parse an opaque SFTP handle from raw bytes.
    fn parse_handle(&self, handle: &[u8], expected_kind: HandleKind) -> Option<u64> {
        let bytes: [u8; HANDLE_LEN] = handle.try_into().ok()?;
        if bytes[0] != HANDLE_MAGIC || bytes[1] != expected_kind as u8 {
            return None;
        }
        let id = u64::from_be_bytes(bytes[2..10].try_into().ok()?);
        let cookie = u64::from_be_bytes(bytes[10..18].try_into().ok()?);
        (cookie == self.session_cookie).then_some(id)
    }

    fn parse_any_handle(&self, handle: &[u8]) -> Option<u64> {
        let bytes: [u8; HANDLE_LEN] = handle.try_into().ok()?;
        if bytes[0] != HANDLE_MAGIC {
            return None;
        }
        if !matches!(
            bytes[1],
            value if value == HandleKind::Dir as u8
                || value == HandleKind::Read as u8
                || value == HandleKind::Write as u8
        ) {
            return None;
        }
        let id = u64::from_be_bytes(bytes[2..10].try_into().ok()?);
        let cookie = u64::from_be_bytes(bytes[10..18].try_into().ok()?);
        (cookie == self.session_cookie).then_some(id)
    }

    pub fn create_dir_handle(&self, path: &str) -> Bytes {
        let id = self.generate_handle();
        self.handles.write().insert(
            id,
            HandleType::Dir {
                path: Arc::from(path),
                read_done: false,
            },
        );
        self.encode_handle(HandleKind::Dir, id)
    }

    pub fn create_read_handle(&self, path: &str, handle: Box<dyn ReadHandle>) -> Bytes {
        let id = self.generate_handle();
        let size = handle.size();
        self.handles.write().insert(
            id,
            HandleType::Read {
                path: Arc::from(path),
                handle: Arc::new(Mutex::new(handle)),
                size,
            },
        );
        self.encode_handle(HandleKind::Read, id)
    }

    pub fn create_write_handle(&self, path: &str, handle: Box<dyn WriteHandle + Send>) -> Bytes {
        let id = self.generate_handle();
        self.handles.write().insert(
            id,
            HandleType::Write {
                path: Arc::from(path),
                handle: Arc::new(Mutex::new(Some(handle))),
                pending_attrs: SetAttrs::default(),
            },
        );
        self.encode_handle(HandleKind::Write, id)
    }

    /// Get a reference to the handle for read operations
    pub fn get_read_handle(&self, handle: &[u8]) -> Option<(Arc<str>, SharedReadHandle, u64)> {
        let id = self.parse_handle(handle, HandleKind::Read)?;
        let handles = self.handles.read();
        match handles.get(&id)? {
            HandleType::Read { path, handle, size } => {
                Some((Arc::clone(path), handle.clone(), *size))
            }
            _ => None,
        }
    }

    /// Get a reference to the write handle
    pub fn get_write_handle(&self, handle: &[u8]) -> Option<(Arc<str>, SharedWriteHandle)> {
        let id = self.parse_handle(handle, HandleKind::Write)?;
        let handles = self.handles.read();
        match handles.get(&id)? {
            HandleType::Write { path, handle, .. } => Some((Arc::clone(path), handle.clone())),
            _ => None,
        }
    }

    pub fn queue_write_attrs(&self, handle: &[u8], attrs: &SetAttrs) -> bool {
        let Some(id) = self.parse_handle(handle, HandleKind::Write) else {
            return false;
        };
        let mut handles = self.handles.write();
        match handles.get_mut(&id) {
            Some(HandleType::Write { pending_attrs, .. }) => {
                pending_attrs.merge_from(attrs);
                true
            }
            _ => false,
        }
    }

    /// Get directory handle info
    pub fn get_dir_handle(&self, handle: &[u8]) -> Option<(Arc<str>, bool)> {
        let id = self.parse_handle(handle, HandleKind::Dir)?;
        let handles = self.handles.read();
        match handles.get(&id)? {
            HandleType::Dir { path, read_done } => Some((Arc::clone(path), *read_done)),
            _ => None,
        }
    }

    /// Mark directory as read
    pub fn mark_dir_read(&self, handle: &[u8]) {
        if let Some(id) = self.parse_handle(handle, HandleKind::Dir) {
            let mut handles = self.handles.write();
            if let Some(HandleType::Dir { read_done, .. }) = handles.get_mut(&id) {
                *read_done = true;
            }
        }
    }

    /// Take the write handle out (for finish/abort)
    pub fn take_write_handle(
        &self,
        handle: &[u8],
    ) -> Option<(Arc<str>, SharedWriteHandle, SetAttrs)> {
        let id = self.parse_handle(handle, HandleKind::Write)?;
        let mut handles = self.handles.write();
        match handles.remove(&id)? {
            HandleType::Write {
                path,
                handle,
                pending_attrs,
            } => Some((path, handle, pending_attrs)),
            other => {
                handles.insert(id, other);
                None
            }
        }
    }

    pub fn remove(&self, handle: &[u8]) {
        if let Some(id) = self.parse_any_handle(handle) {
            self.handles.write().remove(&id);
        }
    }

    /// Check if handle exists and return its type for fstat
    pub fn get_handle_info(&self, handle: &[u8]) -> Option<HandleInfo> {
        let id = self.parse_any_handle(handle)?;
        let handles = self.handles.read();
        match handles.get(&id)? {
            HandleType::Dir { path, .. } => Some(HandleInfo::Dir {
                path: Arc::clone(path),
            }),
            HandleType::Read { path, size, .. } => Some(HandleInfo::Read {
                path: Arc::clone(path),
                size: *size,
            }),
            HandleType::Write { path, .. } => Some(HandleInfo::Write {
                path: Arc::clone(path),
            }),
        }
    }
}

/// Info about a handle for fstat
pub enum HandleInfo {
    Dir { path: Arc<str> },
    Read { path: Arc<str>, size: u64 },
    Write { path: Arc<str> },
}

impl Default for HandleManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BufferedReadHandle;
    use bytes::Bytes;
    use proptest::prelude::*;
    use std::collections::HashSet;

    #[test]
    fn test_handles_are_unique() {
        let manager = HandleManager::new();
        let handles: Vec<Bytes> = (0..1000)
            .map(|i| {
                let read_handle = Box::new(BufferedReadHandle::new(Bytes::new()));
                manager.create_read_handle(&format!("path{}", i), read_handle)
            })
            .collect();
        let unique: HashSet<_> = handles.iter().collect();
        assert_eq!(handles.len(), unique.len());
    }

    #[test]
    fn test_get_returns_created_data() {
        let manager = HandleManager::new();
        let content = Bytes::from_static(b"hello");
        let read_handle = Box::new(BufferedReadHandle::new(content));
        let handle = manager.create_read_handle("test.txt", read_handle);

        let info = manager.get_handle_info(handle.as_ref());
        assert!(info.is_some());
        match info.unwrap() {
            HandleInfo::Read { path, size } => {
                assert_eq!(&*path, "test.txt");
                assert_eq!(size, 5);
            }
            _ => panic!("Wrong handle type"),
        }
    }

    #[test]
    fn test_remove_actually_removes() {
        let manager = HandleManager::new();
        let read_handle = Box::new(BufferedReadHandle::new(Bytes::new()));
        let handle = manager.create_read_handle("test.txt", read_handle);

        assert!(manager.get_handle_info(handle.as_ref()).is_some());
        manager.remove(handle.as_ref());
        assert!(manager.get_handle_info(handle.as_ref()).is_none());
    }

    #[test]
    fn test_handle_from_another_manager_is_rejected() {
        let manager = HandleManager::new();
        let other_manager = HandleManager::new();
        let read_handle = Box::new(BufferedReadHandle::new(Bytes::new()));
        let handle = manager.create_read_handle("test.txt", read_handle);

        assert!(manager.get_handle_info(handle.as_ref()).is_some());
        assert!(other_manager.get_handle_info(handle.as_ref()).is_none());
    }

    #[test]
    fn test_wrong_handle_kind_is_rejected() {
        let manager = HandleManager::new();
        let read_handle = Box::new(BufferedReadHandle::new(Bytes::new()));
        let read_handle = manager.create_read_handle("test.txt", read_handle);
        let dir_handle = manager.create_dir_handle("dir");

        assert!(manager.get_read_handle(read_handle.as_ref()).is_some());
        assert!(manager.get_dir_handle(read_handle.as_ref()).is_none());
        assert!(manager.get_read_handle(dir_handle.as_ref()).is_none());
    }

    #[test]
    fn test_tampered_handle_is_rejected() {
        let manager = HandleManager::new();
        let read_handle = Box::new(BufferedReadHandle::new(Bytes::new()));
        let handle = manager.create_read_handle("test.txt", read_handle);
        let mut tampered = handle.to_vec();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;

        assert!(manager.get_handle_info(handle.as_ref()).is_some());
        assert!(manager.get_handle_info(&tampered).is_none());
    }

    #[test]
    fn test_arc_path_is_same_allocation() {
        // Verify that get_read_handle returns Arc<str> clone (cheap, no heap alloc)
        let manager = HandleManager::new();
        let read_handle = Box::new(BufferedReadHandle::new(Bytes::from_static(b"data")));
        let handle = manager.create_read_handle("mypath", read_handle);

        let (path1, _, _) = manager.get_read_handle(handle.as_ref()).unwrap();
        let (path2, _, _) = manager.get_read_handle(handle.as_ref()).unwrap();
        // Both should point to the same allocation
        assert!(Arc::ptr_eq(&path1, &path2));
    }

    proptest! {
        #[test]
        fn prop_handles_are_unique(count in 1usize..500) {
            let manager = HandleManager::new();
            let handles: Vec<Bytes> = (0..count)
                .map(|i| {
                    let read_handle = Box::new(BufferedReadHandle::new(Bytes::new()));
                    manager.create_read_handle(&format!("path{}", i), read_handle)
                })
                .collect();
            let unique: HashSet<_> = handles.iter().collect();
            prop_assert_eq!(handles.len(), unique.len());
        }

        #[test]
        fn prop_get_returns_created_path(path in "[a-z][a-z0-9]{0,20}") {
            let manager = HandleManager::new();
            let handle = manager.create_dir_handle(&path);
            let data = manager.get_dir_handle(handle.as_ref());
            prop_assert!(data.is_some());
            let (p, _) = data.unwrap();
            prop_assert_eq!(&*p, path.as_str());
        }

        #[test]
        fn prop_remove_clears_handle(path in "[a-z][a-z0-9]{0,20}") {
            let manager = HandleManager::new();
            let read_handle = Box::new(BufferedReadHandle::new(Bytes::new()));
            let handle = manager.create_read_handle(&path, read_handle);
            manager.remove(handle.as_ref());
            prop_assert!(manager.get_handle_info(handle.as_ref()).is_none());
        }

        #[test]
        fn prop_invalid_handle_returns_none(handle in "[a-z]+") {
            let manager = HandleManager::new();
            prop_assert!(manager.get_handle_info(handle.as_bytes()).is_none());
        }
    }
}
