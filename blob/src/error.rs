pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("blob store configuration: {0}")]
    Config(String),
    #[error("volume snapshot {0} is not in the blob store")]
    NotFound(String),
    #[error("chunk {hash} is corrupt or truncated: {reason}")]
    Corrupt { hash: String, reason: String },
    #[error("snapshot manifest: {0}")]
    Manifest(String),
    #[error("blob store: {0}")]
    Store(#[from] object_store::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
