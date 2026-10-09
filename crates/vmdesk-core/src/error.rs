#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid connection path: {0}")]
    InvalidPath(&'static str),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
