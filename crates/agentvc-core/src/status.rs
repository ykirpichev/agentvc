//! `agentvc status`: where am I and what is going on.
//!
//! For one attempt: its task, status, head snapshot, the files changed in
//! its view since that snapshot, its active runs, its PR, and how far the
//! commit it is based on is behind the remote's default branch (so `rebase`
//! is needed). For the space: open tasks with their attempts, what needs
//! review, and what is running. Nothing is fetched or snapshotted.

use serde::Serialize;

use crate::error::Result;
use crate::export::HostRepo;
use crate::run::Run;
use crate::snapshot::{self, FileChange};
use crate::space::{Attempt, Snapshot, Space, Status, Task};

#[derive(Clone, Debug, Serialize)]
pub struct Upstream {
    /// The commit the attempt's head snapshot is based on: the commit it was
    /// last rebased onto, else the host HEAD at `agentvc init`.
    pub base: String,
    /// Remote-tracking branch compared with, like `origin/main`.
    pub upstream: String,
    /// Commits on `upstream` that `base` lacks. When non-zero, run
    /// `agentvc rebase aN` before `pr`.
    pub behind: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AttemptStatus {
    pub attempt: Attempt,
    pub task: Option<Task>,
    pub head: Snapshot,
    /// Files changed in the view since `head`; `None` when the attempt is
    /// closed or its view is gone.
    pub changes: Option<Vec<FileChange>>,
    pub runs: Vec<Run>,
    pub pr_url: Option<String>,
    /// The local branch `agentvc land` put it on, and the commit.
    pub landed: Option<crate::land::Landing>,
    /// What GitHub said about its PR (or its parent's) at the last
    /// `agentvc pr --refresh`: state, checks, reviews, comment threads.
    pub pr: Option<crate::feedback::PrFeedback>,
    /// `None` outside a git repo, or without a remote-tracking branch.
    pub upstream: Option<Upstream>,
    /// Files still conflicted after a rebase, take or merge.
    pub conflicts: Vec<String>,
    /// Other open attempts (of other tasks) that change the same files.
    pub overlaps: Vec<crate::overlaps::Overlap>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AttemptSummary {
    pub id: i64,
    pub status: Status,
    pub reason: Option<String>,
    pub actor: String,
    pub model: Option<String>,
    pub parent_attempt: Option<i64>,
    /// Passed and not promoted: waiting for a human to compare and pick.
    pub needs_review: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskStatus {
    pub task: Task,
    pub attempts: Vec<AttemptSummary>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SpaceStatus {
    pub workdir: std::path::PathBuf,
    /// Tasks with no promoted attempt yet.
    pub open_tasks: Vec<TaskStatus>,
    /// Attempt ids that passed and are not promoted, across all tasks.
    pub needs_review: Vec<i64>,
    /// Runs that have not finished.
    pub runs: Vec<Run>,
    /// Number of tasks with a promoted attempt.
    pub done_tasks: usize,
    /// PRs not known to be merged or closed (merged ones drop out after
    /// `agentvc pr --refresh`).
    pub open_prs: Vec<OpenPr>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OpenPr {
    pub url: String,
    /// Attempts exported to it, newest first.
    pub attempts: Vec<i64>,
    /// From the last refresh; `None` before the first one.
    pub feedback: Option<crate::feedback::PrFeedback>,
}

/// The remote-tracking branch to compare with: `<remote>/HEAD`'s target,
/// else `<remote>/main`, else `<remote>/master`.
fn upstream_ref(repo: &HostRepo, remote: &str) -> Option<String> {
    if let Ok(r) = repo.git(
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            &format!("refs/remotes/{remote}/HEAD"),
        ],
        None,
    ) {
        if !r.is_empty() {
            return Some(r);
        }
    }
    ["main", "master"]
        .iter()
        .map(|b| format!("{remote}/{b}"))
        .find(|r| repo.rev(&format!("refs/remotes/{r}")).is_some())
}

fn upstream(space: &Space, head: &Snapshot) -> Result<Option<Upstream>> {
    let Ok(repo) = HostRepo::open(space.workdir()) else {
        return Ok(None);
    };
    let Some(base) = space.git_base_commit(head)? else {
        return Ok(None);
    };
    let Some(up) = upstream_ref(&repo, "origin") else {
        return Ok(None);
    };
    let Ok(n) = repo.git(&["rev-list", "--count", &format!("{base}..{up}")], None) else {
        return Ok(None);
    };
    Ok(Some(Upstream {
        base,
        upstream: up,
        behind: n.parse().unwrap_or(0),
    }))
}

impl Space {
    /// Status of one attempt, see [`AttemptStatus`].
    pub fn attempt_status(&self, id: i64) -> Result<AttemptStatus> {
        let attempt = self.attempt(id)?;
        let task = attempt.task_id.map(|t| self.task(t)).transpose()?;
        let head = self.head_snapshot(id)?;
        let changes = if attempt.status.is_open() && self.ensure_view(&attempt).unwrap_or(false) {
            let now = self.capture_view(&attempt)?;
            Some(snapshot::diff(
                self.objects(),
                &crate::objects::Oid::from_hex(&head.tree)?,
                &now,
            )?)
        } else {
            None
        };
        let runs = self
            .runs(true)?
            .into_iter()
            .filter(|r| r.attempt_id == id)
            .collect();
        Ok(AttemptStatus {
            overlaps: if attempt.status.is_open() && id != crate::space::ROOT_ATTEMPT {
                self.overlaps(Some(id))?
            } else {
                Vec::new()
            },
            conflicts: self
                .conflicts(id, false)?
                .into_iter()
                .map(|c| c.path)
                .collect(),
            pr_url: self.pr_url(id)?,
            landed: self.landing(id)?,
            pr: self.pr_feedback(id)?,
            upstream: upstream(self, &head)?,
            attempt,
            task,
            head,
            changes,
            runs,
        })
    }

    /// Status of the whole space, see [`SpaceStatus`].
    pub fn space_status(&self) -> Result<SpaceStatus> {
        let mut open_tasks = Vec::new();
        let mut needs_review = Vec::new();
        let mut done_tasks = 0;
        for task in self.tasks()? {
            let attempts = self.attempts(Some(task.id))?;
            if attempts.iter().any(|a| a.status == Status::Promoted) {
                done_tasks += 1;
                continue;
            }
            let attempts: Vec<AttemptSummary> = attempts
                .into_iter()
                .map(|a| AttemptSummary {
                    needs_review: a.status == Status::Passed,
                    id: a.id,
                    status: a.status,
                    reason: a.reason,
                    actor: a.actor,
                    model: a.model,
                    parent_attempt: a.parent_attempt,
                })
                .collect();
            needs_review.extend(attempts.iter().filter(|a| a.needs_review).map(|a| a.id));
            open_tasks.push(TaskStatus { task, attempts });
        }
        Ok(SpaceStatus {
            workdir: self.workdir().to_path_buf(),
            open_tasks,
            needs_review,
            runs: self.runs(true)?,
            done_tasks,
            open_prs: self
                .open_prs()?
                .into_iter()
                .map(|(url, attempts, feedback)| OpenPr {
                    url,
                    attempts,
                    feedback,
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::ForkRequest;
    use std::fs;
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn attempt_and_space_status() {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path().join("w");
        let remote = tmp.path().join("remote.git");
        fs::create_dir_all(&w).unwrap();
        git(
            tmp.path(),
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        git(&w, &["init", "-q", "-b", "main"]);
        fs::write(w.join("a.txt"), "a\n").unwrap();
        git(&w, &["add", "-A"]);
        git(&w, &["commit", "-q", "-m", "base"]);
        git(&w, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git(&w, &["push", "-q", "origin", "main"]);
        git(&w, &["fetch", "-q", "origin"]);
        let space = Space::init(&w).unwrap();

        let t = space.create_task("goal", None).unwrap();
        let fork = || {
            space
                .fork(&ForkRequest {
                    task_id: Some(t.id),
                    actor: "agent".into(),
                    ..Default::default()
                })
                .unwrap()
                .attempt
        };
        let a = fork();
        let b = fork();
        fs::write(a.view_path.join("b.txt"), "b\n").unwrap();

        let st = space.attempt_status(a.id).unwrap();
        assert_eq!(st.task.as_ref().unwrap().id, t.id);
        let changes = st.changes.unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "b.txt");
        let up = st.upstream.unwrap();
        assert_eq!((up.upstream.as_str(), up.behind), ("origin/main", 0));
        // Status takes no snapshot.
        assert!(space.snapshots(a.id).unwrap().is_empty());

        // main moves on the remote: the attempt is behind by one commit.
        fs::write(w.join("c.txt"), "c\n").unwrap();
        git(&w, &["add", "c.txt"]);
        git(&w, &["commit", "-q", "-m", "later"]);
        git(&w, &["push", "-q", "origin", "main"]);
        git(&w, &["fetch", "-q", "origin"]);
        assert_eq!(
            space.attempt_status(a.id).unwrap().upstream.unwrap().behind,
            1
        );

        space.set_status(b.id, Status::Passed, None).unwrap();
        let s = space.space_status().unwrap();
        assert_eq!(s.open_tasks.len(), 1);
        assert_eq!(s.needs_review, vec![b.id]);
        assert_eq!(s.open_tasks[0].attempts.len(), 2);

        space.set_status(b.id, Status::Promoted, None).unwrap();
        let s = space.space_status().unwrap();
        assert!(s.open_tasks.is_empty());
        assert_eq!(s.done_tasks, 1);
    }
}
