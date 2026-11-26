//! Shipping without GitHub (#75): `agentvc land` and `--verify`.
//!
//! **Verify.** `export` writes commits with `git commit-tree`, so the
//! repository's own `pre-commit` and `commit-msg` hooks (formatters,
//! linters, DCO sign-off) never ran, and a PR could fail checks a local
//! commit would have caught. With `--verify` (or `agentvc config
//! export.verify true`) `export`, `pr` and `land` first run those hooks in
//! the attempt's view, through its git link (#68), as `git commit` would:
//! the index is the tree being exported, `HEAD` the commit it builds on.
//! A failing hook stops the export with code `hook_failed` and its output,
//! before any commit or ref is written. A `commit-msg` hook may rewrite the
//! message (a `Signed-off-by` line); the rewritten message is used. A
//! `pre-commit` hook that changes staged files fails the export too, since
//! what ships is the snapshot: fix the view, snapshot, export again.
//!
//! **Land.** `agentvc land aN [--onto <branch>]` puts an attempt on a local
//! branch, by default the checked-out one: rebase onto the branch tip when
//! the branch moved (a conflict fails with `conflict`, changing nothing),
//! then fast-forward the branch to one clean commit (`--squash`, the
//! default), or write a merge commit of the tip and the attempt's own commit
//! (`--merge`). A checked-out branch moves its index and working tree
//! along; local edits that the move would overwrite fail with
//! `checkout_dirty` and move nothing, other local edits stay as they are.
//! The landing is recorded on the attempt (`agentvc status aN`, the UI).

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{Error, IoContext, Result};
use crate::export::{self, ExportOptions, HostRepo};
use crate::rebase::{self, RebaseOptions};
use crate::space::{Attempt, Snapshot, Space, SPACE_DIR};

/// Hooks `--verify` runs, in order.
pub const VERIFY_HOOKS: &[&str] = &["pre-commit", "commit-msg"];

/// Most hook output kept in a `hook_failed` error.
const MAX_OUTPUT: usize = 8000;

fn view_git(view: &Path, index: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(view).stdin(Stdio::null());
    for v in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(v);
    }
    cmd.env("GIT_INDEX_FILE", index);
    cmd
}

fn tail(s: &str) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.to_string();
    }
    let mut start = s.len() - MAX_OUTPUT;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("...{}", &s[start..])
}

/// Run the repository's `pre-commit` and `commit-msg` hooks for `snapshot`
/// in the attempt's view. Returns the message (as `commit-msg` left it) and
/// the hooks that ran. The snapshot's objects must already be in the host
/// repository.
pub fn verify(
    space: &Space,
    attempt: &Attempt,
    snapshot: &Snapshot,
    message: String,
) -> Result<(String, Vec<String>)> {
    let no_git = |why: &str| Error::NoViewGit {
        id: attempt.id,
        reason: why.to_string(),
    };
    if !space.ensure_view(attempt)? {
        return Err(no_git("its view is gone"));
    }
    let view = &attempt.view_path;
    if !view.join(".git").exists() {
        // Promoted attempts keep their link; older views may have none yet.
        space.link_view_git(attempt)?;
    }
    if !view.join(".git").exists() {
        return Err(no_git("the host has no git commits to link it to"));
    }
    let scratch = space.workdir().join(SPACE_DIR).join("tmp");
    fs::create_dir_all(&scratch).at(&scratch)?;
    let index = scratch.join(format!("verify-a{}.index", attempt.id));
    let msg_file = scratch.join(format!("verify-a{}.msg", attempt.id));
    let _ = fs::remove_file(&index);
    let result = run_hooks(view, &index, &msg_file, snapshot, message);
    let _ = fs::remove_file(&index);
    let _ = fs::remove_file(&msg_file);
    result
}

