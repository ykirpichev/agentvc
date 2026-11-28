//! The model each runner's own tool picks when agentvc passes no `--model`.
//!
//! An agent whose settings leave the model empty runs the tool with no
//! `--model`, so it uses whatever the person chose there: `/model` in Claude
//! Code (CLI or the VS Code / Cursor extension, which saves it to
//! `~/.claude/settings.json`), or `model` in Codex's `config.toml`. The review
//! UI shows that model as the default, so "Default" says what will run.
//!
//! Cursor's chat model lives in Cursor's private state and is not readable;
//! the `cursor` runner reports nothing.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// A tool's own model choice and where it was found.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ToolModel {
    pub model: String,
    /// `ANTHROPIC_MODEL`, `.claude/settings.json`, `~/.claude/settings.json`, ...
    pub source: String,
}

/// Where to look: the workspace folder, the home folder and the environment.
pub struct Lookup<'a> {
    pub workspace: &'a Path,
    pub home: Option<PathBuf>,
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

/// The tool models for agents started in `workspace`, keyed by runner.
pub fn tool_models(workspace: &Path) -> BTreeMap<String, ToolModel> {
    let env = |k: &str| std::env::var(k).ok();
    resolve(&Lookup {
        workspace,
        home: std::env::var_os("HOME").map(PathBuf::from),
        env: &env,
    })
}

pub fn resolve(l: &Lookup) -> BTreeMap<String, ToolModel> {
    let mut out = BTreeMap::new();
    if let Some(m) = claude(l) {
        out.insert("claude".to_string(), m);
    }
    if let Some(m) = codex(l) {
        out.insert("codex".to_string(), m);
    }
    out
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Claude Code's order: `ANTHROPIC_MODEL`, then the `model` of local
/// project, project and user settings (managed settings are not read).
fn claude(l: &Lookup) -> Option<ToolModel> {
    if let Some(m) = nonempty((l.env)("ANTHROPIC_MODEL")) {
        return Some(ToolModel {
            model: m,
            source: "ANTHROPIC_MODEL".into(),
        });
    }
    let user_dir = nonempty((l.env)("CLAUDE_CONFIG_DIR"))
        .map(PathBuf::from)
        .or_else(|| l.home.as_ref().map(|h| h.join(".claude")));
    let mut files: Vec<(PathBuf, String)> = vec![
        (
            l.workspace.join(".claude/settings.local.json"),
            ".claude/settings.local.json".into(),
        ),
        (
            l.workspace.join(".claude/settings.json"),
            ".claude/settings.json".into(),
        ),
    ];
    if let Some(d) = user_dir {
        let shown = match &l.home {
            Some(h) if d.starts_with(h) => {
                format!(
                    "~/{}",
                    d.strip_prefix(h).unwrap().join("settings.json").display()
                )
            }
            _ => d.join("settings.json").display().to_string(),
        };
        // Forward slashes on every OS: this is shown to people, not opened.
        files.push((d.join("settings.json"), shown.replace('\\', "/")));
    }
    files.into_iter().find_map(|(path, source)| {
        let text = fs::read_to_string(&path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let m = nonempty(v.get("model")?.as_str().map(str::to_string))?;
        // `default` clears the choice: the account's default runs.
        (m != "default").then_some(ToolModel { model: m, source })
    })
}

/// Codex: the top-level `model = "..."` of `$CODEX_HOME/config.toml`
/// (default `~/.codex`).
fn codex(l: &Lookup) -> Option<ToolModel> {
    let (dir, shown) = match nonempty((l.env)("CODEX_HOME")) {
        Some(d) => (PathBuf::from(&d), format!("{d}/config.toml")),
        None => (
            l.home.as_ref()?.join(".codex"),
            "~/.codex/config.toml".into(),
        ),
    };
    let text = fs::read_to_string(dir.join("config.toml")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            break; // top-level keys come before the first table
        }
        let Some(rest) = line.strip_prefix("model") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.split('#').next().unwrap_or("").trim();
        let value = value.trim_matches(|c| c == '"' || c == '\'');
        if let Some(m) = nonempty(Some(value.to_string())) {
            return Some(ToolModel {
                model: m,
                source: shown,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn run(ws: &Path, home: &Path, env: &[(&str, &str)]) -> BTreeMap<String, ToolModel> {
        let env: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let get = move |k: &str| env.get(k).cloned();
        resolve(&Lookup {
            workspace: ws,
            home: Some(home.to_path_buf()),
            env: &get,
        })
    }

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn claude_follows_settings_order() {
        let t = tempfile::tempdir().unwrap();
        let (ws, home) = (t.path().join("ws"), t.path().join("home"));
        assert!(run(&ws, &home, &[]).is_empty());

        write(&home.join(".claude/settings.json"), r#"{"model":"sonnet"}"#);
        let m = &run(&ws, &home, &[])["claude"];
        assert_eq!(
            (m.model.as_str(), m.source.as_str()),
            ("sonnet", "~/.claude/settings.json")
        );

        write(
            &ws.join(".claude/settings.json"),
            r#"{"model":"opus","hooks":{}}"#,
        );
        assert_eq!(
            run(&ws, &home, &[])["claude"].source,
            ".claude/settings.json"
        );

        write(
            &ws.join(".claude/settings.local.json"),
            r#"{"model":"haiku"}"#,
        );
        assert_eq!(run(&ws, &home, &[])["claude"].model, "haiku");

        let m = &run(&ws, &home, &[("ANTHROPIC_MODEL", "opus[1m]")])["claude"];
        assert_eq!(
            (m.model.as_str(), m.source.as_str()),
            ("opus[1m]", "ANTHROPIC_MODEL")
        );
    }

    #[test]
    fn claude_default_and_bad_files_fall_through() {
        let t = tempfile::tempdir().unwrap();
        let (ws, home) = (t.path().join("ws"), t.path().join("home"));
        write(&ws.join(".claude/settings.local.json"), "not json");
        write(&ws.join(".claude/settings.json"), r#"{"model":"default"}"#);
        write(
            &home.join(".claude/settings.json"),
            r#"{"model":"opusplan"}"#,
        );
        assert_eq!(run(&ws, &home, &[])["claude"].model, "opusplan");

        let cfg = t.path().join("cfg");
        write(&cfg.join("settings.json"), r#"{"model":"sonnet[1m]"}"#);
        let m = &run(&ws, &home, &[("CLAUDE_CONFIG_DIR", cfg.to_str().unwrap())])["claude"];
        assert_eq!(m.model, "sonnet[1m]");
        assert!(m.source.ends_with("cfg/settings.json"));
    }

    #[test]
    fn codex_reads_top_level_model() {
        let t = tempfile::tempdir().unwrap();
        let (ws, home) = (t.path().join("ws"), t.path().join("home"));
        write(
            &home.join(".codex/config.toml"),
            "# mine\nmodel_provider = \"openai\"\nmodel = \"gpt-5-codex\" # fast\n\n[profiles.x]\nmodel = \"o3\"\n",
        );
        let m = &run(&ws, &home, &[])["codex"];
        assert_eq!(
            (m.model.as_str(), m.source.as_str()),
            ("gpt-5-codex", "~/.codex/config.toml")
        );

        write(
            &home.join(".codex/config.toml"),
            "[profiles.x]\nmodel = \"o3\"\n",
        );
        assert!(!run(&ws, &home, &[]).contains_key("codex"));
    }
}
