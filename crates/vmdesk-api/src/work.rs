//! Work sessions, notes, the current-work summary and event search.

use actix_web::{HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;
use vmdesk_collect::work::{CurrentWork, current_work};
use vmdesk_core::VmId;
use vmdesk_db::{SessionId, WorkSession};

use crate::{ApiError, AppState};

pub(crate) fn routes(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/vms/{id}/sessions").route(web::get().to(sessions)))
        .service(web::resource("/sessions/{id}").route(web::patch().to(rename)))
        .service(
            web::resource("/vms/{id}/notes")
                .route(web::get().to(notes))
                .route(web::post().to(add_note)),
        )
        .service(web::resource("/vms/{id}/current-work").route(web::get().to(work)))
        .service(web::resource("/search").route(web::get().to(search)));
}

type Reply = Result<HttpResponse, ApiError>;

#[derive(Serialize)]
struct SessionView {
    #[serde(flatten)]
    session: WorkSession,
    /// Whether activity now would still extend the session.
    ongoing: bool,
}

fn view(session: WorkSession) -> SessionView {
    SessionView {
        ongoing: session.ongoing(Utc::now()),
        session,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Limit {
    /// Newest first, at most this many (default 20, max 500).
    limit: Option<u32>,
}

async fn sessions(state: web::Data<AppState>, id: web::Path<Uuid>, q: web::Query<Limit>) -> Reply {
    let vm = state.db.get_vm(VmId(id.into_inner())).await?;
    let limit = q.limit.unwrap_or(20).min(500);
    let sessions = state.db.sessions(vm.id, limit).await?;
    Ok(HttpResponse::Ok().json(sessions.into_iter().map(view).collect::<Vec<_>>()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rename {
    /// An empty title clears it.
    title: String,
}

async fn rename(state: web::Data<AppState>, id: web::Path<Uuid>, body: web::Json<Rename>) -> Reply {
    let session = state
        .db
        .rename_session(SessionId(id.into_inner()), &body.title)
        .await?;
    Ok(HttpResponse::Ok().json(view(session)))
}

async fn notes(state: web::Data<AppState>, id: web::Path<Uuid>) -> Reply {
    let vm = state.db.get_vm(VmId(id.into_inner())).await?;
    Ok(HttpResponse::Ok().json(state.db.notes(vm.id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewNote {
    body: String,
    #[serde(default, deserialize_with = "pin")]
    session: Pin,
}

/// Where a new note goes, from its `session` field.
#[derive(Default)]
enum Pin {
    /// Field left out: the VM's latest session, if it has one.
    #[default]
    Latest,
    /// `null`: no session.
    Unpinned,
    Session(SessionId),
}

fn pin<'de, D: Deserializer<'de>>(d: D) -> Result<Pin, D::Error> {
    Ok(Option::deserialize(d)?.map_or(Pin::Unpinned, Pin::Session))
}

async fn add_note(
    state: web::Data<AppState>,
    id: web::Path<Uuid>,
    body: web::Json<NewNote>,
) -> Reply {
    let vm = state.db.get_vm(VmId(id.into_inner())).await?;
    let session = match body.session {
        Pin::Latest => state.db.latest_session(vm.id).await?.map(|s| s.id),
        Pin::Unpinned => None,
        Pin::Session(s) => Some(s),
    };
    let note = state.db.add_note(vm.id, session, &body.body).await?;
    Ok(HttpResponse::Created().json(note))
}

#[derive(Serialize)]
struct WorkView {
    #[serde(flatten)]
    work: CurrentWork,
    /// The rendered summary in the server's local time.
    line: String,
}

/// 404 when the VM has no snapshot yet.
async fn work(state: web::Data<AppState>, id: web::Path<Uuid>) -> Reply {
    let vm = state.db.get_vm(VmId(id.into_inner())).await?;
    let snapshot = state
        .db
        .latest_snapshot(vm.id)
        .await?
        .ok_or(vmdesk_db::Error::NotFound)?;
    let session = state.db.latest_session(vm.id).await?;
    let notes = state.db.notes(vm.id).await?;
    let work = current_work(&snapshot, session.as_ref(), &notes);
    Ok(HttpResponse::Ok().json(WorkView {
        line: work.line(),
        work,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchQuery {
    q: String,
    /// Only events at or after this time.
    since: Option<DateTime<Utc>>,
    /// Newest first, at most this many (default 50, max 500).
    limit: Option<u32>,
}

async fn search(state: web::Data<AppState>, q: web::Query<SearchQuery>) -> Reply {
    let limit = q.limit.unwrap_or(50).min(500);
    Ok(HttpResponse::Ok().json(state.db.search(&q.q, q.since, limit).await?))
}