fn run_hooks(
    view: &Path,
    index: &Path,
    msg_file: &Path,
    snapshot: &Snapshot,
    message: String,
) -> Result<(String, Vec<String>)> {
    let git = |args: &[&str]| -> Result<String> {
        let out = view_git(view, index).args(args).output().at("git")?;
        if !out.status.success() {
            return Err(Error::Git {
                cmd: args.first().copied().unwrap_or("").to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    // Stage exactly the tree being exported, then refresh stat data so
    // `git diff` (index against the view) only shows unsnapshotted edits.
    git(&["read-tree", &snapshot.tree])?;
    let _ = view_git(view, index)
        .args(["update-index", "-q", "--refresh"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    fs::write(msg_file, &message).at(msg_file)?;
    let msg_arg = msg_file.to_string_lossy().into_owned();
    let mut ran = Vec::new();
    for hook in VERIFY_HOOKS {
        let hook_path = git(&["rev-parse", "--git-path", &format!("hooks/{hook}")])?;
        let hook_path = view.join(hook_path);
        if !is_executable(&hook_path) {
            continue;
        }
        let mut args = vec!["hook", "run", "--ignore-missing", hook];
        if *hook == "commit-msg" {
            args.extend(["--", msg_arg.as_str()]);
        }
        let out = view_git(view, index).args(&args).output().at("git")?;
        if !out.status.success() {
            let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
            output += &String::from_utf8_lossy(&out.stderr);
            return Err(Error::HookFailed {
                hook: hook.to_string(),
                exit_code: out.status.code(),
                output: tail(output.trim()),
            });
        }
        if *hook == "pre-commit" {
            let staged = git(&["write-tree"])?;
            if staged != snapshot.tree {
                return Err(Error::HookFailed {
                    hook: hook.to_string(),
                    exit_code: Some(0),
                    output: "the hook changed staged files; what ships is the snapshot, so \
                             apply the fix in the view, snapshot it and export again"
                        .into(),
                });
            }
        }
        ran.push(hook.to_string());
    }
    let message = fs::read_to_string(msg_file).at(msg_file)?;
    Ok((message, ran))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// How `land` puts the attempt on the branch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LandMode {
    /// Rebase onto the branch if it moved, then fast-forward it to one commit.
    #[default]
    Squash,
    /// A merge commit of the branch tip and the attempt's own commit.
    Merge,
}

#[derive(Clone, Debug, Default)]
pub struct LandOptions {
    /// Local branch to land on. Defaults to the checked-out branch.
    pub onto: Option<String>,
    pub mode: LandMode,
    /// Commit message. Defaults to the task goal.
    pub message: Option<String>,
    /// Extra trailers, added to those in `agentvc config pr.trailer`.
    pub trailers: Vec<String>,
    /// See [`ExportOptions::verify`].
    pub verify: Option<bool>,
    /// Who lands, for the operation log. Defaults to the attempt's actor.
    pub actor: Option<String>,
}

/// Where an attempt landed.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Landing {
    pub branch: String,
    pub commit: String,
    pub mode: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct LandResult {
    pub attempt: i64,
    pub branch: String,
    pub mode: LandMode,
    /// The commit the branch points at now.
    pub commit: String,
    /// Where the branch was before.
    pub previous: String,
    /// The attempt was rebased onto the branch first (it had moved).
    pub rebased: bool,
    /// The branch is checked out in the host, and its index and working
    /// tree moved along.
    pub checkout_updated: bool,
    /// Git hooks that ran and passed (`--verify`).
    pub hooks: Vec<String>,
}

/// Land attempt `attempt_id` on a local branch; see the module docs.
pub fn land(space: &Space, attempt_id: i64, opts: &LandOptions) -> Result<LandResult> {
    let repo = HostRepo::open(space.workdir())?;
    let attempt = space.attempt(attempt_id)?;
    let checked = repo.checked_out_branch();
    let branch = match &opts.onto {
        Some(b) => b.clone(),
        None => checked.clone().ok_or(Error::DetachedHead)?,
    };
    if repo
        .git(&["check-ref-format", "--branch", &branch], None)
        .is_err()
        || branch.starts_with('-')
    {
        return Err(Error::BadBranch(branch));
    }
    let refname = format!("refs/heads/{branch}");
    let tip = repo.rev(&refname).ok_or_else(|| Error::NotFound {
        kind: "branch",
        id: branch.clone(),
    })?;
    let checked_out = checked.as_deref() == Some(branch.as_str());
    let mut trailers = space.config_values(crate::config::PR_TRAILER)?;
    trailers.extend(opts.trailers.iter().cloned());
    let actor = opts.actor.clone().unwrap_or_else(|| attempt.actor.clone());

    let mut rebased = false;
    let (commit, made) = match opts.mode {
        LandMode::Squash => {
            // Build on the tip: rebase when the attempt is based elsewhere.
            // A conflict fails here, before anything changed.
            let head = space.head_snapshot(attempt_id)?;
            if space.git_base_commit(&head)?.as_deref() != Some(tip.as_str()) {
                let r = rebase::rebase(
                    space,
                    attempt_id,
                    &RebaseOptions {
                        onto: Some(tip.clone()),
                        no_fetch: true,
                        actor: Some(actor.clone()),
                        ..Default::default()
                    },
                )?;
                rebased = r.rebased;
            }
            let attempt = space.attempt(attempt_id)?;
            let made = export::make_commit(
                space,
                &repo,
                &attempt,
                &ExportOptions {
                    branch: branch.clone(),
                    base: Some(tip.clone()),
                    message: opts.message.clone(),
                    trailers,
                    verify: opts.verify,
                    ..Default::default()
                },
            )?;
            (made.commit.clone(), made)
        }
        LandMode::Merge => {
            let made = export::make_commit(
                space,
                &repo,
                &attempt,
                &ExportOptions {
                    branch: branch.clone(),
                    message: opts.message.clone(),
                    trailers,
                    verify: opts.verify,
                    ..Default::default()
                },
            )?;
            let merge = if repo.is_ancestor(&tip, &made.commit) {
                // Nothing to merge: the attempt already builds on the tip.
                made.commit.clone()
            } else {
                merge_commit(&repo, attempt_id, &branch, &tip, &made.commit)?
            };
            (merge, made)
        }
    };
    export::move_branch(
        &repo,
        &branch,
        Some(&tip),
        &commit,
        checked_out,
        &format!("agentvc land a{attempt_id}"),
    )?;
    record_export(space, attempt_id, &made, &branch)?;
    space.record(
        &actor,
        "attempt.land",
        json!({
            "attempt": attempt_id, "branch": branch, "commit": commit,
            "previous": tip, "mode": opts.mode, "rebased": rebased,
        }),
    )?;
    Ok(LandResult {
        attempt: attempt_id,
        branch,
        mode: opts.mode,
        commit,
        previous: tip,
        rebased,
        checkout_updated: checked_out,
        hooks: made.hooks,
    })
}

fn record_export(space: &Space, attempt: i64, made: &export::Made, branch: &str) -> Result<()> {
    space.db.execute(
        "INSERT INTO exports (attempt_id, snapshot_id, branch, commit_id, parent, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            attempt,
            made.snapshot.id,
            branch,
            made.commit,
            made.parent,
            crate::space::now()
        ],
    )?;
    Ok(())
}

/// A merge commit of `tip` and `commit`, or `conflict` with the paths.
fn merge_commit(
    repo: &HostRepo,
    attempt: i64,
    branch: &str,
    tip: &str,
    commit: &str,
) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo.workdir)
        .args([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            tip,
            commit,
        ])
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .output()
        .at("git")?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = stdout.lines();
    let tree = lines.next().unwrap_or("").trim().to_string();
    match out.status.code() {
        Some(0) => {}
        Some(1) => {
            return Err(Error::Conflict {
                attempt,
                onto: branch.to_string(),
                paths: lines
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect(),
            })
        }
        _ => {
            return Err(Error::Git {
                cmd: "merge-tree".into(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
    }
    let msg = format!("Merge attempt a{attempt} into {branch}\n");
    let mut env = Vec::new();
    if repo.git(&["config", "user.email"], None).is_err() {
        env.extend([
            ("GIT_AUTHOR_NAME", "agentvc"),
            ("GIT_AUTHOR_EMAIL", "agentvc@localhost"),
            ("GIT_COMMITTER_NAME", "agentvc"),
            ("GIT_COMMITTER_EMAIL", "agentvc@localhost"),
        ]);
    }
    repo.git_env(
        &["commit-tree", &tree, "-p", tip, "-p", commit, "-F", "-"],
        Some(&msg),
        &env,
    )
}

impl Space {
    /// Where an attempt was last landed, if it was.
    pub fn landing(&self, attempt: i64) -> Result<Option<Landing>> {
        let mut stmt = self.db.prepare(
            "SELECT payload, created_at FROM operations WHERE kind = 'attempt.land' ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (payload, created_at) = row?;
            let Ok(p) = serde_json::from_str::<Value>(&payload) else {
                continue;
            };
            if p["attempt"].as_i64() != Some(attempt) {
                continue;
            }
            let s = |k: &str| p[k].as_str().unwrap_or("").to_string();
            return Ok(Some(Landing {
                branch: s("branch"),
                commit: s("commit"),
                mode: s("mode"),
                created_at,
            }));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::ForkRequest;

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

    fn repo() -> (crate::testutil::TestDir, Space, String) {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        git(w, &["init", "-q", "-b", "main"]);
        fs::write(w.join("a.txt"), "one\n").unwrap();
        fs::write(w.join("b.txt"), "bee\n").unwrap();
        git(w, &["add", "-A"]);
        git(w, &["commit", "-q", "-m", "base"]);
        let head = git(w, &["rev-parse", "HEAD"]);
        let space = Space::init(w).unwrap();
        (tmp, space, head)
    }

    /// A fork that changed a.txt.
    fn attempt(space: &Space) -> Attempt {
        let t = space.create_task("change a", None).unwrap();
        let a = space
            .fork(&ForkRequest {
                task_id: Some(t.id),
                actor: "agent".into(),
                ..Default::default()
            })
            .unwrap()
            .attempt;
        fs::write(a.view_path.join("a.txt"), "two\n").unwrap();
        a
    }

    fn hook(w: &Path, name: &str, body: &str) {
        let p = w.join(".git/hooks").join(name);
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn land_fast_forwards_a_clean_checkout() {
        let (tmp, space, head) = repo();
        let w = tmp.path();
        let a = attempt(&space);
        let r = land(&space, a.id, &LandOptions::default()).unwrap();
        assert_eq!(r.branch, "main");
        assert_eq!(r.previous, head);
        assert!(!r.rebased && r.checkout_updated);
        assert_eq!(git(w, &["rev-parse", "main"]), r.commit);
        assert_eq!(git(w, &["rev-parse", "main^"]), head);
        assert_eq!(fs::read_to_string(w.join("a.txt")).unwrap(), "two\n");
        assert_eq!(
            git(w, &["status", "--porcelain", "--untracked-files=no"]),
            ""
        );
        let l = space.landing(a.id).unwrap().unwrap();
        assert_eq!(
            (l.branch.as_str(), l.commit.as_str()),
            ("main", r.commit.as_str())
        );
        assert_eq!(space.attempt_status(a.id).unwrap().landed, Some(l));
    }

    #[test]
    fn land_rebases_when_the_branch_moved_and_keeps_unrelated_local_edits() {
        let (tmp, space, _) = repo();
        let w = tmp.path();
        let a = attempt(&space);
        fs::write(w.join("c.txt"), "new on main\n").unwrap();
        git(w, &["add", "c.txt"]);
        git(w, &["commit", "-q", "-m", "main moved"]);
        let tip = git(w, &["rev-parse", "HEAD"]);
        // A local edit the landing does not touch.
        fs::write(w.join("b.txt"), "local\n").unwrap();
        let r = land(&space, a.id, &LandOptions::default()).unwrap();
        assert!(r.rebased);
        assert_eq!(git(w, &["rev-parse", "main^"]), tip);
        assert_eq!(git(w, &["show", "main:c.txt"]), "new on main");
        assert_eq!(git(w, &["show", "main:a.txt"]), "two");
        assert_eq!(fs::read_to_string(w.join("b.txt")).unwrap(), "local\n");
        assert_eq!(
            git(w, &["status", "--porcelain", "--untracked-files=no"]),
            "M b.txt"
        );
    }

    #[test]
    fn land_refuses_conflicts_and_overlapping_local_edits_without_moving() {
        let (tmp, space, _) = repo();
        let w = tmp.path();
        let a = attempt(&space);
        // Local, uncommitted edit to the file the attempt changed.
        fs::write(w.join("a.txt"), "mine\n").unwrap();
        let before = git(w, &["rev-parse", "main"]);
        let e = land(&space, a.id, &LandOptions::default()).unwrap_err();
        assert_eq!(e.code(), "checkout_dirty");
        assert_eq!(git(w, &["rev-parse", "main"]), before);
        assert_eq!(fs::read_to_string(w.join("a.txt")).unwrap(), "mine\n");

        // Committed on main instead: the rebase conflicts.
        git(w, &["commit", "-q", "-am", "main edits a"]);
        let tip = git(w, &["rev-parse", "main"]);
        let e = land(&space, a.id, &LandOptions::default()).unwrap_err();
        assert_eq!(e.code(), "conflict");
        assert_eq!(git(w, &["rev-parse", "main"]), tip);
        assert_eq!(space.landing(a.id).unwrap(), None);
    }

    #[test]
    fn land_merge_and_onto_another_branch() {
        let (tmp, space, head) = repo();
        let w = tmp.path();
        git(w, &["branch", "release"]);
        fs::write(w.join("c.txt"), "c\n").unwrap();
        git(w, &["add", "c.txt"]);
        git(w, &["commit", "-q", "-m", "release moved"]);
        git(w, &["branch", "-f", "release", "HEAD"]);
        git(w, &["reset", "-q", "--hard", &head]);
        let a = attempt(&space);
        let r = land(
            &space,
            a.id,
            &LandOptions {
                onto: Some("release".into()),
                mode: LandMode::Merge,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!r.checkout_updated);
        assert_eq!(
            git(w, &["rev-list", "--count", "--merges", "-1", "release"]),
            "1"
        );
        assert_eq!(git(w, &["show", "release:a.txt"]), "two");
        assert_eq!(git(w, &["show", "release:c.txt"]), "c");
        // The checkout stayed where it was.
        assert_eq!(git(w, &["rev-parse", "main"]), head);
    }

    #[test]
    fn verify_runs_hooks_and_a_failure_moves_nothing() {
        let (tmp, space, head) = repo();
        let w = tmp.path();
        let a = attempt(&space);
        // pre-commit sees the attempt's change staged, as `git commit` would.
        hook(
            w,
            "pre-commit",
            "git diff --cached --name-only > \"$(git rev-parse --git-common-dir)/staged\"\n\
             if git show :a.txt | grep -q two; then echo 'no twos allowed' >&2; exit 3; fi",
        );
        let e = land(
            &space,
            a.id,
            &LandOptions {
                verify: Some(true),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(e.code(), "hook_failed");
        assert!(e.to_string().contains("no twos allowed"), "{e}");
        assert!(e.to_string().contains("pre-commit"), "{e}");
        assert_eq!(
            fs::read_to_string(w.join(".git/staged")).unwrap(),
            "a.txt\n"
        );
        assert_eq!(git(w, &["rev-parse", "main"]), head);

        // A passing pre-commit, and a commit-msg hook that signs off.
        hook(w, "pre-commit", "exit 0");
        hook(w, "commit-msg", "echo 'Signed-off-by: t <t@t>' >> \"$1\"");
        let r = land(
            &space,
            a.id,
            &LandOptions {
                verify: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.hooks, ["pre-commit", "commit-msg"]);
        assert!(git(w, &["log", "-1", "--format=%B", "main"]).ends_with("Signed-off-by: t <t@t>"));
    }

    #[test]
    fn export_verify_follows_the_config() {
        let (tmp, space, _) = repo();
        let w = tmp.path();
        let a = attempt(&space);
        hook(w, "pre-commit", "echo nope >&2; exit 1");
        let opts = ExportOptions {
            branch: "out".into(),
            ..Default::default()
        };
        // Off by default.
        export::export(&space, a.id, &opts).unwrap();
        space
            .set_config(crate::config::EXPORT_VERIFY, &["true".into()])
            .unwrap();
        let e = export::export(
            &space,
            a.id,
            &ExportOptions {
                branch: "out2".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(e.code(), "hook_failed");
        assert!(git(w, &["branch", "--list", "out2"]).is_empty());
        // --no-verify wins over the config.
        export::export(
            &space,
            a.id,
            &ExportOptions {
                branch: "out2".into(),
                verify: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
    }
}
