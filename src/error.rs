#[derive(Debug, thiserror::Error)]
pub enum KvError {
    #[error("key not found")]
    KeyNotFound,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt log record: {0}")]
    Corrupt(String),
    #[error("unsupported log format: {0}")]
    UnsupportedFormat(String),
    #[error("checksum mismatch")]
    ChecksumMismatch,
    #[error("record too large: {0} bytes")]
    RecordTooLarge(usize),
    #[error("directory is locked by another store")]
    Locked,
    #[error("store is poisoned after a failed write")]
    Poisoned,
}

pub type Result<T> = std::result::Result<T, KvError>;
