//! What a Claude Code agent inherits from the person's own Claude Code
//! setup, and the guard rails agentvc adds on top.
//!
//! An agent runs `claude -p` in its own worktree, which lives outside the
//! workspace's checkout. Claude Code loads the user's settings
//! (`~/.claude/settings.json`), their user MCP servers and the repo's
//! checked-in `.claude/settings.json` and `.mcp.json` by itself, but the
//! parts keyed by the workspace's *path* would be lost:
//!
//! - `.claude/settings.local.json` (git-ignored allowlists, hooks, env),
//! - the project entry of `~/.claude.json`: local MCP servers, approved
//!   `.mcp.json` servers and old-style `allowedTools`.
//!
//! [`read`] finds those for a workspace and [`ClaudeSetup::settings_arg`] /
//! [`ClaudeSetup::mcp_servers_arg`] hand them to the agent with
//! `--settings` and `--mcp-config`. Two things are never inherited, so the
//! agent stays in its worktree: `permissions.additionalDirectories` and
//! `permissions.defaultMode` (the workspace's agent setting picks the mode).
//!
//! On top, agentvc adds [`guard rails`](GUARD_RAILS): a `PreToolUse` hook
//! ([`guard`]) that denies file edits outside the worktree or inside git's
//! metadata, a deny rule for `git push`, and optionally Claude Code's
//! command sandbox.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Map, Value};

/// Environment variable holding the agent's worktree, for [`guard`].
pub const ENV_WORKTREE: &str = "AGENTVC_WORKTREE";

/// The rules agentvc adds to every Claude Code agent, in words.
pub const GUARD_RAILS: &[&str] = &[
    "File edits only inside the agent's own worktree (outside paths and .git are refused)",
    "No git push: agentvc commits and lands the work after you review it",
    "Extra directories from your settings (additionalDirectories) are not passed on",
];

/// One file of the person's setup that agentvc looked at.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SetupSource {
    /// What it is, in words.
    pub label: String,
    pub path: PathBuf,
    pub found: bool,
    /// How the agent gets it: `claude` (Claude Code reads it itself, the
    /// repo copy from the agent's worktree) or `agentvc` (passed on).
    pub via: String,
}

/// An MCP server the agent will have.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    /// `user`, `local` (this checkout, from ~/.claude.json), `project`
    /// (`.mcp.json`) or `agentvc`.
    pub scope: String,
}

/// What an agent started in a workspace inherits.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ClaudeSetup {
    pub sources: Vec<SetupSource>,
    /// Permission rules, merged over every source.
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
    pub mcp_servers: Vec<McpServer>,
    /// Settings that were left out so the agent stays in its worktree.
    pub dropped: Vec<String>,
    /// `--settings` for the agent: the workspace-local bits.
    #[serde(skip)]
    local_settings: Map<String, Value>,
    /// Local-scope MCP servers to add with `--mcp-config`.
    #[serde(skip)]
    local_mcp: Map<String, Value>,
}

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn claude_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    home().map(|h| h.join(".claude"))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// `~/.claude.json` (inside `$CLAUDE_CONFIG_DIR` when that is set).
fn claude_json() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join(".claude.json"));
    }
    home().map(|h| h.join(".claude.json"))
}

