use crate::backend::Backend;
use crate::ssh_handler::{AuthConfig, SshServer};
use russh::keys::PublicKey;
use russh::server::{run_stream, Config as SshConfig, Server as _};
use russh::{cipher, compression, Limits, Preferred};
use std::borrow::Cow;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, info, warn};

const CWD_KEY_PATH: &str = "ssh_host_ed25519_key";
fn generate_ed25519_key() -> Result<russh::keys::PrivateKey, BoxError> {
    let mut seed = [0u8; russh::keys::ssh_key::private::Ed25519PrivateKey::BYTE_SIZE];
    getrandom::fill(&mut seed).map_err(|error| -> BoxError {
        Box::new(std::io::Error::other(format!(
            "failed to generate host key seed: {error}"
        )))
    })?;
    let private = russh::keys::ssh_key::private::Ed25519PrivateKey::from_bytes(&seed);
    Ok(russh::keys::PrivateKey::from(
        russh::keys::ssh_key::private::Ed25519Keypair::from(private),
    ))
}
/// Server configuration
#[derive(Clone)]
#[must_use]
pub struct ServerConfig {
    /// Port to bind to
    pub port: u16,
    /// SSH server keys
    pub keys: Vec<russh::keys::PrivateKey>,
    /// Authentication rejection time
    pub auth_rejection_time: Duration,
    /// Preferred ciphers (in order of preference)
    pub ciphers: Option<Vec<cipher::Name>>,
    /// Enable compression (disabled by default for better throughput)
    pub compression: bool,
    /// Enable `TCP_NODELAY` (disable Nagle's algorithm) for lower latency
    /// Enabled by default for better small file/packet performance
    pub nodelay: bool,
    /// SSH channel window size for flow control (default: 2MB)
    pub window_size: u32,
    /// Maximum SSH channel packet size (default: 256KB)
    pub maximum_packet_size: u32,
    /// Rekey write limit in bytes (default: 1GB). Lower for testing.
    pub rekey_write_limit: usize,
    /// Maximum concurrent SSH connections. `None` means unlimited.
    pub max_connections: Option<usize>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 2222,
            keys: Vec::new(),
            auth_rejection_time: Duration::from_secs(3),
            ciphers: None,
            compression: false,
            nodelay: true, // Enable by default for better small file performance
            window_size: 2 * 1024 * 1024, // 2MB default
            maximum_packet_size: 256 * 1024,
            rekey_write_limit: 1 << 30, // 1GB default (matches russh default)
            max_connections: None,
        }
    }
}

