//! The branch graph the review UI draws (`GET /api/graph`), plus the two
//! actions it needs beside the review ones: undo the last operation and
//! open a pull request for an attempt.
//!
//! The graph is main's recent commits (the trunk) with every attempt hung
//! off the commit it is based on, or off the attempt it was forked from.

use std::collections::{BTreeSet, HashMap};

use agentvc_core::github::{open_pr, PrOptions, PrResult};
use agentvc_core::status::TrunkCommit;
use agentvc_core::undo::UndoResult;
use agentvc_core::{Attempt, Error, Space};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::api::{parse_attempt_id, ApiResult};
use crate::review::json_body;
use crate::AppState;

/// Most trunk commits sent, newest first.
const MAX_TRUNK: usize = 60;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/graph", get(graph))
        .route("/undo", post(undo))
        .route("/attempts/{id}/pr", post(pr))
}

#[derive(Serialize)]
pub(crate) struct Graph {
    /// The remote-tracking branch the trunk follows, like `origin/main`;
    /// `HEAD` when the repo has no remote.
    upstream: String,
    /// Main's commits, newest first, down to the oldest one an attempt is
    /// based on. Bases that are not on main come last, `on_upstream: false`.
    trunk: Vec<TrunkCommit>,
    attempts: Vec<GraphAttempt>,
}

#[derive(Serialize)]
struct GraphAttempt {
    attempt: Attempt,
    /// The git commit its head is based on.
    base_commit: Option<String>,
    /// Commits on the upstream its base lacks; `None` when unknown.
    behind: Option<u64>,
    /// Conflicted files still open (rebase, take or merge left markers).
    conflicts: usize,
    head_snapshot: Option<i64>,
    pr: Option<PrRef>,
}

#[derive(Serialize)]
struct PrRef {
    number: u64,
    url: String,
    /// `open`, `closed` or `merged`; `None` before the first refresh.
    state: Option<String>,
}

async fn graph(State(st): State<AppState>) -> ApiResult<Graph> {
    Ok(Json(st.with_space(build).await?))
}

fn build(s: &Space) -> agentvc_core::Result<Graph> {
    let upstream = s.upstream_branch().unwrap_or_else(|| "HEAD".to_string());

    let mut attempts = Vec::new();
    for attempt in s.attempts(None)? {
        let head = match s.head_snapshot(attempt.id) {
            Ok(h) => Some(h),
            Err(Error::NotFound { .. }) => None,
            Err(e) => return Err(e),
        };
        let base_commit = match &head {
            Some(h) => s.git_base_commit(h)?,
            None => None,
        };
        let open = attempt.status.is_open();
        let conflicts = if open {
            s.conflicts(attempt.id, false)?.len()
        } else {
            0
        };
        let pr = match s.pr_feedback(attempt.id)? {
            Some(f) => Some(PrRef {
                number: f.number,
                url: f.url,
                state: Some(f.state),
            }),
            None => s.pr_url(attempt.id)?.map(|url| PrRef {
                number: agentvc_core::github::parse_pr_url(&url)
                    .map(|(_, n)| n)
                    .unwrap_or(0),
                url,
                state: None,
            }),
        };
        attempts.push(GraphAttempt {
            head_snapshot: head.as_ref().map(|h| h.id),
            attempt,
            base_commit,
            behind: None,
            conflicts,
            pr,
        });
    }

    let bases: BTreeSet<String> = attempts
        .iter()
        .filter_map(|a| a.base_commit.clone())
        .collect();
    let trunk = s.trunk(&upstream, &bases, MAX_TRUNK);
    let mut behind: HashMap<String, Option<u64>> = HashMap::new();
    for a in &mut attempts {
        if let Some(base) = &a.base_commit {
            a.behind = *behind
                .entry(base.clone())
                .or_insert_with(|| s.commits_behind(base, &upstream));
        }
    }
    Ok(Graph {
        upstream,
        trunk,
        attempts,
    })
}

// ---- undo -------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct UndoBody {
    /// Operation id; the newest undoable one when absent.
    op: Option<i64>,
}

async fn undo(
    State(st): State<AppState>,
    body: Result<Json<UndoBody>, JsonRejection>,
) -> ApiResult<UndoResult> {
    let body = json_body(body)?;
    let actor = st.actor().to_string();
    let out = st.with_space(move |s| s.undo(&actor, body.op)).await?;
    st.poke();
    Ok(Json(out))
}

// ---- open a PR --------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrBody {
    #[serde(default)]
    draft: bool,
    title: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct PrResponse {
    pr: PrResult,
    attempt: Attempt,
}

/// Export the attempt, push its branch and open (or update) its PR, as
/// `agentvc pr aN` does with its defaults and `pr.*` config.
async fn pr(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<PrBody>, JsonRejection>,
) -> ApiResult<PrResponse> {
    let body = json_body(body)?;
    let id = parse_attempt_id(&id)?;
    let out = st
        .with_space(move |s| {
            let opts = PrOptions {
                draft: body.draft,
                title: body.title.filter(|t| !t.trim().is_empty()),
                ..PrOptions::default()
            };
            let pr = open_pr(s, id, &opts)?;
            Ok(PrResponse {
                pr,
                attempt: s.attempt(id)?,
            })
        })
        .await?;
    st.poke();
    Ok(Json(out))
}
