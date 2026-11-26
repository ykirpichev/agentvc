//! Review endpoints: stats, inbox, compare and the write actions.

use std::fs;
use std::path::Path;

use agentvc_core::{ForkRequest, Ref, Space, Status};
use agentvcd::{AppState, Config};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Task t1 with three attempts forked from a1:
/// a2 passed (lib.rs -> 2, adds new.txt, a passing check costing $0.25),
/// a3 passed (lib.rs -> 2 too, edits keep.txt, $0.50 as a string),
/// a4 failed (lib.rs -> 3).
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path();
    fs::write(w.join("lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
    fs::write(w.join("keep.txt"), "a\nb\nc\n").unwrap();
    let space = Space::init(w).unwrap();
    let task = space.create_task("make f return 2", None).unwrap();
    let fork = |actor: &str| {
        space
            .fork(&ForkRequest {
                task_id: Some(task.id),
                from: Some(Ref::Attempt(1)),
                actor: actor.into(),
                model: Some("model-x".into()),
                prompt: None,
                ..Default::default()
            })
            .unwrap()
            .attempt
    };
    let (a2, a3, a4) = (fork("agent:one"), fork("agent:two"), fork("agent:three"));
    fs::write(a2.view_path.join("lib.rs"), "pub fn f() -> u32 { 2 }\n").unwrap();
    fs::write(a2.view_path.join("new.txt"), "hello\n").unwrap();
    space
        .snapshot_with(
            a2.id,
            "run-exit",
            Some(
                json!({ "exit_code": 0, "check_exit_code": 0, "duration_ms": 1500,
                         "cost_usd": 0.25, "summary": "Returned 2." }),
            ),
            true,
        )
        .unwrap();
    space.set_status(a2.id, Status::Passed, None).unwrap();
    fs::write(a3.view_path.join("lib.rs"), "pub fn f() -> u32 { 2 }\n").unwrap();
    fs::write(a3.view_path.join("keep.txt"), "a\nB\nc\nd\n").unwrap();
    // `agentvc snapshot --meta` stores strings; stats still read them.
    space
        .snapshot(a3.id, "tool", Some(json!({ "cost_usd": "0.5" })))
        .unwrap();
    space.set_status(a3.id, Status::Passed, None).unwrap();
    fs::write(a4.view_path.join("lib.rs"), "pub fn f() -> u32 { 3 }\n").unwrap();
    space.snapshot(a4.id, "tool", None).unwrap();
    space.set_status(a4.id, Status::Failed, None).unwrap();
    tmp
}

fn app(dir: &Path) -> axum::Router {
    let mut config = Config::new(dir);
    config.actor = agentvcd::human_actor(Some("Ana"));
    agentvcd::router(AppState::start(&config).unwrap())
}

/// Actors of the attempt status operations, oldest first.
fn status_actors(dir: &Path) -> Vec<String> {
    let db = rusqlite::Connection::open(dir.join(".agentvc/meta.db")).unwrap();
    let mut q = db
        .prepare("SELECT actor FROM operations WHERE kind LIKE 'attempt.%' AND kind != 'attempt.fork' ORDER BY id")
        .unwrap();
    q.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
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

#[tokio::test]
async fn stats_from_snapshot_meta() {
    let tmp = fixture();
    let app = app(tmp.path());
    let (s, v) = send(&app, "GET", "/api/tasks/t1/stats", None).await;
    assert_eq!(s, StatusCode::OK);
    let a2 = v
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["attempt"] == 2)
        .unwrap();
    assert_eq!(a2["check"]["passed"], true);
    assert_eq!(a2["cost_usd"], 0.25);
    assert_eq!(a2["run_ms"], 1500);
    assert_eq!(a2["summary"], "Returned 2.");
    let a3 = v
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["attempt"] == 3)
        .unwrap();
    assert!(a3["check"].is_null());
    assert_eq!(a3["cost_usd"], 0.5);
    let a4 = v
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["attempt"] == 4)
        .unwrap();
    assert!(a4["cost_usd"].is_null());
}

