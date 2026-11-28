//! `agentvc doctor`: check that the database, objects, views, layers, runs
//! and sync state of a space agree, and with `--repair` fix what is safe to
//! fix. The `fsck` of agentvc.
//!
//! Every finding has a stable `code`, a `severity` and says whether
//! `--repair` can fix it. Repairs never delete history: they remount or
//! rewrite views from their latest snapshot, copy missing objects back from
//! the host git repository, mark runs whose process is gone as finished
//! (`ps` shows them as `lost`), remove directories no attempt or layer
//! refers to, and put back a worktree's `stack.json` from its `.bak` when it
//! no longer parses.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::params;
use serde::Serialize;

use crate::error::{Error, Result};
use crate::export::HostRepo;
use crate::objects::{Mode, Oid};
use crate::space::{now, Attempt, Space, ROOT_ATTEMPT, SPACE_DIR};
use crate::sync::CatFile;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    /// Stable code: `db_corrupt`, `missing_object`, `orphan_view`, ...
    pub code: &'static str,
    pub severity: Severity,
    /// What it is about: `a3`, `s12`, `r4`, an object id or a path.
    pub subject: String,
    pub message: String,
    /// Whether `--repair` knows how to fix it.
    pub repairable: bool,
    /// Whether this run fixed it.
    pub repaired: bool,
}

#[derive(Clone, Debug, Default)]
pub struct DoctorOptions {
    pub repair: bool,
    /// Skip checks that talk to the git remote.
    pub offline: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DoctorResult {
    pub findings: Vec<Finding>,
    /// Errors not repaired. The CLI exits non-zero when this is above 0.
    pub errors: usize,
    pub warnings: usize,
    pub repaired: usize,
    pub checked: Checked,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Checked {
    pub attempts: usize,
    pub snapshots: usize,
    pub objects: usize,
    pub runs: usize,
    /// Worktrees whose `stack.json` was checked.
    pub stacks: usize,
}

/// A repair: returns whether it fixed the problem.
type Fix<'a> = &'a mut dyn FnMut(&Space) -> Result<bool>;

/// Findings of one code beyond this are summed up in one more finding.
const MAX_PER_CODE: usize = 50;

struct Report<'a> {
    space: &'a Space,
    repair: bool,
    out: Vec<Finding>,
}

impl Report<'_> {
    /// Add a finding; `fix` runs with `--repair` when it is given, and
    /// returns whether it fixed the problem.
    fn add(
        &mut self,
        code: &'static str,
        severity: Severity,
        subject: impl Into<String>,
        message: impl Into<String>,
        fix: Option<Fix>,
    ) {
        let repairable = fix.is_some();
        let repaired = match fix {
            Some(f) if self.repair => f(self.space).unwrap_or(false),
            _ => false,
        };
        self.out.push(Finding {
            code,
            severity,
            subject: subject.into(),
            message: message.into(),
            repairable,
            repaired,
        });
    }
}

pub fn doctor(space: &Space, opts: &DoctorOptions) -> Result<DoctorResult> {
    let mut r = Report {
        space,
        repair: opts.repair,
        out: Vec::new(),
    };
    let mut checked = Checked::default();
    check_db(&mut r)?;
    check_objects(&mut r, &mut checked)?;
    check_views(&mut r, &mut checked)?;
    check_orphans(&mut r)?;
    check_runs(&mut r, &mut checked)?;
    check_git(&mut r, opts.offline)?;
    check_stacks(&mut r, &mut checked);

    // Cap the noise: a broken store can have thousands of missing objects.
    let mut seen: std::collections::HashMap<&'static str, usize> = Default::default();
    let mut findings = Vec::new();
    let mut extra: std::collections::BTreeMap<&'static str, (Severity, usize)> = Default::default();
    for f in r.out {
        let n = seen.entry(f.code).or_default();
        *n += 1;
        if *n <= MAX_PER_CODE {
            findings.push(f);
        } else {
            let e = extra.entry(f.code).or_insert((f.severity, 0));
            e.1 += 1;
        }
    }
    for (code, (severity, n)) in extra {
        findings.push(Finding {
            code,
            severity,
            subject: "space".into(),
            message: format!("{n} more `{code}` findings not listed"),
            repairable: false,
            repaired: false,
        });
    }
    let errors = findings
        .iter()
        .filter(|f| f.severity == Severity::Error && !f.repaired)
        .count();
    let warnings = findings
        .iter()
        .filter(|f| f.severity == Severity::Warning && !f.repaired)
        .count();
    let repaired = findings.iter().filter(|f| f.repaired).count();
    if opts.repair && repaired > 0 {
        space.record(
            "agentvc",
            "doctor.repair",
            serde_json::json!({
                "repaired": findings.iter().filter(|f| f.repaired)
                    .map(|f| serde_json::json!({ "code": f.code, "subject": f.subject }))
                    .collect::<Vec<_>>(),
            }),
        )?;
    }
    Ok(DoctorResult {
        findings,
        errors,
        warnings,
        repaired,
        checked,
    })
}

