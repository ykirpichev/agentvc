//! JSON API over the space. Reads live here; review actions in `review.rs`.

use std::collections::BTreeMap;

use agentvc_core::objects::{Mode, Oid};
use agentvc_core::snapshot::FileChange;
use agentvc_core::{Attempt, Error, GitBase, Ref, Snapshot, Space, Task};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{events, graph, history, rebase, review, take, AppState};

/// Files larger than this are listed but their content is not sent.
pub const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/space", get(space_info))
        .route("/overview", get(overview))
        .route("/overlaps", get(overlaps))
        .route("/tasks", get(list_tasks))
        .route("/tasks/{id}", get(get_task))
        .route("/attempts", get(list_attempts))
        .route("/attempts/{id}", get(get_attempt))
        .route("/attempts/{id}/snapshots", get(list_snapshots))
        .route("/snapshots/{id}", get(get_snapshot))
        .route("/diff", get(diff))
        .route("/query", get(history::query))
        .route("/blame", get(history::blame))
        .route("/file", get(file))
        .route("/events", get(events::websocket))
        .route("/attempts/{id}/rebase", post(rebase::rebase))
        .route("/attempts/{id}/conflicts", get(crate::conflicts::list))
        .route(
            "/attempts/{id}/conflicts/resolve",
            post(crate::conflicts::resolve),
        )
        .route("/attempts/{id}/take", post(take::take))
        .merge(review::routes())
        .merge(crate::workspaces::routes())
        .merge(graph::routes())
        .fallback(|| async {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "no_route",
                "no such API endpoint".into(),
            )
        })
}

// ---- errors ---------------------------------------------------------------

