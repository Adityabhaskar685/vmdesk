use std::path::PathBuf;

use actix_web::http::header;
use actix_web::{HttpResponse, web};
use serde::Deserialize;
use uuid::Uuid;
use vmdesk_core::VmId;
use vmdesk_db::{ListFilter, NewVm, VmPatch};

use crate::{ApiError, AppState};

pub(crate) fn routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/vms")
            .route(web::get().to(list))
            .route(web::post().to(create)),
    )
    .service(
        web::resource("/vms/{id}")
            .route(web::get().to(get))
            .route(web::patch().to(update))
            .route(web::delete().to(delete)),
    )
    .service(web::resource("/import/ssh-config").route(web::post().to(import_ssh_config)));
}

type Reply = Result<HttpResponse, ApiError>;

async fn list(state: web::Data<AppState>, filter: web::Query<ListFilter>) -> Reply {
    Ok(HttpResponse::Ok().json(state.db.list_vms(&filter).await?))
}

async fn create(state: web::Data<AppState>, body: web::Json<NewVm>) -> Reply {
    let vm = state.db.create_vm(body.into_inner()).await?;
    Ok(HttpResponse::Created()
        .insert_header((header::LOCATION, format!("/api/v1/vms/{}", vm.id)))
        .json(vm))
}

async fn get(state: web::Data<AppState>, id: web::Path<Uuid>) -> Reply {
    Ok(HttpResponse::Ok().json(state.db.get_vm(VmId(id.into_inner())).await?))
}

async fn update(
    state: web::Data<AppState>,
    id: web::Path<Uuid>,
    body: web::Json<VmPatch>,
) -> Reply {
    Ok(HttpResponse::Ok().json(
        state
            .db
            .update_vm(VmId(id.into_inner()), body.into_inner())
            .await?,
    ))
}

async fn delete(state: web::Data<AppState>, id: web::Path<Uuid>) -> Reply {
    state.db.delete_vm(VmId(id.into_inner())).await?;
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportBody {
    /// Default to "~/.ssh/config"
    path: Option<PathBuf>,
}

async fn import_ssh_config(
    state: web::Data<AppState>,
    body: Option<web::Json<ImportBody>>,
) -> Reply {
    let path = body.and_then(|b| b.into_inner().path);
    Ok(HttpResponse::Ok().json(crate::import_ssh_config(&state.db, path).await?))
}
