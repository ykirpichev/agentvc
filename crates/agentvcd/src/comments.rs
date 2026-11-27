//! Review comments over the API (#167). See `agentvc_core::comments`.
//!
//! - `GET /api/comments?target=KEY[&state=open][&drafts=1]` (or
//!   `branch=WS/NAME`, `agent=agN`, `attempt=aN`): `{threads}`, each with
//!   `current` (its lines now) and `outdated`.
//! - `POST /api/comments {target, lines?: {path, side, start, end}, body,
//!   pending?}`: `{thread}`.
//! - `POST /api/comments/{id}/reply {body}`, `/resolve`, `/reopen`;
//!   `DELETE /api/comments/{id}` deletes your own draft.
//! - `POST /api/reviews/submit {target, to: "agent"|"github"|"both"|"none"}`:
//!   the drafts become one review; `{review, threads, delivery, agent}`.
//! - `POST /api/reviews/discard {target}`: delete all your drafts there.
//! - `POST /api/comments/sync {target}`: bring in the branch's GitHub PR
//!   review threads and push local replies and states (#170); `no_pr` when
//!   it has none.
//!
//! Event: `comments_changed {targets}` whenever the store changes, from
//! here or from the CLI, an agent's MCP call or its hook.
//!
//! Calls relayed by the hosted service may read but not write, like agents.

use std::path::PathBuf;
use std::time::Duration;

use agentvc_core::comments::{
    self, AddRequest, Deliver, Filter, LineRange, Submitted, Target, Thread,
};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::agents::{agentvc_exe, local_or_allowed};
use crate::api::{ApiError, ApiResult};
use crate::events::Event;
use crate::review::json_body;
use crate::AppState;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/comments", get(list).post(add))
        .route("/comments/{id}", get(show).delete(discard))
        .route("/comments/{id}/reply", post(reply))
        .route("/comments/{id}/resolve", post(resolve))
        .route("/comments/{id}/reopen", post(reopen))
        .route("/comments/sync", post(sync))
        .route("/reviews/submit", post(submit))
        .route("/reviews/discard", post(discard_all))
}

async fn blocking<T, F>(f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> agentvc_core::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::from)
}

fn bad_target(msg: String) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "bad_target", msg)
}

fn parse(key: &str) -> Result<Target, ApiError> {
    Target::parse_key(key).map_err(|e| bad_target(e.to_string()))
}

#[derive(Deserialize, Default)]
struct ListQuery {
    target: Option<String>,
    branch: Option<String>,
    agent: Option<String>,
    attempt: Option<String>,
    /// `open`: open and addressed threads only.
    state: Option<String>,
    drafts: Option<String>,
}

impl ListQuery {
    fn target(&self) -> Result<Option<Target>, ApiError> {
        if let Some(t) = &self.target {
            return parse(t).map(Some);
        }
        if let Some(b) = &self.branch {
            return parse(&format!("branch:{b}")).map(Some);
        }
        if let Some(a) = &self.agent {
            return parse(&format!("agent:{a}")).map(Some);
        }
        if let Some(a) = &self.attempt {
            return parse(&format!("attempt:{a}")).map(Some);
        }
        Ok(None)
    }
}

fn flag(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|v| v != "0" && v != "false")
}

#[derive(Serialize)]
struct ThreadsResponse {
    threads: Vec<Thread>,
}

#[derive(Serialize)]
struct ThreadResponse {
    thread: Thread,
}

