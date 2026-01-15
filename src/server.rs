use crate::backend::Backend;
use crate::ssh_handler::{AuthCallback, SshServer};
use russh::keys::ssh_key::rand_core::OsRng;
use russh::server::{Config as SshConfig, Server as _};
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
}

/// SFTP server builder
pub struct Server<B: Backend> {
    backend: Arc<B>,
    config: ServerConfig,
    auth_callback: AuthCallback,
}

impl<B: Backend> Server<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend: Arc::new(backend),
            config: ServerConfig::default(),
            auth_callback: Arc::new(|_, _| false),
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
        self.auth_callback = Arc::new(callback);
        self
    }

    /// Set static users for authentication
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

        let ssh_config = SshConfig {
            auth_rejection_time: self.config.auth_rejection_time,
            auth_rejection_time_initial: Some(Duration::from_secs(0)),
            methods: russh::MethodSet::PASSWORD,
            keys,
            ..Default::default()
        };

        let ssh_config = Arc::new(ssh_config);
        let mut server = SshServer::new(self.backend, self.auth_callback);

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
