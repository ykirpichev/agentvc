//! Workspaces over the API (#107 follow-up): every worktree of the repo
//! with its stack, edits to any of them, and a poller that turns changes in
//! any workspace into events.
//!
//! - `GET /api/workspaces[?remote=1]`: `agentvc workspace ls --json`.
//! - `POST /api/workspaces/{id}/stack` with `{"op": "move"|"restack"|
//!   "rename"|"delete"|"sync", "branch", "onto", "new_name", "queue"}`:
//!   `{"outcome": "applied"|"queued", "request_id", "result"}`.
//! - `POST /api/requests/{id}/cancel`: cancel a queued edit.
//! - `GET /api/workspaces/{id}/branches/{name}`: a branch's own commits and
//!   files.
//! - A restack stopped on a conflict (`"keep_conflicts": true` on an edit):
//!   `GET /api/workspaces/{id}/conflicts`, `POST .../conflicts/resolve`
//!   `{"path", "hunks": [{"index", "take", "text"}]}` (hunks from the
//!   branch's side: `ours` is the branch), then `POST .../continue` or
//!   `POST .../abort`.
//!
//! Events: `stack_changed {workspace}`, `workspaces_changed`, and
//! `request_queued|request_applied|request_failed|request_cancelled
//! {request_id, workspace, reason}`.

use std::path::PathBuf;
use std::time::Duration;

use agentvc_core::workspace::{
    self, EditOutcome, Fingerprints, Op, Request, RequestStatus, WorkspaceList,
};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiResult};
use crate::events::Event;
use crate::review::json_body;
use crate::AppState;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/workspaces", get(list))
        .route("/workspaces/{id}/stack", post(edit))
        .route("/requests/{id}/cancel", post(cancel))
        .route("/workspaces/{id}/branches/{name}", get(branch))
        .route("/workspaces/{id}/conflicts", get(conflicts))
        .route("/workspaces/{id}/conflicts/resolve", post(resolve))
        .route("/workspaces/{id}/continue", post(cont))
        .route("/workspaces/{id}/abort", post(abort))
}

#[derive(Deserialize)]
pub(crate) struct ListQuery {
    /// Also fetch workspaces published from other machines.
    #[serde(default)]
    remote: Option<String>,
}

async fn list(State(st): State<AppState>, Query(q): Query<ListQuery>) -> ApiResult<WorkspaceList> {
    let remote = q.remote.is_some_and(|r| r != "0" && r != "false");
    let dir = st.workdir().to_path_buf();
    Ok(Json(
        st.with_space(move |s| workspace::list(&dir, Some(s), remote))
            .await?,
    ))
}

#[derive(Deserialize)]
pub(crate) struct EditBody {
    #[serde(flatten)]
    op: Op,
    /// Queue even if the workspace is idle.
    #[serde(default)]
    queue: bool,
    /// Stop a conflicting restack in the worktree, to resolve it here.
    #[serde(default)]
    keep_conflicts: bool,
}

/// Blocking work off the async runtime, without the space lock.
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

async fn edit(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<EditBody>, JsonRejection>,
) -> ApiResult<EditOutcome> {
    let body = json_body(body)?;
    let dir = st.workdir().to_path_buf();
    let from = format!("api:{}", st.actor());
    let out = blocking(move || {
        workspace::edit_with(
            &dir,
            &id,
            body.op,
            Some(&from),
            body.queue,
            body.keep_conflicts,
        )
    })
    .await;
    st.poke_workspaces();
    Ok(Json(out?))
}

#[derive(Serialize)]
pub(crate) struct CancelResponse {
    request: Request,
}

