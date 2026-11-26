//! Story mode (#21): an attempt's work told as a stack of logical changes,
//! exported as a series of commits instead of one squashed commit.
//!
//! A story is an ordered list of *changes*. Each has a stable change id
//! (`c` and 8 letters, like jj's), a title and the cumulative tree after it,
//! all on top of the story's base commit (the commit the attempt is based
//! on). Change k's diff is tree(k-1) -> tree(k); the last change's tree is
//! the attempt's head.
//!
//! - `split` makes a story from snapshot boundaries the user picks: change k
//!   ends at boundary snapshot k, the last change ends at the head.
//! - Work recorded after the story was made is "pending": `absorb` folds each
//!   pending file into the last change that touched it (like `jj absorb`, at
//!   file granularity; files no change touched go to the last change), and
//!   `append` puts it in a new change on top.
//! - When the attempt is rebased, the story is restacked: each change is
//!   cherry-picked onto the new base in order with `git merge-tree`, keeping
//!   its change id and title, so review comments keyed by change id survive.
//! - `export` (and `pr`) write one commit per change, each with an
//!   `Agentvc-Change` trailer, and with `stack` a branch per change.
//! - `check` runs a command against every change in a scratch view forked
//!   from the attempt, so each commit is known to build.

use std::collections::BTreeMap;

use rusqlite::params;
use serde::Serialize;

use crate::error::{Error, Result};
use crate::export::{copy_tree_objects, HostRepo};
use crate::objects::{Mode, Oid};
use crate::snapshot::{self, FileChange};
use crate::space::{now, Ref, Space, Status};
use crate::sync::CatFile;

pub const CHANGE_TRAILER: &str = "Agentvc-Change";

#[derive(Clone, Debug, Serialize)]
pub struct Change {
    pub change_id: String,
    /// 1-based position in the story.
    pub position: i64,
    pub title: String,
    /// Cumulative tree after this change.
    pub tree: String,
    /// The snapshot this change ended at when it was made.
    pub source_snapshot: Option<i64>,
    /// Files this change touches (tree of the previous change -> this one).
    pub files: Vec<FileChange>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Story {
    pub attempt: i64,
    pub base_commit: String,
    pub base_tree: String,
    pub changes: Vec<Change>,
    /// Files that differ between the attempt's head and the last change:
    /// work recorded after the story was made. `absorb` or `append` them.
    pub pending: Vec<FileChange>,
    /// True when this call restacked the story onto a new base.
    pub restacked: bool,
}

/// One change as given to [`Space::story_split`]: where it ends, and its
/// title.
#[derive(Clone, Debug)]
pub struct Boundary {
    pub snapshot: Ref,
    pub title: String,
}

fn new_change_id() -> String {
    // Letters only, so a change id never looks like a ref (aN, sN).
    let mut seed = std::collections::hash_map::RandomState::new();
    let mut out = String::from("c");
    let mut bits: u64 = 0;
    for i in 0..8 {
        if i % 4 == 0 {
            use std::hash::{BuildHasher, Hasher};
            let mut h = seed.build_hasher();
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            );
            h.write_u32(std::process::id());
            bits = h.finish();
            seed = std::collections::hash_map::RandomState::new();
        }
        out.push((b'k' + (bits % 16) as u8) as char); // k..z, as jj does
        bits /= 16;
    }
    out
}

/// A dangling commit holding `tree`, for `git merge-tree`, which merges
/// commits. No ref points at it.
fn tree_commit(repo: &HostRepo, tree: &str) -> Result<String> {
    repo.git_env(
        &["commit-tree", tree, "-m", "agentvc story"],
        None,
        &[
            ("GIT_AUTHOR_NAME", "agentvc"),
            ("GIT_AUTHOR_EMAIL", "agentvc@localhost"),
            ("GIT_COMMITTER_NAME", "agentvc"),
            ("GIT_COMMITTER_EMAIL", "agentvc@localhost"),
        ],
    )
}

