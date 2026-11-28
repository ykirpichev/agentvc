//! The take action: files from one attempt into another.

use std::fs;
use std::path::Path;

use agentvc_core::view::BackendChoice;
use agentvc_core::{ForkRequest, Space};
use agentvcd::{AppState, Config};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// The daemon token these tests run with.
const TOKEN: &str = "test-token";

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

async fn post(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", "localhost:7420")
        .header("x-agentvc-token", TOKEN)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
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
async fn take_a_file_then_conflict_answers_409() {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path();
    git(w, &["init", "-q", "-b", "main"]);
    fs::write(w.join("lib.rs"), "1\n").unwrap();
    fs::write(w.join("README.md"), "readme\n").unwrap();
    git(w, &["add", "-A"]);
    git(w, &["commit", "-q", "-m", "base"]);
    let space = Space::init(w).unwrap();
    let task = space.create_task("t", None).unwrap();
    let fork = || {
        space
            .fork(&ForkRequest {
                task_id: Some(task.id),
                actor: "agent:x".into(),
                backend: BackendChoice::Copy,
                ..Default::default()
            })
            .unwrap()
            .attempt
    };
    let (a, b) = (fork(), fork());
    fs::write(b.view_path.join("lib.rs"), "2\n").unwrap();
    fs::write(b.view_path.join("README.md"), "b\n").unwrap();
    space.snapshot(b.id, "tool", None).unwrap();
    fs::write(a.view_path.join("README.md"), "a\n").unwrap();
    drop(space);

    let mut config = Config::new(w).with_token(TOKEN);
    config.actor = agentvcd::human_actor(Some("Ana"));
    let app = agentvcd::router(AppState::start(&config).unwrap());

    let uri = format!("/api/attempts/a{}/take", a.id);
    let (st, v) = post(
        &app,
        &uri,
        json!({ "source": format!("a{}", b.id), "paths": ["lib.rs"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["take"]["taken"][0]["path"], "lib.rs");
    assert_eq!(
        v["take"]["snapshot"]["meta"]["source"],
        format!("a{}", b.id)
    );
    assert_eq!(
        fs::read_to_string(a.view_path.join("lib.rs")).unwrap(),
        "2\n"
    );

    let (st, v) = post(&app, &uri, json!({ "source": format!("a{}", b.id) })).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"]["code"], "conflict");
    assert_eq!(v["error"]["paths"], json!(["README.md"]));
    assert_eq!(
        fs::read_to_string(a.view_path.join("README.md")).unwrap(),
        "a\n"
    );

    let (st, v) = post(&app, &uri, json!({ "source": "a2", "paths": ["nope"] })).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
}
