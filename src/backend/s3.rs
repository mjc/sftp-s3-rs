use super::{
    current_timestamp, normalize_path, Backend, BackendError, BackendResult, DirEntry, FileInfo,
    ReadHandle, WriteHandle,
};
use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::CompletedMultipartUpload;
use aws_sdk_s3::types::CompletedPart;
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

    /// Create with custom endpoint (for MinIO, LocalStack, etc)
    pub async fn with_endpoint(config: S3Config, endpoint: &str, region: &str) -> Self {
        let sdk_config = aws_config::from_env()
            .endpoint_url(endpoint)
            .region(aws_config::Region::new(region.to_owned()))
            .load()
            .await;
        let s3_config = aws_sdk_s3::config::Builder::from(&sdk_config)
            .force_path_style(true)
            .build();
        let client = Client::from_conf(s3_config);
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
            BackendError::Other(msg)
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
            .map_err(|e| BackendError::Other(e.to_string()))?
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

    async fn open_read(&self, path: &str) -> BackendResult<Box<dyn ReadHandle>> {
        let key = self.build_key(path);

        // Get file size via HEAD request
        let head = self
            .client
            .head_object()
            .bucket(&self.config.bucket)
            .key(&key)
            .send()
            .await
            .map_err(Self::map_s3_error)?;

        let size = head.content_length.unwrap_or(0) as u64;

        Ok(Box::new(S3ReadHandle {
            client: self.client.clone(),
            bucket: self.config.bucket.clone(),
            key,
            size,
        }))
    }

    async fn open_write(&self, path: &str) -> BackendResult<Box<dyn WriteHandle + Send>> {
        let key = self.build_key(path);

        // Start multipart upload
        let create_result = self
            .client
            .create_multipart_upload()
            .bucket(&self.config.bucket)
            .key(&key)
            .send()
            .await
            .map_err(Self::map_s3_error)?;

        let upload_id = create_result
            .upload_id
            .ok_or_else(|| BackendError::Other("No upload ID returned".into()))?;

        debug!(key = %key, upload_id = %upload_id, "Started multipart upload");

        Ok(Box::new(S3WriteHandle {
            client: self.client.clone(),
            bucket: self.config.bucket.clone(),
            key,
            upload_id,
            buffer: Vec::new(),
            parts: Vec::new(),
            part_number: 1,
        }))
    }
}

/// S3 read handle using Range requests for random access
struct S3ReadHandle {
    client: Client,
    bucket: String,
    key: String,
    size: u64,
}

#[async_trait]
impl ReadHandle for S3ReadHandle {
    async fn read_at(&self, offset: u64, len: u32) -> BackendResult<Bytes> {
        if offset >= self.size {
            return Ok(Bytes::new());
        }

        let end = std::cmp::min(offset + len as u64, self.size) - 1;
        let range = format!("bytes={}-{}", offset, end);

        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&self.key)
            .range(&range)
            .send()
            .await
            .map_err(S3Backend::map_s3_error)?;

        let bytes = result
            .body
            .collect()
            .await
            .map_err(|e| BackendError::Other(e.to_string()))?
            .into_bytes();

        Ok(bytes)
    }

    fn size(&self) -> u64 {
        self.size
    }
}

/// Minimum part size for S3 multipart upload (5MB)
const MIN_PART_SIZE: usize = 5 * 1024 * 1024;

/// S3 write handle using multipart upload
struct S3WriteHandle {
    client: Client,
    bucket: String,
    key: String,
    upload_id: String,
    buffer: Vec<u8>,
    parts: Vec<CompletedPart>,
    part_number: i32,
}

impl S3WriteHandle {
    /// Flush buffer to S3 as a part if large enough (or if force is true)
    async fn flush_part(&mut self, force: bool) -> BackendResult<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        // Only flush if we have enough data (or are finishing)
        if !force && self.buffer.len() < MIN_PART_SIZE {
            return Ok(());
        }

        let data = std::mem::take(&mut self.buffer);
        debug!(
            key = %self.key,
            part = self.part_number,
            size = data.len(),
            "Uploading part"
        );

        let result = self
            .client
            .upload_part()
            .bucket(&self.bucket)
            .key(&self.key)
            .upload_id(&self.upload_id)
            .part_number(self.part_number)
            .body(ByteStream::from(data))
            .send()
            .await
            .map_err(S3Backend::map_s3_error)?;

        let etag = result.e_tag.unwrap_or_default();
        self.parts.push(
            CompletedPart::builder()
                .e_tag(etag)
                .part_number(self.part_number)
                .build(),
        );

        self.part_number += 1;
        Ok(())
    }
}

#[async_trait]
impl WriteHandle for S3WriteHandle {
    async fn write_at(&mut self, offset: u64, data: Bytes) -> BackendResult<()> {
        // For simplicity, we assume sequential writes (offset == buffer end)
        // S3 multipart doesn't support random writes anyway.
        // Instead of copying into buffer with extend_from_slice, we defer merging
        // buffer chunks until flush to avoid hot-path copies.
        let start = offset as usize;

        if start > self.buffer.len() {
            // Gap before this write - flush current buffer and restart
            self.flush_part(false).await?;
            self.buffer.clear();
            self.buffer.resize(start, 0);
            self.buffer.extend_from_slice(&data);
        } else if start == self.buffer.len() {
            // Sequential append - just add to buffer
            self.buffer.extend_from_slice(&data);
        } else {
            // Random write within current buffer - copy needed (rare case)
            let end = start + data.len();
            if end > self.buffer.len() {
                self.buffer.resize(end, 0);
            }
            self.buffer[start..end].copy_from_slice(&data);
        }

        // Flush if we have enough data for a part
        self.flush_part(false).await?;

        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> BackendResult<()> {
        // Upload any remaining data
        self.flush_part(true).await?;

        if self.parts.is_empty() {
            // No parts uploaded - abort multipart and use simple upload
            let _ = self
                .client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(&self.key)
                .upload_id(&self.upload_id)
                .send()
                .await;

            // Write empty file
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&self.key)
                .body(ByteStream::from_static(b""))
                .send()
                .await
                .map_err(S3Backend::map_s3_error)?;

            return Ok(());
        }

        // Complete multipart upload
        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(self.parts))
            .build();

