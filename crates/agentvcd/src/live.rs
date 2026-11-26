//! The live board: every running attempt with its current step, files
//! touched, spend and run, plus stopping a run from the UI.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use agentvc_core::run::Run;
use agentvc_core::snapshot::FileChange;
use agentvc_core::{Attempt, Snapshot, Space, Status};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;

use crate::api::{parse_attempt_id, ApiError, ApiResult};
use crate::review::{attempt_stats_in, AttemptStats};
use crate::AppState;

/// The workdir attempt; never shown as a running agent.
const ROOT_ATTEMPT: i64 = 1;
/// Files listed per card; the count is always complete.
const MAX_FILES: usize = 12;
/// Lines of run output shown per card.
const LOG_LINES: usize = 12;
const LOG_TAIL_BYTES: u64 = 8 * 1024;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/live", get(live))
        .route("/attempts/{id}/stop", post(stop))
}

#[derive(Serialize)]
struct LiveRun {
    id: i64,
    command: Vec<String>,
    check_command: Option<String>,
    started_at: i64,
    /// The process is still there (it may have died without recording an exit).
    alive: bool,
    /// Last lines of the run's log, if it logs to a file.
    log_tail: Vec<String>,
}

#[derive(Serialize)]
struct LiveAttempt {
    attempt: Attempt,
    task_goal: Option<String>,
    /// Latest snapshot: its trigger is the agent's current step.
    head: Option<Snapshot>,
    stats: AttemptStats,
    files_changed: usize,
    files: Vec<FileChange>,
    /// Paths this attempt changed that another running attempt changed too,
    /// with the ids of those attempts.
    overlaps: BTreeMap<String, Vec<i64>>,
    run: Option<LiveRun>,
}

fn log_tail(path: &Path) -> Vec<String> {
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(LOG_TAIL_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let _ = f.take(LOG_TAIL_BYTES).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // probably cut mid-line
    }
    let from = lines.len().saturating_sub(LOG_LINES);
    lines[from..].iter().map(|l| l.to_string()).collect()
}

fn latest_active_run(runs: &[Run], attempt: i64) -> Option<&Run> {
    runs.iter().rev().find(|r| r.attempt_id == attempt)
}

fn build(s: &Space) -> agentvc_core::Result<Vec<LiveAttempt>> {
    let goals: BTreeMap<i64, String> = s.tasks()?.into_iter().map(|t| (t.id, t.goal)).collect();
    let runs = s.runs(true)?;
    let running: Vec<Attempt> = s
        .attempts(None)?
        .into_iter()
        .filter(|a| a.status == Status::Exploring && a.id != ROOT_ATTEMPT)
        .collect();

    let mut out = Vec::with_capacity(running.len());
    let mut touched: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for a in running {
        let snaps = s.snapshots(a.id)?;
        let head = snaps.last().cloned();
        let files = match (a.parent_snapshot, &head) {
            // own_changes compares a rebased attempt with its new base, so
            // the files main brought in do not count as overlaps.
            (Some(_), Some(_)) => s.own_changes(a.id)?,
            _ => Vec::new(),
        };
        for f in &files {
            touched.entry(f.path.clone()).or_default().push(a.id);
        }
        let run = latest_active_run(&runs, a.id).map(|r| LiveRun {
            id: r.id,
            command: r.command.clone(),
            check_command: r.check_command.clone(),
            started_at: r.started_at,
            alive: r.alive.unwrap_or(false),
            log_tail: r.log_path.as_deref().map(log_tail).unwrap_or_default(),
        });
        out.push(LiveAttempt {
            task_goal: a.task_id.and_then(|t| goals.get(&t).cloned()),
            stats: attempt_stats_in(s, &a, &snaps)?,
            head,
            files_changed: files.len(),
            files,
            overlaps: BTreeMap::new(),
            run,
            attempt: a,
        });
    }
    for la in &mut out {
        for f in &la.files {
            let others: Vec<i64> = touched[&f.path]
                .iter()
                .copied()
                .filter(|id| *id != la.attempt.id)
                .collect();
            if !others.is_empty() {
                la.overlaps.insert(f.path.clone(), others);
            }
        }
        la.files.truncate(MAX_FILES);
    }
    // Newest first. A stable order, so cards don't jump under the
    // reviewer's cursor as agents make progress.
    out.sort_by_key(|la| std::cmp::Reverse(la.attempt.id));
    Ok(out)
}

async fn live(State(st): State<AppState>) -> ApiResult<Vec<LiveAttempt>> {
    Ok(Json(st.with_space(build).await?))
}

#[derive(Serialize)]
struct StopResponse {
    run: i64,
    pid: i64,
    /// The signal sent: `TERM`.
    signal: &'static str,
}

/// Ask the attempt's running command to stop (SIGTERM). The run records its
/// exit and outcome itself, which then streams to every open UI.
async fn stop(State(st): State<AppState>, UrlPath(id): UrlPath<String>) -> ApiResult<StopResponse> {
    let id = parse_attempt_id(&id)?;
    let run = st
        .with_space(move |s| {
            s.attempt(id)?;
            let runs = s.runs(true)?;
            Ok(latest_active_run(&runs, id).cloned())
        })
        .await?;
    let no_run = || {
        ApiError::new(
            StatusCode::CONFLICT,
            "no_active_run",
            format!("a{id} has no running command to stop"),
        )
    };
    let run = run.ok_or_else(no_run)?;
    let pid = match (run.pid, run.alive) {
        (Some(pid), Some(true)) => pid,
        _ => return Err(no_run()),
    };
    terminate(pid).map_err(|e| ApiError::internal(format!("cannot signal pid {pid}: {e}")))?;
    st.poke();
    Ok(Json(StopResponse {
        run: run.id,
        pid,
        signal: "TERM",
    }))
}

#[cfg(unix)]
fn terminate(pid: i64) -> std::io::Result<()> {
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("kill failed"))
    }
}

#[cfg(not(unix))]
fn terminate(_pid: i64) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "stopping runs is only supported on Unix",
    ))
}
