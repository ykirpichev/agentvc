//! `POST /api/attempts/{id}/rebase`: move an attempt onto a newer commit of
//! the host repository, as `agentvc rebase` does.

use agentvc_core::rebase::{rebase as core_rebase, RebaseOptions};
use agentvc_core::{Attempt, RebaseResult};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{parse_attempt_id, ApiResult};
use crate::review::json_body;
use crate::AppState;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RebaseBody {
    /// Git revision; defaults to the remote's default branch, fetched.
    onto: Option<String>,
    remote: Option<String>,
    #[serde(default)]
    no_fetch: bool,
    #[serde(default)]
    keep_conflicts: bool,
}

#[derive(Serialize)]
pub(crate) struct RebaseResponse {
    rebase: RebaseResult,
    attempt: Attempt,
}

/// Rebase as the daemon's actor. Conflicts answer 409 with code `conflict`
/// and `paths`, and change nothing unless `keep_conflicts` is set.
pub(crate) async fn rebase(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<RebaseBody>, JsonRejection>,
) -> ApiResult<RebaseResponse> {
    let body = json_body(body)?;
    let id = parse_attempt_id(&id)?;
    let opts = RebaseOptions {
        onto: body.onto.filter(|o| !o.trim().is_empty()),
        remote: body.remote,
        no_fetch: body.no_fetch,
        keep_conflicts: body.keep_conflicts,
        actor: Some(st.actor().to_string()),
    };
    let out = st
        .with_space(move |s| {
            let rebase = core_rebase(s, id, &opts)?;
            Ok(RebaseResponse {
                rebase,
                attempt: s.attempt(id)?,
            })
        })
        .await?;
    st.poke();
    Ok(Json(out))
}
