//! Claude Code integration: hook handlers that snapshot the attempt an agent
//! is working in after each editing tool call, and an installer that writes
//! the hook configuration into Claude Code's settings.
//!
//! Hooks must never break an agent session, so [`handle`] reports "skipped"
//! for anything outside a space and callers are expected to exit 0 on error.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{Error, IoContext, Result};
use crate::space::{SnapshotResult, Space};

/// Tools whose calls can change files. Read-only tools are not hooked.
pub const SNAPSHOT_TOOLS: &str = "Edit|Write|MultiEdit|NotebookEdit|Bash";

/// The `agentvc hook <event>` events we install.
pub const EVENTS: &[(&str, &str, Option<&str>)] = &[
    ("PostToolUse", "post-tool-use", Some(SNAPSHOT_TOOLS)),
    ("Stop", "stop", None),
    ("SubagentStop", "subagent-stop", None),
];

#[derive(Debug, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum HookOutcome {
    Skipped { reason: String },
    Snapshot(SnapshotResult),
}

fn skipped(reason: impl Into<String>) -> Result<HookOutcome> {
    Ok(HookOutcome::Skipped {
        reason: reason.into(),
    })
}

/// Handle one hook event. `input` is the JSON Claude Code sends on stdin;
/// `fallback_cwd` is used when it carries no `cwd`.
pub fn handle(event: &str, input: &Value, fallback_cwd: &Path) -> Result<HookOutcome> {
    let cwd = input
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback_cwd.to_path_buf());
    let space = match Space::discover(&cwd) {
        Ok(s) => s,
        Err(Error::NoSpace(_)) => return skipped("not in an agentvc space"),
        Err(e) => return Err(e),
    };
    // An edit names the file it touched; snapshot the view that holds that
    // file, so an agent whose shell sits in another directory still records
    // its work in the right attempt.
    let target = edited_dir(input).unwrap_or(cwd);
    let attempt = match space.attempt_for_path(&target) {
        Ok(a) => a,
        Err(Error::NotInView(_)) => return skipped("not in an attempt view"),
        Err(e) => return Err(e),
    };
    if !attempt.status.is_open() {
        return skipped(format!("a{} is {}", attempt.id, attempt.status));
    }
    let tool = input.get("tool_name").and_then(Value::as_str);
    let trigger = match (event, tool) {
        ("post-tool-use", Some(t)) => format!("tool:{t}"),
        (e, _) => e.to_string(),
    };
    let mut meta = json!({ "hook": event });
    for key in ["session_id", "transcript_path", "tool_name"] {
        if let Some(v) = input.get(key) {
            meta[key] = v.clone();
        }
    }
    // Tool calls that changed nothing (most Bash calls) don't add a snapshot.
    // Stop events always record one, as the boundary of an agent turn.
    let always = event != "post-tool-use";
    Ok(HookOutcome::Snapshot(space.snapshot_with(
        attempt.id,
        &trigger,
        Some(meta),
        always,
    )?))
}

/// The directory of the file an editing tool call wrote, when the hook input
/// names one by absolute path and that directory exists.
fn edited_dir(input: &Value) -> Option<PathBuf> {
    let tool_input = input.get("tool_input")?;
    let file = ["file_path", "notebook_path"]
        .iter()
        .find_map(|k| tool_input.get(*k).and_then(Value::as_str))?;
    let dir = Path::new(file).parent()?;
    (dir.is_absolute() && dir.is_dir()).then(|| dir.to_path_buf())
}

/// Whether a hook command was written by us.
fn is_ours(cmd: &str) -> bool {
    cmd.contains("agentvc")
        && EVENTS
            .iter()
            .any(|(_, ev, _)| cmd.ends_with(&format!(" hook {ev}")))
}

/// Remove our hook entries from a Claude Code settings object.
fn strip(settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for groups in hooks.values_mut() {
        let Some(arr) = groups.as_array_mut() else {
            continue;
        };
        for group in arr.iter_mut() {
            if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                list.retain(|h| {
                    !h.get("command")
                        .and_then(Value::as_str)
                        .is_some_and(is_ours)
                });
            }
        }
        arr.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|l| !l.is_empty())
        });
    }
    hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
}

fn read_settings(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text).map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).at(path),
    }
}

fn write_settings(path: &Path, settings: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).at(dir)?;
    }
    let text = serde_json::to_string_pretty(settings).expect("json serializes") + "\n";
    fs::write(path, text).at(path)
}

/// Add our hooks to the Claude Code settings file at `path`, keeping every
/// other setting and hook. Running it again replaces our entries.
pub fn install(path: &Path, exe: &Path) -> Result<Value> {
    let mut settings = read_settings(path)?;
    if !settings.is_object() {
        settings = json!({});
    }
    strip(&mut settings);
    let exe = exe.to_string_lossy();
    let quoted = if exe.contains(' ') {
        format!("\"{exe}\"")
    } else {
        exe.into_owned()
    };
    let hooks = settings
        .as_object_mut()
        .expect("object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    for (claude_event, ours, matcher) in EVENTS {
        let mut group = json!({
            "hooks": [{ "type": "command", "command": format!("{quoted} hook {ours}") }]
        });
        if let Some(m) = matcher {
            group["matcher"] = json!(m);
        }
        let list = hooks
            .as_object_mut()
            .expect("hooks is an object")
            .entry(*claude_event)
            .or_insert_with(|| json!([]));
        if let Some(arr) = list.as_array_mut() {
            arr.push(group);
        }
    }
    write_settings(path, &settings)?;
    Ok(settings)
}

