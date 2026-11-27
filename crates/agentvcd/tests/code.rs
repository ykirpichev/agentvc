//! Code browsing (#171): `/api/tree`, `/api/search`, and the browse refs in
//! `/api/file` and `/api/blame`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use agentvc_core::stack::Window;
use agentvc_core::view::BackendChoice;
use agentvc_core::{ForkRequest, Ref, Space};
use agentvcd::{AppState, Config};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

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

/// A repo `main` (with a space) holding `a.txt` and `src/lib.rs`, and a
/// linked worktree `w2` whose stack is `feat` (adds `src/new.rs`, edits
/// `a.txt`).
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let root = PathBuf::from(
        root.to_string_lossy()
            .strip_prefix(r"\\?\")
            .map(str::to_string)
            .unwrap_or_else(|| root.to_string_lossy().into_owned()),
    );
    let main = root.join("main");
    fs::create_dir_all(main.join("src")).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["config", "user.email", "t@t"]);
    fs::write(main.join("a.txt"), "alpha\nbeta\n").unwrap();
    fs::write(main.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
    fs::write(main.join(".gitignore"), "target/\n").unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "init"]);
    let w2 = root.join("w2");
    git(
        &main,
        &["worktree", "add", "-q", "--detach", w2.to_str().unwrap()],
    );
    let mut w = Window::open(&w2).unwrap();
    fs::write(w2.join("src/new.rs"), "fn fresh() {}\n").unwrap();
    fs::write(w2.join("a.txt"), "alpha\nBETA\n").unwrap();
    w.create(Some("feat"), Some("feat"), true).unwrap();
    Space::init(&main).unwrap();
    (tmp, main, w2)
}

async fn call(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::get(uri)
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

fn enc(s: &str) -> String {
    s.replace('/', "%2F").replace(':', "%3A")
}

fn names(v: &Value) -> Vec<String> {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let mark = e["change"]
                .as_str()
                .map(|c| format!(" {c}"))
                .unwrap_or_default();
            format!("{}{}", e["name"].as_str().unwrap(), mark)
        })
        .collect()
}

