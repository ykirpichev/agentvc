//! MCP server for agentvc (`agentvc mcp`).
//!
//! Speaks the Model Context Protocol over stdio: newline-delimited JSON-RPC
//! 2.0 on stdin and stdout, logs on stderr. Claude Code, Claude Desktop,
//! Cursor and VS Code agents all launch servers this way.
//!
//! Every tool maps to one `agentvc` CLI command. The server runs that command
//! as `agentvc --json -C <dir> ...` and returns its JSON unchanged, both as
//! `structuredContent` and as text, so an agent sees exactly what `--json`
//! prints, including `{"ok": false, "error": {"code": ...}}` on failure
//! (then with `isError: true`). Running the real binary keeps the two in
//! lockstep and keeps a misbehaving command from taking the server down.
//!
//! Resources:
//! - `agentvc://tree`: the attempt tree for the current task, the task of
//!   the attempt whose view contains the server's directory (all attempts if
//!   that attempt has no task).
//! - `agentvc://attempt/aN`: an attempt's metadata, snapshots, and the
//!   files it changed since it was forked.

pub mod tools;

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// Protocol versions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "agentvc records every agent attempt at a task as a node in an attempt tree, \
each in its own copy-on-write view. Workflow: `task_new` (or reuse a task from `task_list`), \
`fork` an attempt before trying an approach and work only inside its view_path, `snapshot` with \
meta such as {\"tests\": \"pass\"} at milestones, `mark` it passed or failed, and leave `promote`, \
`abandon` and `pr` to the human unless asked. `run` forks and runs another agent in the background; \
watch it with `ps`. If main moved while an attempt was open, `rebase` it before `pr`; on error.code \
conflict, fix the listed paths or retry with keep_conflicts. Every result is the JSON of the matching `agentvc --json` command; on failure \
error.code is a stable code such as no_space, not_found or attempt_closed. Ids look like t1 \
(task), a3 (attempt) and s7 (snapshot).";

// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
/// MCP: resource not found.
const RESOURCE_NOT_FOUND: i64 = -32002;

/// The server state: which binary to run and where.
pub struct Server {
    exe: PathBuf,
    dir: PathBuf,
}

impl Server {
    /// `exe` is the `agentvc` binary; `dir` is where commands run (`-C`),
    /// which decides the space and the "current attempt".
    pub fn new(exe: impl Into<PathBuf>, dir: impl Into<PathBuf>) -> Self {
        Server {
            exe: exe.into(),
            dir: dir.into(),
        }
    }

    /// Serve on stdin and stdout until stdin closes.
    pub fn serve_stdio(&self) -> io::Result<()> {
        self.serve(io::stdin().lock(), io::stdout().lock())
    }

    /// Serve newline-delimited JSON-RPC from `input` to `output` until EOF.
    pub fn serve(&self, input: impl BufRead, mut output: impl Write) -> io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle_line(&line) {
                writeln!(output, "{reply}")?;
                output.flush()?;
            }
        }
        Ok(())
    }

    /// Handle one line of input; `None` when nothing should be written back
    /// (notifications and responses).
    pub fn handle_line(&self, line: &str) -> Option<Value> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(error(Value::Null, PARSE_ERROR, &e.to_string())),
        };
        match msg {
            // JSON-RPC batches (protocol 2025-03-26).
            Value::Array(items) if !items.is_empty() => {
                let replies: Vec<Value> =
                    items.into_iter().filter_map(|m| self.handle(m)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            m => self.handle(m),
        }
    }

    fn handle(&self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            // A response to a request we never send, or garbage.
            return match (id, msg.get("result").or(msg.get("error"))) {
                (_, Some(_)) => None,
                (id, None) => Some(error(
                    id.unwrap_or(Value::Null),
                    INVALID_REQUEST,
                    "not a JSON-RPC request",
                )),
            };
        };
        // Notifications (no id) never get a reply.
        let id = id?;
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        Some(match self.dispatch(method, &params) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => error(id, code, &message),
        })
    }

    fn dispatch(&self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => Ok(initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({
                "tools": tools::TOOLS.iter().map(tools::Tool::describe).collect::<Vec<_>>()
            })),
            "tools/call" => self.call_tool(params),
            "resources/list" => Ok(self.list_resources()),
            "resources/templates/list" => Ok(json!({
                "resourceTemplates": [{
                    "uriTemplate": "agentvc://attempt/{attempt}",
                    "name": "attempt",
                    "title": "Attempt",
                    "description": "An attempt's metadata, snapshots and the files it changed since it was forked. {attempt} is aN.",
                    "mimeType": "application/json",
                }]
            })),
            "resources/read" => self.read_resource(params),
            "prompts/list" => Ok(json!({ "prompts": [] })),
            _ => Err((METHOD_NOT_FOUND, format!("method {method:?} not found"))),
        }
    }

    fn call_tool(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, "tools/call needs a tool name".to_string()))?;
        let tool = tools::find(name).ok_or((INVALID_PARAMS, format!("unknown tool {name:?}")))?;
        let out = match tool.build(params.get("arguments").unwrap_or(&Value::Null)) {
            Ok(argv) => self.agentvc(&argv),
            Err(tools::BadArgs(msg)) => failure("bad_args", &msg),
        };
        let is_error = out.get("ok") != Some(&Value::Bool(true));
        Ok(json!({
            "content": [{ "type": "text", "text": out.to_string() }],
            "structuredContent": out,
            "isError": is_error,
        }))
    }

    /// Run `agentvc --json -C <dir> <argv>` and return its JSON object.
    pub fn agentvc(&self, argv: &[String]) -> Value {
        let out = Command::new(&self.exe)
            .arg("--json")
            .arg("-C")
            .arg(&self.dir)
            .args(argv)
            .stdin(Stdio::null())
            .output();
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                return failure(
                    "spawn_failed",
                    &format!("could not start {}: {e}", self.exe.display()),
                )
            }
        };
        match serde_json::from_slice::<Value>(&out.stdout) {
            Ok(v @ Value::Object(_)) => v,
            _ => failure(
                "error",
                &format!(
                    "agentvc {} exited with {} and no JSON: {}",
                    argv.first().map(String::as_str).unwrap_or(""),
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ),
        }
    }

    fn list_resources(&self) -> Value {
        let mut resources = vec![json!({
            "uri": "agentvc://tree",
            "name": "tree",
            "title": "Attempt tree",
            "description": "The attempt tree for the current task.",
            "mimeType": "application/json",
        })];
        let log = self.agentvc(&["log".into()]);
        for a in log["attempts"].as_array().into_iter().flatten() {
            let id = a["id"].as_i64().unwrap_or_default();
            resources.push(json!({
                "uri": format!("agentvc://attempt/a{id}"),
                "name": format!("a{id}"),
                "title": format!("a{id} {} ({})", a["status"].as_str().unwrap_or(""), a["actor"].as_str().unwrap_or("")),
                "mimeType": "application/json",
            }));
        }
        json!({ "resources": resources })
    }

    fn read_resource(&self, params: &Value) -> Result<Value, (i64, String)> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, "resources/read needs a uri".to_string()))?;
        let body = if uri == "agentvc://tree" {
            self.tree()
        } else if let Some(a) = uri.strip_prefix("agentvc://attempt/") {
            self.attempt_resource(a)
        } else {
            return Err((RESOURCE_NOT_FOUND, format!("unknown resource {uri}")));
        };
        if body.get("ok") != Some(&Value::Bool(true)) {
            let code = body["error"]["code"].as_str().unwrap_or("error");
            let message = body["error"]["message"].as_str().unwrap_or("failed");
            return Err((
                if code == "not_found" || code == "bad_ref" {
                    RESOURCE_NOT_FOUND
                } else {
                    INVALID_PARAMS
                },
                format!("{code}: {message}"),
            ));
        }
        Ok(json!({
            "contents": [{
                "uri": uri,
                "mimeType": "application/json",
                "text": body.to_string(),
            }]
        }))
    }

    /// `agentvc://tree`: `log` for the current attempt's task.
    fn tree(&self) -> Value {
        let task = current_task(&self.dir);
        let mut argv = vec!["log".to_string()];
        if let Some(t) = task {
            argv.push(format!("--task=t{t}"));
        }
        let mut v = self.agentvc(&argv);
        if v["ok"] == true {
            v["task"] = json!(task);
        }
        v
    }

    /// `agentvc://attempt/aN`: `show` plus `diff` against the fork point.
    fn attempt_resource(&self, attempt: &str) -> Value {
        let mut show = self.agentvc(&["show".into(), "--".into(), attempt.into()]);
        if show["ok"] != true {
            return show;
        }
        let diff = if show["attempt"]["parent_snapshot"].is_null() {
            Value::Null
        } else {
            let d = self.agentvc(&["diff".into(), "--".into(), attempt.into()]);
            if d["ok"] != true {
                return d;
            }
            json!({ "from": d["from"], "to": d["to"], "changes": d["changes"] })
        };
        show["diff"] = diff;
        show
    }
}

