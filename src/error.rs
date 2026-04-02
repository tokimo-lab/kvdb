use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq)]
pub enum KvdbError {
    // Storage errors
    #[error("disk full")]
    DiskFull,
    #[error("corrupted data")]
    CorruptedData,
    #[error("invalid page id: {0}")]
    InvalidPageId(u64),
    #[error("page not found: {0}")]
    PageNotFound(u64),
    #[error("page overflow")]
    PageOverflow,

    // Transaction errors
    #[error("transaction already active")]
    TransactionAlreadyActive,
    #[error("no active transaction")]
    NoActiveTransaction,
    #[error("transaction conflict")]
    TransactionConflict,

    // B-tree errors
    #[error("key not found")]
    KeyNotFound,
    #[error("key already exists")]
    KeyAlreadyExists,
    #[error("node full")]
    NodeFull,
    #[error("node empty")]
    NodeEmpty,

    // WAL errors
    #[error("WAL corrupted")]
    WalCorrupted,
    #[error("WAL replay failed")]
    WalReplayFailed,

    // I/O errors
    #[error("I/O error: {0}")]
    IoError(String),

    // Other
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("database closed")]
    DatabaseClosed,
}

impl From<std::io::Error> for KvdbError {
    fn from(e: std::io::Error) -> Self {
        KvdbError::IoError(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, KvdbError>;
