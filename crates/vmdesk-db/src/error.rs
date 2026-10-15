#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Invalid(String),
    #[error("corrupt record: {0}")]
    Corrupt(String),
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Corrupt(e.to_string())
    }
}

impl From<vmdesk_core::Error> for Error {
    fn from(e: vmdesk_core::Error) -> Self {
        Self::Invalid(e.to_string())
    }
}

pub(crate) fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.is_unique_violation())
}

/// Maps a unique-constraint failure to [`Error::Conflict`], anything else to [`Error::Sqlx`].
pub(crate) fn conflict_on_unique(e: sqlx::Error, message: impl FnOnce() -> String) -> Error {
    if is_unique_violation(&e) {
        Error::Conflict(message())
    } else {
        Error::Sqlx(e)
    }
}
