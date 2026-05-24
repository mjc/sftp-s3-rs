#![allow(dead_code)]

use crate::backend::{Backend, WriteHandle};
use bytes::BytesMut;
use std::borrow::Cow;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

const SCP_OK: u8 = 0;
const SCP_ERROR: u8 = 1;
const SCP_WARNING: u8 = 2;

/// SCP protocol handler
pub struct ScpHandler<B: Backend> {
    backend: Arc<B>,
}

/// SCP protocol message types
#[derive(Debug, Clone)]
enum ScpMessage {
    /// File command: C0755 size filename
    File {
        mode: String,
        size: u64,
        name: String,
    },
    /// Directory command: D0755 0 dirname
    Dir { mode: String, name: String },
    /// End directory: E
    EndDir,
    /// Set times: T mtime 0 atime 0
    Times { mtime: u64, atime: u64 },
    /// End of transfer
    Eof,
}

/// SCP transfer mode
#[derive(Debug, Clone, Copy)]
enum ScpMode {
    /// Receive files (-t)
    Receive,
    /// Send files (-f)
    Send,
}

#[derive(Debug, Clone, Copy)]
struct PendingTimes {
    mtime: u32,
    atime: u32,
}

#[derive(Debug)]
struct DirState {
    path: String,
    times: Option<PendingTimes>,
}

impl<B: Backend> ScpHandler<B> {
    /// Create a new SCP handler
    pub fn new(backend: Arc<B>) -> Self {
        Self { backend }
    }

    /// Run SCP protocol handler.
    ///
    /// # Errors
    ///
    /// Returns an error if command parsing, backend operations, or stream I/O fails.
    pub async fn run<S: AsyncReadExt + AsyncWriteExt + Unpin + Send>(
        &mut self,
        mut stream: S,
        command: &str,
    ) -> Result<(), ScpError> {
        debug!("SCP handler started with command: {}", command);

        // Parse SCP command to determine mode and options
        let (mode, recursive, preserve_times, target_path) = Self::parse_command(command)?;
        debug!(
            "Parsed SCP command: mode={:?} recursive={} preserve_times={} target={}",
            mode, recursive, preserve_times, target_path
        );

        match mode {
            ScpMode::Receive => {
                self.handle_receive(&mut stream, recursive, preserve_times, &target_path)
                    .await
            }
            ScpMode::Send => {
                self.handle_send(&mut stream, recursive, preserve_times, &target_path)
                    .await
            }
        }
    }

