use crate::backend::Backend;
use crate::ssh_handler::{AuthConfig, SshServer};
use russh::keys::PublicKey;
use russh::server::{Config as SshConfig, Server as _};
use russh::{cipher, compression, Limits, Preferred};
use std::borrow::Cow;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tracing::{info, warn};

const CWD_KEY_PATH: &str = "ssh_host_ed25519_key";
type ServerSetup<B> = (u16, SshServer<B>, Arc<SshConfig>);

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
    /// Maximum SSH packet size (default: 32KB, max: 256KB)
    pub maximum_packet_size: u32,
    /// Rekey write limit in bytes (default: 1GB). Lower for testing.
    pub rekey_write_limit: usize,
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
            maximum_packet_size: 65_535, // russh caps this at the TCP packet limit
            rekey_write_limit: 1 << 30, // 1GB default (matches russh default)
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
        let key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        self.keys.push(key);
        self
    }

    /// Set rekey write limit in bytes (default: 1GB). Useful for testing rekey behaviour.
    pub fn with_rekey_write_limit(mut self, limit: usize) -> Self {
        self.rekey_write_limit = limit;
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
        let key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();

        let key_str = key
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap();
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(CWD_KEY_PATH)
        {
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
    fn prepare(mut self) -> Result<ServerSetup<B>, Box<dyn std::error::Error + Send + Sync>> {
        let port = self.config.port;
        let mut keys = self.config.keys.clone();
        if keys.is_empty() {
            keys.push(russh::keys::PrivateKey::random(
                &mut rand::rng(),
                russh::keys::Algorithm::Ed25519,
            )?);
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
            // Default to ChaCha20-Poly1305 first (faster with AVX2), then AES-GCM
            preferred.cipher = Cow::Borrowed(&[
                cipher::CHACHA20_POLY1305,
                cipher::AES_256_GCM,
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
        Ok((port, server, ssh_config))
    }

    /// Run the server
    ///
    /// # Errors
    ///
    /// Returns an error if SSH server setup fails or the listener cannot run.
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (port, mut server, ssh_config) = self.prepare()?;
        let addr = format!("0.0.0.0:{port}");
        info!(addr = %addr, "Starting SFTP server");
        server.run_on_address(ssh_config, ("0.0.0.0", port)).await?;
        Ok(())
    }

    /// Run the server on a pre-bound `TcpListener` (useful for testing with dynamic ports).
    ///
    /// # Errors
    ///
    /// Returns an error if SSH server setup fails or the listener cannot run.
    pub async fn run_on_socket(
        self,
        socket: &TcpListener,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (_, mut server, ssh_config) = self.prepare()?;
        server.run_on_socket(ssh_config, socket).await?;
        Ok(())
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

// Re-export auth types for advanced usage
pub use crate::ssh_handler::{PasswordAuthCallback, PubkeyAuthCallback};

/// Parse a cipher name string into a `cipher::Name`.
#[must_use]
pub fn parse_cipher(s: &str) -> Option<cipher::Name> {
    match s.trim() {
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
        assert_eq!(config.maximum_packet_size, 65_535);
    }

    #[test]
    fn test_server_config_builder_chaining() {
        let config = ServerConfig::new()
            .port(2345)
            .with_compression()
            .with_generated_key();
        assert_eq!(config.port, 2345);
        assert!(config.compression);
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
        use crate::backend::MemoryBackend;

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
        use crate::backend::MemoryBackend;
        let key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let pubkey = key.public_key().clone();

        let server = Server::new(MemoryBackend::new())
            .with_authorized_keys(vec![("user".to_string(), vec![pubkey.clone()])]);

        assert!(server.auth_config.pubkey_callback.is_some());
        let cb = server.auth_config.pubkey_callback.as_ref().unwrap();
        assert!(cb("user", &pubkey));
    }
}
