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
    Dir { path: String, read_done: bool },
    /// Read handle (streaming)
    Read {
        path: String,
        handle: SharedReadHandle,
        size: u64,
    },
    /// Write handle (streaming)
    Write {
        path: String,
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

    pub fn create_dir_handle(&self, path: String) -> String {
        let id = self.generate_handle();
        self.handles.write().insert(
            id,
            HandleType::Dir {
                path,
                read_done: false,
            },
        );
        id.to_string()
    }

    pub fn create_read_handle(&self, path: String, handle: Box<dyn ReadHandle>) -> String {
        let id = self.generate_handle();
        let size = handle.size();
        self.handles.write().insert(
            id,
            HandleType::Read {
                path,
                handle: Arc::new(Mutex::new(handle)),
                size,
            },
        );
        id.to_string()
    }

    pub fn create_write_handle(&self, path: String, handle: Box<dyn WriteHandle + Send>) -> String {
        let id = self.generate_handle();
        self.handles.write().insert(
            id,
            HandleType::Write {
                path,
                handle: Arc::new(Mutex::new(Some(handle))),
            },
        );
        id.to_string()
    }

    /// Get a reference to the handle for read operations
    pub fn get_read_handle(&self, handle: &str) -> Option<(String, SharedReadHandle, u64)> {
        let id: u64 = handle.parse().ok()?;
        let handles = self.handles.read();
        match handles.get(&id)? {
            HandleType::Read { path, handle, size } => Some((path.clone(), handle.clone(), *size)),
            _ => None,
        }
    }

    /// Get a reference to the write handle
    pub fn get_write_handle(&self, handle: &str) -> Option<(String, SharedWriteHandle)> {
        let id: u64 = handle.parse().ok()?;
        let handles = self.handles.read();
        match handles.get(&id)? {
            HandleType::Write { path, handle } => Some((path.clone(), handle.clone())),
            _ => None,
        }
    }

    /// Get directory handle info
    pub fn get_dir_handle(&self, handle: &str) -> Option<(String, bool)> {
        let id: u64 = handle.parse().ok()?;
        let handles = self.handles.read();
        match handles.get(&id)? {
            HandleType::Dir { path, read_done } => Some((path.clone(), *read_done)),
            _ => None,
        }
    }

    /// Mark directory as read
    pub fn mark_dir_read(&self, handle: &str) {
        if let Ok(id) = handle.parse::<u64>() {
            let mut handles = self.handles.write();
            if let Some(HandleType::Dir { read_done, .. }) = handles.get_mut(&id) {
                *read_done = true;
            }
        }
    }

    /// Take the write handle out (for finish/abort)
    pub fn take_write_handle(&self, handle: &str) -> Option<SharedWriteHandle> {
        let id: u64 = handle.parse().ok()?;
        let handles = self.handles.read();
        match handles.get(&id)? {
            HandleType::Write { handle, .. } => Some(handle.clone()),
            _ => None,
        }
    }

    pub fn remove(&self, handle: &str) {
        if let Ok(id) = handle.parse::<u64>() {
            self.handles.write().remove(&id);
        }
    }

    /// Check if handle exists and return its type for fstat
    pub fn get_handle_info(&self, handle: &str) -> Option<HandleInfo> {
        let id: u64 = handle.parse().ok()?;
        let handles = self.handles.read();
        match handles.get(&id)? {
            HandleType::Dir { path, .. } => Some(HandleInfo::Dir { path: path.clone() }),
            HandleType::Read { path, size, .. } => Some(HandleInfo::Read {
                path: path.clone(),
                size: *size,
            }),
            HandleType::Write { path, .. } => Some(HandleInfo::Write { path: path.clone() }),
        }
    }
}

/// Info about a handle for fstat
pub enum HandleInfo {
    Dir { path: String },
    Read { path: String, size: u64 },
    Write { path: String },
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
                manager.create_read_handle(format!("path{}", i), read_handle)
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
        let handle = manager.create_read_handle("test.txt".to_string(), read_handle);

        let info = manager.get_handle_info(&handle);
        assert!(info.is_some());
        match info.unwrap() {
            HandleInfo::Read { path, size } => {
                assert_eq!(path, "test.txt");
                assert_eq!(size, 5);
            }
            _ => panic!("Wrong handle type"),
        }
    }

    #[test]
    fn test_remove_actually_removes() {
        let manager = HandleManager::new();
        let read_handle = Box::new(BufferedReadHandle::new(Bytes::new()));
        let handle = manager.create_read_handle("test.txt".to_string(), read_handle);

        assert!(manager.get_handle_info(&handle).is_some());
        manager.remove(&handle);
        assert!(manager.get_handle_info(&handle).is_none());
    }

    proptest! {
        #[test]
        fn prop_handles_are_unique(count in 1usize..500) {
            let manager = HandleManager::new();
            let handles: Vec<String> = (0..count)
                .map(|i| {
                    let read_handle = Box::new(BufferedReadHandle::new(Bytes::new()));
                    manager.create_read_handle(format!("path{}", i), read_handle)
                })
                .collect();
            let unique: HashSet<_> = handles.iter().collect();
            prop_assert_eq!(handles.len(), unique.len());
        }

        #[test]
        fn prop_get_returns_created_path(path in "[a-z][a-z0-9]{0,20}") {
            let manager = HandleManager::new();
            let handle = manager.create_dir_handle(path.clone());
            let data = manager.get_dir_handle(&handle);
            prop_assert!(data.is_some());
            let (p, _) = data.unwrap();
            prop_assert_eq!(p, path);
        }

        #[test]
        fn prop_remove_clears_handle(path in "[a-z][a-z0-9]{0,20}") {
            let manager = HandleManager::new();
            let read_handle = Box::new(BufferedReadHandle::new(Bytes::new()));
            let handle = manager.create_read_handle(path.clone(), read_handle);
            manager.remove(&handle);
            prop_assert!(manager.get_handle_info(&handle).is_none());
        }

        #[test]
        fn prop_invalid_handle_returns_none(handle in "[a-z]+") {
            let manager = HandleManager::new();
            // Numeric handles only, so alphabetic strings should return None
            prop_assert!(manager.get_handle_info(&handle).is_none());
        }
    }
}
