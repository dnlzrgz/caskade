#[derive(Debug, thiserror::Error)]
pub enum KvError {
    #[error("key not found")]
    KeyNotFound,
    #[error("io error")]
    Io(#[from] std::io::Error),
    #[error("serialization error")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, KvError>;
