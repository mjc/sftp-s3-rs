//! Simple SFTP server using in-memory backend
//!
//! Run with: cargo run --example memory_server
//!
//! Environment variables:
//!   HOST_KEY      - Server private key (PEM/OpenSSH format)
//!   HOST_KEY_FILE - Path to server private key file
//!   PORT          - Port to listen on (default: 2223)
//!
//! Connect with: sftp -P 2223 user@localhost

use sftp_s3::{MemoryBackend, Server, ServerConfig};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("sftp_s3=debug".parse()?))
        .init();

    let backend = MemoryBackend::new();
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(2223);
    let config = ServerConfig::new().port(port).with_key_from_env()?;

    println!("Starting SFTP server on port {}", port);
    println!("Connect with: sftp -P {} user@localhost", port);
    println!("Username: user, Password: pass");

    Server::new(backend)
        .config(config)
        .with_users(vec![("user".into(), "pass".into())])
        .run()
        .await
}