async fn cancel(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult<CancelResponse> {
    let dir = st.workdir().to_path_buf();
    let request = blocking(move || workspace::cancel(&dir, &id)).await?;
    st.poke_workspaces();
    Ok(Json(CancelResponse { request }))
}

async fn branch(
    State(st): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<workspace::BranchDetail> {
    let dir = st.workdir().to_path_buf();
    Ok(Json(
        blocking(move || workspace::branch_detail(&dir, &id, &name)).await?,
    ))
}

async fn conflicts(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<workspace::WsConflicts> {
    let dir = st.workdir().to_path_buf();
    Ok(Json(
        blocking(move || workspace::conflicts(&dir, &id)).await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolveBody {
    path: String,
    hunks: Vec<agentvc_core::conflicts::HunkChoice>,
}

async fn resolve(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<ResolveBody>, JsonRejection>,
) -> ApiResult<workspace::WsResolve> {
    let body = json_body(body)?;
    let dir = st.workdir().to_path_buf();
    let out = blocking(move || workspace::resolve(&dir, &id, &body.path, &body.hunks)).await;
    st.poke_workspaces();
    Ok(Json(out?))
}

async fn cont(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<workspace::WsFinish> {
    finish(st, id, false).await
}

async fn abort(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<workspace::WsFinish> {
    finish(st, id, true).await
}

async fn finish(st: AppState, id: String, abort: bool) -> ApiResult<workspace::WsFinish> {
    let dir = st.workdir().to_path_buf();
    let out = blocking(move || workspace::finish(&dir, &id, abort)).await;
    st.poke_workspaces();
    Ok(Json(out?))
}

/// The events that take `old` to `new`.
pub(crate) fn diff(old: &Fingerprints, new: &Fingerprints) -> Vec<Event> {
    let mut out = Vec::new();
    let (a, b): (Vec<_>, Vec<_>) = (
        old.workspaces.keys().collect(),
        new.workspaces.keys().collect(),
    );
    if a != b {
        out.push(Event::WorkspacesChanged);
    }
    for (id, fp) in &new.workspaces {
        if old.workspaces.get(id).is_some_and(|o| o != fp) {
            out.push(Event::StackChanged {
                workspace: id.clone(),
            });
        }
    }
    for (rid, (ws, status, reason)) in &new.requests {
        let before = old.requests.get(rid).map(|r| r.1);
        if before == Some(*status) {
            continue;
        }
        let (request_id, workspace, reason) = (rid.clone(), ws.clone(), reason.clone());
        out.push(match status {
            RequestStatus::Queued => Event::RequestQueued {
                request_id,
                workspace,
                reason,
            },
            RequestStatus::Applied => Event::RequestApplied {
                request_id,
                workspace,
            },
            RequestStatus::Failed => Event::RequestFailed {
                request_id,
                workspace,
                reason,
            },
            RequestStatus::Cancelled => Event::RequestCancelled {
                request_id,
                workspace,
            },
        });
    }
    out
}

/// Watch every workspace of the repo at `dir` and broadcast what changes.
/// Quiet (and stops) when the space is not in a git worktree.
pub(crate) fn spawn_poller(state: AppState, dir: PathBuf, every: Duration) {
    tokio::spawn(async move {
        let probe = {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || workspace::fingerprints(&dir)).await
        };
        let Ok(Ok(mut last)) = probe else { return };
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = state.workspaces_poked() => {}
            }
            let d = dir.clone();
            let Ok(res) = tokio::task::spawn_blocking(move || workspace::fingerprints(&d)).await
            else {
                break;
            };
            match res {
                Ok(now) => {
                    for e in diff(&last, &now) {
                        state.send(e);
                    }
                    last = now;
                }
                Err(e) => eprintln!("agentvcd: workspace poll failed: {e}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_reports_stacks_sets_and_request_transitions() {
        let mut a = Fingerprints::default();
        a.workspaces.insert("w1".into(), "x".into());
        a.requests
            .insert("r1".into(), ("w1".into(), RequestStatus::Queued, None));
        let mut b = a.clone();
        assert!(diff(&a, &b).is_empty());
        b.workspaces.insert("w1".into(), "y".into());
        b.workspaces.insert("w2".into(), "z".into());
        b.requests
            .insert("r1".into(), ("w1".into(), RequestStatus::Applied, None));
        b.requests.insert(
            "r2".into(),
            ("w2".into(), RequestStatus::Failed, Some("nope".into())),
        );
        let ev: Vec<String> = diff(&a, &b)
            .iter()
            .map(|e| {
                serde_json::to_value(e).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            ev,
            [
                "workspaces_changed",
                "stack_changed",
                "request_applied",
                "request_failed"
            ]
        );
    }
}
