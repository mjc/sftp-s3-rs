use super::{
    current_timestamp, normalize_path, Backend, BackendError, BackendResult, DirEntry, FileInfo,
};
use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use bytes::Bytes;
use std::collections::HashSet;
use tracing::debug;

/// Marker file for empty directories (matching Elixir implementation)
const KEEP_MARKER: &str = ".keep";

/// S3 storage backend configuration
#[derive(Debug, Clone)]
pub struct S3Config {
    /// S3 bucket name (required)
    pub bucket: String,
    /// Key prefix for all objects (optional, for multi-tenant setups)
    pub prefix: String,
}

impl S3Config {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            prefix: String::new(),
        }
    }

    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }
}

/// S3 storage backend
pub struct S3Backend {
    client: Client,
    config: S3Config,
}

impl S3Backend {
    /// Create a new S3 backend with the given client and configuration
    pub fn new(client: Client, config: S3Config) -> Self {
        Self { client, config }
    }

    /// Create from AWS SDK config loaded from environment
    pub async fn from_env(config: S3Config) -> Self {
        let aws_config = aws_config::load_from_env().await;
        let client = Client::new(&aws_config);
        Self::new(client, config)
    }

    /// Build the full S3 key from a path
    fn build_key(&self, path: &str) -> String {
        let normalized = normalize_path(path);
        if self.config.prefix.is_empty() {
            normalized.into_owned()
        } else if normalized.is_empty() {
            self.config.prefix.trim_end_matches('/').to_string()
        } else {
            format!(
                "{}/{}",
                self.config.prefix.trim_end_matches('/'),
                normalized
            )
        }
    }

    /// Convert S3 error to BackendError
    fn map_s3_error(err: impl std::fmt::Display) -> BackendError {
        let msg = err.to_string();
        if msg.contains("NoSuchKey") || msg.contains("NotFound") || msg.contains("404") {
            BackendError::NotFound
        } else if msg.contains("AccessDenied") || msg.contains("403") {
            BackendError::PermissionDenied
        } else {
            BackendError::Io(msg)
        }
    }

    /// Parse AWS DateTime to Unix timestamp
    fn parse_datetime(dt: &aws_sdk_s3::primitives::DateTime) -> u32 {
        dt.secs() as u32
    }
}

#[async_trait]
impl Backend for S3Backend {
    async fn list_dir(&self, path: &str) -> BackendResult<Vec<DirEntry>> {
        let normalized = normalize_path(path);
        let prefix = if normalized.is_empty() {
            if self.config.prefix.is_empty() {
                String::new()
            } else {
                format!("{}/", self.config.prefix.trim_end_matches('/'))
            }
        } else {
            format!("{}/", self.build_key(normalized.as_ref()))
        };

        debug!(prefix = %prefix, "Listing S3 objects");

        let result = self
            .client
            .list_objects_v2()
            .bucket(&self.config.bucket)
            .prefix(&prefix)
            .send()
            .await
            .map_err(Self::map_s3_error)?;

        let mut seen = HashSet::new();
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

        if let Some(contents) = result.contents {
            for obj in contents {
                if let Some(key) = obj.key {
                    let relative = if prefix.is_empty() {
                        key.clone()
                    } else {
                        key.strip_prefix(&prefix).unwrap_or(&key).to_string()
                    };

                    // Get first path component
                    let name = relative.split('/').next().unwrap_or(&relative);

                    // Skip empty names and .keep markers at root level
                    if name.is_empty() || name == KEEP_MARKER {
                        continue;
                    }

                    if seen.insert(name.to_string()) {
                        // Determine if directory (has objects under it) or file
                        let is_dir = relative.contains('/');
                        let mtime = obj
                            .last_modified
                            .as_ref()
                            .map(Self::parse_datetime)
                            .unwrap_or_else(current_timestamp);
                        let size = obj.size.unwrap_or(0) as u64;

                        let attrs = if is_dir {
                            FileInfo::directory_with_mtime(mtime)
                        } else {
                            FileInfo::file_with_mtime(size, mtime)
                        };

                        entries.push(DirEntry {
                            name: name.to_string(),
                            attrs,
                        });
                    }
                }
            }
        }

        Ok(entries)
    }

