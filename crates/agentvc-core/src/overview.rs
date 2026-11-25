//! A whole-space summary for dashboards outside the daemon: every task, its
//! attempt tree, the status and reason of each attempt, the latest run and
//! any PR. `agentvc overview --out FILE` writes it; the Claude project
//! artifact and the desktop side panel render it (see `docs/overview.md`).
//!
//! The document is self-contained JSON with a stable `schema` tag, so readers
//! can reject versions they do not understand. It is sized to stay well under
//! 256 KiB for spaces with hundreds of attempts: long prompts and criteria are
//! cut, and file lists are reduced to counts.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::export::HostRepo;
use crate::run::Run;
use crate::snapshot::ChangeKind;
use crate::space::{Attempt, Ref, Space, Status};

/// Schema tag of the document written by this build.
pub const SCHEMA: &str = "agentvc.overview/1";

/// Longest prompt kept, in characters.
const PROMPT_CHARS: usize = 600;
/// Longest task criteria kept, in characters.
const CRITERIA_CHARS: usize = 1200;

#[derive(Clone, Debug, Serialize)]
pub struct Overview {
    pub schema: &'static str,
    /// Unix seconds when the document was built.
    pub generated_at: i64,
    pub space: SpaceSummary,
    /// Newest activity first.
    pub tasks: Vec<TaskNode>,
    /// Attempts that belong to no task, other than the root `a1`.
    pub untasked: Vec<AttemptNode>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SpaceSummary {
    /// The working directory's name.
    pub name: String,
    /// `owner/name` of the `origin` remote on GitHub, when there is one.
    pub repo: Option<String>,
    /// Commit the host repository's HEAD points at.
    pub head: Option<String>,
    pub counts: Counts,
}

/// Attempts per status.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Counts {
    pub total: usize,
    pub running: usize,
    pub exploring: usize,
    pub passed: usize,
    pub failed: usize,
    pub abandoned: usize,
    pub promoted: usize,
}