fn read_json(p: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(p).ok()?;
    match serde_json::from_str::<Value>(&text).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn push_unique(into: &mut Vec<String>, more: Vec<String>) {
    for s in more {
        if !into.contains(&s) {
            into.push(s);
        }
    }
}

impl ClaudeSetup {
    fn add_rules(&mut self, settings: &Map<String, Value>) {
        let p = settings.get("permissions");
        push_unique(&mut self.allow, strings(p.and_then(|p| p.get("allow"))));
        push_unique(&mut self.ask, strings(p.and_then(|p| p.get("ask"))));
        push_unique(&mut self.deny, strings(p.and_then(|p| p.get("deny"))));
    }

    fn add_mcp(&mut self, servers: Option<&Value>, scope: &str) {
        if let Some(m) = servers.and_then(Value::as_object) {
            for name in m.keys() {
                if !self.mcp_servers.iter().any(|s| &s.name == name) {
                    self.mcp_servers.push(McpServer {
                        name: name.clone(),
                        scope: scope.into(),
                    });
                }
            }
        }
    }

    /// The JSON for `--settings`: what was found for the workspace plus
    /// agentvc's guard rails. `exe` is agentvc, for the guard hook.
    pub fn settings_arg(&self, exe: &Path, sandbox: bool) -> Value {
        let mut s = self.local_settings.clone();
        let perms = s
            .entry("permissions")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        let mut perms = perms;
        let mut deny = strings(perms.get("deny"));
        push_unique(
            &mut deny,
            vec!["Bash(git push:*)".into(), "Bash(git push)".into()],
        );
        perms.insert("deny".into(), json!(deny));
        s.insert("permissions".into(), Value::Object(perms));
        let guard = json!({
            "matcher": "Edit|MultiEdit|Write|NotebookEdit",
            "hooks": [{
                "type": "command",
                "command": format!("{} agent guard", shell_quote(&exe.to_string_lossy())),
            }],
        });
        let hooks = s
            .entry("hooks")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        let mut hooks = hooks;
        let mut pre = hooks
            .get("PreToolUse")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        pre.insert(0, guard);
        hooks.insert("PreToolUse".into(), Value::Array(pre));
        s.insert("hooks".into(), Value::Object(hooks));
        if sandbox {
            let mut sb = s
                .get("sandbox")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            sb.insert("enabled".into(), json!(true));
            sb.entry("autoAllowBashIfSandboxed").or_insert(json!(true));
            s.insert("sandbox".into(), Value::Object(sb));
        }
        Value::Object(s)
    }

    /// The local-scope MCP servers to add next to agentvc's own.
    pub fn mcp_servers_arg(&self) -> &Map<String, Value> {
        &self.local_mcp
    }
}

/// Quote `s` for `sh -c` (hooks run through the shell).
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The person's Claude Code setup as an agent started from the workspace
/// checked out at `workspace` sees it.
pub fn read(workspace: &Path) -> ClaudeSetup {
    let mut out = ClaudeSetup::default();
    let ws = crate::stack::canon(workspace);

    // 1. User settings: Claude Code reads them itself.
    if let Some(d) = claude_dir() {
        let p = d.join("settings.json");
        let found = read_json(&p);
        if let Some(s) = &found {
            out.add_rules(s);
        }
        out.sources.push(SetupSource {
            label: "Your Claude Code settings".into(),
            path: p,
            found: found.is_some(),
            via: "claude".into(),
        });
    }

    // 2. The repo's checked-in settings: in the agent's worktree too.
    let p = ws.join(".claude").join("settings.json");
    let found = read_json(&p);
    if let Some(s) = &found {
        out.add_rules(s);
    }
    out.sources.push(SetupSource {
        label: "Repo settings (.claude/settings.json)".into(),
        path: p,
        found: found.is_some(),
        via: "claude".into(),
    });

    // 3. This checkout's local settings: git-ignored, so passed on.
    let p = ws.join(".claude").join("settings.local.json");
    let found = read_json(&p);
    if let Some(mut s) = found.clone() {
        if let Some(perms) = s.get_mut("permissions").and_then(Value::as_object_mut) {
            for k in ["additionalDirectories", "defaultMode"] {
                if perms.remove(k).is_some() {
                    out.dropped.push(format!("permissions.{k}"));
                }
            }
        }
        out.add_rules(&s);
        out.local_settings = s;
    }
    out.sources.push(SetupSource {
        label: "This checkout's local settings (.claude/settings.local.json)".into(),
        path: p,
        found: found.is_some(),
        via: "agentvc".into(),
    });

    // 4. ~/.claude.json: user MCP servers (read by Claude Code), and this
    // checkout's project entry (passed on).
    if let Some(p) = claude_json() {
        let cj = read_json(&p);
        if let Some(cj) = &cj {
            out.add_mcp(cj.get("mcpServers"), "user");
            let entry = cj.get("projects").and_then(Value::as_object).and_then(|m| {
                m.iter()
                    .find(|(k, _)| crate::stack::canon(Path::new(k)) == ws)
                    .map(|(_, v)| v.clone())
            });
            if let Some(Value::Object(e)) = entry {
                if let Some(Value::Object(m)) = e.get("mcpServers") {
                    for (k, v) in m {
                        out.local_mcp.insert(k.clone(), v.clone());
                    }
                }
                out.add_mcp(e.get("mcpServers"), "local");
                let allowed = strings(e.get("allowedTools"));
                if !allowed.is_empty() {
                    push_unique(&mut out.allow, allowed.clone());
                    let perms = out
                        .local_settings
                        .entry("permissions")
                        .or_insert_with(|| json!({}));
                    if let Some(perms) = perms.as_object_mut() {
                        let mut a = strings(perms.get("allow"));
                        push_unique(&mut a, allowed);
                        perms.insert("allow".into(), json!(a));
                    }
                }
                for k in [
                    "enabledMcpjsonServers",
                    "disabledMcpjsonServers",
                    "enableAllProjectMcpServers",
                ] {
                    if let Some(v) = e.get(k) {
                        out.local_settings.entry(k).or_insert(v.clone());
                    }
                }
            }
        }
        out.sources.push(SetupSource {
            label: "Your MCP servers and this checkout's approvals (~/.claude.json)".into(),
            path: p,
            found: cj.is_some(),
            via: "agentvc".into(),
        });
    }
    let all = out
        .local_settings
        .get("enableAllProjectMcpServers")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let on = strings(out.local_settings.get("enabledMcpjsonServers"));
    let off = strings(out.local_settings.get("disabledMcpjsonServers"));

    // 5. The repo's .mcp.json: Claude Code reads the worktree's copy; its
    // servers run when this checkout approved them.
    let p = ws.join(".mcp.json");
    if let Some(m) = read_json(&p) {
        if let Some(servers) = m.get("mcpServers").and_then(Value::as_object) {
            for name in servers.keys() {
                if (all || on.contains(name)) && !off.contains(name) {
                    out.mcp_servers.push(McpServer {
                        name: name.clone(),
                        scope: "project".into(),
                    });
                }
            }
        }
    }
    out.mcp_servers.push(McpServer {
        name: "agentvc".into(),
        scope: "agentvc".into(),
    });
    out
}

/// Lexically resolve `p` (joined to `base` when relative), then resolve
/// symlinks of the longest part that exists.
fn resolve(base: &Path, p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    let mut clean = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other),
        }
    }
    let mut existing = clean.clone();
    let mut rest = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(n) => {
                rest.push(n.to_os_string());
                existing.pop();
            }
            None => break,
        }
    }
    let mut out = std::fs::canonicalize(&existing).unwrap_or(existing);
    for n in rest.into_iter().rev() {
        out.push(n);
    }
    out
}

