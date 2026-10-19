//! The agent on each VM (one row per VM) and the events it reports.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;
use vmdesk_core::VmId;
use vmdesk_proto::{Event, EventKind};

use crate::inventory::{from_ms, parse_id};
use crate::status::LAPTOP;
use crate::timeline::{event_data, insert_event};
use crate::{Db, Error, Result};

/// What the laptop knows about a VM's agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentRecord {
    pub vm: VmId,
    pub agent_id: Uuid,
    pub version: String,
    pub proto: u16,
    pub installed_at: Option<DateTime<Utc>>,
    /// Highest agent event seq stored; the next sync resumes after it.
    pub last_seq: u64,
    pub last_contact_at: Option<DateTime<Utc>>,
}

/// Event source of an agent's events. Each agent id has its own seq space, so a
/// re-imaged VM's new agent never collides with the old one's history.
#[must_use]
pub fn agent_source(agent_id: Uuid) -> String {
    format!("agent:{agent_id}")
}

const COLUMNS: &str = "vm_id, agent_id, version, proto, installed_at, last_seq, last_contact_at";

impl Db {
    pub async fn agent(&self, vm: VmId) -> Result<Option<AgentRecord>> {
        sqlx::query(&format!("SELECT {COLUMNS} FROM agents WHERE vm_id = ?"))
            .bind(vm.to_string())
            .fetch_optional(self.pool())
            .await?
            .map(|r| agent_from_row(&r))
            .transpose()
    }

    pub async fn agents(&self) -> Result<Vec<AgentRecord>> {
        sqlx::query(&format!("SELECT {COLUMNS} FROM agents"))
            .fetch_all(self.pool())
            .await?
            .iter()
            .map(agent_from_row)
            .collect()
    }

