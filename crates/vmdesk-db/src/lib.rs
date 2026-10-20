//! `SQLite` persistance: connection pool, migrations, inventory and provider sync.

mod agents;
mod error;
mod inventory;
mod known_hosts;
mod notes;
mod search;
mod sessions;
mod status;
mod sync;
mod timeline;

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

pub use agents::{AgentRecord, agent_source};
pub use error::{Error, Result};
pub use inventory::{EndpointInput, ListFilter, NewVm, VmPatch};
pub use notes::{Note, NoteId};
pub use search::SearchHit;
pub use sessions::{CommandRuns, IDLE_GAP, RepoTouched, SessionId, SessionSummary, WorkSession};
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
        let db = Self { pool };
        sessions::backfill(&db).await?;
        Ok(db)
    }

    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// A transaction holding the write lock from its start, for every transaction
    /// here writes. Begun with a plain `BEGIN`, one that reads first fails with
    /// "database is locked", without waiting, when another connection writes between
    /// its read and its write.
    pub(crate) async fn write(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
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
    let vm = tests::add_vm(&db, "box").await;
    (db, vm)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) async fn add_vm(db: &Db, name: &str) -> vmdesk_core::VmId {
        db.create_vm(NewVm {
            name: name.into(),
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
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn migrates_empty_inventory() {
        let db = Db::open_in_memory().await.unwrap();
        assert_eq!(db.vm_count().await.unwrap(), 0);
    }

    /// The app's monitor, poller and window write at once. A transaction that reads
    /// before it writes must wait for the others, not fail with "database is locked".
    #[tokio::test]
    async fn concurrent_writers_wait_for_each_other() {
        use vmdesk_core::{AgentState, SshState, VmStatus};

        let dir = std::env::temp_dir().join(format!("vmdesk-writers-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("vmdesk.db")).await.unwrap();
        let vm = add_vm(&db, "box").await;
        let writers: Vec<_> = (0..8)
            .map(|w| {
                let db = db.clone();
                tokio::spawn(async move {
                    for i in 0..25 {
                        let ssh = if (w + i) % 2 == 0 {
                            SshState::Ok
                        } else {
                            SshState::Timeout
                        };
                        db.save_status(&VmStatus {
                            vm,
                            route: None,
                            ssh,
                            agent: AgentState::Unknown,
                            checked_at: chrono::Utc::now(),
                            detail: None,
                        })
                        .await?;
                        db.set_agent_state(vm, [AgentState::Ok, AgentState::NotRunning][i % 2])
                            .await?;
                        db.add_note(vm, None, "note").await?;
                    }
                    Ok::<_, Error>(())
                })
            })
            .collect();
        for w in writers {
            w.await.unwrap().unwrap();
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