impl ServerConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_key(mut self, key: russh::keys::PrivateKey) -> Self {
        self.keys.push(key);
        self
    }

    /// Set preferred ciphers (in order of preference)
    pub fn with_ciphers(mut self, ciphers: Vec<cipher::Name>) -> Self {
        self.ciphers = Some(ciphers);
        self
    }

    /// Enable compression (disabled by default for better throughput)
    pub fn with_compression(mut self) -> Self {
        self.compression = true;
        self
    }

    /// Generate a random Ed25519 key (useful for testing/development)
    ///
    /// # Panics
    ///
    /// Panics if the operating system random number generator fails.
    pub fn with_generated_key(mut self) -> Self {
        let key = generate_ed25519_key().unwrap();
        self.keys.push(key);
        self
    }

    /// Set rekey write limit in bytes (default: 1GB). Useful for testing rekey behaviour.
    pub fn with_rekey_write_limit(mut self, limit: usize) -> Self {
        self.rekey_write_limit = limit;
        self
    }

    /// Limit the maximum number of concurrent SSH connections.
    pub fn with_max_connections(mut self, max: usize) -> Self {
        assert!(max > 0, "max_connections must be greater than zero");
        self.max_connections = Some(max);
        self
    }

    /// Load a host key from a file (OpenSSH format)
    ///
    /// # Errors
    ///
    /// Returns an error if the key file cannot be read or decoded.
    pub fn with_key_file(mut self, path: impl AsRef<Path>) -> Result<Self, russh::keys::Error> {
        let key = russh::keys::load_secret_key(path, None)?;
        self.keys.push(key);
        Ok(self)
    }

    /// Load a host key from PEM/OpenSSH format string data
    ///
    /// # Errors
    ///
    /// Returns an error if the key data cannot be decoded.
    pub fn with_key_data(mut self, data: &str) -> Result<Self, russh::keys::Error> {
        let key = russh::keys::decode_secret_key(data, None)?;
        self.keys.push(key);
        Ok(self)
    }

    /// Load host key from `HOST_KEY` environment variable, or generate one if not set.
    ///
    /// # Errors
    ///
    /// Returns an error if a configured key cannot be read or decoded.
    pub fn with_key_from_env(self) -> Result<Self, russh::keys::Error> {
        if let Ok(key_data) = std::env::var("HOST_KEY") {
            self.with_key_data(&key_data)
        } else if let Ok(key_path) = std::env::var("HOST_KEY_FILE") {
            self.with_key_file(&key_path)
        } else {
            Ok(self.with_generated_key())
        }
    }

    /// Load host keys from standard system locations (`/etc/ssh/ssh_host_*_key`)
    /// Returns self unchanged if no keys are found (doesn't fail)
    pub fn with_system_keys(mut self) -> Self {
        const SYSTEM_KEY_PATHS: &[&str] = &[
            "/etc/ssh/ssh_host_ed25519_key",
            "/etc/ssh/ssh_host_rsa_key",
            "/etc/ssh/ssh_host_ecdsa_key",
        ];

        for path in SYSTEM_KEY_PATHS {
            if let Ok(key) = russh::keys::load_secret_key(path, None) {
                self.keys.push(key);
            }
        }
        self
    }

    /// Load host key from env, then system, then cwd, then generate (and save to cwd)
    ///
    /// # Errors
    ///
    /// Returns an error if a configured key cannot be read or decoded.
    ///
    /// # Panics
    ///
    /// Panics if generating a fallback key or serializing it to OpenSSH format fails.
    pub fn with_default_keys(mut self) -> Result<Self, russh::keys::Error> {
        if let Ok(key_data) = std::env::var("HOST_KEY") {
            return self.with_key_data(&key_data);
        }
        if let Ok(key_path) = std::env::var("HOST_KEY_FILE") {
            return self.with_key_file(&key_path);
        }

        self = self.with_system_keys();
        if !self.keys.is_empty() {
            return Ok(self);
        }

        // Try loading from cwd
        if let Ok(key) = russh::keys::load_secret_key(CWD_KEY_PATH, None) {
            self.keys.push(key);
            return Ok(self);
        }

        // Generate and try to save to cwd
        let key = generate_ed25519_key().unwrap();

        let key_str = key
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap();
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(CWD_KEY_PATH) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(key_str.as_bytes()) {
                    warn!(
                        "Generated host key but failed to write to {}: {}",
                        CWD_KEY_PATH, e
                    );
                } else {
                    info!("Generated and saved host key to {}", CWD_KEY_PATH);
                }
            }
            Err(e) => {
                warn!(
                    "Generated host key but failed to create {}: {}",
                    CWD_KEY_PATH, e
                );
            }
        }

        self.keys.push(key);
        Ok(self)
    }
}

/// SFTP server builder
#[must_use]
pub struct Server<B: Backend> {
    backend: Arc<B>,
    config: ServerConfig,
    auth_config: AuthConfig,
}

type BoxError = Box<dyn Error + Send + Sync>;

struct PreparedServer<B: Backend> {
    port: u16,
    max_connections: Option<usize>,
    server: SshServer<B>,
    ssh_config: Arc<SshConfig>,
}

/// Running server handle for lifecycle control.
pub struct ServerHandle {
    local_addr: std::net::SocketAddr,
    shutdown_tx: watch::Sender<bool>,
    accept_task: JoinHandle<Result<(), BoxError>>,
}

impl ServerHandle {
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    pub async fn shutdown(self) -> Result<(), BoxError> {
        let _ = self.shutdown_tx.send(true);
        self.wait().await
    }

    pub async fn wait(self) -> Result<(), BoxError> {
        match self.accept_task.await {
            Ok(result) => result,
            Err(err) => Err(Box::new(err)),
        }
    }
}

