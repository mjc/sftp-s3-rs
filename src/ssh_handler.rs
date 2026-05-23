use crate::backend::Backend;
use crate::scp_handler::ScpHandler;
use crate::sftp_handler::SftpHandler;
use russh::keys::PublicKey;
use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::SerializedPacket;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

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
}

impl<B: Backend> russh::server::Handler for SshSession<B> {
    type Error = russh::Error;

    fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> impl Future<Output = Result<Auth, Self::Error>> + Send {
        let user = user.to_string();
        let password = password.to_string();
        let callback = self.auth_config.password_callback.clone();

        async move {
            debug!(user = %user, "Password authentication attempt");

            if let Some(ref callback) = callback {
                let result = callback(&user, &password);
                if result {
                    info!(user = %user, "Password authentication successful");
                    return Ok(Auth::Accept);
                }
            }

            info!(user = %user, "Password authentication failed");
            Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &PublicKey,
    ) -> impl Future<Output = Result<Auth, Self::Error>> + Send {
        let user = user.to_string();
        let public_key = public_key.clone();
        let callback = self.auth_config.pubkey_callback.clone();

        async move {
            debug!(user = %user, key_type = ?public_key.algorithm(), "Public key authentication attempt");

            if let Some(ref callback) = callback {
                let result = callback(&user, &public_key);
                if result {
                    info!(user = %user, "Public key authentication successful");
                    return Ok(Auth::Accept);
                }
            }

            info!(user = %user, "Public key authentication failed");
            Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
        let channels = self.channels.clone();
        let channel_id = channel.id();

        async move {
            debug!(channel_id = ?channel_id, "Opening session channel");
            channels.lock().await.insert(channel_id, channel);
            Ok(true)
        }
    }

    fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let name = name.to_string();
        let channels = self.channels.clone();
        let backend = self.backend.clone();

        // Handle session operations synchronously before the async block
        let result = match name.as_str() {
            "sftp" | "scp" => session.channel_success(channel_id),
            _ => session.channel_failure(channel_id),
        };

        async move {
            debug!(channel_id = ?channel_id, name = %name, "Subsystem request");
            result?;

            match name.as_str() {
                "sftp" => {
                    if let Some(channel) = channels.lock().await.remove(&channel_id) {
                        let sftp_handler = SftpHandler::new(backend);
                        let (mut read_half, write_half) = channel.split();
                        let write_half = Arc::new(write_half);
                        tokio::spawn(async move {
                            let mut reader = read_half.make_reader();
                            let mut handler = sftp_handler;
                            let mut send_packet = move |packet: SerializedPacket| {
                                let write_half = Arc::clone(&write_half);
                                async move {
                                    match packet {
                                        SerializedPacket::Contiguous(bytes) => {
                                            write_half.data_bytes(bytes).await
                                        }
                                        SerializedPacket::Split { header, data } => {
                                            write_half.data_bytes(header).await?;
                                            write_half.data_bytes(data).await
                                        }
                                    }
                                }
                            };
                            russh_sftp::server::serve_with_packet_sender(
                                &mut reader,
                                &mut send_packet,
                                &mut handler,
                            )
                            .await;
                        });
                    }
                }
                "scp" => {
                    if let Some(channel) = channels.lock().await.remove(&channel_id) {
                        let mut scp_handler = ScpHandler::new(backend);
                        let stream = channel.into_stream();
                        if let Err(e) = scp_handler.run(stream, "scp -t /").await {
                            debug!(channel_id = ?channel_id, error = %e, "SCP subsystem failed");
                        }
                    }
                }
                _ => {}
            }

            Ok(())
        }
    }

    fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let command = String::from_utf8_lossy(data).to_string();
        let channels = self.channels.clone();
        let backend = self.backend.clone();

        // Handle session operations synchronously
        let is_scp = command.starts_with("scp ");
        let result = if is_scp {
            debug!(channel_id = ?channel_id, command = %command, "Accepting SCP command");
            session.channel_success(channel_id)
        } else {
            warn!(channel_id = ?channel_id, command = %command, "Rejecting non-SCP exec request");
            session.channel_failure(channel_id)
        };

        async move {
            debug!(channel_id = ?channel_id, command = %command, "Exec request processed");
            result?;

            if is_scp {
                if let Some(channel) = channels.lock().await.remove(&channel_id) {
                    let mut scp_handler = ScpHandler::new(backend);
                    let stream = channel.into_stream();
                    tokio::spawn(async move {
                        if let Err(e) = scp_handler.run(stream, &command).await {
                            warn!(error = %e, "SCP command failed");
                        }
                    });
                }
            }

            Ok(())
        }
    }

    fn channel_eof(
        &mut self,
        channel_id: ChannelId,
        session: &mut Session,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let result = session.close(channel_id);

        async move {
            debug!(channel_id = ?channel_id, "Channel EOF");
            result?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sftp_subsystem_uses_owned_bytes_sender_path() {
        let source = include_str!("ssh_handler.rs");
        let production_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("ssh_handler source should contain production code");

        assert!(
            production_source.contains("russh_sftp::server::serve_with_packet_sender"),
            "SFTP subsystem must keep using the russh-sftp packet sender path"
        );
        assert!(
            production_source.contains("SerializedPacket::Split { header, data }"),
            "SFTP subsystem must preserve split DATA packets on the channel writer path"
        );
        assert!(
            production_source.contains("write_half.data_bytes(header).await?"),
            "SFTP response headers must be sent through russh ChannelWriteHalf::data_bytes"
        );
        assert!(
            production_source.contains("write_half.data_bytes(data).await"),
            "SFTP response payloads must be sent through russh ChannelWriteHalf::data_bytes"
        );
        assert!(
            !production_source.contains("russh_sftp::server::run("),
            "SFTP subsystem must not regress to the copying stream-based server::run path"
        );
    }
}
