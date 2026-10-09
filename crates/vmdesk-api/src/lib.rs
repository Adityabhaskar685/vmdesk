//! Local HTTP API. Bound to 127.0.0.1 only. every request needs the install token.

use actix_web::{HttpResponse, Responder, web};
use serde::Serialize;
use vmdesk_db::Db;

/// Shared App State
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
    vms: i64,
}

async fn health(state: web::Data<AppState>) -> impl Responder {
    match state.db.vm_count().await {
        Ok(vms) => HttpResponse::Ok().json(Health {
            status: "ok",
            version: env!("CARGO_PKG_VERSION"),
            vms,
        }),
        Err(e) => {
            tracing::error!(error = %e, "health check failed");
            HttpResponse::InternalServerError().finish()
        }
    }
}

/// Registers all `/api/v1` routes.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(web::scope("/api/v1").route("/health", web::get().to(health)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, test};

    #[actix_web::test]
    async fn health_reports_ok() {
        let db = Db::open_in_memory().await.unwrap();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(AppState { db }))
                .configure(configure),
        )
        .await;
        let req = test::TestRequest::get().uri("/api/v1/health").to_request();
        let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;
        assert_eq!(body["status"], "ok");
        assert_eq!(body["vms"], 0);
    }
}