/// An error response: `{"error": {"code", "message"}}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    /// Extra fields merged into the error object, e.g. conflict `paths`.
    details: Option<serde_json::Value>,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: String) -> ApiError {
        ApiError {
            status,
            code,
            message,
            details: None,
        }
    }

    pub(crate) fn internal(message: String) -> ApiError {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    fn bad_request(code: &'static str, message: String) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, code, message)
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> ApiError {
        let status = match &e {
            Error::NotFound { .. } => StatusCode::NOT_FOUND,
            Error::BadRef(_) => StatusCode::BAD_REQUEST,
            Error::AttemptClosed { .. } | Error::Conflict { .. } | Error::TakeConflict { .. } => {
                StatusCode::CONFLICT
            }
            Error::RootAttempt
            | Error::NothingToTake { .. }
            | Error::BadHunk { .. }
            | Error::BadQuery(_) => StatusCode::BAD_REQUEST,
            Error::Stack { code, .. } => match *code {
                "no_workspace" | "no_request" | "not_in_stack" | "no_branch" => {
                    StatusCode::NOT_FOUND
                }
                "conflict"
                | "stack_locked"
                | "request_not_queued"
                | "remote_workspace"
                | "checked_out_elsewhere"
                | "owned_by_other_window"
                | "rebase_in_progress"
                | "unresolved_conflicts"
                | "no_conflicts"
                | "not_conflicted" => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            },
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let mut err = ApiError::new(status, e.code(), e.to_string());
        err.details = e.details();
        err
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut err = json!({ "code": self.code, "message": self.message });
        if let (Some(m), Some(serde_json::Value::Object(d))) = (err.as_object_mut(), self.details) {
            m.extend(d);
        }
        let body = json!({ "error": err });
        (self.status, Json(body)).into_response()
    }
}

pub(crate) type ApiResult<T> = Result<Json<T>, ApiError>;

/// Accepts `t3` or `3`.
fn parse_task_id(s: &str) -> Result<i64, ApiError> {
    s.strip_prefix('t')
        .unwrap_or(s)
        .parse()
        .map_err(|_| ApiError::bad_request("bad_task", format!("invalid task id {s:?}")))
}

/// Accepts `a3` or `3`.
pub(crate) fn parse_attempt_id(s: &str) -> Result<i64, ApiError> {
    s.strip_prefix('a')
        .unwrap_or(s)
        .parse()
        .map_err(|_| ApiError::bad_request("bad_ref", format!("invalid attempt id {s:?}")))
}

fn parse_snapshot_id(s: &str) -> Result<i64, ApiError> {
    s.strip_prefix('s')
        .unwrap_or(s)
        .parse()
        .map_err(|_| ApiError::bad_request("bad_ref", format!("invalid snapshot id {s:?}")))
}

// ---- space ----------------------------------------------------------------

#[derive(Serialize)]
struct SpaceInfo {
    workdir: String,
    name: String,
    format_version: u32,
    daemon_version: &'static str,
}

async fn space_info(State(st): State<AppState>) -> ApiResult<SpaceInfo> {
    let w = st.workdir();
    Ok(Json(SpaceInfo {
        workdir: w.to_string_lossy().into_owned(),
        name: w
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        format_version: agentvc_core::FORMAT_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION"),
    }))
}

/// The whole-space summary that `agentvc overview` writes, for side panels
/// and other dashboards that want one request instead of many.
async fn overview(State(st): State<AppState>) -> ApiResult<agentvc_core::Overview> {
    Ok(Json(st.with_space(|s| s.overview()).await?))
}

#[derive(Serialize)]
struct OverlapsResponse {
    overlaps: Vec<agentvc_core::overlaps::Overlap>,
}

/// Pairs of open attempts of different tasks that change the same files,
/// for overlap badges on the live board and attempt pages (#94).
async fn overlaps(State(st): State<AppState>) -> ApiResult<OverlapsResponse> {
    let overlaps = st.with_space(|s| s.overlaps(None)).await?;
    Ok(Json(OverlapsResponse { overlaps }))
}

// ---- tasks ----------------------------------------------------------------

/// Number of attempts per status.
#[derive(Default, Serialize)]
pub struct StatusCounts {
    pub(crate) total: usize,
    pub(crate) exploring: usize,
    pub(crate) passed: usize,
    pub(crate) failed: usize,
    pub(crate) abandoned: usize,
    pub(crate) promoted: usize,
}

impl StatusCounts {
    pub(crate) fn add(&mut self, a: &Attempt) {
        use agentvc_core::Status::*;
        self.total += 1;
        *match a.status {
            Exploring => &mut self.exploring,
            Passed => &mut self.passed,
            Failed => &mut self.failed,
            Abandoned => &mut self.abandoned,
            Promoted => &mut self.promoted,
        } += 1;
    }
}

#[derive(Serialize)]
struct TaskSummary {
    #[serde(flatten)]
    task: Task,
    attempts: StatusCounts,
    /// Creation time of the newest attempt, or of the task.
    updated_at: i64,
}

async fn list_tasks(State(st): State<AppState>) -> ApiResult<Vec<TaskSummary>> {
    let out = st
        .with_space(|s| {
            let mut by_task: BTreeMap<i64, (StatusCounts, i64)> = BTreeMap::new();
            for a in s.attempts(None)? {
                if let Some(t) = a.task_id {
                    let e = by_task.entry(t).or_default();
                    e.0.add(&a);
                    e.1 = e.1.max(a.created_at);
                }
            }
            Ok(s.tasks()?
                .into_iter()
                .map(|task| {
                    let (attempts, last) = by_task.remove(&task.id).unwrap_or_default();
                    TaskSummary {
                        updated_at: last.max(task.created_at),
                        task,
                        attempts,
                    }
                })
                .collect())
        })
        .await?;
    Ok(Json(out))
}

#[derive(Serialize)]
struct TaskDetail {
    task: Task,
    attempts: Vec<Attempt>,
}

async fn get_task(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult<TaskDetail> {
    let id = parse_task_id(&id)?;
    let out = st
        .with_space(move |s| {
            Ok(TaskDetail {
                task: s.task(id)?,
                attempts: s.attempts(Some(id))?,
            })
        })
        .await?;
    Ok(Json(out))
}

// ---- attempts -------------------------------------------------------------

#[derive(Deserialize)]
struct AttemptsQuery {
    task: Option<String>,
}

async fn list_attempts(
    State(st): State<AppState>,
    Query(q): Query<AttemptsQuery>,
) -> ApiResult<Vec<Attempt>> {
    let task = q.task.as_deref().map(parse_task_id).transpose()?;
    let out = st
        .with_space(move |s| {
            if let Some(t) = task {
                s.task(t)?;
            }
            s.attempts(task)
        })
        .await?;
    Ok(Json(out))
}

#[derive(Serialize)]
struct AttemptDetail {
    attempt: Attempt,
    /// Latest snapshot (the parent snapshot for a fresh fork), if any.
    head: Option<Snapshot>,
    snapshot_count: usize,
    children: Vec<i64>,
    /// The attempt's own latest rebase: its diffs compare against this.
    git_base: Option<GitBase>,
    /// The git commit the head is based on (what `pr` parents on).
    base_commit: Option<String>,
    /// Its pull request, if `agentvc pr` opened one.
    pr_url: Option<String>,
    /// The local branch `agentvc land` put it on.
    landed: Option<agentvc_core::land::Landing>,
    /// GitHub's checks, reviews and comment threads on its PR, as of the
    /// last refresh.
    pr: Option<agentvc_core::feedback::PrFeedback>,
}

async fn get_attempt(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<AttemptDetail> {
    let id = parse_attempt_id(&id)?;
    let out = st
        .with_space(move |s| {
            let attempt = s.attempt(id)?;
            let snapshot_count = s.snapshots(id)?.len();
            let head = match s.head_snapshot(id) {
                Ok(h) => Some(h),
                Err(Error::NotFound { .. }) => None,
                Err(e) => return Err(e),
            };
            let children = s
                .attempts(None)?
                .into_iter()
                .filter(|a| a.parent_attempt == Some(id))
                .map(|a| a.id)
                .collect();
            let git_base = s.own_git_base(id)?;
            let base_commit = match &head {
                Some(h) => s.git_base_commit(h)?,
                None => None,
            };
            Ok(AttemptDetail {
                attempt,
                head,
                snapshot_count,
                children,
                git_base,
                base_commit,
                pr_url: s.pr_url(id)?,
                landed: s.landing(id)?,
                pr: s.pr_feedback(id)?,
            })
        })
        .await?;
    Ok(Json(out))
}

async fn list_snapshots(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Vec<Snapshot>> {
    let id = parse_attempt_id(&id)?;
    let out = st
        .with_space(move |s| {
            s.attempt(id)?;
            s.snapshots(id)
        })
        .await?;
    Ok(Json(out))
}

async fn get_snapshot(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult<Snapshot> {
    let id = parse_snapshot_id(&id)?;
    Ok(Json(st.with_space(move |s| s.snapshot_by_id(id)).await?))
}

// ---- diff -----------------------------------------------------------------

#[derive(Deserialize)]
struct DiffQuery {
    /// Base: `aN`, `sN` or `git:<commit>` (a commit an attempt was rebased
    /// onto). Defaults to the commit `b` was rebased onto, else the snapshot
    /// it was forked from (for `aN`), or the previous snapshot of the same
    /// attempt (for `sN`).
    a: Option<String>,
    b: String,
}

#[derive(Serialize)]
struct DiffResponse {
    /// The base snapshot; null when the base is a git commit.
    a: Option<Snapshot>,
    b: Snapshot,
    /// The base as a ref the file endpoint accepts (`sN` or `git:<commit>`).
    a_ref: String,
    b_ref: String,
    changes: Vec<FileChange>,
}

/// One side of a diff: a snapshot, the tree of a rebased-onto commit, or
/// the tree of another commit of the host repo (a stacked branch's base).
pub(crate) enum Side {
    Snap(Snapshot),
    Git(GitBase),
    Commit { commit: String, tree: String },
}

impl Side {
    fn tree(&self) -> &str {
        match self {
            Side::Snap(s) => &s.tree,
            Side::Git(g) => &g.tree,
            Side::Commit { tree, .. } => tree,
        }
    }

    fn label(&self) -> String {
        match self {
            Side::Snap(s) => format!("s{}", s.id),
            Side::Git(g) => format!("git:{}", g.commit),
            Side::Commit { commit, .. } => format!("git:{commit}"),
        }
    }
}

/// Resolve `aN`, `sN` or `git:<commit>` (a recorded rebase base, else any
/// commit of the host repo).
fn resolve_side(s: &Space, r: &str) -> Result<Side, ApiError> {
    if let Some(commit) = r.strip_prefix("git:") {
        if let Some(g) = s.git_base_by_commit(commit)? {
            return Ok(Side::Git(g));
        }
        return match s.commit_tree(commit)? {
            Some(tree) => Ok(Side::Commit {
                commit: commit.to_string(),
                tree,
            }),
            None => Err(ApiError::from(Error::NotFound {
                kind: "commit",
                id: r.to_string(),
            })),
        };
    }
    Ok(Side::Snap(s.resolve(s.parse_ref(r)?)?))
}

/// The default base for `b`, when the caller gives none.
fn base_of(s: &Space, b: Ref) -> agentvc_core::Result<Side> {
    let no_base = || Error::NotFound {
        kind: "base snapshot of",
        id: b.to_string(),
    };
    match b {
        Ref::Attempt(id) => {
            if let Some(g) = s.own_git_base(id)? {
                return Ok(Side::Git(g));
            }
            let p = s.attempt(id)?.parent_snapshot.ok_or_else(no_base)?;
            s.snapshot_by_id(p).map(Side::Snap)
        }
        Ref::Snapshot(id) => {
            let snap = s.snapshot_by_id(id)?;
            let prev = s
                .snapshots(snap.attempt_id)?
                .into_iter()
                .rfind(|x| x.id < id);
            match prev {
                Some(p) => Ok(Side::Snap(p)),
                None => {
                    let parent = s.attempt(snap.attempt_id)?.parent_snapshot;
                    s.snapshot_by_id(parent.ok_or_else(no_base)?)
                        .map(Side::Snap)
                }
            }
        }
    }
}

async fn diff(State(st): State<AppState>, Query(q): Query<DiffQuery>) -> ApiResult<DiffResponse> {
    let out = st
        .with_space(move |s| {
            // aN, sN or a tag name.
            let b = s.parse_ref(&q.b)?;
            let sb = s.resolve(b)?;
            let sa = match &q.a {
                Some(a) => match resolve_side(s, a) {
                    Ok(side) => side,
                    Err(e) => return Ok(Err(e)),
                },
                None => base_of(s, b)?,
            };
            let changes = agentvc_core::snapshot::diff(
                s.objects(),
                &Oid::from_hex(sa.tree())?,
                &sb.tree_oid()?,
            )?;
            Ok(Ok(DiffResponse {
                a_ref: sa.label(),
                b_ref: format!("s{}", sb.id),
                a: match sa {
                    Side::Snap(x) => Some(x),
                    Side::Git(_) | Side::Commit { .. } => None,
                },
                b: sb,
                changes,
            }))
        })
        .await??;
    Ok(Json(out))
}

// ---- files ----------------------------------------------------------------

#[derive(Deserialize)]
struct FileQuery {
    #[serde(rename = "ref")]
    r#ref: String,
    path: String,
}

#[derive(Serialize)]
struct FileResponse {
    #[serde(rename = "ref")]
    r#ref: String,
    /// The snapshot read, or null for a `git:<commit>` ref.
    snapshot: Option<i64>,
    path: String,
    /// `file`, `executable` or `symlink`.
    mode: &'static str,
    oid: String,
    size: usize,
    /// True when the content is not valid UTF-8 text.
    binary: bool,
    /// True when the file is larger than [`MAX_FILE_BYTES`].
    too_large: bool,
    /// UTF-8 text, or null for binary and oversized files.
    content: Option<String>,
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::File => "file",
        Mode::Executable => "executable",
        Mode::Symlink => "symlink",
        Mode::Tree => "tree",
    }
}

/// Find `path` in a stored tree by walking one directory per component.
pub(crate) fn lookup(
    s: &Space,
    tree: &Oid,
    path: &str,
) -> agentvc_core::Result<Option<(Mode, Oid)>> {
    let mut cur = *tree;
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    for (i, part) in parts.iter().enumerate() {
        let Some(e) = s
            .objects()
            .read_tree(&cur)?
            .into_iter()
            .find(|e| e.name == *part)
        else {
            return Ok(None);
        };
        let last = i + 1 == parts.len();
        match (last, e.mode) {
            (true, Mode::Tree) => return Ok(None),
            (true, m) => return Ok(Some((m, e.oid))),
            (false, Mode::Tree) => cur = e.oid,
            (false, _) => return Ok(None),
        }
    }
    Ok(None)
}

async fn file(State(st): State<AppState>, Query(q): Query<FileQuery>) -> ApiResult<FileResponse> {
    let out = st
        .with_space(move |s| {
            let side = match resolve_side(s, &q.r#ref) {
                Ok(side) => side,
                Err(e) => return Ok(Err(e)),
            };
            let tree = Oid::from_hex(side.tree())?;
            let not_found = || Error::NotFound {
                kind: "file",
                id: format!("{}:{}", q.r#ref, q.path),
            };
            let (mode, oid) = lookup(s, &tree, &q.path)?.ok_or_else(not_found)?;
            let data = s.objects().read_blob(&oid)?;
            let size = data.len();
            let too_large = size > MAX_FILE_BYTES;
            let text = if too_large {
                None
            } else {
                String::from_utf8(data).ok()
            };
            Ok(Ok(FileResponse {
                r#ref: q.r#ref,
                snapshot: match side {
                    Side::Snap(x) => Some(x.id),
                    Side::Git(_) | Side::Commit { .. } => None,
                },
                path: q.path,
                mode: mode_name(mode),
                oid: oid.to_hex(),
                size,
                binary: !too_large && text.is_none(),
                too_large,
                content: text,
            }))
        })
        .await??;
    Ok(Json(out))
}
