//! Agents over the API (#145): start coding agents on any workspace's
//! stack, see which need a person, answer them and land their work. See
//! `agentvc_core::agent`.
//!
//! - `GET /api/agents[?all=1]`: `agentvc agent ls --json`.
//! - `POST /api/agents` `{workspace, from, at?, prompt, mode: "new"|"revise",
//!   model?, check?, command?}`: `{agent}`.
//! - `GET /api/agents/{id}`: `{agent, steps}`.
//! - `POST /api/agents/{id}/reply {text}`, `/approve`, `/deny {reason?}`,
//!   `/stop`, `/land`, `/discard`.
//!
//! Event: `agent_changed {agent, state}` whenever an agent's record changes.
//!
//! Calls relayed from the hosted service may read agents but not start,
//! answer or land them unless the daemon runs with `--allow-remote-agents`:
//! those run code on this machine.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use agentvc_core::agent::{
    self, Agent, AgentDetail, AgentList, AgentSettings, AgentState, Mode, StartRequest,
    WorkspaceAgentSettings,
};
use agentvc_core::workspace::EditOutcome;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiResult};
use crate::events::Event;
use crate::review::json_body;
use crate::AppState;

/// Header the cloud relay puts on every call it runs here.
pub(crate) const RELAY_HEADER: &str = "x-agentvc-relay";

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/agents", get(list).post(start))
        .route("/agents/{id}", get(show))
        .route("/agents/{id}/reply", post(reply))
        .route("/agents/{id}/approve", post(approve))
        .route("/agents/{id}/deny", post(deny))
        .route("/agents/{id}/stop", post(stop))
        .route("/agents/{id}/land", post(land))
        .route("/agents/{id}/discard", post(discard))
        .route("/agent-permissions", get(permissions))
        .route(
            "/workspaces/{id}/agent-settings",
            get(get_settings).put(put_settings).post(put_settings),
        )
}

/// What agents may do, per workspace and per open agent.
async fn permissions(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<agent::PermissionsReport> {
    // It names files on this machine; not for the hosted service.
    local_or_allowed(&headers)?;
    let dir = st.workdir().to_path_buf();
    Ok(Json(blocking(move || agent::permissions(&dir)).await?))
}

async fn get_settings(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<WorkspaceAgentSettings> {
    let dir = st.workdir().to_path_buf();
    Ok(Json(
        blocking(move || agent::settings(&dir, Some(&id))).await?,
    ))
}

async fn put_settings(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<AgentSettings>, JsonRejection>,
) -> ApiResult<WorkspaceAgentSettings> {
    local_or_allowed(&headers)?;
    let b = json_body(body)?;
    // The hosted service never sets shell commands on this machine.
    if relayed(&headers) && (b.command.is_some() || b.check.is_some()) {
        return Err(remote_shell_off());
    }
    if relayed(&headers) && b.permissions.as_deref() == Some("bypass") {
        return Err(remote_bypass_off());
    }
    let dir = st.workdir().to_path_buf();
    let r = blocking(move || agent::set_settings(&dir, Some(&id), b)).await?;
    st.poke_workspaces();
    Ok(Json(r))
}

fn relayed(h: &HeaderMap) -> bool {
    h.contains_key(RELAY_HEADER)
}

fn remote_shell_off() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "remote_shell_off",
        "commands and check commands can only be set on this machine, not through the hosted service".into(),
    )
}

fn remote_bypass_off() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "remote_bypass_off",
        "agents can only be set to never ask on this machine, not through the hosted service"
            .into(),
    )
}

/// Refuse a relayed call that would run or steer an agent here.
pub(crate) fn local_or_allowed(h: &HeaderMap) -> Result<(), ApiError> {
    match h.get(RELAY_HEADER).and_then(|v| v.to_str().ok()) {
        None | Some("agents") => Ok(()),
        Some(_) => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "remote_agents_off",
            "this machine does not take agent commands from the hosted service; start its daemon with --allow-remote-agents".into(),
        )),
    }
}

/// The `agentvc` program the daemon starts workers with: `$AGENTVC_BIN`,
/// else next to `agentvcd`, else on `PATH`.
pub(crate) fn agentvc_exe() -> PathBuf {
    if let Some(p) = std::env::var_os("AGENTVC_BIN").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| {
            let name = if cfg!(windows) {
                "agentvc.exe"
            } else {
                "agentvc"
            };
            let p = e.with_file_name(name);
            p.exists().then_some(p)
        })
        .unwrap_or_else(|| PathBuf::from("agentvc"))
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

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    all: Option<String>,
}

