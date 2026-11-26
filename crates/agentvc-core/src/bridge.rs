//! Bridging the host checkout (the root attempt's working directory) and
//! attempts, for humans who keep working in their normal checkout (#72).
//!
//! - [`fork_from_worktree`]: the checkout's uncommitted work (tracked edits,
//!   deletions and untracked files; ignored files stay out) becomes a new
//!   attempt based on `HEAD`. With `move_changes` the checkout is reset to
//!   `HEAD` afterwards, like `git stash`. The view is cloned from the
//!   checkout, so build output comes along warm.
//! - [`apply`]: write an attempt's changes into the checkout as uncommitted
//!   changes, like `git stash pop` or checking a branch out, refusing with
//!   `dirty_worktree` when that would overwrite local edits.
//!
//! Neither touches the index or the checked-out branch: they write and
//! delete files in the working tree only.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

use rusqlite::params;
use serde::Serialize;

use crate::error::{Error, IoContext, Result};
use crate::export::HostRepo;
use crate::objects::{hash_object, Mode, Oid};
use crate::snapshot;
use crate::space::{now, Attempt, ForkRequest, ForkResult, Ref, Space, ROOT_ATTEMPT};
use crate::sync::CatFile;

#[derive(Clone, Debug, Serialize)]
pub struct WorktreeForkResult {
    pub fork: ForkResult,
    /// The commit the attempt is based on (the checkout's `HEAD`).
    pub base: String,
    /// The snapshot holding the checkout's work.
    pub snapshot: i64,
    /// Paths that differed from `HEAD` and went into the attempt.
    pub paths: Vec<String>,
    /// With `--move`: paths reset to `HEAD` in the checkout.
    pub moved: Vec<String>,
    /// With `--move`: paths left alone because the snapshot does not hold
    /// them (agentvc never captures them, e.g. `.claude/settings.local.json`).
    pub kept: Vec<String>,
    /// Paths with staged changes: the index is not touched, so `git status`
    /// still shows them as staged after `--move`.
    pub staged: Vec<String>,
}

/// One entry of `git status --porcelain -z`.
struct StatusEntry {
    path: String,
    staged: bool,
}

/// Paths that differ from `HEAD` in the index or the working tree, untracked
/// files included and ignored files not, as git sees them.
fn git_status(repo: &HostRepo) -> Result<Vec<StatusEntry>> {
    let out = repo.git_raw(&[
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--no-renames",
    ])?;
    let mut entries = Vec::new();
    for rec in out.split('\0') {
        if rec.len() < 4 {
            continue;
        }
        let x = rec.as_bytes()[0];
        entries.push(StatusEntry {
            path: rec[3..].to_string(),
            staged: x != b' ' && x != b'?',
        });
    }
    Ok(entries)
}

/// The tree of `HEAD`, copied into the space's store.
fn head_tree(space: &Space, repo: &HostRepo) -> Result<(String, Oid)> {
    let head = repo.rev("HEAD").ok_or_else(|| Error::NotFound {
        kind: "git revision",
        id: "HEAD".into(),
    })?;
    let tree = Oid::from_hex(&repo.git(&["rev-parse", &format!("{head}^{{tree}}")], None)?)?;
    let mut git = CatFile::start(space.workdir())?;
    crate::sync::import_tree(space, &mut git, &tree)?;
    Ok((head, tree))
}

/// What a path holds on disk: (mode, blob id), or `None` when it is absent.
/// A directory where a file is expected counts as a distinct state.
fn disk_state(path: &Path) -> Result<Option<(Mode, Oid)>> {
    let Ok(m) = fs::symlink_metadata(path) else {
        return Ok(None);
    };
    if m.file_type().is_symlink() {
        let target = fs::read_link(path).at(path)?;
        let bytes = target.as_os_str().as_encoded_bytes().to_vec();
        return Ok(Some((Mode::Symlink, hash_object("blob", &bytes))));
    }
    if m.is_dir() {
        return Ok(Some((Mode::Tree, hash_object("tree", b"dir"))));
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        if m.permissions().mode() & 0o111 != 0 {
            Mode::Executable
        } else {
            Mode::File
        }
    };
    #[cfg(not(unix))]
    let mode = Mode::File;
    let data = fs::read(path).at(path)?;
    Ok(Some((mode, hash_object("blob", &data))))
}

