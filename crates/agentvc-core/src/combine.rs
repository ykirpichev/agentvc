//! Combining attempts (#71): take files or hunks from one attempt into
//! another, or merge a whole attempt into another.
//!
//! The source attempt's changes are what `agentvc diff aY` shows: its head
//! snapshot against its base (the commit it was last rebased onto, else the
//! snapshot it was forked from). The chosen part of them is laid onto that
//! base as a tree, then three-way merged with the target's current view
//! (`git merge-tree`, as `rebase` does), so a file both attempts changed
//! merges line by line and real overlaps are conflicts:
//!
//! - without `keep_conflicts`, nothing changes and the error has code
//!   `conflict` and `paths`;
//! - with it, the files get conflict markers in the view and the target
//!   becomes `exploring`.
//!
//! The result is recorded as a `take` (or `merge`) snapshot of the target
//! whose metadata names the source attempt and snapshot and what was taken,
//! which is how the PR body credits the source. The source is never changed.

use std::collections::{BTreeMap, BTreeSet};

use globset::{Glob, GlobSetBuilder};
use rusqlite::params;
use serde::Serialize;
use serde_json::json;

use crate::error::{Error, Result};
use crate::export::{copy_tree_objects, HostRepo};
use crate::objects::{Mode, ObjectStore, Oid, TreeEntry};
use crate::patch::{self, DEFAULT_CONTEXT};
use crate::rebase::{import_tree, insert_snapshot, merge_trees, write_changes, CatFile};
use crate::resolve::{Resolution, Rules, Sides};
use crate::snapshot::{self, ChangeKind, FileChange};
use crate::space::{Snapshot, Space, Status, ROOT_ATTEMPT, SPACE_DIR};

/// Snapshot triggers recorded by [`take`].
pub const TAKE_TRIGGER: &str = "take";
pub const MERGE_TRIGGER: &str = "merge";

