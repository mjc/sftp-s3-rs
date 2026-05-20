use super::{
    current_timestamp, normalize_path, unix_secs_to_u32, Backend, BackendError, BackendResult,
    DirEntry, FileInfo, ReadHandle, WriteHandle,
};
use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::CompletedMultipartUpload;
use aws_sdk_s3::types::CompletedPart;
use aws_sdk_s3::Client;
use bytes::Bytes;
use std::collections::BTreeMap;
use std::fmt::{Debug, Display};
use tracing::debug;

/// Marker file for empty directories (matching Elixir implementation)
const KEEP_MARKER: &str = ".keep";
const DIRECTORY_LISTING_MTIME: u32 = 0;

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
    fn map_s3_error(err: impl Display + Debug) -> BackendError {
        let msg = err.to_string();
        let debug_msg = format!("{err:?}");
        let searchable = format!("{msg}\n{debug_msg}").to_ascii_lowercase();

        if searchable.contains("nosuchkey")
            || searchable.contains("notfound")
            || searchable.contains("not found")
            || searchable.contains("404")
        {
            BackendError::NotFound
        } else if searchable.contains("accessdenied")
            || searchable.contains("access denied")
            || searchable.contains("forbidden")
            || searchable.contains("403")
        {
            BackendError::PermissionDenied
        } else {
            BackendError::Other(if msg == "service error" {
                debug_msg
            } else {
                msg
            })
        }
    }

    /// Parse AWS DateTime to Unix timestamp
    fn parse_datetime(dt: &aws_sdk_s3::primitives::DateTime) -> u32 {
        u64::try_from(dt.secs()).map_or(0, unix_secs_to_u32)
    }

    fn s3_size_to_u64(size: i64) -> u64 {
        u64::try_from(size).unwrap_or(0)
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

        let mut entries_by_name = BTreeMap::new();
        let mut continuation_token = None;

        loop {
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(&self.config.bucket)
                .prefix(&prefix)
                .delimiter("/");

            if let Some(token) = continuation_token.as_deref() {
                request = request.continuation_token(token);
            }

            let result = request.send().await.map_err(Self::map_s3_error)?;

            if let Some(contents) = result.contents {
                for obj in contents {
                    let Some(key) = obj.key else {
                        continue;
                    };
                    let mtime = obj
                        .last_modified
                        .as_ref()
                        .map_or_else(current_timestamp, Self::parse_datetime);
                    let size = Self::s3_size_to_u64(obj.size.unwrap_or(0));

                    Self::insert_listing_file(&mut entries_by_name, &key, &prefix, size, mtime);
                }
            }

            if let Some(common_prefixes) = result.common_prefixes {
                for common_prefix in common_prefixes {
                    let Some(prefix_key) = common_prefix.prefix else {
                        continue;
                    };
                    Self::insert_listing_prefix(&mut entries_by_name, &prefix_key, &prefix);
                }
            }

            if result.is_truncated.unwrap_or(false) {
                continuation_token = result.next_continuation_token;
                if continuation_token.is_some() {
                    continue;
                }
            }

            break;
        }

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
        entries.extend(
            entries_by_name
                .into_iter()
                .map(|(name, attrs)| DirEntry { name, attrs }),
        );

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
        let head_error = match self
            .client
            .head_object()
            .bucket(&self.config.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(result) => {
                let size = Self::s3_size_to_u64(result.content_length.unwrap_or(0));
                let mtime = result
                    .last_modified
                    .as_ref()
                    .map_or_else(current_timestamp, Self::parse_datetime);
                return Ok(FileInfo::file_with_mtime(size, mtime));
            }
            Err(err) => {
                let mapped = Self::map_s3_error(err);
                Some(mapped)
            }
        };

        // Check if it's a directory (has objects with this prefix)
        let prefix = format!("{key}/");
        let result = self
            .client
            .list_objects_v2()
            .bucket(&self.config.bucket)
            .prefix(&prefix)
            .delimiter("/")
            .max_keys(1)
            .send()
            .await
            .map_err(Self::map_s3_error)?;

        let has_contents = result.contents.is_some_and(|c| !c.is_empty());
        let has_prefixes = result.common_prefixes.is_some_and(|p| !p.is_empty());

        if has_contents || has_prefixes {
            Ok(FileInfo::directory())
        } else {
            Err(head_error.unwrap_or(BackendError::NotFound))
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

        let size = Self::s3_size_to_u64(head.content_length.unwrap_or(0));

        Ok(Box::new(S3ReadHandle {
            client: self.client.clone(),
            bucket: self.config.bucket.clone(),
            key,
            size,
        }))
    }

    async fn open_write(&self, path: &str) -> BackendResult<Box<dyn WriteHandle + Send>> {
        let key = self.build_key(path);

        Ok(Box::new(S3WriteHandle {
            client: self.client.clone(),
            bucket: self.config.bucket.clone(),
            key,
            upload_id: None,
            buffer: Vec::new(),
            parts: Vec::new(),
            part_number: 1,
            next_offset: 0,
        }))
    }
}