async fn list(
    State(st): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<ThreadsResponse> {
    let f = Filter {
        target: q.target()?,
        active: q.state.as_deref() == Some("open"),
        pending: flag(&q.drafts),
    };
    let me = st.actor().to_string();
    let dir = st.workdir().to_path_buf();
    let mut threads = blocking(move || comments::list(&dir, &f)).await?;
    // Drafts are private to whoever writes them.
    threads.retain(|t| t.state != comments::State::Pending || t.author() == me);
    Ok(Json(ThreadsResponse { threads }))
}

async fn show(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult<ThreadResponse> {
    let dir = st.workdir().to_path_buf();
    let thread = blocking(move || comments::get(&dir, &id)).await?;
    Ok(Json(ThreadResponse { thread }))
}

#[derive(Deserialize)]
struct AddBody {
    target: String,
    #[serde(default)]
    lines: Option<LineRange>,
    body: String,
    #[serde(default)]
    pending: bool,
}

async fn add(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<AddBody>, JsonRejection>,
) -> ApiResult<ThreadResponse> {
    local_or_allowed(&headers)?;
    let b = json_body(body)?;
    let req = AddRequest {
        target: parse(&b.target)?,
        lines: b.lines,
        body: b.body,
        author: st.actor().to_string(),
        pending: b.pending,
    };
    let dir = st.workdir().to_path_buf();
    let thread = blocking(move || comments::add(&dir, &req)).await?;
    changed(&st, &[thread.target.key()]);
    Ok(Json(ThreadResponse { thread }))
}

#[derive(Deserialize)]
struct ReplyBody {
    body: String,
}

async fn reply(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<ReplyBody>, JsonRejection>,
) -> ApiResult<ThreadResponse> {
    local_or_allowed(&headers)?;
    let b = json_body(body)?;
    let me = st.actor().to_string();
    let dir = st.workdir().to_path_buf();
    let thread = blocking(move || comments::reply(&dir, &id, &b.body, &me)).await?;
    changed(&st, &[thread.target.key()]);
    Ok(Json(ThreadResponse { thread }))
}

async fn resolve(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<ThreadResponse> {
    local_or_allowed(&headers)?;
    let me = st.actor().to_string();
    let dir = st.workdir().to_path_buf();
    let thread = blocking(move || comments::resolve(&dir, &id, &me)).await?;
    changed(&st, &[thread.target.key()]);
    Ok(Json(ThreadResponse { thread }))
}

async fn reopen(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<ThreadResponse> {
    local_or_allowed(&headers)?;
    let me = st.actor().to_string();
    let dir = st.workdir().to_path_buf();
    let thread = blocking(move || comments::reopen(&dir, &id, &me)).await?;
    changed(&st, &[thread.target.key()]);
    Ok(Json(ThreadResponse { thread }))
}

async fn discard(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<ThreadResponse> {
    local_or_allowed(&headers)?;
    let me = st.actor().to_string();
    let dir = st.workdir().to_path_buf();
    let thread = blocking(move || comments::discard(&dir, &id, &me)).await?;
    changed(&st, &[thread.target.key()]);
    Ok(Json(ThreadResponse { thread }))
}

#[derive(Deserialize)]
struct SubmitBody {
    target: String,
    /// `agent`, `github`, `both` or `none` (default).
    #[serde(default)]
    to: Option<String>,
}

async fn submit(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<SubmitBody>, JsonRejection>,
) -> ApiResult<Submitted> {
    local_or_allowed(&headers)?;
    let b = json_body(body)?;
    let target = parse(&b.target)?;
    let to = match b.to.as_deref().unwrap_or("none") {
        "none" => Deliver::default(),
        "agent" => Deliver {
            agent: true,
            github: false,
        },
        "github" => Deliver {
            agent: false,
            github: true,
        },
        "both" => Deliver {
            agent: true,
            github: true,
        },
        other => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "bad_destination",
                format!("{other:?} is not agent, github, both or none"),
            ))
        }
    };
    let me = st.actor().to_string();
    let dir = st.workdir().to_path_buf();
    let key = target.key();
    let exe = agentvc_exe();
    let out = blocking(move || comments::submit(&dir, &target, to, &me, Some(&exe))).await?;
    changed(&st, &[key]);
    st.poke_agents();
    Ok(Json(out))
}

#[derive(Deserialize)]
struct SyncBody {
    target: String,
}

async fn sync(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<SyncBody>, JsonRejection>,
) -> ApiResult<comments::github::Synced> {
    local_or_allowed(&headers)?;
    let b = json_body(body)?;
    let target = parse(&b.target)?;
    let dir = st.workdir().to_path_buf();
    let key = target.key();
    let out = blocking(move || comments::github::sync(&dir, &target)).await?;
    if !out.imported.is_empty() || !out.updated.is_empty() || !out.pushed.is_empty() {
        changed(&st, &[key]);
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
struct DiscardBody {
    target: String,
}

#[derive(Serialize)]
struct Discarded {
    discarded: Vec<String>,
}

async fn discard_all(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<DiscardBody>, JsonRejection>,
) -> ApiResult<Discarded> {
    local_or_allowed(&headers)?;
    let b = json_body(body)?;
    let target = parse(&b.target)?;
    let me = st.actor().to_string();
    let dir = st.workdir().to_path_buf();
    let key = target.key();
    let discarded = blocking(move || comments::discard_all(&dir, &target, &me)).await?;
    changed(&st, &[key]);
    Ok(Json(Discarded { discarded }))
}

fn changed(st: &AppState, targets: &[String]) {
    st.send(Event::CommentsChanged {
        targets: targets.to_vec(),
    });
    // The sidebar's badges come with the workspaces.
    st.poke_workspaces();
}

/// Watch the store for writes from other processes (the CLI, agents).
pub(crate) fn spawn_poller(state: AppState, dir: PathBuf, every: Duration) {
    tokio::spawn(async move {
        let d = dir.clone();
        let mut last = tokio::task::spawn_blocking(move || comments::fingerprint(&d))
            .await
            .ok()
            .flatten();
        let mut since = agentvc_core::space::now();
        loop {
            tokio::time::sleep(every).await;
            let d = dir.clone();
            let Ok(now) = tokio::task::spawn_blocking(move || comments::fingerprint(&d)).await
            else {
                continue;
            };
            if now != last {
                let d = dir.clone();
                let s = since;
                let targets = tokio::task::spawn_blocking(move || comments::touched_targets(&d, s))
                    .await
                    .unwrap_or_default();
                state.send(Event::CommentsChanged { targets });
                state.poke_workspaces();
                last = now;
                since = agentvc_core::space::now() - 1;
            }
        }
    });
}
