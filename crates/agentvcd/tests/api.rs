use std::fs;
use std::path::Path;
use std::time::Duration;

use agentvc_core::view::BackendChoice;
use agentvc_core::{ForkRequest, Ref, Space, Status};
use agentvcd::{AppState, Config};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

/// A space with task t1, attempts a2 (passed, edits lib.rs, adds new.txt)
/// and a3 (exploring, untouched).
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path();
    fs::write(w.join("lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
    fs::create_dir_all(w.join("src/deep")).unwrap();
    fs::write(w.join("src/deep/x.txt"), "x\n").unwrap();
    fs::write(w.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
    let space = Space::init(w).unwrap();
    let task = space
        .create_task("make f return 2", Some("tests pass"))
        .unwrap();
    let fork = |actor: &str| {
        space
            .fork(&ForkRequest {
                task_id: Some(task.id),
                from: Some(Ref::Attempt(1)),
                actor: actor.into(),
                model: Some("model-x".into()),
                prompt: Some("do it".into()),
                // Plain directories: overlay mounts would outlive the tempdir.
                backend: BackendChoice::Copy,
            })
            .unwrap()
            .attempt
    };
    let a2 = fork("agent:one");
    fork("agent:two");
    fs::write(a2.view_path.join("lib.rs"), "pub fn f() -> u32 { 2 }\n").unwrap();
    fs::write(a2.view_path.join("new.txt"), "hello\n").unwrap();
    space.snapshot(a2.id, "tool", None).unwrap();
    space.set_status(a2.id, Status::Passed, None).unwrap();
    tmp
}

fn app(dir: &Path) -> axum::Router {
    agentvcd::router(AppState::start(&Config::new(dir)).unwrap())
}

async fn call(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::get(uri)
        .header("host", "localhost:7420")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, v)
}

#[tokio::test]
async fn tasks_and_attempts() {
    let tmp = fixture();
    let app = app(tmp.path());

    let (s, v) = call(&app, "/api/space").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["format_version"], 1);

    let (s, v) = call(&app, "/api/tasks").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v[0]["goal"], "make f return 2");
    assert_eq!(v[0]["attempts"]["total"], 2);
    assert_eq!(v[0]["attempts"]["passed"], 1);
    assert_eq!(v[0]["attempts"]["exploring"], 1);

    let (_, v) = call(&app, "/api/tasks/t1").await;
    assert_eq!(v["task"]["criteria"], "tests pass");
    assert_eq!(v["attempts"].as_array().unwrap().len(), 2);

    let (_, v) = call(&app, "/api/attempts?task=1").await;
    let ids: Vec<i64> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![2, 3]);
    let (_, v) = call(&app, "/api/attempts").await;
    assert_eq!(v.as_array().unwrap().len(), 3);

    let (_, v) = call(&app, "/api/attempts/a2").await;
    assert_eq!(v["attempt"]["status"], "passed");
    assert_eq!(v["attempt"]["model"], "model-x");
    assert!(v["snapshot_count"].as_u64().unwrap() >= 1);
    let (_, v) = call(&app, "/api/attempts/1").await;
    assert_eq!(v["children"], serde_json::json!([2, 3]));

    let (_, v) = call(&app, "/api/attempts/a2/snapshots").await;
    assert_eq!(v[0]["attempt_id"], 2);

    let (s, v) = call(&app, "/api/overview").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["schema"], "agentvc.overview/1");
    assert_eq!(v["tasks"][0]["id"], "t1");
    assert_eq!(v["tasks"][0]["state"], "open");
    assert_eq!(v["tasks"][0]["attempts"][0]["id"], "a2");
    assert_eq!(v["tasks"][0]["attempts"][0]["changes"]["added"], 1);
    assert_eq!(v["space"]["counts"]["total"], 2);
}

