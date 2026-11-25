use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

fn agentvc(dir: &Path, args: &[&str]) -> Value {
    let out = Command::cargo_bin("agentvc")
        .unwrap()
        .arg("--json")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "bad json from {args:?}: {e}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn full_attempt_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path();
    fs::write(w.join("lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();

    let v = agentvc(w, &["init"]);
    assert_eq!(v["ok"], true);

    let v = agentvc(w, &["task", "new", "make f return 2"]);
    assert_eq!(v["task"]["id"], 1);

    let fork = |actor: &str| {
        let v = agentvc(
            w,
            &["fork", "--task", "t1", "--actor", actor, "--from", "a1"],
        );
        assert_eq!(v["ok"], true, "{v}");
        (
            v["fork"]["attempt"]["id"].as_i64().unwrap(),
            v["fork"]["attempt"]["view_path"]
                .as_str()
                .unwrap()
                .to_string(),
        )
    };
    let (a, view_a) = fork("agent:one");
    let (b, view_b) = fork("agent:two");

    fs::write(
        Path::new(&view_a).join("lib.rs"),
        "pub fn f() -> u32 { 2 }\n",
    )
    .unwrap();
    fs::write(
        Path::new(&view_b).join("lib.rs"),
        "pub fn f() -> u32 { 1 + 1 }\n",
    )
    .unwrap();
    fs::write(Path::new(&view_b).join("notes.md"), "tried arithmetic\n").unwrap();

    // Snapshot from inside the view: the attempt is detected from the directory.
    let v = agentvc(Path::new(&view_a), &["snapshot", "--meta", "tests=pass"]);
    assert_eq!(v["snapshot"]["attempt_id"], a);
    assert_eq!(v["snapshot"]["meta"]["tests"], "pass");
    agentvc(w, &["snapshot", &format!("a{b}")]);

    let v = agentvc(w, &["diff", &format!("a{a}"), &format!("a{b}")]);
    let paths: Vec<_> = v["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(paths, vec!["lib.rs", "notes.md"]);

    let v = agentvc(w, &["promote", &format!("a{a}")]);
    assert_eq!(v["attempt"]["status"], "promoted");
    assert_eq!(v["closed_siblings"], serde_json::json!([b]));

    let v = agentvc(w, &["log", "--task", "1"]);
    assert_eq!(v["attempts"].as_array().unwrap().len(), 2);

    // The root working directory never changed.
    assert_eq!(
        fs::read_to_string(w.join("lib.rs")).unwrap(),
        "pub fn f() -> u32 { 1 }\n"
    );
}

#[test]
fn errors_have_stable_codes() {
    let tmp = tempfile::tempdir().unwrap();
    let v = agentvc(tmp.path(), &["log"]);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "no_space");

    agentvc(tmp.path(), &["init"]);
    let v = agentvc(tmp.path(), &["show", "a99"]);
    assert_eq!(v["error"]["code"], "not_found");
    let v = agentvc(tmp.path(), &["init"]);
    assert_eq!(v["error"]["code"], "space_exists");
}

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

#[test]
fn export_writes_branch_without_touching_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path();
    git(w, &["init", "-q", "-b", "main"]);
    fs::write(w.join("lib.rs"), "1\n").unwrap();
    git(w, &["add", "-A"]);
    git(w, &["commit", "-q", "-m", "base"]);
    agentvc(w, &["init"]);
    agentvc(w, &["task", "new", "bump"]);
    let v = agentvc(w, &["fork", "--task", "t1", "--from", "a1"]);
    let view = v["fork"]["attempt"]["view_path"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(Path::new(&view).join("lib.rs"), "2\n").unwrap();

    let v = agentvc(w, &["export", "a2", "--branch", "bump"]);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(
        git(w, &["rev-parse", "bump^{tree}"]),
        v["export"]["tree"].as_str().unwrap()
    );
    assert_eq!(git(w, &["show", "bump:lib.rs"]), "2");
    assert_eq!(fs::read_to_string(w.join("lib.rs")).unwrap(), "1\n");
    assert_eq!(git(w, &["status", "--porcelain"]), "");

    let v = agentvc(w, &["export", "a2", "--branch", "bump"]);
    assert_eq!(v["error"]["code"], "branch_exists");
    let v = agentvc(w, &["export", "a2", "--branch", "bump", "--force"]);
    assert_eq!(v["ok"], true, "{v}");
}

/// A tiny fake of the GitHub REST API. Records `(method, path, body)` and
/// answers the calls `agentvc pr` makes.
/// Requests the fake GitHub API received: `(method, path, json body)`.
type RequestLog = std::sync::Arc<std::sync::Mutex<Vec<(String, String, Value)>>>;

fn fake_github() -> (String, RequestLog) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                continue;
            }
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap().to_string();
            let path = parts.next().unwrap().to_string();
            let mut len = 0;
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        len = v.trim().parse().unwrap();
                    }
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let mut log = seen.lock().unwrap();
            let created = log.iter().any(|(m, _, _)| m == "POST");
            log.push((method.clone(), path.clone(), body));
            drop(log);
            let pr = serde_json::json!({
                "number": 7, "html_url": "https://github.com/acme/widget/pull/7"
            });
            let reply = match (method.as_str(), path.as_str()) {
                ("GET", "/repos/acme/widget") => serde_json::json!({ "default_branch": "main" }),
                ("GET", p) if p.starts_with("/repos/acme/widget/pulls?") => {
                    if created {
                        serde_json::json!([pr])
                    } else {
                        serde_json::json!([])
                    }
                }
                _ => pr,
            };
            let reply = reply.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
        }
    });
    (base, log)
}

