//! `/api/workspaces`, cross-workspace stack edits and their events.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use agentvc_core::stack::Window;
use agentvc_core::workspace::{set_activity, Activity};
use agentvc_core::Space;
use agentvcd::{AppState, Config};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// The daemon token these tests run with.
const TOKEN: &str = "test-token";

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repo `main` (with an agentvc space) and a linked worktree `w2` whose
/// stack is b1 <- b2 <- b3.
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    // Resolve symlinks (macOS /var) but keep a path git accepts on Windows
    // (no \\?\ prefix).
    let root = tmp.path().canonicalize().unwrap();
    let root = PathBuf::from(
        root.to_string_lossy()
            .strip_prefix(r"\\?\")
            .map(str::to_string)
            .unwrap_or_else(|| root.to_string_lossy().into_owned()),
    );
    let main = root.join("main");
    fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    // Stack commands commit with the repo's identity (CI has none).
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["config", "user.email", "t@t"]);
    fs::write(main.join("a.txt"), "a\n").unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "init"]);
    let w2 = root.join("w2");
    git(
        &main,
        &["worktree", "add", "-q", "--detach", w2.to_str().unwrap()],
    );
    let mut w = Window::open(&w2).unwrap();
    for n in ["b1", "b2", "b3"] {
        fs::write(w2.join(format!("{n}.txt")), format!("{n}\n")).unwrap();
        w.create(Some(n), Some(n), true).unwrap();
    }
    Space::init(&main).unwrap();
    (tmp, main, w2)
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
        .header("x-agentvc-token", TOKEN)
        .header("content-type", "application/json")
        .body(match body {
            Some(b) => Body::from(b.to_string()),
            None => Body::empty(),
        })
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
async fn list_edit_queue_and_events() {
    let (_tmp, main, w2) = fixture();
    let mut config = Config::new(&main).with_token(TOKEN);
    config.poll_interval = Duration::from_millis(50);
    let app = agentvcd::router(AppState::start(&config).unwrap());

    let (s, v) = send(&app, "GET", "/api/workspaces", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let ws = v["workspaces"].as_array().unwrap();
    assert_eq!(ws.len(), 2);
    assert_eq!(ws[0]["kind"], "main");
    assert_eq!(ws[1]["name"], "w2");
    assert_eq!(ws[1]["state"], "idle");
    let b = &ws[1]["branches"];
    assert_eq!(b.as_array().unwrap().len(), 3);
    assert_eq!(b[2]["name"], "b3");
    assert_eq!(b[2]["parent"], "b2");
    assert_eq!(b[2]["workspace"], ws[1]["id"]);
    assert_eq!(b[2]["behind"], 0);
    let w2id = ws[1]["id"].as_str().unwrap().to_string();

    // Idle: applied at once.
    let (s, v) = send(
        &app,
        "POST",
        &format!("/api/workspaces/{w2id}/stack"),
        Some(json!({ "op": "move", "branch": "b3", "onto": "main" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["outcome"], "applied");
    let w = Window::open(&w2).unwrap();
    assert_eq!(
        w.state
            .branches
            .iter()
            .find(|b| b.name == "b3")
            .unwrap()
            .parent,
        "main"
    );

    // Unknown workspace / request.
    let (s, v) = send(
        &app,
        "POST",
        "/api/workspaces/nope/stack",
        Some(json!({ "op": "sync" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["error"]["code"], "no_workspace");
    let (s, _) = send(&app, "POST", "/api/requests/r0/cancel", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Events: an agent in w2 makes the next edit wait; stopping applies it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = app.clone();
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
    let (mut sock, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/api/events"))
            .await
            .unwrap();
    let mut next = async || -> Value {
        let msg = tokio::time::timeout(Duration::from_secs(10), sock.next())
            .await
            .expect("timed out waiting for event")
            .unwrap()
            .unwrap();
        serde_json::from_str(msg.to_text().unwrap()).unwrap()
    };
    assert_eq!(next().await["type"], "hello");
    set_activity(&w2, Activity::AgentRunning, Some("agent:test")).unwrap();
    let (s, v) = send(
        &app,
        "POST",
        "/api/workspaces/w2/stack",
        Some(json!({ "op": "rename", "branch": "b2", "new_name": "b2x" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["outcome"], "queued");
    let rid = v["request_id"].as_str().unwrap().to_string();
    loop {
        let v = next().await;
        if v["type"] == "request_queued" {
            assert_eq!(v["request_id"], rid.as_str());
            assert_eq!(v["workspace"], w2id.as_str());
            break;
        }
    }
    set_activity(&w2, Activity::Idle, None).unwrap();
    let mut seen = (false, false);
    while !(seen.0 && seen.1) {
        let v = next().await;
        match v["type"].as_str().unwrap() {
            "request_applied" => {
                assert_eq!(v["request_id"], rid.as_str());
                seen.0 = true;
            }
            "stack_changed" if v["workspace"] == w2id.as_str() => seen.1 = true,
            _ => {}
        }
    }
    assert!(Window::open(&w2).unwrap().tracks("b2x"));

    // Cancelling an applied request is refused.
    let (s, v) = send(&app, "POST", &format!("/api/requests/{rid}/cancel"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"]["code"], "request_not_queued");
}

/// A branch's diff reads files at its base and tip, commits no rebase
/// recorded; anything that is not a commit id stays not found.
#[tokio::test]
async fn files_at_a_branch_base_and_tip() {
    let (_tmp, main, _w2) = fixture();
    let config = Config::new(&main).with_token(TOKEN);
    let app = agentvcd::router(AppState::start(&config).unwrap());

    let (_, v) = send(&app, "GET", "/api/workspaces", None).await;
    let w2id = v["workspaces"][1]["id"].as_str().unwrap().to_string();
    let (s, d) = send(
        &app,
        "GET",
        &format!("/api/workspaces/{w2id}/branches/b2"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{d}");
    let (base, tip) = (d["base"].as_str().unwrap(), d["tip"].as_str().unwrap());

    // b1.txt is there at b2's base (b1's tip); b2.txt only at its tip.
    let (s, f) = send(
        &app,
        "GET",
        &format!("/api/file?ref=git:{base}&path=b1.txt"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{f}");
    assert_eq!(f["content"], "b1\n");
    let (s, _) = send(
        &app,
        "GET",
        &format!("/api/file?ref=git:{base}&path=b2.txt"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, f) = send(
        &app,
        "GET",
        &format!("/api/file?ref=git:{tip}&path=b2.txt"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{f}");
    assert_eq!(f["content"], "b2\n");

    for bad in [
        "git:--all",
        "git:HEAD",
        "git:0000000000000000000000000000000000000000",
    ] {
        let (s, v) = send(
            &app,
            "GET",
            &format!("/api/file?ref={bad}&path=b1.txt"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{bad}: {v}");
    }
}
