//! `events.db`: the event log and the latest snapshot.
//!
//! Events are only ever removed from the oldest end, so the log is always the contiguous
//! range `oldest..=latest` and the laptop can tell rotation from loss by `oldest_seq`.

use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value;
use sqlx::SqliteConnection;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use vmdesk_proto::{Event, EventKind, Snapshot};

/// `AUTOINCREMENT` keeps a seq from being reused even after the newest rows are deleted.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    kind TEXT NOT NULL,
    data_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS latest (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    taken_at INTEGER NOT NULL,
    snapshot_json TEXT NOT NULL
);";

/// Events dropped per step when the database is over its size cap.
const PRUNE_CHUNK: i64 = 256;

#[derive(Debug, Clone, Copy)]
pub struct Retention {
    pub max_age: TimeDelta,
    /// Bytes in use by the database. Deleted rows free pages that later inserts reuse, so
    /// the file itself levels off at about this size without a VACUUM.
    pub max_bytes: i64,
}

impl Retention {
    pub const DEFAULT: Self = Self {
        max_age: TimeDelta::days(90),
        max_bytes: 50 << 20,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeqRange {
    /// Smallest stored seq; `latest + 1` when the log is empty.
    pub oldest: u64,
    /// Largest seq ever assigned; 0 before the first event.
    pub latest: u64,
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    retention: Retention,
}

impl Store {
    pub async fn open(path: &Path, retention: Retention) -> Result<Self> {
        // Created here so it starts private; SQLite gives its -wal and -shm files the
        // mode of the database file.
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        Ok(Self { pool, retention })
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub async fn latest(&self) -> Result<Option<Snapshot>> {
        let json: Option<String> =
            sqlx::query_scalar("SELECT snapshot_json FROM latest WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
    }

    pub async fn range(&self) -> Result<SeqRange> {
        let (latest, oldest): (i64, Option<i64>) = sqlx::query_as(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'events'), 0), \
            (SELECT MIN(seq) FROM events)",
        )
        .fetch_one(&self.pool)
        .await?;
        let latest = latest.cast_unsigned();
        Ok(SeqRange {
            oldest: oldest.map_or(latest + 1, i64::cast_unsigned),
            latest,
        })
    }

    /// Appends `events`, replaces the latest snapshot and applies retention in one
    /// transaction: the stored snapshot is always the one the stored events lead up to, so
    /// a restarted daemon neither repeats nor loses events.
    pub async fn record(
        &self,
        snapshot: &Snapshot,
        events: &[(DateTime<Utc>, EventKind)],
    ) -> Result<()> {
        let taken_at = snapshot.taken_at.unwrap_or_else(Utc::now);
        let mut tx = self.pool.begin().await?;
        for (ts, kind) in events {
            let mut json = serde_json::to_value(kind)?;
            // `{"kind": .., "data": {..}}`: the kind goes in its own column.
            let data = json.get_mut("data").map_or(Value::Null, Value::take);
            sqlx::query("INSERT INTO events (ts, kind, data_json) VALUES (?, ?, ?)")
                .bind(ts.timestamp_millis())
                .bind(kind.name())
                .bind(data.to_string())
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("INSERT OR REPLACE INTO latest (id, taken_at, snapshot_json) VALUES (1, ?, ?)")
            .bind(taken_at.timestamp_millis())
            .bind(serde_json::to_string(snapshot)?)
            .execute(&mut *tx)
            .await?;
        self.prune(&mut tx, taken_at).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn events_after(&self, since: u64, limit: u32) -> Result<Vec<Event>> {
        let rows: Vec<(i64, i64, String, String)> = sqlx::query_as(
            "SELECT seq, ts, kind, data_json FROM events WHERE seq > ? ORDER BY seq LIMIT ?",
        )
        .bind(i64::try_from(since).unwrap_or(i64::MAX))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(seq, ts, kind, data)| {
                let data: Value = serde_json::from_str(&data)?;
                Ok(Event {
                    seq: seq.cast_unsigned(),
                    ts: DateTime::from_timestamp_millis(ts)
                        .with_context(|| format!("event {seq}: bad time {ts}"))?,
                    kind: serde_json::from_value(serde_json::json!({"kind": kind, "data": data}))
                        .with_context(|| format!("event {seq}: unknown {kind}"))?,
                })
            })
            .collect()
    }

    async fn prune(&self, conn: &mut SqliteConnection, now: DateTime<Utc>) -> Result<()> {
        // Everything before the first event young enough to keep.
        let cutoff = (now - self.retention.max_age).timestamp_millis();
        sqlx::query(
            "DELETE FROM events WHERE seq < COALESCE(\
            (SELECT seq FROM events WHERE ts >= ? ORDER BY seq LIMIT 1), \
            (SELECT MAX(seq) FROM events) + 1)",
        )
        .bind(cutoff)
        .execute(&mut *conn)
        .await?;
        loop {
            let used: i64 = sqlx::query_scalar(
                "SELECT (page_count - freelist_count) * page_size \
                FROM pragma_page_count(), pragma_freelist_count(), pragma_page_size()",
            )
            .fetch_one(&mut *conn)
            .await?;
            if used <= self.retention.max_bytes {
                return Ok(());
            }
            let deleted =
                sqlx::query("DELETE FROM events WHERE seq < (SELECT MIN(seq) FROM events) + ?")
                    .bind(PRUNE_CHUNK)
                    .execute(&mut *conn)
                    .await?
                    .rows_affected();
            if deleted == 0 {
                return Ok(()); // only the snapshot is left
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use vmdesk_proto::SystemInfo;

    use super::*;
    use crate::home::TempHome;

    pub fn snap(hostname: &str, at: DateTime<Utc>) -> Snapshot {
        Snapshot {
            taken_at: Some(at),
            system: SystemInfo {
                hostname: hostname.into(),
                ..SystemInfo::default()
            },
            ..Snapshot::default()
        }
    }

    pub fn tmux(name: &str) -> EventKind {
        EventKind::TmuxClosed { name: name.into() }
    }

    async fn seqs(store: &Store) -> Vec<u64> {
        let events = store.events_after(0, 10_000).await.unwrap();
        events.iter().map(|e| e.seq).collect()
    }

    #[tokio::test]
    async fn seqs_grow_across_rotation_and_reopen() {
        let tmp = TempHome::new();
        let path = tmp.home().events_db();
        let store = Store::open(&path, Retention::DEFAULT).await.unwrap();
        assert_eq!(
            store.range().await.unwrap(),
            SeqRange {
                oldest: 1,
                latest: 0
            }
        );
        assert_eq!(store.latest().await.unwrap(), None);

        let t0 = Utc::now() - TimeDelta::days(100);
        let first = snap("a", t0);
        store
            .record(&first, &[(t0, tmux("x")), (t0, tmux("y"))])
            .await
            .unwrap();
        assert_eq!(store.latest().await.unwrap(), Some(first));
        let events = store.events_after(0, 10).await.unwrap();
        assert_eq!(
            events.iter().map(|e| (e.seq, &e.kind)).collect::<Vec<_>>(),
            [(1, &tmux("x")), (2, &tmux("y"))],
            "kind and data round-trip"
        );
        assert_eq!(seqs(&store).await, [1, 2]);

        // 100 days later: both are past 90 days and rotate out, the new one stays.
        let now = Utc::now();
        store
            .record(&snap("b", now), &[(now, tmux("z"))])
            .await
            .unwrap();
        assert_eq!(seqs(&store).await, [3]);
        assert_eq!(
            store.range().await.unwrap(),
            SeqRange {
                oldest: 3,
                latest: 3
            }
        );

        // Everything rotated out: the range is empty, and the next seq is still 4.
        store
            .record(&snap("c", now + TimeDelta::days(91)), &[])
            .await
            .unwrap();
        assert_eq!(
            store.range().await.unwrap(),
            SeqRange {
                oldest: 4,
                latest: 3
            }
        );
        store.close().await;
        let store = Store::open(&path, Retention::DEFAULT).await.unwrap();
        store
            .record(
                &snap("d", now + TimeDelta::days(91)),
                &[(now + TimeDelta::days(91), tmux("w"))],
            )
            .await
            .unwrap();
        assert_eq!(seqs(&store).await, [4], "seq not reused after reopening");
        assert_eq!(store.latest().await.unwrap().unwrap().system.hostname, "d");
        assert_eq!(store.events_after(4, 10).await.unwrap(), []);
    }

    #[tokio::test]
    async fn size_cap_drops_oldest_and_file_levels_off() {
        let tmp = TempHome::new();
        let path = tmp.home().events_db();
        let cap = 256 << 10;
        let store = Store::open(
            &path,
            Retention {
                max_bytes: cap,
                ..Retention::DEFAULT
            },
        )
        .await
        .unwrap();
        let now = Utc::now();
        let padding = "p".repeat(300);
        for tick in 0..40 {
            let events: Vec<_> = (0..50)
                .map(|i| (now, tmux(&format!("{tick}-{i}-{padding}"))))
                .collect();
            store.record(&snap("a", now), &events).await.unwrap();
        }
        let range = store.range().await.unwrap();
        assert_eq!(range.latest, 2000);
        assert!(range.oldest > 1, "oldest events dropped: {range:?}");
        let kept = seqs(&store).await;
        assert!(!kept.is_empty(), "the newest events are kept");
        assert_eq!(
            kept,
            (range.oldest..=range.latest).collect::<Vec<_>>(),
            "contiguous"
        );
        store.close().await;
        let size = i64::try_from(fs::metadata(&path).unwrap().len()).unwrap();
        assert!(
            size < cap + (64 << 10),
            "{size} bytes on disk for a {cap} byte cap"
        );
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
