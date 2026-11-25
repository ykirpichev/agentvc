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

#[test]
fn run_records_outcome_and_check() {
    let tmp = tempfile::tempdir().unwrap();
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
    let tmp = tempfile::tempdir().unwrap();
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
    let tmp = tempfile::tempdir().unwrap();
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
    let other = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(assert_cmd::cargo::cargo_bin("agentvc"))
        .args(["hook", "stop"])
        .current_dir(other.path())
        .stdin(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}
