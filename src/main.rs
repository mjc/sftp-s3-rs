//! SFTP server with pluggable backends (local filesystem, S3, memory)

use clap::{Parser, Subcommand};
use sftp_s3::{LocalBackend, MemoryBackend, Server, ServerConfig};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "sftp-s3")]
#[command(about = "SFTP server with pluggable backends", long_about = None)]
struct Cli {
    /// Port to listen on
    #[arg(short, long, env = "PORT", default_value = "2222")]
    port: u16,

    /// Path to host key file (OpenSSH format)
    #[arg(long, env = "HOST_KEY_FILE")]
    host_key_file: Option<PathBuf>,

    /// Host key data (PEM/OpenSSH format, alternative to --host-key-file)
    #[arg(long, env = "HOST_KEY", hide = true)]
    host_key: Option<String>,

    /// User credentials (user:password format, can be repeated)
    #[arg(short, long = "user", env = "SFTP_USERS", value_delimiter = ',')]
    users: Vec<String>,

    /// Path to authorized_keys file for public key auth
    #[arg(long, env = "AUTHORIZED_KEYS_FILE")]
    authorized_keys_file: Option<PathBuf>,

    /// Authorized public keys (OpenSSH format, newline-separated)
    #[arg(long, env = "AUTHORIZED_KEYS", hide = true)]
    authorized_keys: Option<String>,

    #[command(subcommand)]
    backend: BackendCommand,
}

#[derive(Subcommand)]
enum BackendCommand {
    /// Serve files from local filesystem
    Local {
        /// Root directory to serve
        #[arg(default_value = ".")]
        root: PathBuf,
    },
    /// Serve files from S3 bucket
    #[cfg(feature = "s3")]
    S3 {
        /// S3 bucket name
        #[arg(env = "S3_BUCKET")]
        bucket: String,

        /// S3 key prefix (optional)
        #[arg(long, env = "S3_PREFIX", default_value = "")]
        prefix: String,

        /// S3 endpoint URL (for S3-compatible services)
        #[arg(long, env = "S3_ENDPOINT")]
        endpoint: Option<String>,

        /// AWS region
        #[arg(long, env = "AWS_REGION", default_value = "us-east-1")]
        region: String,
    },
    /// Use in-memory storage (for testing)
    Memory,
}

/// Parse an OpenSSH public key line
fn parse_pubkey(line: &str) -> Option<russh::keys::PublicKey> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    russh::keys::parse_public_key_base64(parts[1]).ok()
}

/// Load authorized keys from file or string
fn load_authorized_keys(file: Option<&PathBuf>, data: Option<&str>) -> Vec<russh::keys::PublicKey> {
    let contents = if let Some(path) = file {
        match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Warning: could not read {}: {}", path.display(), e);
                return Vec::new();
            }
        }
    } else if let Some(data) = data {
        data.to_string()
    } else {
        return Vec::new();
    };

    contents.lines().filter_map(parse_pubkey).collect()
}

/// Parse user:password credentials
fn parse_users(users: &[String]) -> Vec<(String, String)> {
    users
        .iter()
        .filter_map(|s| {
            let mut parts = s.splitn(2, ':');
            let user = parts.next()?.to_string();
            let pass = parts.next()?.to_string();
            Some((user, pass))
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();

    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("sftp_s3=info".parse().unwrap()),
        )
        .init();

    // Build server config
    let mut config = ServerConfig::new().port(cli.port);

    // Load host key
    if let Some(ref path) = cli.host_key_file {
        config = config.with_key_file(path)?;
        eprintln!("Loaded host key from {}", path.display());
    } else if let Some(ref data) = cli.host_key {
        config = config.with_key_data(data)?;
        eprintln!("Loaded host key from HOST_KEY env var");
    } else {
        config = config.with_generated_key();
        eprintln!("Warning: Using generated host key (clients will see key change warnings)");
        eprintln!("         Set HOST_KEY_FILE or HOST_KEY for persistent keys");
    }

    // Parse credentials
    let users = parse_users(&cli.users);
    let authorized_keys = load_authorized_keys(
        cli.authorized_keys_file.as_ref(),
        cli.authorized_keys.as_deref(),
    );

    if users.is_empty() && authorized_keys.is_empty() {
        eprintln!("Warning: No authentication configured!");
        eprintln!("         Use --user user:pass or --authorized-keys-file path");
    }

    if !authorized_keys.is_empty() {
        eprintln!("Loaded {} authorized public key(s)", authorized_keys.len());
    }

    eprintln!("Starting SFTP server on port {}", cli.port);

    // Run with appropriate backend
    match cli.backend {
        BackendCommand::Local { root } => {
            let root = root.canonicalize()?;
            eprintln!("Backend: local filesystem at {}", root.display());

            let mut server = Server::new(LocalBackend::new(&root)).config(config);

            if !users.is_empty() {
                server = server.with_users(users);
            }
            if !authorized_keys.is_empty() {
                server = server
                    .with_pubkey_auth(move |_user, key| authorized_keys.iter().any(|k| k == key));
            }

            server.run().await
        }
        #[cfg(feature = "s3")]
        BackendCommand::S3 {
            bucket,
            prefix,
            endpoint,
            region,
        } => {
            eprintln!("Backend: S3 bucket '{}' (prefix: '{}')", bucket, prefix);

            let s3_config = sftp_s3::S3Config::new(&bucket).with_prefix(&prefix);
            let backend = if let Some(endpoint) = endpoint {
                eprintln!("Using custom S3 endpoint: {}", endpoint);
                sftp_s3::S3Backend::with_endpoint(s3_config, &endpoint, &region).await
            } else {
                sftp_s3::S3Backend::from_env(s3_config).await
            };

            let mut server = Server::new(backend).config(config);

            if !users.is_empty() {
                server = server.with_users(users);
            }
            if !authorized_keys.is_empty() {
                server = server
                    .with_pubkey_auth(move |_user, key| authorized_keys.iter().any(|k| k == key));
            }

            server.run().await
        }
        BackendCommand::Memory => {
            eprintln!("Backend: in-memory (data will be lost on exit)");

            let mut server = Server::new(MemoryBackend::new()).config(config);

            if !users.is_empty() {
                server = server.with_users(users);
            }
            if !authorized_keys.is_empty() {
                server = server
                    .with_pubkey_auth(move |_user, key| authorized_keys.iter().any(|k| k == key));
            }

            server.run().await
        }
    }
}
