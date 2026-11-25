//! Drives `agentvc mcp` over stdio like an MCP client would, and checks the
//! results against the CLI's own `--json` output.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_agentvc");

fn cli(dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(BIN)
        .arg("--json")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Client {
    fn start(cwd: &Path) -> Client {
        let mut child = Command::new(BIN)
            .arg("mcp")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut c = Client {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        let init = c.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" },
            }),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        c.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        c
    }

    fn send(&mut self, msg: Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }

    /// Call a tool; returns (structuredContent, isError) after checking the
    /// text content carries the same JSON.
    fn tool(&mut self, name: &str, args: Value) -> (Value, bool) {
        let r = self.request("tools/call", json!({ "name": name, "arguments": args }));
        let result = &r["result"];
        let structured = result["structuredContent"].clone();
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, structured);
        (structured, result["isError"] == true)
    }

    fn ok(&mut self, name: &str, args: Value) -> Value {
        let (v, is_error) = self.tool(name, args.clone());
        assert!(!is_error, "{name} {args}: {v}");
        assert_eq!(v["ok"], true);
        v
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn lists_every_tool_with_a_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = Client::start(tmp.path());
    let r = c.request("tools/list", json!({}));
    let tools = r["result"]["tools"].as_array().unwrap();
    let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for want in [
        "task_new",
        "task_list",
        "fork",
        "snapshot",
        "mark",
        "abandon",
        "promote",
        "log",
        "show",
        "diff",
        "path",
        "run",
        "ps",
        "pr",
    ] {
        assert!(names.contains(&want), "missing tool {want}");
    }
    for t in tools {
        assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        assert!(t["description"].as_str().is_some_and(|d| !d.is_empty()));
    }
    assert_eq!(c.request("ping", json!({}))["result"], json!({}));
}

#[test]
fn missing_space_returns_no_space() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = Client::start(tmp.path());
    for (tool, args) in [
        ("log", json!({})),
        ("task_new", json!({ "goal": "x" })),
        ("show", json!({ "attempt": "a1" })),
    ] {
        let (v, is_error) = c.tool(tool, args);
        assert!(is_error, "{tool}: {v}");
        assert_eq!(v["error"]["code"], "no_space", "{tool}: {v}");
    }
    // The tree resource fails the same way, and listing still works.
    let r = c.request("resources/read", json!({ "uri": "agentvc://tree" }));
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("no_space"),
        "{r}"
    );
    let r = c.request("resources/list", json!({}));
    assert_eq!(r["result"]["resources"].as_array().unwrap().len(), 1);
}

