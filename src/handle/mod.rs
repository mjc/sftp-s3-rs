use parking_lot::RwLock;
use std::collections::HashMap;
use uuid::Uuid;

/// Types of file handles
#[derive(Debug, Clone)]
pub enum HandleType {
    /// Directory handle for listing
    Dir { path: String, read_done: bool },
    /// Read handle with buffered content
    Read { path: String, content: Vec<u8> },
    /// Write handle with accumulating buffer
    Write { path: String, buffer: Vec<u8> },
}

/// Manages file handles for SFTP sessions
pub struct HandleManager {
    handles: RwLock<HashMap<String, HandleType>>,
}

impl HandleManager {
    pub fn new() -> Self {
        Self {
            handles: RwLock::new(HashMap::new()),
        }
    }

    fn generate_handle() -> String {
        Uuid::new_v4().to_string()
    }

    pub fn create_dir_handle(&self, path: String) -> String {
        let handle = Self::generate_handle();
        self.handles.write().insert(
            handle.clone(),
            HandleType::Dir {
                path,
                read_done: false,
            },
        );
        handle
    }

    pub fn create_read_handle(&self, path: String, content: Vec<u8>) -> String {
        let handle = Self::generate_handle();
        self.handles
            .write()
            .insert(handle.clone(), HandleType::Read { path, content });
        handle
    }

    pub fn create_write_handle(&self, path: String) -> String {
        let handle = Self::generate_handle();
        self.handles.write().insert(
            handle.clone(),
            HandleType::Write {
                path,
                buffer: Vec::new(),
            },
        );
        handle
    }

    pub fn get(&self, handle: &str) -> Option<HandleType> {
        self.handles.read().get(handle).cloned()
    }

    pub fn update(&self, handle: &str, data: HandleType) {
        self.handles.write().insert(handle.to_string(), data);
    }

    pub fn remove(&self, handle: &str) -> Option<HandleType> {
        self.handles.write().remove(handle)
    }
}

impl Default for HandleManager {
    fn default() -> Self {
        Self::new()
    }
}
