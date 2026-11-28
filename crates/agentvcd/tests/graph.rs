//! The branch graph, undo and the PR action the review UI's graph uses.

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

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A git repo with a space, attempt a2 (edits lib.rs, and README.md when
/// `conflict`), and a newer commit on main that edits README.md and adds
/// NEWS.md. Returns the dir and the new main commit.
fn fixture(conflict: bool) -> (tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path();
    git(w, &["init", "-q", "-b", "main"]);
    fs::write(w.join("lib.rs"), "1\n").unwrap();
    fs::write(w.join("README.md"), "readme\n").unwrap();
    git(w, &["add", "-A"]);
    git(w, &["commit", "-q", "-m", "base"]);
    let space = Space::init(w).unwrap();
    let task = space.create_task("bump", None).unwrap();
    let a = space
        .fork(&ForkRequest {
            task_id: Some(task.id),
            actor: "agent:one".into(),
            backend: BackendChoice::Copy,
            ..Default::default()
        })
        .unwrap()
        .attempt;
    fs::write(a.view_path.join("lib.rs"), "2\n").unwrap();
    if conflict {
        fs::write(a.view_path.join("README.md"), "readme from a2\n").unwrap();
    }
    space.snapshot(a.id, "tool", None).unwrap();
    fs::write(w.join("README.md"), "readme from main\n").unwrap();
    fs::write(w.join("NEWS.md"), "news\n").unwrap();
    git(w, &["add", "-A"]);
    git(w, &["commit", "-q", "-m", "main moves on"]);
    let main = git(w, &["rev-parse", "HEAD"]);
    (tmp, main)
}

fn app(dir: &Path) -> axum::Router {
    let mut config = Config::new(dir).with_token(TOKEN);
    config.actor = agentvcd::human_actor(Some("Ana"));
    agentvcd::router(AppState::start(&config).unwrap())
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "localhost:7420")
        .header("x-agentvc-token", TOKEN);
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, v)
}

#[tokio::test]
async fn graph_hangs_attempts_off_main_and_counts_behind() {
    let (tmp, main) = fixture(true);
    let app = app(tmp.path());

    let (s, g) = send(&app, "GET", "/api/graph", None).await;
    assert_eq!(s, StatusCode::OK, "{g}");
    // No remote: the trunk follows HEAD, newest first, down to the base.
    assert_eq!(g["upstream"], "HEAD");
    let trunk = g["trunk"].as_array().unwrap();
    assert_eq!(trunk.len(), 2, "{g}");
    assert_eq!(trunk[0]["commit"], main);
    assert_eq!(trunk[0]["summary"], "main moves on");
    assert_eq!(trunk[1]["summary"], "base");
    assert_eq!(trunk[1]["on_upstream"], true);

    let a2 = g["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["attempt"]["id"] == 2)
        .unwrap()
        .clone();
    assert_eq!(a2["base_commit"], trunk[1]["commit"]);
    assert_eq!(a2["behind"], 1);
    assert_eq!(a2["conflicts"], 0);
    assert!(a2["pr"].is_null());

    // A rebase that keeps conflicts shows up as open conflicts, based on main.
    let (s, v) = send(
        &app,
        "POST",
        "/api/attempts/a2/rebase",
        Some(json!({ "onto": "main", "keep_conflicts": true, "no_auto_resolve": true })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, g) = send(&app, "GET", "/api/graph", None).await;
    let a2 = &g["attempts"].as_array().unwrap()[1];
    assert_eq!(a2["attempt"]["id"], 2);
    assert_eq!(a2["base_commit"], main);
    assert_eq!(a2["behind"], 0);
    assert_eq!(a2["conflicts"], 1, "{g}");
    assert_eq!(g["trunk"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn undo_reverses_the_newest_operation() {
    let (tmp, _) = fixture(false);
    let app = app(tmp.path());

    let (s, v) = send(&app, "POST", "/api/attempts/2/promote", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["attempt"]["status"], "promoted");

    let (s, u) = send(&app, "POST", "/api/undo", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{u}");
    assert!(u["op"].as_i64().unwrap() > 0);
    let (_, v) = send(&app, "GET", "/api/attempts/2", None).await;
    assert_eq!(v["attempt"]["status"], "exploring");

    // Undoing the undo redoes it.
    let (s, _) = send(&app, "POST", "/api/undo", Some(json!({ "op": u["op"] }))).await;
    assert_eq!(s, StatusCode::OK);
    let (_, v) = send(&app, "GET", "/api/attempts/2", None).await;
    assert_eq!(v["attempt"]["status"], "promoted");

    let (s, v) = send(&app, "POST", "/api/undo", Some(json!({ "op": 9999 }))).await;
    assert!(s.is_client_error(), "{s} {v}");
    assert!(v["error"]["code"].is_string());
}

#[tokio::test]
async fn pr_without_a_remote_fails_with_a_code() {
    let (tmp, _) = fixture(false);
    let app = app(tmp.path());
    let (s, v) = send(&app, "POST", "/api/attempts/2/pr", Some(json!({}))).await;
    assert!(!s.is_success(), "{v}");
    assert!(v["error"]["code"].is_string(), "{v}");
    let (s, v) = send(
        &app,
        "POST",
        "/api/attempts/2/pr",
        Some(json!({ "bogus": 1 })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
}
