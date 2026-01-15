//! Local filesystem SFTP server example
//!
//! Run with: cargo run --example local_server -- /path/to/serve
//! Connect with: sftp -P 2222 user@localhost

use sftp_s3::{LocalBackend, Server, ServerConfig};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("sftp_s3=debug".parse()?))
        .init();

    // Get root directory from args or use current directory
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());

    let root = std::path::Path::new(&root).canonicalize()?;
    println!("Starting SFTP server serving: {}", root.display());
    println!("Connect with: sftp -P 2222 user@localhost");
    println!("Username: user, Password: pass");

    let backend = LocalBackend::new(&root);
    let config = ServerConfig::new().port(2222).with_generated_key();

    Server::new(backend)
        .config(config)
        .with_users(vec![("user".into(), "pass".into())])
        .run()
        .await
}
