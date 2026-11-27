use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

/// A temporary directory that unmounts overlay views under it before it is
/// deleted (forks use overlayfs when the tests run as root).
struct TestDir(tempfile::TempDir);

impl std::ops::Deref for TestDir {
    type Target = tempfile::TempDir;
    fn deref(&self) -> &tempfile::TempDir {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        agentvc_core::overlay::unmount_under(self.0.path());
    }
}

fn tempdir() -> TestDir {
    TestDir(tempfile::tempdir().unwrap())
}

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
    let tmp = tempdir();
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

    let v = agentvc(w, &["overview"]);
    assert_eq!(v["overview"]["tasks"][0]["winner"], format!("a{a}"));
    let out = w.join("overview.json");
    let v = agentvc(w, &["overview", "--out", out.to_str().unwrap()]);
    assert_eq!(v["tasks"], 1);
    let doc: Value = serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(doc["schema"], "agentvc.overview/1");
    assert_eq!(doc["tasks"][0]["state"], "done");
    fs::remove_file(&out).unwrap();

    // The root working directory never changed.
    assert_eq!(
        fs::read_to_string(w.join("lib.rs")).unwrap(),
        "pub fn f() -> u32 { 1 }\n"
    );
}

#[test]
fn errors_have_stable_codes() {
    let tmp = tempdir();
    let v = agentvc(tmp.path(), &["log"]);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "no_space");

    agentvc(tmp.path(), &["init"]);
    let v = agentvc(tmp.path(), &["show", "a99"]);
    assert_eq!(v["error"]["code"], "not_found");
    let v = agentvc(tmp.path(), &["init"]);
    assert_eq!(v["error"]["code"], "space_exists");
}

#[test]
fn run_records_outcome_and_check() {
    let tmp = tempdir();
    let w = tmp.path();
    fs::write(w.join("answer.txt"), "41\n").unwrap();
    agentvc(w, &["init"]);
    agentvc(w, &["task", "new", "fix the answer"]);

    // A fake agent that edits a file; the check decides the outcome.
    let v = agentvc(
        w,
        &[
            "run",
            "--task",
            "t1",
            "--actor",
            "agent:fake",
            "--check",
            "grep -q 42 answer.txt",
            "--",
            "sh",
            "-c",
            "echo working; echo 42 > answer.txt",
        ],
    );
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["attempt"]["status"], "passed");
    assert_eq!(v["run"]["exit_code"], 0);
    assert_eq!(v["run"]["check_exit_code"], 0);
    assert_eq!(v["snapshot"]["trigger"], "run-exit");
    // The same fork stats `fork --json` reports.
    assert!(v["fork"]["backend"].is_string(), "{v}");
    assert!(v["fork"]["millis"].is_u64(), "{v}");
    assert!(v["fork"]["bytes_copied"].is_u64(), "{v}");
    let log = fs::read_to_string(v["run"]["log_path"].as_str().unwrap()).unwrap();
    assert!(log.contains("working"));

    let v = agentvc(w, &["run", "--task", "t1", "--", "sh", "-c", "exit 2"]);
    assert_eq!(v["attempt"]["status"], "failed");
    assert_eq!(v["run"]["exit_code"], 2);

    // The root working directory is untouched.
    assert_eq!(fs::read_to_string(w.join("answer.txt")).unwrap(), "41\n");
}

#[test]
fn detached_run_finishes_in_background() {
    let tmp = tempdir();
    let w = tmp.path();
    fs::write(w.join("f"), "x\n").unwrap();
    agentvc(w, &["init"]);
    let v = agentvc(
        w,
        &[
            "run",
            "--detach",
            "--",
            "sh",
            "-c",
            "sleep 0.3; echo done > f",
        ],
    );
    assert_eq!(v["ok"], true, "{v}");
    let a = v["attempt"]["id"].as_i64().unwrap();
    assert!(v["pid"].as_u64().is_some());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let ps = agentvc(w, &["ps"]);
        if ps["runs"].as_array().unwrap().is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run never finished: {ps}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let all = agentvc(w, &["ps", "--all"]);
    assert_eq!(all["runs"][0]["exit_code"], 0);
    let show = agentvc(w, &["show", &format!("a{a}")]);
    let snaps = show["snapshots"].as_array().unwrap();
    assert_eq!(snaps.last().unwrap()["trigger"], "run-exit");
}