impl<B: Backend> Server<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend: Arc::new(backend),
            config: ServerConfig::default(),
            auth_config: AuthConfig::default(),
        }
    }

    pub fn config(mut self, config: ServerConfig) -> Self {
        self.config = config;
        self
    }

    /// Set password authentication callback
    pub fn with_password_auth<F>(mut self, callback: F) -> Self
    where
        F: Fn(&str, &str) -> bool + Send + Sync + 'static,
    {
        self.auth_config.password_callback = Some(Arc::new(callback));
        self
    }

    /// Set public key authentication callback
    pub fn with_pubkey_auth<F>(mut self, callback: F) -> Self
    where
        F: Fn(&str, &PublicKey) -> bool + Send + Sync + 'static,
    {
        self.auth_config.pubkey_callback = Some(Arc::new(callback));
        self
    }

    /// Set authorized keys for a user (convenience method for pubkey auth)
    pub fn with_authorized_keys(self, authorized: Vec<(String, Vec<PublicKey>)>) -> Self {
        let authorized = Arc::new(authorized);
        self.with_pubkey_auth(move |user, key| {
            authorized
                .iter()
                .find(|(u, _)| u == user)
                .is_some_and(|(_, keys)| keys.iter().any(|k| k == key))
        })
    }

    /// Set static users for password authentication
    pub fn with_users(self, users: Vec<(String, String)>) -> Self {
        let users = Arc::new(users);
        self.with_password_auth(move |user, pass| users.iter().any(|(u, p)| u == user && p == pass))
    }

    /// Load authorized keys from `~/.ssh/authorized_keys`.
    /// Returns self unchanged if file not found or not readable (doesn't fail)
    pub fn with_default_auth(self) -> Self {
        if let Some(home) = std::env::var_os("HOME") {
            let path = Path::new(&home).join(".ssh/authorized_keys");
            if let Ok(contents) = std::fs::read_to_string(&path) {
                let keys: Vec<PublicKey> = contents
                    .lines()
                    .filter_map(|line| {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') {
                            return None;
                        }
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() < 2 {
                            return None;
                        }
                        russh::keys::parse_public_key_base64(parts[1]).ok()
                    })
                    .collect();

                if !keys.is_empty() {
                    info!("Loaded {} key(s) from ~/.ssh/authorized_keys", keys.len());
                    let keys = Arc::new(keys);
                    return self.with_pubkey_auth(move |_user, key| keys.iter().any(|k| k == key));
                }
            }
        }
        self
    }

    /// Build SSH config and server from current settings (shared setup for `run`/`run_on_socket`).
    #[allow(clippy::type_complexity)]
    fn prepare(mut self) -> Result<PreparedServer<B>, BoxError> {
        let port = self.config.port;
        let max_connections = self.config.max_connections;
        let mut keys = self.config.keys.clone();
        if keys.is_empty() {
            keys.push(generate_ed25519_key()?);
        }

        // If no auth configured, try ~/.ssh/authorized_keys
        if self.auth_config.password_callback.is_none()
            && self.auth_config.pubkey_callback.is_none()
        {
            self = self.with_default_auth();
        }

        // Determine which auth methods to advertise
        let mut methods = russh::MethodSet::empty();
        if self.auth_config.password_callback.is_some() {
            methods.push(russh::MethodKind::Password);
        }
        if self.auth_config.pubkey_callback.is_some() {
            methods.push(russh::MethodKind::PublicKey);
        }
        // Default to password if nothing configured (and no authorized_keys found)
        if methods.is_empty() {
            methods.push(russh::MethodKind::Password);
        }

        let mut preferred = Preferred::DEFAULT;
        if let Some(ref ciphers) = self.config.ciphers {
            preferred.cipher = Cow::Owned(ciphers.clone());
        } else {
            // Prefer hardware-accelerated AES-GCM on modern hosts, while keeping
            // ChaCha20-Poly1305 available for clients that prefer it.
            preferred.cipher = Cow::Borrowed(&[
                cipher::AES_128_GCM,
                cipher::AES_256_GCM,
                cipher::CHACHA20_POLY1305,
                cipher::AES_256_CTR,
                cipher::AES_128_CTR,
            ]);
        }
        if !self.config.compression {
            preferred.compression = Cow::Borrowed(&[compression::NONE]);
        }

        let ssh_config = Arc::new(SshConfig {
            auth_rejection_time: self.config.auth_rejection_time,
            auth_rejection_time_initial: Some(Duration::from_secs(0)),
            methods,
            keys,
            preferred,
            nodelay: self.config.nodelay,
            window_size: self.config.window_size,
            maximum_packet_size: self.config.maximum_packet_size,
            limits: Limits {
                rekey_write_limit: self.config.rekey_write_limit,
                ..Limits::default()
            },
            ..Default::default()
        });

        let server = SshServer::new(self.backend, self.auth_config);
        Ok(PreparedServer {
            port,
            max_connections,
            server,
            ssh_config,
        })
    }

    /// Start the server and return a lifecycle handle.
    pub async fn serve(self) -> Result<ServerHandle, BoxError> {
        let prepared = self.prepare()?;
        let bind_addr = ("0.0.0.0", prepared.port);
        let socket = TcpListener::bind(bind_addr).await?;
        Self::serve_prepared_on_socket(prepared, socket).await
    }

    /// Start the server on a pre-bound listener and return a lifecycle handle.
    pub async fn serve_on_socket(self, socket: TcpListener) -> Result<ServerHandle, BoxError> {
        let prepared = self.prepare()?;
        Self::serve_prepared_on_socket(prepared, socket).await
    }

    async fn serve_prepared_on_socket(
        prepared: PreparedServer<B>,
        socket: TcpListener,
    ) -> Result<ServerHandle, BoxError> {
        let local_addr = socket.local_addr()?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        if let Some(limit) = prepared.max_connections {
            info!(addr = %local_addr, max_connections = limit, "Starting SFTP server");
        } else {
            info!(addr = %local_addr, "Starting SFTP server");
        }

        let accept_task = tokio::spawn(run_accept_loop(
            socket,
            prepared.server,
            prepared.ssh_config,
            prepared.max_connections,
            shutdown_rx,
        ));

        Ok(ServerHandle {
            local_addr,
            shutdown_tx,
            accept_task,
        })
    }

    /// Run the server until it exits.
    pub async fn run(self) -> Result<(), BoxError> {
        self.serve().await?.wait().await
    }

    /// Run the server on a pre-bound `TcpListener` (useful for testing with dynamic ports).
    ///
    /// # Errors
    ///
    /// Returns an error if SSH server setup fails or the listener cannot run.
    pub async fn run_on_socket(self, socket: &TcpListener) -> Result<(), BoxError> {
        let cloned = clone_listener(socket)?;
        self.serve_on_socket(cloned).await?.wait().await
    }
}

