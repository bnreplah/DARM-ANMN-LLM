use thiserror::Error;

pub type Result<T, E = DarmError> = std::result::Result<T, E>;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DarmError {
    #[error("invalid signature")]
    InvalidSignature,
    #[error("invalid public key")]
    InvalidPublicKey,
    #[error("malformed {what}: {detail}")]
    Malformed { what: &'static str, detail: String },
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    #[error("not authorized: {0}")]
    Unauthorized(String),
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("duplicate")]
    Duplicate,
    #[error("io: {0}")]
    Io(String),
    #[error("randomness unavailable: {0}")]
    Entropy(String),
}

impl DarmError {
    pub fn malformed(what: &'static str, detail: impl Into<String>) -> Self {
        Self::Malformed {
            what,
            detail: detail.into(),
        }
    }
}

impl From<std::io::Error> for DarmError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}