/// Make `rel` under `root` hold `want` (a blob from the store) or nothing.
fn write_path(space: &Space, root: &Path, rel: &str, want: Option<(Mode, Oid)>) -> Result<()> {
    let path = root.join(rel);
    if let Ok(m) = fs::symlink_metadata(&path) {
        if m.is_dir() && !m.file_type().is_symlink() {
            fs::remove_dir_all(&path).at(&path)?;
        } else {
            fs::remove_file(&path).at(&path)?;
        }
    }
    let Some((mode, oid)) = want else {
        // Drop directories the deletion left empty, up to the root.
        let mut dir = path.parent();
        while let Some(d) = dir {
            if d == root || fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).at(parent)?;
    }
    let data = space.objects().read_blob(&oid)?;
    match mode {
        Mode::Symlink => {
            let target = String::from_utf8_lossy(&data).into_owned();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &path).at(&path)?;
            #[cfg(not(unix))]
            fs::write(&path, target).at(&path)?;
        }
        _ => {
            fs::write(&path, &data).at(&path)?;
            #[cfg(unix)]
            if mode == Mode::Executable {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).at(&path)?;
            }
        }
    }
    Ok(())
}

/// Fork a new attempt holding the host checkout's uncommitted work, based
/// on `HEAD`. `req.from` is ignored: the parent is always the root attempt.
pub fn fork_from_worktree(
    space: &Space,
    req: &ForkRequest,
    move_changes: bool,
) -> Result<WorktreeForkResult> {
    let workdir = space.workdir().to_path_buf();
    let repo = HostRepo::open(&workdir)?;
    let status = git_status(&repo)?;
    let (head, head_tree) = head_tree(space, &repo)?;

    let mut req = req.clone();
    req.from = Some(Ref::Attempt(ROOT_ATTEMPT));
    let fork = space.fork(&req)?;
    let id = fork.attempt.id;
    // The attempt's own first snapshot, based on HEAD: `diff aN` then shows
    // exactly the checkout's work, and export parents on HEAD.
    let snap = space
        .snapshot_with(
            id,
            "worktree",
            Some(serde_json::json!({ "from_worktree": head })),
            true,
        )?
        .snapshot;
    space.db.execute(
        "INSERT INTO git_bases (attempt_id, snapshot_id, commit_id, tree, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, snap.id, head, head_tree.to_hex(), now()],
    )?;
    let attempt = space.attempt(id)?;
    let _ = space.link_view_git(&attempt);

    let in_head = snapshot::flatten(space.objects(), &head_tree)?;
    let in_snap = snapshot::flatten(space.objects(), &snap.tree_oid()?)?;
    let mut paths: Vec<String> = status.iter().map(|e| e.path.clone()).collect();
    paths.sort();
    paths.dedup();
    let staged: Vec<String> = status
        .iter()
        .filter(|e| e.staged)
        .map(|e| e.path.clone())
        .collect();

    let (mut moved, mut kept) = (Vec::new(), Vec::new());
    if move_changes {
        for p in &paths {
            // Only reset what the attempt really holds: its snapshot must
            // have the checkout's current state of the path.
            let now_on_disk = disk_state(&workdir.join(p))?;
            let held = match (&now_on_disk, in_snap.get(p)) {
                (None, None) => true,
                (Some(d), Some(s)) => d == s,
                _ => false,
            };
            if !held {
                kept.push(p.clone());
                continue;
            }
            write_path(space, &workdir, p, in_head.get(p).copied())?;
            moved.push(p.clone());
        }
    }
    space.record(
        &req.actor,
        "attempt.from_worktree",
        serde_json::json!({ "attempt": id, "base": head, "paths": paths, "moved": moved }),
    )?;
    let fork = ForkResult { attempt, ..fork };
    Ok(WorktreeForkResult {
        fork,
        base: head,
        snapshot: snap.id,
        paths,
        moved,
        kept,
        staged,
    })
}