// ---- stacks -------------------------------------------------------------------

/// The git dirs of the host repository's worktrees: the main one, then
/// `<common>/worktrees/*`.
fn worktree_git_dirs(space: &Space) -> Vec<PathBuf> {
    let Ok(repo) = HostRepo::open(space.workdir()) else {
        return Vec::new();
    };
    let Ok(common) = repo.git(&["rev-parse", "--git-common-dir"], None) else {
        return Vec::new();
    };
    let common = repo.workdir.join(common.trim());
    let mut dirs = vec![common.clone()];
    if let Ok(rd) = fs::read_dir(common.join("worktrees")) {
        let mut more: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        more.sort();
        dirs.extend(more);
    }
    dirs
}

/// Every worktree's `stack.json` parses (#204); a bad one is restored from
/// its `stack.json.bak` when that is good.
fn check_stacks(r: &mut Report, checked: &mut Checked) {
    use crate::stack::{check_state, restore_backup, StateHealth};
    for git_dir in worktree_git_dirs(r.space) {
        let health = check_state(&git_dir);
        if health == StateHealth::Missing {
            continue;
        }
        checked.stacks += 1;
        let StateHealth::Bad { error, backup } = health else {
            continue;
        };
        let file = git_dir.join("agentvc").join("stack.json");
        let subject = file.display().to_string();
        if backup {
            let mut fix = |_: &Space| restore_backup(&git_dir);
            r.add(
                "bad_stack_state",
                Severity::Error,
                subject.clone(),
                format!("{subject} is not valid ({error}); stack commands use stack.json.bak meanwhile, repair puts it back (the bad file is kept as stack.json.bad)"),
                Some(&mut fix),
            );
        } else {
            r.add(
                "bad_stack_state",
                Severity::Error,
                subject.clone(),
                format!("{subject} is not valid ({error}) and there is no usable stack.json.bak; fix or remove it, then `agentvc stack adopt` the branches again"),
                None,
            );
        }
    }
}

// ---- database ----------------------------------------------------------------

fn check_db(r: &mut Report) -> Result<()> {
    let db = &r.space.db;
    let integrity: Vec<String> = db
        .prepare("PRAGMA integrity_check")?
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    if integrity != ["ok"] {
        r.add(
            "db_corrupt",
            Severity::Error,
            "meta.db",
            format!("SQLite integrity check failed: {}", integrity.join("; ")),
            None,
        );
    }
    let fk: Vec<(String, i64, String)> = db
        .prepare("PRAGMA foreign_key_check")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    for (table, rowid, parent) in fk {
        r.add(
            "db_dangling_row",
            Severity::Error,
            format!("{table}#{rowid}"),
            format!("row {rowid} of {table} points at a missing row of {parent}"),
            None,
        );
    }
    match r.space.meta("format")?.map(|v| v.parse::<u32>()) {
        Some(Ok(v)) if v == crate::FORMAT_VERSION => {}
        Some(Ok(v)) if v > crate::FORMAT_VERSION => r.add(
            "schema_newer",
            Severity::Error,
            "meta.db",
            format!(
                "the space has format {v}, newer than this agentvc ({}); upgrade agentvc",
                crate::FORMAT_VERSION
            ),
            None,
        ),
        other => r.add(
            "schema_version",
            Severity::Error,
            "meta.db",
            format!(
                "the space's format version is {other:?}, expected {}",
                crate::FORMAT_VERSION
            ),
            None,
        ),
    }
    Ok(())
}

// ---- objects -----------------------------------------------------------------

