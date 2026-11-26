//! Review endpoints: per-attempt stats, the inbox, side-by-side compare, and
//! the write actions a reviewer takes (promote, abandon, mark).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use agentvc_core::objects::Oid;
use agentvc_core::{Attempt, Error, Ref, Snapshot, Space, Status, Task};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{lookup, parse_attempt_id, ApiError, ApiResult, StatusCounts, MAX_FILE_BYTES};
use crate::AppState;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/inbox", get(inbox))
        .route("/tasks/{id}/stats", get(task_stats))
        .route("/compare", get(compare))
        .route("/attempts/{id}/promote", post(promote))
        .route("/attempts/{id}/abandon", post(abandon))
        .route("/attempts/{id}/mark", post(mark))
        .merge(crate::live::routes())
}

// ---- stats ----------------------------------------------------------------

/// Outcome of the latest `agentvc run` with a check command.
#[derive(Clone, Debug, Serialize)]
pub struct CheckResult {
    passed: bool,
    exit_code: Option<i64>,
    snapshot: i64,
}

/// Numbers a reviewer weighs, gathered from snapshot metadata.
#[derive(Clone, Debug, Default, Serialize)]
pub struct AttemptStats {
    attempt: i64,
    snapshots: usize,
    /// Latest check result, if any run had a check.
    check: Option<CheckResult>,
    /// Exit code of the latest run, if any.
    last_exit_code: Option<i64>,
    /// Sum of `cost_usd` over snapshots that report it.
    cost_usd: Option<f64>,
    /// Sum of `tokens` over snapshots that report it.
    tokens: Option<i64>,
    /// Seconds from the attempt's creation to its latest snapshot.
    duration_s: i64,
    /// Sum of run durations, when the attempt was driven by `agentvc run`.
    run_ms: Option<i64>,
    /// The agent's latest self-reported summary.
    summary: Option<String>,
    last_activity: i64,
}

/// A number, or a string holding one (`agentvc snapshot --meta k=v` stores
/// strings).
fn as_f64(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_start_matches('$').parse().ok(),
        _ => None,
    }
}

fn as_i64(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn attempt_stats(a: &Attempt, snaps: &[Snapshot]) -> AttemptStats {
    let mut st = AttemptStats {
        attempt: a.id,
        snapshots: snaps.len(),
        last_activity: a.created_at,
        ..Default::default()
    };
    for s in snaps {
        st.last_activity = st.last_activity.max(s.created_at);
        let Some(m) = s.meta.as_ref() else { continue };
        if let Some(c) = as_f64(m.get("cost_usd").or_else(|| m.get("cost"))) {
            *st.cost_usd.get_or_insert(0.0) += c;
        }
        if let Some(t) = as_i64(m.get("tokens")) {
            *st.tokens.get_or_insert(0) += t;
        }
        if let Some(summary) = m.get("summary").and_then(Value::as_str) {
            st.summary = Some(summary.to_string());
        }
        if s.trigger == "run-exit" {
            if let Some(ms) = as_i64(m.get("duration_ms")) {
                *st.run_ms.get_or_insert(0) += ms;
            }
            st.last_exit_code = as_i64(m.get("exit_code"));
            if let Some(code) = as_i64(m.get("check_exit_code")) {
                st.check = Some(CheckResult {
                    passed: code == 0,
                    exit_code: Some(code),
                    snapshot: s.id,
                });
            }
        }
    }
    st.duration_s = (st.last_activity - a.created_at).max(0);
    st
}

fn stats_for(s: &Space, attempts: &[Attempt]) -> agentvc_core::Result<Vec<AttemptStats>> {
    attempts
        .iter()
        .map(|a| Ok(attempt_stats(a, &s.snapshots(a.id)?)))
        .collect()
}

fn parse_task_id(s: &str) -> Result<i64, ApiError> {
    s.strip_prefix('t').unwrap_or(s).parse().map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_task",
            format!("invalid task id {s:?}"),
        )
    })
}

