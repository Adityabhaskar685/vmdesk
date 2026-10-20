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

#[actix_web::test]
async fn status_before_and_after_a_check() {
    let app = app!();
    let req = auth(test::TestRequest::post().uri("/api/v1/vms"))
        .set_json(json!({ "name": "closed", "username": "a", "endpoints": [{ "address": "127.0.0.1", "port": 1 }] }))
        .to_request();
    let vm: Value = test::call_and_read_body_json(&app, req).await;
    let id = vm["id"].as_str().unwrap().to_owned();

    let req = auth(test::TestRequest::get().uri("/api/v1/status")).to_request();
    let all: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(all[0]["name"], "closed");
    assert!(all[0]["ssh"].is_null(), "never checked");

    let req = auth(test::TestRequest::post().uri(&format!("/api/v1/vms/{id}/check"))).to_request();
    let checked: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(checked["ssh"], "refused");
    assert_eq!(checked["headline"], "ssh_failing");
    assert!(checked["route"].is_string());

    let req = auth(test::TestRequest::get().uri(&format!("/api/v1/vms/{id}/status"))).to_request();
    let stored: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(stored["ssh"], "refused");

    let req =
        auth(test::TestRequest::post().uri(&format!("/api/v1/vms/{}/check", uuid::Uuid::now_v7())))
            .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NOT_FOUND
    );
}

#[actix_web::test]
async fn snapshot_and_events_routes() {
    let app = app!();
    let req = auth(test::TestRequest::post().uri("/api/v1/vms"))
        .set_json(json!({ "name": "closed", "username": "a", "endpoints": [{ "address": "127.0.0.1", "port": 1 }] }))
        .to_request();
    let vm: Value = test::call_and_read_body_json(&app, req).await;
    let id = vm["id"].as_str().unwrap().to_owned();

    let req =
        auth(test::TestRequest::get().uri(&format!("/api/v1/vms/{id}/snapshot"))).to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NOT_FOUND
    );

    let req =
        auth(test::TestRequest::post().uri(&format!("/api/v1/vms/{id}/snapshot"))).to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    let body: Value = test::read_body_json(res).await;
    assert_eq!(body["error"], "unreachable");

    let req = auth(test::TestRequest::get().uri(&format!("/api/v1/vms/{id}/events?limit=5")))
        .to_request();
    let events: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(events, json!([]));
}

#[actix_web::test]
async fn agent_route() {
    let db = vmdesk_db::Db::open_in_memory().await.unwrap();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(AppState {
                db: db.clone(),
                token: Arc::from(TOKEN),
            }))
            .configure(configure),
    )
    .await;
    let req = auth(test::TestRequest::post().uri("/api/v1/vms"))
        .set_json(
            json!({ "name": "box", "username": "a", "endpoints": [{ "address": "10.0.0.9" }] }),
        )
        .to_request();
    let vm: Value = test::call_and_read_body_json(&app, req).await;
    let id: vmdesk_core::VmId = vm["id"].as_str().unwrap().parse().unwrap();
    let get = |id: String| auth(test::TestRequest::get().uri(&format!("/api/v1/vms/{id}/agent")));

    let res = test::call_service(&app, get(id.to_string()).to_request()).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND, "no agent yet");

    let agent_id = uuid::Uuid::now_v7();
    db.save_agent(
        &vmdesk_db::AgentRecord {
            vm: id,
            agent_id,
            version: "0.1.0".into(),
            proto: 1,
            installed_at: None,
            last_seq: 42,
            last_contact_at: None,
        },
        None,
    )
    .await
    .unwrap();
    let body: Value = test::call_and_read_body_json(&app, get(id.to_string()).to_request()).await;
    assert_eq!(body["agent_id"], agent_id.to_string());
    assert_eq!(
        (body["last_seq"].as_u64(), body["state"].as_str()),
        (Some(42), Some("unknown"))
    );

    db.set_agent_state(id, vmdesk_core::AgentState::Outdated)
        .await
        .unwrap();
    let body: Value = test::call_and_read_body_json(&app, get(id.to_string()).to_request()).await;
    assert_eq!(body["state"], "outdated");

    let res = test::call_service(&app, get(uuid::Uuid::now_v7().to_string()).to_request()).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND, "no VM");
}