/// The `PreToolUse` guard: given Claude Code's hook input, why the tool
/// call must be refused, if it must.
pub fn guard(input: &Value, worktree: &Path) -> Option<String> {
    let ti = input.get("tool_input")?;
    let path = ["file_path", "notebook_path", "path"]
        .iter()
        .find_map(|k| ti.get(*k).and_then(Value::as_str))?;
    let base = input
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| worktree.to_path_buf());
    let root = std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
    let target = resolve(&base, Path::new(path));
    let Ok(inside) = target.strip_prefix(&root) else {
        return Some(format!(
            "{path} is outside your worktree ({}). Agents started from agentvc may only change files in their own worktree.",
            root.display()
        ));
    };
    if inside.components().any(|c| c.as_os_str() == ".git") {
        return Some(format!(
            "{path} is git metadata; agentvc commits your work for you when you stop."
        ));
    }
    None
}

/// The hook's answer to Claude Code for a refused call.
pub fn guard_deny(reason: &str) -> Value {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_keeps_edits_in_the_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("ag1");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        let call = |p: &str| {
            guard(
                &json!({ "cwd": wt, "tool_name": "Write", "tool_input": { "file_path": p } }),
                &wt,
            )
        };
        assert_eq!(call(&wt.join("src/new.rs").to_string_lossy()), None);
        assert_eq!(call("src/deeper/new.rs"), None, "relative to cwd");
        assert!(call(&tmp.path().join("other/x").to_string_lossy()).is_some());
        assert!(call("../ag2/x").is_some(), "no escaping with ..");
        assert!(call("/etc/passwd").is_some());
        assert!(call(".git/config").is_some(), "no git metadata");
        // Not a file tool: nothing to say.
        assert_eq!(
            guard(&json!({ "tool_input": { "command": "ls" } }), &wt),
            None
        );
    }

    #[test]
    fn settings_arg_adds_guard_rails_over_local_settings() {
        let s = ClaudeSetup {
            local_settings: json!({
                "permissions": { "allow": ["Bash(cargo test:*)"], "deny": ["Read(.env)"] },
                "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [] }] },
            })
            .as_object()
            .unwrap()
            .clone(),
            ..Default::default()
        };
        let v = s.settings_arg(Path::new("/opt/agent vc/agentvc"), true);
        assert_eq!(v["permissions"]["allow"], json!(["Bash(cargo test:*)"]));
        let deny = strings(v["permissions"].get("deny"));
        assert!(deny.contains(&"Read(.env)".to_string()));
        assert!(deny.contains(&"Bash(git push:*)".to_string()));
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2, "the person's hooks are kept");
        assert_eq!(
            pre[0]["hooks"][0]["command"],
            json!("'/opt/agent vc/agentvc' agent guard")
        );
        assert_eq!(v["sandbox"]["enabled"], json!(true));
    }

    #[test]
    fn reads_the_checkouts_local_setup() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("repo");
        let cfg = tmp.path().join("cfg");
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            ws.join(".claude/settings.local.json"),
            r#"{"permissions":{"allow":["Bash(npm test)"],"additionalDirectories":["/"],"defaultMode":"bypassPermissions"}}"#,
        )
        .unwrap();
        std::fs::write(
            ws.join(".mcp.json"),
            r#"{"mcpServers":{"db":{"command":"db-mcp"},"off":{"command":"x"}}}"#,
        )
        .unwrap();
        std::fs::write(
            cfg.join("settings.json"),
            r#"{"permissions":{"allow":["Bash(git status)"]}}"#,
        )
        .unwrap();
        let canon_ws = std::fs::canonicalize(&ws).unwrap();
        std::fs::write(
            cfg.join(".claude.json"),
            json!({
                "mcpServers": { "linear": { "command": "linear" } },
                "projects": { canon_ws.to_string_lossy(): {
                    "mcpServers": { "local-db": { "command": "pg" } },
                    "allowedTools": ["Bash(make)"],
                    "enabledMcpjsonServers": ["db"],
                } },
            })
            .to_string(),
        )
        .unwrap();
        // CLAUDE_CONFIG_DIR is process-wide; only this test sets it.
        std::env::set_var("CLAUDE_CONFIG_DIR", &cfg);
        let s = read(&ws);
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        assert_eq!(
            s.allow,
            vec!["Bash(git status)", "Bash(npm test)", "Bash(make)"]
        );
        assert_eq!(
            s.dropped,
            vec![
                "permissions.additionalDirectories",
                "permissions.defaultMode"
            ]
        );
        let names: Vec<_> = s
            .mcp_servers
            .iter()
            .map(|m| format!("{}:{}", m.scope, m.name))
            .collect();
        assert_eq!(
            names,
            vec![
                "user:linear",
                "local:local-db",
                "project:db",
                "agentvc:agentvc"
            ]
        );
        assert!(s.mcp_servers_arg().contains_key("local-db"));
        let arg = s.settings_arg(Path::new("agentvc"), false);
        assert_eq!(arg["enabledMcpjsonServers"], json!(["db"]));
        assert!(arg["permissions"].get("additionalDirectories").is_none());
        assert!(arg.get("sandbox").is_none());
    }
}