impl Counts {
    fn add(&mut self, a: &AttemptNode) {
        self.total += 1;
        if a.run.as_ref().is_some_and(|r| r.state == RunState::Running) {
            self.running += 1;
        }
        *match a.status {
            Status::Exploring => &mut self.exploring,
            Status::Passed => &mut self.passed,
            Status::Failed => &mut self.failed,
            Status::Abandoned => &mut self.abandoned,
            Status::Promoted => &mut self.promoted,
        } += 1;
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskNode {
    /// `tN`.
    pub id: String,
    pub goal: String,
    pub criteria: Option<String>,
    pub created_at: i64,
    /// Newest snapshot, run or attempt in the task, or its creation time.
    pub updated_at: i64,
    /// `open` while any attempt is exploring or running, `done` once one is
    /// promoted, `idle` otherwise.
    pub state: TaskState,
    /// The promoted attempt, `aN`.
    pub winner: Option<String>,
    pub counts: Counts,
    /// In creation order; `parent` links them into a tree.
    pub attempts: Vec<AttemptNode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Open,
    Done,
    Idle,
}

#[derive(Clone, Debug, Serialize)]
pub struct AttemptNode {
    /// `aN`.
    pub id: String,
    /// Parent attempt `aN`, when it was forked from one.
    pub parent: Option<String>,
    pub actor: String,
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub status: Status,
    /// Why it was abandoned, failed or marked, e.g. `lost to a5`.
    pub reason: Option<String>,
    pub backend: String,
    pub created_at: i64,
    /// Newest snapshot or run event of this attempt.
    pub updated_at: i64,
    pub snapshots: usize,
    /// Files changed since the snapshot it was forked from.
    pub changes: Option<ChangeCounts>,
    /// The latest run in this attempt.
    pub run: Option<RunSummary>,
    /// The latest export that opened a PR.
    pub pr: Option<PrLink>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ChangeCounts {
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunSummary {
    /// `rN`.
    pub id: String,
    pub state: RunState,
    pub command: String,
    pub check: Option<String>,
    pub exit_code: Option<i32>,
    pub check_exit_code: Option<i32>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub duration_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Running,
    /// Never finished and its process is gone.
    Lost,
    Finished,
}

#[derive(Clone, Debug, Serialize)]
pub struct PrLink {
    pub url: String,
    /// Parsed from the URL's `/pull/N`.
    pub number: Option<u64>,
    pub branch: String,
    pub commit: String,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn cut(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_string(),
    }
}

fn pr_number(url: &str) -> Option<u64> {
    let (_, rest) = url.rsplit_once("/pull/")?;
    rest.split(['/', '#', '?']).next()?.parse().ok()
}

fn summarize_run(r: &Run) -> RunSummary {
    let state = match (r.finished_at, r.alive) {
        (Some(_), _) => RunState::Finished,
        (None, Some(false)) => RunState::Lost,
        (None, _) => RunState::Running,
    };
    RunSummary {
        id: format!("r{}", r.id),
        state,
        command: cut(&r.command.join(" "), 200),
        check: r.check_command.clone(),
        exit_code: r.exit_code,
        check_exit_code: r.check_exit_code,
        started_at: r.started_at,
        finished_at: r.finished_at,
        duration_ms: r.duration_ms,
    }
}

impl Space {
    /// Build the overview document for this space.
    pub fn overview(&self) -> Result<Overview> {
        // Latest run per attempt, with liveness for unfinished ones.
        let mut runs: BTreeMap<i64, Run> = BTreeMap::new();
        for mut r in self.runs(false)? {
            if r.finished_at.is_none() {
                r.alive = Some(r.pid.is_some_and(crate::run::pid_alive));
            }
            runs.insert(r.attempt_id, r);
        }

        let mut nodes: BTreeMap<Option<i64>, Vec<AttemptNode>> = BTreeMap::new();
        for a in self.attempts(None)? {
            if a.task_id.is_none() && a.id == crate::space::ROOT_ATTEMPT {
                continue;
            }
            let node = self.attempt_node(&a, runs.get(&a.id))?;
            nodes.entry(a.task_id).or_default().push(node);
        }

        let mut counts = Counts::default();
        let mut tasks = Vec::new();
        for t in self.tasks()? {
            let attempts = nodes.remove(&Some(t.id)).unwrap_or_default();
            let mut c = Counts::default();
            for a in &attempts {
                c.add(a);
                counts.add(a);
            }
            let winner = attempts
                .iter()
                .rev()
                .find(|a| {
                    a.status == Status::Promoted
                        && !a
                            .reason
                            .as_deref()
                            .is_some_and(|r| r.starts_with("continued in"))
                })
                .map(|a| a.id.clone());
            let state = if c.running > 0 || c.exploring > 0 {
                TaskState::Open
            } else if winner.is_some() {
                TaskState::Done
            } else {
                TaskState::Idle
            };
            let updated_at = attempts
                .iter()
                .map(|a| a.updated_at)
                .fold(t.created_at, i64::max);
            tasks.push(TaskNode {
                id: format!("t{}", t.id),
                goal: t.goal,
                criteria: t.criteria.map(|c| cut(&c, CRITERIA_CHARS)),
                created_at: t.created_at,
                updated_at,
                state,
                winner,
                counts: c,
                attempts,
            });
        }
        tasks.sort_by_key(|t| std::cmp::Reverse(t.updated_at));
        let untasked = nodes.remove(&None).unwrap_or_default();
        for a in &untasked {
            counts.add(a);
        }

        let (repo, head) = match HostRepo::open(self.workdir()) {
            Ok(host) => (
                host.git(&["remote", "get-url", "origin"], None)
                    .ok()
                    .and_then(|u| crate::github::parse_remote(&u)),
                host.rev("HEAD"),
            ),
            Err(_) => (None, None),
        };

        Ok(Overview {
            schema: SCHEMA,
            generated_at: now(),
            space: SpaceSummary {
                name: self
                    .workdir()
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                repo,
                head,
                counts,
            },
            tasks,
            untasked,
        })
    }

    fn attempt_node(&self, a: &Attempt, run: Option<&Run>) -> Result<AttemptNode> {
        let snaps = self.snapshots(a.id)?;
        let mut updated_at = snaps
            .iter()
            .map(|s| s.created_at)
            .fold(a.created_at, i64::max);
        let run = run.map(|r| {
            updated_at = updated_at.max(r.finished_at.unwrap_or(r.started_at));
            summarize_run(r)
        });
        let changes = match a.parent_snapshot {
            Some(p) => match self.diff(Ref::Snapshot(p), Ref::Attempt(a.id)) {
                Ok(list) => {
                    let mut c = ChangeCounts::default();
                    for f in list {
                        *match f.kind {
                            ChangeKind::Added => &mut c.added,
                            ChangeKind::Modified => &mut c.modified,
                            ChangeKind::Removed => &mut c.removed,
                        } += 1;
                    }
                    Some(c)
                }
                Err(Error::NotFound { .. }) => None,
                Err(e) => return Err(e),
            },
            None => None,
        };
        let pr = self.exports(a.id)?.into_iter().rev().find_map(|e| {
            let url = e.pr_url?;
            Some(PrLink {
                number: pr_number(&url),
                url,
                branch: e.branch,
                commit: e.commit,
            })
        });
        Ok(AttemptNode {
            id: format!("a{}", a.id),
            parent: a.parent_attempt.map(|p| format!("a{p}")),
            actor: a.actor.clone(),
            model: a.model.clone(),
            prompt: a.prompt.as_deref().map(|p| cut(p, PROMPT_CHARS)),
            status: a.status,
            reason: a.reason.clone(),
            backend: a.backend.clone(),
            created_at: a.created_at,
            updated_at,
            snapshots: snaps.len(),
            changes,
            run,
            pr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::ForkRequest;
    use crate::testutil::tempdir;
    use std::fs;

    #[test]
    fn cut_keeps_short_text_and_marks_long_text() {
        assert_eq!(cut("  hi  ", 5), "hi");
        assert_eq!(cut("abcdef", 3), "abc…");
        assert_eq!(cut("ééééé", 2), "éé…");
    }

    #[test]
    fn parses_pr_numbers() {
        assert_eq!(pr_number("https://github.com/o/r/pull/48"), Some(48));
        assert_eq!(pr_number("https://github.com/o/r/pull/7#x"), Some(7));
        assert_eq!(pr_number("https://example.com/x"), None);
    }

    #[test]
    fn builds_a_task_tree_with_winner_and_reasons() {
        let dir = tempdir();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let s = Space::init(dir.path()).unwrap();
        let t = s.create_task("Add a greeting", Some("prints hi")).unwrap();
        let fork = |from: Option<Ref>| {
            s.fork(&ForkRequest {
                task_id: Some(t.id),
                from,
                actor: "agent:claude".into(),
                model: Some("opus".into()),
                prompt: Some("x".repeat(PROMPT_CHARS + 50)),
                ..Default::default()
            })
            .unwrap()
            .attempt
        };
        let a = fork(Some(Ref::Attempt(1)));
        let b = fork(Some(Ref::Attempt(1)));
        fs::write(a.view_path.join("b.txt"), "two\n").unwrap();
        fs::write(a.view_path.join("a.txt"), "uno\n").unwrap();
        s.snapshot(a.id, "manual", None).unwrap();
        let c = fork(Some(Ref::Attempt(a.id)));
        s.set_status(b.id, Status::Abandoned, Some("too slow"))
            .unwrap();
        // Promoting c promotes a along with it ("continued in"); c wins.
        s.set_status(c.id, Status::Promoted, None).unwrap();
        // A follow-up forked from the winner keeps the task open.
        let d = fork(Some(Ref::Attempt(c.id)));

        let o = s.overview().unwrap();
        assert_eq!(o.schema, SCHEMA);
        assert!(o.untasked.is_empty());
        assert_eq!(o.tasks.len(), 1);
        let task = &o.tasks[0];
        assert_eq!(task.id, format!("t{}", t.id));
        assert_eq!(task.winner.as_deref(), Some(&*format!("a{}", c.id)));
        // d is still exploring, so the task is open.
        assert_eq!(task.state, TaskState::Open);
        assert_eq!(task.counts.total, 4);
        assert_eq!(task.counts.abandoned, 1);
        assert_eq!(task.counts.exploring, 1);
        assert_eq!(o.space.counts.promoted, 2);

        let node = |id: i64| {
            task.attempts
                .iter()
                .find(|n| n.id == format!("a{id}"))
                .unwrap()
        };
        let na = node(a.id);
        assert_eq!(na.parent.as_deref(), Some("a1"));
        let ch = na.changes.as_ref().unwrap();
        assert_eq!((ch.added, ch.modified, ch.removed), (1, 1, 0));
        assert!(na.prompt.as_ref().unwrap().ends_with('…'));
        assert_eq!(node(b.id).reason.as_deref(), Some("too slow"));
        assert_eq!(node(c.id).parent, Some(format!("a{}", a.id)));
        assert_eq!(node(d.id).status, Status::Exploring);

        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json["tasks"][0]["attempts"][0]["status"], "promoted");
    }
}
