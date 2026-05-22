//! SFTP server with pluggable backends (local filesystem, S3, memory)

use clap::Parser;
use sftp_s3::{parse_cipher, LocalBackend, MemoryBackend, Server, ServerConfig, AVAILABLE_CIPHERS};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy)]
enum Backend {
    Local,
    #[cfg(feature = "s3")]
    S3,
    Memory,
}

impl std::str::FromStr for Backend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "local" => Ok(Backend::Local),
            #[cfg(feature = "s3")]
            "s3" => Ok(Backend::S3),
            "memory" => Ok(Backend::Memory),
            other => {
                let available = if cfg!(feature = "s3") {
                    "local, s3, memory"
                } else {
                    "local, memory"
                };
                Err(format!(
                    "unknown backend '{other}', choose from: {available}"
                ))
            }
        }
    }
}

#[derive(Parser)]
#[command(name = "sftp-s3")]
#[command(about = "SFTP server with pluggable backends", long_about = None)]
struct Cli {
    /// Backend to use
    #[arg(long, env = "BACKEND")]
    backend: Backend,

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

    /// Path to `authorized_keys` file for public key auth
    #[arg(long, env = "AUTHORIZED_KEYS_FILE")]
    authorized_keys_file: Option<PathBuf>,

    /// Authorized public keys (OpenSSH format, newline-separated)
    #[arg(long, env = "AUTHORIZED_KEYS", hide = true)]
    authorized_keys: Option<String>,

    // Local backend options
    /// Root directory to serve (for local backend)
    #[arg(long, env = "LOCAL_ROOT", default_value = ".")]
    root: PathBuf,

    // S3 backend options
    /// S3 bucket name (for s3 backend)
    #[arg(long, env = "S3_BUCKET")]
    bucket: Option<String>,

    /// S3 key prefix (for s3 backend)
    #[arg(long, env = "S3_PREFIX", default_value = "")]
    prefix: String,

    /// S3 endpoint URL (for S3-compatible services)
    #[arg(long, env = "S3_ENDPOINT")]
    endpoint: Option<String>,

    /// AWS region (for s3 backend)
    #[arg(long, env = "AWS_REGION", default_value = "us-east-1")]
    region: String,

    /// Preferred ciphers (comma-separated, in order of preference)
    /// Available: aes128-gcm, aes256-gcm, aes128-ctr, aes256-ctr, chacha20-poly1305
    #[arg(long, env = "CIPHERS", value_delimiter = ',')]
    ciphers: Option<Vec<String>>,

    /// Enable compression (disabled by default for better throughput)
    #[arg(long, env = "COMPRESSION")]
    compression: bool,
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

/// Expand ~ to home directory
fn expand_tilde(path: &std::path::Path) -> PathBuf {
    if let Some(path_str) = path.to_str() {
        if path_str.starts_with('~') {
            if let Ok(home) = std::env::var("HOME") {
                let expanded = path_str.replacen('~', &home, 1);
                return PathBuf::from(expanded);
            }
        }
    }
    path.to_path_buf()
}

/// Load authorized keys from file or string
fn load_authorized_keys(file: Option<&PathBuf>, data: Option<&str>) -> Vec<russh::keys::PublicKey> {
    let contents = if let Some(path) = file {
        let expanded_path = expand_tilde(path);
        match std::fs::read_to_string(&expanded_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Warning: could not read {}: {}", expanded_path.display(), e);
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
    // Use RUST_LOG=sftp_s3=debug to see operation latencies
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("sftp_s3=info"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .init();

    // Build server config
    let mut config = ServerConfig::new().port(cli.port);

    // Load host key
    if let Some(ref path) = cli.host_key_file {
        let expanded_path = expand_tilde(path);
        config = config.with_key_file(&expanded_path)?;
        eprintln!("Loaded host key from {}", expanded_path.display());
    } else if let Some(ref data) = cli.host_key {
        config = config.with_key_data(data)?;
        eprintln!("Loaded host key from HOST_KEY env var");
    } else {
        config = config.with_generated_key();
        eprintln!("Warning: Using generated host key (clients will see key change warnings)");
        eprintln!("         Set HOST_KEY_FILE or HOST_KEY for persistent keys");
    }

    // Parse ciphers
    if let Some(ref cipher_names) = cli.ciphers {
        let mut ciphers = Vec::new();
        for name in cipher_names {
            if let Some(cipher) = parse_cipher(name) {
                ciphers.push(cipher);
            } else {
                eprintln!(
                    "Unknown cipher '{}'. Available: {}",
                    name,
                    AVAILABLE_CIPHERS.join(", ")
                );
                std::process::exit(1);
            }
        }
        config = config.with_ciphers(ciphers.clone());
        eprintln!("Using ciphers: {cipher_names:?}");
    }

    // Enable compression if requested (disabled by default)
    if cli.compression {
        config = config.with_compression();
        eprintln!("Compression enabled");
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
        Backend::Local => {
            let expanded_root = expand_tilde(&cli.root);
            let root = expanded_root.canonicalize()?;
            eprintln!("Backend: local filesystem at {}", root.display());
            run_server(LocalBackend::new(&root), config, users, authorized_keys).await
        }
        #[cfg(feature = "s3")]
        Backend::S3 => {
            let bucket = cli.bucket.as_ref().ok_or("S3 bucket required (--bucket)")?;
            eprintln!("Backend: S3 bucket '{}' (prefix: '{}')", bucket, cli.prefix);

            let s3_config = sftp_s3::S3Config::new(bucket).with_prefix(&cli.prefix);
            let backend = if let Some(endpoint) = &cli.endpoint {
                eprintln!("Using custom S3 endpoint: {endpoint}");
                sftp_s3::S3Backend::with_endpoint(s3_config, endpoint, &cli.region).await
            } else {
                sftp_s3::S3Backend::from_env(s3_config).await
            };

            run_server(backend, config, users, authorized_keys).await
        }
        Backend::Memory => {
            eprintln!("Backend: in-memory (data will be lost on exit)");
            run_server(MemoryBackend::new(), config, users, authorized_keys).await
        }
    }
}

/// Start the server with the given backend, applying credential configuration.
async fn run_server<B: sftp_s3::Backend>(
    backend: B,
    config: ServerConfig,
    users: Vec<(String, String)>,
    authorized_keys: Vec<russh::keys::PublicKey>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut server = Server::new(backend).config(config);

    if !users.is_empty() {
        server = server.with_users(users);
    }
    if !authorized_keys.is_empty() {
        server =
            server.with_pubkey_auth(move |_user, key| authorized_keys.iter().any(|k| k == key));
    }

    server.run().await
}