    async fn file_info(&self, path: &str) -> BackendResult<FileInfo> {
        let normalized = normalize_path(path);

        // Root is always a directory
        if normalized.is_empty() {
            return Ok(FileInfo::directory());
        }

        let key = self.build_key(normalized.as_ref());

        // Try to get the object directly (file case)
        match self
            .client
            .head_object()
            .bucket(&self.config.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(result) => {
                let size = result.content_length.unwrap_or(0) as u64;
                let mtime = result
                    .last_modified
                    .as_ref()
                    .map(Self::parse_datetime)
                    .unwrap_or_else(current_timestamp);
                return Ok(FileInfo::file_with_mtime(size, mtime));
            }
            Err(_) => {
                // Not a file, check if it's a directory
            }
        }

        // Check if it's a directory (has objects with this prefix)
        let prefix = format!("{}/", key);
        let result = self
            .client
            .list_objects_v2()
            .bucket(&self.config.bucket)
            .prefix(&prefix)
            .max_keys(1)
            .send()
            .await
            .map_err(Self::map_s3_error)?;

        if result.contents.map(|c| !c.is_empty()).unwrap_or(false) {
            Ok(FileInfo::directory())
        } else {
            Err(BackendError::NotFound)
        }
    }

    async fn make_dir(&self, path: &str) -> BackendResult<()> {
        let key = format!("{}/{}", self.build_key(path), KEEP_MARKER);

        self.client
            .put_object()
            .bucket(&self.config.bucket)
            .key(&key)
            .body(ByteStream::from_static(b""))
            .send()
            .await
            .map_err(Self::map_s3_error)?;

        Ok(())
    }

    async fn del_dir(&self, path: &str) -> BackendResult<()> {
        let key = format!("{}/{}", self.build_key(path), KEEP_MARKER);

        self.client
            .delete_object()
            .bucket(&self.config.bucket)
            .key(&key)
            .send()
            .await
            .map_err(Self::map_s3_error)?;

        Ok(())
    }

    async fn delete(&self, path: &str) -> BackendResult<()> {
        let key = self.build_key(path);

        self.client
            .delete_object()
            .bucket(&self.config.bucket)
            .key(&key)
            .send()
            .await
            .map_err(Self::map_s3_error)?;

        Ok(())
    }

    async fn rename(&self, src: &str, dst: &str) -> BackendResult<()> {
        let src_key = self.build_key(src);
        let dst_key = self.build_key(dst);
        let copy_source = format!("{}/{}", self.config.bucket, src_key);

        // Copy to new location
        self.client
            .copy_object()
            .bucket(&self.config.bucket)
            .copy_source(&copy_source)
            .key(&dst_key)
            .send()
            .await
            .map_err(Self::map_s3_error)?;

        // Delete original
        self.client
            .delete_object()
            .bucket(&self.config.bucket)
            .key(&src_key)
            .send()
            .await
            .map_err(Self::map_s3_error)?;

        Ok(())
    }

    async fn read_file(&self, path: &str) -> BackendResult<Bytes> {
        let key = self.build_key(path);

        let result = self
            .client
            .get_object()
            .bucket(&self.config.bucket)
            .key(&key)
            .send()
            .await
            .map_err(Self::map_s3_error)?;

        let bytes = result
            .body
            .collect()
            .await
            .map_err(|e| BackendError::Io(e.to_string()))?
            .into_bytes();

        Ok(bytes) // No .to_vec() needed - already Bytes!
    }

    async fn write_file(&self, path: &str, content: Bytes) -> BackendResult<()> {
        let key = self.build_key(path);

        self.client
            .put_object()
            .bucket(&self.config.bucket)
            .key(&key)
            .body(ByteStream::from(content))
            .send()
            .await
            .map_err(Self::map_s3_error)?;

        Ok(())
    }
}
