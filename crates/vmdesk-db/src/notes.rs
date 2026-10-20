//! Notes on a VM, each optionally pinned to one of its work sessions (spec section 11).

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;
use vmdesk_core::VmId;

use crate::inventory::{from_ms, now_ms, parse_id};
use crate::sessions::SessionId;
use crate::{Db, Error, Result};

const MAX_BODY: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NoteId(pub Uuid);

impl fmt::Display for NoteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Note {
    pub id: NoteId,
    pub vm: VmId,
    /// The work session it is pinned to. Notes move with their session when it merges
    /// with another, and stay on the VM if the session disappears.
    pub session: Option<SessionId>,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn validate_body(body: &str) -> Result<&str> {
    let body = body.trim();
    if body.is_empty() {
        return Err(Error::Invalid("empty note".into()));
    }
    if body.chars().count() > MAX_BODY {
        return Err(Error::Invalid(format!(
            "note longer than {MAX_BODY} characters"
        )));
    }
    Ok(body)
}

const COLUMNS: &str = "id, vm_id, session_id, body, created_at, updated_at";

fn note_from_row(row: &SqliteRow) -> Result<Note> {
    let id: String = row.try_get("id")?;
    let session: Option<String> = row.try_get("session_id")?;
    let uuid = |s: &str| {
        s.parse::<Uuid>()
            .map_err(|_| Error::Corrupt(format!("bad id {s:?}")))
    };
    Ok(Note {
        id: NoteId(uuid(&id)?),
        vm: parse_id(&row.try_get::<String, _>("vm_id")?)?,
        session: session.as_deref().map(uuid).transpose()?.map(SessionId),
        body: row.try_get("body")?,
        created_at: from_ms(row.try_get("created_at")?)?,
        updated_at: from_ms(row.try_get("updated_at")?)?,
    })
}

impl Db {
    /// Adds a note to `vm`, pinned to `session` if given, which must be one of its own.
    pub async fn add_note(&self, vm: VmId, session: Option<SessionId>, body: &str) -> Result<Note> {
        let body = validate_body(body)?;
        let mut tx = self.write().await?;
        let known: Option<i64> = sqlx::query_scalar("SELECT 1 FROM vms WHERE id = ?")
            .bind(vm.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        if known.is_none() {
            return Err(Error::NotFound);
        }
        if let Some(s) = session {
            let owner: Option<String> =
                sqlx::query_scalar("SELECT vm_id FROM work_sessions WHERE id = ?")
                    .bind(s.to_string())
                    .fetch_optional(&mut *tx)
                    .await?;
            if owner != Some(vm.to_string()) {
                return Err(Error::Invalid(format!("no session {s} on this VM")));
            }
        }
        let id = NoteId(Uuid::now_v7());
        let now = now_ms();
        sqlx::query(&format!(
            "INSERT INTO notes ({COLUMNS}) VALUES (?, ?, ?, ?, ?, ?)"
        ))
        .bind(id.to_string())
        .bind(vm.to_string())
        .bind(session.map(|s| s.to_string()))
        .bind(body)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.note(id).await
    }

    pub async fn note(&self, id: NoteId) -> Result<Note> {
        sqlx::query(&format!("SELECT {COLUMNS} FROM notes WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await?
            .map(|r| note_from_row(&r))
            .transpose()?
            .ok_or(Error::NotFound)
    }

    /// The VM's notes, newest first.
    pub async fn notes(&self, vm: VmId) -> Result<Vec<Note>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM notes WHERE vm_id = ? ORDER BY created_at DESC, id DESC"
        ))
        .bind(vm.to_string())
        .fetch_all(self.pool())
        .await?
        .iter()
        .map(note_from_row)
        .collect()
    }

    pub async fn update_note(&self, id: NoteId, body: &str) -> Result<Note> {
        let body = validate_body(body)?;
        let done = sqlx::query("UPDATE notes SET body = ?, updated_at = ? WHERE id = ?")
            .bind(body)
            .bind(now_ms())
            .bind(id.to_string())
            .execute(self.pool())
            .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        self.note(id).await
    }

    pub async fn delete_note(&self, id: NoteId) -> Result<()> {
        let done = sqlx::query("DELETE FROM notes WHERE id = ?")
            .bind(id.to_string())
            .execute(self.pool())
            .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use vmdesk_proto::{EventKind, Snapshot};

    use super::*;
    use crate::db_with_vm;

    #[tokio::test]
    async fn notes_crud_newest_first() {
        let (db, vm) = db_with_vm().await;
        let first = db
            .add_note(vm, None, "  check the UPF logs ")
            .await
            .unwrap();
        assert_eq!(first.body, "check the UPF logs");
        let second = db.add_note(vm, None, "second").await.unwrap();
        let listed: Vec<NoteId> = db.notes(vm).await.unwrap().iter().map(|n| n.id).collect();
        assert_eq!(listed, [second.id, first.id]);

        let edited = db.update_note(first.id, "edited").await.unwrap();
        assert_eq!(edited.body, "edited");
        assert!(edited.updated_at >= first.updated_at);
        db.delete_note(second.id).await.unwrap();
        assert_eq!(db.notes(vm).await.unwrap(), [edited]);

        assert!(matches!(
            db.add_note(vm, None, " ").await,
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            db.delete_note(second.id).await,
            Err(Error::NotFound)
        ));
        assert!(matches!(
            db.add_note(VmId::new(), None, "x").await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn a_note_pins_only_to_a_session_of_its_vm() {
        let (db, vm) = db_with_vm().await;
        let at = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        let snap = Snapshot {
            taken_at: Some(at),
            ..Snapshot::default()
        };
        let events = [(at, EventKind::TmuxAttached { name: "a".into() })];
        db.record_snapshot(vm, "ssh", &snap, &events).await.unwrap();
        let session = db.latest_session(vm).await.unwrap().unwrap();

        let pinned = db.add_note(vm, Some(session.id), "pinned").await.unwrap();
        assert_eq!(pinned.session, Some(session.id));

        let other = crate::tests::add_vm(&db, "other").await;
        assert!(matches!(
            db.add_note(other, Some(session.id), "wrong vm").await,
            Err(Error::Invalid(_))
        ));
    }
}
