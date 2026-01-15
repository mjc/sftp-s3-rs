//! SFTP server using S3 backend
//!
//! Run with: cargo run --example s3_server
//!
//! Required environment variables:
//!   AWS_ACCESS_KEY_ID
//!   AWS_SECRET_ACCESS_KEY
//!   AWS_REGION (or AWS_DEFAULT_REGION)
//!   SFTP_BUCKET - S3 bucket name
//!
//! For LocalStack:
//!   AWS_ENDPOINT_URL=http://localhost:4566

use sftp_s3::{S3Backend, S3Config, Server, ServerConfig};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("sftp_s3=debug".parse()?))
        .init();

    let bucket = std::env::var("SFTP_BUCKET").expect("SFTP_BUCKET environment variable required");

    let s3_config = S3Config::new(&bucket);
    let backend = S3Backend::from_env(s3_config).await;

    let config = ServerConfig::new().port(2222).with_generated_key();

    println!("Starting SFTP server on port 2222 with S3 backend");
    println!("Bucket: {}", bucket);
    println!("Connect with: sftp -P 2222 user@localhost");
    println!("Username: user, Password: pass");

    Server::new(backend)
        .config(config)
        .with_users(vec![("user".into(), "pass".into())])
        .run()
        .await
}
