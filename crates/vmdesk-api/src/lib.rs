//! Local HTTP API. Bound to 127.0.0.1; every route except `/api/v1/health`
//! requires `Authorization: Bearer <token>`.

mod agent;
mod auth;
mod error;
mod snapshots;
mod status;
mod vms;

use std::path::PathBuf;
use std::sync::Arc;

use actix_web::middleware::from_fn;
use actix_web::{HttpResponse, web};
use serde::Serialize;
use vmdesk_core::Provider;
use vmdesk_db::{Db, SyncReport};
use vmdesk_providers::SshConfigProvider;

pub use error::ApiError;

/// Shared Application State
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub token: Arc<str>,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub message: String,
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
    vms: i64,
}

async fn health(state: web::Data<AppState>) -> Result<HttpResponse, ApiError> {
    let vms = state.db.vm_count().await?;
    Ok(HttpResponse::Ok().json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        vms,
    }))
}

/// Registers all `/api/v1` routes.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/v1")
            .wrap(from_fn(auth::require_token))
            .app_data(web::JsonConfig::default().error_handler(|e, _| error::bad_request(e)))
            .app_data(web::PathConfig::default().error_handler(|e, _| error::bad_request(e)))
            .app_data(web::QueryConfig::default().error_handler(|e, _| error::bad_request(e)))
            .route("/health", web::get().to(health))
            .configure(vms::routes)
            .configure(status::routes)
            .configure(snapshots::routes)
            .configure(agent::routes),
    );
}

/// Imports `path` (default `~/.ssh/config`) into the inventory.
pub async fn import_ssh_config(
    db: &Db,
    path: Option<PathBuf>,
) -> Result<SyncReport, vmdesk_db::Error> {
    let path = path
        .or_else(SshConfigProvider::default_path)
        .ok_or_else(|| vmdesk_db::Error::Invalid("no home directory, pass a path".into()))?;
    let provider = SshConfigProvider::new(path);
    let listing = provider.list().await.map_err(|e| {
        vmdesk_db::Error::Invalid(format!("cannot read {}: {e}", provider.path().display()))
    })?;
    db.sync_provider(provider.id(), listing, &local_username())
        .await
}

/// The local login name, which `ssh` uses when a lost has no `User`.
#[must_use]
pub fn local_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".to_owned())
}

#[cfg(test)]
mod tests;