/// Copy an object back from the host repository, if git has it.
fn restore_from_git(space: &Space, git: &mut Option<CatFile>, oid: &Oid) -> Result<bool> {
    let Some(g) = git else { return Ok(false) };
    let Ok((kind, data)) = g.read(oid) else {
        return Ok(false);
    };
    if crate::objects::hash_object(&kind, &data) != *oid {
        return Ok(false);
    }
    space.objects().rewrite(&kind, &data)?;
    Ok(true)
}

fn check_objects(r: &mut Report, checked: &mut Checked) -> Result<()> {
    let space = r.space;
    let store = space.objects();
    store.reload_packs();
    let mut git = HostRepo::open(space.workdir())
        .ok()
        .and_then(|_| CatFile::start(space.workdir()).ok());
    // Roots: every unpruned snapshot, and the trees of rebase bases.
    let mut roots: Vec<(String, String)> = Vec::new();
    for (id, tree) in space
        .db
        .prepare("SELECT id, tree FROM snapshots WHERE pruned_at IS NULL ORDER BY id")?
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    {
        checked.snapshots += 1;
        roots.push((format!("s{id}"), tree));
    }
    for (id, tree) in space
        .db
        .prepare("SELECT snapshot_id, tree FROM git_bases ORDER BY id")?
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    {
        roots.push((format!("s{id} (rebase base)"), tree));
    }
    let mut seen: HashSet<Oid> = HashSet::new();
    for (subject, tree) in roots {
        let Ok(root) = Oid::from_hex(&tree) else {
            r.add(
                "bad_tree_id",
                Severity::Error,
                &subject,
                format!("{subject} records tree {tree:?}, which is not an object id"),
                None,
            );
            continue;
        };
        let mut stack = vec![(root, Mode::Tree)];
        while let Some((oid, mode)) = stack.pop() {
            if !seen.insert(oid) {
                continue;
            }
            checked.objects += 1;
            let mut problem = match store.verify(&oid) {
                Ok(_) => None,
                Err(Error::NotFound { .. }) => Some("missing_object"),
                Err(_) => Some("corrupt_object"),
            };
            if let Some(code) = problem {
                let what = if code == "missing_object" {
                    "is missing"
                } else {
                    "is corrupt (its content does not hash to its id)"
                };
                let mut fixed = false;
                let can = git.is_some();
                let mut fix = |s: &Space| -> Result<bool> {
                    fixed =
                        restore_from_git(s, &mut git, &oid)? && s.objects().verify(&oid).is_ok();
                    Ok(fixed)
                };
                r.add(
                    code,
                    Severity::Error,
                    oid.to_hex(),
                    format!("object {oid}, reachable from {subject}, {what}"),
                    can.then_some(&mut fix as Fix),
                );
                if fixed {
                    problem = None;
                }
            }
            if problem.is_some() || mode != Mode::Tree {
                continue;
            }
            match store.read_tree(&oid) {
                Ok(entries) => stack.extend(entries.into_iter().map(|e| (e.oid, e.mode))),
                Err(_) => r.add(
                    "corrupt_object",
                    Severity::Error,
                    oid.to_hex(),
                    format!("object {oid}, reachable from {subject}, is not a valid tree"),
                    None,
                ),
            }
        }
    }
    Ok(())
}

// ---- views -------------------------------------------------------------------

/// Write an attempt's view out again from its latest snapshot, as a plain
/// directory. Used when its view or overlay upper layer is gone.
fn rematerialize(space: &Space, a: &Attempt) -> Result<bool> {
    let head = space.head_snapshot(a.id)?;
    let tree = head.tree_oid()?;
    #[cfg(target_os = "linux")]
    if space.has_overlay_record(a.id)? && !space.remove_overlay_view(a)? {
        return Ok(false);
    }
    if a.view_path.exists() {
        return Ok(false); // never write over something that is there
    }
    crate::snapshot::materialize(space.objects(), &tree, &a.view_path)?;
    let _ = fs::remove_file(index_path(space, a.id));
    space.db.execute(
        "UPDATE attempts SET backend = 'materialize' WHERE id = ?1",
        params![a.id],
    )?;
    let _ = space.link_view_git(&space.attempt(a.id)?);
    Ok(true)
}

