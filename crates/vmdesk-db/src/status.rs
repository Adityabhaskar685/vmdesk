//! Latest reachability status per VM. A change in route, SSH or agent state is also
//! appended to the VM's timeline as a `status_changed` event.

use std::collections::HashMap;
use std::str::FromStr;

use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use vmdesk_core::{VmId, VmStatus};

use crate::inventory::parse_id;
use crate::timeline::insert_event;
use crate::{Db, Error, Result};

/// Event source for observations made by the laptop (the agent will use its own).
const LAPTOP: &str = "laptop";

impl Db {
    /// Stores `status` as the VM's latest. Returns the previous one if it differed.
    /// A status for a VM that no longer exists is dropped.
    pub async fn save_status(&self, status: &VmStatus) -> Result<Option<VmStatus>> {
        let mut tx = self.pool().begin().await?;
        let vm = status.vm.to_string();
        let previous = sqlx::query(
            "SELECT vm_id, route, ssh, agent, checked_at, detail FROM status WHERE vm_id = ?",
        )
        .bind(&vm)
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| status_from_row(&row))
        .transpose()?;

        let saved = sqlx::query(
            "INSERT INTO status (vm_id, route, ssh, agent, checked_at, detail) \
            VALUES (?, ?, ?, ?, ?, ?) \
            ON CONFLICT (vm_id) DO UPDATE SET route = excluded.route, ssh = excluded.ssh, \
            agent = excluded.agent, checked_at = excluded.checked_at, detail = excluded.detail",
        )
        .bind(&vm)
        .bind(&status.route)
        .bind(status.ssh.as_str())
        .bind(status.agent.as_str())
        .bind(status.checked_at.timestamp_millis())
        .bind(&status.detail)
        .execute(&mut *tx)
        .await;
        match saved {
            Ok(_) => {}
            // The VM was removed while it was being checked: nothing to record.
            Err(sqlx::Error::Database(e)) if e.is_foreign_key_violation() => return Ok(None),
            Err(e) => return Err(e.into()),
        }

        let changed = previous.filter(|p| status.changed_from(p));
        if let Some(prev) = &changed {
            let data = serde_json::json!({
                "route": status.route,
                "ssh": status.ssh,
                "agent": status.agent,
                "previous": { "route": prev.route, "ssh": prev.ssh, "agent": prev.agent },
                "detail": status.detail,
            });
            insert_event(
                &mut tx,
                status.vm,
                LAPTOP,
                status.checked_at,
                "status_changed",
                &data,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(changed)
    }

    pub async fn get_status(&self, vm: VmId) -> Result<Option<VmStatus>> {
        sqlx::query(
            "SELECT vm_id, route, ssh, agent, checked_at, detail FROM status WHERE vm_id = ?",
        )
        .bind(vm.to_string())
        .fetch_optional(self.pool())
        .await?
        .map(|row| status_from_row(&row))
        .transpose()
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
}
