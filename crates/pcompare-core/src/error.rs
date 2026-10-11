use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid filter pattern `{pattern}`: {message}")]
    BadPattern { pattern: String, message: String },
    /// Failures from remote back ends (network, auth, protocol).
    #[error("{path}: {message}")]
    Remote { path: String, message: String },
    #[error("{0}")]
    Other(String),
}

impl CoreError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        CoreError::Io {
            path: path.into(),
            source,
        }
    }
}

impl CoreError {
    pub fn remote(path: impl Into<String>, message: impl std::fmt::Display) -> Self {
        CoreError::Remote {
            path: path.into(),
            message: message.to_string(),
        }
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;