/// Whether the Claude Code settings file at `path` has our hooks.
pub fn installed_in(path: &Path) -> bool {
    let Ok(settings) = read_settings(path) else {
        return false;
    };
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    hooks
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|g| g.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|h| h.get("command").and_then(Value::as_str))
        .any(is_ours)
}

/// Write a settings file at `path` that holds only our hooks, for
/// `claude --settings <path>`. Replaces whatever was there.
pub fn write_standalone(path: &Path, exe: &Path) -> Result<Value> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).at(path),
    }
    install(path, exe)
}

/// Remove our hooks from the settings file at `path`.
pub fn uninstall(path: &Path) -> Result<Value> {
    let mut settings = read_settings(path)?;
    strip(&mut settings);
    if settings
        .get("hooks")
        .is_some_and(|h| h.as_object().is_some_and(|o| o.is_empty()))
    {
        settings.as_object_mut().expect("object").remove("hooks");
    }
    write_settings(path, &settings)?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::{ForkRequest, Status};

    #[test]
    fn install_is_idempotent_and_keeps_other_settings() {
        let tmp = crate::testutil::tempdir();
        let path = tmp.path().join(".claude/settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"model":"x","hooks":{"PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"lint"}]}]}}"#,
        )
        .unwrap();
        let exe = Path::new("/usr/local/bin/agentvc");
        install(&path, exe).unwrap();
        let v = install(&path, exe).unwrap();
        assert_eq!(v["model"], "x");
        let post = v["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post.len(), 2, "{v}");
        assert_eq!(post[0]["hooks"][0]["command"], "lint");
        assert_eq!(
            post[1]["hooks"][0]["command"],
            "/usr/local/bin/agentvc hook post-tool-use"
        );
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 1);

        let v = uninstall(&path).unwrap();
        assert_eq!(v["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);
        assert!(v["hooks"].get("Stop").is_none());
    }

    #[test]
    fn installed_in_and_standalone() {
        let tmp = crate::testutil::tempdir();
        let path = tmp.path().join("hooks.json");
        assert!(!installed_in(&path));
        fs::write(&path, r#"{"model":"x"}"#).unwrap();
        let v = write_standalone(&path, Path::new("/bin/agentvc")).unwrap();
        assert!(v.get("model").is_none(), "{v}");
        assert!(installed_in(&path));
        uninstall(&path).unwrap();
        assert!(!installed_in(&path));
    }

    #[test]
    fn handle_snapshots_the_attempt_of_cwd() {
        let tmp = crate::testutil::tempdir();
        fs::write(tmp.path().join("f"), "1").unwrap();
        let space = Space::init(tmp.path()).unwrap();
        let a = space
            .fork(&ForkRequest {
                actor: "agent:claude".into(),
                ..Default::default()
            })
            .unwrap()
            .attempt;
        let input = |cwd: &Path| json!({ "session_id": "s-1", "tool_name": "Edit", "cwd": cwd, "transcript_path": "/t.jsonl" });

        // Nothing changed yet: no new snapshot from a tool call.
        let out = handle("post-tool-use", &input(&a.view_path), Path::new("/")).unwrap();
        assert!(matches!(out, HookOutcome::Snapshot(ref r) if r.unchanged));

        fs::write(a.view_path.join("f"), "2").unwrap();
        let HookOutcome::Snapshot(r) =
            handle("post-tool-use", &input(&a.view_path), Path::new("/")).unwrap()
        else {
            panic!("expected a snapshot");
        };
        assert!(!r.unchanged);
        assert_eq!(r.snapshot.attempt_id, a.id);
        assert_eq!(r.snapshot.trigger, "tool:Edit");
        let meta = r.snapshot.meta.unwrap();
        assert_eq!(meta["session_id"], "s-1");
        assert_eq!(meta["transcript_path"], "/t.jsonl");

        // Stop always records a turn boundary.
        let HookOutcome::Snapshot(r) =
            handle("stop", &input(&a.view_path), Path::new("/")).unwrap()
        else {
            panic!("expected a snapshot");
        };
        assert!(!r.unchanged);
        assert_eq!(r.snapshot.trigger, "stop");

        space
            .set_status(a.id, Status::Abandoned, Some("x"))
            .unwrap();
        assert!(matches!(
            handle("stop", &input(&a.view_path), Path::new("/")).unwrap(),
            HookOutcome::Skipped { .. }
        ));
    }

    #[test]
    fn handle_follows_the_edited_file_into_its_view() {
        let tmp = crate::testutil::tempdir();
        fs::write(tmp.path().join("f"), "1").unwrap();
        let space = Space::init(tmp.path()).unwrap();
        let a = space.fork(&ForkRequest::default()).unwrap().attempt;
        let file = a.view_path.join("f");
        fs::write(&file, "2").unwrap();
        // The shell is still in the main checkout (the root attempt).
        let input = json!({
            "tool_name": "Edit",
            "cwd": tmp.path(),
            "tool_input": { "file_path": file },
        });
        let HookOutcome::Snapshot(r) = handle("post-tool-use", &input, Path::new("/")).unwrap()
        else {
            panic!("expected a snapshot");
        };
        assert_eq!(r.snapshot.attempt_id, a.id);
        assert!(!r.unchanged);
    }

    #[test]
    fn handle_outside_space_is_skipped() {
        let tmp = crate::testutil::tempdir();
        let out = handle("stop", &json!({ "cwd": tmp.path() }), Path::new("/")).unwrap();
        assert!(matches!(out, HookOutcome::Skipped { .. }));
    }
}
