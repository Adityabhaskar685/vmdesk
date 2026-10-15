//! `SQLite` persistance: connection pool, migrations, inventory and provider sync.

mod error;
mod inventory;
mod known_hosts;
mod sync;

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

pub use error::{Error, Result};
pub use inventory::{EndpointInput, ListFilter, NewVm, VmPatch};
pub use sync::SyncReport;

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    /// Opens (creates if needed) the database file and runs migrations.
    pub async fn open(path: &std::path::Path) -> Result<Self, Error> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        Self::with_options(opts, 4).await
    }

    /// In-memory database, used by tests.
    pub async fn open_in_memory() -> Result<Self, Error> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        Self::with_options(opts, 1).await
    }

    async fn with_options(opts: SqliteConnectOptions, max: u32) -> Result<Self, Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("../../migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn vm_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM vms")
            .fetch_one(&self.pool)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrates_empty_inventory() {
        let db = Db::open_in_memory().await.unwrap();
        assert_eq!(db.vm_count().await.unwrap(), 0);
    }
}
