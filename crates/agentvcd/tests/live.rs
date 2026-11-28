//! The live board and stopping runs.

use std::fs;

use agentvc_core::{ForkRequest, Ref, Space, Status};
use agentvcd::{AppState, Config};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// The daemon token these tests run with.
const TOKEN: &str = "test-token";

async fn send(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "localhost:7420")
        .header("x-agentvc-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn live_lists_running_attempts_with_overlaps() {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path();
    fs::write(w.join("lib.rs"), "1\n").unwrap();
    fs::write(w.join("other.rs"), "x\n").unwrap();
    let space = Space::init(w).unwrap();
    let task = space.create_task("fix it", None).unwrap();
    let fork = |actor: &str| {
        space
            .fork(&ForkRequest {
                task_id: Some(task.id),
                from: Some(Ref::Attempt(1)),
                actor: actor.into(),
                ..Default::default()
            })
            .unwrap()
            .attempt
    };
    let (a2, a3, a4) = (fork("agent:one"), fork("agent:two"), fork("agent:three"));
    fs::write(a2.view_path.join("lib.rs"), "2\n").unwrap();
    space
        .snapshot(a2.id, "tool:Edit", Some(json!({ "cost_usd": 0.1 })))
        .unwrap();
    fs::write(a3.view_path.join("lib.rs"), "3\n").unwrap();
    fs::write(a3.view_path.join("other.rs"), "y\n").unwrap();
    space.snapshot(a3.id, "tool:Write", None).unwrap();
    // Finished attempts are not on the board.
    space.set_status(a4.id, Status::Failed, None).unwrap();
    space
        .create_run(
            a3.id,
            &["agent".into(), "go".into()],
            Some("make test"),
            false,
        )
        .unwrap();

    let app = agentvcd::router(AppState::start(&Config::new(w).with_token(TOKEN)).unwrap());
    let (s, v) = send(&app, "GET", "/api/live").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let ids: Vec<i64> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["attempt"]["id"].as_i64().unwrap())
        .collect();
    assert!(ids.contains(&a2.id) && ids.contains(&a3.id));
    assert!(!ids.contains(&1) && !ids.contains(&a4.id));

    let get = |id: i64| {
        v.as_array()
            .unwrap()
            .iter()
            .find(|x| x["attempt"]["id"] == id)
            .unwrap()
    };
    let c2 = get(a2.id);
    assert_eq!(c2["task_goal"], "fix it");
    assert_eq!(c2["head"]["trigger"], "tool:Edit");
    assert_eq!(c2["files_changed"], 1);
    assert_eq!(c2["stats"]["cost_usd"], 0.1);
    assert_eq!(c2["overlaps"], json!({ "lib.rs": [a3.id] }));
    assert!(c2["run"].is_null());
    let c3 = get(a3.id);
    assert_eq!(c3["files_changed"], 2);
    assert_eq!(c3["overlaps"], json!({ "lib.rs": [a2.id] }));
    assert_eq!(c3["run"]["command"], json!(["agent", "go"]));
    assert_eq!(c3["run"]["check_command"], "make test");
    // Registered but never started: no process.
    assert_eq!(c3["run"]["alive"], false);

    // Nothing to stop: no run, or a run without a live process.
    for id in [a2.id, a3.id] {
        let (s, v) = send(&app, "POST", &format!("/api/attempts/a{id}/stop")).await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert_eq!(v["error"]["code"], "no_active_run");
    }
    let (s, v) = send(&app, "POST", "/api/attempts/a99/stop").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "not_found");
}
