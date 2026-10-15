#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid connection path: {0}")]
    InvalidPath(&'static str),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("connection refused: {0}")]
    Refused(String),
    #[error("timed out: {0}")]
    Timeout(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("host key for {vm} is not trusted yet: {key_type} {fingerprint}")]
    UnknownHostKey {
        vm: String,
        key_type: String,
        fingerprint: String,
    },
    #[error(
        "host key for {vm} has changed (trusted {expected}, offered {offered}); connection blocked"
    )]
    HostKeyChanged {
        vm: String,
        expected: String,
        offered: String,
    },
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