/// Convenience function to run a server
///
/// # Errors
///
/// Returns an error if SSH server setup fails or the listener cannot run.
pub async fn run<B: Backend>(
    backend: B,
    config: ServerConfig,
    users: Vec<(String, String)>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Server::new(backend)
        .config(config)
        .with_users(users)
        .run()
        .await
}

async fn run_accept_loop<B: Backend>(
    socket: TcpListener,
    mut server: SshServer<B>,
    ssh_config: Arc<SshConfig>,
    max_connections: Option<usize>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), BoxError> {
    let semaphore = max_connections.map(|limit| Arc::new(Semaphore::new(limit)));
    let mut connections: JoinSet<Result<(), BoxError>> = JoinSet::new();

    loop {
        tokio::select! {
            biased;

            shutdown = shutdown_rx.changed() => {
                match shutdown {
                    Ok(()) if *shutdown_rx.borrow() => {
                        debug!("Server shutdown requested");
                        break;
                    }
                    Ok(()) => {}
                    Err(_) => break,
                }
            }

            Some(join_result) = connections.join_next(), if !connections.is_empty() => {
                handle_connection_result(join_result)?;
            }

            accept_result = socket.accept() => {
                let (stream, peer_addr) = accept_result?;
                let PermitState::Continue(permit) =
                    acquire_connection_permit(semaphore.clone(), &mut shutdown_rx).await?
                else {
                    break;
                };

                let handler = server.new_client(Some(peer_addr));
                let config = ssh_config.clone();
                let mut connection_shutdown = shutdown_rx.clone();

                connections.spawn(async move {
                    run_connection(stream, config, handler, &mut connection_shutdown, permit).await
                });
            }
        }
    }

    while let Some(join_result) = connections.join_next().await {
        handle_connection_result(join_result)?;
    }

    Ok(())
}

