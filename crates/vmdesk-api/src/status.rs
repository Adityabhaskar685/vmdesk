//! Reachability status: latest stored results, and on-demand checks.

use actix_web::{HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;
use vmdesk_core::{AgentState, Headline, SshState, Vm, VmId, VmStatus};
use vmdesk_db::ListFilter;
use vmdesk_reach::Monitor;

use crate::{ApiError, AppState};

pub(crate) fn routes(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/status").route(web::get().to(all)))
        .service(web::resource("/vms/{id}/status").route(web::get().to(one)))
        .service(web::resource("/vms/{id}/check").route(web::post().to(check)));
}

type Reply = Result<HttpResponse, ApiError>;

/// A VM's status with its name and derived headline. Fields are null when the VM
/// has never been checked.
#[derive(Serialize)]
struct StatusView {
    vm: VmId,
    name: String,
    headline: Option<Headline>,
    route: Option<String>,
    ssh: Option<SshState>,
    agent: Option<AgentState>,
    checked_at: Option<DateTime<Utc>>,
    detail: Option<String>,
}

fn view(vm: &Vm, status: Option<&VmStatus>) -> StatusView {
    StatusView {
        vm: vm.id,
        name: vm.name.clone(),
        headline: status.map(|s| s.headline(vm.lifecycle)),
        route: status.and_then(|s| s.route.clone()),
        ssh: status.map(|s| s.ssh),
        agent: status.map(|s| s.agent),
        checked_at: status.map(|s| s.checked_at),
        detail: status.and_then(|s| s.detail.clone()),
    }
}

async fn all(state: web::Data<AppState>) -> Reply {
    let statuses = state.db.all_status().await?;
    let vms = state.db.list_vms(&ListFilter::default()).await?;
    let views: Vec<StatusView> = vms.iter().map(|v| view(v, statuses.get(&v.id))).collect();
    Ok(HttpResponse::Ok().json(views))
}

async fn one(state: web::Data<AppState>, id: web::Path<Uuid>) -> Reply {
    let vm = state.db.get_vm(VmId(id.into_inner())).await?;
    let status = state.db.get_status(vm.id).await?;
    Ok(HttpResponse::Ok().json(view(&vm, status.as_ref())))
}

/// Checks the VM now (route, then SSH login) and returns the fresh result.
async fn check(state: web::Data<AppState>, id: web::Path<Uuid>) -> Reply {
    let vm = state.db.get_vm(VmId(id.into_inner())).await?;
    let checked = Monitor::new(state.db.clone())
        .check_now(Some(&[vm.id]))
        .await?;
    let status = checked.into_iter().next().map(|c| c.status);
    Ok(HttpResponse::Ok().json(view(&vm, status.as_ref())))
}