async fn list(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> ApiResult<AgentList> {
    let all = q.all.is_some_and(|a| a != "0" && a != "false");
    let dir = st.workdir().to_path_buf();
    let mut l = blocking(move || agent::list(&dir, all)).await?;
    l.can_steer = local_or_allowed(&headers).is_ok();
    Ok(Json(l))
}

#[derive(Deserialize)]
struct StartBody {
    workspace: Option<String>,
    from: Option<String>,
    at: Option<String>,
    prompt: String,
    #[serde(default)]
    mode: Option<Mode>,
    model: Option<String>,
    check: Option<String>,
    command: Option<String>,
    runner: Option<String>,
    permissions: Option<String>,
    auto_land: Option<bool>,
}

#[derive(Serialize)]
struct AgentResponse {
    agent: Agent,
}

async fn start(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<StartBody>, JsonRejection>,
) -> ApiResult<AgentResponse> {
    local_or_allowed(&headers)?;
    let b = json_body(body)?;
    // Even with --allow-remote-agents, a relayed start uses the workspace's
    // runner and check; it never brings shell commands of its own.
    if relayed(&headers) && (b.command.is_some() || b.check.is_some()) {
        return Err(remote_shell_off());
    }
    if relayed(&headers) && b.permissions.as_deref() == Some("bypass") {
        return Err(remote_bypass_off());
    }
    let dir = st.workdir().to_path_buf();
    let req = StartRequest {
        workspace: b.workspace,
        from: b.from,
        at: b.at,
        prompt: b.prompt,
        mode: b.mode.unwrap_or(Mode::New),
        model: b.model,
        check: b.check,
        command: b.command,
        runner: b.runner,
        permissions: b.permissions,
        auto_land: b.auto_land,
        exe: agentvc_exe(),
    };
    let a = blocking(move || agent::start(&dir, &req)).await?;
    st.poke_agents();
    Ok(Json(AgentResponse { agent: a }))
}

async fn show(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult<AgentDetail> {
    let dir = st.workdir().to_path_buf();
    Ok(Json(blocking(move || agent::show(&dir, &id)).await?))
}

#[derive(Deserialize)]
struct ReplyBody {
    text: String,
}

async fn reply(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<ReplyBody>, JsonRejection>,
) -> ApiResult<AgentResponse> {
    local_or_allowed(&headers)?;
    let b = json_body(body)?;
    let dir = st.workdir().to_path_buf();
    let a = blocking(move || agent::reply(&dir, &id, &b.text, &agentvc_exe())).await?;
    st.poke_agents();
    Ok(Json(AgentResponse { agent: a }))
}

async fn approve(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<AgentResponse> {
    local_or_allowed(&headers)?;
    let dir = st.workdir().to_path_buf();
    let a = blocking(move || agent::answer(&dir, &id, None, Some(true))).await?;
    st.poke_agents();
    Ok(Json(AgentResponse { agent: a }))
}

#[derive(Deserialize, Default)]
struct DenyBody {
    reason: Option<String>,
}

async fn deny(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<Json<DenyBody>>,
) -> ApiResult<AgentResponse> {
    local_or_allowed(&headers)?;
    let reason = body.map(|Json(b)| b).unwrap_or_default().reason;
    let dir = st.workdir().to_path_buf();
    let a = blocking(move || agent::answer(&dir, &id, reason.as_deref(), Some(false))).await?;
    st.poke_agents();
    Ok(Json(AgentResponse { agent: a }))
}

async fn stop(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<AgentResponse> {
    local_or_allowed(&headers)?;
    let dir = st.workdir().to_path_buf();
    let a = blocking(move || agent::stop(&dir, &id)).await?;
    st.poke_agents();
    Ok(Json(AgentResponse { agent: a }))
}

#[derive(Serialize)]
struct LandResponse {
    agent: Agent,
    edit: EditOutcome,
}

async fn land(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<LandResponse> {
    local_or_allowed(&headers)?;
    let dir = st.workdir().to_path_buf();
    let (a, edit) = blocking(move || agent::land(&dir, &id)).await?;
    st.poke_agents();
    st.poke_workspaces();
    Ok(Json(LandResponse { agent: a, edit }))
}

async fn discard(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<AgentResponse> {
    local_or_allowed(&headers)?;
    let dir = st.workdir().to_path_buf();
    let a = blocking(move || agent::discard(&dir, &id)).await?;
    st.poke_agents();
    Ok(Json(AgentResponse { agent: a }))
}

/// Each agent's state and last update, to spot changes.
fn fingerprints(dir: &std::path::Path) -> BTreeMap<String, (AgentState, i64)> {
    agent::list(dir, true)
        .map(|l| {
            l.agents
                .into_iter()
                .map(|a| (a.id, (a.state, a.updated_at)))
                .collect()
        })
        .unwrap_or_default()
}

/// Turn changes in any agent's record into `agent_changed` events.
pub(crate) fn spawn_poller(state: AppState, dir: PathBuf, every: Duration) {
    tokio::spawn(async move {
        let d = dir.clone();
        let Ok(mut last) = tokio::task::spawn_blocking(move || fingerprints(&d)).await else {
            return;
        };
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = state.agents_poked() => {}
            }
            let d = dir.clone();
            let Ok(now) = tokio::task::spawn_blocking(move || fingerprints(&d)).await else {
                break;
            };
            for (id, (s, at)) in &now {
                if last.get(id) != Some(&(*s, *at)) {
                    state.send(Event::AgentChanged {
                        agent: id.clone(),
                        state: *s,
                    });
                }
            }
            last = now;
        }
    });
}
