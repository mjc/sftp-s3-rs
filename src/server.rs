use crate::backend::Backend;
use crate::ssh_handler::{AuthConfig, SshServer};
use russh::keys::ssh_key::rand_core::OsRng;
use russh::keys::PublicKey;
use russh::server::{Config as SshConfig, Server as _};
use russh::{cipher, compression, Preferred};
use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Server configuration
#[derive(Clone)]
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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 2222,
            keys: Vec::new(),
            auth_rejection_time: Duration::from_secs(3),
            ciphers: None,
            compression: false,
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
    pub fn with_generated_key(mut self) -> Self {
        let key =
            russh::keys::PrivateKey::random(&mut OsRng, russh::keys::Algorithm::Ed25519).unwrap();
        self.keys.push(key);
        self
    }

    /// Load a host key from a file (OpenSSH format)
    pub fn with_key_file(mut self, path: impl AsRef<Path>) -> Result<Self, russh::keys::Error> {
        let key = russh::keys::load_secret_key(path, None)?;
        self.keys.push(key);
        Ok(self)
    }

    /// Load a host key from PEM/OpenSSH format string data
    pub fn with_key_data(mut self, data: &str) -> Result<Self, russh::keys::Error> {
        let key = russh::keys::decode_secret_key(data, None)?;
        self.keys.push(key);
        Ok(self)
    }

    /// Load host key from HOST_KEY environment variable, or generate one if not set
    pub fn with_key_from_env(self) -> Result<Self, russh::keys::Error> {
        if let Ok(key_data) = std::env::var("HOST_KEY") {
            self.with_key_data(&key_data)
        } else if let Ok(key_path) = std::env::var("HOST_KEY_FILE") {
            self.with_key_file(&key_path)
        } else {
            Ok(self.with_generated_key())
        }
    }

    /// Load host keys from standard system locations (/etc/ssh/ssh_host_*_key)
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
        const CWD_KEY_PATH: &str = "ssh_host_ed25519_key";
        if let Ok(key) = russh::keys::load_secret_key(CWD_KEY_PATH, None) {
            self.keys.push(key);
            return Ok(self);
        }

        // Generate and try to save to cwd
        let key =
            russh::keys::PrivateKey::random(&mut OsRng, russh::keys::Algorithm::Ed25519).unwrap();

        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

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
                .map(|(_, keys)| keys.iter().any(|k| k == key))
                .unwrap_or(false)
        })
    }

    /// Set static users for password authentication
    pub fn with_users(self, users: Vec<(String, String)>) -> Self {
        let users = Arc::new(users);
        self.with_password_auth(move |user, pass| users.iter().any(|(u, p)| u == user && p == pass))
    }

    /// Load authorized keys from ~/.ssh/authorized_keys
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

    /// Run the server
    pub async fn run(mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut keys = self.config.keys.clone();
        if keys.is_empty() {
            keys.push(russh::keys::PrivateKey::random(
                &mut OsRng,
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

        let ssh_config = SshConfig {
            auth_rejection_time: self.config.auth_rejection_time,
            auth_rejection_time_initial: Some(Duration::from_secs(0)),
            methods,
            keys,
            preferred,
            ..Default::default()
        };

        let ssh_config = Arc::new(ssh_config);
        let mut server = SshServer::new(self.backend, self.auth_config);

        let addr = format!("0.0.0.0:{}", self.config.port);
        info!(addr = %addr, "Starting SFTP server");

        server
            .run_on_address(ssh_config, ("0.0.0.0", self.config.port))
            .await?;

        Ok(())
    }
}

/// Convenience function to run a server
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

/// Parse a cipher name string into a cipher::Name
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
