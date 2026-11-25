//! Read-only JSON API over the space.

use std::collections::BTreeMap;

use agentvc_core::objects::{Mode, Oid};
use agentvc_core::snapshot::FileChange;
use agentvc_core::{Attempt, Error, Ref, Snapshot, Space, Task};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{events, AppState};

/// Files larger than this are listed but their content is not sent.
pub const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/space", get(space_info))
        .route("/tasks", get(list_tasks))
        .route("/tasks/{id}", get(get_task))
        .route("/attempts", get(list_attempts))
        .route("/attempts/{id}", get(get_attempt))
        .route("/attempts/{id}/snapshots", get(list_snapshots))
        .route("/snapshots/{id}", get(get_snapshot))
        .route("/diff", get(diff))
        .route("/file", get(file))
        .route("/events", get(events::websocket))
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
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: String) -> ApiError {
        ApiError {
            status,
            code,
            message,
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
            Error::AttemptClosed { .. } => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError::new(status, e.code(), e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "code": self.code, "message": self.message } });
        (self.status, Json(body)).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

fn parse_ref(s: &str) -> Result<Ref, ApiError> {
    s.parse::<Ref>().map_err(ApiError::from)
}

/// Accepts `t3` or `3`.
fn parse_task_id(s: &str) -> Result<i64, ApiError> {
    s.strip_prefix('t')
        .unwrap_or(s)
        .parse()
        .map_err(|_| ApiError::bad_request("bad_task", format!("invalid task id {s:?}")))
}

/// Accepts `a3` or `3`.
fn parse_attempt_id(s: &str) -> Result<i64, ApiError> {
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

// ---- tasks ----------------------------------------------------------------

/// Number of attempts per status.
#[derive(Default, Serialize)]
pub struct StatusCounts {
    total: usize,
    exploring: usize,
    passed: usize,
    failed: usize,
    abandoned: usize,
    promoted: usize,
}

impl StatusCounts {
    fn add(&mut self, a: &Attempt) {
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
            Ok(AttemptDetail {
                attempt,
                head,
                snapshot_count,
                children,
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
    /// Base. Defaults to the snapshot `b` was forked from (for `aN`) or the
    /// previous snapshot of the same attempt (for `sN`).
    a: Option<String>,
    b: String,
}

#[derive(Serialize)]
struct DiffResponse {
    a: Snapshot,
    b: Snapshot,
    changes: Vec<FileChange>,
}

/// The default base for `b`, when the caller gives none.
fn base_of(s: &Space, b: Ref) -> agentvc_core::Result<Snapshot> {
    let no_base = || Error::NotFound {
        kind: "base snapshot of",
        id: b.to_string(),
    };
    match b {
        Ref::Attempt(id) => {
            let p = s.attempt(id)?.parent_snapshot.ok_or_else(no_base)?;
            s.snapshot_by_id(p)
        }
        Ref::Snapshot(id) => {
            let snap = s.snapshot_by_id(id)?;
            let prev = s
                .snapshots(snap.attempt_id)?
                .into_iter()
                .rfind(|x| x.id < id);
            match prev {
                Some(p) => Ok(p),
                None => {
                    let parent = s.attempt(snap.attempt_id)?.parent_snapshot;
                    s.snapshot_by_id(parent.ok_or_else(no_base)?)
                }
            }
        }
    }
}

async fn diff(State(st): State<AppState>, Query(q): Query<DiffQuery>) -> ApiResult<DiffResponse> {
    let b = parse_ref(&q.b)?;
    let a = q.a.as_deref().map(parse_ref).transpose()?;
    let out = st
        .with_space(move |s| {
            let sb = s.resolve(b)?;
            let sa = match a {
                Some(a) => s.resolve(a)?,
                None => base_of(s, b)?,
            };
            let changes = s.diff(Ref::Snapshot(sa.id), Ref::Snapshot(sb.id))?;
            Ok(DiffResponse {
                a: sa,
                b: sb,
                changes,
            })
        })
        .await?;
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
    snapshot: i64,
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
fn lookup(s: &Space, tree: &Oid, path: &str) -> agentvc_core::Result<Option<(Mode, Oid)>> {
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
    let r = parse_ref(&q.r#ref)?;
    let out = st
        .with_space(move |s| {
            let snap = s.resolve(r)?;
            let tree = Oid::from_hex(&snap.tree)?;
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
            Ok(FileResponse {
                r#ref: q.r#ref,
                snapshot: snap.id,
                path: q.path,
                mode: mode_name(mode),
                oid: oid.to_hex(),
                size,
                binary: !too_large && text.is_none(),
                too_large,
                content: text,
            })
        })
        .await?;
    Ok(Json(out))
}