    /// Expand tilde in path to root (virtual filesystem has no home dirs)
    fn expand_tilde(path: &str) -> Cow<'_, str> {
        if path == "~" {
            Cow::Borrowed("/")
        } else if let Some(rest) = path.strip_prefix("~/") {
            Cow::Owned(format!("/{rest}"))
        } else {
            Cow::Borrowed(path)
        }
    }

    /// Parse SCP command line
    fn parse_command(command: &str) -> Result<(ScpMode, bool, bool, String), ScpError> {
        if !command.starts_with("scp ") {
            return Err(ScpError::InvalidCommand(command.to_string()));
        }

        let args: Vec<&str> = command[4..].split_whitespace().collect();

        let mode = args
            .iter()
            .fold(None, |acc, &arg| match arg {
                "-t" => Some(ScpMode::Receive),
                "-f" => Some(ScpMode::Send),
                _ => acc,
            })
            .ok_or(ScpError::NoMode)?;

        let recursive = args.contains(&"-r");
        let preserve_times = args.contains(&"-p");
        let target_path = args
            .iter()
            .rfind(|a| !a.starts_with('-'))
            .map_or_else(|| "/".to_string(), |p| Self::expand_tilde(p).into_owned());

        Ok((mode, recursive, preserve_times, target_path))
    }

    /// Handle receive mode (scp -t)
    async fn handle_receive<S: AsyncReadExt + AsyncWriteExt + Unpin>(
        &mut self,
        stream: &mut S,
        recursive: bool,
        preserve_times: bool,
        target_path: &str,
    ) -> Result<(), ScpError> {
        self.send_status(stream, SCP_OK, "").await?;
        let can_set_attrs = self.backend.capabilities().set_attrs;
        let target_is_dir = target_path.ends_with('/')
            || matches!(
                self.backend.file_info(target_path).await,
                Ok(info) if info.is_dir
            );
        let base_dir = if target_is_dir {
            target_path.to_string()
        } else {
            Self::parent_dir(target_path)
        };
        let mut explicit_target = if target_is_dir {
            None
        } else {
            Some(target_path.to_string())
        };
        let mut pending_times: Option<PendingTimes> = None;
        let mut dir_stack: Vec<DirState> = Vec::new();
        let preserve_times = preserve_times && can_set_attrs;

        loop {
            let mut buf = [0u8; 1];
            match stream.read_exact(&mut buf).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    break;
                }
                Err(e) => return Err(ScpError::Io(e)),
            }

            match buf[0] {
                b'C' => {
                    let line = Self::read_line(stream).await?;
                    let ScpMessage::File { mode, size, name } = Self::parse_file_message(&line)?
                    else {
                        unreachable!();
                    };
                    let file_path =
                        Self::receive_path(&base_dir, &dir_stack, &mut explicit_target, &name);
                    let mode = Self::parse_mode_bits(&mode)?;

                    let mut handle = self
                        .backend
                        .open_write(&file_path)
                        .await
                        .map_err(|e| ScpError::Backend(e.to_string()))?;

                    self.send_status(stream, SCP_OK, "").await?;

                    let mut remaining = size;
                    let mut offset = 0u64;
                    let mut transfer_buf = BytesMut::with_capacity(65536);
                    while remaining > 0 {
                        let to_read =
                            std::cmp::min(remaining, transfer_buf.capacity() as u64) as usize;
                        transfer_buf.resize(to_read, 0);
                        stream.read_exact(&mut transfer_buf[..to_read]).await?;
                        let data = transfer_buf.split_to(to_read).freeze();
                        handle
                            .write_at(offset, data)
                            .await
                            .map_err(|e| ScpError::Backend(e.to_string()))?;
                        offset += to_read as u64;
                        remaining -= to_read as u64;
                    }

                    handle
                        .finish()
                        .await
                        .map_err(|e| ScpError::Backend(e.to_string()))?;

                    let mut attrs = crate::backend::SetAttrs {
                        permissions: Some(mode),
                        ..Default::default()
                    };
                    if preserve_times {
                        if let Some(times) = pending_times.take() {
                            attrs.atime = Some(times.atime);
                            attrs.mtime = Some(times.mtime);
                        }
                    } else {
                        pending_times = None;
                    }
                    if can_set_attrs {
                        self.backend
                            .set_attrs(&file_path, attrs)
                            .await
                            .map_err(|e| ScpError::Backend(e.to_string()))?;
                    }

                    Self::read_ack(stream).await?;

                    self.send_status(stream, SCP_OK, "").await?;
                }
                b'D' => {
                    if !recursive {
                        return Err(ScpError::Protocol(
                            "scp -r required for directory receive".into(),
                        ));
                    }

                    let line = Self::read_line(stream).await?;
                    let ScpMessage::Dir { mode, name } = Self::parse_dir_message(&line)? else {
                        unreachable!();
                    };
                    let dir_path =
                        Self::receive_path(&base_dir, &dir_stack, &mut explicit_target, &name);
                    let mode = Self::parse_mode_bits(&mode)?;

                    self.backend
                        .make_dir(&dir_path)
                        .await
                        .map_err(|e| ScpError::Backend(e.to_string()))?;
                    if can_set_attrs {
                        self.backend
                            .set_attrs(
                                &dir_path,
                                crate::backend::SetAttrs {
                                    permissions: Some(mode),
                                    ..Default::default()
                                },
                            )
                            .await
                            .map_err(|e| ScpError::Backend(e.to_string()))?;
                    }

                    dir_stack.push(DirState {
                        path: dir_path,
                        times: if preserve_times {
                            pending_times.take()
                        } else {
                            pending_times = None;
                            None
                        },
                    });

                    self.send_status(stream, SCP_OK, "").await?;
                }
                b'E' => {
                    let line = Self::read_line(stream).await?;
                    if !line.is_empty() {
                        return Err(ScpError::Protocol("unexpected data after E command".into()));
                    }

                    let Some(dir) = dir_stack.pop() else {
                        return Err(ScpError::Protocol("unexpected end-directory marker".into()));
                    };

                    if let Some(times) = dir.times {
                        self.backend
                            .set_attrs(
                                &dir.path,
                                crate::backend::SetAttrs {
                                    atime: Some(times.atime),
                                    mtime: Some(times.mtime),
                                    ..Default::default()
                                },
                            )
                            .await
                            .map_err(|e| ScpError::Backend(e.to_string()))?;
                    }

                    self.send_status(stream, SCP_OK, "").await?;
                }
                b'T' => {
                    let line = Self::read_line(stream).await?;
                    if preserve_times {
                        if let ScpMessage::Times { mtime, atime } =
                            Self::parse_times_message(&line)?
                        {
                            pending_times = Some(PendingTimes {
                                mtime: mtime as u32,
                                atime: atime as u32,
                            });
                        }
                    }
                    self.send_status(stream, SCP_OK, "").await?;
                }
                b => {
                    return Err(ScpError::Protocol(format!(
                        "unexpected command byte: {}",
                        b as char
                    )))
                }
            }
        }

        Ok(())
    }

    async fn receive_file<S: AsyncReadExt + AsyncWriteExt + Unpin>(
        &mut self,
        stream: &mut S,
        line: &str,
        current_dir: &str,
        explicit_target: Option<&str>,
        pending_mtime: &mut Option<u64>,
    ) -> Result<(), ScpError> {
        let msg = Self::parse_file_message(line)?;

        if let ScpMessage::File { mode, size, name } = msg {
            let file_path = explicit_target.map_or_else(
                || format!("{}/{}", current_dir.trim_end_matches('/'), name),
                ToString::to_string,
            );
            debug!("SCP receive file: path={file_path} size={size} mode={mode}");
            debug!("SCP receive file: {file_path} ({size} bytes)");

            let mut handle = self
                .backend
                .open_write(&file_path)
                .await
                .map_err(|e| ScpError::Backend(e.to_string()))?;

            self.send_status(stream, SCP_OK, "").await?;

            let mut remaining = size;
            let mut offset = 0u64;
            let mut transfer_buf = BytesMut::with_capacity(65_536);

            debug!("Starting to read {size} bytes of file data");
            while remaining > 0 {
                let bytes_read = Self::receive_file_chunk(
                    stream,
                    &mut *handle,
                    &mut transfer_buf,
                    remaining,
                    offset,
                )
                .await?;
                offset += bytes_read;
                remaining -= bytes_read;
                debug!("Read {bytes_read} bytes, remaining: {remaining}");
            }
            debug!("Finished reading file data, total: {offset} bytes");

            if mode != "0644" && mode != "0755" {
                debug!("SCP receive: preserving mode {mode}");
            }

            handle
                .finish()
                .await
                .map_err(|e| ScpError::Backend(e.to_string()))?;

            if let Some(mtime) = pending_mtime.take() {
                debug!("SCP receive: setting mtime to {mtime}");
            }

            Self::read_file_confirmation(stream).await?;
            self.send_status(stream, SCP_OK, "").await?;
        }

        Ok(())
    }

    async fn receive_file_chunk<S: AsyncReadExt + AsyncWriteExt + Unpin>(
        stream: &mut S,
        handle: &mut dyn WriteHandle,
        transfer_buf: &mut BytesMut,
        remaining: u64,
        offset: u64,
    ) -> Result<u64, ScpError> {
        let capacity = u64::try_from(transfer_buf.capacity()).unwrap_or(u64::MAX);
        let to_read = usize::try_from(std::cmp::min(remaining, capacity))
            .map_err(|_| ScpError::Protocol("transfer chunk too large".into()))?;
        transfer_buf.resize(to_read, 0);
        stream.read_exact(&mut transfer_buf[..to_read]).await?;

        let data = transfer_buf.split_to(to_read).freeze();
        handle
            .write_at(offset, data)
            .await
            .map_err(|e| ScpError::Backend(e.to_string()))?;

        Ok(u64::try_from(to_read).unwrap_or(u64::MAX))
    }

    async fn read_file_confirmation<S: AsyncReadExt + AsyncWriteExt + Unpin>(
        stream: &mut S,
    ) -> Result<(), ScpError> {
        debug!("Waiting for client confirmation byte...");
        let mut confirm = [0u8; 1];
        stream.read_exact(&mut confirm).await?;
        debug!("Got confirmation byte: {}", confirm[0]);
        if confirm[0] != 0 {
            return Err(ScpError::Protocol(format!(
                "expected null byte after file data, got {}",
                confirm[0]
            )));
        }
        Ok(())
    }

    /// Handle send mode (scp -f)
    async fn handle_send<S: AsyncReadExt + AsyncWriteExt + Unpin + Send>(
        &mut self,
        stream: &mut S,
        recursive: bool,
        preserve_times: bool,
        source_path: &str,
    ) -> Result<(), ScpError> {
        Self::read_ack(stream).await?;
        self.send_entry(stream, source_path, recursive, preserve_times)
            .await
    }

    /// Read a line until newline
    async fn read_line<S: AsyncReadExt + Unpin>(stream: &mut S) -> Result<String, ScpError> {
        let mut line = String::new();
        let mut buf = [0u8; 1];

        loop {
            stream.read_exact(&mut buf).await?;
            if buf[0] == b'\n' {
                break;
            }
            line.push(buf[0] as char);
        }

        Ok(line)
    }

    async fn read_ack<S: AsyncReadExt + Unpin>(stream: &mut S) -> Result<(), ScpError> {
        let mut status = [0u8; 1];
        stream.read_exact(&mut status).await?;
        match status[0] {
            SCP_OK => Ok(()),
            SCP_ERROR | SCP_WARNING => {
                let line = Self::read_line(stream).await?;
                Err(ScpError::Protocol(line))
            }
            other => Err(ScpError::Protocol(format!(
                "unexpected SCP status byte: {other}"
            ))),
        }
    }

    fn receive_path(
        base_dir: &str,
        dir_stack: &[DirState],
        explicit_target: &mut Option<String>,
        name: &str,
    ) -> String {
        if let Some(dir) = dir_stack.last() {
            Self::join_path(&dir.path, name)
        } else if let Some(target) = explicit_target.take() {
            target
        } else {
            Self::join_path(base_dir, name)
        }
    }

    fn join_path(base: &str, name: &str) -> String {
        let name = name.trim_start_matches('/');
        if base == "/" {
            format!("/{name}")
        } else if base.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", base.trim_end_matches('/'), name)
        }
    }

    fn parent_dir(path: &str) -> String {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() || trimmed == "/" {
            "/".to_string()
        } else if let Some((parent, _)) = trimmed.rsplit_once('/') {
            if parent.is_empty() {
                "/".to_string()
            } else {
                parent.to_string()
            }
        } else {
            "/".to_string()
        }
    }

    fn parse_mode_bits(mode: &str) -> Result<u32, ScpError> {
        u32::from_str_radix(mode, 8)
            .map_err(|_| ScpError::Protocol(format!("invalid mode: {mode}")))
    }

    fn display_name(path: &str) -> Result<String, ScpError> {
        let trimmed = path.trim_end_matches('/');
        let name = trimmed
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| ScpError::Protocol("cannot SCP the root directory".into()))?;
        Ok(name.to_string())
    }

    fn send_entry<'a, S>(
        &'a self,
        stream: &'a mut S,
        path: &'a str,
        recursive: bool,
        preserve_times: bool,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), ScpError>> + Send + 'a>>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'a,
    {
        Box::pin(async move {
            let info = self
                .backend
                .lstat(path)
                .await
                .map_err(|e| ScpError::Backend(e.to_string()))?;

            if info.kind == crate::backend::FileKind::Symlink {
                let target = self
                    .backend
                    .read_link(path)
                    .await
                    .unwrap_or_else(|_| "<unknown>".into());
                return Err(ScpError::Protocol(format!(
                    "symlink transfer is unsupported: {path} -> {target}"
                )));
            }

            if preserve_times {
                self.send_control_line(stream, b'T', &format!("{} 0 {} 0", info.mtime, info.atime))
                    .await?;
                Self::read_ack(stream).await?;
            }

            let name = Self::display_name(path)?;
            match info.kind {
                crate::backend::FileKind::File => {
                    self.send_control_line(
                        stream,
                        b'C',
                        &format!("{:04o} {} {}", info.permissions & 0o7777, info.size, name),
                    )
                    .await?;
                    Self::read_ack(stream).await?;

                    let handle = self
                        .backend
                        .open_read(path)
                        .await
                        .map_err(|e| ScpError::Backend(e.to_string()))?;
                    let mut offset = 0u64;
                    while offset < handle.size() {
                        let chunk = handle
                            .read_at(offset, 64 * 1024)
                            .await
                            .map_err(|e| ScpError::Backend(e.to_string()))?;
                        if chunk.is_empty() {
                            break;
                        }
                        stream.write_all(&chunk).await?;
                        offset += chunk.len() as u64;
                    }
                    stream.write_all(&[SCP_OK]).await?;
                    stream.flush().await?;
                    Self::read_ack(stream).await?;
                    Ok(())
                }
                crate::backend::FileKind::Directory => {
                    if !recursive {
                        return Err(ScpError::Protocol(
                            "scp -r required for directory transfer".into(),
                        ));
                    }

                    self.send_control_line(
                        stream,
                        b'D',
                        &format!("{:04o} 0 {}", info.permissions & 0o7777, name),
                    )
                    .await?;
                    Self::read_ack(stream).await?;

                    let mut children: Vec<String> = self
                        .backend
                        .list_dir(path)
                        .await
                        .map_err(|e| ScpError::Backend(e.to_string()))?
                        .into_iter()
                        .filter(|entry| entry.name != "." && entry.name != "..")
                        .map(|entry| entry.name)
                        .collect();
                    children.sort();

                    for child in children {
                        let child_path = Self::join_path(path, &child);
                        self.send_entry(stream, &child_path, recursive, preserve_times)
                            .await?;
                    }

                    self.send_control_line(stream, b'E', "").await?;
                    Self::read_ack(stream).await?;
                    Ok(())
                }
                crate::backend::FileKind::Symlink => unreachable!(),
            }
        })
    }

    /// Parse file message: "0755 size filename"
    fn parse_file_message(line: &str) -> Result<ScpMessage, ScpError> {
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        if parts.len() < 3 {
            return Err(ScpError::Protocol("invalid file message format".into()));
        }

        let mode = parts[0].to_string();
        let size = parts[1]
            .parse::<u64>()
            .map_err(|_| ScpError::Protocol("invalid size in file message".into()))?;
        let name = parts[2].to_string();

        Ok(ScpMessage::File { mode, size, name })
    }

    /// Parse directory message: "0755 0 dirname"
    fn parse_dir_message(line: &str) -> Result<ScpMessage, ScpError> {
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        if parts.len() < 3 {
            return Err(ScpError::Protocol(
                "invalid directory message format".into(),
            ));
        }

        let mode = parts[0].to_string();
        let name = parts[2].to_string();

        Ok(ScpMessage::Dir { mode, name })
    }

    /// Parse times message: "mtime 0 atime 0"
    fn parse_times_message(line: &str) -> Result<ScpMessage, ScpError> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            return Err(ScpError::Protocol("invalid times message format".into()));
        }

        let mtime = parts[0]
            .parse::<u64>()
            .map_err(|_| ScpError::Protocol("invalid mtime".into()))?;
        let atime = parts[2]
            .parse::<u64>()
            .map_err(|_| ScpError::Protocol("invalid atime".into()))?;

        Ok(ScpMessage::Times { mtime, atime })
    }

    /// Send status message
    async fn send_status<S: AsyncWriteExt + Unpin>(
        &self,
        stream: &mut S,
        status: u8,
        message: &str,
    ) -> Result<(), ScpError> {
        debug!("send_status: status={}, message={:?}", status, message);
        let msg = if message.is_empty() {
            vec![status]
        } else {
            let mut buf = vec![status];
            buf.extend_from_slice(message.as_bytes());
            buf.push(b'\n');
            buf
        };

        debug!("send_status: writing {} bytes", msg.len());
        stream.write_all(&msg).await?;
        debug!("send_status: flushing");
        stream.flush().await?;
        debug!("send_status: done");
        Ok(())
    }

    async fn send_control_line<S: AsyncWriteExt + Unpin>(
        &self,
        stream: &mut S,
        opcode: u8,
        payload: &str,
    ) -> Result<(), ScpError> {
        let mut buf = vec![opcode];
        if !payload.is_empty() {
            buf.extend_from_slice(payload.as_bytes());
        }
        buf.push(b'\n');
        stream.write_all(&buf).await?;
        stream.flush().await?;
        Ok(())
    }
}