#[tokio::test]
async fn diff_and_files() {
    let tmp = fixture();
    let app = app(tmp.path());

    let (s, v) = call(&app, "/api/diff?a=a1&b=a2").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["changes"],
        serde_json::json!([
            {"path": "lib.rs", "kind": "modified"},
            {"path": "new.txt", "kind": "added"},
        ])
    );
    // Without `a`, the base is the snapshot the attempt was forked from.
    let (_, v2) = call(&app, "/api/diff?b=a2").await;
    assert_eq!(v2["changes"], v["changes"]);
    let (_, v) = call(&app, "/api/diff?b=a3").await;
    assert_eq!(v["changes"], serde_json::json!([]));

    let (s, v) = call(&app, "/api/file?ref=a2&path=lib.rs").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["content"], "pub fn f() -> u32 { 2 }\n");
    assert_eq!(v["binary"], false);
    let (_, v) = call(&app, "/api/file?ref=a1&path=lib.rs").await;
    assert_eq!(v["content"], "pub fn f() -> u32 { 1 }\n");
    let (_, v) = call(&app, "/api/file?ref=a1&path=src/deep/x.txt").await;
    assert_eq!(v["content"], "x\n");
    let (_, v) = call(&app, "/api/file?ref=a1&path=blob.bin").await;
    assert_eq!(v["binary"], true);
    assert_eq!(v["content"], Value::Null);
    assert_eq!(v["size"], 4);
}

#[tokio::test]
async fn errors_have_stable_codes() {
    let tmp = fixture();
    let app = app(tmp.path());
    for (uri, status, code) in [
        ("/api/tasks/99", StatusCode::NOT_FOUND, "not_found"),
        ("/api/attempts/a99", StatusCode::NOT_FOUND, "not_found"),
        ("/api/attempts?task=7", StatusCode::NOT_FOUND, "not_found"),
        ("/api/diff?a=z..z&b=a2", StatusCode::BAD_REQUEST, "bad_ref"),
        ("/api/diff?b=a1", StatusCode::NOT_FOUND, "not_found"),
        (
            "/api/file?ref=a1&path=nope.rs",
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            "/api/file?ref=a1&path=src",
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        ("/api/tasks/abc", StatusCode::BAD_REQUEST, "bad_task"),
        ("/api/nope", StatusCode::NOT_FOUND, "no_route"),
    ] {
        let (s, v) = call(&app, uri).await;
        assert_eq!(s, status, "{uri}: {v}");
        assert_eq!(v["error"]["code"], code, "{uri}: {v}");
    }
}

#[tokio::test]
async fn rejects_foreign_host_and_origin() {
    let tmp = fixture();
    let app = app(tmp.path());
    for (host, origin) in [
        ("evil.example:7420", None),
        ("localhost:7420", Some("https://evil.example")),
    ] {
        let mut req = Request::get("/api/tasks").header("host", host);
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        let res = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
    let req = Request::get("/api/tasks")
        .header("host", "127.0.0.1:7420")
        .header("origin", "http://localhost:5173")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn spa_fallback_serves_html() {
    let tmp = fixture();
    let app = app(tmp.path());
    let req = Request::get("/tasks/1")
        .header("host", "localhost")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
}

#[tokio::test]
async fn websocket_streams_changes() {
    let tmp = fixture();
    let mut config = Config::new(tmp.path());
    config.poll_interval = Duration::from_millis(50);
    let app = agentvcd::router(AppState::start(&config).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/api/events"))
        .await
        .unwrap();
    let mut next = async || -> Value {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("timed out waiting for event")
            .unwrap()
            .unwrap();
        serde_json::from_str(msg.to_text().unwrap()).unwrap()
    };
    assert_eq!(next().await["type"], "hello");

    // Another process (here: another handle) changes the space.
    let other = Space::discover(tmp.path()).unwrap();
    let a3 = other.attempt(3).unwrap();
    fs::write(a3.view_path.join("lib.rs"), "pub fn f() -> u32 { 3 }\n").unwrap();
    let snap = other.snapshot(3, "tool", None).unwrap().snapshot;
    other
        .set_status(3, Status::Failed, Some("tests red"))
        .unwrap();

    let mut seen = Vec::new();
    while seen.len() < 2 {
        let v = next().await;
        match v["type"].as_str().unwrap() {
            "snapshot_created" if v["snapshot"]["id"] == snap.id => seen.push("snapshot"),
            "attempt_updated" => {
                assert_eq!(v["attempt"]["id"], 3);
                assert_eq!(v["attempt"]["status"], "failed");
                assert_eq!(v["previous_status"], "exploring");
                seen.push("status");
            }
            _ => {}
        }
    }
}