fn index_path(space: &Space, id: i64) -> PathBuf {
    space
        .workdir()
        .join(SPACE_DIR)
        .join("index")
        .join(format!("a{id}"))
}

fn check_views(r: &mut Report, checked: &mut Checked) -> Result<()> {
    let space = r.space;
    for a in space.attempts(None)? {
        checked.attempts += 1;
        if !a.status.is_open() || a.id == ROOT_ATTEMPT {
            continue;
        }
        let subject = format!("a{}", a.id);
        let mut remat = |s: &Space| rematerialize(s, &a);
        if a.backend == crate::gc::REMOVED {
            r.add(
                "missing_view",
                Severity::Error,
                &subject,
                format!("{subject} is open but gc removed its view; repair writes it out from its latest snapshot"),
                Some(&mut remat),
            );
            continue;
        }
        #[cfg(target_os = "linux")]
        if a.backend == crate::view::Backend::Overlay.as_str() {
            if !space.has_overlay_record(a.id)? {
                r.add(
                    "missing_view",
                    Severity::Error,
                    &subject,
                    format!("{subject} is an overlay view with no overlay record; repair writes it out from its latest snapshot"),
                    Some(&mut remat),
                );
                continue;
            }
            let upper = space.overlay_dir(a.id).join("upper");
            if !upper.is_dir() {
                r.add(
                    "missing_view",
                    Severity::Error,
                    &subject,
                    format!(
                        "the upper layer {} of {subject} is gone, so unsnapshotted work is lost; repair writes the view out from its latest snapshot",
                        upper.display()
                    ),
                    Some(&mut remat),
                );
                continue;
            }
            if !crate::overlay::is_mounted(&a.view_path) {
                let mut mount = |s: &Space| s.ensure_view(&a);
                r.add(
                    "unmounted_view",
                    Severity::Warning,
                    &subject,
                    format!(
                        "the overlay view of {subject} is not mounted here (after a reboot, or a userns mount); agentvc remounts it on demand, repair mounts it now"
                    ),
                    Some(&mut mount),
                );
            }
            continue;
        }
        if !a.view_path.is_dir() {
            r.add(
                "missing_view",
                Severity::Error,
                &subject,
                format!(
                    "the view {} of open attempt {subject} is gone; repair writes it out from its latest snapshot",
                    a.view_path.display()
                ),
                Some(&mut remat),
            );
        }
    }
    #[cfg(target_os = "linux")]
    for (a, l) in space.missing_layers()? {
        r.add(
            "missing_layer",
            Severity::Error,
            format!("L{l}"),
            format!(
                "layer L{l} under the overlay view of a{a} is gone; snapshot what is still readable, then `agentvc fork --from a{a}`"
            ),
            None,
        );
    }
    Ok(())
}

// ---- orphans -----------------------------------------------------------------

fn remove_dir(path: &Path) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        crate::overlay::unmount_under(path);
        if crate::overlay::is_mounted(path) {
            return Ok(false);
        }
        crate::overlay::remove_tree(path);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = fs::remove_dir_all(path);
    Ok(!path.exists())
}

fn dir_entries(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<(String, PathBuf)> = rd
        .flatten()
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    v.sort();
    v
}

