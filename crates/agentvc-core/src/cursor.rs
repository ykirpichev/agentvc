//! Cursor integration: `agentvc integrate cursor` writes the MCP server
//! config, a project rule describing the attempt workflow, and Cursor hooks
//! that snapshot the attempt the agent edits. `--remove` reverts all three.
//!
//! Cursor's hooks send their own JSON on stdin; [`hook_input`] reshapes it
//! into the Claude Code form that [`crate::hooks::handle`] understands, so
//! both editors share one snapshot path.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{Error, IoContext, Result};

/// `agentvc hook <event>` names used by Cursor's hooks, and the Cursor hook
/// each one is installed under.
pub const EVENTS: &[(&str, &str)] = &[
    ("afterFileEdit", "cursor-after-file-edit"),
    ("stop", "cursor-stop"),
];

/// Marker on the first line of the rule body, so `--remove` only deletes a
/// rule file we wrote.
const RULE_MARKER: &str = "<!-- written by agentvc integrate cursor -->";

const RULE: &str = r#"---
description: agentvc attempt workflow. Applies whenever this repository has an .agentvc directory.
alwaysApply: true
---
<!-- written by agentvc integrate cursor -->
# Working in an agentvc space

This repository uses agentvc. Every try at a task is an **attempt** with its
own copy-on-write directory (its **view**). Your edits are snapshotted after
each file edit and at the end of each turn, so history is kept for you. Ids:
tasks `t1`, attempts `a3`, snapshots `s7`.

Use the `agentvc` MCP tools (or the `agentvc` CLI with `--json`):

- `log` shows the attempt tree; `show` one attempt and its `view_path`.
- `fork` (with `task`, `from`, `prompt`) starts a new attempt in its own view.
  Fork before a risky refactor, an alternative design, or a speculative fix.
  To try two approaches, fork twice from the same parent.
- `snapshot` with meta at milestones, e.g. `{"tests": "pass"}`.
- `mark` an attempt `passed` or `failed` when you know how it went.
- Leave `promote`, `abandon` and `pr` to the human unless asked.

Rules:

- Work only inside your attempt's view (`.agentvc/views/aN`), using absolute
  paths in it. The main checkout is the root attempt `a1`.
- Background or parallel agents: each one forks its own attempt first
  (`fork` with `prompt` set to its goal) and works only in that view, so
  parallel agents never edit the same files.
- Never edit `.agentvc/` or another attempt's view.
- Report progress with links to the review UI (`agentvcd`, default
  http://localhost:7420): `/attempts/N`, `/tasks/N`, `/compare?a=3,5`.
"#;

/// What happened to one file.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileAction {
    Created,
    Updated,
    Unchanged,
    Removed,
    /// `--remove` left the file because other settings remain in it.
    Kept,
    Absent,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub action: FileAction,
}

#[derive(Clone, Debug, Serialize)]
pub struct IntegrateResult {
    pub editor: &'static str,
    pub removed: bool,
    pub files: Vec<FileChange>,
}

pub fn mcp_path(root: &Path) -> PathBuf {
    root.join(".cursor/mcp.json")
}
pub fn hooks_path(root: &Path) -> PathBuf {
    root.join(".cursor/hooks.json")
}
pub fn rule_path(root: &Path) -> PathBuf {
    root.join(".cursor/rules/agentvc.mdc")
}

/// Write (or with `remove`, revert) Cursor's config for the space at `root`.
/// `exe` is the command Cursor runs: a bare `agentvc` when it is on `PATH`.
pub fn integrate(root: &Path, exe: &str, remove: bool) -> Result<IntegrateResult> {
    let files = if remove {
        vec![
            remove_json(&mcp_path(root), strip_mcp)?,
            remove_json(&hooks_path(root), strip_hooks)?,
            remove_rule(&rule_path(root))?,
        ]
    } else {
        vec![
            write_json(&mcp_path(root), |v| add_mcp(v, exe))?,
            write_json(&hooks_path(root), |v| add_hooks(v, exe))?,
            write_text(&rule_path(root), RULE)?,
        ]
    };
    if remove {
        // Directories we created, once nothing else is in them.
        for d in [root.join(".cursor/rules"), root.join(".cursor")] {
            let _ = fs::remove_dir(d);
        }
    }
    Ok(IntegrateResult {
        editor: "cursor",
        removed: remove,
        files,
    })
}

