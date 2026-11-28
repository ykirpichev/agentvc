//! Exporting an attempt to the host git repository.
//!
//! Snapshots are stored as git loose objects, so export copies the objects of
//! the attempt's head tree into the host repo's object directory, writes one
//! commit on top of a base, and points a branch at it. The user's working tree,
//! index and checked-out branch are never touched: everything goes through
//! `git commit-tree` and `git update-ref`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::error::{Error, IoContext, Result};
use crate::objects::{Mode, Oid};
use crate::space::{now, Attempt, Snapshot, Space};

/// Space metadata key holding the host repo's HEAD at `agentvc init`.
pub const IMPORT_BASE_KEY: &str = "git_import_head";

/// Trailer that ties an exported commit back to its attempt.
pub const ATTEMPT_TRAILER: &str = "Agentvc-Attempt";

const ZERO_OID: &str = "0000000000000000000000000000000000000000";

#[derive(Clone, Debug, Default)]
pub struct ExportOptions {
    /// Branch to create, without `refs/heads/`.
    pub branch: String,
    /// Commit to parent on. Defaults to the host HEAD recorded at init, then
    /// the current HEAD; a repo with no commits gets a root commit.
    pub base: Option<String>,
    /// Commit message. Defaults to the task goal.
    pub message: Option<String>,
    /// Move the branch if it already exists. Not needed when the new commit
    /// fast-forwards the branch.
    pub force: bool,
    /// Extra `Key: value` trailers for the commit message, after agentvc's.
    pub trailers: Vec<String>,
    /// Run the repository's `pre-commit` and `commit-msg` hooks in the
    /// attempt's view first (see [`crate::land::verify`]). `None` follows
    /// `agentvc config export.verify` (default off).
    pub verify: Option<bool>,
    /// Export one commit even when the attempt has a story (#21).
    pub squash: bool,
    /// With a story, also point a branch `<branch>-<k>` at each change but
    /// the last, for stacked review.
    pub stack: bool,
    /// A merged PR from this branch, as GitHub reports it (`agentvc pr`
    /// passes the one it last opened from the branch). See [`Continued`].
    pub merged_pr: Option<MergedPr>,
    /// The branch's head on the remote, fetched just before (`agentvc pr`
    /// passes it). When it differs from the local branch (a bot restacked
    /// the PR, another checkout pushed), the export continues the remote
    /// head instead of the local one, so the push fast-forwards (#230).
    pub remote_head: Option<String>,
}

/// A pull request that GitHub reports as merged.
#[derive(Clone, Debug, Default)]
pub struct MergedPr {
    pub number: u64,
    /// The PR's head commit when it merged.
    pub head: String,
    /// The commit the merge wrote on the base branch (for a squash merge,
    /// the squashed commit).
    pub merge_commit: Option<String>,
}

/// An existing branch whose head had already landed, continued with a
/// merge commit instead of being moved (#213).
///
/// After a squash merge the branch still points at the old PR head, which
/// the new export (parented on the new base) does not descend from. When
/// that head has landed, the export commit gets it as a second parent: the
/// branch fast-forwards (no force push), the first parent is the base, so
/// a PR against the base shows only the new work, and the tree is the
/// attempt's own.
///
/// The head has landed when either
/// - `merged_pr`: it is the head of a PR GitHub reports merged, and the
///   export's base contains that PR's merge commit; or
/// - `contained`: it changed something since its merge-base with the
///   export's base, and a three-way merge of it into the base is clean and
///   gives exactly the base's tree, so every change it made is already
///   there (what a squash merge leaves behind).
#[derive(Clone, Debug, Serialize)]
pub struct Continued {
    /// The branch's previous head, now the commit's second parent.
    pub head: String,
    /// The merged PR that head belonged to, when known.
    pub pr: Option<u64>,
    /// `merged_pr` or `contained`, as above.
    pub via: &'static str,
}

/// One commit of an exported story.
#[derive(Clone, Debug, Serialize)]
pub struct StoryCommit {
    pub change_id: String,
    pub title: String,
    pub commit: String,
    /// The stacked branch pointing at this commit, with `stack`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExportResult {
    pub export_id: i64,
    pub attempt: i64,
    pub snapshot: Snapshot,
    pub branch: String,
    pub commit: String,
    pub parent: Option<String>,
    pub tree: String,
    /// The commit the branch pointed to before, when the export moved it.
    pub replaced: Option<String>,
    /// True when the branch was checked out in the host repo, so its index
    /// and working tree were moved to the new commit too.
    pub checkout_updated: bool,
    pub objects_copied: usize,
    /// Git hooks that ran and passed before the commit was written.
    pub hooks: Vec<String>,
    /// The story's commits, oldest first, when the attempt was exported as a
    /// story; the last one is `commit`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub story: Vec<StoryCommit>,
    /// Set when the branch's old head had landed and the export continued
    /// it with a merge commit (see [`Continued`]).
    pub continued_from: Option<Continued>,
}

/// One recorded export of an attempt.
#[derive(Clone, Debug, Serialize)]
pub struct Export {
    pub id: i64,
    pub attempt_id: i64,
    pub snapshot_id: i64,
    pub branch: String,
    pub commit: String,
    pub parent: Option<String>,
    pub pr_url: Option<String>,
    pub created_at: i64,
}

/// The host git repository of a space.
pub(crate) struct HostRepo {
    pub workdir: PathBuf,
}

impl HostRepo {
    /// The repo whose top level is `workdir`.
    pub fn open(workdir: &Path) -> Result<HostRepo> {
        let repo = HostRepo {
            workdir: workdir.to_path_buf(),
        };
        let prefix = repo
            .git(&["rev-parse", "--show-prefix"], None)
            .map_err(|e| match e {
                Error::Git { .. } => Error::NotGitRepo(workdir.to_path_buf()),
                other => other,
            })?;
        if !prefix.is_empty() {
            return Err(Error::NotRepoRoot {
                workdir: workdir.to_path_buf(),
                prefix,
            });
        }
        Ok(repo)
    }