enum PermitState {
    Continue(Option<OwnedSemaphorePermit>),
    Shutdown,
}

async fn acquire_connection_permit(
    semaphore: Option<Arc<Semaphore>>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<PermitState, BoxError> {
    let Some(semaphore) = semaphore else {
        return Ok(PermitState::Continue(None));
    };

    tokio::select! {
        biased;

        shutdown = shutdown_rx.changed() => {
            match shutdown {
                Ok(()) if *shutdown_rx.borrow() => Ok(PermitState::Shutdown),
                Ok(()) => unreachable!("shutdown receiver changed without a shutdown signal"),
                Err(_) => Ok(PermitState::Shutdown),
            }
        }
        permit = semaphore.acquire_owned() => {
            Ok(PermitState::Continue(Some(permit?)))
        }
    }
}

async fn run_connection<H>(
    socket: tokio::net::TcpStream,
    config: Arc<SshConfig>,
    handler: H,
    shutdown_rx: &mut watch::Receiver<bool>,
    _permit: Option<OwnedSemaphorePermit>,
) -> Result<(), BoxError>
where
    H: russh::server::Handler<Error = russh::Error> + Send + 'static,
{
    if config.nodelay {
        if let Err(err) = socket.set_nodelay(true) {
            warn!("set_nodelay() failed: {err:?}");
        }
    }

    let session = tokio::select! {
        biased;

        shutdown = shutdown_rx.changed() => {
            match shutdown {
                Ok(()) if *shutdown_rx.borrow() => return Ok(()),
                Ok(()) => unreachable!("shutdown receiver changed without a shutdown signal"),
                Err(_) => return Ok(()),
            }
        }
        session = run_stream(config, socket, handler) => session.map_err(|err| -> BoxError { Box::new(err) })?,
    };

    let handle = session.handle();

    tokio::select! {
        biased;

        shutdown = shutdown_rx.changed() => {
            match shutdown {
                Ok(()) if *shutdown_rx.borrow() => {
                    if let Err(err) = handle.disconnect(
                        russh::Disconnect::ByApplication,
                        "server shutting down".into(),
                        "".into(),
                    ).await {
                        debug!("Failed to send disconnect message: {err:?}");
                    }
                }
                Ok(()) => {}
                Err(_) => {}
            }
        }
        result = session => {
            result.map_err(|err| -> BoxError { Box::new(err) })?;
        }
    }

    Ok(())
}

fn handle_connection_result(
    join_result: Result<Result<(), BoxError>, tokio::task::JoinError>,
) -> Result<(), BoxError> {
    match join_result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => {
            warn!("Connection closed with error: {err}");
            Ok(())
        }
        Err(err) => Err(Box::new(err)),
    }
}

#[cfg(unix)]
fn clone_listener(socket: &TcpListener) -> Result<TcpListener, BoxError> {
    use std::net::TcpListener as StdTcpListener;
    use std::os::fd::{AsFd, OwnedFd};

    let owned: OwnedFd = socket.as_fd().try_clone_to_owned()?;
    let std_listener = StdTcpListener::from(owned);
    std_listener.set_nonblocking(true)?;
    Ok(TcpListener::from_std(std_listener)?)
}

#[cfg(windows)]
fn clone_listener(socket: &TcpListener) -> Result<TcpListener, BoxError> {
    use std::net::TcpListener as StdTcpListener;
    use std::os::windows::io::{AsSocket, OwnedSocket};

    let owned: OwnedSocket = socket.as_socket().try_clone_to_owned()?;
    let std_listener = StdTcpListener::from(owned);
    std_listener.set_nonblocking(true)?;
    Ok(TcpListener::from_std(std_listener)?)
}

