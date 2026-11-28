//! `/api/comments` and `/api/reviews` (#167): comment on a branch's lines,
//! send the drafts as a review, hand it to the agent working on it.
// The agent here is an `sh` command.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentvc_core::stack::Window;
use agentvc_core::Space;
use agentvcd::{AppState, Config};
use axum::body::Body;
use axum::http::{Request, StatusCode};
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

/// A repo with origin/main and a stack branch `b1`.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let origin = root.join("origin.git");
    let main = root.join("main");
    fs::create_dir_all(&main).unwrap();
    git(
        &root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    git(&main, &["init", "-q", "-b", "main"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["config", "user.email", "t@t"]);
    let wts = root.join("wts");
    git(
        &main,
        &["config", "agentvc.agent-worktrees", wts.to_str().unwrap()],
    );
    fs::write(main.join("a.txt"), "a\n").unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "init"]);
    git(
        &main,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&main, &["push", "-q", "origin", "main"]);
    git(&main, &["fetch", "-q", "origin"]);
    let mut w = Window::open(&main).unwrap();
    fs::write(main.join("b1.txt"), "one\ntwo\nthree\n").unwrap();
    w.create(Some("b1"), Some("b1"), true).unwrap();
    Space::init(&main).unwrap();
    (tmp, main)
}

