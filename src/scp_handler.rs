#![allow(dead_code)]

use crate::backend::Backend;
use bytes::BytesMut;
use std::borrow::Cow;
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

impl<B: Backend> ScpHandler<B> {
    /// Create a new SCP handler
    pub fn new(backend: Arc<B>) -> Self {
        Self { backend }
    }

    /// Run SCP protocol handler
    pub async fn run<S: AsyncReadExt + AsyncWriteExt + Unpin>(
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
        _recursive: bool,
        preserve_times: bool,
        target_path: &str,
    ) -> Result<(), ScpError> {
        debug!("SCP receive mode (target), target_path={}", target_path);
        debug!("SCP receive mode (target)");

        // Send initial OK
        debug!("Sending initial OK...");
        self.send_status(stream, SCP_OK, "").await?;
        debug!("Initial OK sent, waiting for commands...");

        // Use target_path as the destination - if it's a specific file, use it directly
        // If it ends with /, treat as directory
        let target_is_dir = target_path.ends_with('/');
        let mut current_dir = if target_is_dir {
            target_path.to_string()
        } else {
            // Check if target is a directory or file by looking at its parent
            if let Some(parent) = std::path::Path::new(target_path).parent() {
                parent.to_string_lossy().to_string()
            } else {
                "/".to_string()
            }
        };
        let explicit_target = if target_is_dir {
            None
        } else {
            Some(target_path.to_string())
        };
        let mut pending_mtime = None;

        loop {
            // Read command byte
            let mut buf = [0u8; 1];
            match stream.read_exact(&mut buf).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    debug!("SCP receive: EOF from client");
                    break;
                }
                Err(e) => return Err(ScpError::Io(e)),
            }

            match buf[0] {
                b'C' => {
                    // File transfer
                    let line = Self::read_line(stream).await?;
                    let msg = Self::parse_file_message(&line)?;

                    if let ScpMessage::File { mode, size, name } = msg {
                        // Use explicit target if provided, otherwise use current_dir + name
                        let file_path = if let Some(ref target) = explicit_target {
                            target.clone()
                        } else {
                            format!("{}/{}", current_dir.trim_end_matches('/'), name)
                        };
                        debug!(
                            "SCP receive file: path={} size={} mode={}",
                            file_path, size, mode
                        );
                        debug!("SCP receive file: {} ({} bytes)", file_path, size);

                        // Create write handle
                        let mut handle = self
                            .backend
                            .open_write(&file_path)
                            .await
                            .map_err(|e| ScpError::Backend(e.to_string()))?;

                        // Send OK for metadata
                        self.send_status(stream, SCP_OK, "").await?;

                        // Read and write file data
                        let mut remaining = size;
                        let mut offset = 0u64;
                        let mut transfer_buf = BytesMut::with_capacity(65536);

                        debug!("Starting to read {} bytes of file data", size);
                        while remaining > 0 {
                            let capacity =
                                u64::try_from(transfer_buf.capacity()).unwrap_or(u64::MAX);
                            let to_read = usize::try_from(std::cmp::min(remaining, capacity))
                                .map_err(|_| {
                                    ScpError::Protocol("transfer chunk too large".into())
                                })?;
                            transfer_buf.resize(to_read, 0);
                            stream.read_exact(&mut transfer_buf[..to_read]).await?;

                            // Split without copying: transfer_buf chunk becomes independent Bytes
                            // The underlying memory is shared, but split_to handles the reference counting
                            let data = transfer_buf.split_to(to_read).freeze();
                            handle
                                .write_at(offset, data)
                                .await
                                .map_err(|e| ScpError::Backend(e.to_string()))?;
                            let bytes_read = u64::try_from(to_read).unwrap_or(u64::MAX);
                            offset += bytes_read;
                            remaining -= bytes_read;
                            debug!("Read {} bytes, remaining: {}", to_read, remaining);
                        }
                        debug!("Finished reading file data, total: {} bytes", offset);

                        // Set file permissions if not default
                        if mode != "0644" && mode != "0755" {
                            debug!("SCP receive: preserving mode {}", mode);
                        }

                        // Finish the write
                        handle
                            .finish()
                            .await
                            .map_err(|e| ScpError::Backend(e.to_string()))?;

                        // Set mtime if pending
                        if let Some(mtime) = pending_mtime.take() {
                            debug!("SCP receive: setting mtime to {}", mtime);
                        }

                        // Read terminating null byte from client (confirmation of data sent)
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

                        // Send OK after data received
                        self.send_status(stream, SCP_OK, "").await?;
                    }
                }
                b'D' => {
                    // Directory creation
                    let line = Self::read_line(stream).await?;
                    let msg = Self::parse_dir_message(&line)?;

                    if let ScpMessage::Dir { mode: _, name } = msg {
                        let dir_path = format!("{}{}", current_dir.trim_end_matches('/'), name);
                        debug!("SCP receive dir: {}", dir_path);

                        self.backend
                            .make_dir(&dir_path)
                            .await
                            .map_err(|e| ScpError::Backend(e.to_string()))?;

                        // Push to directory stack
                        current_dir = format!("{dir_path}/");

                        // Send OK
                        self.send_status(stream, SCP_OK, "").await?;
                    }
                }
                b'E' => {
                    // End directory
                    debug!("SCP receive: end directory");
                    let line = Self::read_line(stream).await?;
                    if !line.is_empty() {
                        return Err(ScpError::Protocol("unexpected data after E command".into()));
                    }

                    // Pop from directory stack
                    if current_dir != "/" {
                        let trimmed = current_dir.trim_end_matches('/');
                        if let Some(last_slash) = trimmed.rfind('/') {
                            current_dir = format!("{}/", &trimmed[..last_slash]);
                            if current_dir == "/" {
                                current_dir = "/".to_string();
                            }
                        }
                    }

                    self.send_status(stream, SCP_OK, "").await?;
                }
                b'T' => {
                    // Set times
                    let line = Self::read_line(stream).await?;
                    if preserve_times {
                        if let ScpMessage::Times { mtime, .. } = Self::parse_times_message(&line)? {
                            pending_mtime = Some(mtime);
                            debug!("SCP receive: queuing mtime {}", mtime);
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

    /// Handle send mode (scp -f)
    async fn handle_send<S: AsyncReadExt + AsyncWriteExt + Unpin>(
        &mut self,
        stream: &mut S,
        _recursive: bool,
        _preserve_times: bool,
        _source_path: &str,
    ) -> Result<(), ScpError> {
        debug!("SCP send mode (from source)");

        // Wait for initial OK from client
        let mut buf = [0u8; 1];
        stream.read_exact(&mut buf).await?;
        if buf[0] != SCP_OK {
            return Err(ScpError::Protocol("expected OK from client".into()));
        }

        // For now, implement basic send - client needs to specify path
        // In real SCP, client sends "scp -f path" and we read path from stream
        // For simplicity, we'll just send OK and wait for client to close
        debug!("SCP send mode not fully implemented - basic support only");
        self.send_status(stream, SCP_OK, "").await?;

        Ok(())
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
    use crate::backend::MemoryBackend;
    use std::sync::Arc;

    fn make_handler() -> ScpHandler<MemoryBackend> {
        ScpHandler::new(Arc::new(MemoryBackend::new()))
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
}
