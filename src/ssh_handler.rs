use crate::backend::Backend;
use crate::sftp_handler::SftpHandler;
use async_trait::async_trait;
use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info};

/// Authentication callback type
pub type AuthCallback = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

/// SSH server that creates sessions for each connection
pub struct SshServer<B: Backend> {
    backend: Arc<B>,
    auth_callback: AuthCallback,
}

impl<B: Backend> SshServer<B> {
    pub fn new(backend: Arc<B>, auth_callback: AuthCallback) -> Self {
        Self {
            backend,
            auth_callback,
        }
    }
}

impl<B: Backend> Clone for SshServer<B> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            auth_callback: self.auth_callback.clone(),
        }
    }
}

impl<B: Backend> russh::server::Server for SshServer<B> {
    type Handler = SshSession<B>;

    fn new_client(&mut self, addr: Option<std::net::SocketAddr>) -> Self::Handler {
        info!(?addr, "New SSH connection");
        SshSession::new(self.backend.clone(), self.auth_callback.clone())
    }
}

/// Individual SSH session handler
pub struct SshSession<B: Backend> {
    backend: Arc<B>,
    auth_callback: AuthCallback,
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl<B: Backend> SshSession<B> {
    pub fn new(backend: Arc<B>, auth_callback: AuthCallback) -> Self {
        Self {
            backend,
            auth_callback,
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
        debug!(user, password, "Password authentication attempt");
        let result = (self.auth_callback)(user, password);
        debug!(user, password, result, "Auth callback result");
        if result {
            info!(user, "Authentication successful");
            Ok(Auth::Accept)
        } else {
            info!(user, "Authentication failed");
            Ok(Auth::Reject {
                proceed_with_methods: None,
            })
        }
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        _public_key: &russh::keys::PublicKey,
    ) -> Result<Auth, Self::Error> {
        debug!(user, "Public key authentication attempt - rejected");
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
