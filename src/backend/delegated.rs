use super::{
    Backend, BackendResult, BufferedReadHandle, BufferedWriteWithBackend, DirEntry, FileInfo,
    ReadHandle, SetAttrs, WriteHandle,
};
use async_trait::async_trait;
use bytes::Bytes;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum BackendRequest {
    ListDir {
        path: String,
    },
    FileInfo {
        path: String,
    },
    Lstat {
        path: String,
    },
    MakeDir {
        path: String,
    },
    DelDir {
        path: String,
    },
    Delete {
        path: String,
    },
    Rename {
        src: String,
        dst: String,
    },
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        content: Bytes,
    },
    ReadLink {
        path: String,
    },
    Symlink {
        linkpath: String,
        targetpath: String,
    },
    SetAttrs {
        path: String,
        attrs: SetAttrs,
    },
}

#[derive(Debug, Clone)]
pub enum BackendResponse {
    Unit,
    DirEntries(Vec<DirEntry>),
    FileInfo(FileInfo),
    Bytes(Bytes),
    Path(String),
}

pub type DelegatedBackendFn = Arc<
    dyn Fn(BackendRequest) -> Pin<Box<dyn Future<Output = BackendResult<BackendResponse>> + Send>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct DelegatedBackend {
    handler: DelegatedBackendFn,
    capabilities: super::BackendCapabilities,
}

impl DelegatedBackend {
    pub fn new(handler: DelegatedBackendFn) -> Self {
        Self {
            handler,
            capabilities: super::BackendCapabilities {
                symlinks: true,
                set_attrs: true,
                delegated_safe_streaming_fallback: true,
            },
        }
    }

    pub fn with_capabilities(
        handler: DelegatedBackendFn,
        capabilities: super::BackendCapabilities,
    ) -> Self {
        Self {
            handler,
            capabilities,
        }
    }

    async fn call(&self, request: BackendRequest) -> BackendResult<BackendResponse> {
        (self.handler)(request).await
    }
}

#[async_trait]
impl Backend for DelegatedBackend {
    fn capabilities(&self) -> super::BackendCapabilities {
        self.capabilities
    }

    async fn list_dir(&self, path: &str) -> BackendResult<Vec<DirEntry>> {
        match self
            .call(BackendRequest::ListDir {
                path: path.to_string(),
            })
            .await?
        {
            BackendResponse::DirEntries(entries) => Ok(entries),
            other => Err(super::BackendError::Other(format!(
                "delegated backend returned unexpected response for list_dir: {other:?}"
            ))),
        }
    }

    async fn file_info(&self, path: &str) -> BackendResult<FileInfo> {
        match self
            .call(BackendRequest::FileInfo {
                path: path.to_string(),
            })
            .await?
        {
            BackendResponse::FileInfo(info) => Ok(info),
            other => Err(super::BackendError::Other(format!(
                "delegated backend returned unexpected response for file_info: {other:?}"
            ))),
        }
    }

    async fn lstat(&self, path: &str) -> BackendResult<FileInfo> {
        match self
            .call(BackendRequest::Lstat {
                path: path.to_string(),
            })
            .await?
        {
            BackendResponse::FileInfo(info) => Ok(info),
            other => Err(super::BackendError::Other(format!(
                "delegated backend returned unexpected response for lstat: {other:?}"
            ))),
        }
    }

    async fn make_dir(&self, path: &str) -> BackendResult<()> {
        match self
            .call(BackendRequest::MakeDir {
                path: path.to_string(),
            })
            .await?
        {
            BackendResponse::Unit => Ok(()),
            other => Err(super::BackendError::Other(format!(
                "delegated backend returned unexpected response for make_dir: {other:?}"
            ))),
        }
    }

    async fn del_dir(&self, path: &str) -> BackendResult<()> {
        match self
            .call(BackendRequest::DelDir {
                path: path.to_string(),
            })
            .await?
        {
            BackendResponse::Unit => Ok(()),
            other => Err(super::BackendError::Other(format!(
                "delegated backend returned unexpected response for del_dir: {other:?}"
            ))),
        }
    }

    async fn delete(&self, path: &str) -> BackendResult<()> {
        match self
            .call(BackendRequest::Delete {
                path: path.to_string(),
            })
            .await?
        {
            BackendResponse::Unit => Ok(()),
            other => Err(super::BackendError::Other(format!(
                "delegated backend returned unexpected response for delete: {other:?}"
            ))),
        }
    }

    async fn rename(&self, src: &str, dst: &str) -> BackendResult<()> {
        match self
            .call(BackendRequest::Rename {
                src: src.to_string(),
                dst: dst.to_string(),
            })
            .await?
        {
            BackendResponse::Unit => Ok(()),
            other => Err(super::BackendError::Other(format!(
                "delegated backend returned unexpected response for rename: {other:?}"
            ))),
        }
    }