#[derive(Clone, Debug, Default)]
pub struct ApplyOptions {
    /// Only these paths (files, or directories and everything under them).
    pub paths: Vec<String>,
    /// Overwrite local changes.
    pub force: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApplyResult {
    pub attempt: i64,
    /// What the changes are relative to: `git:<commit>` or `sN`.
    pub base: String,
    /// The snapshot whose state was applied.
    pub snapshot: i64,
    pub written: Vec<String>,
    pub deleted: Vec<String>,
    /// Paths that already matched the attempt.
    pub unchanged: Vec<String>,
}

fn selected(path: &str, filters: &[String]) -> bool {
    filters.is_empty()
        || filters.iter().any(|f| {
            let f = f.trim_end_matches('/');
            f.is_empty() || path == f || path.starts_with(&format!("{f}/"))
        })
}

/// Write the changes of attempt `id` (against its base) into the host
/// checkout as uncommitted changes.
pub fn apply(space: &Space, id: i64, opts: &ApplyOptions) -> Result<ApplyResult> {
    if id == ROOT_ATTEMPT {
        return Err(Error::RootAttempt);
    }
    let attempt: Attempt = space.attempt(id)?;
    // Pick up unsnapshotted work in an open view first.
    if attempt.status.is_open() && space.ensure_view(&attempt).unwrap_or(false) {
        space.snapshot(id, "apply", None)?;
    }
    let head = space.head_snapshot(id)?;
    let (base_label, base_tree) = match space.own_git_base(id)? {
        Some(b) => (format!("git:{}", b.commit), Oid::from_hex(&b.tree)?),
        None => {
            let p = attempt.parent_snapshot.ok_or(Error::RootAttempt)?;
            (format!("s{p}"), space.snapshot_by_id(p)?.tree_oid()?)
        }
    };
    let target = head.tree_oid()?;
    let changes: Vec<_> = snapshot::diff(space.objects(), &base_tree, &target)?
        .into_iter()
        .filter(|c| selected(&c.path, &opts.paths))
        .collect();
    let in_base = snapshot::flatten(space.objects(), &base_tree)?;
    let in_target = snapshot::flatten(space.objects(), &target)?;

    let workdir = space.workdir().to_path_buf();
    // Paths git considers locally changed (None outside a git repo).
    let dirty: Option<HashSet<String>> = HostRepo::open(&workdir)
        .ok()
        .and_then(|r| git_status(&r).ok())
        .map(|s| s.into_iter().map(|e| e.path).collect());

    let mut plan: BTreeMap<String, Option<(Mode, Oid)>> = BTreeMap::new();
    let mut conflicts = Vec::new();
    let mut unchanged = Vec::new();
    for c in &changes {
        let want = in_target.get(&c.path).copied();
        let base = in_base.get(&c.path).copied();
        let have = disk_state(&workdir.join(&c.path))?;
        if have == want {
            unchanged.push(c.path.clone());
            continue;
        }
        let local_edit = have != base
            && match &dirty {
                Some(d) => {
                    d.contains(&c.path) || d.iter().any(|p| p.starts_with(&format!("{}/", c.path)))
                }
                None => true,
            };
        if local_edit && !opts.force {
            conflicts.push(c.path.clone());
        }
        plan.insert(c.path.clone(), want);
    }
    if !conflicts.is_empty() {
        return Err(Error::DirtyWorktree { paths: conflicts });
    }
    let (mut written, mut deleted) = (Vec::new(), Vec::new());
    for (p, want) in &plan {
        write_path(space, &workdir, p, *want)?;
        if want.is_some() {
            written.push(p.clone());
        } else {
            deleted.push(p.clone());
        }
    }
    space.record(
        &space.human(),
        "attempt.apply",
        serde_json::json!({ "attempt": id, "snapshot": head.id, "written": written, "deleted": deleted }),
    )?;
    Ok(ApplyResult {
        attempt: id,
        base: base_label,
        snapshot: head.id,
        written,
        deleted,
        unchanged,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn repo() -> (crate::testutil::TestDir, Space) {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        git(w, &["init", "-q", "-b", "main"]);
        fs::write(w.join(".gitignore"), "target/\n").unwrap();
        fs::write(w.join("lib.rs"), "1\n").unwrap();
        fs::write(w.join("gone.rs"), "gone\n").unwrap();
        fs::create_dir_all(w.join("src")).unwrap();
        fs::write(w.join("src/a.rs"), "a\n").unwrap();
        git(w, &["add", "-A"]);
        git(w, &["commit", "-qm", "base"]);
        let space = Space::init(w).unwrap();
        (tmp, space)
    }

    fn req() -> ForkRequest {
        ForkRequest {
            actor: "human".into(),
            backend: crate::view::BackendChoice::Copy,
            ..Default::default()
        }
    }

    fn edit(w: &Path) {
        fs::write(w.join("lib.rs"), "2\n").unwrap();
        fs::remove_file(w.join("gone.rs")).unwrap();
        fs::create_dir_all(w.join("src/new")).unwrap();
        fs::write(w.join("src/new/b.rs"), "b\n").unwrap();
        fs::create_dir_all(w.join("target")).unwrap();
        fs::write(w.join("target/build"), "ignored\n").unwrap();
    }

    #[test]
    fn stash_and_pop_round_trip() {
        let (_tmp, space) = repo();
        let w = space.workdir().to_path_buf();
        edit(&w);
        let r = fork_from_worktree(&space, &req(), true).unwrap();
        assert_eq!(r.paths, ["gone.rs", "lib.rs", "src/new/b.rs"]);
        assert_eq!(r.moved, r.paths);
        assert!(r.kept.is_empty() && r.staged.is_empty());
        // The checkout is clean; ignored files stay.
        assert_eq!(git(&w, &["status", "--porcelain"]), "");
        assert_eq!(
            fs::read_to_string(w.join("target/build")).unwrap(),
            "ignored\n"
        );
        assert!(!w.join("src/new").exists());
        // The attempt holds the work, relative to HEAD.
        let a = r.fork.attempt.id;
        let changes = space.own_changes(a).unwrap();
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["gone.rs", "lib.rs", "src/new/b.rs"]);
        let view = &r.fork.attempt.view_path;
        assert_eq!(fs::read_to_string(view.join("lib.rs")).unwrap(), "2\n");
        assert!(view.join("target/build").exists(), "the view starts warm");

        let ap = apply(&space, a, &ApplyOptions::default()).unwrap();
        assert_eq!(ap.written, ["lib.rs", "src/new/b.rs"]);
        assert_eq!(ap.deleted, ["gone.rs"]);
        assert_eq!(fs::read_to_string(w.join("lib.rs")).unwrap(), "2\n");
        assert_eq!(fs::read_to_string(w.join("src/new/b.rs")).unwrap(), "b\n");
        assert!(!w.join("gone.rs").exists());
        // Index and branch untouched: everything is an unstaged change.
        // (git diff --cached is empty when nothing is staged.)
        assert_eq!(git(&w, &["diff", "--cached", "--name-only"]), "");
        assert_eq!(git(&w, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
        // Applying again is a no-op.
        let again = apply(&space, a, &ApplyOptions::default()).unwrap();
        assert!(again.written.is_empty() && again.deleted.is_empty());
        assert_eq!(again.unchanged.len(), 3);
    }

    #[test]
    fn without_move_the_checkout_keeps_its_work() {
        let (_tmp, space) = repo();
        let w = space.workdir().to_path_buf();
        edit(&w);
        let r = fork_from_worktree(&space, &req(), false).unwrap();
        assert!(r.moved.is_empty());
        assert_eq!(fs::read_to_string(w.join("lib.rs")).unwrap(), "2\n");
    }

    #[test]
    fn apply_refuses_to_overwrite_local_edits() {
        let (_tmp, space) = repo();
        let w = space.workdir().to_path_buf();
        let a = space.fork(&req()).unwrap().attempt;
        fs::write(a.view_path.join("lib.rs"), "from attempt\n").unwrap();
        fs::write(a.view_path.join("src/a.rs"), "attempt a\n").unwrap();
        space.snapshot(a.id, "manual", None).unwrap();
        // A local edit to one of the same files.
        fs::write(w.join("lib.rs"), "local\n").unwrap();
        let e = apply(&space, a.id, &ApplyOptions::default()).unwrap_err();
        assert_eq!(e.code(), "dirty_worktree");
        match &e {
            Error::DirtyWorktree { paths } => assert_eq!(paths, &["lib.rs"]),
            other => panic!("{other:?}"),
        }
        assert_eq!(fs::read_to_string(w.join("lib.rs")).unwrap(), "local\n");
        assert_eq!(
            fs::read_to_string(w.join("src/a.rs")).unwrap(),
            "a\n",
            "nothing written"
        );
        // --paths limits what is applied.
        let r = apply(
            &space,
            a.id,
            &ApplyOptions {
                paths: vec!["src".into()],
                force: false,
            },
        )
        .unwrap();
        assert_eq!(r.written, ["src/a.rs"]);
        // --force overwrites.
        let r = apply(
            &space,
            a.id,
            &ApplyOptions {
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.written, ["lib.rs"]);
        assert_eq!(
            fs::read_to_string(w.join("lib.rs")).unwrap(),
            "from attempt\n"
        );
    }

    #[test]
    fn move_leaves_staged_changes_in_the_index() {
        let (_tmp, space) = repo();
        let w = space.workdir().to_path_buf();
        fs::write(w.join("lib.rs"), "staged\n").unwrap();
        git(&w, &["add", "lib.rs"]);
        let r = fork_from_worktree(&space, &req(), true).unwrap();
        assert_eq!(r.staged, ["lib.rs"]);
        assert_eq!(fs::read_to_string(w.join("lib.rs")).unwrap(), "1\n");
        // The staged blob is still in the index, untouched.
        assert_eq!(git(&w, &["show", ":lib.rs"]), "staged");
    }
}
