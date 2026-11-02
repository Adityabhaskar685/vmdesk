//! Snapshots (zstd-compressed JSON) and the per-VM event timeline.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};
use uuid::Uuid;
use vmdesk_core::VmId;
use vmdesk_proto::{EventKind, Snapshot};

use crate::{Db, Error, Result, sessions};

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

/// Where an event sits in its VM's timeline, which runs in the order of these fields:
/// time, then `seq`, then source. No two events of a VM share one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EventKey {
    pub ts: DateTime<Utc>,
    pub seq: i64,
    pub source: String,
}

impl TimelineEntry {
    #[must_use]
    pub fn key(&self) -> EventKey {
        EventKey {
            ts: self.ts,
            seq: self.seq,
            source: self.source.clone(),
        }
    }
}

/// Which events of a VM a timeline query reads, from the newest down unless `After`.
#[derive(Clone, Copy)]
enum Range<'a> {
    Newest,
    Before(&'a EventKey),
    AtOrBefore(&'a EventKey),
    After(&'a EventKey),
}

/// The event's content for `data_json`; its kind goes in its own column.
pub(crate) fn event_data(kind: &EventKind) -> Result<serde_json::Value> {
    // Serialized as `{"kind": .., "data": {..}}`.
    let mut data = serde_json::to_value(kind)?;
    Ok(data
        .get_mut("data")
        .map_or(serde_json::Value::Null, serde_json::Value::take))
}

/// An event read back from its `kind` and `data_json` columns. `None` for kinds that
/// are not an [`EventKind`], such as the laptop's `status_changed`.
pub(crate) fn parse_event(kind: &str, data_json: &str) -> Option<EventKind> {
    let kind = serde_json::Value::String(kind.to_owned());
    serde_json::from_str(&format!(r#"{{"kind":{kind},"data":{data_json}}}"#)).ok()
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
    /// Stores a snapshot and the events derived from it, updates the work sessions those
    /// events touch, then applies retention.
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

        let mut tx = self.write().await?;
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
            insert_event(&mut tx, vm, source, *ts, kind.name(), &event_data(kind)?).await?;
        }
        if !events.is_empty() {
            let window = sessions::window(events.iter().map(|(t, k)| (t.timestamp_millis(), k)));
            sessions::refresh_in(&mut tx, vm, window).await?;
        }
        prune(&mut tx, vm, taken_at.timestamp_millis()).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn latest_snapshot(&self, vm: VmId) -> Result<Option<Snapshot>> {
        latest_snapshot_in(&mut *self.pool().acquire().await?, vm).await
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
        before: Option<&EventKey>,
    ) -> Result<Vec<TimelineEntry>> {
        let range = before.map_or(Range::Newest, Range::Before);
        self.events(vm, range, limit).await
    }

    /// Newest first: up to `newer` events after `at`, the event at `at`, and up to
    /// `older` events before it.
    pub async fn timeline_around(
        &self,
        vm: VmId,
        at: &EventKey,
        newer: u32,
        older: u32,
    ) -> Result<Vec<TimelineEntry>> {
        let mut events = self.events(vm, Range::After(at), newer).await?;
        let mut rest = self
            .events(vm, Range::AtOrBefore(at), older.saturating_add(1))
            .await?;
        if rest.first().is_some_and(|e| e.key() != *at) {
            rest.truncate(usize::try_from(older).unwrap_or(usize::MAX));
        }
        events.append(&mut rest);
        Ok(events)
    }

    /// Up to `limit` events of `range`, newest first.
    async fn events(&self, vm: VmId, range: Range<'_>, limit: u32) -> Result<Vec<TimelineEntry>> {
        // The plain `ts` bound lets the (vm_id, ts) index start at the key.
        let (bound, order, key) = match range {
            Range::Newest => ("", "DESC", None),
            Range::Before(k) => (
                "AND ts <= ?3 AND (ts, seq, source) < (?3, ?4, ?5)",
                "DESC",
                Some(k),
            ),
            Range::AtOrBefore(k) => (
                "AND ts <= ?3 AND (ts, seq, source) <= (?3, ?4, ?5)",
                "DESC",
                Some(k),
            ),
            Range::After(k) => (
                "AND ts >= ?3 AND (ts, seq, source) > (?3, ?4, ?5)",
                "ASC",
                Some(k),
            ),
        };
        let sql = format!(
            "SELECT source, seq, ts, kind, data_json FROM events WHERE vm_id = ?1 {bound} \
            ORDER BY ts {order}, seq {order}, source {order} LIMIT ?2"
        );
        let mut query = sqlx::query(&sql)
            .bind(vm.to_string())
            .bind(i64::from(limit));
        if let Some(k) = key {
            query = query
                .bind(k.ts.timestamp_millis())
                .bind(k.seq)
                .bind(&k.source);
        }
        let rows = query.fetch_all(self.pool()).await?;
        let mut events = rows
            .iter()
            .map(entry_from_row)
            .collect::<Result<Vec<_>>>()?;
        if matches!(range, Range::After(_)) {
            events.reverse();
        }
        Ok(events)
    }
}

pub(crate) async fn latest_snapshot_in(
    conn: &mut SqliteConnection,
    vm: VmId,
) -> Result<Option<Snapshot>> {
    let blob: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT data_zstd FROM snapshots WHERE vm_id = ? ORDER BY taken_at DESC LIMIT 1",
    )
    .bind(vm.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    blob.map(|b| {
        let json = zstd::decode_all(b.as_slice())
            .map_err(|e| Error::Corrupt(format!("decompressing snapshot: {e}")))?;
        Ok(serde_json::from_slice(&json)?)
    })
    .transpose()
}

/// An `events` row with the columns `source, seq, ts, kind, data_json`.
pub(crate) fn entry_from_row(r: &SqliteRow) -> Result<TimelineEntry> {
    let ms: i64 = r.try_get("ts")?;
    Ok(TimelineEntry {
        source: r.try_get("source")?,
        seq: r.try_get("seq")?,
        ts: DateTime::from_timestamp_millis(ms)
            .ok_or_else(|| Error::Corrupt(format!("bad time {ms}")))?,
        kind: r.try_get("kind")?,
        data: serde_json::from_str(&r.try_get::<String, _>("data_json")?)?,
    })
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
        let older = db.timeline(vm, 10, Some(&tl[0].key())).await.unwrap();
        assert_eq!(older.len(), 1);
    }

    #[tokio::test]
    async fn around_an_event_reads_both_sides_in_timeline_order() {
        use chrono::TimeZone;

        let (db, vm) = db_with_vm().await;
        let t0 = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        let minute = |m: i64| t0 + Duration::minutes(m);
        let mut conn = db.pool().acquire().await.unwrap();
        let mut add = async |source: &str, ts, name: &str| {
            let data = serde_json::json!({ "name": name });
            insert_event(&mut conn, vm, source, ts, "tmux_attached", &data)
                .await
                .unwrap();
        };
        for m in 0..10 {
            add("ssh", minute(m), &format!("s{m}")).await;
        }
        // Same time as s4, lower seq; same time and seq as s0, another source.
        add("laptop", minute(4), "beside-s4").await;
        add("agent:01a1", minute(0), "beside-s0").await;
        drop(conn);

        let names = |events: Vec<TimelineEntry>| -> Vec<String> {
            events
                .iter()
                .map(|e| e.data["name"].as_str().unwrap().to_owned())
                .collect()
        };
        let all = db.timeline(vm, 100, None).await.unwrap();
        assert_eq!(
            names(all.clone()),
            [
                "s9",
                "s8",
                "s7",
                "s6",
                "s5",
                "s4",
                "beside-s4",
                "s3",
                "s2",
                "s1",
                "s0",
                "beside-s0"
            ]
        );
        let key = |name: &str| all.iter().find(|e| e.data["name"] == name).unwrap().key();

        let around = |at: EventKey, newer, older| {
            let db = db.clone();
            async move { names(db.timeline_around(vm, &at, newer, older).await.unwrap()) }
        };
        assert_eq!(
            around(key("s4"), 2, 2).await,
            ["s6", "s5", "s4", "beside-s4", "s3"]
        );
        assert_eq!(around(key("s9"), 3, 1).await, ["s9", "s8"], "nothing newer");
        assert_eq!(
            around(key("beside-s0"), 1, 5).await,
            ["s0", "beside-s0"],
            "nothing older"
        );
        let gone = EventKey {
            ts: minute(4) + Duration::seconds(30),
            seq: 99,
            source: "ssh".into(),
        };
        assert_eq!(
            around(gone, 1, 1).await,
            ["s5", "s4"],
            "a deleted event still has neighbours"
        );
        assert_eq!(
            names(db.timeline(vm, 2, Some(&key("s4"))).await.unwrap()),
            ["beside-s4", "s3"],
            "older than an event includes one at the same time"
        );
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