    async fn read_file(&self, path: &str) -> BackendResult<Bytes> {
        match self
            .call(BackendRequest::ReadFile {
                path: path.to_string(),
            })
            .await?
        {
            BackendResponse::Bytes(bytes) => Ok(bytes),
            other => Err(super::BackendError::Other(format!(
                "delegated backend returned unexpected response for read_file: {other:?}"
            ))),
        }
    }

    async fn write_file(&self, path: &str, content: Bytes) -> BackendResult<()> {
        match self
            .call(BackendRequest::WriteFile {
                path: path.to_string(),
                content,
            })
            .await?
        {
            BackendResponse::Unit => Ok(()),
            other => Err(super::BackendError::Other(format!(
                "delegated backend returned unexpected response for write_file: {other:?}"
            ))),
        }
    }

    async fn open_read(&self, path: &str) -> BackendResult<Box<dyn ReadHandle>> {
        let bytes = self.read_file(path).await?;
        Ok(Box::new(BufferedReadHandle::new(bytes)))
    }

    async fn open_write(&self, path: &str) -> BackendResult<Box<dyn WriteHandle + Send>> {
        Ok(Box::new(BufferedWriteWithBackend::new(
            path.to_string(),
            self.clone(),
        )))
    }

    async fn read_link(&self, path: &str) -> BackendResult<String> {
        match self
            .call(BackendRequest::ReadLink {
                path: path.to_string(),
            })
            .await?
        {
            BackendResponse::Path(path) => Ok(path),
            other => Err(super::BackendError::Other(format!(
                "delegated backend returned unexpected response for read_link: {other:?}"
            ))),
        }
    }

    async fn symlink(&self, linkpath: &str, targetpath: &str) -> BackendResult<()> {
        match self
            .call(BackendRequest::Symlink {
                linkpath: linkpath.to_string(),
                targetpath: targetpath.to_string(),
            })
            .await?
        {
            BackendResponse::Unit => Ok(()),
            other => Err(super::BackendError::Other(format!(
                "delegated backend returned unexpected response for symlink: {other:?}"
            ))),
        }
    }