#[actix_web::test]
async fn sessions_notes_current_work_and_search() {
    let db = vmdesk_db::Db::open_in_memory().await.unwrap();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(AppState {
                db: db.clone(),
                token: Arc::from(TOKEN),
            }))
            .configure(configure),
    )
    .await;
    let req = auth(test::TestRequest::post().uri("/api/v1/vms"))
        .set_json(
            json!({ "name": "lab", "username": "a", "endpoints": [{ "address": "10.0.0.9" }] }),
        )
        .to_request();
    let vm: Value = test::call_and_read_body_json(&app, req).await;
    let id = vm["id"].as_str().unwrap().to_owned();
    let vm_id: vmdesk_core::VmId = id.parse().unwrap();
    let call = |req: test::TestRequest| test::call_service(&app, auth(req).to_request());

    let res = call(test::TestRequest::get().uri(&format!("/api/v1/vms/{id}/current-work"))).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND, "no snapshot yet");

    let now = chrono::Utc::now();
    let started = now.timestamp() - 600;
    let snapshot = vmdesk_proto::Snapshot {
        taken_at: Some(now),
        processes: vec![vmdesk_proto::ProcessInfo {
            pid: 7,
            ppid: 6,
            user: "a".into(),
            cmdline: "pytest upf-tests/n4".into(),
            cwd: Some("/root/regal".into()),
            started_at: started,
            cpu_pct: 0.0,
            rss_kb: 0,
        }],
        ..vmdesk_proto::Snapshot::default()
    };
    let event = vmdesk_proto::EventKind::ProcessStarted {
        pid: 7,
        cmdline: "pytest upf-tests/n4".into(),
        cwd: Some("/root/regal".into()),
    };
    let at = chrono::DateTime::from_timestamp(started, 0).unwrap();
    db.record_snapshot(vm_id, "ssh", &snapshot, &[(at, event)])
        .await
        .unwrap();

    let res = call(test::TestRequest::get().uri(&format!("/api/v1/vms/{id}/sessions"))).await;
    assert_eq!(res.status(), StatusCode::OK);
    let sessions: Value = test::read_body_json(res).await;
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    assert_eq!(sessions[0]["ongoing"], true);
    assert_eq!(
        sessions[0]["summary"]["commands"][0]["cmdline"],
        "pytest upf-tests/n4"
    );
    let session = sessions[0]["id"].as_str().unwrap().to_owned();

    let patch = |body: Value, session: &str| {
        test::TestRequest::patch()
            .uri(&format!("/api/v1/sessions/{session}"))
            .set_json(body)
    };
    let res = call(patch(json!({ "title": "n4 tests" }), &session)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let renamed: Value = test::read_body_json(res).await;
    assert_eq!(renamed["title"], "n4 tests");
    for (body, status) in [
        (json!({ "title": null }), StatusCode::BAD_REQUEST),
        (json!({}), StatusCode::BAD_REQUEST),
        (
            json!({ "title": "x".repeat(201) }),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        assert_eq!(
            call(patch(body.clone(), &session)).await.status(),
            status,
            "{body}"
        );
    }
    let res = call(patch(
        json!({ "title": "x" }),
        &uuid::Uuid::now_v7().to_string(),
    ))
    .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let post = |body: Value| {
        test::TestRequest::post()
            .uri(&format!("/api/v1/vms/{id}/notes"))
            .set_json(body)
    };
    let res = call(post(json!({ "body": "waiting on CI" }))).await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let pinned: Value = test::read_body_json(res).await;
    assert_eq!(pinned["session"], session.as_str(), "the latest session");
    let res = call(post(json!({ "body": "about the VM", "session": null }))).await;
    let unpinned: Value = test::read_body_json(res).await;
    assert_eq!(unpinned["session"], Value::Null);
    let res = call(post(
        json!({ "body": "x", "session": uuid::Uuid::now_v7() }),
    ))
    .await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let res = call(post(json!({ "body": "  " }))).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let res = call(test::TestRequest::get().uri(&format!("/api/v1/vms/{id}/notes"))).await;
    let notes: Value = test::read_body_json(res).await;
    assert_eq!(notes.as_array().unwrap().len(), 2);

    let res = call(test::TestRequest::get().uri(&format!("/api/v1/vms/{id}/current-work"))).await;
    let work: Value = test::read_body_json(res).await;
    assert_eq!(work["note"], "waiting on CI");
    assert_eq!(work["anchor"]["kind"], "process");
    let line = work["line"].as_str().unwrap();
    assert!(
        line.starts_with("pytest upf-tests/n4 in /root/regal · since "),
        "{line}"
    );

    let search = |q: &str| test::TestRequest::get().uri(&format!("/api/v1/search?{q}"));
    let hits: Value = test::read_body_json(call(search("q=upf-tests")).await).await;
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(
        (hits[0]["vm_name"].as_str(), hits[0]["kind"].as_str()),
        (Some("lab"), Some("process_started"))
    );
    let hits: Value = test::read_body_json(call(search("q=%22pytest%22%20OR")).await).await;
    assert_eq!(hits.as_array().unwrap().len(), 0, "quotes and OR are words");
    let future = (now + chrono::Duration::hours(1)).to_rfc3339();
    let later = format!("q=pytest&since={}", future.replace('+', "%2B"));
    let hits: Value = test::read_body_json(call(search(&later)).await).await;
    assert_eq!(hits.as_array().unwrap().len(), 0);
    let hits: Value = test::read_body_json(call(search("q=pytest")).await).await;
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(
        call(search("q=--")).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        call(search("limit=5")).await.status(),
        StatusCode::BAD_REQUEST
    );
}
