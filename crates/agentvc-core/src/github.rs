//! Opening a GitHub pull request from an attempt: export it to a branch, push
//! the branch, and create (or update) the PR through the REST API.
//!
//! The PR body tells the story of the task: the goal and criteria, the chosen
//! attempt, the winning path of attempts it was forked from, and every
//! attempt off that path that lost, with its reason, tests and cost, so a
//! reviewer on GitHub sees what was tried.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::config;
use crate::error::{Error, Result};
use crate::export::{export, ExportOptions, ExportResult, HostRepo};
use crate::space::{Attempt, Space, Status};

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
    /// Text put above the generated body (attribution lines, notes). Kept on
    /// later updates that pass none.
    pub body_prefix: Option<String>,
    /// Extra `Key: value` trailers for the exported commit, added to those
    /// in `agentvc config pr.trailer`.
    pub trailers: Vec<String>,
    /// Run the repository's git hooks first; see [`ExportOptions::verify`].
    pub verify: Option<bool>,
    /// One commit even when the attempt has a story.
    pub squash: bool,
    /// Push a branch per story change too (see [`ExportOptions::stack`]).
    pub stack: bool,
}

/// Start and end of the part of a PR body that `agentvc pr` writes. Text
/// outside the markers (a prefix, a human-written section) is kept when the
/// PR is updated.
pub const BODY_BEGIN: &str =
    "<!-- agentvc:begin (written by agentvc pr; text outside these markers is kept on update) -->";
pub const BODY_END: &str = "<!-- agentvc:end -->";

/// The PR body to send: `generated` between agentvc's markers, with the
/// text around the markers of `existing` kept. `prefix` replaces the text
/// above the markers; without one, an existing prefix stays. An existing
/// body without markers (from before markers, or written by hand) is
/// replaced.
pub fn compose_body(existing: Option<&str>, prefix: Option<&str>, generated: &str) -> String {
    let (mut before, after) = match existing.and_then(split_body) {
        Some((b, a)) => (b.to_string(), a.to_string()),
        None => (String::new(), String::new()),
    };
    if let Some(p) = prefix {
        before = if p.trim().is_empty() {
            String::new()
        } else {
            format!("{}\n\n", p.trim_end())
        };
    }
    format!(
        "{before}{BODY_BEGIN}\n{}\n{BODY_END}{after}",
        generated.trim_end()
    )
}

