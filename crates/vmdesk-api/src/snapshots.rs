//! A VM's latest snapshot, collecting one on demand, and its timeline.

use std::sync::Arc;

use actix_web::{HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;
use vmdesk_core::{HostKeyStore, VmId};
use vmdesk_db::ListFilter;
use vmdesk_transport::Transport;

use crate::{ApiError, AppState};

pub(crate) fn routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/vms/{id}/snapshot")
            .route(web::get().to(latest))
            .route(web::post().to(collect)),
    )
    .service(web::resource("/vms/{id}/events").route(web::get().to(events)));
}

type Reply = Result<HttpResponse, ApiError>;

async fn latest(state: web::Data<AppState>, id: web::Path<Uuid>) -> Reply {
    let vm = state.db.get_vm(VmId(id.into_inner())).await?;
    match state.db.latest_snapshot(vm.id).await? {
        Some(snap) => Ok(HttpResponse::Ok().json(snap)),
        None => Err(vmdesk_db::Error::NotFound.into()),
    }
}

/// Collects a snapshot now over the VM's connection path. 502 if it cannot be reached.
async fn collect(state: web::Data<AppState>, id: web::Path<Uuid>) -> Reply {
    let vm = state.db.get_vm(VmId(id.into_inner())).await?;
    let all = state.db.list_vms(&ListFilter::default()).await?;
    let inventory = all.into_iter().map(|v| (v.id, v)).collect();
    let transport = Transport::new(Arc::new(state.db.clone()) as Arc<dyn HostKeyStore>);
    let res = vmdesk_collect::remote::collect(&state.db, &transport, &vm, &inventory).await;
    transport.close_all().await;
    match res {
        Ok(snap) => Ok(HttpResponse::Ok().json(snap)),
        Err(e) => Ok(HttpResponse::BadGateway().json(crate::ErrorResponse {
            error: "unreachable".to_owned(),
            message: e.to_string(),
        })),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventQuery {
    /// Newest first, at most this many (default 100, max 1000).
    limit: Option<u32>,
    /// Only events before this time, for paging back.
    before: Option<DateTime<Utc>>,
}

async fn events(
    state: web::Data<AppState>,
    id: web::Path<Uuid>,
    q: web::Query<EventQuery>,
) -> Reply {
    let vm = state.db.get_vm(VmId(id.into_inner())).await?;
    let limit = q.limit.unwrap_or(100).min(1000);
    Ok(HttpResponse::Ok().json(state.db.timeline(vm.id, limit, q.before).await?))
}