fn check_orphans(r: &mut Report) -> Result<()> {
    let space = r.space;
    let dir = space.workdir().join(SPACE_DIR);
    let attempts = space.attempts(None)?;
    let live_views: HashSet<PathBuf> = attempts
        .iter()
        .filter(|a| a.backend != crate::gc::REMOVED)
        .map(|a| a.view_path.clone())
        .collect();
    for (name, path) in dir_entries(&dir.join("views")) {
        if live_views.contains(&path) {
            continue;
        }
        // A fork that is being made has a row with backend `pending`.
        let pending = attempts
            .iter()
            .any(|a| a.backend == "pending" && name == format!("a{}", a.id));
        if pending {
            continue;
        }
        let mut fix = |_: &Space| remove_dir(&path);
        r.add(
            "orphan_view",
            Severity::Warning,
            path.display().to_string(),
            format!("views/{name} belongs to no attempt (a crashed fork, or a view gc could not delete); repair deletes it"),
            Some(&mut fix),
        );
    }
    // Per-view git dirs (#68) of attempts that are gone or whose view gc
    // removed. `shared.json` and `hooks/` are shared and stay.
    for (name, path) in dir_entries(&dir.join("git")) {
        let Some(id) = name.strip_prefix('a').and_then(|n| n.parse::<i64>().ok()) else {
            continue;
        };
        let used = attempts
            .iter()
            .any(|a| a.id == id && a.backend != crate::gc::REMOVED);
        if used {
            continue;
        }
        let mut fix = |s: &Space| {
            let _ = s.unlink_view_git(id);
            remove_dir(&path)
        };
        r.add(
            "orphan_view_git",
            Severity::Warning,
            path.display().to_string(),
            format!("git/{name} is the git dir of a view that no longer exists; repair deletes it and its pin ref"),
            Some(&mut fix),
        );
    }
    #[cfg(target_os = "linux")]
    {
        for (name, path) in dir_entries(&dir.join("overlay")) {
            let id = name.strip_prefix('a').and_then(|n| n.parse::<i64>().ok());
            let used = match id {
                Some(id) => space.has_overlay_record(id)?,
                None => false,
            };
            if used {
                continue;
            }
            let mut fix = |_: &Space| remove_dir(&path);
            r.add(
                "orphan_overlay",
                Severity::Warning,
                path.display().to_string(),
                format!("overlay/{name} (upper and work dirs) belongs to no overlay view; repair deletes it"),
                Some(&mut fix),
            );
        }
        let rows: HashSet<i64> = space.layer_rows()?.into_iter().map(|(id, _)| id).collect();
        for (name, path) in dir_entries(&dir.join("layers")) {
            let id = name
                .strip_prefix('L')
                .map(|n| n.strip_suffix(".manifest").unwrap_or(n))
                .and_then(|n| n.parse::<i64>().ok());
            let Some(id) = id else { continue }; // aN.lock files
            if rows.contains(&id) {
                continue;
            }
            let mut fix = |_: &Space| {
                if path.is_dir() {
                    remove_dir(&path)
                } else {
                    Ok(fs::remove_file(&path).is_ok())
                }
            };
            r.add(
                "orphan_layer",
                Severity::Warning,
                path.display().to_string(),
                format!("layers/{name} has no layer record; repair deletes it"),
                Some(&mut fix),
            );
        }
    }
    Ok(())
}

// ---- runs --------------------------------------------------------------------

/// A run with no pid after this long never started its command.
const START_TIMEOUT_SECS: i64 = 600;

fn check_runs(r: &mut Report, checked: &mut Checked) -> Result<()> {
    let space = r.space;
    for run in space.runs(true)? {
        checked.runs += 1;
        let stale = match (run.pid, run.alive) {
            (Some(_), Some(false)) => true,
            (None, _) => now() - run.started_at > START_TIMEOUT_SECS,
            _ => false,
        };
        if !stale {
            continue;
        }
        let id = run.id;
        let mut fix = |s: &Space| -> Result<bool> {
            Ok(s.db.execute(
                "UPDATE runs SET finished_at = ?1 WHERE id = ?2 AND finished_at IS NULL",
                params![now(), id],
            )? > 0)
        };
        r.add(
            "stale_run",
            Severity::Warning,
            format!("r{id}"),
            match run.pid {
                Some(p) => format!("run r{id} of a{} is marked running but process {p} is gone; repair marks it lost", run.attempt_id),
                None => format!("run r{id} of a{} never started its command; repair marks it lost", run.attempt_id),
            },
            Some(&mut fix),
        );
    }
    Ok(())
}

// ---- git: rebase bases and sync ----------------------------------------------

