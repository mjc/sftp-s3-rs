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
    Key(#[from] russh::keys::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendError;

    #[test]
    fn test_io_error_converts_to_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
        assert!(err.to_string().contains("I/O error"));
    }

    #[test]
    fn test_backend_error_converts_to_error() {
        let be = BackendError::NotFound;
        let err: Error = be.into();
        assert!(matches!(err, Error::Backend(_)));
    }

    #[test]
    fn test_backend_io_error_converts() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let be: BackendError = io_err.into();
        assert!(matches!(be, BackendError::Io(_)));
        assert!(be.to_string().contains("I/O error"));
    }

    #[test]
    fn test_backend_other_error_display() {
        let be = BackendError::Other("custom error".to_string());
        assert!(be.to_string().contains("custom error"));
    }

    #[test]
    fn test_error_display_variants() {
        let e = Error::Config("bad config".to_string());
        assert!(e.to_string().contains("bad config"));

        let e = Error::Sftp("sftp fail".to_string());
        assert!(e.to_string().contains("sftp fail"));
    }
}
