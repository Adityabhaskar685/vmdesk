//! Latest reachability status per VM. A change in route, SSH or agent state is also
//! appended to the VM's timeline as a `status_changed` event.

use std::collections::HashMap;
use std::str::FromStr;

use chrono::Utc;
use sqlx::sqlite::{SqliteQueryResult, SqliteRow};
use sqlx::{Row, SqliteConnection};
use vmdesk_core::{AgentState, SshState, VmId, VmStatus};

use crate::inventory::parse_id;
use crate::timeline::insert_event;
use crate::{Db, Error, Result};

/// Event source for observations made by the laptop.
pub(crate) const LAPTOP: &str = "laptop";

impl Db {
    /// Stores `status` as the VM's latest route and SSH result. Returns the previous
    /// status if it differed. The agent state belongs to
    /// [`set_agent_state`](Self::set_agent_state): a VM that already has a status keeps
    /// its stored agent state whatever `status.agent` says, so a reachability check
    /// that raced an agent sync cannot undo it. A status for a removed VM is dropped.
    pub async fn save_status(&self, status: &VmStatus) -> Result<Option<VmStatus>> {
        let mut tx = self.write().await?;
        let previous = status_in(&mut tx, status.vm).await?;
        let status = VmStatus {
            agent: previous.as_ref().map_or(status.agent, |p| p.agent),
            ..status.clone()
        };
        let saved = sqlx::query(
            "INSERT INTO status (vm_id, route, ssh, agent, checked_at, detail) \
            VALUES (?, ?, ?, ?, ?, ?) \
            ON CONFLICT (vm_id) DO UPDATE SET route = excluded.route, ssh = excluded.ssh, \
            checked_at = excluded.checked_at, detail = excluded.detail",
        )
        .bind(status.vm.to_string())
        .bind(&status.route)
        .bind(status.ssh.as_str())
        .bind(status.agent.as_str())
        .bind(status.checked_at.timestamp_millis())
        .bind(&status.detail)
        .execute(&mut *tx)
        .await;
        if dropped(saved)? {
            return Ok(None);
        }

        let changed = previous.filter(|p| status.changed_from(p));
        if let Some(prev) = &changed {
            insert_change(&mut tx, &status, prev).await?;
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// Records the agent state a sync found. Returns the previous state if it changed;
    /// the change is also a `status_changed` event. A VM that was never checked gets
    /// a status with SSH `not_checked`.
    pub async fn set_agent_state(&self, vm: VmId, agent: AgentState) -> Result<Option<AgentState>> {
        let mut tx = self.write().await?;
        let previous = status_in(&mut tx, vm).await?;
        let saved = sqlx::query(
            "INSERT INTO status (vm_id, ssh, agent, checked_at) VALUES (?, ?, ?, ?) \
            ON CONFLICT (vm_id) DO UPDATE SET agent = excluded.agent",
        )
        .bind(vm.to_string())
        .bind(SshState::NotChecked.as_str())
        .bind(agent.as_str())
        .bind(Utc::now().timestamp_millis())
        .execute(&mut *tx)
        .await;
        if dropped(saved)? {
            return Ok(None);
        }

        let changed = previous.filter(|p| p.agent != agent);
        if let Some(prev) = &changed {
            let next = VmStatus {
                agent,
                checked_at: Utc::now(),
                ..prev.clone()
            };
            insert_change(&mut tx, &next, prev).await?;
        }
        tx.commit().await?;
        Ok(changed.map(|p| p.agent))
    }

    pub async fn get_status(&self, vm: VmId) -> Result<Option<VmStatus>> {
        status_in(&mut *self.pool().acquire().await?, vm).await
    }

    /// The latest status of every VM that has been checked.
    pub async fn all_status(&self) -> Result<HashMap<VmId, VmStatus>> {
        let rows = sqlx::query("SELECT vm_id, route, ssh, agent, checked_at, detail FROM status")
            .fetch_all(self.pool())
            .await?;
        rows.iter()
            .map(|row| status_from_row(row).map(|s| (s.vm, s)))
            .collect()
    }
}

async fn status_in(conn: &mut SqliteConnection, vm: VmId) -> Result<Option<VmStatus>> {
    sqlx::query("SELECT vm_id, route, ssh, agent, checked_at, detail FROM status WHERE vm_id = ?")
        .bind(vm.to_string())
        .fetch_optional(&mut *conn)
        .await?
        .map(|row| status_from_row(&row))
        .transpose()
}

/// Whether a status write was dropped because its VM has been removed meanwhile.
fn dropped(saved: Result<SqliteQueryResult, sqlx::Error>) -> Result<bool> {
    match saved {
        Ok(_) => Ok(false),
        Err(sqlx::Error::Database(e)) if e.is_foreign_key_violation() => Ok(true),
        Err(e) => Err(e.into()),
    }
}

async fn insert_change(
    conn: &mut SqliteConnection,
    next: &VmStatus,
    prev: &VmStatus,
) -> Result<()> {
    let data = serde_json::json!({
        "route": next.route,
        "ssh": next.ssh,
        "agent": next.agent,
        "previous": { "route": prev.route, "ssh": prev.ssh, "agent": prev.agent },
        "detail": next.detail,
    });
    insert_event(
        conn,
        next.vm,
        LAPTOP,
        next.checked_at,
        "status_changed",
        &data,
    )
    .await
}

fn status_from_row(row: &SqliteRow) -> Result<VmStatus> {
    let ms: i64 = row.try_get("checked_at")?;
    Ok(VmStatus {
        vm: parse_id(&row.try_get::<String, _>("vm_id")?)?,
        route: row.try_get("route")?,
        ssh: parse(&row.try_get::<String, _>("ssh")?)?,
        agent: parse(&row.try_get::<String, _>("agent")?)?,
        checked_at: chrono::DateTime::from_timestamp_millis(ms)
            .ok_or_else(|| Error::Corrupt(format!("bad time {ms}")))?,
        detail: row.try_get("detail")?,
    })
}

fn parse<T: FromStr<Err = vmdesk_core::Error>>(s: &str) -> Result<T> {
    s.parse()
        .map_err(|e: vmdesk_core::Error| Error::Corrupt(e.to_string()))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use vmdesk_core::{AgentState, CredentialRef, EndpointKind, SshState};

    use super::*;
    use crate::{EndpointInput, NewVm};

    #[tokio::test]
    async fn latest_status_and_change_events() {
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
        let events = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM events WHERE kind = 'status_changed'",
            )
            .fetch_one(db.pool())
            .await
            .unwrap()
        };

        let mut s = VmStatus {
            vm: vm.id,
            route: Some("wlan0".into()),
            ssh: SshState::Ok,
            agent: AgentState::Unknown,
            checked_at: Utc::now(),
            detail: None,
        };
        assert!(
            db.save_status(&s).await.unwrap().is_none(),
            "first status is not a change"
        );
        s.checked_at = Utc::now();
        assert!(db.save_status(&s).await.unwrap().is_none());
        assert_eq!(events().await, 0);

        s.route = Some("tun0".into());
        s.ssh = SshState::Timeout;
        let prev = db.save_status(&s).await.unwrap().unwrap();
        assert_eq!(prev.ssh, SshState::Ok);
        assert_eq!(events().await, 1);
        s.ssh = SshState::Ok;
        db.save_status(&s).await.unwrap();
        assert_eq!(events().await, 2);

        let stored = db.get_status(vm.id).await.unwrap().unwrap();
        assert_eq!(
            (stored.route.as_deref(), stored.ssh),
            (Some("tun0"), SshState::Ok)
        );
        assert_eq!(db.all_status().await.unwrap().len(), 1);

        db.delete_vm(vm.id).await.unwrap();
        assert!(db.get_status(vm.id).await.unwrap().is_none());
        // A check that finishes after the VM was removed is dropped, not an error.
        assert!(db.save_status(&s).await.unwrap().is_none());
        assert!(db.get_status(vm.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn agent_state_is_owned_by_the_agent_sync() {
        let (db, vm) = crate::db_with_vm().await;
        let changes = || async {
            sqlx::query_scalar::<_, String>(
                "SELECT data_json FROM events WHERE kind = 'status_changed' ORDER BY seq",
            )
            .fetch_all(db.pool())
            .await
            .unwrap()
        };

        // Never checked: the sync's result is kept and SSH stays unknown.
        assert_eq!(db.set_agent_state(vm, AgentState::Ok).await.unwrap(), None);
        let s = db.get_status(vm).await.unwrap().unwrap();
        assert_eq!((s.ssh, s.agent), (SshState::NotChecked, AgentState::Ok));

        // A reachability check carrying a stale agent state does not undo it.
        let check = VmStatus {
            vm,
            route: Some("wlan0".into()),
            ssh: SshState::Ok,
            agent: AgentState::Unknown,
            checked_at: Utc::now(),
            detail: None,
        };
        let prev = db.save_status(&check).await.unwrap().expect("ssh changed");
        assert_eq!(prev.ssh, SshState::NotChecked);
        let s = db.get_status(vm).await.unwrap().unwrap();
        assert_eq!((s.ssh, s.agent), (SshState::Ok, AgentState::Ok));
        assert_eq!(changes().await.len(), 1);

        assert_eq!(db.set_agent_state(vm, AgentState::Ok).await.unwrap(), None);
        assert_eq!(changes().await.len(), 1, "no change, no event");
        assert_eq!(
            db.set_agent_state(vm, AgentState::NotRunning)
                .await
                .unwrap(),
            Some(AgentState::Ok)
        );
        let s = db.get_status(vm).await.unwrap().unwrap();
        assert_eq!(
            (s.route.as_deref(), s.ssh, s.agent),
            (Some("wlan0"), SshState::Ok, AgentState::NotRunning),
            "only the agent column changed"
        );
        let last: serde_json::Value =
            serde_json::from_str(changes().await.last().unwrap()).unwrap();
        assert_eq!(
            (&last["agent"], &last["previous"]["agent"], &last["ssh"]),
            (
                &serde_json::json!("not_running"),
                &serde_json::json!("ok"),
                &serde_json::json!("ok")
            )
        );
        let newest = db.timeline(vm, 1, None).await.unwrap();
        assert_eq!(newest[0].summary(), "agent ok -> not_running");

        db.delete_vm(vm).await.unwrap();
        assert_eq!(db.set_agent_state(vm, AgentState::Ok).await.unwrap(), None);
    }
}