/// SCP error types
#[derive(Debug, thiserror::Error)]
pub enum ScpError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Backend error: {0}")]
    Backend(String),
    #[error("Protocol error: {0}")]
    Protocol(String),
    #[error("Invalid SCP command: {0}")]
    InvalidCommand(String),
    #[error("SCP mode not specified (-t or -f required)")]
    NoMode,
    #[error("Unexpected end of file")]
    UnexpectedEof,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        Backend, BackendCapabilities, BackendRequest, BackendResponse, DelegatedBackend,
        DelegatedBackendFn, MemoryBackend,
    };
    use bytes::Bytes;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::sync::Mutex;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};

    fn make_handler() -> ScpHandler<MemoryBackend> {
        ScpHandler::new(Arc::new(MemoryBackend::new()))
    }

    fn make_handler_with_backend() -> (Arc<MemoryBackend>, ScpHandler<MemoryBackend>) {
        let backend = Arc::new(MemoryBackend::new());
        let handler = ScpHandler::new(Arc::clone(&backend));
        (backend, handler)
    }

    fn make_backend_and_handler() -> (Arc<MemoryBackend>, ScpHandler<MemoryBackend>) {
        make_handler_with_backend()
    }

    fn make_no_attrs_backend_and_handler() -> (DelegatedBackend, ScpHandler<DelegatedBackend>) {
        let files = Arc::new(Mutex::new(HashMap::<String, Bytes>::new()));
        let dirs = Arc::new(Mutex::new(HashSet::<String>::new()));
        let handler: DelegatedBackendFn = Arc::new({
            let files = Arc::clone(&files);
            let dirs = Arc::clone(&dirs);
            move |request| {
                let files = Arc::clone(&files);
                let dirs = Arc::clone(&dirs);
                Box::pin(async move {
                    match request {
                        BackendRequest::ReadFile { path } => files
                            .lock()
                            .unwrap()
                            .get(&path)
                            .cloned()
                            .map(BackendResponse::Bytes)
                            .ok_or(crate::backend::BackendError::NotFound),
                        BackendRequest::WriteFile { path, content } => {
                            files.lock().unwrap().insert(path, content);
                            Ok(BackendResponse::Unit)
                        }
                        BackendRequest::MakeDir { path } => {
                            dirs.lock().unwrap().insert(path);
                            Ok(BackendResponse::Unit)
                        }
                        BackendRequest::ListDir { .. }
                        | BackendRequest::FileInfo { .. }
                        | BackendRequest::Lstat { .. }
                        | BackendRequest::SetAttrs { .. } => {
                            Err(crate::backend::BackendError::Unsupported)
                        }
                        other => Err(crate::backend::BackendError::Other(format!(
                            "unexpected request: {other:?}"
                        ))),
                    }
                })
            }
        });

        let backend = DelegatedBackend::with_capabilities(
            handler,
            BackendCapabilities {
                symlinks: false,
                set_attrs: false,
                delegated_safe_streaming_fallback: true,
            },
        );
        let scp = ScpHandler::new(Arc::new(backend.clone()));
        (backend, scp)
    }

    async fn read_line(stream: &mut DuplexStream) -> String {
        let mut out = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).await.unwrap();
            out.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        String::from_utf8(out).unwrap()
    }
    // --- parse_command ---

    #[test]
    fn test_parse_command_receive() {
        let (mode, rec, ptime, path) =
            ScpHandler::<MemoryBackend>::parse_command("scp -t /remote/file.txt").unwrap();
        assert!(matches!(mode, ScpMode::Receive));
        assert!(!rec);
        assert!(!ptime);
        assert_eq!(path, "/remote/file.txt");
    }

    #[test]
    fn test_parse_command_send() {
        let (mode, rec, ptime, path) =
            ScpHandler::<MemoryBackend>::parse_command("scp -f /source.txt").unwrap();
        assert!(matches!(mode, ScpMode::Send));
        assert!(!rec);
        assert!(!ptime);
        assert_eq!(path, "/source.txt");
    }

    #[test]
    fn test_parse_command_recursive_preserve() {
        let (mode, rec, ptime, path) =
            ScpHandler::<MemoryBackend>::parse_command("scp -r -p -t /dir/").unwrap();
        assert!(matches!(mode, ScpMode::Receive));
        assert!(rec);
        assert!(ptime);
        assert_eq!(path, "/dir/");
    }

    #[test]
    fn test_parse_command_no_mode_error() {
        let result = ScpHandler::<MemoryBackend>::parse_command("scp /file.txt");
        assert!(matches!(result, Err(ScpError::NoMode)));
    }

    #[test]
    fn test_parse_command_invalid_command_error() {
        let result = ScpHandler::<MemoryBackend>::parse_command("rsync -t /file");
        assert!(matches!(result, Err(ScpError::InvalidCommand(_))));
    }

    #[test]
    fn test_parse_command_default_path() {
        let (_, _, _, path) = ScpHandler::<MemoryBackend>::parse_command("scp -t").unwrap();
        assert_eq!(path, "/");
    }

    #[test]
    fn test_parse_command_uses_last_non_flag_path() {
        let (_, _, _, path) =
            ScpHandler::<MemoryBackend>::parse_command("scp -p -t /first /second").unwrap();

        assert_eq!(path, "/second");
    }

    // --- parse_file_message ---

    #[test]
    fn test_parse_file_message_valid() {
        let msg = ScpHandler::<MemoryBackend>::parse_file_message("0644 12345 myfile.txt").unwrap();
        match msg {
            ScpMessage::File { mode, size, name } => {
                assert_eq!(mode, "0644");
                assert_eq!(size, 12345);
                assert_eq!(name, "myfile.txt");
            }
            _ => panic!("Expected ScpMessage::File"),
        }
    }

    #[test]
    fn test_parse_file_message_filename_with_spaces() {
        let msg =
            ScpHandler::<MemoryBackend>::parse_file_message("0755 99 my file name.txt").unwrap();
        match msg {
            ScpMessage::File { name, .. } => assert_eq!(name, "my file name.txt"),
            _ => panic!("Expected File"),
        }
    }

    #[test]
    fn test_parse_file_message_invalid_size() {
        let result = ScpHandler::<MemoryBackend>::parse_file_message("0644 notanumber file.txt");
        assert!(matches!(result, Err(ScpError::Protocol(_))));
    }

    #[test]
    fn test_parse_file_message_too_few_parts() {
        let result = ScpHandler::<MemoryBackend>::parse_file_message("0644 123");
        assert!(matches!(result, Err(ScpError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_receive_file_writes_to_current_directory() {
        let (backend, mut handler) = make_handler_with_backend();
        let (mut client, mut server) = tokio::io::duplex(128);

        let server_task = tokio::spawn(async move {
            let mut pending_mtime = None;
            handler
                .receive_file(
                    &mut server,
                    "0644 5 hello.txt",
                    "/docs",
                    None,
                    &mut pending_mtime,
                )
                .await
        });

        let mut status = [0u8; 1];
        client.read_exact(&mut status).await.unwrap();
        assert_eq!(status[0], SCP_OK);
        client.write_all(b"hello\0").await.unwrap();
        client.read_exact(&mut status).await.unwrap();
        assert_eq!(status[0], SCP_OK);

        server_task.await.unwrap().unwrap();
        assert_eq!(
            backend.read_file("/docs/hello.txt").await.unwrap(),
            Bytes::from_static(b"hello")
        );
    }

    #[tokio::test]
    async fn test_receive_file_uses_explicit_target() {
        let (backend, mut handler) = make_handler_with_backend();
        let (mut client, mut server) = tokio::io::duplex(128);

        let server_task = tokio::spawn(async move {
            let mut pending_mtime = None;
            handler
                .receive_file(
                    &mut server,
                    "0644 4 ignored-name.txt",
                    "/docs",
                    Some("/target.txt"),
                    &mut pending_mtime,
                )
                .await
        });

        let mut status = [0u8; 1];
        client.read_exact(&mut status).await.unwrap();
        assert_eq!(status[0], SCP_OK);
        client.write_all(b"data\0").await.unwrap();
        client.read_exact(&mut status).await.unwrap();
        assert_eq!(status[0], SCP_OK);

        server_task.await.unwrap().unwrap();
        assert_eq!(
            backend.read_file("/target.txt").await.unwrap(),
            Bytes::from_static(b"data")
        );
        assert!(matches!(
            backend.read_file("/docs/ignored-name.txt").await,
            Err(crate::backend::BackendError::NotFound)
        ));
    }

    #[tokio::test]
    async fn test_receive_file_rejects_bad_confirmation_byte() {
        let (_backend, mut handler) = make_handler_with_backend();
        let (mut client, mut server) = tokio::io::duplex(128);

        let server_task = tokio::spawn(async move {
            let mut pending_mtime = None;
            handler
                .receive_file(&mut server, "0644 3 bad.txt", "/", None, &mut pending_mtime)
                .await
        });

        let mut status = [0u8; 1];
        client.read_exact(&mut status).await.unwrap();
        assert_eq!(status[0], SCP_OK);
        client.write_all(b"badx").await.unwrap();

        let result = server_task.await.unwrap();
        assert!(matches!(result, Err(ScpError::Protocol(msg)) if msg.contains("null byte")));
    }

    #[tokio::test]
    async fn test_receive_file_consumes_pending_mtime() {
        let (_backend, mut handler) = make_handler_with_backend();
        let (mut client, mut server) = tokio::io::duplex(128);

        let server_task = tokio::spawn(async move {
            let mut pending_mtime = Some(123);
            let result = handler
                .receive_file(
                    &mut server,
                    "0644 1 time.txt",
                    "/",
                    None,
                    &mut pending_mtime,
                )
                .await;
            result.map(|()| pending_mtime)
        });

        let mut status = [0u8; 1];
        client.read_exact(&mut status).await.unwrap();
        assert_eq!(status[0], SCP_OK);
        client.write_all(b"t\0").await.unwrap();
        client.read_exact(&mut status).await.unwrap();
        assert_eq!(status[0], SCP_OK);

        let pending_mtime = server_task.await.unwrap().unwrap();
        assert_eq!(pending_mtime, None);
    }

    // --- parse_dir_message ---

    #[test]
    fn test_parse_dir_message_valid() {
        let msg = ScpHandler::<MemoryBackend>::parse_dir_message("0755 0 mydir").unwrap();
        match msg {
            ScpMessage::Dir { mode, name } => {
                assert_eq!(mode, "0755");
                assert_eq!(name, "mydir");
            }
            _ => panic!("Expected Dir"),
        }
    }

    #[test]
    fn test_parse_dir_message_too_few_parts() {
        let result = ScpHandler::<MemoryBackend>::parse_dir_message("0755 0");
        assert!(matches!(result, Err(ScpError::Protocol(_))));
    }

    // --- expand_tilde ---

    #[test]
    fn test_expand_tilde_alone() {
        let result = ScpHandler::<MemoryBackend>::expand_tilde("~");
        assert_eq!(result, "/");
    }

    #[test]
    fn test_expand_tilde_with_path() {
        let result = ScpHandler::<MemoryBackend>::expand_tilde("~/docs/file.txt");
        assert_eq!(result, "/docs/file.txt");
    }

    #[test]
    fn test_expand_tilde_no_tilde() {
        let result = ScpHandler::<MemoryBackend>::expand_tilde("/abs/path");
        assert_eq!(result, "/abs/path");
    }

    #[test]
    fn test_expand_tilde_relative() {
        let result = ScpHandler::<MemoryBackend>::expand_tilde("relative/path");
        assert_eq!(result, "relative/path");
    }

    // --- ScpError display ---

    #[test]
    fn test_scp_error_display() {
        assert_eq!(
            ScpError::NoMode.to_string(),
            "SCP mode not specified (-t or -f required)"
        );
        assert_eq!(
            ScpError::UnexpectedEof.to_string(),
            "Unexpected end of file"
        );
        assert!(ScpError::Backend("oops".into())
            .to_string()
            .contains("oops"));
        assert!(ScpError::Protocol("bad".into()).to_string().contains("bad"));
        assert!(ScpError::InvalidCommand("rsync".into())
            .to_string()
            .contains("rsync"));
    }

    #[tokio::test]
    async fn test_receive_file_upload() {
        let (backend, mut handler) = make_backend_and_handler();
        let (mut client, server) = duplex(4096);

        let task = tokio::spawn(async move { handler.run(server, "scp -t /uploaded.txt").await });

        let mut ack = [0u8; 1];
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);

        client.write_all(b"C0644 5 uploaded.txt\n").await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);
        client.write_all(b"hello").await.unwrap();
        client.write_all(&[SCP_OK]).await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);
        drop(client);

        task.await.unwrap().unwrap();
        assert_eq!(
            backend.read_file("uploaded.txt").await.unwrap(),
            bytes::Bytes::from_static(b"hello")
        );
    }

    #[tokio::test]
    async fn test_receive_recursive_directory_upload() {
        let (backend, mut handler) = make_backend_and_handler();
        let (mut client, server) = duplex(8192);

        let task = tokio::spawn(async move { handler.run(server, "scp -r -t /dst/").await });

        let mut ack = [0u8; 1];
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);

        client.write_all(b"D0755 0 nested\n").await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);

        client.write_all(b"C0644 4 file.txt\n").await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);
        client.write_all(b"data").await.unwrap();
        client.write_all(&[SCP_OK]).await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);

        client.write_all(b"E\n").await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);
        drop(client);

        task.await.unwrap().unwrap();
        assert_eq!(
            backend.read_file("dst/nested/file.txt").await.unwrap(),
            bytes::Bytes::from_static(b"data")
        );
        assert_eq!(
            backend.file_info("dst/nested").await.unwrap().permissions,
            0o755
        );
        assert_eq!(
            backend
                .file_info("dst/nested/file.txt")
                .await
                .unwrap()
                .permissions,
            0o644
        );
    }

    #[tokio::test]
    async fn test_receive_recursive_directory_upload_skips_set_attrs_when_unsupported() {
        let (backend, mut handler) = make_no_attrs_backend_and_handler();
        let (mut client, server) = duplex(8192);

        let task = tokio::spawn(async move { handler.run(server, "scp -r -t /dst/").await });

        let mut ack = [0u8; 1];
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);

        client.write_all(b"D0755 0 nested\n").await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);

        client.write_all(b"C0644 4 file.txt\n").await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);
        client.write_all(b"data").await.unwrap();
        client.write_all(&[SCP_OK]).await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);

        client.write_all(b"E\n").await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);
        drop(client);

        task.await.unwrap().unwrap();
        assert_eq!(
            backend.read_file("/dst/nested/file.txt").await.unwrap(),
            bytes::Bytes::from_static(b"data")
        );
    }

    #[tokio::test]
    async fn test_receive_recursive_directory_uses_explicit_target_path() {
        let (backend, mut handler) = make_backend_and_handler();
        let (mut client, server) = duplex(8192);

        let task = tokio::spawn(async move { handler.run(server, "scp -r -t /renamed").await });

        let mut ack = [0u8; 1];
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);

        client.write_all(b"D0750 0 nested\n").await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);

        client.write_all(b"C0600 4 file.txt\n").await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);
        client.write_all(b"data").await.unwrap();
        client.write_all(&[SCP_OK]).await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);

        client.write_all(b"E\n").await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);
        drop(client);

        task.await.unwrap().unwrap();
        assert_eq!(
            backend.read_file("renamed/file.txt").await.unwrap(),
            bytes::Bytes::from_static(b"data")
        );
        assert_eq!(
            backend.file_info("renamed").await.unwrap().permissions,
            0o750
        );
        assert_eq!(
            backend
                .file_info("renamed/file.txt")
                .await
                .unwrap()
                .permissions,
            0o600
        );
    }

    #[tokio::test]
    async fn test_receive_preserves_times_when_supported() {
        let (backend, mut handler) = make_backend_and_handler();
        let (mut client, server) = duplex(4096);

        let task = tokio::spawn(async move { handler.run(server, "scp -p -t /timed.txt").await });

        let mut ack = [0u8; 1];
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);

        client.write_all(b"T200 0 100 0\n").await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);

        client.write_all(b"C0644 1 timed.txt\n").await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);
        client.write_all(b"x").await.unwrap();
        client.write_all(&[SCP_OK]).await.unwrap();
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], SCP_OK);
        drop(client);

        task.await.unwrap().unwrap();
        let info = backend.file_info("timed.txt").await.unwrap();
        assert_eq!(info.mtime, 200);
        assert_eq!(info.atime, 100);
    }

    #[tokio::test]
    async fn test_send_file_download() {
        let (backend, mut handler) = make_backend_and_handler();
        backend
            .write_file("source.txt", bytes::Bytes::from_static(b"hello"))
            .await
            .unwrap();
        let (mut client, server) = duplex(4096);

        let task = tokio::spawn(async move { handler.run(server, "scp -f /source.txt").await });

        client.write_all(&[SCP_OK]).await.unwrap();
        let file_line = read_line(&mut client).await;
        assert_eq!(file_line, "C0644 5 source.txt\n");
        client.write_all(&[SCP_OK]).await.unwrap();

        let mut data = [0u8; 5];
        client.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"hello");

        let mut term = [0u8; 1];
        client.read_exact(&mut term).await.unwrap();
        assert_eq!(term[0], SCP_OK);
        client.write_all(&[SCP_OK]).await.unwrap();

        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_send_recursive_directory_download() {
        let (backend, mut handler) = make_backend_and_handler();
        backend.make_dir("src").await.unwrap();
        backend
            .write_file("src/file.txt", bytes::Bytes::from_static(b"data"))
            .await
            .unwrap();
        let (mut client, server) = duplex(8192);

        let task = tokio::spawn(async move { handler.run(server, "scp -r -f /src").await });

        client.write_all(&[SCP_OK]).await.unwrap();

        let dir_line = read_line(&mut client).await;
        assert_eq!(dir_line, "D0755 0 src\n");
        client.write_all(&[SCP_OK]).await.unwrap();

        let file_line = read_line(&mut client).await;
        assert_eq!(file_line, "C0644 4 file.txt\n");
        client.write_all(&[SCP_OK]).await.unwrap();

        let mut data = [0u8; 4];
        client.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"data");

        let mut term = [0u8; 1];
        client.read_exact(&mut term).await.unwrap();
        assert_eq!(term[0], SCP_OK);
        client.write_all(&[SCP_OK]).await.unwrap();

        let end_line = read_line(&mut client).await;
        assert_eq!(end_line, "E\n");
        client.write_all(&[SCP_OK]).await.unwrap();

        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_send_symlink_fails_explicitly() {
        let (backend, mut handler) = make_backend_and_handler();
        backend.symlink("link.txt", "target.txt").await.unwrap();
        let (mut client, server) = duplex(4096);

        let task = tokio::spawn(async move { handler.run(server, "scp -f /link.txt").await });
        client.write_all(&[SCP_OK]).await.unwrap();

        let error = task.await.unwrap().unwrap_err();
        assert!(matches!(error, ScpError::Protocol(_)));
        assert!(error
            .to_string()
            .contains("symlink transfer is unsupported"));
    }
}