/// The environment is the process's, and the tests run in parallel: they
/// share one agentvc home (their repos are separate spaces in it).
fn shared_env() {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| tempfile::tempdir().unwrap());
    std::env::set_var("AGENTVC_HOME", home.path());
    std::env::set_var("AGENTVC_BIN", "true");
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    relayed: bool,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "localhost:7420")
        .header("x-agentvc-token", TOKEN)
        .header("content-type", "application/json");
    if relayed {
        req = req.header("x-agentvc-relay", "1");
    }
    let req = req
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
async fn comment_send_reply_resolve() {
    let (_tmp, main) = fixture();
    shared_env();
    let state = AppState::start(&Config::new(&main).with_token(TOKEN)).unwrap();
    let app = agentvcd::router(state);
    let (_, ws) = send(&app, "GET", "/api/workspaces", None, false).await;
    let wid = ws["workspaces"][0]["id"].as_str().unwrap().to_string();
    let target = format!("branch:{wid}/b1");

    let draft = json!({
        "target": target,
        "lines": { "path": "b1.txt", "side": "new", "start": 2, "end": 3 },
        "body": "two and three should go",
        "pending": true,
    });
    // The hosted service may read comments, not write them.
    let (s, v) = send(&app, "POST", "/api/comments", Some(draft.clone()), true).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, v) = send(&app, "POST", "/api/comments", Some(draft), false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let id = v["thread"]["id"].as_str().unwrap().to_string();
    assert_eq!(v["thread"]["state"], "pending");
    assert_eq!(v["thread"]["anchor"]["lines"], json!(["two", "three"]));
    assert_eq!(v["thread"]["current"], json!({ "start": 2, "end": 3 }));

    // Drafts only show when asked for.
    let uri = format!("/api/comments?branch={wid}/b1");
    let (_, v) = send(&app, "GET", &uri, None, false).await;
    assert_eq!(v["threads"], json!([]));
    let (_, v) = send(&app, "GET", &format!("{uri}&drafts=1"), None, false).await;
    assert_eq!(v["threads"].as_array().unwrap().len(), 1);

    let (s, v) = send(
        &app,
        "POST",
        "/api/reviews/submit",
        Some(json!({ "target": target, "to": "agent" })),
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["delivery"], "waiting", "no agent works on b1");
    assert_eq!(v["review"]["threads"], json!([id]));

    let (_, v) = send(&app, "GET", &format!("{uri}&state=open"), None, false).await;
    assert_eq!(v["threads"][0]["state"], "open");
    let (_, v) = send(&app, "GET", "/api/workspaces", None, false).await;
    let b1 = v["workspaces"][0]["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "b1")
        .unwrap()
        .clone();
    assert_eq!(b1["comments"], 1);

    let (s, v) = send(
        &app,
        "POST",
        &format!("/api/comments/{id}/reply"),
        Some(json!({ "body": "or keep two" })),
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["thread"]["comments"].as_array().unwrap().len(), 2);
    let (_, v) = send(
        &app,
        "POST",
        &format!("/api/comments/{id}/resolve"),
        None,
        false,
    )
    .await;
    assert_eq!(v["thread"]["state"], "resolved");
    let (_, v) = send(
        &app,
        "POST",
        &format!("/api/comments/{id}/reopen"),
        None,
        false,
    )
    .await;
    assert_eq!(v["thread"]["state"], "open");
    // A sent thread is not a draft any more.
    let (s, v) = send(&app, "DELETE", &format!("/api/comments/{id}"), None, false).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "not_a_draft");

    // No PR on the branch: nothing to sync with, and nothing to post to.
    let (s, v) = send(
        &app,
        "POST",
        "/api/comments/sync",
        Some(json!({ "target": target })),
        false,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "no_pr");

    let (s, v) = send(
        &app,
        "POST",
        "/api/comments",
        Some(json!({ "target": "b1", "body": "x" })),
        false,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "bad_target");
    let (s, v) = send(&app, "GET", "/api/comments/c99", None, false).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");

    // An agent that stopped gets a review on it as its next prompt.
    let (s, v) = send(
        &app,
        "POST",
        "/api/agents",
        Some(json!({
            "workspace": "main", "from": "b1", "prompt": "Add c",
            "command": "printf 'c1\\nc2\\n' > c.txt", "auto_land": false,
        })),
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let ag = v["agent"]["id"].as_str().unwrap().to_string();
    let dir = agentvc_core::agent::agent_dir(&main, &ag).unwrap();
    let a = agentvc_core::agent::work(&dir, Path::new("true")).unwrap();
    assert_eq!(a.state, agentvc_core::agent::AgentState::Ready);
    let (s, v) = send(
        &app,
        "POST",
        "/api/comments",
        Some(json!({
            "target": format!("agent:{ag}"),
            "lines": { "path": "c.txt", "start": 2 },
            "body": "c2 is wrong",
            "pending": true,
        })),
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = send(
        &app,
        "POST",
        "/api/reviews/submit",
        Some(json!({ "target": format!("agent:{ag}"), "to": "agent" })),
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["delivery"], "prompt", "{v}");
    assert_eq!(v["agent"]["state"], "queued");
    let prompt = v["agent"]["pending_reply"].as_str().unwrap();
    assert!(
        prompt.contains("c.txt:2\n    > c2\n  c2 is wrong"),
        "{prompt}"
    );
    assert!(v["review"]["delivered_via"] == "prompt");
}

#[tokio::test]
async fn a_review_sent_while_an_agent_works_starts_its_next_turn() {
    let (_tmp, main) = fixture();
    shared_env();
    let state = AppState::start(&Config::new(&main).with_token(TOKEN)).unwrap();
    let app = agentvcd::router(state);
    let (s, v) = send(
        &app,
        "POST",
        "/api/agents",
        Some(json!({
            "workspace": "main", "from": "b1", "prompt": "Add d",
            "command": "printf 'd\\n' > d.txt",
        })),
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let ag = v["agent"]["id"].as_str().unwrap().to_string();
    // The agent hasn't stopped yet: a comment on its start branch's file.
    let (s, v) = send(
        &app,
        "POST",
        "/api/comments",
        Some(json!({
            "target": format!("agent:{ag}"),
            "lines": { "path": "b1.txt", "start": 1 },
            "body": "rename one",
            "pending": true,
        })),
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, v) = send(
        &app,
        "POST",
        "/api/reviews/submit",
        Some(json!({ "target": format!("agent:{ag}"), "to": "agent" })),
        false,
    )
    .await;
    assert_eq!(v["delivery"], "queued", "{v}");

    // Its turn ends without the hook handing it over: the review is its next prompt.
    let dir = agentvc_core::agent::agent_dir(&main, &ag).unwrap();
    let a = agentvc_core::agent::work(&dir, Path::new("true")).unwrap();
    assert_eq!(a.state, agentvc_core::agent::AgentState::Queued, "{a:?}");
    let prompt = a.pending_reply.unwrap();
    assert!(
        prompt.contains("b1.txt:1\n    > one\n  rename one"),
        "{prompt}"
    );
    let (_, v) = send(
        &app,
        "GET",
        &format!("/api/comments?agent={ag}"),
        None,
        false,
    )
    .await;
    assert_eq!(v["threads"][0]["state"], "open");
}