impl S3Backend {
    fn insert_listing_file(
        entries_by_name: &mut BTreeMap<String, FileInfo>,
        key: &str,
        prefix: &str,
        size: u64,
        mtime: u32,
    ) {
        if let Some(name) = Self::strip_listing_directory_marker(key, prefix) {
            entries_by_name
                .entry(name)
                .or_insert_with(|| FileInfo::directory_with_mtime(mtime));
            return;
        }

        if let Some(name) = Self::strip_listing_prefix(key, prefix) {
            entries_by_name
                .entry(name)
                .or_insert_with(|| FileInfo::file_with_mtime(size, mtime));
        }
    }

    fn insert_listing_prefix(
        entries_by_name: &mut BTreeMap<String, FileInfo>,
        prefix_key: &str,
        listing_prefix: &str,
    ) {
        let trimmed = prefix_key.trim_end_matches('/');
        if let Some(name) = Self::strip_listing_prefix(trimmed, listing_prefix) {
            entries_by_name
                .entry(name)
                .or_insert_with(|| FileInfo::directory_with_mtime(DIRECTORY_LISTING_MTIME));
        }
    }

    fn strip_listing_directory_marker(key: &str, prefix: &str) -> Option<String> {
        let entry = if prefix.is_empty() {
            key
        } else {
            key.strip_prefix(prefix)?
        };

        let trimmed = entry.trim_end_matches('/');
        if entry == trimmed || trimmed.is_empty() || trimmed == KEEP_MARKER || trimmed.contains('/')
        {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    fn strip_listing_prefix(key: &str, prefix: &str) -> Option<String> {
        let entry = if prefix.is_empty() {
            key
        } else {
            key.strip_prefix(prefix)?
        };

        if entry.is_empty() || entry == KEEP_MARKER || entry.contains('/') {
            None
        } else {
            Some(entry.to_string())
        }
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

        let end = std::cmp::min(offset + u64::from(len), self.size) - 1;
        let range = format!("bytes={offset}-{end}");

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
    upload_id: Option<String>,
    buffer: Vec<u8>,
    parts: Vec<CompletedPart>,
    part_number: i32,
    next_offset: u64,
}

impl S3WriteHandle {
    async fn ensure_multipart_started(&mut self) -> BackendResult<String> {
        if let Some(upload_id) = &self.upload_id {
            return Ok(upload_id.clone());
        }

        let create_result = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(&self.key)
            .send()
            .await
            .map_err(S3Backend::map_s3_error)?;

        let upload_id = create_result
            .upload_id
            .ok_or_else(|| BackendError::Other("No upload ID returned".into()))?;

        debug!(key = %self.key, upload_id = %upload_id, "Started multipart upload");
        self.upload_id = Some(upload_id.clone());
        Ok(upload_id)
    }

    /// Flush buffer to S3 as a part if large enough (or if force is true)
    async fn flush_part(&mut self, force: bool) -> BackendResult<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        // Only flush if we have enough data (or are finishing)
        if !force && self.buffer.len() < MIN_PART_SIZE {
            return Ok(());
        }

        let upload_id = self.ensure_multipart_started().await?;
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
            .upload_id(upload_id)
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
        // S3 multipart uploads are append-only. Track the global stream offset
        // instead of the current in-memory buffer length, because the buffer is
        // drained each time a part is uploaded.
        if offset != self.next_offset {
            return Err(BackendError::Other(format!(
                "non-sequential S3 write offset: got {offset}, expected {}",
                self.next_offset
            )));
        }

        self.next_offset += data.len() as u64;
        self.buffer.extend_from_slice(&data);
        self.flush_part(false).await?;

        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> BackendResult<()> {
        if self.parts.is_empty() && self.buffer.len() < MIN_PART_SIZE {
            let data = std::mem::take(&mut self.buffer);
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&self.key)
                .body(ByteStream::from(data))
                .send()
                .await
                .map_err(S3Backend::map_s3_error)?;

            return Ok(());
        }

        // Upload any remaining data
        self.flush_part(true).await?;

        // Complete multipart upload
        let upload_id = self
            .upload_id
            .ok_or_else(|| BackendError::Other("No upload ID for multipart completion".into()))?;
        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(self.parts))
            .build();

        debug!(
            key = %self.key,
            upload_id = %upload_id,
            "Completing multipart upload"
        );

        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(&self.key)
            .upload_id(upload_id)
            .multipart_upload(completed)
            .send()
            .await
            .map_err(S3Backend::map_s3_error)?;

        Ok(())
    }

    async fn abort(self: Box<Self>) -> BackendResult<()> {
        let Some(upload_id) = self.upload_id else {
            return Ok(());
        };

        debug!(
            key = %self.key,
            upload_id = %upload_id,
            "Aborting multipart upload"
        );

        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(&self.key)
            .upload_id(upload_id)
            .send()
            .await
            .map_err(S3Backend::map_s3_error)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_client() -> Client {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_s3::config::Credentials::for_tests())
            .build();
        Client::from_conf(config)
    }

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

    // --- listing prefix tests ---

    #[test]
    fn test_strip_listing_prefix_root_file() {
        assert_eq!(
            S3Backend::strip_listing_prefix("file.txt", ""),
            Some("file.txt".to_string())
        );
    }

    #[test]
    fn test_strip_listing_prefix_prefixed_file() {
        assert_eq!(
            S3Backend::strip_listing_prefix("tenant/file.txt", "tenant/"),
            Some("file.txt".to_string())
        );
    }

    #[test]
    fn test_strip_listing_prefix_ignores_nested_files() {
        assert_eq!(
            S3Backend::strip_listing_prefix("tenant/dir/file.txt", "tenant/"),
            None
        );
    }

    #[test]
    fn test_strip_listing_prefix_ignores_keep_markers_and_empty_entries() {
        assert_eq!(S3Backend::strip_listing_prefix("tenant/", "tenant/"), None);
        assert_eq!(
            S3Backend::strip_listing_prefix("tenant/.keep", "tenant/"),
            None
        );
    }

    #[test]
    fn test_strip_listing_directory_marker() {
        assert_eq!(
            S3Backend::strip_listing_directory_marker("tenant/empty-dir/", "tenant/"),
            Some("empty-dir".to_string())
        );
        assert_eq!(
            S3Backend::strip_listing_directory_marker("tenant/nested/dir/", "tenant/"),
            None
        );
        assert_eq!(
            S3Backend::strip_listing_directory_marker("tenant/file.txt", "tenant/"),
            None
        );
    }

    #[test]
    fn test_listing_collection_sorts_and_deduplicates_immediate_entries() {
        let mut entries = BTreeMap::new();

        S3Backend::insert_listing_file(&mut entries, "tenant/z.txt", "tenant/", 9, 100);
        S3Backend::insert_listing_prefix(&mut entries, "tenant/dir/", "tenant/");
        S3Backend::insert_listing_file(&mut entries, "tenant/a.txt", "tenant/", 1, 100);
        S3Backend::insert_listing_file(&mut entries, "tenant/dir/file.txt", "tenant/", 4, 100);
        S3Backend::insert_listing_file(&mut entries, "tenant/.keep", "tenant/", 0, 100);
        S3Backend::insert_listing_file(&mut entries, "tenant/empty-dir/", "tenant/", 0, 200);

        let names: Vec<_> = entries.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["a.txt", "dir", "empty-dir", "z.txt"]);
        let dir = entries.get("dir").unwrap();
        assert!(dir.is_dir);
        assert_eq!(dir.mtime, DIRECTORY_LISTING_MTIME);
        assert_eq!(dir.atime, DIRECTORY_LISTING_MTIME);

        let empty_dir = entries.get("empty-dir").unwrap();
        assert!(empty_dir.is_dir);
        assert_eq!(empty_dir.mtime, 200);

        let file = entries.get("a.txt").unwrap();
        assert!(!file.is_dir);
        assert_eq!(file.mtime, 100);
    }