#[test]
fn pr_pushes_branch_and_opens_pr_with_rejected_attempts() {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path().join("work");
    let remote = tmp.path().join("remotes/acme/widget.git");
    fs::create_dir_all(&w).unwrap();
    fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare"]);
    git(&w, &["init", "-q", "-b", "main"]);
    fs::write(w.join("lib.rs"), "1\n").unwrap();
    git(&w, &["add", "-A"]);
    git(&w, &["commit", "-q", "-m", "base"]);
    git(&w, &["remote", "add", "origin", remote.to_str().unwrap()]);

    agentvc(&w, &["init"]);
    agentvc(
        &w,
        &[
            "task",
            "new",
            "make f return 2",
            "--criteria",
            "unit tests pass",
        ],
    );
    let fork = |actor: &str| {
        let v = agentvc(
            &w,
            &["fork", "--task", "t1", "--from", "a1", "--actor", actor],
        );
        v["fork"]["attempt"]["view_path"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let view_a = fork("agent:one");
    let view_b = fork("agent:two");
    fs::write(Path::new(&view_a).join("lib.rs"), "2\n").unwrap();
    fs::write(Path::new(&view_b).join("lib.rs"), "two\n").unwrap();
    agentvc(
        &w,
        &[
            "snapshot",
            "a2",
            "--meta",
            "tests=12/12",
            "--meta",
            "cost=$0.40",
        ],
    );
    agentvc(
        &w,
        &[
            "snapshot",
            "a3",
            "--meta",
            "tests=9/12",
            "--meta",
            "cost=$0.55",
        ],
    );
    agentvc(&w, &["promote", "a2"]);

    let (api, log) = fake_github();
    let run = |args: &[&str]| -> Value {
        let out = Command::cargo_bin("agentvc")
            .unwrap()
            .arg("--json")
            .arg("-C")
            .arg(&w)
            .args(args)
            .env("GITHUB_TOKEN", "test-token")
            .env("AGENTVC_GITHUB_API", &api)
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("ALL_PROXY")
            .env_remove("http_proxy")
            .env_remove("https_proxy")
            .env_remove("all_proxy")
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "bad json: {e}\n{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
    };

    let v = run(&["pr", "a2", "--draft"]);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["pr"]["url"], "https://github.com/acme/widget/pull/7");
    assert_eq!(v["pr"]["updated"], false);
    assert_eq!(v["pr"]["base"], "main");
    let commit = v["pr"]["export"]["commit"].as_str().unwrap().to_string();
    assert_eq!(git(&remote, &["rev-parse", "agentvc/a2"]), commit);

    {
        let log = log.lock().unwrap();
        let (_, _, body) = log.iter().find(|(m, _, _)| m == "POST").unwrap();
        assert_eq!(body["head"], "agentvc/a2");
        assert_eq!(body["base"], "main");
        assert_eq!(body["draft"], true);
        assert_eq!(body["title"], "make f return 2");
        let text = body["body"].as_str().unwrap();
        assert!(text.contains("unit tests pass"), "{text}");
        assert!(text.contains("**a2** by agent:one"), "{text}");
        assert!(text.contains("- Tests: 12/12"), "{text}");
        assert!(
            text.contains("| a3 | abandoned | lost to a2 | agent:two | 9/12 | $0.55 |"),
            "{text}"
        );
    }
    let space = agentvc_core::Space::discover(&w).unwrap();
    assert_eq!(
        space.pr_url(2).unwrap().as_deref(),
        Some("https://github.com/acme/widget/pull/7")
    );

    // Running it again refreshes the branch and updates the open PR.
    let v = run(&["pr", "a2"]);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["pr"]["updated"], true);
    assert!(log
        .lock()
        .unwrap()
        .iter()
        .any(|(m, p, _)| m == "PATCH" && p == "/repos/acme/widget/pulls/7"));

    // Without a token (and no `gh` on PATH) the command fails with a stable code.
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let git_path = String::from_utf8(
        std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    std::os::unix::fs::symlink(git_path.trim(), bin.join("git")).unwrap();
    let out = Command::cargo_bin("agentvc")
        .unwrap()
        .args(["--json", "-C"])
        .arg(&w)
        .args(["pr", "a2"])
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .env("PATH", &bin)
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "no_token", "{v}");
}
