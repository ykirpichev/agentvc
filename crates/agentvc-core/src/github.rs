//! Opening a GitHub pull request from an attempt: export it to a branch, push
//! the branch, and create (or update) the PR through the REST API.
//!
//! The PR body tells the story of the task: the goal and criteria, the chosen
//! attempt, and every sibling attempt that lost, with its reason, tests and
//! cost, so a reviewer on GitHub sees what was tried.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::export::{export, ExportOptions, ExportResult, HostRepo};
use crate::space::{Attempt, Space};

/// Overrides the API base URL (GitHub Enterprise, tests).
pub const API_ENV: &str = "AGENTVC_GITHUB_API";
const DEFAULT_API: &str = "https://api.github.com";

#[derive(Clone, Debug, Default)]
pub struct PrOptions {
    /// Branch to export to and push. Defaults to `agentvc/aN`.
    pub branch: Option<String>,
    /// Branch the PR merges into. Defaults to the repo's default branch.
    pub base: Option<String>,
    pub draft: bool,
    /// Git remote to push to. Defaults to `origin`.
    pub remote: Option<String>,
    /// `owner/name` on GitHub. Defaults to the one in the remote's URL.
    pub repo: Option<String>,
    pub title: Option<String>,
    /// Move the branch even if another attempt exported to it.
    pub force: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct PrResult {
    pub export: ExportResult,
    pub repo: String,
    pub base: String,
    pub number: u64,
    pub url: String,
    /// True when a PR for this branch was already open and was updated.
    pub updated: bool,
}

/// `owner/name` from a git remote URL in any of the usual forms:
/// `https://github.com/o/r.git`, `git@github.com:o/r`, `ssh://git@host/o/r`,
/// or a local path ending in `o/r.git`.
pub fn parse_remote(url: &str) -> Option<String> {
    let s = url.trim().trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let s = s.replace(':', "/");
    let mut parts = s.rsplit('/').filter(|p| !p.is_empty());
    let name = parts.next()?;
    let owner = parts.next()?;
    // GitHub owners never contain '.' or '@'; those are hosts and users.
    if owner.contains(['.', '@']) {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// A GitHub token from `GITHUB_TOKEN`, `GH_TOKEN` or `gh auth token`.
pub fn token() -> Result<String> {
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(t) = std::env::var(var) {
            if !t.trim().is_empty() {
                return Ok(t.trim().to_string());
            }
        }
    }
    let out = Command::new("gh").args(["auth", "token"]).output();
    match out {
        Ok(o) if o.status.success() => {
            let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if t.is_empty() {
                Err(Error::NoToken)
            } else {
                Ok(t)
            }
        }
        _ => Err(Error::NoToken),
    }
}

/// Merged metadata of an attempt's snapshots; later snapshots win.
fn attempt_meta(space: &Space, attempt: i64) -> Result<BTreeMap<String, String>> {
    let mut meta = BTreeMap::new();
    for s in space.snapshots(attempt)? {
        if let Some(Value::Object(m)) = s.meta {
            for (k, v) in m {
                let v = match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                meta.insert(k, v);
            }
        }
    }
    Ok(meta)
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn who(a: &Attempt) -> String {
    match &a.model {
        Some(m) => format!("{} ({m})", a.actor),
        None => a.actor.clone(),
    }
}

/// The PR body for `winner`: task, criteria, the chosen attempt and the
/// rejected siblings with their reasons, tests and cost.
pub fn pr_body(space: &Space, winner: &Attempt, export: &ExportResult) -> Result<String> {
    let task = winner.task_id.map(|t| space.task(t)).transpose()?;
    let mut body = String::new();
    body += "## Task\n\n";
    match &task {
        Some(t) => {
            body += &format!("**t{}**: {}\n\n", t.id, t.goal.trim());
            if let Some(c) = &t.criteria {
                body += &format!("**Acceptance criteria**\n\n{}\n\n", c.trim());
            }
        }
        None => body += "_No task recorded for this attempt._\n\n",
    }

    let meta = attempt_meta(space, winner.id)?;
    body += "## Chosen attempt\n\n";
    body += &format!(
        "**a{}** by {}, {}, exported from snapshot s{} as `{}`\n\n",
        winner.id,
        who(winner),
        winner.status,
        export.snapshot.id,
        &export.commit[..12.min(export.commit.len())]
    );
    body += &format!(
        "- Tests: {}\n- Cost: {}\n",
        meta.get("tests")
            .map(String::as_str)
            .unwrap_or("not recorded"),
        meta.get("cost")
            .map(String::as_str)
            .unwrap_or("not recorded"),
    );
    for (k, v) in meta.iter().filter(|(k, _)| *k != "tests" && *k != "cost") {
        body += &format!("- {k}: {v}\n");
    }
    if let Some(p) = &winner.prompt {
        body += &format!(
            "\n<details><summary>Prompt</summary>\n\n{}\n\n</details>\n",
            p.trim()
        );
    }
    body += "\n";

    let siblings: Vec<Attempt> = match &task {
        Some(t) => space
            .attempts(Some(t.id))?
            .into_iter()
            .filter(|a| a.id != winner.id)
            .collect(),
        None => Vec::new(),
    };
    body += "## Rejected attempts\n\n";
    if siblings.is_empty() {
        body += "_None: this was the only attempt._\n";
    } else {
        body += "| Attempt | Outcome | Reason | Agent | Tests | Cost |\n";
        body += "| --- | --- | --- | --- | --- | --- |\n";
        for s in &siblings {
            let m = attempt_meta(space, s.id)?;
            body += &format!(
                "| a{} | {} | {} | {} | {} | {} |\n",
                s.id,
                s.status,
                cell(s.reason.as_deref().unwrap_or("-")),
                cell(&who(s)),
                cell(m.get("tests").map(String::as_str).unwrap_or("-")),
                cell(m.get("cost").map(String::as_str).unwrap_or("-")),
            );
        }
    }
    body += &format!(
        "\n---\n_Exported by agentvc from attempt a{} (snapshot s{})._\n",
        winner.id, export.snapshot.id
    );
    Ok(body)
}

struct Api {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl Api {
    fn new(token: String) -> Api {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .into();
        Api {
            base: std::env::var(API_ENV)
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_API.to_string())
                .trim_end_matches('/')
                .to_string(),
            token,
            agent,
        }
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let net = |e: ureq::Error| Error::GitHub {
            status: 0,
            message: e.to_string(),
        };
        let auth = format!("Bearer {}", self.token);
        let mut resp = match method {
            "GET" => self
                .agent
                .get(&url)
                .header("Authorization", &auth)
                .header("Accept", "application/vnd.github+json")
                .header("User-Agent", "agentvc")
                .call()
                .map_err(net)?,
            _ => {
                let req = match method {
                    "POST" => self.agent.post(&url),
                    _ => self.agent.patch(&url),
                };
                req.header("Authorization", &auth)
                    .header("Accept", "application/vnd.github+json")
                    .header("User-Agent", "agentvc")
                    .header("Content-Type", "application/json")
                    .send(body.unwrap_or(Value::Null).to_string())
                    .map_err(net)?
            }
        };
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(net)?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            let mut message = v["message"].as_str().unwrap_or(&text).to_string();
            if let Some(errs) = v["errors"].as_array() {
                for e in errs {
                    if let Some(m) = e["message"].as_str() {
                        message += &format!("; {m}");
                    }
                }
            }
            return Err(Error::GitHub { status, message });
        }
        Ok(v)
    }
}

/// Export `attempt_id`, push its branch and open a PR, or update the PR that
/// is already open for that branch. Records the PR URL on the export.
pub fn open_pr(space: &Space, attempt_id: i64, opts: &PrOptions) -> Result<PrResult> {
    let attempt = space.attempt(attempt_id)?;
    let host = HostRepo::open(space.workdir())?;
    let remote = opts.remote.clone().unwrap_or_else(|| "origin".to_string());
    let remote_url = host.git(&["remote", "get-url", &remote], None)?;
    let repo = match &opts.repo {
        Some(r) => r.clone(),
        None => parse_remote(&remote_url).ok_or_else(|| Error::BadRemote(remote_url.clone()))?,
    };
    let api = Api::new(token()?);
    let base = match &opts.base {
        Some(b) => b.clone(),
        None => api.call("GET", &format!("/repos/{repo}"), None)?["default_branch"]
            .as_str()
            .unwrap_or("main")
            .to_string(),
    };

    let branch = opts
        .branch
        .clone()
        .unwrap_or_else(|| format!("agentvc/a{attempt_id}"));
    // Re-running `pr` for the same attempt refreshes its branch.
    let ours = space
        .last_export_to(&branch)?
        .is_some_and(|e| e.attempt_id == attempt_id);
    let exp = export(
        space,
        attempt_id,
        &ExportOptions {
            branch: branch.clone(),
            base: None,
            message: opts.title.clone(),
            force: opts.force || ours,
        },
    )?;

    let refspec = format!("+refs/heads/{branch}:refs/heads/{branch}");
    let push_spec = if exp.replaced.is_some() {
        refspec.as_str()
    } else {
        &refspec[1..]
    };
    host.git(&["push", "--quiet", &remote, push_spec], None)?;

    let title = opts.title.clone().unwrap_or_else(|| {
        attempt
            .task_id
            .and_then(|t| space.task(t).ok())
            .map(|t| t.goal)
            .unwrap_or_else(|| format!("agentvc attempt a{attempt_id}"))
    });
    let title = title.lines().next().unwrap_or("").trim().to_string();
    let body = pr_body(space, &attempt, &exp)?;

    let owner = repo.split('/').next().unwrap_or("");
    let open = api.call(
        "GET",
        &format!("/repos/{repo}/pulls?state=open&head={owner}:{branch}"),
        None,
    )?;
    let (pr, updated) = match open.as_array().and_then(|a| a.first()) {
        Some(existing) => {
            let n = existing["number"].as_u64().unwrap_or(0);
            let v = api.call(
                "PATCH",
                &format!("/repos/{repo}/pulls/{n}"),
                Some(json!({ "title": title, "body": body })),
            )?;
            (v, true)
        }
        None => {
            let v = api.call(
                "POST",
                &format!("/repos/{repo}/pulls"),
                Some(json!({
                    "title": title, "body": body, "head": branch,
                    "base": base, "draft": opts.draft,
                })),
            )?;
            (v, false)
        }
    };
    let url = pr["html_url"].as_str().unwrap_or_default().to_string();
    let number = pr["number"].as_u64().unwrap_or(0);
    space.set_export_pr(exp.export_id, &url)?;
    space.record(
        &attempt.actor,
        "attempt.pr",
        json!({ "attempt": attempt_id, "url": url, "number": number, "updated": updated }),
    )?;
    Ok(PrResult {
        export: exp,
        repo,
        base,
        number,
        url,
        updated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_urls() {
        for url in [
            "https://github.com/ykirpichev/agentvc.git",
            "https://github.com/ykirpichev/agentvc",
            "git@github.com:ykirpichev/agentvc.git",
            "ssh://git@github.com/ykirpichev/agentvc",
            "http://proxy@127.0.0.1:4242/git/ykirpichev/agentvc",
            "/tmp/remotes/ykirpichev/agentvc.git",
        ] {
            assert_eq!(
                parse_remote(url).as_deref(),
                Some("ykirpichev/agentvc"),
                "{url}"
            );
        }
        assert_eq!(parse_remote("agentvc"), None);
    }
}