async fn task_stats(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Vec<AttemptStats>> {
    let id = parse_task_id(&id)?;
    let out = st
        .with_space(move |s| {
            s.task(id)?;
            stats_for(s, &s.attempts(Some(id))?)
        })
        .await?;
    Ok(Json(out))
}

// ---- inbox ----------------------------------------------------------------

#[derive(Serialize)]
struct InboxItem {
    task: Task,
    attempts: StatusCounts,
    /// Why the task waits on a human: `ready_to_pick`, `failed`.
    reasons: Vec<&'static str>,
    /// Best check result across open attempts: `passed`, `failed` or null.
    best_check: Option<&'static str>,
    /// Attempts that passed, best candidates first.
    candidates: Vec<i64>,
    cost_usd: Option<f64>,
    updated_at: i64,
    /// The promoted attempt's PR feedback, for `checks_failed` and
    /// `changes_requested`.
    pr: Option<agentvc_core::feedback::PrFeedback>,
}

/// Tasks waiting on a human: two or more passed attempts and no winner yet
/// (ready to pick), failed attempts and no winner (needs a decision), or a
/// winner whose open PR has failing checks or a change request.
async fn inbox(State(st): State<AppState>) -> ApiResult<Vec<InboxItem>> {
    let out = st
        .with_space(|s| {
            let mut by_task: BTreeMap<i64, Vec<Attempt>> = BTreeMap::new();
            for a in s.attempts(None)? {
                if let Some(t) = a.task_id {
                    by_task.entry(t).or_default().push(a);
                }
            }
            let mut items = Vec::new();
            for task in s.tasks()? {
                let attempts = by_task.remove(&task.id).unwrap_or_default();
                let mut counts = StatusCounts::default();
                attempts.iter().for_each(|a| counts.add(a));
                let mut reasons = Vec::new();
                if counts.promoted > 0 {
                    // Picked and shipped: back in the inbox only when its
                    // open PR needs attention (#76).
                    let mut feedback = None;
                    for a in attempts.iter().filter(|a| a.status == Status::Promoted) {
                        if let Some(f) = s.pr_feedback(a.id)?.filter(|f| f.is_open()) {
                            feedback = Some((a.id, f));
                        }
                    }
                    let Some((winner, f)) = feedback else {
                        continue;
                    };
                    if f.checks.state == "fail" {
                        reasons.push("checks_failed");
                    }
                    if f.decision == "changes_requested" {
                        reasons.push("changes_requested");
                    }
                    if reasons.is_empty() {
                        continue;
                    }
                    let updated_at = f.refreshed_at.max(task.created_at);
                    items.push(InboxItem {
                        task,
                        attempts: counts,
                        reasons,
                        best_check: None,
                        candidates: vec![winner],
                        cost_usd: None,
                        updated_at,
                        pr: Some(f),
                    });
                    continue;
                }
                if counts.passed >= 2 {
                    reasons.push("ready_to_pick");
                }
                if counts.failed > 0 {
                    reasons.push("failed");
                }
                if counts.passed == 1 && counts.exploring == 0 {
                    reasons.push("ready_to_pick");
                }
                if reasons.is_empty() {
                    continue;
                }
                let stats = stats_for(s, &attempts)?;
                let open: Vec<&AttemptStats> = stats
                    .iter()
                    .zip(&attempts)
                    .filter(|(_, a)| a.status.is_open())
                    .map(|(st, _)| st)
                    .collect();
                let best_check = if open
                    .iter()
                    .any(|st| st.check.as_ref().is_some_and(|c| c.passed))
                {
                    Some("passed")
                } else if open.iter().any(|st| st.check.is_some()) {
                    Some("failed")
                } else {
                    None
                };
                let cost = stats
                    .iter()
                    .filter_map(|st| st.cost_usd)
                    .reduce(|x, y| x + y);
                let updated_at = stats
                    .iter()
                    .map(|st| st.last_activity)
                    .max()
                    .unwrap_or(task.created_at)
                    .max(task.created_at);
                let candidates = attempts
                    .iter()
                    .filter(|a| a.status == Status::Passed)
                    .map(|a| a.id)
                    .collect();
                items.push(InboxItem {
                    task,
                    attempts: counts,
                    reasons,
                    best_check,
                    candidates,
                    cost_usd: cost,
                    updated_at,
                    pr: None,
                });
            }
            // Ready-to-pick first, then most recently active.
            items.sort_by(|a, b| {
                let ready = |i: &InboxItem| i.reasons.contains(&"ready_to_pick");
                ready(b)
                    .cmp(&ready(a))
                    .then(b.updated_at.cmp(&a.updated_at))
            });
            Ok(items)
        })
        .await?;
    Ok(Json(out))
}

// ---- compare --------------------------------------------------------------

#[derive(Deserialize)]
struct CompareQuery {
    /// Comma separated attempt ids, e.g. `a2,a3`.
    attempts: String,
}

#[derive(Serialize)]
struct CompareColumn {
    attempt: Attempt,
    head: Snapshot,
    stats: AttemptStats,
    files_changed: usize,
    lines_added: usize,
    lines_removed: usize,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum CellKind {
    Unchanged,
    Added,
    Modified,
    Removed,
}

#[derive(Serialize)]
struct CompareCell {
    kind: CellKind,
    /// Blob id in this attempt, or null when the file is absent.
    oid: Option<String>,
    added: usize,
    removed: usize,
    /// True when either side is binary or too large to diff.
    binary: bool,
}

#[derive(Serialize)]
struct CompareFile {
    path: String,
    /// True when every attempt has the same content at this path.
    same: bool,
    /// One cell per column, in column order.
    cells: Vec<CompareCell>,
}

#[derive(Serialize)]
struct CompareResponse {
    /// The newest snapshot every compared attempt descends from.
    base: Snapshot,
    columns: Vec<CompareColumn>,
    /// Union of changed paths, differing files first, then by path.
    files: Vec<CompareFile>,
}

/// Snapshot ids an attempt's latest state descends from (including its own).
fn lineage(s: &Space, attempt: &Attempt) -> agentvc_core::Result<BTreeSet<i64>> {
    let mut out: BTreeSet<i64> = s.snapshots(attempt.id)?.iter().map(|x| x.id).collect();
    let mut next = attempt.parent_snapshot;
    let mut guard = 0;
    while let Some(sid) = next {
        guard += 1;
        if guard > 10_000 {
            break;
        }
        let snap = s.snapshot_by_id(sid)?;
        let owner = s.attempt(snap.attempt_id)?;
        out.extend(
            s.snapshots(owner.id)?
                .iter()
                .map(|x| x.id)
                .filter(|id| *id <= sid),
        );
        next = owner.parent_snapshot;
    }
    Ok(out)
}

fn read_text(s: &Space, oid: &Oid) -> agentvc_core::Result<Option<String>> {
    let data = s.objects().read_blob(oid)?;
    if data.len() > MAX_FILE_BYTES {
        return Ok(None);
    }
    Ok(String::from_utf8(data).ok())
}

/// Lines added and removed between two texts, by patience diff: lines unique
/// on both sides anchor the alignment and the gaps between anchors recurse.
/// Same algorithm as the UI's aligned view (`ui/src/lib/align.ts`), so the
/// counts in column headers match the rendered diff. Near-linear on real
/// files, where Myers is quadratic in the number of changes.
fn line_counts(old: &str, new: &str) -> (usize, usize) {
    let mut ids: HashMap<&str, u32> = HashMap::new();
    let mut id = |l| {
        let n = ids.len() as u32;
        *ids.entry(l).or_insert(n)
    };
    let a: Vec<u32> = old.lines().map(&mut id).collect();
    let b: Vec<u32> = new.lines().map(&mut id).collect();
    let mut counts = (0, 0);
    diff_range(&a, &b, &mut counts);
    counts
}

/// Regions without unique lines smaller than this (LCS table cells) get an
/// exact LCS; larger ones count as a full replace.
const LCS_CELLS: usize = 250_000;

fn diff_range(mut a: &[u32], mut b: &[u32], counts: &mut (usize, usize)) {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    (a, b) = (&a[pre..], &b[pre..]);
    let suf = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    (a, b) = (&a[..a.len() - suf], &b[..b.len() - suf]);
    if a.is_empty() || b.is_empty() {
        counts.0 += b.len();
        counts.1 += a.len();
        return;
    }
    let anchors = unique_anchors(a, b);
    if anchors.is_empty() {
        let common = if a.len() * b.len() <= LCS_CELLS {
            lcs_len(a, b)
        } else {
            0
        };
        counts.0 += b.len() - common;
        counts.1 += a.len() - common;
        return;
    }
    let (mut pa, mut pb) = (0, 0);
    for (ia, ib) in anchors {
        diff_range(&a[pa..ia], &b[pb..ib], counts);
        (pa, pb) = (ia + 1, ib + 1);
    }
    diff_range(&a[pa..], &b[pb..], counts);
}

/// Positions of lines occurring once on each side, as the longest run both
/// sides agree on the order of.
fn unique_anchors(a: &[u32], b: &[u32]) -> Vec<(usize, usize)> {
    // Position, or usize::MAX when the line repeats.
    let mut ca: HashMap<u32, usize> = HashMap::new();
    for (i, x) in a.iter().enumerate() {
        ca.entry(*x).and_modify(|p| *p = usize::MAX).or_insert(i);
    }
    let mut cb: HashMap<u32, usize> = HashMap::new();
    for (j, y) in b.iter().enumerate() {
        if ca.contains_key(y) {
            cb.entry(*y).and_modify(|p| *p = usize::MAX).or_insert(j);
        }
    }
    let pairs: Vec<(usize, usize)> = a
        .iter()
        .enumerate()
        .filter_map(|(i, x)| match (ca.get(x), cb.get(x)) {
            (Some(&pa), Some(&pb)) if pa == i && pb != usize::MAX => Some((i, pb)),
            _ => None,
        })
        .collect();
    // Longest increasing subsequence on b positions (patience sorting).
    let mut tails: Vec<usize> = Vec::new();
    let mut prev = vec![usize::MAX; pairs.len()];
    for (k, &(_, v)) in pairs.iter().enumerate() {
        let lo = tails.partition_point(|&t| pairs[t].1 < v);
        if lo > 0 {
            prev[k] = tails[lo - 1];
        }
        if lo == tails.len() {
            tails.push(k);
        } else {
            tails[lo] = k;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut k = tails.last().copied().unwrap_or(usize::MAX);
    while k != usize::MAX {
        out.push(pairs[k]);
        k = prev[k];
    }
    out.reverse();
    out
}

fn lcs_len(a: &[u32], b: &[u32]) -> usize {
    let mut row = vec![0usize; b.len() + 1];
    for x in a {
        let mut diag = 0;
        for (j, y) in b.iter().enumerate() {
            let up = row[j + 1];
            row[j + 1] = if x == y { diag + 1 } else { up.max(row[j]) };
            diag = up;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::line_counts;

    #[test]
    fn counts_lines() {
        assert_eq!(line_counts("a\nb\nc\n", "a\nB\nc\nd\n"), (2, 1));
        assert_eq!(line_counts("", "x\ny\n"), (2, 0));
        assert_eq!(line_counts("x\n", ""), (0, 1));
        // Repeated lines (no unique anchors) fall back to an exact LCS.
        assert_eq!(line_counts("}\n}\n}\n", "}\n{\n}\n"), (1, 1));
        // Large regenerated file: half the lines change, counted exactly.
        let old: String = (0..5000).map(|i| format!("row {i}\n")).collect();
        let new: String = (0..5000)
            .map(|i| {
                if i % 2 == 0 {
                    format!("row {i} new\n")
                } else {
                    format!("row {i}\n")
                }
            })
            .collect();
        assert_eq!(line_counts(&old, &new), (2500, 2500));
    }
}

fn build_compare(s: &Space, ids: &[i64]) -> agentvc_core::Result<CompareResponse> {
    let attempts: Vec<Attempt> = ids
        .iter()
        .map(|id| s.attempt(*id))
        .collect::<Result<_, _>>()?;
    let mut common: Option<BTreeSet<i64>> = None;
    for a in &attempts {
        let l = lineage(s, a)?;
        common = Some(match common {
            None => l,
            Some(c) => c.intersection(&l).copied().collect(),
        });
    }
    let base_id = common
        .and_then(|c| c.last().copied())
        .ok_or_else(|| Error::NotFound {
            kind: "common base of",
            id: ids
                .iter()
                .map(|i| format!("a{i}"))
                .collect::<Vec<_>>()
                .join(","),
        })?;
    let base = s.snapshot_by_id(base_id)?;
    let base_tree = base.tree_oid()?;

    let heads: Vec<Snapshot> = attempts
        .iter()
        .map(|a| s.resolve(Ref::Attempt(a.id)))
        .collect::<Result<_, _>>()?;
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for h in &heads {
        for c in s.diff(Ref::Snapshot(base.id), Ref::Snapshot(h.id))? {
            paths.insert(c.path);
        }
    }

    let head_trees: Vec<Oid> = heads
        .iter()
        .map(|h| h.tree_oid())
        .collect::<Result<_, _>>()?;
    let mut text_cache: HashMap<Oid, Option<String>> = HashMap::new();
    let mut text = |oid: &Oid| -> agentvc_core::Result<Option<String>> {
        if let Some(t) = text_cache.get(oid) {
            return Ok(t.clone());
        }
        let t = read_text(s, oid)?;
        text_cache.insert(*oid, t.clone());
        Ok(t)
    };

    let mut files = Vec::with_capacity(paths.len());
    let mut totals = vec![(0usize, 0usize, 0usize); heads.len()];
    for path in paths {
        let base_entry = lookup(s, &base_tree, &path)?.map(|(_, o)| o);
        let mut cells = Vec::with_capacity(heads.len());
        for (i, tree) in head_trees.iter().enumerate() {
            let entry = lookup(s, tree, &path)?.map(|(_, o)| o);
            let kind = match (&base_entry, &entry) {
                (None, None) => CellKind::Unchanged,
                (None, Some(_)) => CellKind::Added,
                (Some(_), None) => CellKind::Removed,
                (Some(a), Some(b)) if a == b => CellKind::Unchanged,
                (Some(_), Some(_)) => CellKind::Modified,
            };
            let (mut added, mut removed, mut binary) = (0, 0, false);
            if kind != CellKind::Unchanged {
                let old = match &base_entry {
                    Some(o) => text(o)?,
                    None => Some(String::new()),
                };
                let new = match &entry {
                    Some(o) => text(o)?,
                    None => Some(String::new()),
                };
                match (old, new) {
                    (Some(o), Some(n)) => (added, removed) = line_counts(&o, &n),
                    _ => binary = true,
                }
                totals[i].0 += 1;
                totals[i].1 += added;
                totals[i].2 += removed;
            }
            cells.push(CompareCell {
                kind,
                oid: entry.map(|o| o.to_hex()),
                added,
                removed,
                binary,
            });
        }
        let same = cells.windows(2).all(|w| w[0].oid == w[1].oid);
        files.push(CompareFile { path, same, cells });
    }
    files.sort_by(|a, b| a.same.cmp(&b.same).then_with(|| a.path.cmp(&b.path)));

    let columns = attempts
        .into_iter()
        .zip(heads)
        .zip(totals)
        .map(
            |((attempt, head), (files_changed, lines_added, lines_removed))| {
                let stats = attempt_stats(&attempt, &s.snapshots(attempt.id)?);
                Ok(CompareColumn {
                    attempt,
                    head,
                    stats,
                    files_changed,
                    lines_added,
                    lines_removed,
                })
            },
        )
        .collect::<agentvc_core::Result<_>>()?;
    Ok(CompareResponse {
        base,
        columns,
        files,
    })
}

async fn compare(
    State(st): State<AppState>,
    Query(q): Query<CompareQuery>,
) -> ApiResult<CompareResponse> {
    let ids: Vec<i64> = q
        .attempts
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(parse_attempt_id)
        .collect::<Result<_, _>>()?;
    let mut uniq = ids.clone();
    uniq.sort_unstable();
    uniq.dedup();
    if !(2..=4).contains(&ids.len()) || uniq.len() != ids.len() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_compare",
            "compare takes 2 to 4 distinct attempts".into(),
        ));
    }
    Ok(Json(st.with_space(move |s| build_compare(s, &ids)).await?))
}

// ---- actions --------------------------------------------------------------

/// Unwrap a JSON body, turning a bad one into our error shape.
pub(crate) fn json_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(b)| b).map_err(|e| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_body",
            format!("expected a JSON body: {}", e.body_text()),
        )
    })
}