#[tokio::test]
async fn tree_search_file_and_blame_over_every_ref_kind() {
    let (_tmp, main, w2) = fixture();
    let app = agentvcd::router(AppState::start(&Config::new(&main)).unwrap());
    let (_, v) = call(&app, "/api/workspaces").await;
    let ws = v["workspaces"][1]["id"].as_str().unwrap().to_string();

    // main: the trunk commit.
    let (s, v) = call(&app, "/api/tree?ref=main").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["kind"], "commit");
    assert_eq!(v["label"], "main");
    assert_eq!(names(&v), ["src", ".gitignore", "a.txt"]);

    // A branch: its tip, marked against its parent.
    let feat = enc(&format!("branch:{ws}/feat"));
    let (s, v) = call(&app, &format!("/api/tree?ref={feat}&changed=auto")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["label"], "feat");
    assert!(v["changed_base"].as_str().unwrap().starts_with("git:"));
    assert_eq!(names(&v), ["src modified", ".gitignore", "a.txt modified"]);
    assert_eq!(v["changes"].as_array().unwrap().len(), 2);
    // Git spells Windows paths with forward slashes.
    let slash = |p: &str| p.replace('\\', "/");
    assert_eq!(
        slash(v["folder"].as_str().unwrap()),
        slash(&w2.to_string_lossy())
    );
    let (_, v) = call(&app, &format!("/api/tree?ref={feat}&path=src&changed=auto")).await;
    assert_eq!(names(&v), ["lib.rs", "new.rs added"]);

    // Every path, for go-to-file.
    let (_, v) = call(
        &app,
        &format!("/api/tree?ref={feat}&recursive=1&paths_only=1"),
    )
    .await;
    assert_eq!(
        v["paths"],
        serde_json::json!([".gitignore", "a.txt", "src/lib.rs", "src/new.rs"])
    );

    // Search.
    let (s, v) = call(&app, &format!("/api/search?ref={feat}&q=beta")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["matches"][0]["path"], "a.txt");
    assert_eq!(v["matches"][0]["line"], 2);
    let (_, v) = call(&app, &format!("/api/search?ref={feat}&q=beta&case=1")).await;
    assert_eq!(v["matches"].as_array().unwrap().len(), 0);
    let (_, v) = call(
        &app,
        &format!("/api/search?ref={feat}&q=fn%20%5Cw%2B&regex=1&path=src"),
    )
    .await;
    assert_eq!(v["files"], 2);
    let (s, v) = call(&app, &format!("/api/search?ref={feat}&q=(&regex=1")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    // The workspace's working tree: uncommitted edits, ignored files left out.
    fs::write(w2.join("src/lib.rs"), "pub fn answer() -> u32 { 43 }\n").unwrap();
    fs::create_dir_all(w2.join("target")).unwrap();
    fs::write(w2.join("target/out.bin"), "x").unwrap();
    let wt = enc(&format!("workspace:{ws}"));
    let (s, v) = call(&app, &format!("/api/file?ref={wt}&path=src/lib.rs")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["content"], "pub fn answer() -> u32 { 43 }\n");
    // Captures are reused for a few seconds; a new ref key misses the cache.
    let (_, v) = call(&app, &format!("/api/tree?ref={wt}&changed=auto")).await;
    assert_eq!(v["kind"], "worktree");
    assert!(!names(&v).iter().any(|n| n.starts_with("target")));
    let (_, v) = call(&app, &format!("/api/tree?ref={wt}&path=src&changed=auto")).await;
    assert_eq!(names(&v), ["lib.rs modified", "new.rs added"]);

    // Files at a branch ref, and diff bases accept the new refs.
    let (s, v) = call(&app, &format!("/api/file?ref={feat}&path=src/new.rs")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["content"], "fn fresh() {}\n");

    // Blame: git's, for commits and working trees.
    let (s, v) = call(&app, "/api/blame?ref=main&path=a.txt").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["lines"][1]["text"], "beta");
    assert_eq!(v["lines"][1]["origin"]["kind"], "git");
    assert_eq!(v["lines"][1]["origin"]["summary"], "init");
    let (_, v) = call(&app, &format!("/api/blame?ref={feat}&path=a.txt")).await;
    assert_eq!(v["lines"][1]["text"], "BETA");
    assert_eq!(v["lines"][1]["origin"]["summary"], "feat");
    let (_, v) = call(&app, &format!("/api/blame?ref={wt}&path=src/lib.rs")).await;
    assert_eq!(v["working"], true);
    assert_eq!(v["lines"][0]["origin"]["author"], "Not Committed Yet");

    // Versions: aN and sN.
    let space = Space::discover(&main).unwrap();
    let a = space
        .fork(&ForkRequest {
            task_id: None,
            from: Some(Ref::Attempt(1)),
            actor: "agent:x".into(),
            model: None,
            prompt: None,
            backend: BackendChoice::Copy,
        })
        .unwrap()
        .attempt;
    fs::write(a.view_path.join("v.txt"), "v\n").unwrap();
    space.snapshot(a.id, "tool", None).unwrap();
    let (s, v) = call(&app, &format!("/api/tree?ref=a{}&changed=auto", a.id)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["kind"], "snapshot");
    assert!(names(&v).contains(&"v.txt added".to_string()), "{v}");

    // The page itself: a path ending in a file name is still a UI route.
    let req = Request::get("/review/code/main/src/lib.rs")
        .header("host", "localhost:7420")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let req = Request::get("/missing.js")
        .header("host", "localhost:7420")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    // Errors carry codes.
    let (s, v) = call(&app, "/api/tree?ref=branch:nope/x").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["error"]["code"], "not_found");
    let (s, _) = call(&app, "/api/tree?ref=main&path=nope").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, v) = call(&app, "/api/tree?ref=branch:nobranch").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
}

#[tokio::test]
async fn a_10k_file_tree_lists_fast() {
    let (_tmp, main, _w2) = fixture();
    for d in 0..100 {
        let dir = main.join(format!("pkg{d:03}"));
        fs::create_dir_all(&dir).unwrap();
        for f in 0..100 {
            fs::write(dir.join(format!("f{f:03}.txt")), format!("{d} {f}\n")).unwrap();
        }
    }
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "many"]);
    let app = agentvcd::router(AppState::start(&Config::new(&main)).unwrap());
    // The first call copies the commit's tree into the store.
    let (s, v) = call(&app, "/api/tree?ref=main&recursive=1&paths_only=1").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["paths"].as_array().unwrap().len(), 10_003);
    let t = Instant::now();
    let (_, v) = call(&app, "/api/tree?ref=main&recursive=1&paths_only=1").await;
    let took = t.elapsed();
    assert_eq!(v["paths"].as_array().unwrap().len(), 10_003);
    // 200 ms in release builds; debug builds and CI machines get headroom.
    let budget = if cfg!(debug_assertions) { 3_000 } else { 200 };
    assert!(took.as_millis() < budget, "listing took {took:?}");
    eprintln!("10k paths listed in {took:?}");
}