fn quote(exe: &str) -> String {
    if exe.contains(' ') {
        format!("\"{exe}\"")
    } else {
        exe.to_string()
    }
}

fn add_mcp(v: &mut Value, exe: &str) {
    let servers = obj(v).entry("mcpServers").or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    servers["agentvc"] = json!({
        "command": exe,
        "args": ["mcp", "--space", "${workspaceFolder}"],
    });
}

fn strip_mcp(v: &mut Value) {
    if let Some(s) = v.get_mut("mcpServers").and_then(Value::as_object_mut) {
        s.remove("agentvc");
        if s.is_empty() {
            obj(v).remove("mcpServers");
        }
    }
}

fn is_ours(cmd: &str) -> bool {
    cmd.contains("agentvc")
        && EVENTS
            .iter()
            .any(|(_, ev)| cmd.ends_with(&format!(" hook {ev}")))
}

fn add_hooks(v: &mut Value, exe: &str) {
    strip_hooks(v);
    let o = obj(v);
    o.entry("version").or_insert(json!(1));
    let hooks = o.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    for (cursor_event, ours) in EVENTS {
        let list = hooks
            .as_object_mut()
            .expect("hooks is an object")
            .entry(*cursor_event)
            .or_insert_with(|| json!([]));
        if let Some(arr) = list.as_array_mut() {
            arr.push(json!({ "command": format!("{} hook {ours}", quote(exe)) }));
        }
    }
}

fn strip_hooks(v: &mut Value) {
    let Some(hooks) = v.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for list in hooks.values_mut() {
        if let Some(arr) = list.as_array_mut() {
            arr.retain(|h| {
                !h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_ours)
            });
        }
    }
    hooks.retain(|_, l| l.as_array().is_none_or(|a| !a.is_empty()));
    if hooks.is_empty() {
        let o = obj(v);
        o.remove("hooks");
        // `version` alone is ours too: nothing else uses the file.
        if o.len() == 1 && o.contains_key("version") {
            o.remove("version");
        }
    }
}

fn obj(v: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !v.is_object() {
        *v = json!({});
    }
    v.as_object_mut().expect("object")
}

fn read_json(path: &Path) -> Result<Option<Value>> {
    match fs::read_to_string(path) {
        Ok(t) if t.trim().is_empty() => Ok(Some(json!({}))),
        Ok(t) => serde_json::from_str(&t).map(Some).map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).at(path),
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).expect("json serializes") + "\n"
}

fn write_json(path: &Path, f: impl FnOnce(&mut Value)) -> Result<FileChange> {
    let before = read_json(path)?;
    let mut v = before.clone().unwrap_or_else(|| json!({}));
    f(&mut v);
    let action = match &before {
        None => FileAction::Created,
        Some(b) if *b == v => FileAction::Unchanged,
        Some(_) => FileAction::Updated,
    };
    if action != FileAction::Unchanged {
        write_text(path, &pretty(&v))?;
    }
    Ok(FileChange {
        path: path.to_path_buf(),
        action,
    })
}

fn write_text(path: &Path, text: &str) -> Result<FileChange> {
    let before = fs::read_to_string(path).ok();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).at(dir)?;
    }
    let action = match before.as_deref() {
        None => FileAction::Created,
        Some(b) if b == text => FileAction::Unchanged,
        Some(_) => FileAction::Updated,
    };
    if action != FileAction::Unchanged {
        fs::write(path, text).at(path)?;
    }
    Ok(FileChange {
        path: path.to_path_buf(),
        action,
    })
}

