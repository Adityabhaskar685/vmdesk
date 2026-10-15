use std::sync::Arc;

use actix_web::http::{StatusCode, header};
use actix_web::{App, test, web};
use serde_json::{Value, json};

use crate::{AppState, configure};

const TOKEN: &str = "test-token";

macro_rules! app {
    () => {{
        let db = vmdesk_db::Db::open_in_memory().await.unwrap();
        let state = AppState {
            db,
            token: Arc::from(TOKEN),
        };
        test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .configure(configure),
        )
        .await
    }};
}

fn auth(req: test::TestRequest) -> test::TestRequest {
    req.insert_header((header::AUTHORIZATION, format!("Bearer {TOKEN}")))
}

#[actix_web::test]
async fn health_is_open_everything_else_needs_token() {
    let app = app!();
    let res = test::call_service(
        &app,
        test::TestRequest::get().uri("/api/v1/health").to_request(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);

    let res = test::call_service(
        &app,
        test::TestRequest::get().uri("/api/v1/vms").to_request(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    let wrong = test::TestRequest::get()
        .uri("/api/v1/vms")
        .insert_header((header::AUTHORIZATION, "Bearer nope"))
        .to_request();
    assert_eq!(
        test::call_service(&app, wrong).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let ok = auth(test::TestRequest::get().uri("/api/v1/vms")).to_request();
    assert_eq!(test::call_service(&app, ok).await.status(), StatusCode::OK);
}

#[actix_web::test]
async fn crud_flow() {
    let app = app!();
    let req = auth(test::TestRequest::post().uri("/api/v1/vms"))
        .set_json(json!({
            "name": "bastion-01",
            "username": "aditya",
            "endpoints": [{ "address": "172.20.4.1", "port": 2222 }]
        }))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let bastion: Value = test::read_body_json(res).await;
    let bastion_id = bastion["id"].as_str().unwrap().to_owned();
    assert_eq!(bastion["credential"]["type"], "ssh_agent");

    let req = auth(test::TestRequest::post().uri("/api/v1/vms"))
        .set_json(json!({
            "name": "regal-dev-07",
            "username": "aditya",
            "endpoints": [{ "address": "10.24.18.32" }],
            "paths": [[{ "kind": "jump", "via": bastion_id }, { "kind": "direct" }]],
            "tags": ["regal"]
        }))
        .to_request();
    let lab: Value = test::call_and_read_body_json(&app, req).await;
    let lab_id = lab["id"].as_str().unwrap().to_owned();
    assert_eq!(lab["paths"][0]["hops"][0]["via"], bastion_id.as_str());

    let req = auth(test::TestRequest::get().uri("/api/v1/vms?tag=regal")).to_request();
    let listed: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let req = auth(test::TestRequest::patch().uri(&format!("/api/v1/vms/{lab_id}")))
        .set_json(json!({ "favourite": true }))
        .to_request();
    let patched: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(patched["favourite"], true);
    assert_eq!(patched["tags"], json!(["regal"]));

    let req =
        auth(test::TestRequest::delete().uri(&format!("/api/v1/vms/{bastion_id}"))).to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body: Value = test::read_body_json(res).await;
    assert!(body["message"].as_str().unwrap().contains("regal-dev-07"));

    let req = auth(test::TestRequest::delete().uri(&format!("/api/v1/vms/{lab_id}"))).to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NO_CONTENT
    );
    let req = auth(test::TestRequest::get().uri(&format!("/api/v1/vms/{lab_id}"))).to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NOT_FOUND
    );
}

#[actix_web::test]
async fn validation_and_conflict_statuses() {
    let app = app!();
    let body = json!({ "name": "dev", "username": "a", "endpoints": [{ "address": "1.2.3.4" }] });
    let req = auth(test::TestRequest::post().uri("/api/v1/vms"))
        .set_json(&body)
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::CREATED
    );
    let req = auth(test::TestRequest::post().uri("/api/v1/vms"))
        .set_json(&body)
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::CONFLICT
    );

    let bad =
        json!({ "name": "bad name", "username": "a", "endpoints": [{ "address": "1.2.3.4" }] });
    let req = auth(test::TestRequest::post().uri("/api/v1/vms"))
        .set_json(&bad)
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = test::read_body_json(res).await;
    assert_eq!(body["error"], "invalid");
}

#[actix_web::test]
async fn imports_ssh_config_with_jump_paths() {
    let dir = std::env::temp_dir().join(format!("vmdesk-api-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("config");
    std::fs::write(
        &cfg,
        "Host lab r7\n  HostName 10.24.18.32\n  ProxyJump bastion\n\nHost bastion\n  HostName 172.20.4.1\n  User ops\n",
    )
    .unwrap();

    let app = app!();
    let req = auth(test::TestRequest::post().uri("/api/v1/import/ssh-config"))
        .set_json(json!({ "path": cfg }))
        .to_request();
    let report: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(report["added"], json!(["lab", "bastion"]));
    assert_eq!(report["warnings"], json!([]));

    let req = auth(test::TestRequest::get().uri("/api/v1/vms?provider=ssh-config")).to_request();
    let vms: Value = test::call_and_read_body_json(&app, req).await;
    let vms = vms.as_array().unwrap();
    let bastion = vms.iter().find(|v| v["name"] == "bastion").unwrap();
    let lab = vms.iter().find(|v| v["name"] == "lab").unwrap();
    assert_eq!(bastion["username"], "ops");
    assert_eq!(lab["labels"]["aliases"], "r7");
    assert_eq!(lab["paths"][0]["hops"][0]["via"], bastion["id"]);
    assert_eq!(lab["paths"][0]["source"], "provider");

    let missing = auth(test::TestRequest::post().uri("/api/v1/import/ssh-config"))
        .set_json(json!({ "path": dir.join("nope") }))
        .to_request();
    assert_eq!(
        test::call_service(&app, missing).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    std::fs::remove_dir_all(dir).ok();
}
#[actix_web::test]
async fn malformed_input_is_a_json_400() {
    let app = app!();
    let cases = [
        auth(test::TestRequest::post().uri("/api/v1/vms"))
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_payload("{\"name\":")
            .to_request(),
        auth(test::TestRequest::post().uri("/api/v1/vms"))
            .set_json(json!({ "name": "x", "username": "a", "endpoints": [], "bogus": 1 }))
            .to_request(),
        auth(test::TestRequest::get().uri("/api/v1/vms/not-a-uuid")).to_request(),
    ];
    for req in cases {
        let res = test::call_service(&app, req).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body: Value = test::read_body_json(res).await;
        assert_eq!(body["error"], "bad_request");
        assert!(!body["message"].as_str().unwrap().is_empty());
    }

    let res = test::call_service(
        &app,
        test::TestRequest::get().uri("/api/v1/vms").to_request(),
    )
    .await;
    let body: Value = test::read_body_json(res).await;
    assert_eq!(body["error"], "unauthorized");
}