#[derive(Serialize)]
struct ActionResponse {
    attempt: Attempt,
    /// Siblings closed as `lost to aN` by a promote.
    closed_siblings: Vec<i64>,
}

async fn promote(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult<ActionResponse> {
    let id = parse_attempt_id(&id)?;
    let actor = st.actor().to_string();
    let out = st
        .with_space(move |s| {
            let attempt = s.set_status_as(&actor, id, Status::Promoted, None)?;
            let lost = format!("lost to a{id}");
            let closed_siblings = match attempt.task_id {
                Some(t) => s
                    .attempts(Some(t))?
                    .into_iter()
                    .filter(|x| x.reason.as_deref() == Some(lost.as_str()))
                    .map(|x| x.id)
                    .collect(),
                None => Vec::new(),
            };
            Ok(ActionResponse {
                attempt,
                closed_siblings,
            })
        })
        .await?;
    st.poke();
    Ok(Json(out))
}

#[derive(Deserialize)]
struct AbandonBody {
    reason: String,
}

async fn abandon(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<AbandonBody>, JsonRejection>,
) -> ApiResult<ActionResponse> {
    let body = json_body(body)?;
    let id = parse_attempt_id(&id)?;
    let actor = st.actor().to_string();
    let reason = body.reason.trim().to_string();
    if reason.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "reason_required",
            "abandoning an attempt needs a reason".into(),
        ));
    }
    let attempt = st
        .with_space(move |s| s.set_status_as(&actor, id, Status::Abandoned, Some(&reason)))
        .await?;
    st.poke();
    Ok(Json(ActionResponse {
        attempt,
        closed_siblings: Vec::new(),
    }))
}

#[derive(Deserialize)]
struct MarkBody {
    status: String,
}

async fn mark(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<MarkBody>, JsonRejection>,
) -> ApiResult<ActionResponse> {
    let body = json_body(body)?;
    let id = parse_attempt_id(&id)?;
    let actor = st.actor().to_string();
    let status = match Status::parse(&body.status) {
        Some(s @ (Status::Passed | Status::Failed | Status::Exploring)) => s,
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "bad_status",
                format!(
                    "mark takes passed, failed or exploring, not {:?}",
                    body.status
                ),
            ))
        }
    };
    let attempt = st
        .with_space(move |s| s.set_status_as(&actor, id, status, None))
        .await?;
    st.poke();
    Ok(Json(ActionResponse {
        attempt,
        closed_siblings: Vec::new(),
    }))
}
