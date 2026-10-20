//! Event search across every VM (spec section 13): "which VM was I running the UPF
//! tests on last week?". Events are indexed by migration 0006 and kept in sync by
//! triggers on `events`.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::Row;
use vmdesk_core::VmId;

use crate::inventory::parse_id;
use crate::timeline::{TimelineEntry, entry_from_row};
use crate::{Db, Error, Result};

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub vm: VmId,
    pub vm_name: String,
    #[serde(flatten)]
    pub event: TimelineEntry,
    /// [`TimelineEntry::summary`].
    pub summary: String,
}

/// An FTS5 query matching events that contain every word of `input`, each as the
/// start of a word: `upf-tests` finds `pytest upf-tests/` and `http.ser` finds
/// `python3 -m http.server`. Each word is quoted, so nothing in `input` is FTS5
/// syntax. `None` when no word has a letter or digit to match.
pub(crate) fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .map(|w| format!("\"{}\"*", w.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

impl Db {
    /// Events on any VM matching every word of `query`, newest first, at most `limit`,
    /// only those at or after `since` when given.
    pub async fn search(
        &self,
        query: &str,
        since: Option<DateTime<Utc>>,
        limit: u32,
    ) -> Result<Vec<SearchHit>> {
        let fts = fts_query(query)
            .ok_or_else(|| Error::Invalid("nothing to search for: no letters or digits".into()))?;
        let rows = sqlx::query(
            "SELECT e.vm_id, v.name AS vm_name, e.source, e.seq, e.ts, e.kind, e.data_json \
            FROM events_fts f JOIN events e ON e.rowid = f.rowid JOIN vms v ON v.id = e.vm_id \
            WHERE events_fts MATCH ?1 AND (?2 IS NULL OR e.ts >= ?2) \
            ORDER BY e.ts DESC, e.seq DESC LIMIT ?3",
        )
        .bind(fts)
        .bind(since.map(|t| t.timestamp_millis()))
        .bind(i64::from(limit))
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|r| {
                let event = entry_from_row(r)?;
                Ok(SearchHit {
                    vm: parse_id(&r.try_get::<String, _>("vm_id")?)?,
                    vm_name: r.try_get("vm_name")?,
                    summary: event.summary(),
                    event,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use vmdesk_proto::{EventKind, Snapshot};

    use super::*;
    use crate::db_with_vm;
    use crate::tests::add_vm;

    fn started(cmdline: &str, cwd: &str) -> EventKind {
        EventKind::ProcessStarted {
            pid: 1,
            cmdline: cmdline.into(),
            cwd: Some(cwd.into()),
        }
    }

    async fn record(db: &Db, vm: VmId, at: DateTime<Utc>, kind: EventKind) {
        let snap = Snapshot {
            taken_at: Some(at),
            ..Snapshot::default()
        };
        db.record_snapshot(vm, "ssh", &snap, &[(at, kind)])
            .await
            .unwrap();
    }

    async fn found(db: &Db, q: &str) -> Vec<String> {
        db.search(q, None, 50)
            .await
            .unwrap()
            .into_iter()
            .map(|h| format!("{} {}", h.vm_name, h.summary))
            .collect()
    }

    async fn index_ok(db: &Db) {
        sqlx::query("INSERT INTO events_fts (events_fts, rank) VALUES ('integrity-check', 1)")
            .execute(db.pool())
            .await
            .expect("the index matches the events");
    }

    #[test]
    fn queries_are_quoted_words() {
        assert_eq!(fts_query("upf-tests").as_deref(), Some(r#""upf-tests"*"#));
        assert_eq!(
            fts_query(r#" say "hi" NOT  x* "#).as_deref(),
            Some(r#""say"* """hi"""* "NOT"* "x*"*"#)
        );
        assert_eq!(fts_query(r#" " - * "#), None);
    }

    #[tokio::test]
    async fn finds_events_on_any_vm_newest_first() {
        let (db, lab) = db_with_vm().await;
        let other = add_vm(&db, "other").await;
        let t = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        record(
            &db,
            lab,
            t,
            started("pytest upf-tests/test_n4.py", "/root/regal"),
        )
        .await;
        record(
            &db,
            other,
            t + Duration::hours(1),
            started("python3 -m http.server 8080", "/tmp"),
        )
        .await;
        record(
            &db,
            other,
            t + Duration::hours(2),
            EventKind::GitCommitted {
                repo: "/root/regal".into(),
                branch: Some("group2-fix".into()),
                sha: "abc".into(),
                subject: "Fix the UPF teardown".into(),
            },
        )
        .await;

        assert_eq!(
            found(&db, "upf").await,
            [
                "other /root/regal on group2-fix: Fix the UPF teardown",
                "box pytest upf-tests/test_n4.py  (in /root/regal)",
            ]
        );
        assert_eq!(found(&db, "upf-tests").await.len(), 1);
        assert_eq!(found(&db, "http.server").await.len(), 1);
        assert_eq!(found(&db, "http.ser").await.len(), 1, "a word prefix");
        assert_eq!(found(&db, "regal group2").await.len(), 1, "every word");
        assert!(
            found(&db, r#"pytest" OR "x"#).await.is_empty(),
            "no FTS syntax"
        );
        assert!(found(&db, "teardown NEAR fix").await.is_empty());
        assert!(matches!(
            db.search(" -- ", None, 5).await,
            Err(Error::Invalid(_))
        ));

        let recent = db
            .search("regal", Some(t + Duration::minutes(30)), 50)
            .await
            .unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(
            (recent[0].vm, recent[0].event.kind.as_str()),
            (other, "git_committed")
        );
    }

    #[tokio::test]
    async fn the_index_follows_inserts_and_deletes() {
        let (db, vm) = db_with_vm().await;
        let other = add_vm(&db, "other").await;
        let t = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        record(&db, vm, t, started("pytest tests/group2", "/r")).await;
        record(&db, other, t, started("pytest tests/group5", "/r")).await;
        db.insert_agent_events(
            vm,
            uuid::Uuid::now_v7(),
            &[vmdesk_proto::Event {
                seq: 1,
                ts: t,
                kind: started("pytest tests/agent", "/r"),
            }],
        )
        .await
        .unwrap();
        db.set_agent_state(vm, vmdesk_core::AgentState::Ok)
            .await
            .unwrap();
        assert_eq!(found(&db, "pytest").await.len(), 3);
        index_ok(&db).await;

        db.delete_vm(vm).await.unwrap();
        assert_eq!(
            found(&db, "pytest").await,
            ["other pytest tests/group5  (in /r)"]
        );
        index_ok(&db).await;
    }

    #[tokio::test]
    async fn the_migration_indexes_existing_events() {
        let dir = std::env::temp_dir().join(format!("vmdesk-search-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vmdesk.db");
        {
            let pool = sqlx::SqlitePool::connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
            let mut migrator = sqlx::migrate!("../../migrations");
            migrator.migrations = std::borrow::Cow::Owned(
                migrator
                    .migrations
                    .iter()
                    .filter(|m| m.version < 6)
                    .cloned()
                    .collect(),
            );
            migrator.run(&pool).await.unwrap();
            sqlx::query(
                "INSERT INTO vms (id, provider_id, name, username, credential_ref, created_at, \
                updated_at) VALUES ('01a13ec9-3d0b-7088-9c63-cd603a3dc958', 'static', 'dev93', \
                'root', '{\"type\":\"ssh_agent\"}', 0, 0)",
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO events (vm_id, source, seq, ts, kind, data_json) VALUES \
                ('01a13ec9-3d0b-7088-9c63-cd603a3dc958', 'ssh', 1, 1800000000000, \
                'process_started', '{\"pid\":4,\"cmdline\":\"pytest upf-tests\",\"cwd\":\"/r\"}')",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }
        let db = Db::open(&path).await.unwrap();
        assert_eq!(
            found(&db, "upf-tests").await,
            ["dev93 pytest upf-tests  (in /r)"]
        );
        let vm: VmId = "01a13ec9-3d0b-7088-9c63-cd603a3dc958".parse().unwrap();
        assert_eq!(
            db.sessions(vm, 5).await.unwrap().len(),
            1,
            "and derives sessions"
        );
        index_ok(&db).await;
        std::fs::remove_dir_all(dir).ok();
    }
}