/// Text before and after agentvc's markers, if the body has both.
///
/// Markers count only as whole lines, so a body that mentions them in prose
/// (like this project's own PRs) is not cut there.
fn split_body(body: &str) -> Option<(&str, &str)> {
    let line_at = |from: usize, marker: &str| -> Option<usize> {
        let mut pos = from;
        loop {
            let i = pos + body[pos..].find(marker)?;
            let starts = i == 0 || body.as_bytes()[i - 1] == b'\n';
            let rest = &body[i + marker.len()..];
            let ends = rest.is_empty() || rest.starts_with('\n') || rest.starts_with("\r\n");
            if starts && ends {
                return Some(i);
            }
            pos = i + marker.len();
        }
    };
    let begin = line_at(0, BODY_BEGIN)?;
    let end = line_at(begin + BODY_BEGIN.len(), BODY_END)?;
    Some((&body[..begin], &body[end + BODY_END.len()..]))
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

/// The attempts `winner` was forked from on its own task, oldest first. Each
/// comes with the snapshot the next step on the path was forked from (`None`
/// only if that link is missing). Stops at the first ancestor on another task
/// (usually the root attempt).
fn winning_path(space: &Space, winner: &Attempt) -> Result<Vec<(Attempt, Option<i64>)>> {
    let mut path = Vec::new();
    let mut seen = std::collections::HashSet::from([winner.id]);
    let mut child = winner.clone();
    while let Some(p) = child.parent_attempt {
        if winner.task_id.is_none() || !seen.insert(p) {
            break;
        }
        let parent = space.attempt(p)?;
        if parent.task_id != winner.task_id {
            break;
        }
        path.push((parent.clone(), child.parent_snapshot));
        child = parent;
    }
    path.reverse();
    Ok(path)
}

/// A metadata value for a table cell: run commands carry whole agent prompts,
/// so long values are cut at 60 characters.
fn short(v: &str) -> String {
    match v.char_indices().nth(60) {
        Some((i, _)) => format!("{}…", &v[..i]),
        None => v.to_string(),
    }
}

/// A table of attempts with their outcome, reason, agent, tests and cost.
fn attempt_table(space: &Space, attempts: &[Attempt]) -> Result<String> {
    let mut t = String::new();
    t += "| Attempt | Outcome | Reason | Agent | Tests | Cost |\n";
    t += "| --- | --- | --- | --- | --- | --- |\n";
    for s in attempts {
        let m = attempt_meta(space, s.id)?;
        t += &format!(
            "| a{} | {} | {} | {} | {} | {} |\n",
            s.id,
            s.status,
            cell(s.reason.as_deref().unwrap_or("-")),
            cell(&who(s)),
            cell(m.get("tests").map(String::as_str).unwrap_or("-")),
            cell(m.get("cost").map(String::as_str).unwrap_or("-")),
        );
    }
    Ok(t)
}

/// The PR body for `winner`: task, criteria, the chosen attempt, the winning
/// path of attempts it was forked from, and the rejected (abandoned or
/// failed) attempts off that path with their reasons, tests and cost.
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
    let picked_by = promoted_by(space, winner.id)?
        .map(|p| format!(" by {p}"))
        .unwrap_or_default();
    body += &format!(
        "**a{}** by {}, {}{picked_by}, exported from snapshot s{} as `{}`\n\n",
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

    if !export.story.is_empty() {
        body += "## Story\n\n";
        body +=
            "Review commit by commit; change ids survive rebases (`Agentvc-Change` trailer).\n\n";
        body += "| # | Change | Commit | Title |\n| --- | --- | --- | --- |\n";
        for (i, c) in export.story.iter().enumerate() {
            body += &format!(
                "| {} | `{}` | `{}` | {} |\n",
                i + 1,
                c.change_id,
                &c.commit[..12.min(c.commit.len())],
                cell(&c.title)
            );
        }
        body += "\n";
    }

    // The attempts the winner was forked from, oldest first, each paired with
    // the snapshot it handed on to the next step.
    let path = winning_path(space, winner)?;
    if !path.is_empty() {
        body += "## Winning path\n\n";
        let chain: Vec<String> = path
            .iter()
            .map(|(a, _)| format!("a{}", a.id))
            .chain([format!("**a{}** (chosen)", winner.id)])
            .collect();
        body += &format!("{}\n\n", chain.join(" → "));
        body += "| Step | Attempt | Snapshot | Outcome | Agent | Tests | Cost | Other metadata |\n";
        body += "| --- | --- | --- | --- | --- | --- | --- | --- |\n";
        let steps = path
            .iter()
            .map(|(a, s)| (a, s.map(|s| format!("s{s}"))))
            .chain([(winner, Some(format!("s{} (exported)", export.snapshot.id)))]);
        for (i, (a, snap)) in steps.enumerate() {
            let m = attempt_meta(space, a.id)?;
            let other: Vec<String> = m
                .iter()
                .filter(|(k, _)| *k != "tests" && *k != "cost")
                .map(|(k, v)| format!("{k}: {}", short(v)))
                .collect();
            let other = if other.is_empty() {
                "-".to_string()
            } else {
                other.join(", ")
            };
            body += &format!(
                "| {} | a{} | {} | {} | {} | {} | {} | {} |\n",
                i + 1,
                a.id,
                snap.as_deref().unwrap_or("-"),
                a.status,
                cell(&who(a)),
                cell(m.get("tests").map(String::as_str).unwrap_or("-")),
                cell(m.get("cost").map(String::as_str).unwrap_or("-")),
                cell(&other),
            );
        }
        body += "\n";
    }

    // Work taken from other attempts (`agentvc take` / `merge`) credits them.
    let on_path_ids: Vec<i64> = path.iter().map(|(a, _)| a.id).chain([winner.id]).collect();
    let taken = crate::combine::contributions(space, &on_path_ids)?;
    if !taken.is_empty() {
        body += "## Contributors\n\n";
        body += "Work taken from other attempts into the winning path.\n\n";
        body += "| Attempt | Taken into | How | Files |\n| --- | --- | --- | --- |\n";
        for c in &taken {
            let source = space.attempt(c.source).ok();
            let mut files: Vec<String> = c.paths.iter().map(|p| format!("`{p}`")).collect();
            if let Some(h) = c.hunks.as_ref().and_then(|h| h.as_object()) {
                for f in files.iter_mut() {
                    let p = f.trim_matches('`');
                    if let Some(n) = h.get(p).and_then(|n| n.as_array()) {
                        let n: Vec<String> = n.iter().map(|v| v.to_string()).collect();
                        *f = format!("{f} (hunks {})", n.join(", "));
                    }
                }
            }
            body += &format!(
                "| a{}{} | a{} (s{}) | {} from {} | {} |\n",
                c.source,
                source
                    .map(|a| format!(" by {}", who(&a)))
                    .unwrap_or_default(),
                c.into,
                c.snapshot,
                c.mode,
                c.source_snapshot,
                cell(&files.join(", ")),
            );
        }
        body += "\n";
    }

    let on_path = |id: i64| id == winner.id || path.iter().any(|(a, _)| a.id == id);
    let (rejected, open): (Vec<Attempt>, Vec<Attempt>) = match &task {
        Some(t) => space
            .attempts(Some(t.id))?
            .into_iter()
            .filter(|a| !on_path(a.id))
            .partition(|a| matches!(a.status, Status::Abandoned | Status::Failed)),
        None => (Vec::new(), Vec::new()),
    };
    body += "## Rejected attempts\n\n";
    if rejected.is_empty() {
        body += "_None: no other attempt was abandoned or failed._\n";
    } else {
        body += &attempt_table(space, &rejected)?;
    }
    if !open.is_empty() {
        body += "\n## Other attempts\n\n";
        body += "Not on the winning path and not closed as rejected.\n\n";
        body += &attempt_table(space, &open)?;
    }
    body += &format!(
        "\n---\n_Exported by agentvc from attempt a{} (snapshot s{})._\n",
        winner.id, export.snapshot.id
    );
    Ok(body)
}

/// Root certificates from `SSL_CERT_FILE`, when set: corporate proxies and
/// sandboxes re-sign TLS with their own CA, which the built-in Mozilla roots
/// don't trust. As with curl and OpenSSL, the file replaces the defaults.
fn roots_from_env() -> Option<ureq::tls::RootCerts> {
    let path = std::env::var_os("SSL_CERT_FILE")?;
    let pem = std::fs::read(path).ok()?;
    let certs: Vec<_> = ureq::tls::parse_pem(&pem)
        .filter_map(|item| match item {
            Ok(ureq::tls::PemItem::Certificate(c)) => Some(c),
            _ => None,
        })
        .collect();
    (!certs.is_empty()).then(|| ureq::tls::RootCerts::new_with_certs(&certs))
}

pub(crate) struct Api {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl Api {
    pub(crate) fn new(token: String) -> Api {
        let mut config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)));
        if let Some(roots) = roots_from_env() {
            config = config.tls_config(ureq::tls::TlsConfig::builder().root_certs(roots).build());
        }
        let agent: ureq::Agent = config.build().into();
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

    #[cfg(test)]
    pub(crate) fn set_base(&mut self, base: &str) {
        self.base = base.trim_end_matches('/').to_string();
    }

    pub(crate) fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
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
        let header = |k: &str| {
            resp.headers()
                .get(k)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let remaining = header("x-ratelimit-remaining");
        let reset = header("x-ratelimit-reset").and_then(|v| v.parse::<i64>().ok());
        let retry_after = header("retry-after").and_then(|v| v.parse::<i64>().ok());
        if status == 429 || (status == 403 && remaining.as_deref() == Some("0")) {
            let reset_at = reset.or_else(|| retry_after.map(|s| crate::space::now() + s));
            return Err(Error::RateLimited { reset_at });
        }
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

/// `owner/name` on GitHub: `repo` when given, else parsed from `remote`'s URL.
pub fn repo_for(workdir: &std::path::Path, remote: &str, repo: Option<&str>) -> Result<String> {
    if let Some(r) = repo {
        return Ok(r.to_string());
    }
    let url = HostRepo::open(workdir)?.git(&["remote", "get-url", remote], None)?;
    parse_remote(&url).ok_or(Error::BadRemote(url))
}

/// `(owner/name, number)` from a PR URL like `https://github.com/o/r/pull/7`.
pub fn parse_pr_url(url: &str) -> Option<(String, u64)> {
    let rest = url.split("/pull/").collect::<Vec<_>>();
    let [head, tail] = rest.as_slice() else {
        return None;
    };
    let n = tail.split(['/', '#', '?']).next()?.parse().ok()?;
    let mut parts = head.rsplit('/');
    let name = parts.next()?;
    let owner = parts.next()?;
    Some((format!("{owner}/{name}"), n))
}

/// The space comment on a PR after [`save_space_comment`].
#[derive(Clone, Debug, Serialize)]
pub struct SpaceComment {
    pub repo: String,
    pub pr: u64,
    pub url: String,
    /// True when an earlier space comment was edited in place.
    pub updated: bool,
    pub bundle: crate::bundle::Bundle,
}

fn find_space_comment(api: &Api, repo: &str, pr: u64) -> Result<Option<Value>> {
    let mut page = 1;
    loop {
        let list = api.call(
            "GET",
            &format!("/repos/{repo}/issues/{pr}/comments?per_page=100&page={page}"),
            None,
        )?;
        let Some(items) = list.as_array().filter(|a| !a.is_empty()) else {
            return Ok(None);
        };
        if let Some(c) = items.iter().find(|c| {
            c["body"]
                .as_str()
                .is_some_and(|b| b.starts_with(crate::bundle::MARKER))
        }) {
            return Ok(Some(c.clone()));
        }
        if items.len() < 100 {
            return Ok(None);
        }
        page += 1;
    }
}

/// Write the whole space (see [`crate::bundle`]) into one comment on PR
/// `pr`, editing the previous one in place.
pub fn save_space_comment(space: &Space, repo: &str, pr: u64) -> Result<SpaceComment> {
    let bundle = crate::bundle::encode(space, pr)?;
    let api = Api::new(token()?);
    let (v, updated) = match find_space_comment(&api, repo, pr)? {
        Some(c) => {
            let id = c["id"].as_u64().unwrap_or(0);
            let v = api.call(
                "PATCH",
                &format!("/repos/{repo}/issues/comments/{id}"),
                Some(json!({ "body": bundle.body })),
            )?;
            (v, true)
        }
        None => (
            api.call(
                "POST",
                &format!("/repos/{repo}/issues/{pr}/comments"),
                Some(json!({ "body": bundle.body })),
            )?,
            false,
        ),
    };
    Ok(SpaceComment {
        repo: repo.to_string(),
        pr,
        url: v["html_url"].as_str().unwrap_or_default().to_string(),
        updated,
        bundle,
    })
}

/// Read the space comment of PR `pr`, or of the PR whose head is the branch
/// checked out in `workdir` (newest first, open or closed).
pub fn load_space_comment(
    workdir: &std::path::Path,
    repo: &str,
    pr: Option<u64>,
) -> Result<(u64, Value)> {
    let api = Api::new(token()?);
    let pr = match pr {
        Some(n) => n,
        None => {
            let branch = HostRepo::open(workdir)?
                .git(&["symbolic-ref", "--short", "-q", "HEAD"], None)
                .map_err(|_| Error::NotFound {
                    kind: "branch",
                    id: "HEAD (detached; pass a PR number)".into(),
                })?;
            let owner = repo.split('/').next().unwrap_or("");
            let list = api.call(
                "GET",
                &format!("/repos/{repo}/pulls?state=all&head={owner}:{branch}"),
                None,
            )?;
            list.as_array()
                .and_then(|a| a.first())
                .and_then(|p| p["number"].as_u64())
                .ok_or(Error::NotFound {
                    kind: "pull request for branch",
                    id: branch,
                })?
        }
    };
    let c = find_space_comment(&api, repo, pr)?.ok_or(Error::NotFound {
        kind: "agentvc space comment on PR",
        id: format!("#{pr}"),
    })?;
    Ok((
        pr,
        crate::bundle::decode(c["body"].as_str().unwrap_or_default())?,
    ))
}

/// Spaces found by [`space_comments`], and the PRs whose comment didn't decode.
pub type SpaceComments = (Vec<PrSpace>, Vec<(u64, String)>);

/// One PR's space comment, found by [`space_comments`].
#[derive(Clone, Debug)]
pub struct PrSpace {
    pub pr: u64,
    pub title: String,
    /// When the comment was last written; newer copies of a space win.
    pub updated_at: String,
    /// The decoded payload (see [`crate::bundle::decode`]).
    pub payload: Value,
}

/// The space comments of `prs`, or of every PR in `repo` (open and closed)
/// when `prs` is empty. PRs without one are skipped; a comment that does
/// not decode is reported in `errors` as (PR, message).
pub fn space_comments(repo: &str, prs: &[u64]) -> Result<SpaceComments> {
    let api = Api::new(token()?);
    let mut list: Vec<(u64, String)> = Vec::new();
    if prs.is_empty() {
        let mut page = 1;
        loop {
            let v = api.call(
                "GET",
                &format!("/repos/{repo}/pulls?state=all&per_page=100&page={page}"),
                None,
            )?;
            let items = v.as_array().cloned().unwrap_or_default();
            for p in &items {
                if let Some(n) = p["number"].as_u64() {
                    list.push((n, p["title"].as_str().unwrap_or_default().to_string()));
                }
            }
            if items.len() < 100 {
                break;
            }
            page += 1;
        }
    } else {
        for &n in prs {
            let p = api.call("GET", &format!("/repos/{repo}/pulls/{n}"), None)?;
            list.push((n, p["title"].as_str().unwrap_or_default().to_string()));
        }
    }
    let (mut found, mut errors) = (Vec::new(), Vec::new());
    for (pr, title) in list {
        let Some(c) = find_space_comment(&api, repo, pr)? else {
            continue;
        };
        match crate::bundle::decode(c["body"].as_str().unwrap_or_default()) {
            Ok(payload) => found.push(PrSpace {
                pr,
                title,
                updated_at: c["updated_at"].as_str().unwrap_or_default().to_string(),
                payload,
            }),
            Err(e) => errors.push((pr, e.to_string())),
        }
    }
    Ok((found, errors))
}

/// Where [`save_space`] put the space.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SaveResult {
    /// The push to `refs/agentvc/spaces/<id>`.
    pub git: Option<crate::sync::SyncResult>,
    pub git_error: Option<Value>,
    /// The comment on the space's latest PR.
    pub pr: Option<SpaceComment>,
    pub pr_error: Option<Value>,
}

impl SaveResult {
    /// Whether the space reached the remote one way or the other.
    pub fn saved(&self) -> bool {
        self.git.is_some() || self.pr.is_some()
    }
}

fn err_json(e: &Error) -> Value {
    let mut v = json!({ "code": e.code(), "message": e.to_string() });
    if let (Some(Value::Object(d)), Some(o)) = (e.details(), v.as_object_mut()) {
        o.extend(d);
    }
    v
}

/// Space metadata key set when the remote refused a space push, so automatic
/// saves stop trying (cloud sessions may push only their own branch).
pub const PUSH_REFUSED_KEY: &str = "sync_push_refused";

/// Save the space everywhere it can go: push it to the remote (unless
/// `skip_git`) and write it into the comment of `pr`, or of the space's latest
/// PR. Either one is enough; failures are reported, not returned.
pub fn save_space(
    space: &Space,
    opts: &crate::sync::SyncOptions,
    pr: Option<(String, u64)>,
    skip_git: bool,
) -> SaveResult {
    let mut r = SaveResult::default();
    if !skip_git {
        match crate::sync::sync(space, opts) {
            Ok(s) => {
                let _ = space.delete_meta(PUSH_REFUSED_KEY);
                r.git = Some(s);
            }
            Err(e) => {
                let remote = opts.remote.as_deref().unwrap_or("origin");
                let e = push_error(e, remote, "refs/agentvc/spaces/*");
                if matches!(e, Error::PushNotAllowed { .. }) {
                    let _ = space.set_meta(PUSH_REFUSED_KEY, "1");
                }
                r.git_error = Some(err_json(&e));
            }
        }
    }
    let pr = pr.or_else(|| {
        space
            .latest_pr_url()
            .ok()
            .flatten()
            .and_then(|u| parse_pr_url(&u))
    });
    if let Some((repo, n)) = pr {
        match save_space_comment(space, &repo, n) {
            Ok(c) => r.pr = Some(c),
            Err(e) => r.pr_error = Some(err_json(&e)),
        }
    }
    r
}

/// [`save_space`] after commands that change history, when `agentvc sync
/// --auto` is on. Skips the push once the remote has refused one.
pub fn auto_save(space: &Space) -> Option<SaveResult> {
    let remote = space.meta(crate::sync::AUTO_SYNC_KEY).ok().flatten()?;
    let refused = space.meta(PUSH_REFUSED_KEY).ok().flatten().is_some();
    Some(save_space(
        space,
        &crate::sync::SyncOptions {
            remote: Some(remote),
            force: false,
            prefer: None,
        },
        None,
        refused,
    ))
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
    let base = match opts.base.clone().or(space.config(config::PR_BASE)?) {
        Some(b) => b,
        None => api.call("GET", &format!("/repos/{repo}"), None)?["default_branch"]
            .as_str()
            .unwrap_or("main")
            .to_string(),
    };

    let branch = match opts.branch.clone().or(space.config(config::PR_BRANCH)?) {
        Some(b) => b,
        None => format!("agentvc/a{attempt_id}"),
    };
    // Re-running `pr` for the same attempt, or for one continued from it
    // (`fork --from aN`, review fixes), refreshes its branch.
    let ours = match space.last_export_to(&branch)? {
        Some(e) => {
            e.attempt_id == attempt_id
                || (e.attempt_id != crate::space::ROOT_ATTEMPT
                    && space.descends_from(attempt_id, e.attempt_id)?)
        }
        None => false,
    };
    let mut trailers = space.config_values(config::PR_TRAILER)?;
    for t in &opts.trailers {
        if !trailers.contains(t) {
            trailers.push(t.clone());
        }
    }
    let exp = export(
        space,
        attempt_id,
        &ExportOptions {
            branch: branch.clone(),
            base: None,
            message: opts.title.clone(),
            force: opts.force || ours,
            trailers,
            verify: opts.verify,
            squash: opts.squash,
            stack: opts.stack,
        },
    )?;

    let refspec = format!("+refs/heads/{branch}:refs/heads/{branch}");
    let push_spec = if exp.replaced.is_some() {
        refspec.as_str()
    } else {
        &refspec[1..]
    };
    host.git(&["push", "--quiet", &remote, push_spec], None)
        .map_err(|e| push_error(e, &remote, &branch))?;
    for b in exp.story.iter().filter_map(|c| c.branch.as_ref()) {
        host.git(
            &[
                "push",
                "--quiet",
                &remote,
                &format!("+refs/heads/{b}:refs/heads/{b}"),
            ],
            None,
        )
        .map_err(|e| push_error(e, &remote, b))?;
    }

    let title = opts.title.clone().unwrap_or_else(|| {
        attempt
            .task_id
            .and_then(|t| space.task(t).ok())
            .map(|t| t.goal)
            .unwrap_or_else(|| format!("agentvc attempt a{attempt_id}"))
    });
    let title = title.lines().next().unwrap_or("").trim().to_string();
    let generated = pr_body(space, &attempt, &exp)?;
    let prefix = match &opts.body_prefix {
        Some(p) => Some(p.clone()),
        None => match space.config(config::PR_BODY_PREFIX)? {
            Some(path) => {
                let p = space.workdir().join(path);
                Some(std::fs::read_to_string(&p).map_err(|source| Error::Io { path: p, source })?)
            }
            None => None,
        },
    };

    let owner = repo.split('/').next().unwrap_or("");
    let open = api.call(
        "GET",
        &format!("/repos/{repo}/pulls?state=open&head={owner}:{branch}"),
        None,
    )?;
    let (pr, updated) = match open.as_array().and_then(|a| a.first()) {
        Some(existing) => {
            let n = existing["number"].as_u64().unwrap_or(0);
            let body = compose_body(existing["body"].as_str(), prefix.as_deref(), &generated);
            let v = api.call(
                "PATCH",
                &format!("/repos/{repo}/pulls/{n}"),
                Some(json!({ "title": title, "body": body })),
            )?;
            (v, true)
        }
        None => {
            let body = compose_body(None, prefix.as_deref(), &generated);
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

/// A refused push says so: git proxies answer 403 for branches the
/// credentials may not write, which otherwise reads as a generic git error.
pub(crate) fn push_error(e: Error, remote: &str, branch: &str) -> Error {
    match e {
        Error::Git { stderr, .. } if push_refused(&stderr) => Error::PushNotAllowed {
            remote: remote.to_string(),
            branch: branch.to_string(),
            stderr,
        },
        other => other,
    }
}

fn push_refused(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("403")
        || s.contains("permission to")
        || s.contains("permission denied")
        || s.contains("not allowed")
        || s.contains("protected branch")
}

/// Who promoted attempt `id` (the latest promote that still stands).
fn promoted_by(space: &Space, id: i64) -> Result<Option<String>> {
    Ok(space
        .operations(None)?
        .into_iter()
        .find(|o| {
            o.kind == "attempt.promoted" && o.undone_by.is_none() && o.payload["attempt"] == id
        })
        .map(|o| o.actor))
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
        assert_eq!(
            parse_pr_url("https://github.com/acme/widget/pull/7"),
            Some(("acme/widget".into(), 7))
        );
        assert_eq!(parse_pr_url("https://github.com/acme/widget"), None);
    }

    #[test]
    fn body_keeps_prefix_and_human_sections_across_updates() {
        let first = compose_body(None, Some("Requested by Yury\n"), "## Task\nold");
        assert!(
            first.starts_with("Requested by Yury\n\n<!-- agentvc:begin"),
            "{first}"
        );
        assert!(
            first.ends_with("## Task\nold\n<!-- agentvc:end -->"),
            "{first}"
        );

        // A reviewer adds notes below; an update without a prefix keeps both.
        let edited = format!("{first}\n\n## Notes from review\nkeep me\n");
        let second = compose_body(Some(&edited), None, "## Task\nnew");
        assert!(second.starts_with("Requested by Yury\n\n"), "{second}");
        assert!(
            second.contains("## Task\nnew\n<!-- agentvc:end -->"),
            "{second}"
        );
        assert!(!second.contains("old"), "{second}");
        assert!(
            second.ends_with("\n\n## Notes from review\nkeep me\n"),
            "{second}"
        );

        // A new prefix replaces the old one, still keeping the section.
        let third = compose_body(Some(&second), Some("Requested by Ann"), "## Task\nnew");
        assert!(
            third.starts_with("Requested by Ann\n\n<!-- agentvc:begin"),
            "{third}"
        );
        assert!(third.contains("keep me"));

        // Markers mentioned in prose, inside the prefix, are not markers.
        let prose = "Our part sits between `<!-- agentvc:begin -->` and `<!-- agentvc:end -->`.";
        let with_prose = compose_body(None, Some(prose), "## Task\nv1");
        let updated = compose_body(Some(&with_prose), None, "## Task\nv2");
        assert_eq!(updated, compose_body(None, Some(prose), "## Task\nv2"));

        // A body from before markers is replaced.
        let legacy = compose_body(Some("old generated body"), None, "gen");
        assert_eq!(legacy, format!("{BODY_BEGIN}\ngen\n{BODY_END}"));
    }

    #[test]
    fn refused_pushes_are_recognised() {
        let e = push_error(
            Error::Git {
                cmd: "push".into(),
                stderr: "remote: Permission to o/r.git denied.\nfatal: unable to access: The requested URL returned error: 403".into(),
            },
            "origin",
            "main",
        );
        assert_eq!(e.code(), "push_not_allowed");
        let e = push_error(
            Error::Git {
                cmd: "push".into(),
                stderr: "fatal: could not read from remote".into(),
            },
            "origin",
            "main",
        );
        assert_eq!(e.code(), "git");
    }

    #[test]
    fn pr_body_shows_winning_path_and_rejects_only_off_path() {
        use crate::space::{ForkRequest, Ref};

        let tmp = crate::testutil::tempdir();
        std::fs::write(tmp.path().join("main.rs"), "fn main() {}\n").unwrap();
        let space = Space::init(tmp.path()).unwrap();
        let task = space.create_task("speed up main", None).unwrap();
        let fork = |from: Option<i64>| {
            space
                .fork(&ForkRequest {
                    task_id: Some(task.id),
                    from: from.map(Ref::Attempt),
                    actor: "agent".into(),
                    ..Default::default()
                })
                .unwrap()
                .attempt
                .id
        };
        let edit = |id: i64, text: &str, meta: Value| {
            let view = space.attempt(id).unwrap().view_path;
            std::fs::write(view.join("main.rs"), text).unwrap();
            space.snapshot(id, "tool", Some(meta)).unwrap();
        };

        // Chain: first -> second -> chosen, plus siblings off the path.
        let first = fork(None);
        edit(
            first,
            "fn main() { a() }\n",
            json!({"tests": "3/5", "cost": "$0.10"}),
        );
        let second = fork(Some(first));
        edit(
            second,
            "fn main() { b() }\n",
            json!({"tests": "4/5", "note": "x"}),
        );
        let chosen = fork(Some(second));
        edit(chosen, "fn main() { c() }\n", json!({"tests": "5/5"}));
        let failed = fork(None);
        space
            .set_status(failed, Status::Failed, Some("tests red"))
            .unwrap();
        let side = fork(Some(first)); // branches off the path
        let lost = fork(None);
        space.set_status(chosen, Status::Promoted, None).unwrap();

        let winner = space.attempt(chosen).unwrap();
        let snap = space.head_snapshot(chosen).unwrap();
        let exp = ExportResult {
            export_id: 1,
            attempt: chosen,
            snapshot: snap.clone(),
            branch: "agentvc/test".into(),
            commit: "0123456789abcdef".into(),
            parent: None,
            tree: snap.tree.clone(),
            replaced: None,
            checkout_updated: false,
            objects_copied: 0,
            hooks: vec![],
            story: Vec::new(),
        };
        let body = pr_body(&space, &winner, &exp).unwrap();

        let (path, rest) = body
            .split_once("## Winning path")
            .unwrap()
            .1
            .split_once("## Rejected attempts")
            .unwrap();
        assert!(
            path.contains(&format!("a{first} → a{second} → **a{chosen}** (chosen)")),
            "{body}"
        );
        // Each step names the snapshot carried forward and its metadata.
        let from_first = space.attempt(second).unwrap().parent_snapshot.unwrap();
        let from_second = winner.parent_snapshot.unwrap();
        assert!(path.contains(&format!(
            "| 1 | a{first} | s{from_first} | promoted | agent | 3/5 | $0.10 | - |"
        )));
        assert!(path.contains(&format!(
            "| 2 | a{second} | s{from_second} | promoted | agent | 4/5 | - | note: x |"
        )));
        assert!(path.contains(&format!(
            "| 3 | a{chosen} | s{} (exported) | promoted | agent | 5/5 | - | - |",
            snap.id
        )));

        // Rejected lists the off-path losers only, never the promoted ancestors.
        for id in [failed, side, lost] {
            assert!(rest.contains(&format!("| a{id} | abandoned |")), "{body}");
        }
        for id in [first, second, chosen] {
            assert!(!rest.contains(&format!("| a{id} |")), "{body}");
        }
        assert!(!body.contains("## Other attempts"));
    }

    #[test]
    fn short_cuts_long_metadata_values() {
        assert_eq!(short("pass"), "pass");
        let cut = short(&"é".repeat(100));
        assert_eq!(cut.chars().count(), 61);
        assert!(cut.ends_with('…'));
    }
}
