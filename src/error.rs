#[derive(Debug, thiserror::Error)]
pub enum KvError {
    #[error("key not found")]
    KeyNotFound,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt log record: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, KvError>;
