//! `POST /api/attempts/{id}/take`: take files or hunks of another attempt's
//! changes into this one, as `agentvc take` does (`merge` with no paths).

use agentvc_core::combine::{take as core_take, TakeOptions, TakeResult};
use agentvc_core::Attempt;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{parse_attempt_id, ApiResult};
use crate::review::json_body;
use crate::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TakeBody {
    /// The attempt to take from, as aN.
    source: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    hunks: Vec<String>,
    #[serde(default)]
    keep_conflicts: bool,
}

#[derive(Serialize)]
pub(crate) struct TakeResponse {
    take: TakeResult,
    attempt: Attempt,
}

/// Take as the daemon's actor. Conflicts answer 409 with code `conflict`
/// and `paths`, and change nothing unless `keep_conflicts` is set.
pub(crate) async fn take(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<TakeBody>, JsonRejection>,
) -> ApiResult<TakeResponse> {
    let body = json_body(body)?;
    let id = parse_attempt_id(&id)?;
    let source = parse_attempt_id(&body.source)?;
    let opts = TakeOptions {
        paths: body.paths,
        hunks: body.hunks,
        keep_conflicts: body.keep_conflicts,
        actor: Some(st.actor().to_string()),
    };
    let out = st
        .with_space(move |s| {
            let take = core_take(s, id, source, &opts)?;
            Ok(TakeResponse {
                take,
                attempt: s.attempt(id)?,
            })
        })
        .await?;
    st.poke();
    Ok(Json(out))
}