fn remove_json(path: &Path, strip: fn(&mut Value)) -> Result<FileChange> {
    let Some(mut v) = read_json(path)? else {
        return Ok(FileChange {
            path: path.to_path_buf(),
            action: FileAction::Absent,
        });
    };
    let before = v.clone();
    strip(&mut v);
    let action = if v.as_object().is_some_and(|o| o.is_empty()) {
        fs::remove_file(path).at(path)?;
        FileAction::Removed
    } else if v != before {
        write_text(path, &pretty(&v))?;
        FileAction::Kept
    } else {
        FileAction::Unchanged
    };
    Ok(FileChange {
        path: path.to_path_buf(),
        action,
    })
}

fn remove_rule(path: &Path) -> Result<FileChange> {
    let action = match fs::read_to_string(path) {
        Ok(t) if t.contains(RULE_MARKER) => {
            fs::remove_file(path).at(path)?;
            FileAction::Removed
        }
        Ok(_) => FileAction::Kept,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileAction::Absent,
        Err(e) => return Err(e).at(path),
    };
    Ok(FileChange {
        path: path.to_path_buf(),
        action,
    })
}

/// Reshape a Cursor hook's stdin into the Claude Code form that
/// [`crate::hooks::handle`] takes, returning the event to handle it as.
/// Returns `None` for an event that is not one of ours.
///
/// Cursor sends `workspace_roots`, `conversation_id`, `generation_id`, and
/// for `afterFileEdit` the edited `file_path`.
pub fn hook_input(event: &str, input: &Value) -> Option<(&'static str, Value)> {
    let root = input
        .get("workspace_roots")
        .and_then(Value::as_array)
        .and_then(|r| r.first())
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut out = json!({});
    if let Some(r) = &root {
        out["cwd"] = json!(r);
    }
    if let Some(c) = input.get("conversation_id") {
        out["session_id"] = c.clone();
    }
    match event {
        "cursor-after-file-edit" => {
            out["tool_name"] = json!("cursor:edit");
            if let Some(f) = input.get("file_path").and_then(Value::as_str) {
                let p = Path::new(f);
                let abs = match (&root, p.is_absolute()) {
                    (Some(r), false) => Path::new(r).join(p),
                    _ => p.to_path_buf(),
                };
                out["tool_input"] = json!({ "file_path": abs });
            }
            Some(("post-tool-use", out))
        }
        "cursor-stop" => Some(("stop", out)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{handle, HookOutcome};
    use crate::space::{ForkRequest, Space};

    fn listing(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        for e in walk(root) {
            out.push(e.strip_prefix(root).unwrap().display().to_string());
        }
        out.sort();
        out
    }

    fn walk(d: &Path) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Ok(rd) = fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                v.push(p.clone());
                if p.is_dir() {
                    v.extend(walk(&p));
                }
            }
        }
        v
    }

    #[test]
    fn integrate_writes_three_files_and_remove_reverts_them() {
        let tmp = crate::testutil::tempdir();
        let root = tmp.path();
        let r = integrate(root, "agentvc", false).unwrap();
        assert!(r.files.iter().all(|f| f.action == FileAction::Created));
        let mcp: Value =
            serde_json::from_str(&fs::read_to_string(mcp_path(root)).unwrap()).unwrap();
        assert_eq!(mcp["mcpServers"]["agentvc"]["args"][0], "mcp");
        let hooks: Value =
            serde_json::from_str(&fs::read_to_string(hooks_path(root)).unwrap()).unwrap();
        assert_eq!(hooks["version"], 1);
        assert_eq!(
            hooks["hooks"]["afterFileEdit"][0]["command"],
            "agentvc hook cursor-after-file-edit"
        );
        // Idempotent.
        let again = integrate(root, "agentvc", false).unwrap();
        assert!(again
            .files
            .iter()
            .all(|f| f.action == FileAction::Unchanged));

        let r = integrate(root, "agentvc", true).unwrap();
        assert!(
            r.files.iter().all(|f| f.action == FileAction::Removed),
            "{r:?}"
        );
        assert!(listing(root).is_empty(), "{:?}", listing(root));
    }

    #[test]
    fn remove_keeps_the_users_own_settings() {
        let tmp = crate::testutil::tempdir();
        let root = tmp.path();
        fs::create_dir_all(root.join(".cursor/rules")).unwrap();
        let mine_mcp = json!({ "mcpServers": { "github": { "command": "gh-mcp" } } });
        let mine_hooks =
            json!({ "version": 1, "hooks": { "afterFileEdit": [{ "command": "./fmt.sh" }] } });
        fs::write(mcp_path(root), pretty(&mine_mcp)).unwrap();
        fs::write(hooks_path(root), pretty(&mine_hooks)).unwrap();
        fs::write(root.join(".cursor/rules/style.mdc"), "be nice").unwrap();

        let r = integrate(root, "/opt/my tools/agentvc", false).unwrap();
        assert_eq!(r.files[0].action, FileAction::Updated);
        let hooks: Value =
            serde_json::from_str(&fs::read_to_string(hooks_path(root)).unwrap()).unwrap();
        assert_eq!(hooks["hooks"]["afterFileEdit"].as_array().unwrap().len(), 2);
        assert_eq!(
            hooks["hooks"]["stop"][0]["command"],
            "\"/opt/my tools/agentvc\" hook cursor-stop"
        );

        integrate(root, "/opt/my tools/agentvc", true).unwrap();
        let back =
            |p: &Path| -> Value { serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap() };
        assert_eq!(back(&mcp_path(root)), mine_mcp);
        assert_eq!(back(&hooks_path(root)), mine_hooks);
        assert!(root.join(".cursor/rules/style.mdc").exists());
        assert!(!rule_path(root).exists());
    }

    #[test]
    fn remove_leaves_a_rule_we_did_not_write() {
        let tmp = crate::testutil::tempdir();
        fs::create_dir_all(tmp.path().join(".cursor/rules")).unwrap();
        fs::write(rule_path(tmp.path()), "my own agentvc notes").unwrap();
        let r = integrate(tmp.path(), "agentvc", true).unwrap();
        assert_eq!(r.files[2].action, FileAction::Kept);
        assert!(rule_path(tmp.path()).exists());
    }

    #[test]
    fn cursor_edit_hook_snapshots_the_view_of_the_edited_file() {
        let tmp = crate::testutil::tempdir();
        fs::write(tmp.path().join("f"), "1").unwrap();
        let space = Space::init(tmp.path()).unwrap();
        let a = space.fork(&ForkRequest::default()).unwrap().attempt;
        fs::write(a.view_path.join("f"), "2").unwrap();
        // Cursor's window is on the main checkout; the agent edited the view.
        let cursor = json!({
            "hook_event_name": "afterFileEdit",
            "conversation_id": "c1",
            "workspace_roots": [tmp.path()],
            "file_path": a.view_path.join("f"),
            "edits": [{ "old_string": "1", "new_string": "2" }],
        });
        let (event, input) = hook_input("cursor-after-file-edit", &cursor).unwrap();
        let HookOutcome::Snapshot(r) = handle(event, &input, Path::new("/")).unwrap() else {
            panic!("expected a snapshot");
        };
        assert_eq!(r.snapshot.attempt_id, a.id);
        assert_eq!(r.snapshot.trigger, "tool:cursor:edit");
        assert_eq!(r.snapshot.meta.as_ref().unwrap()["session_id"], "c1");

        // Stop in a window opened on the view records the turn boundary.
        let stop = json!({ "workspace_roots": [a.view_path], "status": "completed" });
        let (event, input) = hook_input("cursor-stop", &stop).unwrap();
        let HookOutcome::Snapshot(r) = handle(event, &input, Path::new("/")).unwrap() else {
            panic!("expected a snapshot");
        };
        assert_eq!(
            (r.snapshot.attempt_id, r.snapshot.trigger.as_str()),
            (a.id, "stop")
        );
    }

    #[test]
    fn relative_file_paths_resolve_against_the_workspace() {
        let (_, v) = hook_input(
            "cursor-after-file-edit",
            &json!({ "workspace_roots": ["/w"], "file_path": "src/a.rs" }),
        )
        .unwrap();
        assert_eq!(
            Path::new(v["tool_input"]["file_path"].as_str().unwrap()),
            Path::new("/w/src/a.rs")
        );
        assert!(hook_input("post-tool-use", &json!({})).is_none());
    }
}