fn check_git(r: &mut Report, offline: bool) -> Result<()> {
    let space = r.space;
    let Ok(repo) = HostRepo::open(space.workdir()) else {
        return Ok(());
    };
    let has_commit = |c: &str| {
        repo.git(&["cat-file", "-e", &format!("{c}^{{commit}}")], None)
            .is_ok()
    };
    let bases: Vec<(i64, String)> = space
        .db
        .prepare("SELECT DISTINCT snapshot_id, commit_id FROM git_bases ORDER BY snapshot_id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let mut missing = HashSet::new();
    for (snap, commit) in bases {
        if missing.contains(&commit) || !has_commit(&commit) {
            missing.insert(commit.clone());
            r.add(
                "missing_git_base",
                Severity::Warning,
                format!("s{snap}"),
                format!(
                    "s{snap} was rebased onto commit {commit}, which the host repository no longer has; `git fetch`, or `agentvc rebase` the attempt again before export"
                ),
                None,
            );
        }
    }

    let Some(last) = space.meta(crate::sync::SYNC_COMMIT_KEY)? else {
        return Ok(());
    };
    if !has_commit(&last) {
        r.add(
            "sync_commit_missing",
            Severity::Warning,
            "sync",
            format!("the last sync commit {last} is not in the host repository; the next `agentvc sync` starts a new history"),
            None,
        );
    }
    if offline {
        return Ok(());
    }
    let remote = space
        .meta(crate::sync::AUTO_SYNC_KEY)?
        .unwrap_or_else(|| "origin".into());
    let refname = format!(
        "{}{}",
        crate::sync::REF_PREFIX,
        crate::sync::space_id(space)?
    );
    match crate::sync::remote_tip(&repo, &remote, &refname) {
        Err(e) => r.add(
            "sync_unreachable",
            Severity::Info,
            "sync",
            format!("could not reach {remote} to compare sync state: {e}"),
            None,
        ),
        Ok(None) => r.add(
            "sync_ref_missing",
            Severity::Warning,
            "sync",
            format!("this space synced as {refname}, but {remote} does not have it; run `agentvc sync`"),
            None,
        ),
        Ok(Some(tip)) if tip != last && !repo.is_ancestor(&tip, &last) => r.add(
            "sync_conflict",
            Severity::Warning,
            "sync",
            format!(
                "{refname} on {remote} moved to {tip} since this space last synced ({last}); another machine continued it, `agentvc sync` merges it in"
            ),
            None,
        ),
        Ok(Some(_)) => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::ForkRequest;

    fn space() -> (crate::testutil::TestDir, Space, Attempt) {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        fs::write(w.join("main.rs"), "fn main() {}\n").unwrap();
        let space = Space::init(w).unwrap();
        let a = space
            .fork(&ForkRequest {
                actor: "agent".into(),
                backend: crate::view::BackendChoice::Copy,
                ..Default::default()
            })
            .unwrap()
            .attempt;
        fs::write(a.view_path.join("lib.rs"), "pub fn f() {}\n").unwrap();
        space.snapshot(a.id, "manual", None).unwrap();
        (tmp, space, a)
    }

    fn codes(r: &DoctorResult) -> Vec<&'static str> {
        r.findings.iter().map(|f| f.code).collect()
    }

    fn check(space: &Space, repair: bool) -> DoctorResult {
        doctor(
            space,
            &DoctorOptions {
                repair,
                offline: true,
            },
        )
        .unwrap()
    }

    /// #204: doctor checks every worktree's `stack.json` and restores a
    /// bad one from its `.bak`.
    #[test]
    fn a_bad_stack_state_is_reported_and_restored() {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(w)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        fs::write(w.join("main.rs"), "fn main() {}\n").unwrap();
        let space = Space::init(w).unwrap();
        let mut win = crate::stack::Window::open(w).unwrap();
        win.state.trunk = "one".into();
        win.save().unwrap();
        win.state.trunk = "two".into();
        win.save().unwrap();
        let file = win.git_dir.join("agentvc").join("stack.json");
        fs::write(&file, "{\"version\": 1, \"tru").unwrap();

        let r = check(&space, false);
        let f = r
            .findings
            .iter()
            .find(|f| f.code == "bad_stack_state")
            .expect("reported");
        assert!(f.repairable && !f.repaired);
        assert_eq!(r.checked.stacks, 1);
        assert!(r.errors >= 1);

        let r = check(&space, true);
        assert!(r
            .findings
            .iter()
            .any(|f| f.code == "bad_stack_state" && f.repaired));
        let back = crate::stack::Window::open(w).unwrap();
        assert_eq!(back.state.trunk, "one", "the previous good state");
        assert!(!codes(&check(&space, false)).contains(&"bad_stack_state"));

        // No backup: reported, not repairable.
        fs::write(&file, "").unwrap();
        fs::remove_file(win.git_dir.join("agentvc").join("stack.json.bak")).unwrap();
        let r = check(&space, true);
        let f = r
            .findings
            .iter()
            .find(|f| f.code == "bad_stack_state")
            .expect("reported");
        assert!(!f.repairable && !f.repaired);
    }

    #[test]
    fn a_healthy_space_has_no_findings() {
        let (_tmp, space, _) = space();
        let r = check(&space, false);
        assert!(r.findings.is_empty(), "{:?}", r.findings);
        assert_eq!(r.errors, 0);
        assert!(r.checked.objects >= 3);
    }

    #[test]
    fn finds_missing_and_corrupt_objects() {
        let (_tmp, space, a) = space();
        let head = space.head_snapshot(a.id).unwrap();
        let files = crate::snapshot::flatten(space.objects(), &head.tree_oid().unwrap()).unwrap();
        let (_, lib) = files["lib.rs"];
        let (_, main) = files["main.rs"];
        fs::remove_file(space.objects().loose_path(&lib)).unwrap();
        // Same zlib framing, different content: hashes to another id.
        let other = space.objects().write_blob(b"tampered").unwrap();
        let p = space.objects().loose_path(&main);
        fs::remove_file(&p).unwrap();
        fs::copy(space.objects().loose_path(&other), &p).unwrap();
        let r = check(&space, false);
        assert!(codes(&r).contains(&"missing_object"), "{:?}", r.findings);
        assert!(codes(&r).contains(&"corrupt_object"), "{:?}", r.findings);
        assert!(r.errors >= 2);
    }

    #[test]
    fn repairs_a_missing_object_from_git() {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(w)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["init", "-q"]);
        fs::write(w.join("main.rs"), "fn main() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        let space = Space::init(w).unwrap();
        let head = space.head_snapshot(ROOT_ATTEMPT).unwrap();
        let files = crate::snapshot::flatten(space.objects(), &head.tree_oid().unwrap()).unwrap();
        let (_, main) = files["main.rs"];
        fs::remove_file(space.objects().loose_path(&main)).unwrap();
        let r = check(&space, true);
        let f = r
            .findings
            .iter()
            .find(|f| f.code == "missing_object")
            .unwrap();
        assert!(f.repaired, "{f:?}");
        assert_eq!(r.errors, 0);
        assert!(check(&space, false).findings.is_empty());
    }

    #[test]
    fn rewrites_a_missing_view_from_its_snapshot() {
        let (_tmp, space, a) = space();
        fs::remove_dir_all(&a.view_path).unwrap();
        let r = check(&space, false);
        assert_eq!(codes(&r), ["missing_view"]);
        assert_eq!(r.errors, 1);
        let r = check(&space, true);
        assert!(r.findings[0].repaired);
        assert_eq!(r.errors, 0);
        assert_eq!(
            fs::read_to_string(a.view_path.join("lib.rs")).unwrap(),
            "pub fn f() {}\n"
        );
        assert!(check(&space, false).findings.is_empty());
    }

    #[test]
    fn removes_orphan_views_and_marks_stale_runs() {
        let (_tmp, space, a) = space();
        let orphan = space.workdir().join(SPACE_DIR).join("views").join("a99");
        fs::create_dir_all(&orphan).unwrap();
        fs::write(orphan.join("x"), "x").unwrap();
        let run = space
            .create_run(a.id, &["sleep".into()], None, false)
            .unwrap();
        // A pid that cannot exist.
        space
            .db
            .execute(
                "UPDATE runs SET pid = 2147483646 WHERE id = ?1",
                params![run.id],
            )
            .unwrap();
        let r = check(&space, false);
        let mut c = codes(&r);
        c.sort();
        assert_eq!(c, ["orphan_view", "stale_run"]);
        assert_eq!(r.errors, 0);
        assert_eq!(r.warnings, 2);
        let r = check(&space, true);
        assert_eq!(r.repaired, 2);
        assert!(!orphan.exists());
        assert!(a.view_path.exists());
        assert!(space.run(run.id).unwrap().finished_at.is_some());
        assert!(check(&space, false).findings.is_empty());
    }

    #[test]
    fn reports_db_problems() {
        let (_tmp, space, _) = space();
        space.set_meta("format", "99").unwrap();
        let r = check(&space, false);
        assert_eq!(codes(&r), ["schema_newer"]);
        assert_eq!(r.errors, 1);
    }
}
