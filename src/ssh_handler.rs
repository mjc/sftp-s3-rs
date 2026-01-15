use crate::backend::Backend;
use crate::sftp_handler::SftpHandler;
use async_trait::async_trait;
use russh::keys::PublicKey;
use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info};

/// Password authentication callback type
pub type PasswordAuthCallback = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

/// Public key authentication callback type
/// Returns true if the given public key is authorized for the user
pub type PubkeyAuthCallback = Arc<dyn Fn(&str, &PublicKey) -> bool + Send + Sync>;

/// Authentication configuration
#[derive(Clone, Default)]
pub struct AuthConfig {
    pub password_callback: Option<PasswordAuthCallback>,
    pub pubkey_callback: Option<PubkeyAuthCallback>,
}

/// SSH server that creates sessions for each connection
pub struct SshServer<B: Backend> {
    backend: Arc<B>,
    auth_config: AuthConfig,
}

impl<B: Backend> SshServer<B> {
    pub fn new(backend: Arc<B>, auth_config: AuthConfig) -> Self {
        Self {
            backend,
            auth_config,
        }
    }
}

impl<B: Backend> Clone for SshServer<B> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            auth_config: self.auth_config.clone(),
        }
    }
}

impl<B: Backend> russh::server::Server for SshServer<B> {
    type Handler = SshSession<B>;

    fn new_client(&mut self, addr: Option<std::net::SocketAddr>) -> Self::Handler {
        info!(?addr, "New SSH connection");
        SshSession::new(self.backend.clone(), self.auth_config.clone())
    }
}

/// Individual SSH session handler
pub struct SshSession<B: Backend> {
    backend: Arc<B>,
    auth_config: AuthConfig,
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl<B: Backend> SshSession<B> {
    pub fn new(backend: Arc<B>, auth_config: AuthConfig) -> Self {
        Self {
            backend,
            auth_config,
            channels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn get_channel(&self, channel_id: ChannelId) -> Option<Channel<Msg>> {
        self.channels.lock().await.remove(&channel_id)
    }
}

#[async_trait]
impl<B: Backend> russh::server::Handler for SshSession<B> {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        debug!(user, "Password authentication attempt");

        if let Some(ref callback) = self.auth_config.password_callback {
            let result = callback(user, password);
            if result {
                info!(user, "Password authentication successful");
                return Ok(Auth::Accept);
            }
        }

        info!(user, "Password authentication failed");
        Ok(Auth::Reject {
            proceed_with_methods: None,
        })
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        debug!(user, key_type = ?public_key.algorithm(), "Public key authentication attempt");

        if let Some(ref callback) = self.auth_config.pubkey_callback {
            let result = callback(user, public_key);
            if result {
                info!(user, "Public key authentication successful");
                return Ok(Auth::Accept);
            }
        }

        info!(user, "Public key authentication failed");
        Ok(Auth::Reject {
            proceed_with_methods: None,
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        debug!(channel_id = ?channel.id(), "Opening session channel");
        self.channels.lock().await.insert(channel.id(), channel);
        Ok(true)
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(channel_id = ?channel_id, name, "Subsystem request");

        if name == "sftp" {
            if let Some(channel) = self.get_channel(channel_id).await {
                let sftp_handler = SftpHandler::new(self.backend.clone());
                session.channel_success(channel_id)?;

                // Run SFTP handler (blocking until session ends)
                russh_sftp::server::run(channel.into_stream(), sftp_handler).await;
            }
        } else {
            session.channel_failure(channel_id)?;
        }

        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel_id: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(channel_id = ?channel_id, "Channel EOF");
        session.close(channel_id)?;
        Ok(())
    }
}
