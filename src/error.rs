use thiserror::Error;

/// Top-level error type for the crate
#[derive(Debug, Error)]
pub enum Error {
    #[error("SSH error: {0}")]
    Ssh(#[from] russh::Error),

    #[error("SFTP error: {0}")]
    Sftp(String),

    #[error("Backend error: {0}")]
    Backend(#[from] crate::backend::BackendError),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Key error: {0}")]
    Key(#[from] russh_keys::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
