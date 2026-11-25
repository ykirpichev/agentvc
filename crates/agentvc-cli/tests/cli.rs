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