    /// Run git in the repo and return trimmed stdout.
    pub fn git(&self, args: &[&str], stdin: Option<&str>) -> Result<String> {
        self.git_env(args, stdin, &[])
    }

    pub fn git_env(
        &self,
        args: &[&str],
        stdin: Option<&str>,
        env: &[(&str, &str)],
    ) -> Result<String> {
        Ok(self.run(args, stdin, env)?.trim().to_string())
    }

    /// Run git in the repo and return stdout as it is, for patches.
    pub fn git_raw(&self, args: &[&str]) -> Result<String> {
        self.run(args, None, &[])
    }

    fn run(&self, args: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> Result<String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.workdir)
            .args(args)
            // Set when agentvc runs from a git hook (a view's post-commit);
            // they would point this command at the view's git directory.
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_PREFIX")
            .envs(env.iter().copied())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().at("git")?;
        if let Some(input) = stdin {
            child
                .stdin
                .take()
                .expect("stdin is piped")
                .write_all(input.as_bytes())
                .at("git stdin")?;
        }
        let out = child.wait_with_output().at("git")?;
        if !out.status.success() {
            return Err(Error::Git {
                cmd: args.first().copied().unwrap_or("").to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Resolve a revision to a commit id, or `None` if it does not exist.
    pub fn rev(&self, rev: &str) -> Option<String> {
        self.git(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{rev}^{{commit}}"),
            ],
            None,
        )
        .ok()
        .filter(|s| !s.is_empty())
    }

    /// Whether commit `a` is an ancestor of (or equal to) commit `b`.
    pub fn is_ancestor(&self, a: &str, b: &str) -> bool {
        self.git(&["merge-base", "--is-ancestor", a, b], None)
            .is_ok()
    }

    /// The branch checked out in the host repo, if any (`None` when HEAD is
    /// detached).
    pub fn checked_out_branch(&self) -> Option<String> {
        self.git(&["symbolic-ref", "--quiet", "HEAD"], None)
            .ok()
            .and_then(|r| r.strip_prefix("refs/heads/").map(str::to_string))
    }

    pub(crate) fn objects_dir(&self) -> Result<PathBuf> {
        let p = self.git(
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "objects",
            ],
            None,
        )?;
        Ok(PathBuf::from(p))
    }
}

/// The host repo's HEAD commit, if `workdir` is the top of a git repo with
/// at least one commit.
pub(crate) fn host_head(workdir: &Path) -> Option<String> {
    HostRepo::open(workdir).ok()?.rev("HEAD")
}

/// Copy the objects reachable from `tree` into a git object directory.
/// Returns how many objects were copied.
pub(crate) fn copy_tree_objects(space: &Space, tree: &Oid, dest: &Path) -> Result<usize> {
    let copy = |oid: &Oid| -> Result<bool> {
        let hex = oid.to_hex();
        let to = dest.join(&hex[..2]).join(&hex[2..]);
        if to.exists() {
            return Ok(false);
        }
        let dir = to.parent().expect("object path has a parent");
        fs::create_dir_all(dir).at(dir)?;
        let tmp = dir.join(format!(".agentvc-tmp-{}-{hex}", std::process::id()));
        // Loose objects are copied as they are; packed ones are written out
        // loose for git.
        let from = space.objects().loose_path(oid);
        if fs::copy(&from, &tmp).is_err() {
            fs::write(&tmp, space.objects().loose_bytes(oid)?).at(&tmp)?;
        }
        fs::rename(&tmp, &to).at(&to)?;
        Ok(true)
    };
    let mut copied = 0;
    let mut stack = vec![*tree];
    while let Some(t) = stack.pop() {
        // A tree already present means git (or an earlier export) wrote its
        // children first, so the whole subtree is there.
        if !copy(&t)? {
            continue;
        }
        copied += 1;
        for e in space.objects().read_tree(&t)? {
            if e.mode == Mode::Tree {
                stack.push(e.oid);
            } else if copy(&e.oid)? {
                copied += 1;
            }
        }
    }
    Ok(copied)
}

/// The commit message for an exported attempt: the subject, the task and
/// attempt details, and `Agentvc-*` trailers.
pub fn commit_message(
    space: &Space,
    attempt: &Attempt,
    snapshot: &Snapshot,
    message: Option<&str>,
    trailers: &[String],
) -> Result<String> {
    let task = attempt.task_id.map(|t| space.task(t)).transpose()?;
    let mut msg = match (message, &task) {
        (Some(m), _) => m.trim().to_string(),
        (None, Some(t)) => t.goal.trim().to_string(),
        (None, None) => format!("agentvc attempt a{}", attempt.id),
    };
    msg += "\n\n";
    if let Some(t) = &task {
        msg += &format!("Task t{}: {}\n", t.id, t.goal.trim());
        if let Some(c) = &t.criteria {
            msg += &format!("Criteria: {}\n", c.trim());
        }
    }
    msg += &format!(
        "Attempt a{} by {}{}, snapshot s{}\n\n",
        attempt.id,
        attempt.actor,
        attempt
            .model
            .as_ref()
            .map(|m| format!(" ({m})"))
            .unwrap_or_default(),
        snapshot.id
    );
    msg += &format!("{ATTEMPT_TRAILER}: a{}\n", attempt.id);
    msg += &format!("Agentvc-Snapshot: s{}\n", snapshot.id);
    if let Some(t) = &task {
        msg += &format!("Agentvc-Task: t{}\n", t.id);
    }
    msg += &format!("Agentvc-Actor: {}\n", attempt.actor);
    if let Some(m) = &attempt.model {
        msg += &format!("Agentvc-Model: {m}\n");
    }
    for t in trailers {
        let t = t.trim();
        if !t.is_empty() {
            msg += t;
            msg.push('\n');
        }
    }
    Ok(msg)
}

/// The message of one story commit: the change's title, then where it
/// comes from and agentvc's trailers, with the change id.
fn story_message(
    space: &Space,
    attempt: &Attempt,
    change: &crate::story::Change,
    of: usize,
    trailers: &[String],
) -> Result<String> {
    let task = attempt.task_id.map(|t| space.task(t)).transpose()?;
    let title = change.title.trim();
    let mut msg = if title.is_empty() {
        format!("Change {} of attempt a{}", change.position, attempt.id)
    } else {
        title.to_string()
    };
    msg += "\n\n";
    msg += &format!("Change {}/{of} of attempt a{}", change.position, attempt.id);
    if let Some(t) = &task {
        msg += &format!(" (task t{}: {})", t.id, t.goal.trim());
    }
    msg += "\n\n";
    msg += &format!("{}: {}\n", crate::story::CHANGE_TRAILER, change.change_id);
    msg += &format!("{ATTEMPT_TRAILER}: a{}\n", attempt.id);
    if let Some(s) = change.source_snapshot {
        msg += &format!("Agentvc-Snapshot: s{s}\n");
    }
    if let Some(t) = &task {
        msg += &format!("Agentvc-Task: t{}\n", t.id);
    }
    msg += &format!("Agentvc-Actor: {}\n", attempt.actor);
    for t in trailers {
        let t = t.trim();
        if !t.is_empty() {
            msg += t;
            msg.push('\n');
        }
    }
    Ok(msg)
}

/// Export attempt `attempt_id` as one commit on `opts.branch` in the host repo,
/// or, when it has a story (#21), as one commit per change.
///
/// An open attempt is snapshotted first, so the commit matches its view.
pub fn export(space: &Space, attempt_id: i64, opts: &ExportOptions) -> Result<ExportResult> {
    let repo = HostRepo::open(space.workdir())?;
    let attempt = space.attempt(attempt_id)?;
    let open = space.conflicts(attempt_id, false)?;
    if !open.is_empty() {
        return Err(Error::UnresolvedConflicts {
            attempt: attempt_id,
            paths: open.into_iter().map(|c| c.path).collect(),
        });
    }
    if repo
        .git(&["check-ref-format", "--branch", &opts.branch], None)
        .is_err()
        || opts.branch.starts_with('-')
    {
        return Err(Error::BadBranch(opts.branch.clone()));
    }
    let refname = format!("refs/heads/{}", opts.branch);
    let existing = repo.rev(&refname);
    let checked_out = repo.checked_out_branch().as_deref() == Some(opts.branch.as_str());
    if checked_out && existing.is_none() {
        // An unborn branch: there is no commit to move the checkout from.
        return Err(Error::BranchCheckedOut(opts.branch.clone()));
    }

    let base = match &opts.base {
        Some(b) => Some(repo.rev(b).ok_or_else(|| Error::NotFound {
            kind: "git revision",
            id: b.clone(),
        })?),
        None => None,
    };

    // The view as it is now, promoted or not, so a fix made after promote
    // is never left out (#116).
    let snapshot = if attempt.status.takes_snapshots() && space.ensure_view(&attempt)? {
        space.snapshot(attempt_id, "export", None)?.snapshot
    } else {
        space.head_snapshot(attempt_id)?
    };
    // By default, parent on the commit this state is based on: the one it was
    // last rebased onto, else the host HEAD recorded at init.
    let parent = match &base {
        Some(b) => Some(b.clone()),
        None => space
            .git_base_commit(&snapshot)?
            .and_then(|h| repo.rev(&h))
            .or_else(|| repo.rev("HEAD")),
    };
    let tree = snapshot.tree_oid()?;
    let mut objects_copied = copy_tree_objects(space, &tree, &repo.objects_dir()?)?;

    // Commit as the user when git knows who that is; otherwise as agentvc,
    // so export never fails on a fresh machine.
    let mut env = Vec::new();
    if repo.git(&["config", "user.email"], None).is_err() {
        env.extend([
            ("GIT_AUTHOR_NAME", "agentvc"),
            ("GIT_AUTHOR_EMAIL", "agentvc@localhost"),
            ("GIT_COMMITTER_NAME", "agentvc"),
            ("GIT_COMMITTER_EMAIL", "agentvc@localhost"),
        ]);
    }
    let commit_tree = |tree: &str, parents: &[&str], message: &str| {
        let mut args = vec!["commit-tree", tree];
        for p in parents {
            args.extend(["-p", p]);
        }
        args.extend(["-F", "-"]);
        repo.git_env(&args, Some(message), &env)
    };

    let verify = match opts.verify {
        Some(v) => v,
        None => crate::config::is_true(
            space
                .config(crate::config::EXPORT_VERIFY)?
                .as_deref()
                .unwrap_or("false"),
        ),
    };
    let mut hooks = Vec::new();

    let story = if opts.squash {
        None
    } else {
        space.story_for_export(attempt_id)?
    };
    let mut story_commits = Vec::new();

    // A branch whose head is not under the new commit's first parent: if
    // that head has landed in it, the top commit continues the branch with
    // it as second parent (#213); otherwise moving it needs --force.
    let first = match &story {
        Some(s) => Some(s.base_commit.clone()),
        None => parent.clone(),
    };
    let mut not_landed = None;
    // The head the push has to build on: the remote's when it is known,
    // else the local branch's.
    let continue_from = opts
        .remote_head
        .clone()
        .filter(|r| repo.rev(r).is_some())
        .or_else(|| existing.clone());
    let continued = match (&continue_from, &first) {
        (Some(old), Some(first)) if !repo.is_ancestor(old, first) => {
            match landed(&repo, old, first, opts.merged_pr.as_ref()) {
                Ok(c) => Some(c),
                Err(reason) => {
                    not_landed = Some(reason);
                    None
                }
            }
        }
        _ => None,
    };
    let second = continued.as_ref().map(|c| c.head.as_str());
    fn parents<'a>(first: Option<&'a String>, second: Option<&'a str>, top: bool) -> Vec<&'a str> {
        first
            .map(String::as_str)
            .into_iter()
            .chain(second.filter(|_| top))
            .collect()
    }

    let (parent, commit) = match &story {
        None => {
            let message = commit_message(
                space,
                &attempt,
                &snapshot,
                opts.message.as_deref(),
                &opts.trailers,
            )?;
            let message = if verify {
                let (m, h) = crate::land::verify(space, &attempt, &snapshot, message)?;
                hooks = h;
                m
            } else {
                message
            };
            let commit = commit_tree(
                &snapshot.tree,
                &parents(parent.as_ref(), second, true),
                &message,
            )?;
            (parent, commit)
        }
        Some(story) => {
            if let Some(b) = &base {
                if *b != story.base_commit {
                    return Err(Error::StoryBoundary(format!(
                        "the story of a{attempt_id} is based on {}; rebase it onto {b} \
                         first, or export with --squash",
                        &story.base_commit[..12.min(story.base_commit.len())]
                    )));
                }
            }
            let parent = Some(story.base_commit.clone());
            let mut prev = parent.clone();
            let n = story.changes.len();
            let mut messages = story
                .changes
                .iter()
                .map(|c| story_message(space, &attempt, c, n, &opts.trailers))
                .collect::<Result<Vec<_>>>()?;
            // Hooks check the story's final tree, which is the snapshot,
            // and the top commit's message.
            if verify {
                let last = messages.pop().expect("a story has changes");
                let (m, h) = crate::land::verify(space, &attempt, &snapshot, last)?;
                messages.push(m);
                hooks = h;
            }
            for (k, (c, message)) in story.changes.iter().zip(messages).enumerate() {
                objects_copied +=
                    copy_tree_objects(space, &Oid::from_hex(&c.tree)?, &repo.objects_dir()?)?;
                let commit = commit_tree(
                    &c.tree,
                    &parents(prev.as_ref(), second, k + 1 == n),
                    &message,
                )?;
                prev = Some(commit.clone());
                story_commits.push(StoryCommit {
                    change_id: c.change_id.clone(),
                    title: c.title.clone(),
                    commit,
                    branch: None,
                });
            }
            (parent, prev.expect("a story has changes"))
        }
    };

    // An existing branch moves without --force only when the new commit
    // builds on it, as `git push` would allow.
    if let Some(old) = &existing {
        // The local branch may only lag behind the remote one: then it
        // moves when its own changes have landed in the base too.
        let lagging = || {
            opts.remote_head.as_deref().is_some_and(|r| r != old)
                && first
                    .as_deref()
                    .is_some_and(|f| landed(&repo, old, f, None).is_ok())
        };
        if !opts.force && !repo.is_ancestor(old, &commit) && !lagging() {
            return Err(match not_landed {
                Some(reason) => Error::BranchNotLanded {
                    branch: opts.branch.clone(),
                    head: old[..12.min(old.len())].to_string(),
                    reason,
                },
                None => Error::BranchExists(opts.branch.clone()),
            });
        }
    }
    let reflog = format!("agentvc export a{attempt_id}");
    move_branch(
        &repo,
        &opts.branch,
        existing.as_deref(),
        &commit,
        checked_out,
        &reflog,
    )?;

    // Stacked branches: one per change below the top, which is `branch`.
    if opts.stack {
        let n = story_commits.len();
        for (k, c) in story_commits
            .iter_mut()
            .enumerate()
            .take(n.saturating_sub(1))
        {
            let name = format!("{}-{}", opts.branch, k + 1);
            let r = format!("refs/heads/{name}");
            if let Some(old) = repo.rev(&r) {
                if !opts.force && !repo.is_ancestor(&old, &c.commit) {
                    return Err(Error::BranchExists(name));
                }
            }
            repo.git(&["update-ref", "-m", &reflog, &r, &c.commit], None)?;
            c.branch = Some(name);
        }
    }

    space.db.execute(
        "INSERT INTO exports (attempt_id, snapshot_id, branch, commit_id, parent, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![attempt_id, snapshot.id, opts.branch, commit, parent, now()],
    )?;
    let export_id = space.db.last_insert_rowid();
    space.record(
        &attempt.actor,
        "attempt.export",
        serde_json::json!({
            "attempt": attempt_id, "snapshot": snapshot.id, "branch": opts.branch,
            "commit": commit, "parent": parent, "replaced": existing,
            "story": story_commits.iter().map(|c| &c.change_id).collect::<Vec<_>>(),
            "continued_from": continued,
        }),
    )?;
    Ok(ExportResult {
        export_id,
        attempt: attempt_id,
        tree: snapshot.tree.clone(),
        snapshot,
        branch: opts.branch.clone(),
        commit,
        parent,
        replaced: existing,
        checkout_updated: checked_out,
        objects_copied,
        hooks,
        story: story_commits,
        continued_from: continued,
    })
}

/// Whether the branch head `old`, which `base` does not contain, has
/// landed in `base` anyway (see [`Continued`]); otherwise why not.
fn landed(
    repo: &HostRepo,
    old: &str,
    base: &str,
    merged: Option<&MergedPr>,
) -> std::result::Result<Continued, String> {
    let short = |c: &str| c[..12.min(c.len())].to_string();
    let pr = merged.filter(|m| m.head == old);
    if let Some(m) = pr {
        match &m.merge_commit {
            Some(mc) if repo.rev(mc).is_some() && repo.is_ancestor(mc, base) => {
                return Ok(Continued {
                    head: old.to_string(),
                    pr: Some(m.number),
                    via: "merged_pr",
                });
            }
            _ => {}
        }
    }
    let tree = |c: &str| repo.git(&["rev-parse", &format!("{c}^{{tree}}")], None);
    let contained = (|| -> Option<bool> {
        let mb = repo.git(&["merge-base", old, base], None).ok()?;
        if tree(old).ok()? == tree(&mb).ok()? {
            // Nothing to land: an empty change is not "landed" work.
            return Some(false);
        }
        // Exit 1 (a conflict) is an error here, so not landed.
        let merged = repo
            .git(&["merge-tree", "--write-tree", base, old], None)
            .ok()?;
        Some(merged.lines().next() == Some(tree(base).ok()?.as_str()))
    })()
    .unwrap_or(false);
    if contained {
        return Ok(Continued {
            head: old.to_string(),
            pr: pr.map(|m| m.number),
            via: "contained",
        });
    }
    Err(match pr {
        Some(m) => format!(
            "its PR #{} merged{}, but the export's base {} does not contain that merge; \
             rebase the attempt onto the updated base branch first",
            m.number,
            m.merge_commit
                .as_deref()
                .map(|c| format!(" as {}", short(c)))
                .unwrap_or_default(),
            short(base)
        ),
        None => match merged {
            Some(m) => format!(
                "PR #{} merged at {}, but the branch has moved since, and its changes are not all in {}",
                m.number,
                short(&m.head),
                short(base)
            ),
            None => format!(
                "it does not build on the export's base {} and not all of its changes are in it \
                 (no merged PR is known for it), so moving the branch would drop them",
                short(base)
            ),
        },
    })
}

/// A commit written for an attempt, not yet on any branch.
pub(crate) struct Made {
    pub snapshot: Snapshot,
    pub parent: Option<String>,
    pub commit: String,
    pub hooks: Vec<String>,
}

/// Write the commit for an attempt's head (snapshotting an open view
/// first): its tree on `opts.base` or the commit the state is based on,
/// after the repository's hooks when verifying.
pub(crate) fn make_commit(
    space: &Space,
    repo: &HostRepo,
    attempt: &Attempt,
    opts: &ExportOptions,
) -> Result<Made> {
    let attempt_id = attempt.id;
    let base = match &opts.base {
        Some(b) => Some(repo.rev(b).ok_or_else(|| Error::NotFound {
            kind: "git revision",
            id: b.clone(),
        })?),
        None => None,
    };

    let snapshot = if attempt.status.takes_snapshots() && space.ensure_view(attempt)? {
        space.snapshot(attempt_id, "export", None)?.snapshot
    } else {
        space.head_snapshot(attempt_id)?
    };
    // By default, parent on the commit this state is based on: the one it was
    // last rebased onto, else the host HEAD recorded at init.
    let parent = match base {
        Some(b) => Some(b),
        None => space
            .git_base_commit(&snapshot)?
            .and_then(|h| repo.rev(&h))
            .or_else(|| repo.rev("HEAD")),
    };
    let tree = snapshot.tree_oid()?;
    copy_tree_objects(space, &tree, &repo.objects_dir()?)?;

    let message = commit_message(
        space,
        attempt,
        &snapshot,
        opts.message.as_deref(),
        &opts.trailers,
    )?;
    let verify = match opts.verify {
        Some(v) => v,
        None => crate::config::is_true(
            space
                .config(crate::config::EXPORT_VERIFY)?
                .as_deref()
                .unwrap_or("false"),
        ),
    };
    let (message, hooks) = if verify {
        crate::land::verify(space, attempt, &snapshot, message)?
    } else {
        (message, Vec::new())
    };
    let mut args = vec!["commit-tree", snapshot.tree.as_str()];
    if let Some(p) = &parent {
        args.extend(["-p", p.as_str()]);
    }
    args.extend(["-F", "-"]);
    // Commit as the user when git knows who that is; otherwise as agentvc,
    // so export never fails on a fresh machine.
    let mut env = Vec::new();
    if repo.git(&["config", "user.email"], None).is_err() {
        env.extend([
            ("GIT_AUTHOR_NAME", "agentvc"),
            ("GIT_AUTHOR_EMAIL", "agentvc@localhost"),
            ("GIT_COMMITTER_NAME", "agentvc"),
            ("GIT_COMMITTER_EMAIL", "agentvc@localhost"),
        ]);
    }
    let commit = repo.git_env(&args, Some(&message), &env)?;
    Ok(Made {
        snapshot,
        parent,
        commit,
        hooks,
    })
}

/// Point `branch` at `new`, compare-and-swap against `old` (`None`: the
/// branch must not exist). A checked-out branch carries its index and
/// working tree along; that refuses with `checkout_dirty`, changing nothing,
/// when local edits would be overwritten.
pub(crate) fn move_branch(
    repo: &HostRepo,
    branch: &str,
    old: Option<&str>,
    new: &str,
    checked_out: bool,
    reflog: &str,
) -> Result<()> {
    let refname = format!("refs/heads/{branch}");
    // A checked-out branch (a cloud thread's own branch) carries its index
    // and working tree along, so the checkout does not look dirty. The
    // two-tree read-tree refuses, changing nothing, when local edits would
    // be overwritten.
    if checked_out {
        let old = old.expect("checked out branch exists");
        repo.git(&["read-tree", "-m", "-u", old, new], None)
            .map_err(|e| match e {
                Error::Git { stderr, .. } => Error::CheckoutDirty {
                    branch: branch.to_string(),
                    stderr,
                },
                other => other,
            })?;
    }
    // Compare-and-swap: creation fails if the branch appeared meanwhile, and a
    // move fails if it changed since we looked.
    let expect = old.unwrap_or(ZERO_OID);
    if let Err(e) = repo.git(&["update-ref", "-m", reflog, &refname, new, expect], None) {
        if checked_out {
            // Put the checkout back where the branch still is.
            let _ = repo.git(&["read-tree", "-m", "-u", new, expect], None);
        }
        return Err(e);
    }
    Ok(())
}

const EXPORT_COLS: &str =
    "id, attempt_id, snapshot_id, branch, commit_id, parent, pr_url, created_at";

fn row_to_export(r: &rusqlite::Row<'_>) -> rusqlite::Result<Export> {
    Ok(Export {
        id: r.get(0)?,
        attempt_id: r.get(1)?,
        snapshot_id: r.get(2)?,
        branch: r.get(3)?,
        commit: r.get(4)?,
        parent: r.get(5)?,
        pr_url: r.get(6)?,
        created_at: r.get(7)?,
    })
}

impl Space {
    /// Exports of one attempt, oldest first.
    pub fn exports(&self, attempt_id: i64) -> Result<Vec<Export>> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {EXPORT_COLS} FROM exports WHERE attempt_id = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![attempt_id], row_to_export)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The most recent export to `branch`, from any attempt.
    pub fn last_export_to(&self, branch: &str) -> Result<Option<Export>> {
        Ok(self
            .db
            .query_row(
                &format!(
                    "SELECT {EXPORT_COLS} FROM exports WHERE branch = ?1 ORDER BY id DESC LIMIT 1"
                ),
                params![branch],
                row_to_export,
            )
            .optional()?)
    }