#[test]
fn hooks_install_and_snapshot_per_tool_call() {
    use std::io::Write;
    let tmp = tempdir();
    let w = tmp.path();
    fs::write(w.join("f"), "1\n").unwrap();
    agentvc(w, &["init"]);
    let v = agentvc(w, &["hooks"]);
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(v["settings"].as_str().unwrap()).unwrap())
            .unwrap();
    let cmd = settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(cmd.ends_with(" hook post-tool-use"));

    let v = agentvc(w, &["fork", "--actor", "agent:claude"]);
    let view = v["fork"]["attempt"]["view_path"]
        .as_str()
        .unwrap()
        .to_string();
    let a = v["fork"]["attempt"]["id"].as_i64().unwrap();
    fs::write(Path::new(&view).join("f"), "2\n").unwrap();

    // Call the hook the way Claude Code does: JSON on stdin, text mode, no output.
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("agentvc"))
        .args(["hook", "post-tool-use"])
        .current_dir(&view)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let input = serde_json::json!({ "session_id": "abc", "tool_name": "Write", "cwd": view });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty());

    let show = agentvc(w, &["show", &format!("a{a}")]);
    let last = show["snapshots"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["trigger"], "tool:Write");
    assert_eq!(last["meta"]["session_id"], "abc");

    // The local settings file is copied into views but never captured.
    let d = agentvc(w, &["diff", &format!("a{a}")]);
    let paths: Vec<_> = d["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].clone())
        .collect();
    assert_eq!(paths, vec![Value::from("f")]);

    // Outside a space the hook is a silent no-op.
    let other = tempdir();
    let status = std::process::Command::new(assert_cmd::cargo::cargo_bin("agentvc"))
        .args(["hook", "stop"])
        .current_dir(other.path())
        .stdin(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
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
    let tmp = tempdir();
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

    // A new state of the attempt parents on the same base, so it does not
    // build on the branch: moving it needs --force.
    fs::write(Path::new(&view).join("lib.rs"), "3\n").unwrap();
    let v = agentvc(w, &["export", "a2", "--branch", "bump"]);
    assert_eq!(v["error"]["code"], "branch_exists");
    let v = agentvc(w, &["export", "a2", "--branch", "bump", "--force"]);
    assert_eq!(v["ok"], true, "{v}");
}

/// A tiny fake of the GitHub REST API. Records `(method, path, body)` and
/// answers the calls `agentvc pr` makes.
/// Requests the fake GitHub API received: `(method, path, json body)`.
type RequestLog = std::sync::Arc<std::sync::Mutex<Vec<(String, String, Value)>>>;

/// Whether the fake GitHub reports PR #7 as merged.
static PR_MERGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
            let pulls = |p: &String| p.contains("/pulls");
            let created = log.iter().any(|(m, p, _)| m == "POST" && pulls(p));
            log.push((method.clone(), path.clone(), body));
            // The open PR carries the body it was last sent, like GitHub.
            let last_body = log
                .iter()
                .rev()
                .find(|(m, p, _)| (m == "POST" || m == "PATCH") && pulls(p))
                .map(|(_, _, b)| b["body"].clone())
                .unwrap_or(Value::Null);
            // The one issue comment agentvc keeps (the space), if any.
            let comment = log
                .iter()
                .rev()
                .find(|(m, p, _)| (m == "POST" || m == "PATCH") && p.contains("comments"))
                .map(|(_, _, b)| {
                    serde_json::json!({
                        "id": 99, "body": b["body"],
                        "html_url": "https://github.com/acme/widget/pull/7#issuecomment-99",
                    })
                });
            drop(log);
            let merged = PR_MERGED.load(std::sync::atomic::Ordering::SeqCst);
            let pr = serde_json::json!({
                "number": 7, "html_url": "https://github.com/acme/widget/pull/7",
                "body": last_body, "state": if merged { "closed" } else { "open" },
                "merged": merged, "draft": false,
                "merged_at": if merged { Value::from("2026-11-26T00:00:00Z") } else { Value::Null },
                "merge_commit_sha": "abc123merge", "head": { "sha": "headsha1" },
            });
            let reply = match (method.as_str(), path.as_str()) {
                ("GET", p) if p.starts_with("/repos/acme/widget/commits/headsha1/check-runs") => {
                    serde_json::json!({ "check_runs": [
                        { "name": "rust", "status": "completed", "conclusion": "failure" },
                        { "name": "ui", "status": "completed", "conclusion": "success" },
                    ]})
                }
                ("GET", "/repos/acme/widget/commits/headsha1/status") => {
                    serde_json::json!({ "statuses": [] })
                }
                ("GET", p) if p.starts_with("/repos/acme/widget/pulls/7/reviews") => {
                    serde_json::json!([{ "user": { "login": "ana" }, "state": "CHANGES_REQUESTED",
                        "body": "Handle the empty case", "submitted_at": "2026-11-26T00:00:00Z" }])
                }
                ("POST", "/graphql") => {
                    serde_json::json!({ "data": { "repository": { "pullRequest": {
                    "reviewThreads": { "nodes": [
                        { "isResolved": false, "isOutdated": false, "path": "lib.rs", "line": 1,
                          "comments": { "nodes": [{ "author": { "login": "ana" },
                            "body": "Off by one here", "url": "https://github.com/acme/widget/pull/7#r1" }] } },
                        { "isResolved": true, "isOutdated": false, "path": "lib.rs", "line": 1,
                          "comments": { "nodes": [] } },
                    ] } } } } })
                }
                ("GET", "/repos/acme/widget") => serde_json::json!({ "default_branch": "main" }),
                ("GET", p) if p.starts_with("/repos/acme/widget/pulls?") => {
                    if created {
                        serde_json::json!([pr])
                    } else {
                        serde_json::json!([])
                    }
                }
                ("GET", p) if p.starts_with("/repos/acme/widget/issues/7/comments") => {
                    serde_json::json!(comment.iter().collect::<Vec<_>>())
                }
                (_, p) if p.contains("comments") => comment.unwrap_or(Value::Null),
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
    let tmp = tempdir();
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
    // Whoever picks the winner is named in the PR body (#77).
    agentvc(&w, &["config", "user.name", "Ana"]);
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

    // The whole space went to the remote too, losers included (#56).
    let sync_ref = v["sync"]["git"]["ref"].as_str().unwrap().to_string();
    assert!(sync_ref.starts_with("refs/agentvc/spaces/"), "{v}");
    assert_eq!(
        git(&remote, &["rev-parse", &sync_ref]),
        v["sync"]["git"]["commit"].as_str().unwrap()
    );
    // ...and into a comment on the PR, for machines that can't push it (#64).
    assert_eq!(v["sync"]["pr"]["pr"], 7, "{v}");
    assert_eq!(v["sync"]["pr"]["updated"], false);
    // Another machine rebuilds the same attempt tree from it.
    git(&w, &["push", "-q", "origin", "main"]);
    let other = tmp.path().join("other");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    let v = agentvc(&other, &["init", "--from-remote", "--no-prepare"]);
    assert_eq!(v["ok"], true, "{v}");
    let strip = |mut v: Value| {
        for a in v["attempts"].as_array_mut().unwrap() {
            a.as_object_mut().unwrap().remove("view_path");
            a.as_object_mut().unwrap().remove("backend");
        }
        v
    };
    assert_eq!(
        strip(agentvc(&other, &["log"])),
        strip(agentvc(&w, &["log"]))
    );
    let v = agentvc(&other, &["show", "a3"]);
    assert_eq!(v["attempt"]["reason"], "lost to a2");
    assert_eq!(v["snapshots"].as_array().unwrap().len(), 1);

    // A third machine rebuilds it from the PR comment alone.
    let third = tmp.path().join("third");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            third.to_str().unwrap(),
        ],
    );
    let in_third = |args: &[&str]| -> Value {
        let out = Command::cargo_bin("agentvc")
            .unwrap()
            .arg("--json")
            .arg("-C")
            .arg(&third)
            .args(args)
            .env("GITHUB_TOKEN", "test-token")
            .env("AGENTVC_GITHUB_API", &api)
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("http_proxy")
            .env_remove("https_proxy")
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let v = in_third(&[
        "init",
        "--from-pr",
        "7",
        "--repo",
        "acme/widget",
        "--no-prepare",
    ]);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["restore"]["rebuilt"], serde_json::json!([2, 3]), "{v}");
    assert_eq!(
        strip(agentvc(&third, &["log"])),
        strip(agentvc(&w, &["log"]))
    );
    let v = agentvc(&third, &["diff", "s1", "a3"]);
    assert_eq!(v["changes"][0]["path"], "lib.rs", "{v}");

    {
        let log = log.lock().unwrap();
        let (_, _, body) = log
            .iter()
            .find(|(m, p, _)| m == "POST" && p.contains("/pulls"))
            .unwrap();
        assert_eq!(body["head"], "agentvc/a2");
        assert_eq!(body["base"], "main");
        assert_eq!(body["draft"], true);
        assert_eq!(body["title"], "make f return 2");
        let text = body["body"].as_str().unwrap();
        assert!(text.contains("unit tests pass"), "{text}");
        assert!(
            text.contains("**a2** by agent:one, promoted by human:ana,"),
            "{text}"
        );
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

    // Without a token (and no `gh` on PATH) the command fails with a stable
    // code. Skipped on Windows: hiding `gh` relies on a PATH of symlinks.
    #[cfg(unix)]
    {
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
}

#[test]
fn fork_backend_can_be_forced() {
    let tmp = tempdir();
    let w = tmp.path();
    fs::write(w.join("f"), "1\n").unwrap();
    agentvc(w, &["init"]);
    let v = agentvc(w, &["fork", "--from", "a1", "--backend", "copy"]);
    assert_eq!(v["fork"]["backend"], "copy", "{v}");
    assert_eq!(v["fork"]["attempt"]["backend"], "copy");

    // Auto always reports a backend, and explains anything but reflink.
    let v = agentvc(w, &["fork", "--from", "a1"]);
    let backend = v["fork"]["backend"].as_str().unwrap();
    assert!(["reflink", "overlay", "copy"].contains(&backend), "{v}");
    if backend != "reflink" {
        assert!(v["fork"]["backend_reason"].is_string(), "{v}");
    }
    let view = v["fork"]["attempt"]["view_path"].as_str().unwrap();
    assert_eq!(
        fs::read_to_string(Path::new(view).join("f")).unwrap(),
        "1\n"
    );
}

#[test]
fn rebase_onto_moved_main_then_pr_has_only_own_changes() {
    let tmp = tempdir();
    let w = tmp.path().join("work");
    let other = tmp.path().join("other");
    let remote = tmp.path().join("remotes/acme/widget.git");
    fs::create_dir_all(&w).unwrap();
    fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    git(&w, &["init", "-q", "-b", "main"]);
    fs::write(w.join("lib.rs"), "1\n").unwrap();
    fs::write(w.join("README.md"), "readme\n").unwrap();
    git(&w, &["add", "-A"]);
    git(&w, &["commit", "-q", "-m", "base"]);
    git(&w, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&w, &["push", "-q", "origin", "main"]);

    agentvc(&w, &["init"]);
    agentvc(&w, &["task", "new", "bump"]);
    let v = agentvc(&w, &["fork", "--task", "t1", "--from", "a1"]);
    let view = v["fork"]["attempt"]["view_path"]
        .as_str()
        .unwrap()
        .to_string();
    let view = Path::new(&view);
    fs::write(view.join("lib.rs"), "2\n").unwrap();
    fs::write(view.join("README.md"), "readme from a2\n").unwrap();

    // Another PR lands on main meanwhile.
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    fs::write(other.join("README.md"), "readme from main\n").unwrap();
    fs::write(other.join("NEWS.md"), "news\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-q", "-m", "other PR"]);
    git(&other, &["push", "-q", "origin", "main"]);
    let main = git(&other, &["rev-parse", "HEAD"]);

    // Conflict: code and paths in --json, nothing changed.
    let before = agentvc(&w, &["show", "a2"])["snapshots"]
        .as_array()
        .unwrap()
        .len();
    let v = agentvc(&w, &["rebase", "a2"]);
    assert_eq!(v["ok"], false, "{v}");
    assert_eq!(v["error"]["code"], "conflict");
    assert_eq!(v["error"]["paths"], serde_json::json!(["README.md"]));
    assert_eq!(v["error"]["onto"], main);
    assert_eq!(
        agentvc(&w, &["show", "a2"])["snapshots"]
            .as_array()
            .unwrap()
            .len(),
        before
    );
    assert_eq!(
        fs::read_to_string(view.join("README.md")).unwrap(),
        "readme from a2\n"
    );

    // --keep-conflicts writes markers; fix, snapshot, then pr.
    let v = agentvc(&w, &["rebase", "a2", "--keep-conflicts"]);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["rebase"]["onto"], main);
    assert_eq!(v["rebase"]["onto_ref"], "origin/main");
    assert_eq!(v["rebase"]["conflicts"], serde_json::json!(["README.md"]));
    assert_eq!(v["rebase"]["status"], "exploring");
    assert!(fs::read_to_string(view.join("README.md"))
        .unwrap()
        .contains("<<<<<<<"));
    assert_eq!(fs::read_to_string(view.join("NEWS.md")).unwrap(), "news\n");
    fs::write(view.join("README.md"), "readme from main and a2\n").unwrap();
    assert_eq!(agentvc(&w, &["snapshot", "a2"])["ok"], true);

    let v = agentvc(&w, &["diff", "a2"]);
    assert_eq!(v["from"], format!("git:{main}"));
    let paths: Vec<&str> = v["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["README.md", "lib.rs"]);
    agentvc(&w, &["promote", "a2"]);

    let (api, _log) = fake_github();
    let out = Command::cargo_bin("agentvc")
        .unwrap()
        .arg("--json")
        .arg("-C")
        .arg(&w)
        .args(["pr", "a2"])
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
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true, "{v}");
    // The pushed commit sits on the new main and reverts nothing.
    assert_eq!(git(&remote, &["rev-parse", "agentvc/a2^"]), main);
    assert_eq!(
        git(&remote, &["diff", "--name-only", "main", "agentvc/a2"]),
        "README.md\nlib.rs"
    );
}