#[test]
fn fork_snapshot_diff_promote_matches_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path();
    fs::write(w.join("lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
    assert_eq!(cli(w, &["init"])["ok"], true);

    let mut c = Client::start(w);
    let t = c.ok(
        "task_new",
        json!({ "goal": "make f return 2", "criteria": "f() == 2" }),
    );
    assert_eq!(t["task"]["id"], 1);

    let fork = |c: &mut Client, actor: &str| {
        let v = c.ok(
            "fork",
            json!({ "task": "t1", "from": "a1", "actor": actor }),
        );
        assert_eq!(v["fork"]["attempt"]["actor"], actor);
        (
            v["fork"]["attempt"]["id"].as_i64().unwrap(),
            v["fork"]["attempt"]["view_path"]
                .as_str()
                .unwrap()
                .to_string(),
        )
    };
    let (a, view_a) = fork(&mut c, "agent:one");
    let (b, _) = fork(&mut c, "agent:two");
    let aid = format!("a{a}");
    let bid = format!("a{b}");

    // The path tool agrees with the fork result.
    let p = c.ok("path", json!({ "attempt": aid }));
    assert_eq!(p["path"], view_a);

    fs::write(
        Path::new(&view_a).join("lib.rs"),
        "pub fn f() -> u32 { 2 }\n",
    )
    .unwrap();
    fs::write(Path::new(&view_a).join("new.rs"), "// new\n").unwrap();
    let s = c.ok(
        "snapshot",
        json!({ "attempt": aid, "meta": { "tests": "pass" } }),
    );
    assert_eq!(s["unchanged"], false);
    assert_eq!(s["snapshot"]["trigger"], "mcp");
    assert_eq!(s["snapshot"]["meta"]["tests"], "pass");

    // Read-only tools return exactly what the CLI prints.
    let d = c.ok("diff", json!({ "a": aid }));
    assert_eq!(d, cli(w, &["diff", &aid]));
    let paths: Vec<_> = d["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["lib.rs", "new.rs"]);
    assert_eq!(
        c.ok("diff", json!({ "a": aid, "b": bid })),
        cli(w, &["diff", &aid, &bid])
    );
    assert_eq!(
        c.ok("show", json!({ "attempt": aid })),
        cli(w, &["show", &aid])
    );

    c.ok("mark", json!({ "attempt": aid, "status": "passed" }));
    let p = c.ok("promote", json!({ "attempt": aid }));
    assert_eq!(p["attempt"]["status"], "promoted");
    assert_eq!(p["closed_siblings"], json!([b]));

    assert_eq!(
        c.ok("log", json!({ "task": "t1" })),
        cli(w, &["log", "--task", "t1"])
    );
    assert_eq!(
        c.ok("show", json!({ "attempt": bid })),
        cli(w, &["show", &bid])
    );
    assert_eq!(c.ok("task_list", json!({})), cli(w, &["task", "list"]));

    // Stable error codes come through as tool errors.
    let (v, is_error) = c.tool("mark", json!({ "attempt": bid, "status": "passed" }));
    assert!(is_error);
    assert_eq!(v["error"]["code"], "attempt_closed");
    let (v, _) = c.tool("show", json!({ "attempt": "a99" }));
    assert_eq!(v["error"]["code"], "not_found");

    // Resources: the attempt (show + diff) and the tree.
    let r = c.request(
        "resources/read",
        json!({ "uri": format!("agentvc://attempt/{aid}") }),
    );
    let body: Value =
        serde_json::from_str(r["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["attempt"]["id"], a);
    assert_eq!(body["diff"]["changes"], d["changes"]);
    let r = c.request("resources/read", json!({ "uri": "agentvc://attempt/a99" }));
    assert_eq!(r["error"]["code"], -32002);

    let r = c.request("resources/list", json!({}));
    let uris: Vec<_> = r["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap().to_string())
        .collect();
    assert!(uris.contains(&"agentvc://tree".to_string()));
    assert!(uris.contains(&format!("agentvc://attempt/{bid}")));
    drop(c);

    // Started inside a view, the tree is that attempt's task.
    let mut c = Client::start(Path::new(&view_a));
    let r = c.request("resources/read", json!({ "uri": "agentvc://tree" }));
    let tree: Value =
        serde_json::from_str(r["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(tree["task"], 1);
    assert_eq!(tree["attempts"].as_array().unwrap().len(), 2);
    // ...and snapshot defaults to that attempt (promoted, so closed).
    let (v, is_error) = c.tool("snapshot", json!({}));
    assert!(is_error);
    assert_eq!(v["error"]["code"], "attempt_closed");
    assert!(v["error"]["message"].as_str().unwrap().contains(&aid));
}

#[test]
fn run_and_ps() {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path();
    fs::write(w.join("a.txt"), "a\n").unwrap();
    cli(w, &["init"]);
    let mut c = Client::start(w);
    let r = c.ok(
        "run",
        json!({ "from": "a1", "command": ["sh", "-c", "echo hi > out.txt"], "check": "test -f out.txt", "detach": false }),
    );
    assert_eq!(r["attempt"]["status"], "passed");
    assert_eq!(r["run"]["exit_code"], 0);
    let ps = c.ok("ps", json!({ "all": true }));
    assert_eq!(ps, cli(w, &["ps", "--all"]));
    assert_eq!(ps["runs"].as_array().unwrap().len(), 1);
}
