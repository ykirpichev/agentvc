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
