//! `GET /api/query` and `GET /api/blame`: attempt history queries, as
//! `agentvc log` with filters and `agentvc blame` (#73).
//!
//! List parameters (`status`, `touched`, `meta`) are comma-separated.

use agentvc_core::blame::Blame;
use agentvc_core::query::{parse_since, Query as HistoryQuery, Row};
use agentvc_core::{Error, Status};
use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;

use crate::api::{parse_attempt_id, ApiResult};
use crate::AppState;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryParams {
    task: Option<String>,
    status: Option<String>,
    actor: Option<String>,
    model: Option<String>,
    touched: Option<String>,
    since: Option<String>,
    meta: Option<String>,
    grep: Option<String>,
}

fn list(s: &Option<String>) -> Vec<String> {
    s.as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

fn bad(msg: String) -> crate::api::ApiError {
    Error::BadQuery(msg).into()
}

pub(crate) async fn query(
    State(st): State<AppState>,
    Query(p): Query<QueryParams>,
) -> ApiResult<Vec<Row>> {
    let task = match p.task.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => Some(
            t.trim_start_matches('t')
                .parse::<i64>()
                .map_err(|_| bad(format!("task {t:?}: expected tN")))?,
        ),
        None => None,
    };
    let status = list(&p.status)
        .iter()
        .map(|s| Status::parse(s).ok_or_else(|| bad(format!("unknown status {s:?}"))))
        .collect::<Result<_, _>>()?;
    let meta = list(&p.meta)
        .iter()
        .map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| bad(format!("meta {kv:?}: expected KEY=VALUE")))
        })
        .collect::<Result<_, _>>()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let since = match p.since.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => Some(parse_since(s, now)?),
        None => None,
    };
    let q = HistoryQuery {
        task,
        status,
        actor: p.actor.filter(|s| !s.is_empty()),
        model: p.model.filter(|s| !s.is_empty()),
        touched: list(&p.touched),
        since,
        meta,
        grep: p.grep.filter(|s| !s.is_empty()),
    };
    let rows = st.with_space(move |s| s.query(&q)).await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BlameParams {
    /// An attempt (`aN`), or with `ref` any browse ref (#171) instead.
    attempt: Option<String>,
    #[serde(rename = "ref")]
    r#ref: Option<String>,
    path: String,
    /// Include the view's unsnapshotted work (default true).
    working: Option<bool>,
}

pub(crate) async fn blame(
    State(st): State<AppState>,
    Query(p): Query<BlameParams>,
) -> ApiResult<Blame> {
    let Some(attempt) = p.attempt else {
        let r = p.r#ref.ok_or_else(|| {
            crate::api::ApiError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "bad_ref",
                "blame what? give attempt or ref".into(),
            )
        })?;
        let b = st
            .with_space(move |s| Ok(crate::code::blame(s, &r, &p.path)))
            .await??;
        return Ok(Json(b));
    };
    let id = parse_attempt_id(&attempt)?;
    let working = p.working.unwrap_or(true);
    let b = st
        .with_space(move |s| s.blame(id, &p.path, working))
        .await?;
    Ok(Json(b))
}
