use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Types of file handles
#[derive(Debug, Clone)]
pub enum HandleType {
    /// Directory handle for listing
    Dir { path: String, read_done: bool },
    /// Read handle with buffered content (Bytes clone is O(1))
    Read { path: String, content: Bytes },
    /// Write handle with accumulating buffer
    Write { path: String, buffer: Vec<u8> },
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

    pub fn create_read_handle(&self, path: String, content: Bytes) -> String {
        let id = self.generate_handle();
        self.handles
            .write()
            .insert(id, HandleType::Read { path, content });
        id.to_string()
    }

    pub fn create_write_handle(&self, path: String) -> String {
        let id = self.generate_handle();
        self.handles.write().insert(
            id,
            HandleType::Write {
                path,
                buffer: Vec::new(),
            },
        );
        id.to_string()
    }

    pub fn get(&self, handle: &str) -> Option<HandleType> {
        let id: u64 = handle.parse().ok()?;
        self.handles.read().get(&id).cloned()
    }

    pub fn update(&self, handle: &str, data: HandleType) {
        if let Ok(id) = handle.parse::<u64>() {
            self.handles.write().insert(id, data);
        }
    }

    pub fn remove(&self, handle: &str) -> Option<HandleType> {
        let id: u64 = handle.parse().ok()?;
        self.handles.write().remove(&id)
    }
}

impl Default for HandleManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashSet;

    #[test]
    fn test_handles_are_unique() {
        let manager = HandleManager::new();
        let handles: Vec<String> = (0..1000)
            .map(|i| manager.create_write_handle(format!("path{}", i)))
            .collect();
        let unique: HashSet<_> = handles.iter().collect();
        assert_eq!(handles.len(), unique.len());
    }

    #[test]
    fn test_get_returns_created_data() {
        let manager = HandleManager::new();
        let content = Bytes::from_static(b"hello");
        let handle = manager.create_read_handle("test.txt".to_string(), content.clone());

        let data = manager.get(&handle);
        assert!(data.is_some());
        match data.unwrap() {
            HandleType::Read { path, content: c } => {
                assert_eq!(path, "test.txt");
                assert_eq!(c, content);
            }
            _ => panic!("Wrong handle type"),
        }
    }

    #[test]
    fn test_remove_actually_removes() {
        let manager = HandleManager::new();
        let handle = manager.create_write_handle("test.txt".to_string());

        assert!(manager.get(&handle).is_some());
        manager.remove(&handle);
        assert!(manager.get(&handle).is_none());
    }

    #[test]
    fn test_update_modifies_data() {
        let manager = HandleManager::new();
        let handle = manager.create_write_handle("test.txt".to_string());

        manager.update(
            &handle,
            HandleType::Write {
                path: "test.txt".to_string(),
                buffer: vec![1, 2, 3],
            },
        );

        match manager.get(&handle).unwrap() {
            HandleType::Write { buffer, .. } => {
                assert_eq!(buffer, vec![1, 2, 3]);
            }
            _ => panic!("Wrong handle type"),
        }
    }

    proptest! {
        #[test]
        fn prop_handles_are_unique(count in 1usize..500) {
            let manager = HandleManager::new();
            let handles: Vec<String> = (0..count)
                .map(|i| manager.create_write_handle(format!("path{}", i)))
                .collect();
            let unique: HashSet<_> = handles.iter().collect();
            prop_assert_eq!(handles.len(), unique.len());
        }

        #[test]
        fn prop_get_returns_created_path(path in "[a-z][a-z0-9]{0,20}") {
            let manager = HandleManager::new();
            let handle = manager.create_dir_handle(path.clone());
            let data = manager.get(&handle);
            prop_assert!(data.is_some());
            match data.unwrap() {
                HandleType::Dir { path: p, .. } => {
                    prop_assert_eq!(p, path);
                }
                _ => prop_assert!(false, "Wrong handle type"),
            }
        }

        #[test]
        fn prop_remove_returns_data(path in "[a-z][a-z0-9]{0,20}") {
            let manager = HandleManager::new();
            let handle = manager.create_write_handle(path.clone());
            let removed = manager.remove(&handle);
            prop_assert!(removed.is_some());
            prop_assert!(manager.get(&handle).is_none());
        }

        #[test]
        fn prop_invalid_handle_returns_none(handle in "[a-z]+") {
            let manager = HandleManager::new();
            // Numeric handles only, so alphabetic strings should return None
            prop_assert!(manager.get(&handle).is_none());
        }
    }
}