// Re-export auth types for advanced usage
pub use crate::ssh_handler::{PasswordAuthCallback, PubkeyAuthCallback};

/// Parse a cipher name string into a `cipher::Name`.
#[must_use]
pub fn parse_cipher(s: &str) -> Option<cipher::Name> {
    match s.trim() {
        "aes128-gcm" | "aes128-gcm@openssh.com" => Some(cipher::AES_128_GCM),
        "aes256-gcm" | "aes256-gcm@openssh.com" => Some(cipher::AES_256_GCM),
        "aes128-ctr" => Some(cipher::AES_128_CTR),
        "aes192-ctr" => Some(cipher::AES_192_CTR),
        "aes256-ctr" => Some(cipher::AES_256_CTR),
        "aes128-cbc" => Some(cipher::AES_128_CBC),
        "aes192-cbc" => Some(cipher::AES_192_CBC),
        "aes256-cbc" => Some(cipher::AES_256_CBC),
        "chacha20-poly1305" | "chacha20-poly1305@openssh.com" => Some(cipher::CHACHA20_POLY1305),
        _ => None,
    }
}

/// List of available cipher names for help text
pub const AVAILABLE_CIPHERS: &[&str] = &[
    "aes128-gcm",
    "aes256-gcm",
    "aes128-ctr",
    "aes192-ctr",
    "aes256-ctr",
    "aes128-cbc",
    "aes192-cbc",
    "aes256-cbc",
    "chacha20-poly1305",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MemoryBackend;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;

    // --- ServerConfig defaults and builder ---

    #[test]
    fn test_server_config_defaults() {
        let config = ServerConfig::default();
        assert_eq!(config.port, 2222);
        assert!(config.keys.is_empty());
        assert_eq!(config.auth_rejection_time, Duration::from_secs(3));
        assert!(config.ciphers.is_none());
        assert!(!config.compression);
        assert!(config.nodelay);
        assert_eq!(config.window_size, 2 * 1024 * 1024);
        assert_eq!(config.maximum_packet_size, 256 * 1024);
        assert!(config.max_connections.is_none());
    }

    #[test]
    fn test_server_config_builder_chaining() {
        let config = ServerConfig::new()
            .port(2345)
            .with_compression()
            .with_max_connections(4)
            .with_generated_key();
        assert_eq!(config.port, 2345);
        assert!(config.compression);
        assert_eq!(config.max_connections, Some(4));
        assert_eq!(config.keys.len(), 1);
    }

    #[test]
    fn test_server_config_port_override() {
        let config = ServerConfig::new().port(9999);
        assert_eq!(config.port, 9999);
    }

    #[test]
    fn test_server_config_with_ciphers() {
        let ciphers = vec![cipher::CHACHA20_POLY1305, cipher::AES_256_GCM];
        let config = ServerConfig::new().with_ciphers(ciphers.clone());
        assert!(config.ciphers.is_some());
        assert_eq!(config.ciphers.as_ref().unwrap().len(), 2);
    }

    // --- parse_cipher ---

    #[test]
    fn test_parse_cipher_known() {
        assert!(parse_cipher("aes256-gcm").is_some());
        assert!(parse_cipher("aes256-gcm@openssh.com").is_some());
        assert!(parse_cipher("aes128-ctr").is_some());
        assert!(parse_cipher("aes192-ctr").is_some());
        assert!(parse_cipher("aes256-ctr").is_some());
        assert!(parse_cipher("aes128-cbc").is_some());
        assert!(parse_cipher("aes192-cbc").is_some());
        assert!(parse_cipher("aes256-cbc").is_some());
        assert!(parse_cipher("chacha20-poly1305").is_some());
        assert!(parse_cipher("chacha20-poly1305@openssh.com").is_some());
    }

    #[test]
    fn test_parse_cipher_unknown() {
        assert!(parse_cipher("unknown-cipher").is_none());
        assert!(parse_cipher("").is_none());
        assert!(parse_cipher("aes512-gcm").is_none());
    }

    #[test]
    fn test_parse_cipher_trims_whitespace() {
        assert!(parse_cipher("  aes256-gcm  ").is_some());
    }

    // --- Server builder ---

    #[test]
    fn test_server_with_users() {
        let server = Server::new(MemoryBackend::new())
            .with_users(vec![("alice".to_string(), "pass".to_string())]);

        // Auth config should have a password callback
        assert!(server.auth_config.password_callback.is_some());
        assert!(server.auth_config.pubkey_callback.is_none());

        // Verify callback works
        let cb = server.auth_config.password_callback.as_ref().unwrap();
        assert!(cb("alice", "pass"));
        assert!(!cb("alice", "wrong"));
        assert!(!cb("bob", "pass"));
    }

    #[test]
    fn test_server_with_authorized_keys() {
        let key = generate_ed25519_key().unwrap();
        let pubkey = key.public_key().clone();

        let server = Server::new(MemoryBackend::new())
            .with_authorized_keys(vec![("user".to_string(), vec![pubkey.clone()])]);

        assert!(server.auth_config.pubkey_callback.is_some());
        let cb = server.auth_config.pubkey_callback.as_ref().unwrap();
        assert!(cb("user", &pubkey));
    }

    fn test_server() -> Server<MemoryBackend> {
        Server::new(MemoryBackend::new())
            .config(ServerConfig::new().port(0).with_generated_key())
            .with_users(vec![("test".to_string(), "pass".to_string())])
    }

    async fn connect_raw(addr: std::net::SocketAddr) -> TcpStream {
        TcpStream::connect(addr).await.unwrap()
    }

    async fn read_ssh_banner(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).await.unwrap();
            buf.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        String::from_utf8(buf).unwrap()
    }

    #[tokio::test]
    async fn test_serve_returns_handle_with_bound_address() {
        let handle = test_server().serve().await.unwrap();
        let addr = handle.local_addr();

        assert_ne!(addr.port(), 0);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_shutdown_stops_accepting_new_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let handle = test_server().serve_on_socket(listener).await.unwrap();
        let addr = handle.local_addr();

        handle.shutdown().await.unwrap();

        let connect_result = TcpStream::connect(addr).await;
        assert!(connect_result.is_err());
    }

    #[tokio::test]
    async fn test_shutdown_drains_existing_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let handle = test_server().serve_on_socket(listener).await.unwrap();
        let addr = handle.local_addr();

        let mut stream = connect_raw(addr).await;
        let banner = read_ssh_banner(&mut stream).await;
        assert!(banner.starts_with("SSH-2.0-"));

        stream.write_all(b"SSH-2.0-test-client\r\n").await.unwrap();

        timeout(Duration::from_secs(2), handle.shutdown())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn test_max_connections_one_queues_second_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let handle = Server::new(MemoryBackend::new())
            .config(
                ServerConfig::new()
                    .port(0)
                    .with_generated_key()
                    .with_max_connections(1),
            )
            .serve_on_socket(listener)
            .await
            .unwrap();
        let addr = handle.local_addr();

        let mut first = connect_raw(addr).await;
        let first_banner = read_ssh_banner(&mut first).await;
        assert!(first_banner.starts_with("SSH-2.0-"));

        let mut second = connect_raw(addr).await;
        let queued = timeout(Duration::from_millis(200), read_ssh_banner(&mut second)).await;
        assert!(
            queued.is_err(),
            "second connection should wait for a permit"
        );

        drop(first);

        let second_banner = timeout(Duration::from_secs(2), read_ssh_banner(&mut second))
            .await
            .unwrap();
        assert!(second_banner.starts_with("SSH-2.0-"));

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_unlimited_connections_preserve_current_behavior() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let handle = test_server().serve_on_socket(listener).await.unwrap();
        let addr = handle.local_addr();

        let mut first = connect_raw(addr).await;
        let mut second = connect_raw(addr).await;

        let first_banner = timeout(Duration::from_secs(2), read_ssh_banner(&mut first))
            .await
            .unwrap();
        let second_banner = timeout(Duration::from_secs(2), read_ssh_banner(&mut second))
            .await
            .unwrap();

        assert!(first_banner.starts_with("SSH-2.0-"));
        assert!(second_banner.starts_with("SSH-2.0-"));

        handle.shutdown().await.unwrap();
    }
}
