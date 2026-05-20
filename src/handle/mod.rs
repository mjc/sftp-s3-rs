use crate::backend::{ReadHandle, WriteHandle};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

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
    },
}

/// Manages file handles for SFTP sessions using numeric IDs
pub struct HandleManager {
    handles: RwLock<HashMap<u64, HandleType>>,
    next_id: AtomicU64,
}

impl HandleManager {
    pub fn new() -> Self {
        Self {
            handles: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn generate_handle(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Parse a handle from raw bytes (ASCII decimal string from SFTP Handle response)
    fn parse_handle(handle: &[u8]) -> Option<u64> {
        std::str::from_utf8(handle).ok()?.parse().ok()
    }

    pub fn create_dir_handle(&self, path: &str) -> String {
        let id = self.generate_handle();
        self.handles.write().insert(
            id,
            HandleType::Dir {
                path: Arc::from(path),
                read_done: false,
            },
        );
        id.to_string()
    }

    pub fn create_read_handle(&self, path: &str, handle: Box<dyn ReadHandle>) -> String {
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
        id.to_string()
    }

    pub fn create_write_handle(&self, path: &str, handle: Box<dyn WriteHandle + Send>) -> String {
        let id = self.generate_handle();
        self.handles.write().insert(
            id,
            HandleType::Write {
                path: Arc::from(path),
                handle: Arc::new(Mutex::new(Some(handle))),
            },
        );
        id.to_string()
    }

    /// Get a reference to the handle for read operations
    pub fn get_read_handle(&self, handle: &[u8]) -> Option<(Arc<str>, SharedReadHandle, u64)> {
        let id = Self::parse_handle(handle)?;
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
        let id = Self::parse_handle(handle)?;
        let handles = self.handles.read();
        match handles.get(&id)? {
            HandleType::Write { path, handle } => Some((Arc::clone(path), handle.clone())),
            _ => None,
        }
    }

    /// Get directory handle info
    pub fn get_dir_handle(&self, handle: &[u8]) -> Option<(Arc<str>, bool)> {
        let id = Self::parse_handle(handle)?;
        let handles = self.handles.read();
        match handles.get(&id)? {
            HandleType::Dir { path, read_done } => Some((Arc::clone(path), *read_done)),
            _ => None,
        }
    }

    /// Mark directory as read
    pub fn mark_dir_read(&self, handle: &[u8]) {
        if let Some(id) = Self::parse_handle(handle) {
            let mut handles = self.handles.write();
            if let Some(HandleType::Dir { read_done, .. }) = handles.get_mut(&id) {
                *read_done = true;
            }
        }
    }

    /// Take the write handle out (for finish/abort)
    pub fn take_write_handle(&self, handle: &[u8]) -> Option<SharedWriteHandle> {
        let id = Self::parse_handle(handle)?;
        let handles = self.handles.read();
        match handles.get(&id)? {
            HandleType::Write { handle, .. } => Some(handle.clone()),
            _ => None,
        }
    }

    pub fn remove(&self, handle: &[u8]) {
        if let Some(id) = Self::parse_handle(handle) {
            self.handles.write().remove(&id);
        }
    }

    /// Check if handle exists and return its type for fstat
    pub fn get_handle_info(&self, handle: &[u8]) -> Option<HandleInfo> {
        let id = Self::parse_handle(handle)?;
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
        let handles: Vec<String> = (0..1000)
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

        let info = manager.get_handle_info(handle.as_bytes());
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

        assert!(manager.get_handle_info(handle.as_bytes()).is_some());
        manager.remove(handle.as_bytes());
        assert!(manager.get_handle_info(handle.as_bytes()).is_none());
    }

    #[test]
    fn test_arc_path_is_same_allocation() {
        // Verify that get_read_handle returns Arc<str> clone (cheap, no heap alloc)
        let manager = HandleManager::new();
        let read_handle = Box::new(BufferedReadHandle::new(Bytes::from_static(b"data")));
        let handle = manager.create_read_handle("mypath", read_handle);

        let (path1, _, _) = manager.get_read_handle(handle.as_bytes()).unwrap();
        let (path2, _, _) = manager.get_read_handle(handle.as_bytes()).unwrap();
        // Both should point to the same allocation
        assert!(Arc::ptr_eq(&path1, &path2));
    }

    proptest! {
        #[test]
        fn prop_handles_are_unique(count in 1usize..500) {
            let manager = HandleManager::new();
            let handles: Vec<String> = (0..count)
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
            let data = manager.get_dir_handle(handle.as_bytes());
            prop_assert!(data.is_some());
            let (p, _) = data.unwrap();
            prop_assert_eq!(&*p, path.as_str());
        }

        #[test]
        fn prop_remove_clears_handle(path in "[a-z][a-z0-9]{0,20}") {
            let manager = HandleManager::new();
            let read_handle = Box::new(BufferedReadHandle::new(Bytes::new()));
            let handle = manager.create_read_handle(&path, read_handle);
            manager.remove(handle.as_bytes());
            prop_assert!(manager.get_handle_info(handle.as_bytes()).is_none());
        }

        #[test]
        fn prop_invalid_handle_returns_none(handle in "[a-z]+") {
            let manager = HandleManager::new();
            // Numeric handles only, so alphabetic strings should return None
            prop_assert!(manager.get_handle_info(handle.as_bytes()).is_none());
        }
    }
}
