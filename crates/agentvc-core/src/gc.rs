//! `agentvc gc`: keep a space from growing without bound.
//!
//! With a snapshot on every tool call, a space piles up three things:
//!
//! - **objects**: loose zlib files, one per blob and tree. gc packs the live
//!   ones into a git packfile (`objects/pack/`, written by `git
//!   pack-objects`, read by [`ObjectStore`](crate::objects::ObjectStore)),
//!   and drops loose and packed objects nothing references;
//! - **trees of old losers**: snapshots of attempts abandoned more than
//!   `gc.keepAbandonedDays` ago (default 30) get their tree pruned. The rows
//!   stay, so tasks, attempts, reasons, triggers and metadata stay queryable
//!   and `show` says "tree pruned";
//! - **views and overlay layers** (#5, #41): views of attempts closed more
//!   than `gc.keepViewsDays` ago (default 7) are deleted (history stays;
//!   forking from one writes its head snapshot out), and overlay layers no
//!   view uses any more go with them.
//!
//! Never pruned: the latest snapshot of every attempt, snapshots of open or
//! promoted attempts, of attempts with an export or PR, snapshots something
//! was forked from, rebase bases, tagged
//! snapshots (`agentvc tag`) and ones with meta `name` or `keep`, and, once the space has synced, any
//! snapshot the last sync did not carry to the remote. The root attempt's
//! view (the host checkout) is never touched.
//!
//! Concurrency: snapshots take a shared lock on `.agentvc/gc.lock` while they
//! capture, and a pruning gc takes it exclusively, so a capture never reuses
//! an object gc is deleting. Loose objects written in the last hour are kept
//! regardless. `gc --auto` (started in the background after snapshots once
//! there are more than `gc.auto` loose objects) only packs: it deletes
//! nothing that is not in a pack, so it does not need the exclusive lock.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use rusqlite::params;
use serde::Serialize;

use crate::error::{Error, IoContext, Result};
use crate::objects::{Mode, Oid};
use crate::space::{now, Attempt, Space, Status, ROOT_ATTEMPT, SPACE_DIR};

pub const KEEP_ABANDONED_DAYS: i64 = 30;
pub const KEEP_VIEWS_DAYS: i64 = 7;
/// Loose objects before `gc --auto` packs them.
pub const AUTO_LOOSE: i64 = 4000;
/// Packs before `gc --auto` merges them into one.
pub const AUTO_PACK_LIMIT: usize = 20;
/// Unreferenced loose objects younger than this stay: a command may be
/// about to reference them.
pub const PRUNE_EXPIRE: Duration = Duration::from_secs(3600);

