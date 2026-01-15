//! SFTP server with SSH public key authentication
//!
//! Run with: cargo run --example pubkey_server -- ~/.ssh/authorized_keys
//! Or:       AUTHORIZED_KEYS="ssh-ed25519 AAAA... user" cargo run --example pubkey_server
//!
//! Connect with: sftp -P 2224 -i ~/.ssh/id_ed25519 user@localhost

use sftp_s3::{MemoryBackend, Server, ServerConfig};
use std::path::Path;
use tracing_subscriber::EnvFilter;

/// Parse an OpenSSH public key line (e.g., "ssh-ed25519 AAAA... comment")
fn parse_pubkey(line: &str) -> Option<russh::keys::PublicKey> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    // Parse OpenSSH format: "algorithm base64-key [comment]"
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    russh::keys::parse_public_key_base64(parts[1]).ok()
}

/// Load authorized keys from a file
fn load_authorized_keys(path: &Path) -> Vec<russh::keys::PublicKey> {
    match std::fs::read_to_string(path) {
        Ok(contents) => contents.lines().filter_map(parse_pubkey).collect(),
        Err(e) => {
            eprintln!("Warning: could not read {}: {}", path.display(), e);
            Vec::new()
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("sftp_s3=debug".parse()?))
        .init();

    // Load authorized keys from file argument or AUTHORIZED_KEYS env var
    let keys: Vec<russh::keys::PublicKey> = if let Some(path) = std::env::args().nth(1) {
        load_authorized_keys(Path::new(&path))
    } else if let Ok(keys_str) = std::env::var("AUTHORIZED_KEYS") {
        keys_str.lines().filter_map(parse_pubkey).collect()
    } else {
        eprintln!("Usage: pubkey_server <authorized_keys_file>");
        eprintln!("   or: AUTHORIZED_KEYS=\"ssh-ed25519 ...\" pubkey_server");
        std::process::exit(1);
    };

    if keys.is_empty() {
        eprintln!("Error: No valid public keys found");
        std::process::exit(1);
    }

    println!("Loaded {} authorized key(s)", keys.len());
    for key in &keys {
        println!(
            "  - {} {}",
            key.algorithm(),
            key.fingerprint(Default::default())
        );
    }

    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(2224);

    let config = ServerConfig::new().port(port).with_generated_key();

    println!("\nStarting SFTP server on port {}", port);
    println!(
        "Connect with: sftp -P {} -i <private_key> anyuser@localhost",
        port
    );

    // All loaded keys authorize any username
    Server::new(MemoryBackend::new())
        .config(config)
        .with_pubkey_auth(move |_user, key| keys.iter().any(|k| k == key))
        .run()
        .await
}
