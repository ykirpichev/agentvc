//! Conflicts left by a rebase, take or merge, and resolving them hunk by
//! hunk from the review UI (#92, #93):
//!
//! - `GET /api/attempts/{id}/conflicts`: the open conflicted files, each
//!   with its hunks (ours = the attempt, theirs = main or the source).
//! - `POST /api/attempts/{id}/conflicts/resolve` with
//!   `{path, hunks: [{index, take, text?}]}` or `{path, all: take}`, where
//!   take is `ours`, `theirs`, `both`, `both-theirs-first`, `base` or
//!   `custom` (with `text`). Writes the file into the view; the response
//!   has `snapshot` once no conflict is left.

use agentvc_core::conflicts::{Conflict, ConflictFile, HunkChoice, ResolveResult, Take};
use agentvc_core::Attempt;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{parse_attempt_id, ApiResult};
use crate::review::json_body;
use crate::AppState;

#[derive(Serialize)]
pub(crate) struct ConflictsResponse {
    attempt: Attempt,
    /// Open conflicts, oldest first.
    conflicts: Vec<Conflict>,
    /// The same files with their hunks, as they are in the view now.
    files: Vec<ConflictFile>,
}

pub(crate) async fn list(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<ConflictsResponse> {
    let id = parse_attempt_id(&id)?;
    let out = st
        .with_space(move |s| {
            let conflicts = s.conflicts(id, false)?;
            let files = conflicts
                .iter()
                .map(|c| s.conflict_file(id, &c.path))
                .collect::<agentvc_core::Result<_>>()?;
            Ok(ConflictsResponse {
                attempt: s.attempt(id)?,
                conflicts,
                files,
            })
        })
        .await?;
    Ok(Json(out))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolveBody {
    path: String,
    #[serde(default)]
    hunks: Vec<HunkChoice>,
    /// Applied to every hunk when `hunks` is empty.
    all: Option<Take>,
}

#[derive(Serialize)]
pub(crate) struct ResolveResponse {
    resolve: ResolveResult,
    attempt: Attempt,
}

pub(crate) async fn resolve(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<ResolveBody>, JsonRejection>,
) -> ApiResult<ResolveResponse> {
    let body = json_body(body)?;
    let id = parse_attempt_id(&id)?;
    let actor = st.actor().to_string();
    let out = st
        .with_space(move |s| {
            let resolve = s.resolve_conflict(id, &body.path, &body.hunks, body.all, &actor)?;
            Ok(ResolveResponse {
                resolve,
                attempt: s.attempt(id)?,
            })
        })
        .await?;
    st.poke();
    Ok(Json(out))
}
