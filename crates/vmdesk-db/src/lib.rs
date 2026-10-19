//! `SQLite` persistance: connection pool, migrations, inventory and provider sync.

mod agents;
mod error;
mod inventory;
mod known_hosts;
mod status;
mod sync;
mod timeline;

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

pub use agents::{AgentRecord, agent_source};
pub use error::{Error, Result};
pub use inventory::{EndpointInput, ListFilter, NewVm, VmPatch};
pub use sync::SyncReport;
pub use timeline::TimelineEntry;

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

/// An in-memory database holding one VM.
#[cfg(test)]
pub(crate) async fn db_with_vm() -> (Db, vmdesk_core::VmId) {
    let db = Db::open_in_memory().await.unwrap();
    let vm = db
        .create_vm(NewVm {
            name: "box".into(),
            username: "u".into(),
            credential: vmdesk_core::CredentialRef::SshAgent,
            endpoints: vec![EndpointInput {
                address: "10.0.0.1".into(),
                port: 22,
                kind: vmdesk_core::EndpointKind::Private,
                bind_interface: None,
            }],
            paths: None,
            tags: vec![],
            favourite: false,
        })
        .await
        .unwrap();
    (db, vm.id)
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