#[tokio::test]
async fn stats_and_graph_carry_checks_and_annotations() {
    let tmp = fixture();
    {
        let space = Space::discover(tmp.path()).unwrap();
        space
            .annotate(
                3,
                &agentvc_core::annotate::AnnotateRequest {
                    actor: "agent:two".into(),
                    cost_usd: Some(0.25),
                    tokens_in: Some(100),
                    tokens_out: Some(20),
                    note: Some("kept keep.txt tidy".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let opts = |cmd: &str| agentvc_core::check::CheckOptions {
            actor: "agent:two".into(),
            name: "tests".into(),
            command: cmd.into(),
            junit: vec![],
            force: false,
            mark: false,
        };
        space
            .run_check(
                3,
                &opts("echo 'test result: ok. 4 passed; 0 failed; 0 ignored'"),
            )
            .unwrap();
        // A later change makes the result stale.
        let v = space.attempt(3).unwrap().view_path;
        fs::write(v.join("more.txt"), "x\n").unwrap();
        space.snapshot(3, "tool", None).unwrap();
    }
    let app = app(tmp.path());
    let (_, v) = send(&app, "GET", "/api/tasks/t1/stats", None).await;
    let a3 = v
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["attempt"] == 3)
        .unwrap();
    assert_eq!(a3["cost_usd"], 0.75);
    assert_eq!(a3["tokens"], 120);
    assert_eq!(a3["check"]["passed"], true);
    assert_eq!(a3["check"]["stale"], true);
    assert_eq!(a3["check"]["name"], "tests");
    assert_eq!(a3["agent"]["notes"][0]["text"], "kept keep.txt tidy");

    let (_, g) = send(&app, "GET", "/api/graph", None).await;
    let g3 = g["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["attempt"]["id"] == 3)
        .unwrap();
    assert_eq!(g3["tests"]["passed"], true);
    assert_eq!(g3["tests"]["stale"], true);
    assert!(g3["tests"]["summary"]
        .as_str()
        .unwrap()
        .contains("4 passed"));
    assert_eq!(g3["agent"]["cost_usd"], 0.25);
    let g4 = g["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["attempt"]["id"] == 4)
        .unwrap();
    assert!(g4.get("tests").is_none() && g4.get("agent").is_none());
}

#[tokio::test]
async fn inbox_lists_tasks_waiting_for_a_pick() {
    let tmp = fixture();
    let app = app(tmp.path());
    let (s, v) = send(&app, "GET", "/api/inbox", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["task"]["goal"], "make f return 2");
    assert_eq!(v[0]["reasons"], json!(["ready_to_pick", "failed"]));
    assert_eq!(v[0]["best_check"], "passed");
    assert_eq!(v[0]["candidates"], json!([2, 3]));
    assert_eq!(v[0]["cost_usd"], 0.75);

    // Once a winner is picked, the task leaves the inbox.
    let (s, _) = send(&app, "POST", "/api/attempts/a2/promote", None).await;
    assert_eq!(s, StatusCode::OK);
    let (_, v) = send(&app, "GET", "/api/inbox", None).await;
    assert_eq!(v, json!([]));
}

#[tokio::test]
async fn compare_unions_files_against_the_common_base() {
    let tmp = fixture();
    let app = app(tmp.path());
    let (s, v) = send(&app, "GET", "/api/compare?attempts=a2,a3,a4", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["base"]["attempt_id"], 1);
    let cols = v["columns"].as_array().unwrap();
    assert_eq!(cols.len(), 3);
    assert_eq!(cols[0]["attempt"]["id"], 2);
    assert_eq!(cols[0]["files_changed"], 2);
    assert_eq!(cols[0]["stats"]["check"]["passed"], true);
    assert_eq!(cols[1]["lines_added"], 1 + 2);
    assert_eq!(cols[1]["lines_removed"], 1 + 1);

    let files: Vec<&str> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    // All three differ on lib.rs and keep.txt and new.txt; none agree.
    assert_eq!(files, ["keep.txt", "lib.rs", "new.txt"]);
    let lib = &v["files"][1];
    assert_eq!(lib["same"], false);
    assert_eq!(lib["cells"][0]["kind"], "modified");
    assert_eq!(lib["cells"][0]["oid"], lib["cells"][1]["oid"]);
    let new = &v["files"][2];
    assert_eq!(new["cells"][0]["kind"], "added");
    assert_eq!(new["cells"][1]["kind"], "unchanged");

    // Two attempts that agree on lib.rs: it is marked same and sorts last.
    let (_, v) = send(&app, "GET", "/api/compare?attempts=a2,a3", None).await;
    let last = v["files"].as_array().unwrap().last().unwrap();
    assert_eq!(last["path"], "lib.rs");
    assert_eq!(last["same"], true);
}

#[tokio::test]
async fn compare_rejects_bad_selections() {
    let tmp = fixture();
    let app = app(tmp.path());
    for q in ["a2", "a2,a2", "a2,a3,a4,a1,a5", "a2,zz"] {
        let (s, v) = send(&app, "GET", &format!("/api/compare?attempts={q}"), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{q}");
        assert!(v["error"]["code"].is_string(), "{q}");
    }
    let (s, v) = send(&app, "GET", "/api/compare?attempts=a2,a99", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "not_found");
}

#[tokio::test]
async fn actions_change_status() {
    let tmp = fixture();
    let app = app(tmp.path());

    let (s, v) = send(
        &app,
        "POST",
        "/api/attempts/a4/abandon",
        Some(json!({ "reason": " " })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["code"], "reason_required");
    let (s, v) = send(&app, "POST", "/api/attempts/a4/abandon", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["code"], "bad_body");
    let (s, v) = send(
        &app,
        "POST",
        "/api/attempts/a4/abandon",
        Some(json!({ "reason": "wrong value" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["attempt"]["status"], "abandoned");
    assert_eq!(v["attempt"]["reason"], "wrong value");

    let (s, v) = send(
        &app,
        "POST",
        "/api/attempts/a3/mark",
        Some(json!({ "status": "promoted" })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["code"], "bad_status");
    let (s, v) = send(
        &app,
        "POST",
        "/api/attempts/a3/mark",
        Some(json!({ "status": "failed" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["attempt"]["status"], "failed");

    let (s, v) = send(&app, "POST", "/api/attempts/a2/promote", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["attempt"]["status"], "promoted");
    assert_eq!(v["closed_siblings"], json!([3]));

    // The fixture's CLI-style changes are the person in the checkout (from
    // git config); the three UI actions above carry the reviewer's name.
    let actors = status_actors(tmp.path());
    assert_eq!(&actors[actors.len() - 3..], ["human:ana"; 3]);
    assert!(actors[..actors.len() - 3]
        .iter()
        .all(|a| a.starts_with("human") && a != "human:ana"));

    // Closed attempts cannot change again.
    let (s, v) = send(&app, "POST", "/api/attempts/a2/promote", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"]["code"], "attempt_closed");

    // Writes are refused from foreign origins like every other request.
    let req = Request::post("/api/attempts/a1/mark")
        .header("host", "localhost:7420")
        .header("origin", "https://evil.example")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"status":"failed"}"#))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}
