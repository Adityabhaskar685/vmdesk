//! Snapshots (zstd-compressed JSON) and the per-VM event timeline.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Row, SqliteConnection};
use uuid::Uuid;
use vmdesk_core::VmId;
use vmdesk_proto::{EventKind, Snapshot};

use crate::{Db, Error, Result};

/// Everything kept for 48 hours, then the newest per hour up to 30 days, then per day.
const KEEP_ALL_MS: i64 = 48 * 3600 * 1000;
const KEEP_HOURLY_MS: i64 = 30 * 24 * 3600 * 1000;
const HOUR_MS: i64 = 3600 * 1000;
const DAY_MS: i64 = 24 * HOUR_MS;
const ZSTD_LEVEL: i32 = 3;

/// One timeline row, whatever produced it (`ssh` collector, `laptop` status checks, ...).
#[derive(Debug, Clone, Serialize)]
pub struct TimelineEntry {
    pub source: String,
    pub seq: i64,
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub data: serde_json::Value,
}

impl TimelineEntry {
    /// One line describing the event, for lists.
    #[must_use]
    pub fn summary(&self) -> String {
        let d = &self.data;
        let s = |k: &str| d.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
        match self.kind.as_str() {
            "process_started" => format!("{}  (in {})", s("cmdline"), s("cwd")),
            "process_exited" => match d.get("duration_secs").and_then(serde_json::Value::as_i64) {
                Some(secs) => format!(
                    "{}  (ran at least {})",
                    s("cmdline"),
                    vmdesk_core::human_duration(secs)
                ),
                None => s("cmdline"),
            },
            "container_started" | "container_stopped" => format!("{} ({})", s("name"), s("image")),
            "tmux_created" | "tmux_closed" | "tmux_attached" => s("name"),
            "git_branch_changed" => format!("{}: {} -> {}", s("repo"), s("from"), s("to")),
            "git_committed" => format!("{} on {}: {}", s("repo"), s("branch"), s("subject")),
            "git_dirty_changed" => format!("{}: {} -> {} modified", s("repo"), d["from"], d["to"]),
            "listener_opened" => format!("{}:{} {}", s("addr"), d["port"], s("process")),
            "listener_closed" => format!("{}:{}", s("addr"), d["port"]),
            "status_changed" => format!(
                "ssh {} -> {} (route {})",
                d["previous"]["ssh"].as_str().unwrap_or("?"),
                s("ssh"),
                s("route")
            ),
            "vm_rebooted" => "rebooted".to_owned(),
            "gap" => format!("not observed since {}", s("from")),
            _ => d.to_string(),
        }
    }
}

/// Appends one event with the next sequence number for `(vm, source)`.
pub(crate) async fn insert_event(
    conn: &mut SqliteConnection,
    vm: VmId,
    source: &str,
    ts: DateTime<Utc>,
    kind: &str,
    data: &serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO events (vm_id, source, seq, ts, kind, data_json) \
        VALUES (?1, ?2, \
        (SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE vm_id = ?1 AND source = ?2), \
        ?3, ?4, ?5)",
    )
    .bind(vm.to_string())
    .bind(source)
    .bind(ts.timestamp_millis())
    .bind(kind)
    .bind(data.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

impl Db {
    /// Stores a snapshot and the events derived from it, then applies retention.
    pub async fn record_snapshot(
        &self,
        vm: VmId,
        source: &str,
        snapshot: &Snapshot,
        events: &[(DateTime<Utc>, EventKind)],
    ) -> Result<()> {
        let taken_at = snapshot.taken_at.unwrap_or_else(Utc::now);
        let json = serde_json::to_vec(snapshot)?;
        let blob = zstd::encode_all(json.as_slice(), ZSTD_LEVEL)
            .map_err(|e| Error::Corrupt(format!("compressing snapshot: {e}")))?;

        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "INSERT INTO snapshots (id, vm_id, taken_at, source, schema_version, data_zstd) \
            VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(vm.to_string())
        .bind(taken_at.timestamp_millis())
        .bind(source)
        .bind(i64::from(snapshot.schema_version))
        .bind(blob)
        .execute(&mut *tx)
        .await?;
        for (ts, kind) in events {
            let mut data = serde_json::to_value(kind)?;
            // `{"kind": .., "data": {..}}`: the kind goes in its own column.
            let data = data
                .get_mut("data")
                .map_or(serde_json::Value::Null, serde_json::Value::take);
            insert_event(&mut tx, vm, source, *ts, kind.name(), &data).await?;
        }
        prune(&mut tx, vm, taken_at.timestamp_millis()).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn latest_snapshot(&self, vm: VmId) -> Result<Option<Snapshot>> {
        let blob: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT data_zstd FROM snapshots WHERE vm_id = ? ORDER BY taken_at DESC LIMIT 1",
        )
        .bind(vm.to_string())
        .fetch_optional(self.pool())
        .await?;
        blob.map(|b| {
            let json = zstd::decode_all(b.as_slice())
                .map_err(|e| Error::Corrupt(format!("decompressing snapshot: {e}")))?;
            Ok(serde_json::from_slice(&json)?)
        })
        .transpose()
    }

    pub async fn snapshot_count(&self, vm: VmId) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM snapshots WHERE vm_id = ?")
                .bind(vm.to_string())
                .fetch_one(self.pool())
                .await?,
        )
    }

    /// Newest first: up to `limit` events, older than `before` when given.
    pub async fn timeline(
        &self,
        vm: VmId,
        limit: u32,
        before: Option<DateTime<Utc>>,
    ) -> Result<Vec<TimelineEntry>> {
        let rows = sqlx::query(
            "SELECT source, seq, ts, kind, data_json FROM events \
            WHERE vm_id = ?1 AND (?2 IS NULL OR ts < ?2) \
            ORDER BY ts DESC, seq DESC LIMIT ?3",
        )
        .bind(vm.to_string())
        .bind(before.map(|b| b.timestamp_millis()))
        .bind(i64::from(limit))
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|r| {
                let ms: i64 = r.try_get("ts")?;
                Ok(TimelineEntry {
                    source: r.try_get("source")?,
                    seq: r.try_get("seq")?,
                    ts: DateTime::from_timestamp_millis(ms)
                        .ok_or_else(|| Error::Corrupt(format!("bad time {ms}")))?,
                    kind: r.try_get("kind")?,
                    data: serde_json::from_str(&r.try_get::<String, _>("data_json")?)?,
                })
            })
            .collect()
    }
}

