//! A VM's agent: what the laptop knows about it, and its last observed state.

use actix_web::{HttpResponse, web};
use serde::Serialize;
use uuid::Uuid;
use vmdesk_core::{AgentState, VmId};
use vmdesk_db::AgentRecord;

use crate::{ApiError, AppState};

pub(crate) fn routes(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/vms/{id}/agent").route(web::get().to(agent)));
}

#[derive(Serialize)]
struct AgentView {
    #[serde(flatten)]
    agent: AgentRecord,
    /// From the VM's status; `unknown` before the first sync.
    state: AgentState,
}

/// 404 when the VM has no agent.
async fn agent(state: web::Data<AppState>, id: web::Path<Uuid>) -> Result<HttpResponse, ApiError> {
    let vm = state.db.get_vm(VmId(id.into_inner())).await?;
    let agent = state
        .db
        .agent(vm.id)
        .await?
        .ok_or(vmdesk_db::Error::NotFound)?;
    let observed = state.db.get_status(vm.id).await?;
    Ok(HttpResponse::Ok().json(AgentView {
        agent,
        state: observed.map_or(AgentState::Unknown, |s| s.agent),
    }))
}