#[derive(Clone, Debug, Default)]
pub struct TakeOptions {
    /// Paths, directories or globs (`src/**/*.rs`) of the source's changed
    /// files to take whole. With no paths and no hunks, everything is taken
    /// (a merge).
    pub paths: Vec<String>,
    /// Single hunks: `file:N` or `file:N,M`, numbered from 1 as in
    /// `agentvc diff aY --patch`.
    pub hunks: Vec<String>,
    pub keep_conflicts: bool,
    /// Report git's conflicts as they are, without resolution strategies.
    pub no_auto_resolve: bool,
    /// Who takes, for the operation log. Defaults to the target's actor.
    pub actor: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TakeResult {
    /// The attempt that received the changes.
    pub into: i64,
    pub source: i64,
    pub source_snapshot: i64,
    /// `take` or `merge`.
    pub mode: &'static str,
    /// The source's changes that were taken (whole files and hunked files).
    pub taken: Vec<FileChange>,
    /// Hunks taken from partially taken files, by path.
    pub hunks: BTreeMap<String, Vec<usize>>,
    /// The snapshot recording the result; `None` when the target already
    /// had all of it.
    pub snapshot: Option<Snapshot>,
    /// Files the take changed in the target's view.
    pub changes: Vec<FileChange>,
    /// Files left with conflict markers (only with `keep_conflicts`).
    pub conflicts: Vec<String>,
    /// Files whose conflicts a resolution strategy resolved (see `resolve`).
    pub resolved: Vec<Resolution>,
    pub status: Status,
}

/// Parse `file:N[,M...]` specs into hunk numbers per file.
fn parse_hunks(specs: &[String]) -> Result<BTreeMap<String, Vec<usize>>> {
    let mut out: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for spec in specs {
        let bad = |reason: &str| Error::BadHunk {
            spec: spec.clone(),
            reason: reason.into(),
        };
        let (path, nums) = spec
            .rsplit_once(':')
            .ok_or_else(|| bad("expected FILE:N, for example src/lib.rs:2"))?;
        if path.is_empty() {
            return Err(bad("the file is empty"));
        }
        let list = out.entry(crate::objects::tree_path(path)).or_default();
        for n in nums.split(',') {
            let n: usize = n
                .trim()
                .parse()
                .map_err(|_| bad("hunk numbers are whole numbers from 1"))?;
            if n == 0 {
                return Err(bad("hunks are numbered from 1"));
            }
            if !list.contains(&n) {
                list.push(n);
            }
        }
        list.sort_unstable();
    }
    Ok(out)
}

/// Whether `path` is selected by one of `patterns`: equal, inside it as a
/// directory, or matching it as a glob.
fn matcher(patterns: &[String]) -> Result<impl Fn(&str) -> bool> {
    let mut b = GlobSetBuilder::new();
    let mut plain = Vec::new();
    for p in patterns {
        let p = crate::objects::tree_path(p)
            .trim_end_matches('/')
            .to_string();
        b.add(Glob::new(&p).map_err(|e| Error::BadHunk {
            spec: p.clone(),
            reason: format!("not a valid path or glob: {e}"),
        })?);
        plain.push(p);
    }
    let set = b.build().map_err(|e| Error::BadHunk {
        spec: patterns.join(" "),
        reason: e.to_string(),
    })?;
    Ok(move |path: &str| {
        set.is_match(path)
            || plain
                .iter()
                .any(|p| path == p || path.starts_with(&format!("{p}/")))
    })
}

/// Write a flat `path -> (mode, blob)` map as a tree.
fn build_tree(store: &ObjectStore, files: &BTreeMap<String, (Mode, Oid)>) -> Result<Oid> {
    fn write(store: &ObjectStore, files: &[(&str, (Mode, Oid))]) -> Result<Oid> {
        let mut entries = Vec::new();
        let mut i = 0;
        while i < files.len() {
            let (path, (mode, oid)) = files[i];
            match path.split_once('/') {
                None => {
                    entries.push(TreeEntry {
                        name: path.to_string(),
                        mode,
                        oid,
                    });
                    i += 1;
                }
                Some((dir, _)) => {
                    let prefix = format!("{dir}/");
                    let mut sub = Vec::new();
                    while i < files.len() && files[i].0.starts_with(&prefix) {
                        sub.push((&files[i].0[prefix.len()..], files[i].1));
                        i += 1;
                    }
                    entries.push(TreeEntry {
                        name: dir.to_string(),
                        mode: Mode::Tree,
                        oid: write(store, &sub)?,
                    });
                }
            }
        }
        store.write_tree(&mut entries)
    }
    // Sorted paths keep every directory's files together.
    let flat: Vec<(&str, (Mode, Oid))> = files.iter().map(|(p, v)| (p.as_str(), *v)).collect();
    write(store, &flat)
}

fn text(store: &ObjectStore, oid: Option<&Oid>) -> Result<Option<String>> {
    match oid {
        None => Ok(Some(String::new())),
        Some(o) => Ok(String::from_utf8(store.read_blob(o)?).ok()),
    }
}

/// The tree an attempt's own changes are relative to, as `diff aN` uses.
fn base_tree(space: &Space, id: i64) -> Result<Option<String>> {
    if let Some(b) = space.own_git_base(id)? {
        return Ok(Some(b.tree));
    }
    match space.attempt(id)?.parent_snapshot {
        Some(p) => Ok(Some(space.snapshot_by_id(p)?.tree)),
        None => Ok(None),
    }
}

/// Take changes from attempt `source` into attempt `into`. See the module
/// docs; with empty `paths` and `hunks` this merges the whole attempt.
pub fn take(space: &Space, into: i64, source: i64, opts: &TakeOptions) -> Result<TakeResult> {
    if into == ROOT_ATTEMPT {
        return Err(Error::RootAttempt);
    }
    if into == source {
        return Err(Error::NothingToTake {
            source_attempt: source,
            reason: "an attempt cannot take from itself".into(),
        });
    }
    let target = space.attempt(into)?;
    if !target.status.is_open() {
        return Err(Error::AttemptClosed {
            id: into,
            status: target.status.to_string(),
        });
    }
    let src_head = space.head_snapshot(source)?;
    let Some(src_base) = base_tree(space, source)? else {
        return Err(Error::NothingToTake {
            source_attempt: source,
            reason: "it has no parent to compare with".into(),
        });
    };
    let store = space.objects();
    let (base_oid, head_oid) = (Oid::from_hex(&src_base)?, Oid::from_hex(&src_head.tree)?);
    let changes = snapshot::diff(store, &base_oid, &head_oid)?;
    if changes.is_empty() {
        return Err(Error::NothingToTake {
            source_attempt: source,
            reason: "it changed nothing".into(),
        });
    }

    let merge_all = opts.paths.is_empty() && opts.hunks.is_empty();
    let mode = if merge_all {
        MERGE_TRIGGER
    } else {
        TAKE_TRIGGER
    };
    let mut hunks = parse_hunks(&opts.hunks)?;
    let whole = matcher(&opts.paths)?;
    let base_files = snapshot::flatten(store, &base_oid)?;
    let head_files = snapshot::flatten(store, &head_oid)?;

    // Lay the chosen changes onto the source's base.
    let mut theirs = base_files.clone();
    let mut taken = Vec::new();
    let changed: BTreeSet<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    for c in &changes {
        if merge_all || whole(&c.path) {
            hunks.remove(&c.path);
            match head_files.get(&c.path) {
                Some(v) => theirs.insert(c.path.clone(), *v),
                None => theirs.remove(&c.path),
            };
            taken.push(c.clone());
        }
    }
    for (path, nums) in &hunks {
        let spec = format!("{path}:{}", join(nums));
        if !changed.contains(path.as_str()) {
            return Err(Error::BadHunk {
                spec,
                reason: format!("a{source} did not change {path}"),
            });
        }
        let old = base_files.get(path);
        let new = head_files.get(path);
        let (Some(old_text), Some(new_text)) = (
            text(store, old.map(|(_, o)| o))?,
            text(store, new.map(|(_, o)| o))?,
        ) else {
            return Err(Error::BadHunk {
                spec,
                reason: "binary files can only be taken whole (--paths)".into(),
            });
        };
        let merged = patch::apply_hunks(&old_text, &new_text, DEFAULT_CONTEXT, nums)
            .map_err(|reason| Error::BadHunk { spec, reason })?;
        if new.is_none() && merged.is_empty() {
            theirs.remove(path);
        } else {
            let mode = new.or(old).map(|(m, _)| *m).unwrap_or(Mode::File);
            theirs.insert(path.clone(), (mode, store.write_blob(merged.as_bytes())?));
        }
        let kind = changes
            .iter()
            .find(|c| &c.path == path)
            .map(|c| c.kind)
            .unwrap_or(ChangeKind::Modified);
        taken.push(FileChange {
            path: path.clone(),
            kind,
        });
    }
    if taken.is_empty() {
        return Err(Error::NothingToTake {
            source_attempt: source,
            reason: format!("none of its changed files match {}", opts.paths.join(" ")),
        });
    }
    taken.sort_by(|a, b| a.path.cmp(&b.path));
    let theirs_oid = build_tree(store, &theirs)?;

    // The target as it is now, captured without recording.
    let view_ok = space.ensure_view(&target)?;
    let target_head = space.head_snapshot(into)?;
    let current = if view_ok {
        let index = space
            .workdir()
            .join(SPACE_DIR)
            .join("index")
            .join(format!("a{into}"));
        snapshot::capture_indexed(store, &target.view_path, Some(&index))?
    } else {
        Oid::from_hex(&target_head.tree)?
    };

    let repo = HostRepo::open(space.workdir())?;
    let objects = repo.objects_dir()?;
    for t in [&base_oid, &current, &theirs_oid] {
        copy_tree_objects(space, t, &objects)?;
    }
    let ident = [
        ("GIT_AUTHOR_NAME", "agentvc"),
        ("GIT_AUTHOR_EMAIL", "agentvc@localhost"),
        ("GIT_COMMITTER_NAME", "agentvc"),
        ("GIT_COMMITTER_EMAIL", "agentvc@localhost"),
    ];
    // merge-tree merges commits: wrap the base tree in a dangling one.
    let base_commit = repo.git_env(
        &["commit-tree", &src_base, "-m", "agentvc take base"],
        None,
        &ident,
    )?;
    let theirs_commit = repo.git_env(
        &[
            "commit-tree",
            &theirs_oid.to_hex(),
            "-p",
            &base_commit,
            "-m",
            &format!("agentvc take from a{source}"),
        ],
        None,
        &ident,
    )?;
    let rules = if opts.no_auto_resolve {
        Rules::disabled()
    } else {
        Rules::from_space(space)?
    };
    let sides = Sides {
        ours: format!("a{into}"),
        theirs: format!("a{source}"),
        theirs_first: false,
    };
    let merge = merge_trees(
        &repo,
        &base_commit,
        &current.to_hex(),
        &theirs_commit,
        &rules,
        &sides,
    )?;
    let resolved = merge.resolved();
    let keep = !merge.conflicts.is_empty();
    if keep && !(opts.keep_conflicts && view_ok) {
        return Err(Error::TakeConflict {
            attempt: into,
            source_attempt: source,
            paths: merge.conflicts,
        });
    }
    let merged = Oid::from_hex(&merge.tree)?;
    {
        let mut git = CatFile::start(&repo)?;
        import_tree(store, &mut git, &merged)?;
    }
    let view_changes = snapshot::diff(store, &current, &merged)?;
    let actor = opts.actor.clone().unwrap_or_else(|| target.actor.clone());
    let result = |snapshot: Option<Snapshot>, status: Status| TakeResult {
        into,
        source,
        source_snapshot: src_head.id,
        mode,
        taken: taken.clone(),
        hunks: hunks.clone(),
        snapshot,
        changes: view_changes.clone(),
        conflicts: merge.conflicts.clone(),
        resolved: resolved.clone(),
        status,
    };
    if view_changes.is_empty() {
        return Ok(result(None, target.status));
    }
    if view_ok {
        write_changes(store, &target.view_path, &merged, &view_changes)?;
    }
    if current.to_hex() != target_head.tree {
        insert_snapshot(space, into, &current.to_hex(), &format!("pre-{mode}"), None)?;
    }
    let mut meta = json!({
        "source": format!("a{source}"),
        "source_snapshot": format!("s{}", src_head.id),
        "paths": taken.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
    });
    if !hunks.is_empty() {
        meta["hunks"] = json!(hunks);
    }
    if keep && !merge.conflicts.is_empty() {
        meta["conflicts"] = json!(merge.conflicts);
    }
    if !resolved.is_empty() {
        meta["auto_resolved"] = json!(resolved);
    }
    let snap = insert_snapshot(space, into, &merge.tree, mode, Some(meta))?;
    space.record_conflicts(&repo, into, snap.id, mode, &merge, &sides)?;
    if keep && target.status != Status::Exploring {
        space.db.execute(
            "UPDATE attempts SET status = 'exploring', reason = ?1 WHERE id = ?2",
            params![
                format!("{mode} conflicts in {} files", merge.conflicts.len()),
                into
            ],
        )?;
    }
    space.record(
        &actor,
        &format!("attempt.{mode}"),
        json!({
            "attempt": into, "source": source, "source_snapshot": src_head.id,
            "snapshot": snap.id, "paths": taken.iter().map(|c| &c.path).collect::<Vec<_>>(),
            "conflicts": merge.conflicts,
        }),
    )?;
    Ok(result(Some(snap), space.attempt(into)?.status))
}

fn join(nums: &[usize]) -> String {
    nums.iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// The attempts whose work was taken into `attempt` or the attempts on its
/// path, with the paths taken: `(source, source snapshot, mode, paths)`
/// for every `take`/`merge` snapshot of `attempts`, oldest first.
pub fn contributions(space: &Space, attempts: &[i64]) -> Result<Vec<Contribution>> {
    let mut out = Vec::new();
    for &a in attempts {
        for s in space.snapshots(a)? {
            if s.trigger != TAKE_TRIGGER && s.trigger != MERGE_TRIGGER {
                continue;
            }
            let Some(m) = &s.meta else { continue };
            let Some(src) = m["source"].as_str() else {
                continue;
            };
            let paths = m["paths"]
                .as_array()
                .map(|p| {
                    p.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            out.push(Contribution {
                into: a,
                snapshot: s.id,
                source: src.trim_start_matches('a').parse().unwrap_or(0),
                source_snapshot: m["source_snapshot"].as_str().unwrap_or("").to_string(),
                mode: s.trigger.clone(),
                paths,
                hunks: m.get("hunks").cloned(),
            });
        }
    }
    Ok(out)
}

#[derive(Clone, Debug, Serialize)]
pub struct Contribution {
    pub into: i64,
    pub snapshot: i64,
    pub source: i64,
    pub source_snapshot: String,
    pub mode: String,
    pub paths: Vec<String>,
    pub hunks: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::{ForkRequest, Ref};
    use crate::view::BackendChoice;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn lines(n: usize) -> String {
        (1..=n).map(|i| format!("{i}\n")).collect()
    }

    /// A host repo with `lib.rs` (20 numbered lines) and `README.md`, and
    /// two sibling attempts a2 and a3.
    fn setup() -> (crate::testutil::TestDir, Space, i64, i64) {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        git(w, &["init", "-q", "-b", "main"]);
        fs::write(w.join("lib.rs"), lines(20)).unwrap();
        fs::write(w.join("README.md"), "readme\n").unwrap();
        git(w, &["add", "."]);
        git(w, &["commit", "-q", "-m", "base"]);
        let space = Space::init(w).unwrap();
        let fork = || {
            space
                .fork(&ForkRequest {
                    from: Some(Ref::Attempt(ROOT_ATTEMPT)),
                    actor: "agent:t".into(),
                    backend: BackendChoice::Copy,
                    ..Default::default()
                })
                .unwrap()
                .attempt
                .id
        };
        let (a, b) = (fork(), fork());
        (tmp, space, a, b)
    }

    fn view(space: &Space, id: i64) -> std::path::PathBuf {
        space.attempt(id).unwrap().view_path
    }

    fn read(space: &Space, id: i64, f: &str) -> String {
        fs::read_to_string(view(space, id).join(f)).unwrap_or_default()
    }

    #[test]
    fn take_a_file_a_glob_and_a_hunk() {
        let (_tmp, space, a, b) = setup();
        let bv = view(&space, b);
        fs::create_dir_all(bv.join("tests")).unwrap();
        fs::write(bv.join("tests/one.rs"), "t1\n").unwrap();
        fs::write(bv.join("tests/two.rs"), "t2\n").unwrap();
        fs::write(bv.join("README.md"), "better readme\n").unwrap();
        let src = lines(20)
            .replace("2\n", "two\n")
            .replace("18\n", "eighteen\n");
        fs::write(bv.join("lib.rs"), &src).unwrap();
        space.snapshot(b, "work", None).unwrap();
        // a has its own change elsewhere, which must survive.
        fs::write(view(&space, a).join("NOTES.md"), "mine\n").unwrap();

        let r = take(
            &space,
            a,
            b,
            &TakeOptions {
                paths: vec!["README.md".into(), "tests/*.rs".into()],
                hunks: vec!["lib.rs:2".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.mode, TAKE_TRIGGER);
        let taken: Vec<&str> = r.taken.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(
            taken,
            ["README.md", "lib.rs", "tests/one.rs", "tests/two.rs"]
        );
        assert_eq!(read(&space, a, "README.md"), "better readme\n");
        assert_eq!(read(&space, a, "tests/two.rs"), "t2\n");
        assert_eq!(read(&space, a, "NOTES.md"), "mine\n");
        let lib = read(&space, a, "lib.rs");
        assert!(
            lib.contains("eighteen\n") && lib.starts_with("1\n2\n"),
            "{lib}"
        );
        let snap = r.snapshot.unwrap();
        assert_eq!(snap.trigger, "take");
        let m = snap.meta.unwrap();
        assert_eq!(m["source"], format!("a{b}"));
        assert!(m["source_snapshot"].as_str().unwrap().starts_with('s'));
        assert_eq!(m["hunks"]["lib.rs"], json!([2]));
        // The source is untouched, and still closes normally.
        assert_eq!(space.attempt(b).unwrap().status, Status::Exploring);
        assert_eq!(read(&space, b, "lib.rs"), src);
        // The pre-take state (NOTES.md) was kept as its own snapshot.
        assert!(space
            .snapshots(a)
            .unwrap()
            .iter()
            .any(|s| s.trigger == "pre-take"));
        space.set_status(a, Status::Promoted, None).unwrap();
        space
            .set_status(b, Status::Abandoned, Some("parts taken into a2"))
            .unwrap();
        let c = contributions(&space, &[a]).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].source, b);

        // The PR body credits b.
        let winner = space.attempt(a).unwrap();
        let snap = space.head_snapshot(a).unwrap();
        let exp = crate::ExportResult {
            export_id: 1,
            attempt: a,
            snapshot: snap.clone(),
            branch: "agentvc/test".into(),
            commit: "0123456789abcdef".into(),
            parent: None,
            tree: snap.tree.clone(),
            replaced: None,
            checkout_updated: false,
            objects_copied: 0,
            hooks: vec![],
            story: Vec::new(),
        };
        let body = crate::github::pr_body(&space, &winner, &exp).unwrap();
        let section = body.split_once("## Contributors").expect("section").1;
        assert!(
            section.contains(&format!("| a{b} by agent:t | a{a} ")),
            "{body}"
        );
        assert!(section.contains("`lib.rs` (hunks 2)"), "{body}");
    }

    #[test]
    fn merge_combines_edits_to_one_file_and_conflicts_change_nothing() {
        let (_tmp, space, a, b) = setup();
        // Different regions of lib.rs: merges cleanly.
        fs::write(
            view(&space, a).join("lib.rs"),
            lines(20).replace("2\n", "a2\n"),
        )
        .unwrap();
        fs::write(
            view(&space, b).join("lib.rs"),
            lines(20).replace("18\n", "b18\n"),
        )
        .unwrap();
        space.snapshot(b, "work", None).unwrap();
        let r = take(&space, a, b, &TakeOptions::default()).unwrap();
        assert_eq!(r.mode, MERGE_TRIGGER);
        let lib = read(&space, a, "lib.rs");
        assert!(lib.contains("a2\n") && lib.contains("b18\n"), "{lib}");

        // Same line in both: conflict, nothing changes.
        fs::write(view(&space, b).join("README.md"), "b readme\n").unwrap();
        space.snapshot(b, "work", None).unwrap();
        fs::write(view(&space, a).join("README.md"), "a readme\n").unwrap();
        let before = space.snapshots(a).unwrap().len();
        let err = take(&space, a, b, &TakeOptions::default()).unwrap_err();
        assert_eq!(err.code(), "conflict");
        assert_eq!(err.details().unwrap()["paths"], json!(["README.md"]));
        assert_eq!(read(&space, a, "README.md"), "a readme\n");
        assert_eq!(space.snapshots(a).unwrap().len(), before);

        // --keep-conflicts writes markers and reopens the attempt.
        space.set_status(a, Status::Passed, None).unwrap();
        let r = take(
            &space,
            a,
            b,
            &TakeOptions {
                paths: vec!["README.md".into()],
                keep_conflicts: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.conflicts, ["README.md"]);
        assert_eq!(r.status, Status::Exploring);
        assert!(read(&space, a, "README.md").contains("<<<<<<<"));
    }

    #[test]
    fn bad_selections_are_errors() {
        let (_tmp, space, a, b) = setup();
        fs::write(
            view(&space, b).join("lib.rs"),
            lines(20).replace("2\n", "x\n"),
        )
        .unwrap();
        space.snapshot(b, "work", None).unwrap();
        let t = |paths: &[&str], hunks: &[&str]| {
            take(
                &space,
                a,
                b,
                &TakeOptions {
                    paths: paths.iter().map(|s| s.to_string()).collect(),
                    hunks: hunks.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                },
            )
            .unwrap_err()
            .code()
        };
        assert_eq!(t(&["nope/*"], &[]), "nothing_to_take");
        assert_eq!(t(&[], &["lib.rs:9"]), "bad_hunk");
        assert_eq!(t(&[], &["README.md:1"]), "bad_hunk");
        assert_eq!(t(&[], &["lib.rs"]), "bad_hunk");
        assert_eq!(
            take(&space, a, a, &TakeOptions::default())
                .unwrap_err()
                .code(),
            "nothing_to_take"
        );
        assert_eq!(
            take(&space, ROOT_ATTEMPT, b, &TakeOptions::default())
                .unwrap_err()
                .code(),
            "root_attempt"
        );
    }
}