    async fn set_attrs(&self, path: &str, attrs: SetAttrs) -> BackendResult<()> {
        match self
            .call(BackendRequest::SetAttrs {
                path: path.to_string(),
                attrs,
            })
            .await?
        {
            BackendResponse::Unit => Ok(()),
            other => Err(super::BackendError::Other(format!(
                "delegated backend returned unexpected response for set_attrs: {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Backend, BackendError, FileKind};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{mpsc, oneshot, Mutex};

    fn memory_like_handler() -> DelegatedBackend {
        let files = Arc::new(Mutex::new(HashMap::<String, Bytes>::new()));
        let handler: DelegatedBackendFn = Arc::new(move |request| {
            let files = files.clone();
            Box::pin(async move {
                match request {
                    BackendRequest::ListDir { .. } => Ok(BackendResponse::DirEntries(vec![
                        crate::backend::DirEntry {
                            name: ".".into(),
                            attrs: FileInfo::directory(),
                        },
                    ])),
                    BackendRequest::Lstat { path } | BackendRequest::FileInfo { path } => {
                        let files = files.lock().await;
                        let bytes = files.get(&path).ok_or(BackendError::NotFound)?;
                        Ok(BackendResponse::FileInfo(
                            FileInfo::file(bytes.len() as u64),
                        ))
                    }
                    BackendRequest::Lstat { path } => {
                        if path.ends_with(".lnk") {
                            Ok(BackendResponse::FileInfo(FileInfo::symlink(10)))
                        } else {
                            let files = files.lock().await;
                            let bytes = files.get(&path).ok_or(BackendError::NotFound)?;
                            Ok(BackendResponse::FileInfo(
                                FileInfo::file(bytes.len() as u64),
                            ))
                        }
                    }
                    BackendRequest::ReadFile { path } => {
                        let files = files.lock().await;
                        let bytes = files.get(&path).ok_or(BackendError::NotFound)?;
                        Ok(BackendResponse::Bytes(bytes.clone()))
                    }
                    BackendRequest::WriteFile { path, content } => {
                        files.lock().await.insert(path, content);
                        Ok(BackendResponse::Unit)
                    }
                    BackendRequest::ReadLink { path } => {
                        Ok(BackendResponse::Path(format!("target:{path}")))
                    }
                    BackendRequest::Symlink { .. }
                    | BackendRequest::SetAttrs { .. }
                    | BackendRequest::MakeDir { .. }
                    | BackendRequest::DelDir { .. }
                    | BackendRequest::Delete { .. }
                    | BackendRequest::Rename { .. } => Ok(BackendResponse::Unit),
                }
            })
        });

        DelegatedBackend::new(handler)
    }

    #[tokio::test]
    async fn test_delegated_read_and_write_file() {
        let backend = memory_like_handler();
        backend
            .write_file("hello.txt", Bytes::from_static(b"hello"))
            .await
            .unwrap();

        let bytes = backend.read_file("hello.txt").await.unwrap();
        let info = backend.file_info("hello.txt").await.unwrap();

        assert_eq!(bytes, Bytes::from_static(b"hello"));
        assert_eq!(info.kind, FileKind::File);
        assert_eq!(info.size, 5);
    }

    #[tokio::test]
    async fn test_delegated_open_read_and_open_write_use_buffered_fallbacks() {
        let backend = memory_like_handler();
        let mut writer = backend.open_write("stream.txt").await.unwrap();
        writer
            .write_at(0, Bytes::from_static(b"stream"))
            .await
            .unwrap();
        writer.finish().await.unwrap();

        let reader = backend.open_read("stream.txt").await.unwrap();
        let bytes = reader.read_at(0, 64).await.unwrap();
        assert_eq!(bytes, Bytes::from_static(b"stream"));
    }

    #[tokio::test]
    async fn test_delegated_symlink_and_set_attrs_forwarding() {
        let requests = Arc::new(Mutex::new(Vec::<BackendRequest>::new()));
        let handler: DelegatedBackendFn = Arc::new({
            let requests = requests.clone();
            move |request| {
                let requests = requests.clone();
                Box::pin(async move {
                    requests.lock().await.push(request.clone());
                    match request {
                        BackendRequest::Lstat { .. } => {
                            Ok(BackendResponse::FileInfo(FileInfo::symlink(10)))
                        }
                        BackendRequest::ReadLink { .. } => {
                            Ok(BackendResponse::Path("target.txt".into()))
                        }
                        _ => Ok(BackendResponse::Unit),
                    }
                })
            }
        });
        let backend = DelegatedBackend::new(handler);

        backend.symlink("link.txt", "target.txt").await.unwrap();
        assert_eq!(backend.read_link("link.txt").await.unwrap(), "target.txt");
        backend
            .set_attrs(
                "link.txt",
                SetAttrs {
                    permissions: Some(0o777),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let lstat = backend.lstat("link.txt").await.unwrap();

        let requests = requests.lock().await;
        assert!(requests.iter().any(|request| matches!(
            request,
            BackendRequest::Symlink { linkpath, targetpath }
            if linkpath == "link.txt" && targetpath == "target.txt"
        )));
        assert!(requests.iter().any(|request| matches!(
            request,
            BackendRequest::SetAttrs { path, attrs }
            if path == "link.txt" && attrs.permissions == Some(0o777)
        )));
        assert!(backend.capabilities().symlinks);
        assert!(backend.capabilities().set_attrs);
        assert_eq!(lstat.kind, FileKind::Symlink);
    }

    #[tokio::test]
    async fn test_delegated_lstat_forwarding() {
        let backend = memory_like_handler();

        let info = backend.lstat("broken.lnk").await.unwrap();

        assert_eq!(info.kind, FileKind::Symlink);
    }

    #[tokio::test]
    async fn test_delegated_backend_can_wrap_actor_task() {
        let (tx, mut rx) = mpsc::channel::<(
            BackendRequest,
            oneshot::Sender<BackendResult<BackendResponse>>,
        )>(8);

        tokio::spawn(async move {
            while let Some((request, reply_tx)) = rx.recv().await {
                let reply = match request {
                    BackendRequest::ListDir { .. } => Ok(BackendResponse::DirEntries(vec![
                        crate::backend::DirEntry {
                            name: "child.txt".into(),
                            attrs: FileInfo::file(4),
                        },
                    ])),
                    BackendRequest::Lstat { .. } => {
                        Ok(BackendResponse::FileInfo(FileInfo::symlink(4)))
                    }
                    BackendRequest::ReadFile { .. } => {
                        Ok(BackendResponse::Bytes(Bytes::from_static(b"data")))
                    }
                    BackendRequest::WriteFile { .. } => Ok(BackendResponse::Unit),
                    _ => Err(BackendError::Unsupported),
                };
                let _ = reply_tx.send(reply);
            }
        });

        let handler: DelegatedBackendFn = Arc::new(move |request| {
            let tx = tx.clone();
            Box::pin(async move {
                let (reply_tx, reply_rx) = oneshot::channel();
                tx.send((request, reply_tx))
                    .await
                    .map_err(|err| BackendError::Other(err.to_string()))?;
                reply_rx
                    .await
                    .map_err(|err| BackendError::Other(err.to_string()))?
            })
        });
        let backend = DelegatedBackend::new(handler);

        let entries = backend.list_dir("/").await.unwrap();
        let bytes = backend.read_file("child.txt").await.unwrap();

        assert!(entries.iter().any(|entry| entry.name == "child.txt"));
        assert_eq!(bytes, Bytes::from_static(b"data"));
    }
}
