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
    let src_root = space.objects().root();
    let copy = |oid: &Oid| -> Result<bool> {
        let hex = oid.to_hex();
        let to = dest.join(&hex[..2]).join(&hex[2..]);
        if to.exists() {
            return Ok(false);
        }
        let from = src_root.join(&hex[..2]).join(&hex[2..]);
        let dir = to.parent().expect("object path has a parent");
        fs::create_dir_all(dir).at(dir)?;
        let tmp = dir.join(format!(".agentvc-tmp-{}-{hex}", std::process::id()));
        fs::copy(&from, &tmp).at(&from)?;
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

/// Export attempt `attempt_id` as one commit on `opts.branch` in the host repo.
///
/// An open attempt is snapshotted first, so the commit matches its view.
pub fn export(space: &Space, attempt_id: i64, opts: &ExportOptions) -> Result<ExportResult> {
    let repo = HostRepo::open(space.workdir())?;
    let attempt = space.attempt(attempt_id)?;
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

    let snapshot = if attempt.status.is_open() && space.ensure_view(&attempt)? {
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
    let tree = Oid::from_hex(&snapshot.tree)?;
    let objects_copied = copy_tree_objects(space, &tree, &repo.objects_dir()?)?;

    let message = commit_message(
        space,
        &attempt,
        &snapshot,
        opts.message.as_deref(),
        &opts.trailers,
    )?;
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

    // An existing branch moves without --force only when the new commit
    // builds on it, as `git push` would allow.
    if let Some(old) = &existing {
        if !opts.force && !repo.is_ancestor(old, &commit) {
            return Err(Error::BranchExists(opts.branch.clone()));
        }
    }
    // A checked-out branch (a cloud thread's own branch) carries its index
    // and working tree along, so the checkout does not look dirty. The
    // two-tree read-tree refuses, changing nothing, when local edits would
    // be overwritten.
    if checked_out {
        let old = existing.as_deref().expect("checked out branch exists");
        repo.git(&["read-tree", "-m", "-u", old, &commit], None)
            .map_err(|e| match e {
                Error::Git { stderr, .. } => Error::CheckoutDirty {
                    branch: opts.branch.clone(),
                    stderr,
                },
                other => other,
            })?;
    }

    // Compare-and-swap: creation fails if the branch appeared meanwhile, and a
    // move fails if it changed since we looked.
    let old = existing.clone().unwrap_or_else(|| ZERO_OID.to_string());
    let reflog = format!("agentvc export a{attempt_id}");
    if let Err(e) = repo.git(
        &["update-ref", "-m", &reflog, &refname, &commit, &old],
        None,
    ) {
        if checked_out {
            // Put the checkout back where the branch still is.
            let _ = repo.git(&["read-tree", "-m", "-u", &commit, &old], None);
        }
        return Err(e);
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
    })
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
            Err(Error::BranchExists(_))
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
            Err(Error::BranchExists(_))
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
    fn parents_on_import_head_not_later_commits() {
        let (tmp, space, head) = repo();
        let w = tmp.path();
        // The user commits on main after init; the attempt did not see it.
        fs::write(w.join("later.txt"), "later\n").unwrap();
        git(w, &["add", "later.txt"]);
        git(w, &["commit", "-q", "-m", "later"]);
        let later = git(w, &["rev-parse", "HEAD"]);

        let task = space.create_task("t", None).unwrap();
        let a = fork(&space, task.id, "agent");
        let r = export(
            &space,
            a.id,
            &ExportOptions {
                branch: "b1".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.parent.as_deref(), Some(head.as_str()));

        let r = export(
            &space,
            a.id,
            &ExportOptions {
                branch: "b2".into(),
                base: Some("main".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.parent.as_deref(), Some(later.as_str()));
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
