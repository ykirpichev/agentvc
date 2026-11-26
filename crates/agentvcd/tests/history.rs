//! GET /api/query and /api/blame.

use std::fs;

use agentvc_core::view::BackendChoice;
use agentvc_core::{ForkRequest, Space};
use agentvcd::{AppState, Config};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .uri(uri)
        .header("host", "localhost:7420")
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
async fn query_and_blame() {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path();
    fs::write(w.join("x.py"), "a\nb\n").unwrap();
    let space = Space::init(w).unwrap();
    let task = space.create_task("t", None).unwrap();
    let a = space
        .fork(&ForkRequest {
            task_id: Some(task.id),
            actor: "agent:claude".into(),
            backend: BackendChoice::Copy,
            ..Default::default()
        })
        .unwrap()
        .attempt;
    fs::write(a.view_path.join("x.py"), "a\nB\n").unwrap();
    space
        .snapshot(
            a.id,
            "tool:Edit",
            Some(serde_json::json!({ "tests": "pass" })),
        )
        .unwrap();
    drop(space);
    let app = agentvcd::router(AppState::start(&Config::new(w)).unwrap());

    let (st, v) = get(&app, "/api/query?task=t1&touched=x.py&meta=tests=pass").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v[0]["id"], a.id);
    assert_eq!(v[0]["touched"][0], "x.py");
    let (_, v) = get(&app, "/api/query?status=failed").await;
    assert_eq!(v.as_array().unwrap().len(), 0);
    let (st, v) = get(&app, "/api/query?status=nope").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "bad_query");

    let (st, v) = get(&app, &format!("/api/blame?attempt=a{}&path=x.py", a.id)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["lines"][1]["text"], "B");
    assert_eq!(v["lines"][1]["origin"]["kind"], "snapshot");
    assert_eq!(v["lines"][1]["origin"]["actor"], "agent:claude");
}
