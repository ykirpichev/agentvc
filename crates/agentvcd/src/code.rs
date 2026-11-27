//! Browse the whole code of a branch, agent, workspace or version (#171).
//!
//! - `GET /api/tree?ref=R[&path=DIR][&changed=auto|BASE]`: one directory,
//!   directories first, each entry marked `added|modified|removed` against
//!   the base with `changed` (`auto` is the ref's own base: a branch's
//!   parent, an agent's start, a workspace's trunk commit).
//! - `GET /api/tree?ref=R&recursive=1&paths_only=1[&changed=..]`: every
//!   file path, for go-to-file, plus the changed files.
//! - `GET /api/search?ref=R&q=TEXT[&regex=1][&case=1][&path=GLOB,..][&limit=N]`.
//!
//! Refs, here and in `/api/file`, `/api/blame` and `/api/diff`'s `a`: `aN`,
//! `sN`, a tag, `git:<commit>`, `main` (the trunk of this workspace),
//! `branch:<workspace>/<name>` (its tip), `agent:<id>` (the agent's live
//! worktree, uncommitted edits included) and `workspace:<id>` (that
//! checkout's working tree). Working trees are captured into the store with
//! a stat index, so a capture after the first only hashes what changed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use agentvc_core::blame::Blame;
use agentvc_core::browse::{self, DirEntry, SearchQuery, SearchResult};
use agentvc_core::objects::Oid;
use agentvc_core::snapshot::{self, FileChange};
use agentvc_core::workspace::{self, WorkspaceInfo};
use agentvc_core::{Error, Space};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiResult};
use crate::AppState;

/// Most paths `recursive=1` returns.
pub const MAX_PATHS: usize = 200_000;
/// Default and largest `limit` of a search.
const SEARCH_LIMIT: usize = 500;
const SEARCH_MAX: usize = 5_000;
/// A resolved browse ref (a captured working tree, a branch tip) is reused
/// this long, so expanding folders one after another stays quick.
const CAPTURE_TTL: Duration = Duration::from_secs(3);

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/tree", get(tree))
        .route("/search", get(search))
}

/// A ref, resolved to a stored tree and what is known about it.
#[derive(Clone, Debug, Serialize)]
pub struct Resolved {
    /// The ref as asked.
    #[serde(rename = "ref")]
    pub r#ref: String,
    /// `snapshot`, `commit` or `worktree`.
    pub kind: &'static str,
    /// For people: `feat/x`, `main`, `ag3 (live)`, `a4 · s12`.
    pub label: String,
    pub tree: String,
    pub commit: Option<String>,
    pub snapshot: Option<i64>,
    pub attempt: Option<i64>,
    /// The folder on this machine holding these files (a checkout, a
    /// worktree or a view), for Open in editor. Its files may differ from
    /// the ref for a branch that is not checked out there.
    pub folder: Option<String>,
    /// The ref's own base, which `changed=auto` diffs against.
    pub base: Option<String>,
    /// A git checkout to blame in (with `commit`, or its working tree).
    #[serde(skip)]
    pub git_dir: Option<PathBuf>,
}