/// The tree of `commit`, copied into the space's store.
fn commit_tree(space: &Space, repo: &HostRepo, git: &mut CatFile, commit: &str) -> Result<Oid> {
    let tree = Oid::from_hex(&repo.git(&["rev-parse", &format!("{commit}^{{tree}}")], None)?)?;
    crate::sync::import_tree(space, git, &tree)?;
    Ok(tree)
}

struct Row {
    change_id: String,
    title: String,
    tree: String,
    source_snapshot: Option<i64>,
}

impl Space {
    /// The base commit and tree of an attempt's head: the commit it was last
    /// rebased onto, else the host HEAD recorded at init.
    fn story_base(
        &self,
        attempt_id: i64,
        repo: &HostRepo,
        git: &mut CatFile,
    ) -> Result<(String, Oid)> {
        let head = self.head_snapshot(attempt_id)?;
        let commit = self
            .git_base_commit(&head)?
            .and_then(|c| repo.rev(&c))
            .ok_or_else(|| Error::NotFound {
                kind: "git base commit of",
                id: format!("a{attempt_id}"),
            })?;
        let tree = commit_tree(self, repo, git, &commit)?;
        Ok((commit, tree))
    }

    fn story_rows(&self, attempt_id: i64) -> Result<Option<(String, String, Vec<Row>)>> {
        let mut stmt = self.db.prepare(
            "SELECT change_id, title, tree, source_snapshot, base_commit, base_tree
             FROM story_changes WHERE attempt_id = ?1 ORDER BY position",
        )?;
        let mut base = None;
        let rows: Vec<Row> = stmt
            .query_map(params![attempt_id], |r| {
                Ok((
                    Row {
                        change_id: r.get(0)?,
                        title: r.get(1)?,
                        tree: r.get(2)?,
                        source_snapshot: r.get(3)?,
                    },
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .map(|x| {
                x.map(|(row, c, t)| {
                    base.get_or_insert((c, t));
                    row
                })
            })
            .collect::<std::result::Result<_, _>>()?;
        Ok(base.map(|(c, t)| (c, t, rows)))
    }

    fn save_story(
        &self,
        attempt_id: i64,
        base_commit: &str,
        base_tree: &Oid,
        rows: &[Row],
    ) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM story_changes WHERE attempt_id = ?1",
            params![attempt_id],
        )?;
        for (i, r) in rows.iter().enumerate() {
            tx.execute(
                "INSERT INTO story_changes (attempt_id, position, change_id, title, tree,
                     base_commit, base_tree, source_snapshot, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    attempt_id,
                    i as i64 + 1,
                    r.change_id,
                    r.title,
                    r.tree,
                    base_commit,
                    base_tree.to_hex(),
                    r.source_snapshot,
                    now()
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Make (or replace) the story of an attempt from snapshot boundaries.
    /// The last change ends at the attempt's head and is titled `last`.
    pub fn story_split(
        &self,
        attempt_id: i64,
        boundaries: &[Boundary],
        last: &str,
    ) -> Result<Story> {
        let attempt = self.attempt(attempt_id)?;
        if attempt.status.is_open() && self.ensure_view(&attempt).unwrap_or(false) {
            self.snapshot(attempt_id, "story", None)?;
        }
        let repo = HostRepo::open(self.workdir())?;
        let mut git = CatFile::start(self.workdir())?;
        let (base_commit, base_tree) = self.story_base(attempt_id, &repo, &mut git)?;
        let head = self.head_snapshot(attempt_id)?;
        let mut rows = Vec::new();
        let mut last_id = 0;
        for b in boundaries {
            let s = self.resolve(b.snapshot)?;
            if s.attempt_id != attempt_id {
                return Err(Error::StoryBoundary(format!(
                    "s{} belongs to a{}, not a{attempt_id}",
                    s.id, s.attempt_id
                )));
            }
            if s.id <= last_id || s.id >= head.id {
                return Err(Error::StoryBoundary(format!(
                    "boundaries must be snapshots of a{attempt_id} in order, before its head s{}; got s{}",
                    head.id, s.id
                )));
            }
            last_id = s.id;
            // A boundary from before a rebase is cherry-picked onto the base.
            let mut tree = s.tree_oid()?.to_hex();
            let its_base = self.git_base_commit(&s)?.and_then(|c| repo.rev(&c));
            if its_base.as_deref() != Some(base_commit.as_str()) {
                let from = its_base.ok_or_else(|| {
                    Error::StoryBoundary(format!("s{} has no git base to move from", s.id))
                })?;
                tree = self.pick(&repo, &mut git, &from, &tree, &base_commit, "boundary")?;
            }
            rows.push(Row {
                change_id: new_change_id(),
                title: b.title.trim().to_string(),
                tree,
                source_snapshot: Some(s.id),
            });
        }
        rows.push(Row {
            change_id: new_change_id(),
            title: last.trim().to_string(),
            tree: head.tree_oid()?.to_hex(),
            source_snapshot: Some(head.id),
        });
        self.save_story(attempt_id, &base_commit, &base_tree, &rows)?;
        self.record(
            &attempt.actor,
            "story.split",
            serde_json::json!({ "attempt": attempt_id, "changes": rows.iter().map(|r| &r.change_id).collect::<Vec<_>>() }),
        )?;
        self.story(attempt_id)?.ok_or(Error::NotFound {
            kind: "story of",
            id: format!("a{attempt_id}"),
        })
    }

    /// Apply the diff `from_commit -> tree` onto commit `onto`; the merged
    /// tree is copied into the store.
    fn pick(
        &self,
        repo: &HostRepo,
        git: &mut CatFile,
        from_commit: &str,
        tree: &str,
        onto: &str,
        what: &str,
    ) -> Result<String> {
        copy_tree_objects(self, &Oid::from_hex(tree)?, &repo.objects_dir()?)?;
        let m = crate::rebase::merge_trees(repo, from_commit, tree, onto)?;
        if !m.conflicts.is_empty() {
            return Err(Error::StoryConflict {
                change: what.to_string(),
                paths: m.conflicts,
            });
        }
        crate::sync::import_tree(self, git, &Oid::from_hex(&m.tree)?)?;
        Ok(m.tree)
    }

    /// The story of an attempt, restacked onto its current base first if
    /// the attempt was rebased since.
    pub fn story(&self, attempt_id: i64) -> Result<Option<Story>> {
        let Some((mut base_commit, base_tree, mut rows)) = self.story_rows(attempt_id)? else {
            return Ok(None);
        };
        let mut base_tree = Oid::from_hex(&base_tree)?;
        let head = self.head_snapshot(attempt_id)?;
        let mut restacked = false;
        let current = match HostRepo::open(self.workdir()) {
            Ok(repo) => self
                .git_base_commit(&head)?
                .and_then(|c| repo.rev(&c))
                .map(|c| (repo, c)),
            Err(_) => None,
        };
        if let Some((repo, new_base)) = current {
            if new_base != base_commit {
                let mut git = CatFile::start(self.workdir())?;
                let new_tree = commit_tree(self, &repo, &mut git, &new_base)?;
                // Cherry-pick each change onto the restacked previous one.
                let mut prev_old = tree_commit(&repo, &base_tree.to_hex())?;
                copy_tree_objects(self, &base_tree, &repo.objects_dir()?)?;
                let mut prev_new = new_base.clone();
                let n = rows.len();
                for (i, r) in rows.iter_mut().enumerate() {
                    if i + 1 == n {
                        break; // the last change ends at the (rebased) head
                    }
                    let old = r.tree.clone();
                    copy_tree_objects(self, &Oid::from_hex(&old)?, &repo.objects_dir()?)?;
                    r.tree =
                        self.pick(&repo, &mut git, &prev_old, &old, &prev_new, &r.change_id)?;
                    prev_old = tree_commit(&repo, &old)?;
                    copy_tree_objects(self, &Oid::from_hex(&r.tree)?, &repo.objects_dir()?)?;
                    prev_new = tree_commit(&repo, &r.tree)?;
                }
                // The last change ends at the head, which the rebase already
                // moved; work after the story stays pending, not folded in.
                if let Some(last) = rows.last_mut() {
                    if last.source_snapshot == Some(head.id) {
                        last.tree = head.tree_oid()?.to_hex();
                    } else {
                        let old = last.tree.clone();
                        copy_tree_objects(self, &Oid::from_hex(&old)?, &repo.objects_dir()?)?;
                        last.tree = self.pick(
                            &repo,
                            &mut git,
                            &prev_old,
                            &old,
                            &prev_new,
                            &last.change_id,
                        )?;
                    }
                }
                base_commit = new_base;
                base_tree = new_tree;
                self.save_story(attempt_id, &base_commit, &base_tree, &rows)?;
                restacked = true;
            }
        }
        let mut changes = Vec::new();
        let mut prev = base_tree;
        for (i, r) in rows.iter().enumerate() {
            let t = Oid::from_hex(&r.tree)?;
            changes.push(Change {
                change_id: r.change_id.clone(),
                position: i as i64 + 1,
                title: r.title.clone(),
                tree: r.tree.clone(),
                source_snapshot: r.source_snapshot,
                files: snapshot::diff(self.objects(), &prev, &t)?,
            });
            prev = t;
        }
        let pending = match head.tree_oid() {
            Ok(h) => snapshot::diff(self.objects(), &prev, &h)?,
            Err(_) => Vec::new(),
        };
        Ok(Some(Story {
            attempt: attempt_id,
            base_commit,
            base_tree: base_tree.to_hex(),
            changes,
            pending,
            restacked,
        }))
    }

    /// Give a fork its parent's story, when it was forked at or after the
    /// story's last change: its own work then shows as pending, ready to
    /// absorb. A fork from before the story starts without one.
    pub(crate) fn inherit_story(&self, parent: i64, from_snapshot: i64, child: i64) -> Result<()> {
        let last: Option<i64> = self.db.query_row(
            "SELECT max(source_snapshot) FROM story_changes WHERE attempt_id = ?1",
            params![parent],
            |r| r.get(0),
        )?;
        if last.is_some_and(|l| from_snapshot >= l) {
            self.db.execute(
                "INSERT INTO story_changes (attempt_id, position, change_id, title, tree,
                     base_commit, base_tree, source_snapshot, created_at)
                 SELECT ?2, position, change_id, title, tree, base_commit, base_tree,
                     source_snapshot, ?3
                 FROM story_changes WHERE attempt_id = ?1",
                params![parent, child, now()],
            )?;
        }
        Ok(())
    }

    /// [`Space::story`] after snapshotting an open view, so work not yet
    /// recorded shows as pending.
    pub fn story_now(&self, attempt_id: i64) -> Result<Option<Story>> {
        let a = self.attempt(attempt_id)?;
        if a.status.is_open() && self.ensure_view(&a).unwrap_or(false) {
            self.snapshot(attempt_id, "story", None)?;
        }
        self.story(attempt_id)
    }

    fn require_story(&self, attempt_id: i64) -> Result<Story> {
        self.story_now(attempt_id)?.ok_or(Error::NotFound {
            kind: "story of",
            id: format!("a{attempt_id}"),
        })
    }

    /// Fold pending work into the changes: each pending file goes into the
    /// last change that touched it, or into the last change if none did.
    /// Returns the story and, per change id, the files absorbed into it.
    pub fn story_absorb(&self, attempt_id: i64) -> Result<(Story, BTreeMap<String, Vec<String>>)> {
        let story = self.require_story(attempt_id)?;
        let head = self.head_snapshot(attempt_id)?.tree_oid()?;
        let final_files = snapshot::flatten(self.objects(), &head)?;
        let mut trees: Vec<BTreeMap<String, (Mode, Oid)>> = story
            .changes
            .iter()
            .map(|c| snapshot::flatten(self.objects(), &Oid::from_hex(&c.tree)?))
            .collect::<Result<_>>()?;
        let n = trees.len();
        let mut into: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for p in &story.pending {
            let k = story
                .changes
                .iter()
                .rposition(|c| c.files.iter().any(|f| f.path == p.path))
                .unwrap_or(n - 1);
            for t in trees.iter_mut().skip(k) {
                match final_files.get(&p.path) {
                    Some(v) => {
                        t.insert(p.path.clone(), *v);
                    }
                    None => {
                        t.remove(&p.path);
                    }
                }
            }
            into.entry(story.changes[k].change_id.clone())
                .or_default()
                .push(p.path.clone());
        }
        let rows: Vec<Row> = story
            .changes
            .iter()
            .zip(&trees)
            .map(|(c, t)| -> Result<Row> {
                Ok(Row {
                    change_id: c.change_id.clone(),
                    title: c.title.clone(),
                    tree: snapshot::tree_from_files(self.objects(), t)?.to_hex(),
                    source_snapshot: c.source_snapshot,
                })
            })
            .collect::<Result<_>>()?;
        self.save_story(
            attempt_id,
            &story.base_commit,
            &Oid::from_hex(&story.base_tree)?,
            &rows,
        )?;
        self.record(
            "agentvc",
            "story.absorb",
            serde_json::json!({ "attempt": attempt_id, "absorbed": into }),
        )?;
        let story = self.story(attempt_id)?.expect("story exists");
        Ok((story, into))
    }

    /// Put pending work into a new change on top.
    pub fn story_append(&self, attempt_id: i64, title: &str) -> Result<Story> {
        let story = self.require_story(attempt_id)?;
        if story.pending.is_empty() {
            return Ok(story);
        }
        let head = self.head_snapshot(attempt_id)?;
        let mut rows = self.story_rows(attempt_id)?.expect("story exists").2;
        rows.push(Row {
            change_id: new_change_id(),
            title: title.trim().to_string(),
            tree: head.tree_oid()?.to_hex(),
            source_snapshot: Some(head.id),
        });
        self.save_story(
            attempt_id,
            &story.base_commit,
            &Oid::from_hex(&story.base_tree)?,
            &rows,
        )?;
        Ok(self.story(attempt_id)?.expect("story exists"))
    }

    /// Rename a change.
    pub fn story_retitle(&self, attempt_id: i64, change_id: &str, title: &str) -> Result<Story> {
        let n = self.db.execute(
            "UPDATE story_changes SET title = ?1 WHERE attempt_id = ?2 AND change_id = ?3",
            params![title.trim(), attempt_id, change_id],
        )?;
        if n == 0 {
            return Err(Error::NotFound {
                kind: "change",
                id: change_id.to_string(),
            });
        }
        self.require_story(attempt_id)
    }

    /// Forget the story; export goes back to one commit.
    pub fn story_clear(&self, attempt_id: i64) -> Result<bool> {
        Ok(self.db.execute(
            "DELETE FROM story_changes WHERE attempt_id = ?1",
            params![attempt_id],
        )? > 0)
    }

    /// The story to export: restacked, with nothing pending. `None` when
    /// the attempt has no story.
    pub fn story_for_export(&self, attempt_id: i64) -> Result<Option<Story>> {
        let Some(story) = self.story(attempt_id)? else {
            return Ok(None);
        };
        if !story.pending.is_empty() {
            return Err(Error::StoryStale {
                attempt: attempt_id,
                paths: story.pending.iter().map(|p| p.path.clone()).collect(),
            });
        }
        Ok(Some(story))
    }
}

/// Result of [`check`]: the check command's outcome at each change.
#[derive(Clone, Debug, Serialize)]
pub struct CheckResult {
    pub change_id: String,
    pub title: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    /// The last lines of the command's output.
    pub tail: String,
}

/// Run `command` (with `sh -c`) on every change of the story, in a scratch
/// view forked from the attempt (so build output is warm), writing each
/// change's tree into it in turn. The scratch attempt is abandoned
/// afterwards with the results as its reason.
pub fn check(space: &Space, attempt_id: i64, command: &str) -> Result<Vec<CheckResult>> {
    let story = space.require_story(attempt_id)?;
    let a = space.attempt(attempt_id)?;
    let fork = space.fork(&crate::space::ForkRequest {
        task_id: a.task_id,
        from: Some(Ref::Attempt(attempt_id)),
        actor: "agentvc:story-check".into(),
        prompt: Some(format!("story check: {command}")),
        ..Default::default()
    })?;
    let view = fork.attempt.view_path.clone();
    let mut current = space.head_snapshot(fork.attempt.id)?.tree_oid()?;
    let mut out = Vec::new();
    for c in &story.changes {
        let target = Oid::from_hex(&c.tree)?;
        let changes = snapshot::diff(space.objects(), &current, &target)?;
        crate::rebase::write_changes(space.objects(), &view, &target, &changes)?;
        current = target;
        let run = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&view)
            .stdin(std::process::Stdio::null())
            .output();
        let (code, text) = match run {
            Ok(o) => {
                let mut t = String::from_utf8_lossy(&o.stdout).into_owned();
                t += &String::from_utf8_lossy(&o.stderr);
                (o.status.code(), t)
            }
            Err(e) => (None, e.to_string()),
        };
        let lines: Vec<&str> = text.lines().collect();
        let tail = lines[lines.len().saturating_sub(20)..].join("\n");
        out.push(CheckResult {
            change_id: c.change_id.clone(),
            title: c.title.clone(),
            passed: code == Some(0),
            exit_code: code,
            tail,
        });
    }
    let summary: Vec<String> = out
        .iter()
        .map(|r| format!("{} {}", r.change_id, if r.passed { "ok" } else { "FAILED" }))
        .collect();
    let _ = space.set_status_as(
        "agentvc:story-check",
        fork.attempt.id,
        Status::Abandoned,
        Some(&format!(
            "story check of a{attempt_id}: {}",
            summary.join(", ")
        )),
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{export, ExportOptions};
    use crate::space::ForkRequest;
    use crate::view::BackendChoice;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

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

    /// A space on a one-commit repo, and an attempt with two snapshots:
    /// s(a) adds a.txt, the head adds b.txt.
    fn setup() -> (crate::testutil::TestDir, Space, String, i64, i64) {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        git(w, &["init", "-q", "-b", "main"]);
        fs::write(w.join("README.md"), "# demo\n").unwrap();
        git(w, &["add", "-A"]);
        git(w, &["commit", "-q", "-m", "base"]);
        let c0 = git(w, &["rev-parse", "HEAD"]);
        let space = Space::init(w).unwrap();
        let a = space
            .fork(&ForkRequest {
                actor: "agent:test".into(),
                backend: BackendChoice::Copy,
                ..Default::default()
            })
            .unwrap()
            .attempt;
        fs::write(a.view_path.join("a.txt"), "a1\n").unwrap();
        let s1 = space.snapshot(a.id, "tool", None).unwrap().snapshot.id;
        fs::write(a.view_path.join("b.txt"), "b1\n").unwrap();
        space.snapshot(a.id, "tool", None).unwrap();
        (tmp, space, c0, a.id, s1)
    }

    fn split(space: &Space, a: i64, s1: i64) -> Story {
        space
            .story_split(
                a,
                &[Boundary {
                    snapshot: Ref::Snapshot(s1),
                    title: "Add a".into(),
                }],
                "Add b",
            )
            .unwrap()
    }

    fn export_to(space: &Space, a: i64, branch: &str, stack: bool) -> crate::export::ExportResult {
        export(
            space,
            a,
            &ExportOptions {
                branch: branch.into(),
                stack,
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn files_in(w: &Path, commit: &str) -> String {
        git(
            w,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", commit],
        )
    }

    #[test]
    fn split_exports_one_commit_per_change() {
        let (tmp, space, c0, a, s1) = setup();
        let w = tmp.path();
        let st = split(&space, a, s1);
        assert_eq!(st.changes.len(), 2);
        assert!(st.pending.is_empty());
        assert_eq!(st.base_commit, c0);
        for c in &st.changes {
            assert_eq!(c.change_id.len(), 9);
            assert!(c.change_id[1..].bytes().all(|b| (b'k'..=b'z').contains(&b)));
        }
        assert_eq!(st.changes[0].files.len(), 1);
        assert_eq!(st.changes[0].files[0].path, "a.txt");
        assert_eq!(st.changes[1].files[0].path, "b.txt");

        let r = export_to(&space, a, "story", true);
        assert_eq!(r.story.len(), 2);
        assert_eq!(r.story[1].commit, r.commit);
        assert_eq!(r.parent.as_deref(), Some(c0.as_str()));
        assert_eq!(
            git(w, &["log", "--format=%s", "story"]),
            "Add b\nAdd a\nbase"
        );
        assert_eq!(files_in(w, &r.story[0].commit), "a.txt");
        assert_eq!(files_in(w, &r.commit), "b.txt");
        assert_eq!(git(w, &["rev-parse", "story^{tree}"]), r.tree);
        let msg = git(w, &["log", "-1", "--format=%B", &r.story[0].commit]);
        assert!(msg.contains(&format!("{CHANGE_TRAILER}: {}", st.changes[0].change_id)));
        // --stack: a branch per change below the top.
        assert_eq!(r.story[0].branch.as_deref(), Some("story-1"));
        assert_eq!(git(w, &["rev-parse", "story-1"]), r.story[0].commit);
        assert!(r.story[1].branch.is_none());

        // --squash ignores the story.
        let one = export(
            &space,
            a,
            &ExportOptions {
                branch: "squashed".into(),
                squash: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(one.story.is_empty());
        assert_eq!(git(w, &["rev-parse", "squashed^"]), c0);
    }

    #[test]
    fn later_work_is_pending_until_absorbed() {
        let (tmp, space, _c0, a, s1) = setup();
        let w = tmp.path();
        let st = split(&space, a, s1);
        let ids: Vec<String> = st.changes.iter().map(|c| c.change_id.clone()).collect();
        let view = space.attempt(a).unwrap().view_path;
        fs::write(view.join("a.txt"), "a2 (review fix)\n").unwrap();
        fs::write(view.join("c.txt"), "new\n").unwrap();
        space.snapshot(a, "tool", None).unwrap();

        let st = space.story(a).unwrap().unwrap();
        let pending: Vec<&str> = st.pending.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(pending, ["a.txt", "c.txt"]);
        let e = export(
            &space,
            a,
            &ExportOptions {
                branch: "x".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(e.code(), "story_stale");

        let (st, into) = space.story_absorb(a).unwrap();
        assert!(st.pending.is_empty());
        assert_eq!(into[&ids[0]], ["a.txt"]);
        assert_eq!(into[&ids[1]], ["c.txt"]);
        let now: Vec<String> = st.changes.iter().map(|c| c.change_id.clone()).collect();
        assert_eq!(now, ids);

        // gc keeps the absorbed trees, which no snapshot holds.
        crate::gc::gc(
            &space,
            &crate::gc::GcOptions {
                prune_expire: Some(std::time::Duration::ZERO),
                ..Default::default()
            },
        )
        .unwrap();
        let r = export_to(&space, a, "story", false);
        assert_eq!(
            git(w, &["show", &format!("{}:a.txt", r.story[0].commit)]),
            "a2 (review fix)"
        );
        assert_eq!(files_in(w, &r.commit), "b.txt\nc.txt");

        // Append instead: a new change on top.
        fs::write(view.join("d.txt"), "d\n").unwrap();
        let st = space.story_append(a, "Add d").unwrap();
        assert_eq!(st.changes.len(), 3);
        assert_eq!(st.changes[2].files[0].path, "d.txt");
        // A follow-up fork carries the story, same change ids.
        let f = space
            .fork(&ForkRequest {
                from: Some(Ref::Attempt(a)),
                actor: "agent:test".into(),
                backend: BackendChoice::Copy,
                ..Default::default()
            })
            .unwrap()
            .attempt;
        let inherited = space.story(f.id).unwrap().unwrap();
        assert_eq!(inherited.changes.len(), 3);
        assert_eq!(inherited.changes[0].change_id, ids[0]);
        assert!(inherited.pending.is_empty());
        let st = space.story_retitle(a, &ids[0], "Add a, reviewed").unwrap();
        assert_eq!(st.changes[0].title, "Add a, reviewed");
        assert!(space.story_clear(a).unwrap());
        assert!(space.story(a).unwrap().is_none());
    }

    #[test]
    fn rebase_restacks_and_keeps_change_ids() {
        let (tmp, space, c0, a, s1) = setup();
        let w = tmp.path();
        let ids: Vec<String> = split(&space, a, s1)
            .changes
            .into_iter()
            .map(|c| c.change_id)
            .collect();
        fs::write(w.join("README.md"), "# demo\n\nfrom main\n").unwrap();
        git(w, &["commit", "-qam", "main moves on"]);
        let c1 = git(w, &["rev-parse", "HEAD"]);
        crate::rebase::rebase(
            &space,
            a,
            &crate::RebaseOptions {
                onto: Some("main".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let st = space.story(a).unwrap().unwrap();
        assert!(st.restacked);
        assert_eq!(st.base_commit, c1);
        assert!(st.pending.is_empty(), "{:?}", st.pending);
        let now: Vec<String> = st.changes.iter().map(|c| c.change_id.clone()).collect();
        assert_eq!(now, ids);
        assert!(!space.story(a).unwrap().unwrap().restacked);

        let r = export_to(&space, a, "story", false);
        assert_eq!(r.parent.as_deref(), Some(c1.as_str()));
        assert_ne!(r.parent.as_deref(), Some(c0.as_str()));
        assert_eq!(files_in(w, &r.story[0].commit), "a.txt");
        assert_eq!(
            git(w, &["show", &format!("{}:README.md", r.story[0].commit)]),
            "# demo\n\nfrom main"
        );
    }

    #[test]
    fn check_runs_at_every_change() {
        let (_tmp, space, _c0, a, s1) = setup();
        split(&space, a, s1);
        let ok = check(&space, a, "test -f a.txt").unwrap();
        assert!(ok.iter().all(|r| r.passed));
        let r = check(&space, a, "test -f b.txt").unwrap();
        assert_eq!(
            r.iter().map(|r| r.passed).collect::<Vec<_>>(),
            [false, true]
        );
        // The scratch forks are closed, and the attempt is untouched.
        let open: Vec<_> = space
            .attempts(None)
            .unwrap()
            .into_iter()
            .filter(|x| x.actor == "agentvc:story-check" && x.status.is_open())
            .collect();
        assert!(open.is_empty());
        assert!(space.story(a).unwrap().unwrap().pending.is_empty());
    }

    #[test]
    fn boundaries_must_be_in_order_and_own() {
        let (_tmp, space, _c0, a, s1) = setup();
        let head = space.head_snapshot(a).unwrap().id;
        let bad = |s: i64| {
            space
                .story_split(
                    a,
                    &[Boundary {
                        snapshot: Ref::Snapshot(s),
                        title: "x".into(),
                    }],
                    "y",
                )
                .unwrap_err()
                .code()
        };
        assert_eq!(bad(head), "bad_boundary");
        assert_eq!(bad(1), "bad_boundary"); // the root attempt's snapshot
        assert!(space.story_split(a, &[], "everything").is_ok());
        let _ = s1;
    }
}