/// Retention (spec section 9): keeps every snapshot from the last 48 hours, then the
/// newest per hour up to 30 days, then the newest per day. The latest is always kept.
async fn prune(conn: &mut SqliteConnection, vm: VmId, now_ms: i64) -> Result<()> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT id, taken_at FROM snapshots WHERE vm_id = ? ORDER BY taken_at DESC")
            .bind(vm.to_string())
            .fetch_all(&mut *conn)
            .await?;
    let mut seen_buckets = std::collections::HashSet::new();
    let mut doomed = Vec::new();
    for (i, (id, taken)) in rows.iter().enumerate() {
        let age = now_ms - taken;
        if i == 0 || age < KEEP_ALL_MS {
            continue;
        }
        let bucket = if age < KEEP_HOURLY_MS {
            (0, taken.div_euclid(HOUR_MS))
        } else {
            (1, taken.div_euclid(DAY_MS))
        };
        // Rows are newest first, so the first one in each bucket is the one kept.
        if !seen_buckets.insert(bucket) {
            doomed.push(id);
        }
    }
    for id in doomed {
        sqlx::query("DELETE FROM snapshots WHERE id = ?")
            .bind(id)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use vmdesk_core::{CredentialRef, EndpointKind};
    use vmdesk_proto::SystemInfo;

    use super::*;
    use crate::{EndpointInput, NewVm};

    async fn db_with_vm() -> (Db, VmId) {
        let db = Db::open_in_memory().await.unwrap();
        let vm = db
            .create_vm(NewVm {
                name: "box".into(),
                username: "u".into(),
                credential: CredentialRef::SshAgent,
                endpoints: vec![EndpointInput {
                    address: "10.0.0.1".into(),
                    port: 22,
                    kind: EndpointKind::Private,
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

    fn snap(at: DateTime<Utc>, host: &str) -> Snapshot {
        Snapshot {
            taken_at: Some(at),
            system: SystemInfo {
                hostname: host.into(),
                ..SystemInfo::default()
            },
            ..Snapshot::default()
        }
    }

    #[tokio::test]
    async fn snapshots_round_trip_with_events() {
        let (db, vm) = db_with_vm().await;
        assert!(db.latest_snapshot(vm).await.unwrap().is_none());
        let t = Utc::now();
        db.record_snapshot(vm, "ssh", &snap(t - Duration::minutes(15), "old"), &[])
            .await
            .unwrap();
        let events = [
            (
                t - Duration::minutes(5),
                EventKind::TmuxClosed { name: "a".into() },
            ),
            (t, EventKind::TmuxAttached { name: "b".into() }),
        ];
        db.record_snapshot(vm, "ssh", &snap(t, "new"), &events)
            .await
            .unwrap();

        assert_eq!(
            db.latest_snapshot(vm)
                .await
                .unwrap()
                .unwrap()
                .system
                .hostname,
            "new"
        );
        let tl = db.timeline(vm, 10, None).await.unwrap();
        assert_eq!(tl.len(), 2);
        assert_eq!((tl[0].kind.as_str(), tl[0].seq), ("tmux_attached", 2));
        assert_eq!(tl[0].data["name"], "b");
        assert_eq!(tl[1].kind, "tmux_closed");
        let older = db.timeline(vm, 10, Some(tl[0].ts)).await.unwrap();
        assert_eq!(older.len(), 1);
    }

    #[tokio::test]
    async fn retention_thins_old_snapshots() {
        let (db, vm) = db_with_vm().await;
        let now = Utc::now();
        // Every 20 minutes for 3 days, oldest first.
        let mut at = now - Duration::days(3);
        while at <= now {
            db.record_snapshot(vm, "ssh", &snap(at, "h"), &[])
                .await
                .unwrap();
            at += Duration::minutes(20);
        }
        let n = db.snapshot_count(vm).await.unwrap();
        // 48 h at 3 per hour (~145), plus roughly one per hour for the day before (~24).
        assert!((150..=180).contains(&n), "kept {n}");
        assert_eq!(
            db.latest_snapshot(vm)
                .await
                .unwrap()
                .unwrap()
                .taken_at
                .unwrap()
                .timestamp(),
            (at - Duration::minutes(20)).timestamp()
        );
    }
}