/// chmod +x where there is such a thing (git runs hooks through sh on
/// Windows regardless).
fn make_executable(_path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn agentvc_text(dir: &Path, args: &[&str]) -> String {
    let out = Command::cargo_bin("agentvc")
        .unwrap()
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn diff_patch_covers_modified_added_deleted_binary_and_working_state() {
    let tmp = tempdir();
    let w = tmp.path();
    fs::write(w.join("lib.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
    fs::write(w.join("gone.txt"), "bye\n").unwrap();
    fs::write(w.join("logo.png"), b"\x89PNG\x00\x00old").unwrap();
    agentvc(w, &["init", "--no-prepare"]);
    let v = agentvc(w, &["fork", "--from", "a1"]);
    let view = Path::new(v["fork"]["attempt"]["view_path"].as_str().unwrap()).to_path_buf();
    fs::write(view.join("lib.rs"), "fn a() {}\nfn b2() {}\nfn c() {}\n").unwrap();
    fs::remove_file(view.join("gone.txt")).unwrap();
    fs::write(view.join("new.txt"), "hello\n").unwrap();
    fs::write(view.join("logo.png"), b"\x89PNG\x00\x00new").unwrap();

    // No refs inside the view: the working state, with no snapshot taken.
    let text = agentvc_text(&view, &["diff", "--patch"]);
    assert!(
        text.contains(
            "--- a/lib.rs\n+++ b/lib.rs\n@@ -1,3 +1,3 @@\n fn a() {}\n-fn b() {}\n+fn b2() {}\n"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "deleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "new file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("Binary files a/logo.png and b/logo.png differ\n"),
        "{text}"
    );
    assert!(
        !text.contains("PNG"),
        "binary content must not be dumped: {text}"
    );
    assert_eq!(
        agentvc(w, &["show", "a2"])["snapshots"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let stat = agentvc_text(&view, &["diff", "--stat"]);
    assert!(stat.contains(" logo.png | Bin"), "{stat}");
    assert!(
        stat.contains("4 files changed, 2 insertions(+), 2 deletions(-)"),
        "{stat}"
    );

    // Between refs, with hunks in JSON.
    agentvc(w, &["snapshot", "a2"]);
    let v = agentvc(w, &["diff", "a1", "a2", "--patch"]);
    let changes = v["changes"].as_array().unwrap();
    let by = |p: &str| changes.iter().find(|c| c["path"] == p).unwrap().clone();
    assert_eq!(by("lib.rs")["kind"], "modified");
    assert_eq!(by("lib.rs")["hunks"][0]["lines"][1], "-fn b() {}");
    assert_eq!(by("lib.rs")["hunks"][0]["old_start"], 1);
    assert_eq!(by("gone.txt")["kind"], "removed");
    assert_eq!(by("new.txt")["kind"], "added");
    assert_eq!(by("logo.png")["binary"], true);
    assert_eq!(by("logo.png")["hunks"].as_array().unwrap().len(), 0);

    // --stat in JSON keeps counts and drops hunks; plain diff is unchanged.
    let v = agentvc(w, &["diff", "a2", "--stat"]);
    assert_eq!(v["from"], "s1");
    assert_eq!(v["changes"][0]["hunks"].as_array().unwrap().len(), 0);
    let v = agentvc(w, &["diff", "a2"]);
    assert!(v["changes"][0].get("hunks").is_none());
}

/// `agentvc` with `--json -C dir`, talking to the fake GitHub at `api`.
fn agentvc_gh(dir: &Path, api: &str, env: &[(&str, &str)], args: &[&str]) -> Value {
    let mut cmd = Command::cargo_bin("agentvc").unwrap();
    cmd.arg("--json")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GITHUB_TOKEN", "test-token")
        .env("AGENTVC_GITHUB_API", api)
        .env_remove("AGENTVC_PR_BRANCH")
        .env_remove("AGENTVC_PR_BASE")
        .env_remove("AGENTVC_PR_BODY_PREFIX");
    for v in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        cmd.env_remove(v);
    }
    cmd.envs(env.iter().copied());
    let out = cmd.output().unwrap();
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "bad json: {e}\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn pr_from_a_thread_branch_keeps_prefix_trailers_and_checkout() {
    let tmp = tempdir();
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
    // A cloud thread: its own branch is checked out and is the only one it
    // may push to.
    git(&w, &["checkout", "-q", "-b", "claude/thread"]);
    agentvc(&w, &["init", "--no-prepare"]);

    let v = agentvc(&w, &["config", "pr.branch", "claude/thread"]);
    assert_eq!(v["config"]["values"][0], "claude/thread", "{v}");
    agentvc(
        &w,
        &[
            "config",
            "pr.trailer",
            "Co-Authored-By: Bot <bot@example.com>",
            "--add",
        ],
    );
    let v = agentvc(&w, &["config", "nope", "x"]);
    assert_eq!(v["error"]["code"], "bad_config_key", "{v}");
    let v = agentvc(&w, &["config"]);
    assert_eq!(
        v["config"].as_array().unwrap().len(),
        agentvc_core::config::KEYS.len(),
        "{v}"
    );

    agentvc(&w, &["task", "new", "make f return 2"]);
    let v = agentvc(&w, &["fork", "--task", "t1", "--from", "a1"]);
    let view = v["fork"]["attempt"]["view_path"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(Path::new(&view).join("lib.rs"), "2\n").unwrap();
    agentvc(&w, &["promote", "a2"]);
    fs::write(tmp.path().join("prefix.md"), "_Requested by **Yury**_\n").unwrap();

    let (api, log) = fake_github();
    let prefix = tmp.path().join("prefix.md");
    let v = agentvc_gh(
        &w,
        &api,
        &[],
        &[
            "pr",
            "a2",
            "--body-prefix",
            prefix.to_str().unwrap(),
            "--trailer",
            "Claude-Session: https://example.com/s/1",
        ],
    );
    assert_eq!(v["ok"], true, "{v}");
    let commit = v["pr"]["export"]["commit"].as_str().unwrap().to_string();
    assert_eq!(v["pr"]["export"]["branch"], "claude/thread");
    assert_eq!(v["pr"]["export"]["checkout_updated"], true);
    // No --force, no detach: the checked-out branch and its checkout moved.
    assert_eq!(git(&w, &["rev-parse", "HEAD"]), commit);
    assert_eq!(
        git(&w, &["symbolic-ref", "--short", "HEAD"]),
        "claude/thread"
    );
    assert_eq!(
        git(&w, &["status", "--porcelain", "--untracked-files=no"]),
        ""
    );
    assert_eq!(fs::read_to_string(w.join("lib.rs")).unwrap(), "2\n");
    assert_eq!(git(&remote, &["rev-parse", "claude/thread"]), commit);
    let msg = git(&w, &["log", "-1", "--format=%B"]);
    assert!(
        msg.ends_with(
            "Co-Authored-By: Bot <bot@example.com>\nClaude-Session: https://example.com/s/1"
        ),
        "{msg}"
    );
    {
        let log = log.lock().unwrap();
        let (_, _, body) = log
            .iter()
            .find(|(m, p, _)| m == "POST" && p.contains("/pulls"))
            .unwrap();
        assert_eq!(body["head"], "claude/thread");
        let text = body["body"].as_str().unwrap();
        assert!(
            text.starts_with("_Requested by **Yury**_\n\n<!-- agentvc:begin"),
            "{text}"
        );
        assert!(text.trim_end().ends_with("<!-- agentvc:end -->"), "{text}");
    }

    // A review fix continued from a2: same branch, no flags, prefix kept.
    let v = agentvc(&w, &["fork", "--from", "a2"]);
    let view = v["fork"]["attempt"]["view_path"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(Path::new(&view).join("lib.rs"), "2 // reviewed\n").unwrap();
    agentvc(&w, &["promote", "a3"]);
    let v = agentvc_gh(&w, &api, &[], &["pr", "a3"]);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["pr"]["updated"], true);
    assert_eq!(
        git(&w, &["rev-parse", "HEAD"]),
        v["pr"]["export"]["commit"].as_str().unwrap()
    );
    assert_eq!(
        fs::read_to_string(w.join("lib.rs")).unwrap(),
        "2 // reviewed\n"
    );
    assert_eq!(
        git(&w, &["status", "--porcelain", "--untracked-files=no"]),
        ""
    );
    {
        let log = log.lock().unwrap();
        let (_, _, body) = log
            .iter()
            .rfind(|(m, p, _)| m == "PATCH" && p.contains("/pulls"))
            .unwrap();
        let text = body["body"].as_str().unwrap();
        assert!(
            text.starts_with("_Requested by **Yury**_\n\n<!-- agentvc:begin"),
            "{text}"
        );
        assert!(text.contains("a3"), "{text}");
    }

    // A push the remote refuses (as the cloud git proxy does with 403) has
    // its own code.
    let hook = remote.join("hooks/pre-receive");
    fs::write(
        &hook,
        "#!/bin/sh\necho 'The requested URL returned error: 403' >&2\nexit 1\n",
    )
    .unwrap();
    make_executable(&hook);
    let v = agentvc_gh(
        &w,
        &api,
        &[("AGENTVC_PR_BRANCH", "not-mine")],
        &["pr", "a3"],
    );
    assert_eq!(v["error"]["code"], "push_not_allowed", "{v}");
}

#[test]
fn status_shows_attempt_in_a_view_and_space_outside() {
    let tmp = tempdir();
    let w = tmp.path();
    fs::write(w.join("a.txt"), "a\n").unwrap();
    agentvc(w, &["init", "--no-prepare"]);
    agentvc(w, &["task", "new", "add b"]);
    let v = agentvc(
        w,
        &[
            "fork",
            "--task",
            "t1",
            "--from",
            "a1",
            "--actor",
            "agent:one",
        ],
    );
    let view = Path::new(v["fork"]["attempt"]["view_path"].as_str().unwrap()).to_path_buf();
    agentvc(
        w,
        &[
            "fork",
            "--task",
            "t1",
            "--from",
            "a1",
            "--actor",
            "agent:two",
        ],
    );
    fs::write(view.join("b.txt"), "b\n").unwrap();

    // Inside a view: that attempt, with what changed since its head.
    let v = agentvc(&view, &["status"]);
    assert_eq!(v["status"]["attempt"]["id"], 2, "{v}");
    assert_eq!(v["status"]["task"]["goal"], "add b");
    assert_eq!(v["status"]["head"]["id"], 1);
    assert_eq!(v["status"]["changes"][0]["path"], "b.txt");
    assert_eq!(v["status"]["changes"][0]["kind"], "added");
    assert!(v["status"]["runs"].as_array().unwrap().is_empty());
    let text = agentvc_text(&view, &["status"]);
    assert!(text.contains("task     t1  add b"), "{text}");
    assert!(text.contains("  A  b.txt"), "{text}");
    // Not a git repo: no upstream to be behind.
    assert!(v["status"]["upstream"].is_null());

    // In the host checkout: the space.
    agentvc(w, &["mark", "a3", "passed"]);
    let v = agentvc(w, &["status"]);
    assert_eq!(v["status"]["needs_review"], serde_json::json!([3]), "{v}");
    assert_eq!(
        v["status"]["open_tasks"][0]["attempts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let text = agentvc_text(w, &["status"]);
    assert!(
        text.contains("a3  passed  agent:two  <- needs review"),
        "{text}"
    );
    assert!(text.contains("needs review: a3"), "{text}");

    // An explicit attempt works anywhere.
    assert_eq!(agentvc(w, &["status", "a2"])["status"]["attempt"]["id"], 2);
}

#[test]
fn sync_falls_back_to_the_pr_comment_when_the_push_is_refused() {
    let tmp = tempdir();
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
    git(&w, &["push", "-q", "origin", "main"]);
    agentvc(&w, &["init", "--no-prepare"]);
    agentvc(&w, &["task", "new", "t"]);
    let v = agentvc(&w, &["fork", "--task", "t1", "--actor", "agent:one"]);
    let view = v["fork"]["attempt"]["view_path"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(Path::new(&view).join("lib.rs"), "2\n").unwrap();
    agentvc(&w, &["snapshot", "a2"]);

    // Refuse every push except branches, like a cloud session's git proxy.
    let hook = remote.join("hooks/pre-receive");
    fs::write(
        &hook,
        "#!/bin/sh\nwhile read old new ref; do case \"$ref\" in refs/heads/*) ;; \
         *) echo 'The requested URL returned error: 403' >&2; exit 1;; esac; done\n",
    )
    .unwrap();
    make_executable(&hook);

    let (api, _log) = fake_github();
    // No PR yet: nothing could be saved.
    let v = agentvc_gh(&w, &api, &[], &["sync", "--auto"]);
    assert_eq!(v["error"]["code"], "not_saved", "{v}");
    let v = agentvc_gh(&w, &api, &[], &["pr", "a2", "--branch", "thread"]);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["sync"]["git_error"]["code"], "push_not_allowed", "{v}");
    assert_eq!(v["sync"]["pr"]["pr"], 7, "{v}");
    // Later automatic saves skip the refused push and edit the comment.
    let v = agentvc_gh(&w, &api, &[], &["abandon", "a2", "--reason", "x"]);
    assert!(v["sync"]["git_error"].is_null(), "{v}");
    assert_eq!(v["sync"]["pr"]["updated"], true, "{v}");

    let other = tmp.path().join("other");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    let v = agentvc_gh(
        &other,
        &api,
        &[],
        &[
            "init",
            "--from-pr",
            "https://github.com/acme/widget/pull/7",
            "--no-prepare",
        ],
    );
    assert_eq!(v["ok"], true, "{v}");
    let v = agentvc(&other, &["show", "a2"]);
    assert_eq!(v["attempt"]["reason"], "x");
    let v = agentvc(&other, &["diff", "s1", "a2"]);
    assert_eq!(v["changes"][0]["path"], "lib.rs", "{v}");

    // Every PR's space merged into a third clone's own space.
    let third = tmp.path().join("third");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            third.to_str().unwrap(),
        ],
    );
    let v = agentvc_gh(
        &third,
        &api,
        &[],
        &["import", "--all-prs", "--repo", "acme/widget"],
    );
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["created"], true, "{v}");
    assert_eq!(v["imports"][0]["import"]["outcome"], "imported", "{v}");
    let v = agentvc_gh(
        &third,
        &api,
        &[],
        &["import", "--all-prs", "--repo", "acme/widget"],
    );
    assert_eq!(v["imports"][0]["import"]["outcome"], "unchanged", "{v}");
    let v = agentvc(&third, &["overview"]);
    let task = &v["overview"]["tasks"][0];
    assert_eq!(task["source"], "acme/widget#7", "{v}");
    assert_eq!(task["attempts"][0]["reason"], "x", "{v}");
}

#[test]
fn op_log_and_undo_promote_then_redo() {
    let tmp = tempdir();
    let w = tmp.path();
    fs::write(w.join("lib.rs"), "pub fn f() {}\n").unwrap();
    agentvc(w, &["init"]);
    agentvc(w, &["task", "new", "goal"]);
    agentvc(w, &["fork", "--task", "t1", "--actor", "agent:a"]);
    agentvc(w, &["fork", "--task", "t1", "--actor", "agent:b"]);
    agentvc(w, &["promote", "a2"]);

    let log = agentvc(w, &["op", "log"]);
    let ops = log["operations"].as_array().unwrap();
    assert_eq!(ops[0]["kind"], "attempt.promoted");
    assert_eq!(ops[0]["undoable"], true);
    let promote = ops[0]["id"].as_i64().unwrap();

    let u = agentvc(w, &["undo"]);
    assert_eq!(u["ok"], true);
    assert_eq!(u["undid"]["id"], promote);
    let a3 = agentvc(w, &["show", "a3"]);
    assert_eq!(a3["attempt"]["status"], "exploring");

    let again = agentvc(w, &["undo", &promote.to_string()]);
    assert_eq!(again["error"]["code"], "already_undone");
    assert_eq!(again["error"]["by"], u["op"]);

    let redo = agentvc(w, &["undo", &format!("op{}", u["op"])]);
    assert_eq!(redo["ok"], true);
    let a3 = agentvc(w, &["show", "a3"]);
    assert_eq!(a3["attempt"]["status"], "abandoned");
    let log = agentvc(w, &["op", "log", "--limit", "3"]);
    assert_eq!(log["operations"].as_array().unwrap().len(), 3);
    assert_eq!(log["operations"][0]["kind"], "undo");
    assert_eq!(log["operations"][1]["undone_by"], redo["op"]);
}

#[test]
fn doctor_and_gc_report_and_exit_codes() {
    let tmp = tempdir();
    let w = tmp.path();
    fs::write(w.join("lib.rs"), "pub fn f() {}\n").unwrap();
    agentvc(w, &["init", "--no-prepare"]);
    let fork = agentvc(w, &["fork", "--backend", "copy"]);
    let view = fork["fork"]["attempt"]["view_path"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(Path::new(&view).join("lib.rs"), "pub fn g() {}\n").unwrap();
    agentvc(w, &["snapshot", "a2"]);

    let v = agentvc(w, &["doctor", "--offline"]);
    assert_eq!(v["ok"], true);
    assert_eq!(v["doctor"]["errors"], 0);

    let v = agentvc(w, &["gc", "--dry-run"]);
    assert_eq!(v["gc"]["dry_run"], true);
    assert!(v["gc"]["objects_packed"].as_u64().unwrap() > 0);
    let v = agentvc(w, &["gc"]);
    assert_eq!(v["gc"]["after"]["packs"], 1);
    assert_eq!(v["gc"]["after"]["loose"], 0);
    // Everything still reads from the pack.
    let v = agentvc(w, &["diff", "a1", "a2"]);
    assert_eq!(v["changes"][0]["path"], "lib.rs");

    // A broken view: doctor fails (with --json too) until --repair fixes it.
    fs::remove_dir_all(&view).unwrap();
    let out = Command::cargo_bin("agentvc")
        .unwrap()
        .args(["--json", "-C"])
        .arg(w)
        .args(["doctor", "--offline"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["doctor"]["findings"][0]["code"], "missing_view");
    assert_eq!(v["doctor"]["findings"][0]["severity"], "error");
    let v = agentvc(w, &["doctor", "--offline", "--repair"]);
    assert_eq!(v["doctor"]["errors"], 0);
    assert_eq!(v["doctor"]["repaired"], 1);
    assert_eq!(
        fs::read_to_string(Path::new(&view).join("lib.rs")).unwrap(),
        "pub fn g() {}\n"
    );
}

#[test]
fn git_works_in_views_and_commits_become_snapshots() {
    let tmp = tempdir();
    let w = tmp.path().join("work");
    let remote = tmp.path().join("remote.git");
    fs::create_dir_all(&w).unwrap();
    fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    git(&w, &["init", "-q", "-b", "main"]);
    fs::write(w.join("lib.rs"), "1\n").unwrap();
    fs::write(w.join("README.md"), "readme\n").unwrap();
    git(&w, &["add", "-A"]);
    git(&w, &["commit", "-q", "-m", "base"]);
    git(&w, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&w, &["push", "-q", "origin", "main"]);
    let base = git(&w, &["rev-parse", "HEAD"]);

    agentvc(&w, &["init"]);
    let v = agentvc(&w, &["fork", "--from", "a1"]);
    assert_eq!(v["fork"]["git"]["head"], base, "{v}");
    assert_eq!(v["fork"]["git"]["capture_commits"], true, "{v}");
    let view = v["fork"]["attempt"]["view_path"]
        .as_str()
        .unwrap()
        .to_string();
    let view = Path::new(&view);

    // git status and git diff show the attempt's changes, like agentvc diff.
    fs::write(view.join("lib.rs"), "2\n").unwrap();
    fs::write(view.join("new.rs"), "new\n").unwrap();
    assert_eq!(git(view, &["status", "--porcelain"]), "M lib.rs\n?? new.rs");
    agentvc(view, &["snapshot"]);
    let d = agentvc(&w, &["diff", "a2"]);
    let paths: Vec<&str> = d["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("{d}"))
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["lib.rs", "new.rs"]);
    assert!(git(view, &["diff"]).contains("+2"));
    assert_eq!(git(view, &["log", "--format=%s"]), "base");
    assert_eq!(git(&w, &["worktree", "list"]).lines().count(), 1);

    // A commit in the view runs the real hook and is recorded as a snapshot.
    git(view, &["add", "-A"]);
    git(view, &["commit", "-q", "-m", "two"]);
    let commit = git(view, &["rev-parse", "HEAD"]);
    let snaps = agentvc(&w, &["show", "a2"])["snapshots"].clone();
    let last = snaps.as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["trigger"], "git-commit", "{snaps}");
    assert_eq!(last["meta"]["git_commit"], commit);
    assert_eq!(last["meta"]["message"], "two");
    assert_eq!(
        git(&w, &["rev-parse", "main"]),
        base,
        "host branch untouched"
    );

    // After a rebase, the view's HEAD is the new base.
    let other = tmp.path().join("other");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    fs::write(other.join("NEWS.md"), "news\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-q", "-m", "other PR"]);
    git(&other, &["push", "-q", "origin", "main"]);
    let main = git(&other, &["rev-parse", "HEAD"]);
    let v = agentvc(&w, &["rebase", "a2"]);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(git(view, &["rev-parse", "HEAD"]), main);
    assert_eq!(git(view, &["status", "--porcelain"]), "M lib.rs\n?? new.rs");

    // Abandoning removes the link and its gitdir.
    agentvc(&w, &["abandon", "a2", "--reason", "test"]);
    assert!(!w.join(".agentvc/git/a2").exists());
    assert_eq!(git(&w, &["worktree", "list"]).lines().count(), 1);
}

#[test]
fn land_with_verify_reports_hook_failures_then_lands() {
    let tmp = tempdir();
    let w = tmp.path();
    git(w, &["init", "-q", "-b", "main"]);
    fs::write(w.join("lib.rs"), "pub fn f() {}\n").unwrap();
    git(w, &["add", "-A"]);
    git(w, &["commit", "-q", "-m", "base"]);
    agentvc(w, &["init", "--no-prepare"]);
    agentvc(w, &["task", "new", "edit lib"]);
    let fork = agentvc(w, &["fork", "--task", "t1", "--actor", "agent:x"]);
    let view = std::path::PathBuf::from(fork["fork"]["attempt"]["view_path"].as_str().unwrap());
    fs::write(view.join("lib.rs"), "pub fn f() -> u8 { 2 }\n").unwrap();
    let hook = w.join(".git/hooks/pre-commit");
    fs::write(&hook, "#!/bin/sh\necho 'lint: bad style' >&2\nexit 1\n").unwrap();
    make_executable(&hook);
    let before = git(w, &["rev-parse", "main"]);

    let v = agentvc(w, &["land", "a2", "--verify"]);
    assert_eq!(v["error"]["code"], "hook_failed", "{v}");
    assert_eq!(v["error"]["hook"], "pre-commit");
    assert_eq!(v["error"]["exit_code"], 1);
    assert!(v["error"]["output"]
        .as_str()
        .unwrap()
        .contains("lint: bad style"));
    assert_eq!(git(w, &["rev-parse", "main"]), before);

    let v = agentvc(w, &["land", "a2"]);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["land"]["branch"], "main");
    assert_eq!(v["land"]["previous"], before.as_str());
    assert_eq!(
        git(w, &["rev-parse", "main"]),
        v["land"]["commit"].as_str().unwrap()
    );
    assert_eq!(
        fs::read_to_string(w.join("lib.rs")).unwrap(),
        "pub fn f() -> u8 { 2 }\n"
    );
    let st = agentvc(w, &["status", "a2"]);
    assert_eq!(st["status"]["landed"]["branch"], "main");
}

#[test]
fn fork_from_worktree_and_apply_round_trip() {
    let tmp = tempdir();
    let w = tmp.path();
    git(w, &["init", "-q", "-b", "main"]);
    fs::write(w.join("lib.rs"), "1\n").unwrap();
    fs::write(w.join("old.rs"), "old\n").unwrap();
    git(w, &["add", "-A"]);
    git(w, &["commit", "-q", "-m", "base"]);
    agentvc(w, &["init", "--no-prepare"]);

    fs::write(w.join("lib.rs"), "2\n").unwrap();
    fs::remove_file(w.join("old.rs")).unwrap();
    fs::write(w.join("new.rs"), "new\n").unwrap();
    let v = agentvc(
        w,
        &["fork", "--from-worktree", "--move", "--backend", "copy"],
    );
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["worktree"]["moved"].as_array().unwrap().len(), 3, "{v}");
    let id = v["fork"]["attempt"]["id"].as_i64().unwrap();
    assert_eq!(git(w, &["status", "--porcelain"]), "");
    let d = agentvc(w, &["diff", &format!("a{id}")]);
    assert_eq!(d["changes"].as_array().unwrap().len(), 3, "{d}");

    // A local edit to one of the files blocks apply.
    fs::write(w.join("lib.rs"), "local\n").unwrap();
    let v = agentvc(w, &["apply", &format!("a{id}")]);
    assert_eq!(v["error"]["code"], "dirty_worktree", "{v}");
    assert_eq!(v["error"]["paths"][0], "lib.rs");
    fs::write(w.join("lib.rs"), "1\n").unwrap();

    let v = agentvc(w, &["apply", &format!("a{id}")]);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(fs::read_to_string(w.join("lib.rs")).unwrap(), "2\n");
    assert_eq!(fs::read_to_string(w.join("new.rs")).unwrap(), "new\n");
    assert!(!w.join("old.rs").exists());
    assert_eq!(git(w, &["diff", "--cached", "--name-only"]), "");
}

#[test]
fn tags_name_snapshots_and_work_as_refs() {
    let tmp = tempdir();
    let w = tmp.path();
    fs::write(w.join("lib.rs"), "1\n").unwrap();
    agentvc(w, &["init", "--no-prepare"]);
    let v = agentvc(w, &["fork", "--backend", "copy"]);
    let view = v["fork"]["attempt"]["view_path"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(Path::new(&view).join("lib.rs"), "2\n").unwrap();

    let v = agentvc(w, &["tag", "known-good", "a2", "--note", "before refactor"]);
    assert_eq!(v["tag"]["name"], "known-good", "{v}");
    let snap = v["tag"]["snapshot_id"].as_i64().unwrap();
    let v = agentvc(w, &["tag", "known-good", "a1"]);
    assert_eq!(v["error"]["code"], "exists");
    let v = agentvc(w, &["tag", "s3", "a1"]);
    assert_eq!(v["error"]["code"], "bad_tag_name");

    fs::write(Path::new(&view).join("lib.rs"), "3\n").unwrap();
    agentvc(w, &["snapshot", "a2"]);
    let d = agentvc(w, &["diff", "known-good", "a2"]);
    assert_eq!(d["changes"][0]["path"], "lib.rs", "{d}");
    let f = agentvc(w, &["fork", "--from", "known-good", "--backend", "copy"]);
    let fview = f["fork"]["attempt"]["view_path"].as_str().unwrap();
    assert_eq!(
        fs::read_to_string(Path::new(fview).join("lib.rs")).unwrap(),
        "2\n"
    );
    assert_eq!(f["fork"]["parent_snapshot"]["id"], snap);

    assert!(agentvc_text(w, &["log"]).contains("#known-good"));
    assert!(agentvc_text(w, &["show", "known-good"]).contains("#known-good"));
    let l = agentvc(w, &["tag", "--list"]);
    assert_eq!(l["tags"][0]["note"], "before refactor");
    agentvc(w, &["tag", "-d", "known-good"]);
    let v = agentvc(w, &["diff", "known-good", "a2"]);
    assert_eq!(v["error"]["code"], "not_found");
}

#[test]
fn take_files_and_hunks_then_merge_conflicts_with_a_code() {
    let tmp = tempdir();
    let w = tmp.path();
    git(w, &["init", "-q", "-b", "main"]);
    let lines: String = (1..=20).map(|i| format!("{i}\n")).collect();
    fs::write(w.join("lib.rs"), &lines).unwrap();
    fs::write(w.join("README.md"), "readme\n").unwrap();
    git(w, &["add", "-A"]);
    git(w, &["commit", "-q", "-m", "base"]);
    agentvc(w, &["init"]);
    agentvc(w, &["task", "new", "t"]);
    let fork = || {
        let v = agentvc(w, &["fork", "--task", "t1", "--from", "a1"]);
        v["fork"]["attempt"]["view_path"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let (va, vb) = (fork(), fork());
    let (va, vb) = (Path::new(&va), Path::new(&vb));
    fs::create_dir_all(vb.join("tests")).unwrap();
    fs::write(vb.join("tests/t.rs"), "test\n").unwrap();
    fs::write(
        vb.join("lib.rs"),
        lines.replace("2\n", "two\n").replace("18\n", "eighteen\n"),
    )
    .unwrap();
    fs::write(vb.join("README.md"), "b\n").unwrap();
    agentvc(w, &["snapshot", "a3"]);

    // The hunk numbering is the one diff --patch shows.
    let patch = agentvc_text(w, &["diff", "a3", "--patch"]);
    assert_eq!(patch.matches("@@ -").count(), 4, "{patch}");

    let v = agentvc(
        w,
        &[
            "take", "a3", "--into", "a2", "--paths", "tests/**", "--hunks", "lib.rs:1",
        ],
    );
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["take"]["snapshot"]["meta"]["source"], "a3");
    assert_eq!(fs::read_to_string(va.join("tests/t.rs")).unwrap(), "test\n");
    let lib = fs::read_to_string(va.join("lib.rs")).unwrap();
    assert!(lib.contains("two\n") && lib.contains("\n18\n"), "{lib}");
    assert_eq!(
        fs::read_to_string(va.join("README.md")).unwrap(),
        "readme\n"
    );

    // Inside a2's view, --into defaults to it.
    fs::write(va.join("README.md"), "a\n").unwrap();
    let v = agentvc(va, &["merge", "a3"]);
    assert_eq!(v["ok"], false, "{v}");
    assert_eq!(v["error"]["code"], "conflict");
    assert_eq!(v["error"]["paths"], serde_json::json!(["README.md"]));
    assert_eq!(fs::read_to_string(va.join("README.md")).unwrap(), "a\n");

    let v = agentvc(w, &["merge", "a3", "--into", "a2", "--keep-conflicts"]);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["take"]["conflicts"], serde_json::json!(["README.md"]));
    assert!(fs::read_to_string(va.join("README.md"))
        .unwrap()
        .contains("<<<<<<<"));
    assert_eq!(
        agentvc(w, &["show", "a3"])["attempt"]["status"],
        "exploring"
    );
}

#[test]
fn log_filters_and_blame() {
    let tmp = tempdir();
    let w = tmp.path();
    git(w, &["init", "-q", "-b", "main"]);
    fs::write(w.join("x.py"), "one\ntwo\n").unwrap();
    git(w, &["add", "-A"]);
    git(w, &["commit", "-q", "-m", "base"]);
    agentvc(w, &["init"]);
    agentvc(w, &["task", "new", "parser"]);
    let fork = |actor: &str, from: &str| {
        let v = agentvc(
            w,
            &["fork", "--task", "t1", "--actor", actor, "--from", from],
        );
        v["fork"]["attempt"]["view_path"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let va = fork("agent:claude", "a1");
    fs::write(Path::new(&va).join("x.py"), "one\nTWO\n").unwrap();
    agentvc(w, &["snapshot", "a2", "--meta", "tests=pass"]);
    let vb = fork("agent:codex", "a1");
    fs::write(Path::new(&vb).join("other.txt"), "x\n").unwrap();
    agentvc(w, &["snapshot", "a3", "--meta", "tests=pass"]);

    let v = agentvc(
        w,
        &[
            "log",
            "--task",
            "t1",
            "--touched",
            "x.py",
            "--meta",
            "tests=pass",
        ],
    );
    let ids: Vec<i64> = v["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [2]);
    let v = agentvc(w, &["log", "--actor", "agent:codex", "--since", "1h"]);
    assert_eq!(v["rows"][0]["id"], 3, "{v}");
    let v = agentvc(w, &["log", "--since", "soon"]);
    assert_eq!(v["error"]["code"], "bad_query", "{v}");

    // Inside a2's view: blame includes unsnapshotted work.
    fs::write(Path::new(&va).join("x.py"), "one\nTWO\nthree\n").unwrap();
    let v = agentvc(Path::new(&va), &["blame", "x.py"]);
    let kinds: Vec<&str> = v["blame"]["lines"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .map(|l| l["origin"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["git", "snapshot", "working"]);
    assert_eq!(v["blame"]["lines"][1]["origin"]["attempt"], 2);
    let text = agentvc_text(w, &["blame", "x.py", "a2", "--no-working"]);
    assert!(text.contains("agent:claude"), "{text}");
    assert!(!text.contains("three"), "{text}");
}

/// Two people on two machines share a space (#77): actions carry their
/// names, `sync` merges what the other added, and a status both changed is
/// a `sync_conflict` naming the attempt and both values.
#[test]
fn two_people_sync_one_space() {
    let tmp = tempdir();
    let remote = tmp.path().join("remote.git");
    git(
        tmp.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            remote.to_str().unwrap(),
        ],
    );
    let ana = tmp.path().join("ana");
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", ana.to_str().unwrap()],
    );
    git(&ana, &["config", "user.name", "Ana Lee"]);
    std::fs::write(ana.join("lib.rs"), "fn f() {}\n").unwrap();
    git(&ana, &["add", "-A"]);
    git(&ana, &["commit", "-q", "-m", "base"]);
    git(&ana, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&ana, &["push", "-q", "origin", "main"]);
    assert_eq!(agentvc(&ana, &["init", "--no-prepare"])["ok"], true);
    agentvc(&ana, &["task", "new", "goal"]);
    for actor in ["agent:x", "agent:y"] {
        let v = agentvc(
            &ana,
            &[
                "fork",
                "--task",
                "t1",
                "--from",
                "a1",
                "--actor",
                actor,
                "--backend",
                "copy",
            ],
        );
        assert_eq!(v["ok"], true, "{v}");
    }
    // A person forking is themselves, from git config or `user.name`.
    let v = agentvc(
        &ana,
        &["fork", "--task", "t1", "--from", "a1", "--backend", "copy"],
    );
    assert_eq!(v["fork"]["attempt"]["actor"], "human:ana-lee", "{v}");
    assert_eq!(agentvc(&ana, &["sync"])["ok"], true);

    let bo = tmp.path().join("bo");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            bo.to_str().unwrap(),
        ],
    );
    let v = agentvc(&bo, &["init", "--from-remote", "--no-prepare"]);
    assert_eq!(v["ok"], true, "{v}");
    // Bo's name is this machine's setting; it doesn't travel with the space.
    agentvc(&bo, &["config", "user.name", "Bo"]);

    agentvc(&ana, &["abandon", "a2", "--reason", "ana: no"]);
    agentvc(&bo, &["mark", "a3", "passed"]);
    agentvc(
        &bo,
        &[
            "fork",
            "--task",
            "t1",
            "--from",
            "a1",
            "--actor",
            "agent:bo",
            "--backend",
            "copy",
        ],
    );
    assert_eq!(agentvc(&ana, &["sync"])["ok"], true);
    let v = agentvc(&bo, &["sync"]);
    assert!(
        v["sync"]["git"]["merged"]["updated"].as_u64().unwrap() > 0,
        "{v}"
    );
    assert_eq!(agentvc(&ana, &["sync"])["ok"], true);
    let outline = |dir: &Path| {
        let mut v: Vec<(String, String)> = agentvc(dir, &["log"])["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                (
                    a["actor"].as_str().unwrap().into(),
                    a["status"].as_str().unwrap().into(),
                )
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(outline(&ana), outline(&bo));
    assert!(outline(&ana).contains(&("agent:bo".into(), "exploring".into())));
    assert!(outline(&ana).contains(&("agent:x".into(), "abandoned".into())));
    assert!(outline(&ana).contains(&("agent:y".into(), "passed".into())));
    // Who did what is in the operation log on both machines.
    let ops = agentvc(&ana, &["op", "log"]).to_string();
    assert!(
        ops.contains("human:ana-lee") && ops.contains("human:bo"),
        "{ops}"
    );

    // Both close a3 differently.
    agentvc(&ana, &["abandon", "a3", "--reason", "dup"]);
    agentvc(&bo, &["promote", "a3"]);
    assert_eq!(agentvc(&bo, &["sync"])["ok"], true);
    let v = agentvc(&ana, &["sync"]);
    assert_eq!(v["error"]["code"], "sync_conflict", "{v}");
    assert_eq!(v["error"]["conflicts"][0]["what"], "a3");
    assert_eq!(v["error"]["conflicts"][0]["ours"], "abandoned");
    assert_eq!(v["error"]["conflicts"][0]["theirs"], "promoted");
    let v = agentvc(&ana, &["sync", "--prefer", "theirs"]);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(
        agentvc(&ana, &["show", "a3"])["attempt"]["status"],
        "promoted"
    );
}

#[test]
fn pr_refresh_brings_checks_reviews_comments_and_the_merge_back() {
    let tmp = tempdir();
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
    agentvc(&w, &["init", "--no-prepare"]);
    agentvc(&w, &["task", "new", "make it 2"]);
    let v = agentvc(&w, &["fork", "--task", "t1", "--actor", "agent:one"]);
    let view = v["fork"]["attempt"]["view_path"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(Path::new(&view).join("lib.rs"), "2\n").unwrap();
    agentvc(&w, &["promote", "a2"]);

    let (api, _log) = fake_github();
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
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{args:?}: {e}: {}", String::from_utf8_lossy(&out.stderr)))
    };
    let v = run(&["pr", "a2", "--repo", "acme/widget", "--no-sync"]);
    assert_eq!(v["ok"], true, "{v}");

    // Before any refresh the PR is listed as open, not refreshed yet.
    let st = run(&["status"]);
    assert_eq!(
        st["status"]["open_prs"][0]["attempts"],
        serde_json::json!([2])
    );
    assert!(st["status"]["open_prs"][0]["feedback"].is_null());

    let v = run(&["pr", "--refresh", "a2"]);
    assert_eq!(v["ok"], true, "{v}");
    let f = &v["refreshed"][0]["feedback"];
    assert_eq!(f["state"], "open");
    assert_eq!(f["checks"]["state"], "fail");
    assert_eq!(f["checks"]["failing"], serde_json::json!(["rust"]));
    assert_eq!(f["decision"], "changes_requested");
    assert_eq!(f["unresolved"], 1);

    let st = run(&["status", "a2"]);
    assert_eq!(st["status"]["pr"]["unresolved"], 1);
    assert_eq!(st["status"]["pr"]["threads"][0]["path"], "lib.rs");

    // The next agent, in a fork of a2, gets the review.
    let v = run(&["fork", "--from", "a2", "--actor", "agent:two"]);
    assert_eq!(v["fork"]["review"]["unresolved"], 1, "{v}");
    assert_eq!(v["fork"]["review"]["decision"], "changes_requested");
    let out = Command::cargo_bin("agentvc")
        .unwrap()
        .arg("-C")
        .arg(&w)
        .args(["comments", "a3"])
        .output()
        .unwrap();
    let md = String::from_utf8_lossy(&out.stdout);
    assert!(md.contains("## Changes requested by ana"), "{md}");
    assert!(
        md.contains("## lib.rs:1\n\n**ana**: Off by one here"),
        "{md}"
    );

    // Merged: the merge commit is recorded once and the PR leaves status.
    PR_MERGED.store(true, std::sync::atomic::Ordering::SeqCst);
    let v = run(&["pr", "--refresh"]);
    assert_eq!(v["refreshed"][0]["newly_merged"], true, "{v}");
    assert_eq!(v["refreshed"][0]["feedback"]["merge_commit"], "abc123merge");
    let st = run(&["status"]);
    assert_eq!(st["status"]["open_prs"], serde_json::json!([]));
    let v = run(&["pr", "--refresh"]);
    assert_eq!(v["refreshed"], serde_json::json!([]));
    PR_MERGED.store(false, std::sync::atomic::Ordering::SeqCst);
}

#[test]
fn annotate_and_check_show_up_in_show_and_log() {
    let tmp = tempdir();
    let w = tmp.path();
    fs::write(w.join("a.txt"), "one\n").unwrap();
    agentvc(w, &["init"]);
    agentvc(w, &["task", "new", "demo"]);
    let v = agentvc(w, &["fork", "--task", "t1", "--actor", "agent:claude"]);
    let view = v["fork"]["attempt"]["view_path"]
        .as_str()
        .unwrap()
        .to_string();

    // From inside the view the agent records as itself.
    let v = agentvc(
        Path::new(&view),
        &[
            "annotate",
            "--model",
            "opus",
            "--cost-usd",
            "0.3",
            "--tokens-in",
            "1000",
            "--tokens-out",
            "200",
            "--note",
            "first pass",
        ],
    );
    assert_eq!(v["annotation"]["actor"], "agent:claude");
    agentvc(
        w,
        &[
            "annotate",
            "a2",
            "--cost-usd",
            "0.12",
            "--transcript",
            "https://example.com/t/1",
        ],
    );
    let v = agentvc(w, &["annotate", "a2"]);
    assert_eq!(v["error"]["code"], "bad_annotation");

    let v = agentvc(
        w,
        &[
            "check",
            "a2",
            "--",
            "echo",
            "test result: ok. 7 passed; 0 failed; 0 ignored",
        ],
    );
    assert_eq!(v["check"]["passed"], true, "{v}");
    assert_eq!(v["check"]["tests"]["passed"], 7);
    assert_eq!(v["status"], "passed");
    assert_eq!(v["cached"], false);
    let v = agentvc(
        w,
        &[
            "check",
            "a2",
            "--",
            "echo",
            "test result: ok. 7 passed; 0 failed; 0 ignored",
        ],
    );
    assert_eq!(v["cached"], true);

    let v = agentvc(w, &["show", "a2"]);
    assert!((v["agent"]["cost_usd"].as_f64().unwrap() - 0.42).abs() < 1e-9);
    assert_eq!(v["agent"]["tokens_in"], 1000);
    assert_eq!(v["agent"]["notes"][0]["text"], "first pass");
    assert_eq!(v["agent"]["transcripts"][0], "https://example.com/t/1");
    assert_eq!(v["attempt"]["model"], "opus");
    assert_eq!(v["checks"][0]["stale"], false);

    // A check result is metadata for `log`: `--meta tests=pass`.
    let v = agentvc(w, &["log", "--meta", "tests=pass"]);
    assert_eq!(v["rows"][0]["id"], 2);
    assert!((v["rows"][0]["cost_usd"].as_f64().unwrap() - 0.42).abs() < 1e-9);

    // After a change, the result is stale until checked again.
    fs::write(Path::new(&view).join("a.txt"), "two\n").unwrap();
    agentvc(w, &["snapshot", "a2"]);
    let v = agentvc(w, &["show", "a2"]);
    assert_eq!(v["checks"][0]["stale"], true);
    let v = agentvc(
        w,
        &["check", "a2", "--name", "lint", "--no-mark", "--", "exit 3"],
    );
    assert_eq!(v["check"]["exit_code"], 3);
    assert_eq!(v["status"], "passed");

    let text = Command::cargo_bin("agentvc")
        .unwrap()
        .arg("-C")
        .arg(w)
        .args(["log"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(text.contains("lint:fail tests:pass? $0.42"), "{text}");
}