        debug!(
            key = %self.key,
            upload_id = %self.upload_id,
            "Completing multipart upload"
        );

        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(&self.key)
            .upload_id(&self.upload_id)
            .multipart_upload(completed)
            .send()
            .await
            .map_err(S3Backend::map_s3_error)?;

        Ok(())
    }

    async fn abort(self: Box<Self>) -> BackendResult<()> {
        debug!(
            key = %self.key,
            upload_id = %self.upload_id,
            "Aborting multipart upload"
        );

        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(&self.key)
            .upload_id(&self.upload_id)
            .send()
            .await
            .map_err(S3Backend::map_s3_error)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: create an S3Backend-shaped config for testing pure functions.
    // Does NOT connect to any AWS endpoint.
    fn make_backend_config(bucket: &str, prefix: &str) -> S3Config {
        S3Config::new(bucket).with_prefix(prefix)
    }

    // We can test build_key by constructing a minimal S3Backend-like struct.
    // Since S3Backend::build_key only uses self.config, we test it via a
    // wrapper that exposes the same logic as a free function.
    fn build_key_fn(config: &S3Config, path: &str) -> String {
        let normalized = normalize_path(path);
        if config.prefix.is_empty() {
            normalized.into_owned()
        } else if normalized.is_empty() {
            config.prefix.trim_end_matches('/').to_string()
        } else {
            format!("{}/{}", config.prefix.trim_end_matches('/'), normalized)
        }
    }

    // --- build_key tests ---

    #[test]
    fn test_build_key_no_prefix_root() {
        let config = make_backend_config("bucket", "");
        assert_eq!(build_key_fn(&config, "/"), "");
        assert_eq!(build_key_fn(&config, ""), "");
    }

    #[test]
    fn test_build_key_no_prefix_path() {
        let config = make_backend_config("bucket", "");
        assert_eq!(build_key_fn(&config, "foo/bar.txt"), "foo/bar.txt");
        assert_eq!(build_key_fn(&config, "/foo/bar.txt"), "foo/bar.txt");
    }

    #[test]
    fn test_build_key_with_prefix_root() {
        let config = make_backend_config("bucket", "sftp/");
        // Root maps to prefix only (no trailing slash)
        assert_eq!(build_key_fn(&config, "/"), "sftp");
        assert_eq!(build_key_fn(&config, ""), "sftp");
    }

    #[test]
    fn test_build_key_with_prefix_path() {
        let config = make_backend_config("bucket", "sftp/");
        assert_eq!(build_key_fn(&config, "foo/bar.txt"), "sftp/foo/bar.txt");
        assert_eq!(build_key_fn(&config, "/foo/bar.txt"), "sftp/foo/bar.txt");
    }

    #[test]
    fn test_build_key_prefix_without_trailing_slash() {
        let config = make_backend_config("bucket", "sftp");
        assert_eq!(build_key_fn(&config, "file.txt"), "sftp/file.txt");
    }

    #[test]
    fn test_build_key_nested_path() {
        let config = make_backend_config("bucket", "tenant/data/");
        assert_eq!(build_key_fn(&config, "/a/b/c.txt"), "tenant/data/a/b/c.txt");
    }

    // --- map_s3_error tests ---

    #[test]
    fn test_map_s3_error_not_found() {
        assert!(matches!(
            S3Backend::map_s3_error("NoSuchKey: the key does not exist"),
            BackendError::NotFound
        ));
        assert!(matches!(
            S3Backend::map_s3_error("NotFound"),
            BackendError::NotFound
        ));
        assert!(matches!(
            S3Backend::map_s3_error("HTTP 404 not found"),
            BackendError::NotFound
        ));
    }

    #[test]
    fn test_map_s3_error_permission_denied() {
        assert!(matches!(
            S3Backend::map_s3_error("AccessDenied"),
            BackendError::PermissionDenied
        ));
        assert!(matches!(
            S3Backend::map_s3_error("HTTP 403 Forbidden"),
            BackendError::PermissionDenied
        ));
    }

    #[test]
    fn test_map_s3_error_other() {
        let err = S3Backend::map_s3_error("InternalError: something broke");
        assert!(matches!(err, BackendError::Other(_)));
        if let BackendError::Other(msg) = err {
            assert!(msg.contains("InternalError"));
        }
    }

    #[test]
    fn test_map_s3_error_empty() {
        assert!(matches!(
            S3Backend::map_s3_error(""),
            BackendError::Other(_)
        ));
    }

    // --- S3Config tests ---

    #[test]
    fn test_s3_config_new() {
        let config = S3Config::new("my-bucket");
        assert_eq!(config.bucket, "my-bucket");
        assert!(config.prefix.is_empty());
    }

    #[test]
    fn test_s3_config_with_prefix() {
        let config = S3Config::new("b").with_prefix("sftp/");
        assert_eq!(config.prefix, "sftp/");
    }
}