/// The task of the attempt whose view contains `dir`, if any.
fn current_task(dir: &Path) -> Option<i64> {
    let space = agentvc_core::Space::discover(dir).ok()?;
    space.attempt_for_path(dir).ok()?.task_id
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "subscribe": false, "listChanged": false },
            "prompts": { "listChanged": false },
        },
        "serverInfo": {
            "name": "agentvc",
            "title": "agentvc",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

/// A failed command result in the same shape the CLI prints.
fn failure(code: &str, message: &str) -> Value {
    json!({ "ok": false, "error": { "code": code, "message": message } })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server::new("/nonexistent/agentvc", "/")
    }

    fn call(s: &Server, msg: Value) -> Value {
        s.handle_line(&msg.to_string()).expect("a reply")
    }

    #[test]
    fn negotiates_protocol_version() {
        let s = server();
        let r = call(
            &s,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}),
        );
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r["result"]["serverInfo"]["name"], "agentvc");
        let r = call(
            &s,
            json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}),
        );
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn notifications_get_no_reply() {
        let s = server();
        assert!(s
            .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none());
    }

    #[test]
    fn errors_for_bad_input() {
        let s = server();
        assert_eq!(
            s.handle_line("{nope").unwrap()["error"]["code"],
            PARSE_ERROR
        );
        let r = call(&s, json!({"jsonrpc":"2.0","id":3,"method":"bogus"}));
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        let r = call(
            &s,
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}}),
        );
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn bad_arguments_are_tool_errors() {
        let s = server();
        let r = call(
            &s,
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"show","arguments":{}}}),
        );
        assert_eq!(r["result"]["isError"], true);
        assert_eq!(
            r["result"]["structuredContent"]["error"]["code"],
            "bad_args"
        );
    }

    #[test]
    fn missing_binary_is_a_tool_error() {
        let s = server();
        let r = call(
            &s,
            json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"log"}}),
        );
        assert_eq!(r["result"]["isError"], true);
        assert_eq!(
            r["result"]["structuredContent"]["error"]["code"],
            "spawn_failed"
        );
    }

    #[test]
    fn lists_tools_and_handles_batches() {
        let s = server();
        let r = s
            .handle_line(
                r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"},{"jsonrpc":"2.0","id":2,"method":"tools/list"}]"#,
            )
            .unwrap();
        let r = r.as_array().unwrap();
        assert_eq!(r.len(), 2);
        let names: Vec<_> = r[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for want in [
            "task_new", "fork", "snapshot", "mark", "abandon", "promote", "log", "show", "diff",
            "path", "run", "ps", "pr", "rebase", "sync",
        ] {
            assert!(names.contains(&want), "missing {want}");
        }
    }
}