    /// Stores `agent` as its VM's agent, replacing any previous one. `gap` (from, to)
    /// is recorded in the same transaction as a `gap` event: the agent rotated out
    /// events from that span before the laptop read them.
    pub async fn save_agent(
        &self,
        agent: &AgentRecord,
        gap: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        sqlx::query(&format!(
            "INSERT INTO agents ({COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?) \
            ON CONFLICT (vm_id) DO UPDATE SET agent_id = excluded.agent_id, \
            version = excluded.version, proto = excluded.proto, \
            installed_at = excluded.installed_at, last_seq = excluded.last_seq, \
            last_contact_at = excluded.last_contact_at"
        ))
        .bind(agent.vm.to_string())
        .bind(agent.agent_id.to_string())
        .bind(&agent.version)
        .bind(i64::from(agent.proto))
        .bind(agent.installed_at.map(|t| t.timestamp_millis()))
        .bind(seq_to_db(agent.last_seq)?)
        .bind(agent.last_contact_at.map(|t| t.timestamp_millis()))
        .execute(&mut *tx)
        .await?;
        if let Some((from, to)) = gap {
            let kind = EventKind::Gap { from, to };
            insert_event(
                &mut tx,
                agent.vm,
                LAPTOP,
                from,
                kind.name(),
                &event_data(&kind)?,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Stores events reported by agent `agent_id` with their own seqs, skipping any
    /// already stored, and advances that agent's `last_seq`. Returns how many were new.
    pub async fn insert_agent_events(
        &self,
        vm: VmId,
        agent_id: Uuid,
        events: &[Event],
    ) -> Result<u64> {
        let source = agent_source(agent_id);
        let mut tx = self.pool().begin().await?;
        let mut added = 0;
        for e in events {
            added += sqlx::query(
                "INSERT OR IGNORE INTO events (vm_id, source, seq, ts, kind, data_json) \
                VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(vm.to_string())
            .bind(&source)
            .bind(seq_to_db(e.seq)?)
            .bind(e.ts.timestamp_millis())
            .bind(e.kind.name())
            .bind(event_data(&e.kind)?.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        if let Some(max) = events.iter().map(|e| e.seq).max() {
            sqlx::query(
                "UPDATE agents SET last_seq = MAX(last_seq, ?) WHERE vm_id = ? AND agent_id = ?",
            )
            .bind(seq_to_db(max)?)
            .bind(vm.to_string())
            .bind(agent_id.to_string())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(added)
    }

    /// Forgets the VM's agent. Its events and snapshots stay in the history.
    pub async fn delete_agent(&self, vm: VmId) -> Result<bool> {
        let done = sqlx::query("DELETE FROM agents WHERE vm_id = ?")
            .bind(vm.to_string())
            .execute(self.pool())
            .await?;
        Ok(done.rows_affected() > 0)
    }
}

fn seq_to_db(seq: u64) -> Result<i64> {
    i64::try_from(seq).map_err(|_| Error::Invalid(format!("event seq {seq} out of range")))
}

fn agent_from_row(row: &SqliteRow) -> Result<AgentRecord> {
    let agent_id: String = row.try_get("agent_id")?;
    let proto: i64 = row.try_get("proto")?;
    let last_seq: i64 = row.try_get("last_seq")?;
    Ok(AgentRecord {
        vm: parse_id(&row.try_get::<String, _>("vm_id")?)?,
        agent_id: agent_id
            .parse()
            .map_err(|_| Error::Corrupt(format!("bad agent id {agent_id:?}")))?,
        version: row.try_get("version")?,
        proto: u16::try_from(proto).map_err(|_| Error::Corrupt(format!("bad proto {proto}")))?,
        installed_at: row
            .try_get::<Option<i64>, _>("installed_at")?
            .map(from_ms)
            .transpose()?,
        last_seq: u64::try_from(last_seq)
            .map_err(|_| Error::Corrupt(format!("bad last_seq {last_seq}")))?,
        last_contact_at: row
            .try_get::<Option<i64>, _>("last_contact_at")?
            .map(from_ms)
            .transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::db_with_vm;

    fn record(vm: VmId, agent_id: Uuid) -> AgentRecord {
        AgentRecord {
            vm,
            agent_id,
            version: "0.1.0".into(),
            proto: 1,
            installed_at: None,
            last_seq: 0,
            last_contact_at: Some(DateTime::from_timestamp_millis(1_700_000_000_456).unwrap()),
        }
    }

    fn event(seq: u64) -> Event {
        Event {
            seq,
            ts: DateTime::from_timestamp(1_700_000_000 + i64::try_from(seq).unwrap(), 0).unwrap(),
            kind: EventKind::TmuxCreated {
                name: format!("s{seq}"),
                pane_cwd: None,
            },
        }
    }

    #[tokio::test]
    async fn one_agent_per_vm_replaced_by_a_new_id() {
        let (db, vm) = db_with_vm().await;
        assert!(db.agent(vm).await.unwrap().is_none());
        let first = record(vm, Uuid::now_v7());
        db.save_agent(&first, None).await.unwrap();
        assert_eq!(db.agent(vm).await.unwrap().as_ref(), Some(&first));

        let second = AgentRecord {
            installed_at: Some(DateTime::from_timestamp_millis(1_700_000_000_123).unwrap()),
            ..record(vm, Uuid::now_v7())
        };
        db.save_agent(&second, None).await.unwrap();
        assert_eq!(db.agents().await.unwrap(), vec![second]);

        let dup = sqlx::query(
            "INSERT INTO agents (vm_id, agent_id, version, proto) VALUES (?, 'x', '0', 1)",
        )
        .bind(vm.to_string())
        .execute(db.pool())
        .await;
        assert!(dup.is_err(), "the migration makes vm_id unique");
    }

    #[tokio::test]
    async fn events_are_stored_once_under_the_agent_source() {
        let (db, vm) = db_with_vm().await;
        let id = Uuid::now_v7();
        db.save_agent(&record(vm, id), None).await.unwrap();

        let batch: Vec<Event> = (1..=3).map(event).collect();
        assert_eq!(db.insert_agent_events(vm, id, &batch).await.unwrap(), 3);
        assert_eq!(db.insert_agent_events(vm, id, &batch).await.unwrap(), 0);
        let overlap: Vec<Event> = (2..=4).map(event).collect();
        assert_eq!(db.insert_agent_events(vm, id, &overlap).await.unwrap(), 1);
        assert_eq!(db.agent(vm).await.unwrap().unwrap().last_seq, 4);

        let tl = db.timeline(vm, 10, None).await.unwrap();
        let seqs: Vec<i64> = tl.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [4, 3, 2, 1]);
        assert!(tl.iter().all(|e| e.source == agent_source(id)));
        assert_eq!(
            (tl[0].kind.as_str(), &tl[0].data["name"]),
            ("tmux_created", &serde_json::json!("s4"))
        );

        // An older batch never moves the cursor back, and another agent's events
        // never move this one's.
        db.insert_agent_events(vm, id, &[event(1)]).await.unwrap();
        db.insert_agent_events(vm, Uuid::now_v7(), &[event(9)])
            .await
            .unwrap();
        assert_eq!(db.agent(vm).await.unwrap().unwrap().last_seq, 4);
    }

    #[tokio::test]
    async fn gap_is_recorded_with_the_agent_and_history_outlives_it() {
        let (db, vm) = db_with_vm().await;
        let id = Uuid::now_v7();
        let to = Utc::now();
        let from = to - Duration::days(3);
        let agent = AgentRecord {
            last_seq: 41,
            ..record(vm, id)
        };
        db.save_agent(&agent, Some((from, to))).await.unwrap();
        db.insert_agent_events(vm, id, &[event(42)]).await.unwrap();

        let tl = db.timeline(vm, 10, None).await.unwrap();
        let gap = tl.iter().find(|e| e.kind == "gap").expect("a gap event");
        assert_eq!(
            (gap.source.as_str(), gap.ts.timestamp_millis()),
            ("laptop", from.timestamp_millis())
        );
        assert_eq!(gap.data["to"], serde_json::to_value(to).unwrap());

        assert!(db.delete_agent(vm).await.unwrap());
        assert!(!db.delete_agent(vm).await.unwrap());
        assert!(db.agent(vm).await.unwrap().is_none());
        assert_eq!(db.timeline(vm, 10, None).await.unwrap().len(), 2);
    }
}