#[derive(Clone, Debug, Default)]
pub struct GcOptions {
    pub dry_run: bool,
    /// Only pack loose objects, and only when there are enough of them.
    pub auto: bool,
    pub keep_abandoned_days: Option<i64>,
    pub keep_views_days: Option<i64>,
    /// Age below which unreferenced loose objects stay (tests use zero).
    pub prune_expire: Option<Duration>,
    /// Do not pack (only prune).
    pub no_pack: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ObjectStats {
    pub loose: usize,
    pub loose_bytes: u64,
    pub packs: usize,
    pub packed: usize,
    pub pack_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RemovedView {
    pub attempt: i64,
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GcResult {
    pub dry_run: bool,
    pub auto: bool,
    /// Why nothing was done.
    pub skipped: Option<String>,
    pub before: ObjectStats,
    pub after: ObjectStats,
    /// Snapshots whose trees were (or would be) pruned.
    pub pruned_snapshots: Vec<i64>,
    /// Expired snapshots kept because the last sync did not carry them.
    pub kept_unsynced: usize,
    /// Objects dropped because nothing references them.
    pub objects_removed: usize,
    /// Live objects written into the new pack.
    pub objects_packed: usize,
    pub views: Vec<RemovedView>,
    /// Views that were due but stayed (busy mount, active run).
    pub views_kept: Vec<i64>,
    #[cfg(target_os = "linux")]
    pub layers: Vec<crate::overlay::RemovedLayer>,
    pub bytes_freed: u64,
    /// Notes, e.g. why packing was skipped.
    pub notes: Vec<String>,
    pub millis: u128,
}

// ---- locks -------------------------------------------------------------------

fn lock_file(space: &Space, name: &str) -> Option<fs::File> {
    let path = space.workdir().join(SPACE_DIR).join(name);
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()
}

#[cfg(unix)]
fn flock(f: &fs::File, op: libc::c_int) -> bool {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: the fd is open for the duration of the call.
        if unsafe { libc::flock(f.as_raw_fd(), op) } == 0 {
            return true;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// Held while capturing a view: a pruning gc waits for it. Best effort, so a
/// space on a filesystem without locks still works.
pub(crate) fn shared_lock(space: &Space) -> Option<fs::File> {
    let f = lock_file(space, "gc.lock")?;
    #[cfg(unix)]
    if !flock(&f, libc::LOCK_SH) {
        return None;
    }
    Some(f)
}

fn exclusive_lock(space: &Space, name: &str, wait: bool) -> Result<fs::File> {
    let f = lock_file(space, name).ok_or(Error::GcBusy)?;
    #[cfg(unix)]
    {
        let op = libc::LOCK_EX | if wait { 0 } else { libc::LOCK_NB };
        if !flock(&f, op) {
            return Err(Error::GcBusy);
        }
    }
    #[cfg(not(unix))]
    let _ = wait;
    Ok(f)
}

// ---- helpers -----------------------------------------------------------------

/// Bytes used by the files under `path`, not following symlinks.
pub fn disk_usage(path: &Path) -> u64 {
    let Ok(m) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !m.is_dir() {
        return m.len();
    }
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total += m.len();
            }
        }
    }
    total
}

fn config_i64(space: &Space, key: &str, default: i64) -> Result<i64> {
    Ok(space
        .config(key)?
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default))
}

fn object_stats(space: &Space) -> ObjectStats {
    let store = space.objects();
    store.reload_packs();
    let loose = store.loose_objects();
    let (packs, packed) = store.pack_stats();
    ObjectStats {
        loose: loose.len(),
        loose_bytes: loose.iter().map(|(_, b, _)| b).sum(),
        packs,
        packed,
        pack_bytes: disk_usage(&store.pack_dir()),
    }
}

/// When an attempt was closed: `closed_at`, else (spaces from before it was
/// recorded) its last snapshot or its creation.
fn closed_at(space: &Space, a: &Attempt) -> Result<i64> {
    let (closed, last): (Option<i64>, Option<i64>) = space.db.query_row(
        "SELECT closed_at, (SELECT MAX(created_at) FROM snapshots WHERE attempt_id = ?1)
         FROM attempts WHERE id = ?1",
        params![a.id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(closed.or(last).unwrap_or(a.created_at))
}

fn ids(space: &Space, sql: &str) -> Result<HashSet<i64>> {
    Ok(space
        .db
        .prepare(sql)?
        .query_map([], |r| r.get::<_, Option<i64>>(0))?
        .filter_map(|r| r.transpose())
        .collect::<std::result::Result<_, _>>()?)
}

/// Snapshot ids carried by the last sync, or `None` when the space never
/// synced. An unreadable sync commit carries nothing.
fn synced_snapshots(space: &Space) -> Result<Option<HashSet<i64>>> {
    let Some(commit) = space.meta(crate::sync::SYNC_COMMIT_KEY)? else {
        return Ok(None);
    };
    let Ok(repo) = crate::export::HostRepo::open(space.workdir()) else {
        return Ok(Some(HashSet::new()));
    };
    let listing = repo
        .git(
            &["ls-tree", "--name-only", &format!("{commit}:snapshots")],
            None,
        )
        .unwrap_or_default();
    Ok(Some(
        listing
            .lines()
            .filter_map(|l| l.trim().strip_prefix('s')?.parse().ok())
            .collect(),
    ))
}

/// Snapshots whose trees are due for pruning, and how many expired ones the
/// last sync did not carry.
fn prunable(space: &Space, cutoff: i64) -> Result<(Vec<i64>, usize)> {
    let mut protected = ids(space, "SELECT parent_snapshot FROM attempts")?;
    protected.extend(ids(space, "SELECT snapshot_id FROM exports")?);
    protected.extend(ids(space, "SELECT snapshot_id FROM git_bases")?);
    protected.extend(ids(space, "SELECT snapshot_id FROM tags")?);
    let exported = ids(space, "SELECT attempt_id FROM exports")?;
    let synced = synced_snapshots(space)?;
    let mut out = Vec::new();
    let mut unsynced = 0;
    for a in space.attempts(None)? {
        if a.status != Status::Abandoned
            || a.id == ROOT_ATTEMPT
            || exported.contains(&a.id)
            || closed_at(space, &a)? > cutoff
        {
            continue;
        }
        let mut snaps = space.snapshots(a.id)?;
        // The latest snapshot stays: `show` and `diff` keep working for the
        // loser, and it is what PR comments (`agentvc pr` in cloud sessions)
        // carry for each attempt.
        snaps.pop();
        for s in snaps {
            let named = s
                .meta
                .as_ref()
                .is_some_and(|m| m.get("name").is_some() || m.get("keep").is_some());
            if s.pruned_at.is_some() || named || protected.contains(&s.id) {
                continue;
            }
            if synced.as_ref().is_some_and(|set| !set.contains(&s.id)) {
                unsynced += 1;
                continue;
            }
            out.push(s.id);
        }
    }
    out.sort_unstable();
    Ok((out, unsynced))
}

/// Every object reachable from what the space still needs: unpruned
/// snapshot trees, rebase bases, and blobs the stat indexes remember.
/// Missing objects are skipped (doctor reports them).
fn live_objects(space: &Space, pruning: &HashSet<i64>) -> Result<HashSet<Oid>> {
    let mut roots: Vec<Oid> = Vec::new();
    let mut stmt = space
        .db
        .prepare("SELECT id, tree FROM snapshots WHERE pruned_at IS NULL")?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    for (id, tree) in rows {
        if !pruning.contains(&id) {
            roots.push(Oid::from_hex(&tree)?);
        }
    }
    let mut stmt = space.db.prepare(
        "SELECT tree FROM git_bases
         UNION SELECT tree FROM story_changes
         UNION SELECT base_tree FROM story_changes",
    )?;
    let bases: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    for t in bases {
        roots.push(Oid::from_hex(&t)?);
    }
    let mut live = HashSet::new();
    let index_dir = space.workdir().join(SPACE_DIR).join("index");
    if let Ok(rd) = fs::read_dir(&index_dir) {
        for e in rd.flatten() {
            live.extend(crate::index::StatIndex::load(&e.path()).oids().copied());
        }
    }
    let store = space.objects();
    let mut stack = roots;
    while let Some(t) = stack.pop() {
        if !live.insert(t) {
            continue;
        }
        let Ok(entries) = store.read_tree(&t) else {
            continue;
        };
        for e in entries {
            if e.mode == Mode::Tree {
                stack.push(e.oid);
            } else {
                live.insert(e.oid);
            }
        }
    }
    Ok(live)
}

/// Write `oids` into a new packfile with `git pack-objects`, reading them
/// from the store itself (its layout is a git object directory). Returns
/// the pack's name, or `Ok(None)` when there was nothing to pack, and an
/// error string when git is unavailable.
fn write_pack(space: &Space, oids: &[Oid]) -> Result<std::result::Result<Option<String>, String>> {
    if oids.is_empty() {
        return Ok(Ok(None));
    }
    let store = space.objects();
    let tmp = space.workdir().join(SPACE_DIR).join("tmp");
    let gitdir = tmp.join("gc-git");
    let out = tmp.join(format!("gc-pack-{}", std::process::id()));
    let _ = fs::remove_dir_all(&out);
    fs::create_dir_all(&out).at(&out)?;
    if !gitdir.join("HEAD").is_file() {
        let made = Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&gitdir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !made.is_ok_and(|s| s.success()) {
            return Ok(Err("git is not available to write packfiles".into()));
        }
    }
    let objects = crate::view::canonical(store.root()).at(store.root())?;
    let child = Command::new("git")
        .arg("--git-dir")
        .arg(&gitdir)
        .args(["pack-objects", "-q", "--delta-base-offset"])
        .arg(out.join("pack"))
        .env("GIT_OBJECT_DIRECTORY", &objects)
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return Ok(Err(format!("could not start git pack-objects: {e}"))),
    };
    {
        let mut stdin = child.stdin.take().expect("piped");
        let mut list = String::with_capacity(oids.len() * 41);
        for o in oids {
            list.push_str(&o.to_hex());
            list.push('\n');
        }
        stdin.write_all(list.as_bytes()).at("git pack-objects")?;
    }
    let done = child.wait_with_output().at("git pack-objects")?;
    if !done.status.success() {
        let _ = fs::remove_dir_all(&out);
        return Ok(Err(format!(
            "git pack-objects failed: {}",
            String::from_utf8_lossy(&done.stderr).trim()
        )));
    }
    let hash = String::from_utf8_lossy(&done.stdout).trim().to_string();
    let name = format!("pack-{hash}");
    let dest = store.pack_dir();
    fs::create_dir_all(&dest).at(&dest)?;
    // The .pack first, the .idx last: readers only open packs with an index.
    for ext in ["pack", "idx"] {
        let from = out.join(format!("{name}.{ext}"));
        let to = dest.join(format!("{name}.{ext}"));
        if to.exists() {
            // The same objects packed again: the name is the content's hash,
            // and on Windows an open pack can't be replaced.
            continue;
        }
        fs::rename(&from, &to).at(&to)?;
    }
    let _ = fs::remove_dir_all(&out);
    store.reload_packs();
    Ok(Ok(Some(name)))
}

fn remove_pack_files(space: &Space, keep: Option<&str>) {
    let dir = space.objects().pack_dir();
    // Windows can't delete a file that is open, so let go of the packs first.
    space.objects().close_packs();
    let Ok(rd) = fs::read_dir(&dir) else { return };
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    // Index files first, so no reader opens a pack being deleted.
    files.sort_by_key(|p| p.extension().is_none_or(|x| x != "idx"));
    for p in files {
        let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned());
        if stem.as_deref() != keep && stem.is_some_and(|s| s.starts_with("pack-")) {
            let _ = fs::remove_file(&p);
        }
    }
    space.objects().reload_packs();
}

/// Rough count of loose objects, from one of the 256 fan-out directories
/// (like `git gc --auto`).
pub fn estimate_loose(space: &Space) -> usize {
    fs::read_dir(space.objects().root().join("17"))
        .map(|rd| rd.count() * 256)
        .unwrap_or(0)
}

/// Whether `gc --auto` would do something: enough loose objects, and the
/// `gc.auto` setting is not 0.
pub fn auto_needed(space: &Space) -> bool {
    let limit = config_i64(space, crate::config::GC_AUTO, AUTO_LOOSE).unwrap_or(AUTO_LOOSE);
    limit > 0 && estimate_loose(space) as i64 > limit
}

// ---- gc ----------------------------------------------------------------------

/// Run gc. See the module docs for what it touches.
pub fn gc(space: &Space, opts: &GcOptions) -> Result<GcResult> {
    let start = std::time::Instant::now();
    let mut res = GcResult {
        dry_run: opts.dry_run,
        auto: opts.auto,
        ..Default::default()
    };
    if opts.auto {
        if !auto_needed(space) {
            res.skipped = Some("not enough loose objects (see gc.auto)".into());
            res.millis = start.elapsed().as_millis();
            return Ok(res);
        }
        let Ok(_pack_lock) = exclusive_lock(space, "gc-pack.lock", false) else {
            res.skipped = Some("another gc is running".into());
            return Ok(res);
        };
        res.before = object_stats(space);
        if !opts.dry_run {
            auto_pack(space, &mut res)?;
        }
        res.after = object_stats(space);
        res.millis = start.elapsed().as_millis();
        return Ok(res);
    }

    let _pack_lock = exclusive_lock(space, "gc-pack.lock", true)?;
    let _lock = exclusive_lock(space, "gc.lock", true)?;
    res.before = object_stats(space);
    let t = now();
    let day = 86_400;

    // 1. Trees of long-abandoned attempts.
    let keep_days = match opts.keep_abandoned_days {
        Some(d) => d,
        None => config_i64(space, crate::config::GC_KEEP_ABANDONED, KEEP_ABANDONED_DAYS)?,
    };
    let (prune, unsynced) = prunable(space, t - keep_days * day)?;
    res.kept_unsynced = unsynced;
    res.pruned_snapshots = prune.clone();
    let pruning: HashSet<i64> = prune.iter().copied().collect();
    if !opts.dry_run && !prune.is_empty() {
        let tx = space.db.unchecked_transaction()?;
        for id in &prune {
            tx.execute(
                "UPDATE snapshots SET pruned_at = ?1 WHERE id = ?2",
                params![t, id],
            )?;
        }
        tx.commit()?;
    }

    // 2. Views of attempts closed long ago, and 3. overlay layers.
    let view_days = match opts.keep_views_days {
        Some(d) => d,
        None => config_i64(space, crate::config::GC_KEEP_VIEWS, KEEP_VIEWS_DAYS)?,
    };
    let dropping = gc_views(space, t - view_days * day, opts.dry_run, &mut res)?;
    #[cfg(target_os = "linux")]
    {
        let layers = space.unused_layers(&dropping)?;
        if !opts.dry_run {
            for l in &layers {
                space.remove_layer(l.layer)?;
            }
        }
        res.bytes_freed += layers.iter().map(|l| l.bytes).sum::<u64>();
        res.layers = layers;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = dropping;

    // 4. Objects: pack what is live, drop what is not.
    let live = live_objects(space, &pruning)?;
    let expire = opts.prune_expire.unwrap_or(PRUNE_EXPIRE);
    let store = space.objects();
    let loose = store.loose_objects();
    let now_st = SystemTime::now();
    let old = |m: &SystemTime| now_st.duration_since(*m).is_ok_and(|age| age >= expire);
    let dead_loose: Vec<&(Oid, u64, SystemTime)> = loose
        .iter()
        .filter(|(o, _, m)| !live.contains(o) && old(m))
        .collect();
    let packed: HashSet<Oid> = store.packed_oids().into_iter().collect();
    let dead_packed = packed.iter().filter(|o| !live.contains(o)).count();
    res.objects_removed = dead_loose.len() + dead_packed;
    let dead_loose_bytes: u64 = dead_loose.iter().map(|(_, b, _)| b).sum();

    if opts.dry_run {
        res.bytes_freed += dead_loose_bytes;
        res.objects_packed = live.iter().filter(|o| store.contains(o)).count();
        res.after = res.before.clone();
        res.millis = start.elapsed().as_millis();
        return Ok(res);
    }
    for (o, _, _) in &dead_loose {
        let _ = fs::remove_file(store.loose_path(o));
    }
    if opts.no_pack {
        if dead_packed > 0 {
            res.notes.push(format!(
                "{dead_packed} unreferenced packed objects stay (--no-pack)"
            ));
            res.objects_removed -= dead_packed;
        }
    } else {
        // Repack everything live; recent unreferenced loose objects stay loose.
        let mut want: Vec<Oid> = live
            .iter()
            .filter(|o| packed.contains(o) || store.loose_path(o).exists())
            .copied()
            .collect();
        want.sort();
        match write_pack(space, &want)? {
            Ok(name) => {
                remove_pack_files(space, name.as_deref());
                res.objects_packed = want.len();
                for o in &want {
                    let _ = fs::remove_file(store.loose_path(o));
                }
            }
            Err(why) => {
                res.notes.push(why);
                res.objects_removed -= dead_packed;
            }
        }
    }
    remove_empty_fanout_dirs(space);
    res.after = object_stats(space);
    let before = res.before.loose_bytes + res.before.pack_bytes;
    let after = res.after.loose_bytes + res.after.pack_bytes;
    res.bytes_freed += before.saturating_sub(after);
    space.record(
        "agentvc",
        "gc",
        serde_json::json!({
            "pruned_snapshots": res.pruned_snapshots,
            "views": res.views.iter().map(|v| v.attempt).collect::<Vec<_>>(),
            "objects_removed": res.objects_removed,
            "bytes_freed": res.bytes_freed,
        }),
    )?;
    res.millis = start.elapsed().as_millis();
    Ok(res)
}

/// `gc --auto`: pack loose objects (and merge packs once there are many).
/// Deletes only loose objects that are now in a pack.
fn auto_pack(space: &Space, res: &mut GcResult) -> Result<()> {
    let store = space.objects();
    let mut want: Vec<Oid> = store
        .loose_objects()
        .into_iter()
        .map(|(o, _, _)| o)
        .collect();
    let merge = store.pack_stats().0 >= AUTO_PACK_LIMIT;
    if merge {
        want.extend(store.packed_oids());
    }
    want.sort();
    want.dedup();
    match write_pack(space, &want)? {
        Ok(name) => {
            if merge {
                remove_pack_files(space, name.as_deref());
            }
            for o in &want {
                let _ = fs::remove_file(store.loose_path(o));
            }
            res.objects_packed = want.len();
            remove_empty_fanout_dirs(space);
        }
        Err(why) => res.notes.push(why),
    }
    Ok(())
}

fn remove_empty_fanout_dirs(space: &Space) {
    if let Ok(rd) = fs::read_dir(space.objects().root()) {
        for e in rd.flatten() {
            if e.file_name().len() == 2 {
                let _ = fs::remove_dir(e.path()); // fails unless empty
            }
        }
    }
}

/// Remove views of attempts closed before `cutoff`. Returns the attempts
/// whose overlay views go (or would go).
fn gc_views(space: &Space, cutoff: i64, dry_run: bool, res: &mut GcResult) -> Result<Vec<i64>> {
    let views_dir = space.workdir().join(SPACE_DIR).join("views");
    let running: HashSet<i64> = space
        .runs(true)?
        .into_iter()
        .filter(|r| r.alive != Some(false))
        .map(|r| r.attempt_id)
        .collect();
    let mut dropping = Vec::new();
    for a in space.attempts(None)? {
        // Only closed attempts, never the host checkout, only views agentvc
        // made (under .agentvc/views).
        if a.status.is_open()
            || a.id == ROOT_ATTEMPT
            || a.backend == REMOVED
            || !a.view_path.starts_with(&views_dir)
            || closed_at(space, &a)? > cutoff
        {
            continue;
        }
        if running.contains(&a.id) {
            res.views_kept.push(a.id);
            continue;
        }
        let overlay = a.backend == crate::view::Backend::Overlay.as_str();
        #[cfg(target_os = "linux")]
        let bytes = if overlay {
            disk_usage(&space.overlay_dir(a.id))
        } else {
            disk_usage(&a.view_path)
        };
        #[cfg(not(target_os = "linux"))]
        let bytes = disk_usage(&a.view_path);
        if !dry_run {
            // The view's git link, git dir and pinned commits (#68).
            let _ = space.unlink_view_git(a.id);
            #[cfg(target_os = "linux")]
            if overlay && !space.remove_overlay_view(&a)? {
                res.views_kept.push(a.id);
                continue;
            }
            if !overlay {
                remove_tree(&a.view_path);
                // Where `undo` of its fork put the view aside.
                remove_tree(&space.trash_dir(a.id));
            }
            let _ = fs::remove_file(
                space
                    .workdir()
                    .join(SPACE_DIR)
                    .join("index")
                    .join(format!("a{}", a.id)),
            );
            space.db.execute(
                "UPDATE attempts SET backend = ?1 WHERE id = ?2",
                params![REMOVED, a.id],
            )?;
        }
        if overlay {
            dropping.push(a.id);
        }
        res.bytes_freed += bytes;
        res.views.push(RemovedView {
            attempt: a.id,
            path: a.view_path.clone(),
            bytes,
        });
    }
    Ok(dropping)
}

/// Backend name of an attempt whose view gc removed.
pub const REMOVED: &str = "removed";

fn remove_tree(path: &Path) {
    #[cfg(target_os = "linux")]
    crate::overlay::remove_tree(path);
    #[cfg(not(target_os = "linux"))]
    let _ = fs::remove_dir_all(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::{ForkRequest, Ref};

    fn space() -> (crate::testutil::TestDir, Space) {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        fs::write(w.join("main.rs"), "fn main() {}\n").unwrap();
        let space = Space::init(w).unwrap();
        (tmp, space)
    }

    fn fork(space: &Space) -> Attempt {
        space
            .fork(&ForkRequest {
                actor: "agent".into(),
                backend: crate::view::BackendChoice::Copy,
                ..Default::default()
            })
            .unwrap()
            .attempt
    }

    fn age_attempt(space: &Space, id: i64, days: i64) {
        space
            .db
            .execute(
                "UPDATE attempts SET closed_at = closed_at - ?1 WHERE id = ?2",
                params![days * 86_400, id],
            )
            .unwrap();
    }

    fn now_opts() -> GcOptions {
        GcOptions {
            prune_expire: Some(Duration::ZERO),
            ..Default::default()
        }
    }

    #[test]
    fn packs_objects_and_reads_them_back() {
        let (_tmp, space) = space();
        let a = fork(&space);
        fs::write(a.view_path.join("lib.rs"), "pub fn f() {}\n".repeat(50)).unwrap();
        let s = space.snapshot(a.id, "manual", None).unwrap().snapshot;
        let r = gc(&space, &now_opts()).unwrap();
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        assert_eq!(r.after.loose, 0);
        assert_eq!(r.after.packs, 1);
        assert!(r.objects_packed >= 3);
        // Reads come from the pack now.
        let files = crate::snapshot::flatten(space.objects(), &s.tree_oid().unwrap()).unwrap();
        let (_, blob) = files["lib.rs"];
        assert_eq!(
            space.objects().read_blob(&blob).unwrap(),
            "pub fn f() {}\n".repeat(50).as_bytes()
        );
        // A second gc keeps one pack, and new writes dedupe against it.
        fs::write(a.view_path.join("lib.rs"), "pub fn g() {}\n".repeat(50)).unwrap();
        space.snapshot(a.id, "manual", None).unwrap();
        let r = gc(&space, &now_opts()).unwrap();
        assert_eq!(r.after.packs, 1);
        assert_eq!(r.after.loose, 0);
        assert_eq!(
            space
                .diff(Ref::Snapshot(s.id), Ref::Attempt(a.id))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn prunes_old_losers_but_keeps_what_is_referenced() {
        let (_tmp, space) = space();
        let task = space.create_task("t", None).unwrap();
        let mk = || {
            space
                .fork(&ForkRequest {
                    task_id: Some(task.id),
                    actor: "agent".into(),
                    backend: crate::view::BackendChoice::Copy,
                    ..Default::default()
                })
                .unwrap()
                .attempt
        };
        let (win, lose, named) = (mk(), mk(), mk());
        fs::write(lose.view_path.join("loser.rs"), "only in the loser").unwrap();
        let ls = space.snapshot(lose.id, "manual", None).unwrap().snapshot;
        fs::write(named.view_path.join("n.rs"), "named").unwrap();
        let ns = space
            .snapshot(
                named.id,
                "manual",
                Some(serde_json::json!({"name": "keep me"})),
            )
            .unwrap()
            .snapshot;
        // A child forked from the loser keeps the fork point.
        let child = space
            .fork(&ForkRequest {
                from: Some(Ref::Attempt(lose.id)),
                actor: "agent".into(),
                backend: crate::view::BackendChoice::Copy,
                ..Default::default()
            })
            .unwrap();
        let fork_point = child.parent_snapshot.id;
        assert_eq!(fork_point, ls.id);
        // Later work in the loser is only in its own snapshot.
        fs::write(lose.view_path.join("loser.rs"), "later loser work").unwrap();
        let later = space.snapshot(lose.id, "manual", None).unwrap().snapshot;
        let later_blob = crate::snapshot::flatten(space.objects(), &later.tree_oid().unwrap())
            .unwrap()["loser.rs"]
            .1;
        // The loser's latest snapshot stays.
        fs::write(lose.view_path.join("loser.rs"), "last loser work").unwrap();
        let last = space.snapshot(lose.id, "manual", None).unwrap().snapshot;
        space.set_status(win.id, Status::Promoted, None).unwrap();
        assert_eq!(space.attempt(lose.id).unwrap().status, Status::Abandoned);
        for id in [lose.id, named.id] {
            age_attempt(&space, id, 40);
        }

        let dry = gc(
            &space,
            &GcOptions {
                dry_run: true,
                ..now_opts()
            },
        )
        .unwrap();
        assert_eq!(dry.pruned_snapshots, vec![later.id]);
        assert!(space.snapshot_by_id(later.id).unwrap().pruned_at.is_none());
        assert!(space.objects().contains(&later_blob));

        let r = gc(&space, &now_opts()).unwrap();
        assert_eq!(r.pruned_snapshots, vec![later.id]);
        assert!(
            !space.objects().contains(&later_blob),
            "pruned tree's own blob dropped"
        );
        assert!(
            space.snapshot_by_id(ns.id).unwrap().pruned_at.is_none(),
            "named kept"
        );
        assert!(
            space.snapshot_by_id(last.id).unwrap().pruned_at.is_none(),
            "head kept"
        );
        assert_eq!(
            space
                .diff(Ref::Snapshot(fork_point), Ref::Attempt(lose.id))
                .unwrap()
                .len(),
            1
        );
        for s in space.snapshots(win.id).unwrap() {
            assert!(s.pruned_at.is_none(), "winner kept");
        }
        // Views of both closed attempts are gone; history stays.
        assert!(!lose.view_path.exists());
        assert_eq!(space.attempt(lose.id).unwrap().backend, REMOVED);
        assert_eq!(
            space.attempt(lose.id).unwrap().reason.as_deref(),
            Some(&*format!("lost to a{}", win.id))
        );
        // Every remaining snapshot can still be read in full.
        for a in space.attempts(None).unwrap() {
            for s in space.snapshots(a.id).unwrap() {
                if let Ok(t) = s.tree_oid() {
                    crate::snapshot::flatten(space.objects(), &t).unwrap();
                }
            }
        }
        // Pruned snapshots say so.
        for id in &r.pruned_snapshots {
            let e = space.snapshot_by_id(*id).unwrap().tree_oid().unwrap_err();
            assert_eq!(e.code(), "tree_pruned");
        }
        // Forking from the closed winner writes its head snapshot out.
        let again = space
            .fork(&ForkRequest {
                from: Some(Ref::Attempt(win.id)),
                actor: "agent".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(again.attempt.view_path.join("main.rs").exists());
    }

    #[test]
    fn keeps_recent_losers_and_unsynced_snapshots() {
        let (_tmp, space) = space();
        let a = fork(&space);
        fs::write(a.view_path.join("x"), "x").unwrap();
        let s = space.snapshot(a.id, "manual", None).unwrap().snapshot;
        fs::write(a.view_path.join("x"), "y").unwrap();
        space.snapshot(a.id, "manual", None).unwrap();
        space
            .set_status(a.id, Status::Abandoned, Some("no"))
            .unwrap();
        let r = gc(&space, &now_opts()).unwrap();
        assert!(r.pruned_snapshots.is_empty());
        assert!(r.views.is_empty());
        age_attempt(&space, a.id, 40);
        // Pretend the space synced without this snapshot.
        space
            .set_meta(
                crate::sync::SYNC_COMMIT_KEY,
                "0000000000000000000000000000000000000000",
            )
            .unwrap();
        let r = gc(&space, &now_opts()).unwrap();
        assert!(r.pruned_snapshots.is_empty());
        assert_eq!(r.kept_unsynced, 1);
        assert!(space.objects().contains(&s.tree_oid().unwrap()));
    }

    #[test]
    fn keeps_recent_unreferenced_objects_and_index_blobs() {
        let (_tmp, space) = space();
        let a = fork(&space);
        fs::write(a.view_path.join("wip"), "unsnapshotted work").unwrap();
        // diff/status capture the view without a snapshot: only the index
        // knows the blob.
        let tree = space.capture_view(&a).unwrap();
        let stray = space.objects().write_blob(b"stray").unwrap();
        let r = gc(&space, &GcOptions::default()).unwrap();
        assert_eq!(r.objects_removed, 0, "{r:?}");
        assert!(space.objects().contains(&stray));
        gc(&space, &now_opts()).unwrap();
        assert!(!space.objects().contains(&stray));
        let files = crate::snapshot::flatten(space.objects(), &tree).ok();
        let _ = files; // the tree itself is unreferenced and may go
        let snap = space.snapshot(a.id, "manual", None).unwrap().snapshot;
        let files = crate::snapshot::flatten(space.objects(), &snap.tree_oid().unwrap()).unwrap();
        let (_, blob) = files["wip"];
        assert_eq!(
            space.objects().read_blob(&blob).unwrap(),
            b"unsnapshotted work"
        );
    }

    #[test]
    fn auto_packs_only_past_the_threshold() {
        let (_tmp, space) = space();
        let r = gc(
            &space,
            &GcOptions {
                auto: true,
                ..now_opts()
            },
        )
        .unwrap();
        assert!(r.skipped.is_some());
        space
            .set_config(crate::config::GC_AUTO, &["1".into()])
            .unwrap();
        // Make sure the sampled fan-out directory has an object.
        let dir = space.objects().root().join("17");
        fs::create_dir_all(&dir).unwrap();
        let mut i = 0u32;
        while !dir.read_dir().unwrap().next().is_some() {
            space.objects().write_blob(&i.to_le_bytes()).unwrap();
            i += 1;
        }
        let r = gc(
            &space,
            &GcOptions {
                auto: true,
                ..now_opts()
            },
        )
        .unwrap();
        assert!(r.skipped.is_none(), "{r:?}");
        assert_eq!(r.after.loose, 0);
        space
            .set_config(crate::config::GC_AUTO, &["0".into()])
            .unwrap();
        assert!(!auto_needed(&space));
    }
}