    #[test]
    fn test_listing_collection_keeps_first_entry_when_file_and_prefix_overlap() {
        let mut entries = BTreeMap::new();

        S3Backend::insert_listing_prefix(&mut entries, "tenant/shared/", "tenant/");
        S3Backend::insert_listing_file(&mut entries, "tenant/shared", "tenant/", 12, 100);

        let info = entries.get("shared").unwrap();
        assert!(info.is_dir);
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

    #[tokio::test]
    async fn test_s3_write_handle_tracks_global_offsets_after_buffer_flush() {
        let mut handle = S3WriteHandle {
            client: dummy_client(),
            bucket: "bucket".to_string(),
            key: "key".to_string(),
            upload_id: None,
            buffer: Vec::new(),
            parts: Vec::new(),
            part_number: 1,
            next_offset: MIN_PART_SIZE as u64,
        };

        handle
            .write_at(MIN_PART_SIZE as u64, Bytes::from_static(b"tail"))
            .await
            .unwrap();

        assert_eq!(handle.next_offset, MIN_PART_SIZE as u64 + 4);
        assert_eq!(handle.buffer, b"tail");
    }

    #[tokio::test]
    async fn test_s3_write_handle_rejects_non_sequential_offsets() {
        let mut handle = S3WriteHandle {
            client: dummy_client(),
            bucket: "bucket".to_string(),
            key: "key".to_string(),
            upload_id: None,
            buffer: Vec::new(),
            parts: Vec::new(),
            part_number: 1,
            next_offset: 4,
        };

        let result = handle.write_at(2, Bytes::from_static(b"nope")).await;

        assert!(matches!(result, Err(BackendError::Other(_))));
        assert!(handle.buffer.is_empty());
        assert_eq!(handle.next_offset, 4);
    }

    #[tokio::test]
    async fn test_s3_open_write_is_lazy_until_data_flush() {
        let backend = S3Backend::new(dummy_client(), S3Config::new("bucket"));

        let handle = backend.open_write("small.txt").await.unwrap();

        handle.abort().await.unwrap();
    }
}