    /// The pull request opened for an attempt, if any.
    /// The most recently recorded PR of any attempt in the space.
    pub fn latest_pr_url(&self) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row(
                "SELECT pr_url FROM exports WHERE pr_url IS NOT NULL ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn pr_url(&self, attempt_id: i64) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row(
                "SELECT pr_url FROM exports WHERE attempt_id = ?1 AND pr_url IS NOT NULL
                 ORDER BY id DESC LIMIT 1",
                params![attempt_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub(crate) fn set_export_pr(&self, export_id: i64, url: &str) -> Result<()> {
        self.db.execute(
            "UPDATE exports SET pr_url = ?1 WHERE id = ?2",
            params![url, export_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::{ForkRequest, Status};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
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

    /// A git repo with one commit and a space on top of it.
    fn repo() -> (crate::testutil::TestDir, Space, String) {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        git(w, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(w.join("src")).unwrap();
        fs::write(w.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
        fs::write(w.join(".gitignore"), "target/\n").unwrap();
        git(w, &["add", "-A"]);
        git(w, &["commit", "-q", "-m", "base"]);
        let head = git(w, &["rev-parse", "HEAD"]);
        let space = Space::init(w).unwrap();
        (tmp, space, head)
    }

    fn fork(space: &Space, task: i64, actor: &str) -> Attempt {
        space
            .fork(&ForkRequest {
                task_id: Some(task),
                actor: actor.into(),
                model: Some("model-x".into()),
                ..Default::default()
            })
            .unwrap()
            .attempt
    }

    #[test]
    fn export_round_trips_tree_and_leaves_worktree_alone() {
        let (tmp, space, head) = repo();
        let w = tmp.path();
        let task = space
            .create_task("make f return 2", Some("tests pass"))
            .unwrap();
        let a = fork(&space, task.id, "agent:one");
        fs::write(a.view_path.join("src/lib.rs"), "pub fn f() -> u32 { 2 }\n").unwrap();
        fs::write(a.view_path.join("NEW.md"), "new\n").unwrap();
        let status_before = git(w, &["status", "--porcelain"]);

        let r = export(
            &space,
            a.id,
            &ExportOptions {
                branch: "agentvc/a2".into(),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(git(w, &["rev-parse", "agentvc/a2^{tree}"]), r.tree);
        assert_eq!(r.tree, space.head_snapshot(a.id).unwrap().tree);
        assert_eq!(r.parent.as_deref(), Some(head.as_str()));
        assert_eq!(git(w, &["rev-parse", "agentvc/a2^"]), head);
        assert!(r.objects_copied > 0);
        git(w, &["fsck", "--no-dangling"]);

        let msg = git(w, &["log", "-1", "--format=%B", "agentvc/a2"]);
        assert!(msg.starts_with("make f return 2\n"), "{msg}");
        assert!(msg.contains("Attempt a2 by agent:one (model-x)"), "{msg}");
        let trailer = git(
            w,
            &[
                "log",
                "-1",
                "--format=%(trailers:key=Agentvc-Attempt,valueonly)",
                "agentvc/a2",
            ],
        );
        assert_eq!(trailer, "a2");

        // The user's checkout, index and working tree are as they were.
        assert_eq!(git(w, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        assert_eq!(git(w, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(w, &["status", "--porcelain"]), status_before);
        assert_eq!(
            fs::read_to_string(w.join("src/lib.rs")).unwrap(),
            "pub fn f() -> u32 { 1 }\n"
        );
        assert_eq!(space.exports(a.id).unwrap().len(), 1);
    }

    /// After a squash merge the branch's old head is in main by content
    /// only: the next export continues the branch with a merge commit, no
    /// --force (#213). Work that is not in main still refuses.
    #[test]
    fn landed_branch_head_is_continued_with_a_merge_commit() {
        let (tmp, space, _head) = repo();
        let w = tmp.path();
        let task = space.create_task("t", None).unwrap();
        let a = fork(&space, task.id, "agent");
        fs::write(a.view_path.join("src/lib.rs"), "pub fn f() -> u32 { 2 }\n").unwrap();
        let first = export(
            &space,
            a.id,
            &ExportOptions {
                branch: "thread".into(),
                ..Default::default()
            },
        )
        .unwrap();
        // Squash-merged into main, then main moves on.
        fs::write(w.join("src/lib.rs"), "pub fn f() -> u32 { 2 }\n").unwrap();
        git(w, &["commit", "-q", "-am", "squashed (#1)"]);
        fs::write(w.join("other"), "o\n").unwrap();
        git(w, &["add", "other"]);
        git(w, &["commit", "-q", "-m", "other"]);
        let main = git(w, &["rev-parse", "main"]);

        let b = fork(&space, task.id, "agent");
        fs::write(b.view_path.join("b"), "b\n").unwrap();
        let opts = ExportOptions {
            branch: "thread".into(),
            base: Some("main".into()),
            ..Default::default()
        };
        let r = export(&space, b.id, &opts).unwrap();
        let c = r.continued_from.as_ref().expect("continued");
        assert_eq!(c.head, first.commit);
        assert_eq!(c.via, "contained");
        assert_eq!(c.pr, None);
        assert_eq!(r.replaced.as_deref(), Some(first.commit.as_str()));
        assert_eq!(git(w, &["rev-parse", "thread^1"]), main);
        assert_eq!(git(w, &["rev-parse", "thread^2"]), first.commit);
        assert_eq!(git(w, &["rev-parse", "thread^{tree}"]), r.tree);
        assert_eq!(r.parent.as_deref(), Some(main.as_str()));

        // Main changes f again, and the branch gets work of its own that is
        // not in main: refused, saying why.
        fs::write(w.join("src/lib.rs"), "pub fn f() -> u32 { 9 }\n").unwrap();
        git(w, &["commit", "-q", "-am", "main changes f again"]);
        let thread = git(w, &["rev-parse", "thread"]);
        let mut c_opts = opts.clone();
        fs::write(b.view_path.join("b"), "b2\n").unwrap();
        let foreign = {
            git(w, &["checkout", "-q", "thread"]);
            fs::write(w.join("src/lib.rs"), "pub fn f() -> u32 { 3 }\n").unwrap();
            git(w, &["commit", "-q", "-am", "unmerged work"]);
            let h = git(w, &["rev-parse", "HEAD"]);
            git(w, &["checkout", "-q", "main"]);
            h
        };
        let err = export(&space, b.id, &c_opts).unwrap_err();
        assert_eq!(err.code(), "branch_exists", "{err}");
        assert!(matches!(err, Error::BranchNotLanded { .. }), "{err}");
        assert!(err.to_string().contains("not all of its changes"), "{err}");
        assert_eq!(git(w, &["rev-parse", "thread"]), foreign);
        // GitHub's word that this head merged (into a main that holds its
        // merge commit) is enough, though the content check refuses.
        c_opts.merged_pr = Some(MergedPr {
            number: 4,
            head: foreign.clone(),
            merge_commit: Some(git(w, &["rev-parse", "main"])),
        });
        let r = export(&space, b.id, &c_opts).unwrap();
        let c = r.continued_from.as_ref().expect("continued");
        assert_eq!((c.via, c.pr), ("merged_pr", Some(4)));
        assert_eq!(git(w, &["rev-parse", "thread^2"]), foreign);
        assert!(repo_ancestor(w, &thread, "thread"));
        // A merged PR whose merge the base does not contain: refused.
        c_opts.merged_pr = Some(MergedPr {
            number: 5,
            head: git(w, &["rev-parse", "thread"]),
            merge_commit: Some(first.commit.clone()),
        });
        let err = export(&space, b.id, &c_opts).unwrap_err();
        assert!(err.to_string().contains("PR #5 merged"), "{err}");
    }

    fn repo_ancestor(w: &Path, a: &str, b: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(w)
            .args(["merge-base", "--is-ancestor", a, b])
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn existing_branch_needs_force() {
        let (tmp, space, _head) = repo();
        let w = tmp.path();
        let task = space.create_task("t", None).unwrap();
        let a = fork(&space, task.id, "agent");
        let opts = ExportOptions {
            branch: "feature".into(),
            ..Default::default()
        };
        let first = export(&space, a.id, &opts).unwrap();
        fs::write(a.view_path.join("x"), "x\n").unwrap();
        assert!(matches!(
            export(&space, a.id, &opts),
            Err(Error::BranchNotLanded { .. })
        ));
        let second = export(
            &space,
            a.id,
            &ExportOptions {
                force: true,
                ..opts.clone()
            },
        )
        .unwrap();
        assert_eq!(second.replaced.as_deref(), Some(first.commit.as_str()));
        assert_eq!(git(w, &["rev-parse", "feature"]), second.commit);

        assert!(matches!(
            export(
                &space,
                a.id,
                &ExportOptions {
                    branch: "bad..name".into(),
                    ..Default::default()
                }
            ),
            Err(Error::BadBranch(_))
        ));
    }

    #[test]
    fn checked_out_branch_fast_forwards_with_its_checkout() {
        let (tmp, space, head) = repo();
        let w = tmp.path();
        let task = space.create_task("t", None).unwrap();
        let a = fork(&space, task.id, "agent");
        fs::write(a.view_path.join("src/lib.rs"), "pub fn f() -> u32 { 2 }\n").unwrap();
        let opts = ExportOptions {
            branch: "main".into(),
            trailers: vec!["Co-Authored-By: Bot <bot@example.com>".into(), " ".into()],
            ..Default::default()
        };
        // main is checked out at the commit the attempt is based on: no
        // --force needed, and the checkout moves along cleanly.
        let r = export(&space, a.id, &opts).unwrap();
        assert!(r.checkout_updated);
        assert_eq!(r.replaced.as_deref(), Some(head.as_str()));
        assert_eq!(git(w, &["rev-parse", "HEAD"]), r.commit);
        assert_eq!(
            git(w, &["status", "--porcelain", "--untracked-files=no"]),
            ""
        );
        assert_eq!(
            fs::read_to_string(w.join("src/lib.rs")).unwrap(),
            "pub fn f() -> u32 { 2 }\n"
        );
        let msg = git(w, &["log", "-1", "--format=%B"]);
        assert!(
            msg.ends_with("Agentvc-Model: model-x\nCo-Authored-By: Bot <bot@example.com>"),
            "{msg}"
        );
        let co = git(
            w,
            &[
                "log",
                "-1",
                "--format=%(trailers:key=Co-Authored-By,valueonly)",
            ],
        );
        assert_eq!(co, "Bot <bot@example.com>");

        // A second export of the attempt parents on the same base again, so
        // it no longer fast-forwards: that needs --force.
        fs::write(a.view_path.join("x"), "x\n").unwrap();
        fs::write(a.view_path.join("src/lib.rs"), "pub fn f() -> u32 { 3 }\n").unwrap();
        assert!(matches!(
            export(&space, a.id, &opts),
            Err(Error::BranchNotLanded { .. })
        ));
        // Local edits the export would overwrite: refused, nothing moves.
        fs::write(w.join("src/lib.rs"), "local edit\n").unwrap();
        let err = export(
            &space,
            a.id,
            &ExportOptions {
                force: true,
                ..opts.clone()
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), "checkout_dirty", "{err}");
        assert_eq!(git(w, &["rev-parse", "HEAD"]), r.commit);
        assert_eq!(
            fs::read_to_string(w.join("src/lib.rs")).unwrap(),
            "local edit\n"
        );

        // Without the edit, --force moves the branch and the checkout.
        git(w, &["checkout", "--", "src/lib.rs"]);
        let r2 = export(
            &space,
            a.id,
            &ExportOptions {
                force: true,
                ..opts.clone()
            },
        )
        .unwrap();
        assert_eq!(git(w, &["rev-parse", "HEAD"]), r2.commit);
        assert_eq!(fs::read_to_string(w.join("x")).unwrap(), "x\n");
        assert_eq!(
            git(w, &["status", "--porcelain", "--untracked-files=no"]),
            ""
        );
    }

    #[test]
    fn parents_on_the_commit_the_attempt_was_forked_from() {
        let (tmp, space, head) = repo();
        let w = tmp.path();
        let task = space.create_task("t", None).unwrap();
        let a = fork(&space, task.id, "agent");
        // The user commits on main after a's fork; a did not see it.
        fs::write(w.join("later.txt"), "later\n").unwrap();
        git(w, &["add", "later.txt"]);
        git(w, &["commit", "-q", "-m", "later"]);
        let later = git(w, &["rev-parse", "HEAD"]);

        let export_to = |id: i64, branch: &str, base: Option<&str>| {
            export(
                &space,
                id,
                &ExportOptions {
                    branch: branch.into(),
                    base: base.map(String::from),
                    ..Default::default()
                },
            )
            .unwrap()
        };
        assert_eq!(
            export_to(a.id, "b1", None).parent.as_deref(),
            Some(head.as_str())
        );
        assert_eq!(
            export_to(a.id, "b2", Some("main")).parent.as_deref(),
            Some(later.as_str())
        );

        // b is forked from the checkout after that commit (#96): its base is
        // the new HEAD, so the export carries only b's own change.
        let b = fork(&space, task.id, "agent");
        fs::write(b.view_path.join("b.txt"), "b\n").unwrap();
        space.snapshot(b.id, "manual", None).unwrap();
        let r = export_to(b.id, "b3", None);
        assert_eq!(r.parent.as_deref(), Some(later.as_str()));
        let files = git(w, &["diff", "--name-only", &later, "b3"]);
        assert_eq!(files, "b.txt");
    }

    #[test]
    fn closed_attempts_export_their_last_snapshot() {
        let (tmp, space, _head) = repo();
        let task = space.create_task("t", None).unwrap();
        let a = fork(&space, task.id, "agent");
        fs::write(a.view_path.join("done.txt"), "done\n").unwrap();
        space.set_status(a.id, Status::Promoted, None).unwrap();
        let r = export(
            &space,
            a.id,
            &ExportOptions {
                branch: "promoted".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(git(tmp.path(), &["show", "promoted:done.txt"]), "done");
        assert_eq!(r.snapshot.trigger, "promoted");
    }

    /// #116: a fix made in a promoted attempt's view (CI, review) is recorded
    /// and exported, never silently left out; an abandoned one stays frozen.
    #[test]
    fn promoted_attempts_keep_recording_their_view() {
        let (tmp, space, _head) = repo();
        let task = space.create_task("t", None).unwrap();
        let a = fork(&space, task.id, "agent");
        fs::write(a.view_path.join("done.txt"), "v1\n").unwrap();
        space.set_status(a.id, Status::Promoted, None).unwrap();
        let opts = ExportOptions {
            branch: "pr".into(),
            force: true,
            ..Default::default()
        };
        export(&space, a.id, &opts).unwrap();
        // The CI fix after promote.
        fs::write(a.view_path.join("done.txt"), "v2\n").unwrap();
        let s = space.snapshot(a.id, "manual", None).unwrap();
        assert!(!s.unchanged);
        fs::write(a.view_path.join("done.txt"), "v3\n").unwrap();
        // Not even snapshotted: export still takes the view as it is.
        let r = export(&space, a.id, &opts).unwrap();
        assert_eq!(git(tmp.path(), &["show", "pr:done.txt"]), "v3");
        assert_eq!(r.snapshot.trigger, "export");
        assert_eq!(space.attempt(a.id).unwrap().status, Status::Promoted);

        let b = fork(&space, task.id, "agent");
        fs::write(b.view_path.join("b.txt"), "b\n").unwrap();
        space.snapshot(b.id, "tool", None).unwrap();
        space.set_status(b.id, Status::Abandoned, None).unwrap();
        fs::write(b.view_path.join("b.txt"), "later\n").unwrap();
        assert_eq!(
            space.snapshot(b.id, "manual", None).unwrap_err().code(),
            "attempt_closed"
        );
    }

    #[test]
    fn unborn_repo_gets_root_commit_and_non_repo_errors() {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        git(w, &["init", "-q", "-b", "main"]);
        fs::write(w.join("a.txt"), "a\n").unwrap();
        let space = Space::init(w).unwrap();
        let r = export(
            &space,
            1,
            &ExportOptions {
                branch: "first".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.parent, None);
        assert_eq!(git(w, &["show", "first:a.txt"]), "a");

        let plain = crate::testutil::tempdir();
        fs::write(plain.path().join("a.txt"), "a\n").unwrap();
        let space = Space::init(plain.path()).unwrap();
        // tempdir may sit inside some other repo on a dev machine; either
        // error is a refusal to export.
        let err = export(
            &space,
            1,
            &ExportOptions {
                branch: "x".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            Error::NotGitRepo(_) | Error::NotRepoRoot { .. }
        ));
    }
}
