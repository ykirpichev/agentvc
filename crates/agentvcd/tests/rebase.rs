//! The rebase action and diffs against a rebased-onto commit.

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
    let mut config = Config::new(dir);
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
        .header("host", "localhost:7420");
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

fn paths(diff: &Value) -> Vec<String> {
    diff["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn rebase_then_diff_shows_only_own_changes() {
    let (tmp, main) = fixture(false);
    let app = app(tmp.path());

    // Before: the attempt is based on the old commit and has no git_base.
    let (_, v) = send(&app, "GET", "/api/attempts/2", None).await;
    assert!(v["git_base"].is_null());
    assert_ne!(v["base_commit"], main);

    let (s, v) = send(
        &app,
        "POST",
        "/api/attempts/a2/rebase",
        Some(json!({ "onto": "main" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["rebase"]["onto"], main);
    assert_eq!(v["rebase"]["rebased"], true);
    assert_eq!(v["attempt"]["id"], 2);

    let (_, v) = send(&app, "GET", "/api/attempts/2", None).await;
    assert_eq!(v["git_base"]["commit"], main);
    assert_eq!(v["base_commit"], main);

    // Default base is now the rebased-onto commit.
    let (s, d) = send(&app, "GET", "/api/diff?b=a2", None).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(d["a_ref"], format!("git:{main}"));
    assert!(d["a"].is_null());
    assert_eq!(paths(&d), ["lib.rs"]);

    // The same base, asked for explicitly, and its files.
    let (_, d2) = send(&app, "GET", &format!("/api/diff?a=git:{main}&b=a2"), None).await;
    assert_eq!(paths(&d2), ["lib.rs"]);
    let (s, f) = send(
        &app,
        "GET",
        &format!("/api/file?ref=git:{main}&path=lib.rs"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{f}");
    assert_eq!(f["content"], "1\n");
    assert!(f["snapshot"].is_null());
    let (_, f) = send(
        &app,
        "GET",
        &format!("/api/file?ref=git:{main}&path=NEWS.md"),
        None,
    )
    .await;
    assert_eq!(f["content"], "news\n");

    // An unknown commit is a 404, not a 500.
    let (s, v) = send(&app, "GET", "/api/file?ref=git:0123&path=lib.rs", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["error"]["code"], "not_found");

    // Recorded as the daemon's actor.
    let db = rusqlite::Connection::open(tmp.path().join(".agentvc/meta.db")).unwrap();
    let actor: String = db
        .query_row(
            "SELECT actor FROM operations WHERE kind = 'attempt.rebase'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actor, "human:ana");
}

#[tokio::test]
async fn conflict_is_409_with_paths_and_keep_conflicts_reopens() {
    let (tmp, main) = fixture(true);
    let app = app(tmp.path());
    let (_, before) = send(&app, "GET", "/api/attempts/2/snapshots", None).await;

    let (s, v) = send(
        &app,
        "POST",
        "/api/attempts/2/rebase",
        Some(json!({ "onto": "main" })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"]["code"], "conflict");
    assert_eq!(v["error"]["paths"], json!(["README.md"]));
    assert_eq!(v["error"]["onto"], main);
    let (_, after) = send(&app, "GET", "/api/attempts/2/snapshots", None).await;
    assert_eq!(before, after);

    let (s, v) = send(
        &app,
        "POST",
        "/api/attempts/2/rebase",
        Some(json!({ "onto": "main", "keep_conflicts": true })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["rebase"]["conflicts"], json!(["README.md"]));
    assert_eq!(v["attempt"]["status"], "exploring");
    let (_, f) = send(&app, "GET", "/api/file?ref=a2&path=README.md", None).await;
    assert!(f["content"].as_str().unwrap().contains("<<<<<<<"), "{f}");

    // Root attempt and bad bodies are client errors.
    let (s, v) = send(&app, "POST", "/api/attempts/1/rebase", Some(json!({}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "root_attempt");
    let (s, v) = send(
        &app,
        "POST",
        "/api/attempts/2/rebase",
        Some(json!({ "ontoo": "x" })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "bad_body");
}