fn not_found(kind: &'static str, id: &str) -> ApiError {
    ApiError::from(Error::NotFound {
        kind,
        id: id.to_string(),
    })
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Whether `r` is one of the refs this module adds (not `aN`, `sN`, a tag
/// or `git:`).
pub(crate) fn is_browse_ref(r: &str) -> bool {
    r == "main"
        || r.starts_with("branch:")
        || r.starts_with("agent:")
        || r.starts_with("workspace:")
}

/// Capture a working tree into the store.
fn capture(s: &Space, key: &str, dir: &Path) -> agentvc_core::Result<String> {
    Ok(s.capture_worktree(key, dir)?.to_hex())
}

fn commit_tree(s: &Space, commit: &str) -> Result<String, ApiError> {
    s.commit_tree(commit)?
        .ok_or_else(|| not_found("commit", commit))
}

fn workspaces(s: &Space) -> Result<Vec<WorkspaceInfo>, ApiError> {
    Ok(workspace::list(s.workdir(), Some(s), false)?.workspaces)
}

fn local_folder(w: &WorkspaceInfo) -> Option<String> {
    (w.this_machine && w.path.is_dir()).then(|| w.path.to_string_lossy().into_owned())
}

fn resolved_cache() -> &'static Mutex<HashMap<String, (Instant, Resolved)>> {
    static C: std::sync::OnceLock<Mutex<HashMap<String, (Instant, Resolved)>>> =
        std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// Resolve any ref the browser accepts. Browse refs (whose lookup lists
/// every workspace) are reused for a few seconds, so expanding one folder
/// after another stays quick.
pub(crate) fn resolve(s: &Space, r: &str) -> Result<Resolved, ApiError> {
    let r = r.trim();
    if !is_browse_ref(r) {
        return resolve_now(s, r);
    }
    let key = format!("{}\0{r}", s.workdir().display());
    if let Some((at, res)) = resolved_cache().lock().unwrap().get(&key) {
        if at.elapsed() < CAPTURE_TTL {
            return Ok(res.clone());
        }
    }
    let res = resolve_now(s, r)?;
    let mut c = resolved_cache().lock().unwrap();
    c.retain(|_, (at, _)| at.elapsed() < CAPTURE_TTL);
    c.insert(key, (Instant::now(), res.clone()));
    Ok(res)
}

fn resolve_now(s: &Space, r: &str) -> Result<Resolved, ApiError> {
    let mut out = Resolved {
        r#ref: r.to_string(),
        kind: "commit",
        label: r.to_string(),
        tree: String::new(),
        commit: None,
        snapshot: None,
        attempt: None,
        folder: None,
        base: None,
        git_dir: Some(s.workdir().to_path_buf()),
    };
    if r == "main" {
        let list = workspace::list(s.workdir(), Some(s), false)?;
        let here = list
            .workspaces
            .iter()
            .find(|w| w.here)
            .or_else(|| list.workspaces.first());
        let commit = here
            .and_then(|w| w.base.clone())
            .or_else(|| {
                [
                    "origin/HEAD",
                    "origin/main",
                    "main",
                    "origin/master",
                    "master",
                ]
                .iter()
                .find_map(|c| git_out(s.workdir(), &["rev-parse", "--verify", "-q", c]))
            })
            .ok_or_else(|| not_found("trunk", "main"))?;
        out.label = here
            .map(|w| w.trunk.trim_start_matches("origin/").to_string())
            .unwrap_or_else(|| "main".into());
        out.tree = commit_tree(s, &commit)?;
        out.commit = Some(commit);
        out.folder = Some(s.workdir().to_string_lossy().into_owned());
        return Ok(out);
    }
    if let Some(rest) = r.strip_prefix("branch:") {
        let (ws, name) = rest.split_once('/').ok_or_else(|| {
            ApiError::from(Error::BadRef(format!(
                "{r}: expected branch:<workspace>/<name>"
            )))
        })?;
        let all = workspaces(s)?;
        let w = all
            .iter()
            .find(|w| w.id == ws || w.name == ws)
            .ok_or_else(|| not_found("workspace", ws))?;
        let (commit, base) = if name == w.trunk || name == w.trunk.trim_start_matches("origin/") {
            (w.base.clone(), None)
        } else {
            let b = w
                .branches
                .iter()
                .find(|b| b.name == name)
                .ok_or_else(|| not_found("branch", rest))?;
            (b.tip.clone(), Some(format!("git:{}", b.parent_rev)))
        };
        let commit = commit.ok_or_else(|| not_found("branch tip", rest))?;
        out.label = name.trim_start_matches("origin/").to_string();
        out.tree = commit_tree(s, &commit)?;
        out.commit = Some(commit);
        out.base = base;
        out.folder = local_folder(w);
        if let Some(f) = &out.folder {
            out.git_dir = Some(PathBuf::from(f));
        }
        return Ok(out);
    }
    if let Some(id) = r.strip_prefix("agent:") {
        let a = agentvc_core::agent::show(s.workdir(), id)?.agent;
        out.base = Some(format!("git:{}", a.start.commit));
        if a.worktree.is_dir() {
            out.kind = "worktree";
            out.label = format!("{} (live)", a.id);
            out.tree = capture(s, &format!("agent-{}", a.id), &a.worktree)?;
            out.folder = Some(a.worktree.to_string_lossy().into_owned());
            out.git_dir = Some(a.worktree.clone());
            return Ok(out);
        }
        // The worktree is gone (landed or discarded): its last commit.
        let commit = a
            .result
            .and_then(|x| x.commit)
            .ok_or_else(|| not_found("agent worktree", id))?;
        out.label = a.id.clone();
        out.tree = commit_tree(s, &commit)?;
        out.commit = Some(commit);
        return Ok(out);
    }
    if let Some(id) = r.strip_prefix("workspace:") {
        let all = workspaces(s)?;
        let w = all
            .iter()
            .find(|w| w.id == id || w.name == id)
            .ok_or_else(|| not_found("workspace", id))?;
        let folder = local_folder(w).ok_or_else(|| not_found("workspace on this machine", id))?;
        out.kind = "worktree";
        out.label = format!("{} (working tree)", w.name);
        out.tree = capture(s, &format!("ws-{}", w.id), &w.path)?;
        out.base = w.base.as_ref().map(|b| format!("git:{b}"));
        out.git_dir = Some(w.path.clone());
        out.folder = Some(folder);
        return Ok(out);
    }
    if let Some(commit) = r.strip_prefix("git:") {
        let tree = match s.git_base_by_commit(commit)? {
            Some(g) => g.tree,
            None => commit_tree(s, commit)?,
        };
        out.label = commit.chars().take(10).collect();
        out.tree = tree;
        out.commit = Some(commit.to_string());
        return Ok(out);
    }
    // aN, sN or a tag.
    let parsed = s.parse_ref(r)?;
    let snap = s.resolve(parsed)?;
    let attempt = s.attempt(snap.attempt_id)?;
    out.kind = "snapshot";
    out.label = format!("a{} · s{}", attempt.id, snap.id);
    out.tree = snap.tree.clone();
    out.snapshot = Some(snap.id);
    out.attempt = Some(attempt.id);
    out.git_dir = None;
    out.base = match s.own_git_base(attempt.id)? {
        Some(g) => Some(format!("git:{}", g.commit)),
        None => attempt.parent_snapshot.map(|p| format!("s{p}")),
    };
    if attempt.status.is_open() && attempt.view_path.is_dir() {
        out.folder = Some(attempt.view_path.to_string_lossy().into_owned());
    }
    Ok(out)
}

/// Files changed against a base, and the base's tree.
type Changes = (Vec<FileChange>, Oid);

/// The base to mark changes against: `auto` is the ref's own base.
fn changes_against(
    s: &Space,
    head: &Resolved,
    changed: Option<&str>,
) -> Result<(Option<String>, Option<Changes>), ApiError> {
    let base = match changed
        .map(str::trim)
        .filter(|c| !c.is_empty() && *c != "none")
    {
        None => return Ok((None, None)),
        Some("auto") => match &head.base {
            Some(b) => b.clone(),
            None => return Ok((None, None)),
        },
        Some(b) => b.to_string(),
    };
    let tree = Oid::from_hex(&resolve(s, &base)?.tree)?;
    let changes = snapshot::diff(s.objects(), &tree, &Oid::from_hex(&head.tree)?)?;
    Ok((Some(base), Some((changes, tree))))
}

// ---- tree -------------------------------------------------------------------

#[derive(Deserialize)]
struct TreeQuery {
    #[serde(rename = "ref")]
    r#ref: String,
    #[serde(default)]
    path: String,
    changed: Option<String>,
    recursive: Option<String>,
    paths_only: Option<String>,
}

fn flag(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|v| v != "0" && v != "false")
}

#[derive(Serialize)]
struct TreeResponse {
    #[serde(flatten)]
    at: Resolved,
    /// The base the marks are against, if any.
    changed_base: Option<String>,
    /// Files changed against it, in the whole tree.
    changes: Option<Vec<FileChange>>,
    path: String,
    /// The directory (without `recursive`).
    #[serde(skip_serializing_if = "Option::is_none")]
    entries: Option<Vec<DirEntry>>,
    /// Every file path (with `recursive=1`).
    #[serde(skip_serializing_if = "Option::is_none")]
    paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
}

async fn tree(State(st): State<AppState>, Query(q): Query<TreeQuery>) -> ApiResult<TreeResponse> {
    let out = st
        .with_space(move |s| {
            Ok((|| -> Result<TreeResponse, ApiError> {
                let at = resolve(s, &q.r#ref)?;
                let (changed_base, changes) = changes_against(s, &at, q.changed.as_deref())?;
                let tree = Oid::from_hex(&at.tree)?;
                let path = browse::clean_dir(&q.path);
                let mut resp = TreeResponse {
                    at,
                    changed_base,
                    changes: None,
                    path: path.clone(),
                    entries: None,
                    paths: None,
                    truncated: false,
                };
                if flag(&q.recursive) {
                    let _ = flag(&q.paths_only); // the only recursive form for now
                    let (paths, truncated) = browse::all_paths(s.objects(), &tree, MAX_PATHS)?;
                    resp.paths = Some(paths);
                    resp.truncated = truncated;
                } else {
                    let entries = browse::list_dir(
                        s.objects(),
                        &tree,
                        &path,
                        changes.as_ref().map(|(c, b)| (c.as_slice(), b)),
                    )?
                    .ok_or_else(|| not_found("directory", &format!("{}:{path}", q.r#ref)))?;
                    resp.entries = Some(entries);
                }
                resp.changes = changes.map(|(c, _)| c);
                Ok(resp)
            })())
        })
        .await??;
    Ok(Json(out))
}

// ---- search -----------------------------------------------------------------

#[derive(Deserialize)]
struct SearchParams {
    #[serde(rename = "ref")]
    r#ref: String,
    q: String,
    regex: Option<String>,
    case: Option<String>,
    /// Comma-separated globs or folders.
    path: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct SearchResponse {
    #[serde(flatten)]
    result: SearchResult,
    #[serde(rename = "ref")]
    r#ref: String,
    tree: String,
}

async fn search(
    State(st): State<AppState>,
    Query(p): Query<SearchParams>,
) -> ApiResult<SearchResponse> {
    if p.q.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_query",
            "search for what? (q is empty)".into(),
        ));
    }
    let out = st
        .with_space(move |s| {
            Ok((|| -> Result<SearchResponse, ApiError> {
                let at = resolve(s, &p.r#ref)?;
                let query = SearchQuery {
                    text: p.q.clone(),
                    regex: flag(&p.regex),
                    case_sensitive: flag(&p.case),
                    paths: p
                        .path
                        .as_deref()
                        .unwrap_or("")
                        .split(',')
                        .map(str::trim)
                        .filter(|g| !g.is_empty())
                        .map(String::from)
                        .collect(),
                    limit: p.limit.unwrap_or(SEARCH_LIMIT).min(SEARCH_MAX),
                };
                let result = browse::search(s.objects(), &Oid::from_hex(&at.tree)?, &query)?;
                Ok(SearchResponse {
                    result,
                    r#ref: at.r#ref,
                    tree: at.tree,
                })
            })())
        })
        .await??;
    Ok(Json(out))
}

// ---- blame ------------------------------------------------------------------

/// Blame `path` at a browse ref: an attempt's own blame for versions, git's
/// for commits and working trees.
pub(crate) fn blame(s: &Space, r: &str, path: &str) -> Result<Blame, ApiError> {
    let at = resolve(s, r)?;
    if let Some(a) = at.attempt {
        // aN: its view as it is now; sN: that attempt's head.
        let working = r.starts_with('a');
        return Ok(s.blame(a, path, working)?);
    }
    let dir = at
        .git_dir
        .clone()
        .unwrap_or_else(|| s.workdir().to_path_buf());
    let commit = if at.kind == "worktree" {
        None
    } else {
        at.commit.as_deref()
    };
    match agentvc_core::blame::blame_git(&dir, commit, path) {
        // A branch of another checkout: its commits are there, not here.
        Err(Error::NotFound { .. }) if commit.is_some() && dir != s.workdir() => {
            Ok(agentvc_core::blame::blame_git(s.workdir(), commit, path)?)
        }
        r => Ok(r?),
    }
}
