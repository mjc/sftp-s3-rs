use crate::backend::Backend;
use crate::ssh_handler::{AuthConfig, SshServer};
use russh::keys::ssh_key::rand_core::OsRng;
use russh::keys::PublicKey;
use russh::server::{Config as SshConfig, Server as _};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// Server configuration
#[derive(Clone)]
pub struct ServerConfig {
    /// Port to bind to
    pub port: u16,
    /// SSH server keys
    pub keys: Vec<russh::keys::PrivateKey>,
    /// Authentication rejection time
    pub auth_rejection_time: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 2222,
            keys: Vec::new(),
            auth_rejection_time: Duration::from_secs(3),
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

    /// Run the server
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut keys = self.config.keys.clone();
        if keys.is_empty() {
            keys.push(russh::keys::PrivateKey::random(
                &mut OsRng,
                russh::keys::Algorithm::Ed25519,
            )?);
        }

        // Determine which auth methods to advertise
        let mut methods = russh::MethodSet::empty();
        if self.auth_config.password_callback.is_some() {
            methods |= russh::MethodSet::PASSWORD;
        }
        if self.auth_config.pubkey_callback.is_some() {
            methods |= russh::MethodSet::PUBLICKEY;
        }
        // Default to password if nothing configured
        if methods.is_empty() {
            methods = russh::MethodSet::PASSWORD;
        }

        let ssh_config = SshConfig {
            auth_rejection_time: self.config.auth_rejection_time,
            auth_rejection_time_initial: Some(Duration::from_secs(0)),
            methods,
            keys,
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
