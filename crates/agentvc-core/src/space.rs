//! A space: the object store, the metadata database and the attempt views
//! for one working directory.
//!
//! Layout:
//!
//! ```text
//! <workdir>/.agentvc/
//!     meta.db        SQLite: tasks, attempts, snapshots, operations
//!     objects/       git loose objects (blobs and trees)
//!     views/aN/      one directory per forked attempt
//!     layers/LN/     frozen read-only layers under overlay views
//!     overlay/aN/    an overlay view's private upper and work dirs
//! ```
//!
//! The working directory itself is the view of the root attempt `a1`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::error::{Error, IoContext, Result};
use crate::objects::{ObjectStore, Oid};
use crate::snapshot::{self, FileChange};
use crate::view::{self, Backend, BackendChoice, ForkStats};

pub const SPACE_DIR: &str = ".agentvc";
pub const ROOT_ATTEMPT: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    goal TEXT NOT NULL,
    criteria TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS attempts (
    id INTEGER PRIMARY KEY,
    task_id INTEGER REFERENCES tasks(id),
    parent_attempt INTEGER REFERENCES attempts(id),
    parent_snapshot INTEGER REFERENCES snapshots(id),
    actor TEXT NOT NULL,
    model TEXT,
    prompt TEXT,
    status TEXT NOT NULL,
    reason TEXT,
    view_path TEXT NOT NULL,
    backend TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS snapshots (
    id INTEGER PRIMARY KEY,
    attempt_id INTEGER NOT NULL REFERENCES attempts(id),
    tree TEXT NOT NULL,
    trigger TEXT NOT NULL,
    meta TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS operations (
    id INTEGER PRIMARY KEY,
    actor TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS exports (
    id INTEGER PRIMARY KEY,
    attempt_id INTEGER NOT NULL REFERENCES attempts(id),
    snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    branch TEXT NOT NULL,
    commit_id TEXT NOT NULL,
    parent TEXT,
    pr_url TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS runs (
    id INTEGER PRIMARY KEY,
    attempt_id INTEGER NOT NULL REFERENCES attempts(id),
    command TEXT NOT NULL,
    check_command TEXT,
    pid INTEGER,
    log_path TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    exit_code INTEGER,
    check_exit_code INTEGER,
    duration_ms INTEGER
);
CREATE TABLE IF NOT EXISTS layers (
    id INTEGER PRIMARY KEY,
    parent INTEGER REFERENCES layers(id),
    source_attempt INTEGER NOT NULL REFERENCES attempts(id),
    files INTEGER NOT NULL DEFAULT 0,
    bytes INTEGER NOT NULL DEFAULT 0,
    complete INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS overlays (
    attempt_id INTEGER PRIMARY KEY REFERENCES attempts(id),
    top_layer INTEGER NOT NULL REFERENCES layers(id),
    mounter TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS git_bases (
    id INTEGER PRIMARY KEY,
    attempt_id INTEGER NOT NULL REFERENCES attempts(id),
    snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    commit_id TEXT NOT NULL,
    tree TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS attempts_task ON attempts(task_id);
CREATE INDEX IF NOT EXISTS snapshots_attempt ON snapshots(attempt_id);
"#;

/// Columns added after the first release, as (table, column, type).
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    // When the attempt was abandoned or promoted (gc retention counts from it).
    ("attempts", "closed_at", "INTEGER"),
    // When `agentvc gc` dropped the snapshot's tree; the row stays.
    ("snapshots", "pruned_at", "INTEGER"),
];

fn migrate(db: &Connection) -> Result<()> {
    for (table, col, ty) in ADDED_COLUMNS {
        let has = db
            .prepare(&format!(
                "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"
            ))?
            .exists(params![col])?;
        if !has {
            db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {ty}"))?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Exploring,
    Passed,
    Failed,
    Abandoned,
    Promoted,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Exploring => "exploring",
            Status::Passed => "passed",
            Status::Failed => "failed",
            Status::Abandoned => "abandoned",
            Status::Promoted => "promoted",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        Some(match s {
            "exploring" => Status::Exploring,
            "passed" => Status::Passed,
            "failed" => Status::Failed,
            "abandoned" => Status::Abandoned,
            "promoted" => Status::Promoted,
            _ => return None,
        })
    }

    /// Open attempts can still change; closed ones are history.
    pub fn is_open(self) -> bool {
        matches!(self, Status::Exploring | Status::Passed | Status::Failed)
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Task {
    pub id: i64,
    pub goal: String,
    pub criteria: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Attempt {
    pub id: i64,
    pub task_id: Option<i64>,
    pub parent_attempt: Option<i64>,
    pub parent_snapshot: Option<i64>,
    pub actor: String,
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub status: Status,
    pub reason: Option<String>,
    pub view_path: PathBuf,
    pub backend: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub id: i64,
    pub attempt_id: i64,
    pub tree: String,
    pub trigger: String,
    pub meta: Option<serde_json::Value>,
    pub created_at: i64,
    /// Set when `agentvc gc` pruned this snapshot's tree: the row, its
    /// trigger and metadata stay, its files are gone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pruned_at: Option<i64>,
}

impl Snapshot {
    /// The snapshot's tree, or `tree_pruned` when gc dropped it.
    pub fn tree_oid(&self) -> Result<Oid> {
        if self.pruned_at.is_some() {
            return Err(Error::TreePruned(self.id));
        }
        Oid::from_hex(&self.tree)
    }
}

/// A user-facing reference to history: `aN` is an attempt (its latest
/// snapshot), `sN` is one snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ref {
    Attempt(i64),
    Snapshot(i64),
}

impl std::str::FromStr for Ref {
    type Err = Error;
    fn from_str(s: &str) -> Result<Ref> {
        let bad = || Error::BadRef(s.to_string());
        let (kind, num) = s.split_at(s.find(|c: char| c.is_ascii_digit()).ok_or_else(bad)?);
        let n: i64 = num.parse().map_err(|_| bad())?;
        match kind {
            "a" => Ok(Ref::Attempt(n)),
            "s" => Ok(Ref::Snapshot(n)),
            _ => Err(bad()),
        }
    }
}

impl std::fmt::Display for Ref {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ref::Attempt(n) => write!(f, "a{n}"),
            Ref::Snapshot(n) => write!(f, "s{n}"),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ForkRequest {
    pub task_id: Option<i64>,
    pub from: Option<Ref>,
    pub actor: String,
    pub model: Option<String>,
    pub prompt: Option<String>,
    /// View backend; `Auto` picks reflink, then overlayfs, then copy.
    pub backend: BackendChoice,
}

#[derive(Clone, Debug, Serialize)]
pub struct ForkResult {
    pub attempt: Attempt,
    pub parent_snapshot: Snapshot,
    pub backend: Backend,
    /// Why this backend was used when it is not the first choice, for
    /// example why overlayfs was unavailable and the view was copied.
    pub backend_reason: Option<String>,
    pub stats: Option<ForkStats>,
    /// Set for overlay views.
    #[cfg(target_os = "linux")]
    pub overlay: Option<crate::overlay::OverlayInfo>,
    /// The view's git link (`.git` file), so git works inside it. `None`
    /// when the host has no git history.
    pub git: Option<crate::viewgit::ViewGit>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SnapshotResult {
    pub snapshot: Snapshot,
    /// True when the tree matched the attempt's previous snapshot, so no new
    /// snapshot was recorded.
    pub unchanged: bool,
}

/// How a view was made, before it is recorded.
struct WarmFork {
    backend: Backend,
    reason: Option<String>,
    stats: Option<ForkStats>,
    #[cfg(target_os = "linux")]
    overlay: Option<crate::overlay::OverlayInfo>,
}

/// Filesystem name for messages, like `stat -f -c %T`.
fn fs_type(path: &Path) -> String {
    #[cfg(target_os = "linux")]
    {
        let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
            return "this filesystem".into();
        };
        // SAFETY: c is a valid C string and buf is a plain struct.
        let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut buf) } != 0 {
            return "this filesystem".into();
        }
        match buf.f_type {
            0xEF53 => "ext4".into(),
            0x794c7630 => "overlayfs".into(),
            0x01021994 => "tmpfs".into(),
            0x9123683E => "btrfs".into(),
            0x58465342 => "xfs".into(),
            0x6969 => "nfs".into(),
            0x65735546 => "fuse".into(),
            t => format!("filesystem type {t:#x}"),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        "this filesystem".into()
    }
}

pub struct Space {
    workdir: PathBuf,
    pub(crate) db: Connection,
    objects: ObjectStore,
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Space {
    /// Create a space in `workdir`, capture its current state as the first
    /// snapshot, and register it as the root attempt `a1`.
    pub fn init(workdir: &Path) -> Result<Space> {
        let workdir = fs::canonicalize(workdir).at(workdir)?;
        let dir = workdir.join(SPACE_DIR);
        if dir.join("meta.db").exists() {
            return Err(Error::SpaceExists(workdir));
        }
        for sub in ["objects", "views"] {
            fs::create_dir_all(dir.join(sub)).at(dir.join(sub))?;
        }
        // Keep the space out of git status in the host repo.
        fs::write(dir.join(".gitignore"), "*\n").at(dir.join(".gitignore"))?;
        let space = Space::open_at(workdir.clone())?;
        space.db.execute(
            "INSERT INTO meta (key, value) VALUES ('format', ?1)",
            params![crate::FORMAT_VERSION.to_string()],
        )?;
        space.db.execute(
            "INSERT INTO attempts (id, actor, status, view_path, backend, created_at)
             VALUES (?1, 'human', 'exploring', ?2, 'workdir', ?3)",
            params![ROOT_ATTEMPT, workdir.to_string_lossy(), now()],
        )?;
        space.snapshot(ROOT_ATTEMPT, "init", None)?;
        // Remember the host repo's HEAD, so exports are parented on the commit
        // the attempts actually started from.
        if let Some(head) = crate::export::host_head(&workdir) {
            space.set_meta(crate::export::IMPORT_BASE_KEY, &head)?;
        }
        space.record("human", "init", serde_json::json!({ "workdir": workdir }))?;
        Ok(space)
    }

    /// Open the space that contains `start`, searching parent directories.
    pub fn discover(start: &Path) -> Result<Space> {
        let start = fs::canonicalize(start).at(start)?;
        for dir in start.ancestors() {
            if dir.join(SPACE_DIR).join("meta.db").is_file() {
                return Space::open_at(dir.to_path_buf());
            }
        }
        Err(Error::NoSpace(start))
    }

    pub(crate) fn open_at(workdir: PathBuf) -> Result<Space> {
        let dir = workdir.join(SPACE_DIR);
        let db = Connection::open(dir.join("meta.db"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        db.execute_batch(SCHEMA)?;
        migrate(&db)?;
        Ok(Space {
            objects: ObjectStore::new(dir.join("objects")),
            workdir,
            db,
        })
    }

    /// A value from the space's key/value metadata.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_meta(&self, key: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM meta WHERE key = ?1", params![key])?;
        Ok(())
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn objects(&self) -> &ObjectStore {
        &self.objects
    }

    fn views_dir(&self) -> PathBuf {
        self.workdir.join(SPACE_DIR).join("views")
    }

    pub(crate) fn record(&self, actor: &str, kind: &str, payload: serde_json::Value) -> Result<()> {
        self.db.execute(
            "INSERT INTO operations (actor, kind, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![actor, kind, payload.to_string(), now()],
        )?;
        Ok(())
    }

    // ---- tasks ---------------------------------------------------------

    pub fn create_task(&self, goal: &str, criteria: Option<&str>) -> Result<Task> {
        self.db.execute(
            "INSERT INTO tasks (goal, criteria, created_at) VALUES (?1, ?2, ?3)",
            params![goal, criteria, now()],
        )?;
        let id = self.db.last_insert_rowid();
        self.record(
            "human",
            "task.create",
            serde_json::json!({ "task": id, "goal": goal }),
        )?;
        self.task(id)
    }

    pub fn task(&self, id: i64) -> Result<Task> {
        self.db
            .query_row(
                "SELECT id, goal, criteria, created_at FROM tasks WHERE id = ?1",
                params![id],
                |r| {
                    Ok(Task {
                        id: r.get(0)?,
                        goal: r.get(1)?,
                        criteria: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                },
            )
            .optional()?
            .ok_or(Error::NotFound {
                kind: "task",
                id: format!("t{id}"),
            })
    }

    pub fn tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self
            .db
            .prepare("SELECT id, goal, criteria, created_at FROM tasks ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(Task {
                id: r.get(0)?,
                goal: r.get(1)?,
                criteria: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---- attempts ------------------------------------------------------

    const ATTEMPT_COLS: &'static str =
        "id, task_id, parent_attempt, parent_snapshot, actor, model, \
         prompt, status, reason, view_path, backend, created_at";

    fn row_to_attempt(r: &rusqlite::Row<'_>) -> rusqlite::Result<Attempt> {
        let status: String = r.get(7)?;
        let path: String = r.get(9)?;
        Ok(Attempt {
            id: r.get(0)?,
            task_id: r.get(1)?,
            parent_attempt: r.get(2)?,
            parent_snapshot: r.get(3)?,
            actor: r.get(4)?,
            model: r.get(5)?,
            prompt: r.get(6)?,
            status: Status::parse(&status).unwrap_or(Status::Exploring),
            reason: r.get(8)?,
            view_path: PathBuf::from(path),
            backend: r.get(10)?,
            created_at: r.get(11)?,
        })
    }

    pub fn attempt(&self, id: i64) -> Result<Attempt> {
        self.db
            .query_row(
                &format!("SELECT {} FROM attempts WHERE id = ?1", Self::ATTEMPT_COLS),
                params![id],
                Self::row_to_attempt,
            )
            .optional()?
            .ok_or(Error::NotFound {
                kind: "attempt",
                id: format!("a{id}"),
            })
    }

    /// All attempts, or only those of one task, in creation order.
    pub fn attempts(&self, task_id: Option<i64>) -> Result<Vec<Attempt>> {
        let sql = match task_id {
            Some(_) => format!(
                "SELECT {} FROM attempts WHERE task_id = ?1 ORDER BY id",
                Self::ATTEMPT_COLS
            ),
            None => format!("SELECT {} FROM attempts ORDER BY id", Self::ATTEMPT_COLS),
        };
        let mut stmt = self.db.prepare(&sql)?;
        let rows = match task_id {
            Some(t) => stmt.query_map(params![t], Self::row_to_attempt)?,
            None => stmt.query_map([], Self::row_to_attempt)?,
        };
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Whether attempt `id` was forked, directly or through other attempts,
    /// from attempt `ancestor`.
    pub fn descends_from(&self, id: i64, ancestor: i64) -> Result<bool> {
        let mut next = self.attempt(id)?.parent_attempt;
        let mut seen = std::collections::HashSet::new();
        while let Some(p) = next {
            if p == ancestor {
                return Ok(true);
            }
            if !seen.insert(p) {
                break;
            }
            next = self.attempt(p)?.parent_attempt;
        }
        Ok(false)
    }

    /// The attempt whose view contains `path` (the deepest match wins, so a
    /// path inside `.agentvc/views/a3` resolves to a3, not the root attempt).
    pub fn attempt_for_path(&self, path: &Path) -> Result<Attempt> {
        let path = fs::canonicalize(path).at(path)?;
        self.attempts(None)?
            .into_iter()
            .filter(|a| path.starts_with(&a.view_path))
            .max_by_key(|a| a.view_path.components().count())
            .ok_or(Error::NotInView(path))
    }

    /// Fork a new attempt in its own view.
    ///
    /// Forking from an attempt snapshots the parent first and then clones its
    /// whole view, ignored build and dependency directories included, so the
    /// child starts warm. Forking from an older snapshot writes that tree out
    /// instead.
    pub fn fork(&self, req: &ForkRequest) -> Result<ForkResult> {
        if let Some(t) = req.task_id {
            self.task(t)?;
        }
        let from = req.from.unwrap_or(Ref::Attempt(ROOT_ATTEMPT));
        let (parent_attempt, parent_snapshot) = match from {
            Ref::Attempt(id) => {
                let parent = self.attempt(id)?;
                let snap = if parent.status.is_open() && self.ensure_view(&parent)? {
                    self.snapshot(id, "fork", None)?.snapshot
                } else {
                    self.head_snapshot(id)?
                };
                (parent, snap)
            }
            Ref::Snapshot(id) => {
                let snap = self.snapshot_by_id(id)?;
                (self.attempt(snap.attempt_id)?, snap)
            }
        };
        // A fork without a task works on its parent's task.
        let task_id = req.task_id.or(parent_attempt.task_id);

        let created = now();
        self.db.execute(
            "INSERT INTO attempts (task_id, parent_attempt, parent_snapshot, actor, model, prompt,
                                   status, view_path, backend, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'exploring', '', 'pending', ?7)",
            params![
                task_id,
                parent_attempt.id,
                parent_snapshot.id,
                req.actor,
                req.model,
                req.prompt,
                created
            ],
        )?;
        let id = self.db.last_insert_rowid();
        let view_path = self.views_dir().join(format!("a{id}"));

        let warm = matches!(from, Ref::Attempt(_)) && self.ensure_view(&parent_attempt)?;
        let made = if warm {
            self.fork_view(&parent_attempt, id, &view_path, req.backend)
        } else {
            let tree = parent_snapshot.tree_oid()?;
            snapshot::materialize(&self.objects, &tree, &view_path).map(|()| WarmFork {
                backend: Backend::Materialize,
                reason: (req.backend != BackendChoice::Auto)
                    .then(|| "forking from a stored snapshot writes its tree out".into()),
                stats: None,
                #[cfg(target_os = "linux")]
                overlay: None,
            })
        };
        let made = match made {
            Ok(m) => m,
            Err(e) => {
                // Leave no half-made attempt behind.
                self.db
                    .execute("DELETE FROM attempts WHERE id = ?1", params![id])?;
                let _ = fs::remove_dir_all(&view_path);
                return Err(e);
            }
        };
        let backend = made.backend;

        self.db.execute(
            "UPDATE attempts SET view_path = ?1, backend = ?2 WHERE id = ?3",
            params![view_path.to_string_lossy(), backend.as_str(), id],
        )?;
        self.record(
            &req.actor,
            "attempt.fork",
            serde_json::json!({ "attempt": id, "from": from.to_string(), "backend": backend }),
        )?;
        let attempt = self.attempt(id)?;
        // Best-effort: a view without git is still a view.
        let git = self.link_view_git(&attempt).ok().flatten();
        Ok(ForkResult {
            git,
            attempt,
            parent_snapshot,
            backend,
            backend_reason: made.reason,
            stats: made.stats,
            #[cfg(target_os = "linux")]
            overlay: made.overlay,
        })
    }

    /// Clone a live parent view: reflink, else overlayfs, else copy, or the
    /// backend the caller forced.
    fn fork_view(
        &self,
        parent: &Attempt,
        id: i64,
        view_path: &Path,
        choice: BackendChoice,
    ) -> Result<WarmFork> {
        let copy = |reason: Option<String>| -> Result<WarmFork> {
            let stats = view::fork_dir(&parent.view_path, view_path, true)?;
            Ok(WarmFork {
                backend: Backend::Copy,
                reason,
                stats: Some(stats),
                #[cfg(target_os = "linux")]
                overlay: None,
            })
        };
        let views = self.views_dir();
        let reflink = match choice {
            BackendChoice::Auto | BackendChoice::Reflink => view::reflinks_supported(&views),
            _ => Err(String::new()),
        };
        match (choice, reflink) {
            (BackendChoice::Copy, _) => return copy(None),
            (BackendChoice::Reflink, Err(why)) => {
                return Err(Error::BackendUnavailable {
                    backend: "reflink",
                    reason: why,
                })
            }
            (_, Ok(())) => {
                let stats = view::fork_dir(&parent.view_path, view_path, false)?;
                return Ok(WarmFork {
                    backend: stats.backend(),
                    reason: None,
                    stats: Some(stats),
                    #[cfg(target_os = "linux")]
                    overlay: None,
                });
            }
            _ => {}
        }
        let no_reflink = match choice {
            BackendChoice::Auto => self.reflink_reason(),
            _ => String::new(),
        };
        #[cfg(target_os = "linux")]
        {
            let start = std::time::Instant::now();
            match self.overlay_fork(parent, id, view_path)? {
                Ok(info) => {
                    let stats = ForkStats {
                        files: info.freeze.files,
                        copied: info.freeze.files,
                        bytes_copied: info.freeze.bytes,
                        dirs: info.freeze.dirs,
                        symlinks: info.freeze.symlinks,
                        millis: start.elapsed().as_millis(),
                        ..Default::default()
                    };
                    let reason = (choice == BackendChoice::Auto)
                        .then(|| format!("{no_reflink}; overlayfs via {}", info.mounter.as_str()));
                    Ok(WarmFork {
                        backend: Backend::Overlay,
                        reason,
                        stats: Some(stats),
                        overlay: Some(info),
                    })
                }
                Err(why) if choice == BackendChoice::Auto => {
                    copy(Some(format!("{no_reflink}; overlayfs unavailable ({why})")))
                }
                Err(why) => Err(Error::BackendUnavailable {
                    backend: "overlay",
                    reason: why,
                }),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = id;
            let why = "overlayfs needs Linux".to_string();
            match choice {
                BackendChoice::Auto => copy(Some(format!("{no_reflink}; {why}"))),
                _ => Err(Error::BackendUnavailable {
                    backend: "overlay",
                    reason: why,
                }),
            }
        }
    }

    /// Make sure an attempt's view can be read, mounting an overlay view
    /// that is not mounted (after a reboot, or in a new process for views
    /// mounted in a user namespace). False when the view directory is gone.
    pub fn ensure_view(&self, attempt: &Attempt) -> Result<bool> {
        let ok = self.ensure_view_mounted(attempt)?;
        if ok {
            // Views made before views had git get their link on first use.
            self.ensure_view_git(attempt);
        }
        Ok(ok)
    }

    fn ensure_view_mounted(&self, attempt: &Attempt) -> Result<bool> {
        if attempt.backend != Backend::Overlay.as_str() {
            return Ok(attempt.view_path.exists());
        }
        #[cfg(target_os = "linux")]
        return self.mount_overlay_view(attempt);
        #[cfg(not(target_os = "linux"))]
        Err(Error::ViewUnmounted {
            id: attempt.id,
            reason: "overlayfs needs Linux".into(),
        })
    }

    fn reflink_reason(&self) -> String {
        let fs = fs_type(&self.views_dir());
        format!("no reflinks on {fs}")
    }

    /// Change an attempt's outcome. Promoting an attempt that belongs to a
    /// task also closes its open siblings as `abandoned(lost to aN)`.
    /// Recorded in the operation log as done by `human`.
    pub fn set_status(&self, id: i64, status: Status, reason: Option<&str>) -> Result<Attempt> {
        self.set_status_as("human", id, status, reason)
    }

    /// [`Space::set_status`], recorded as done by `actor` (e.g. `human:ana`).
    pub fn set_status_as(
        &self,
        actor: &str,
        id: i64,
        status: Status,
        reason: Option<&str>,
    ) -> Result<Attempt> {
        let attempt = self.attempt(id)?;
        if !attempt.status.is_open() {
            return Err(Error::AttemptClosed {
                id,
                status: attempt.status.to_string(),
            });
        }
        if status != Status::Abandoned && self.ensure_view(&attempt)? {
            self.snapshot(id, status.as_str(), None)?;
        }
        let closed_at = (!status.is_open()).then(now);
        self.db.execute(
            "UPDATE attempts SET status = ?1, reason = ?2, closed_at = ?3 WHERE id = ?4",
            params![status.as_str(), reason, closed_at, id],
        )?;
        let mut losers = Vec::new();
        if status == Status::Promoted {
            if let Some(task) = attempt.task_id {
                // Attempts the winner was forked from are part of the winning
                // path, not losers: they are promoted along with it.
                let mut ancestors = std::collections::HashSet::new();
                let mut cur = attempt.parent_attempt;
                while let Some(p) = cur {
                    if !ancestors.insert(p) {
                        break;
                    }
                    cur = self.attempt(p)?.parent_attempt;
                }
                for sib in self.attempts(Some(task))? {
                    if sib.id == id || !sib.status.is_open() {
                        continue;
                    }
                    if ancestors.contains(&sib.id) {
                        self.db.execute(
                            "UPDATE attempts SET status = 'promoted', reason = ?1, closed_at = ?3
                             WHERE id = ?2",
                            params![format!("continued in a{id}"), sib.id, now()],
                        )?;
                    } else {
                        self.db.execute(
                            "UPDATE attempts SET status = 'abandoned', reason = ?1, closed_at = ?3
                             WHERE id = ?2",
                            params![format!("lost to a{id}"), sib.id, now()],
                        )?;
                        losers.push(sib.id);
                    }
                }
            }
        }
        self.record(
            actor,
            &format!("attempt.{}", status.as_str()),
            serde_json::json!({ "attempt": id, "reason": reason, "closed_siblings": losers }),
        )?;
        // Abandoned views lose their git link; a promoted one keeps it for
        // `rebase` and `pr`.
        let closed = if status == Status::Abandoned {
            [&[id][..], &losers].concat()
        } else {
            losers.clone()
        };
        for a in closed {
            let _ = self.unlink_view_git(a);
        }
        #[cfg(target_os = "linux")]
        self.release_views(&[&[id][..], &losers].concat());
        self.attempt(id)
    }

    // ---- snapshots -----------------------------------------------------

    fn row_to_snapshot(r: &rusqlite::Row<'_>) -> rusqlite::Result<Snapshot> {
        let meta: Option<String> = r.get(4)?;
        Ok(Snapshot {
            id: r.get(0)?,
            attempt_id: r.get(1)?,
            tree: r.get(2)?,
            trigger: r.get(3)?,
            meta: meta.and_then(|m| serde_json::from_str(&m).ok()),
            created_at: r.get(5)?,
            pruned_at: r.get(6)?,
        })
    }

    const SNAPSHOT_COLS: &'static str =
        "id, attempt_id, tree, trigger, meta, created_at, pruned_at";

    pub fn snapshot_by_id(&self, id: i64) -> Result<Snapshot> {
        self.db
            .query_row(
                &format!(
                    "SELECT {} FROM snapshots WHERE id = ?1",
                    Self::SNAPSHOT_COLS
                ),
                params![id],
                Self::row_to_snapshot,
            )
            .optional()?
            .ok_or(Error::NotFound {
                kind: "snapshot",
                id: format!("s{id}"),
            })
    }

    pub fn snapshots(&self, attempt_id: i64) -> Result<Vec<Snapshot>> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {} FROM snapshots WHERE attempt_id = ?1 ORDER BY id",
            Self::SNAPSHOT_COLS
        ))?;
        let rows = stmt.query_map(params![attempt_id], Self::row_to_snapshot)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Snapshots of every attempt with an id above `after_id`, oldest first.
    /// Lets watchers (the daemon's event stream) pick up new work cheaply.
    pub fn snapshots_after(&self, after_id: i64) -> Result<Vec<Snapshot>> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {} FROM snapshots WHERE id > ?1 ORDER BY id",
            Self::SNAPSHOT_COLS
        ))?;
        let rows = stmt.query_map(params![after_id], Self::row_to_snapshot)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The latest snapshot of an attempt. A freshly forked attempt with no
    /// snapshot of its own reports its parent snapshot.
    pub fn head_snapshot(&self, attempt_id: i64) -> Result<Snapshot> {
        if let Some(s) = self.snapshots(attempt_id)?.pop() {
            return Ok(s);
        }
        match self.attempt(attempt_id)?.parent_snapshot {
            Some(p) => self.snapshot_by_id(p),
            None => Err(Error::NotFound {
                kind: "snapshot of attempt",
                id: format!("a{attempt_id}"),
            }),
        }
    }

    /// Capture the attempt's view. If nothing changed since its last snapshot,
    /// that snapshot is returned with `unchanged: true`.
    pub fn snapshot(
        &self,
        attempt_id: i64,
        trigger: &str,
        meta: Option<serde_json::Value>,
    ) -> Result<SnapshotResult> {
        let always = meta.is_some();
        self.snapshot_with(attempt_id, trigger, meta, always)
    }

    /// Like [`Space::snapshot`], but `always` decides whether an unchanged
    /// tree still records a new snapshot (to keep its metadata) or returns
    /// the previous one with `unchanged: true`.
    pub fn snapshot_with(
        &self,
        attempt_id: i64,
        trigger: &str,
        meta: Option<serde_json::Value>,
        always: bool,
    ) -> Result<SnapshotResult> {
        let attempt = self.attempt(attempt_id)?;
        if !attempt.status.is_open() {
            return Err(Error::AttemptClosed {
                id: attempt_id,
                status: attempt.status.to_string(),
            });
        }
        let tree = self.capture_view(&attempt)?.to_hex();
        if !always {
            // A fresh fork compares against the snapshot it was forked from.
            if let Ok(last) = self.head_snapshot(attempt_id) {
                if last.tree == tree {
                    return Ok(SnapshotResult {
                        snapshot: last,
                        unchanged: true,
                    });
                }
            }
        }
        self.db.execute(
            "INSERT INTO snapshots (attempt_id, tree, trigger, meta, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                attempt_id,
                tree,
                trigger,
                meta.as_ref().map(|m| m.to_string()),
                now()
            ],
        )?;
        let snap = self.snapshot_by_id(self.db.last_insert_rowid())?;
        Ok(SnapshotResult {
            snapshot: snap,
            unchanged: false,
        })
    }

    /// Write the current state of an attempt's view into the object store and
    /// return its tree, without recording a snapshot (for `diff` and
    /// `status` on the working state).
    pub fn capture_view(&self, attempt: &Attempt) -> Result<Oid> {
        // Never capture an unmounted overlay view: it would look empty.
        self.ensure_view(attempt)?;
        // Keep a pruning `gc` from deleting objects this capture reuses.
        let _gc = crate::gc::shared_lock(self);
        let index = self
            .workdir
            .join(SPACE_DIR)
            .join("index")
            .join(format!("a{}", attempt.id));
        snapshot::capture_indexed(&self.objects, &attempt.view_path, Some(&index))
    }

    /// Resolve a reference to a stored snapshot.
    pub fn resolve(&self, r: Ref) -> Result<Snapshot> {
        match r {
            Ref::Attempt(id) => self.head_snapshot(id),
            Ref::Snapshot(id) => self.snapshot_by_id(id),
        }
    }

    /// Files that differ between two references.
    pub fn diff(&self, a: Ref, b: Ref) -> Result<Vec<FileChange>> {
        let ta = self.resolve(a)?.tree_oid()?;
        let tb = self.resolve(b)?.tree_oid()?;
        snapshot::diff(&self.objects, &ta, &tb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (crate::testutil::TestDir, Space) {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        fs::write(w.join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(w.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(w.join("target")).unwrap();
        fs::write(w.join("target/cache"), "warm").unwrap();
        let space = Space::init(w).unwrap();
        (tmp, space)
    }

    #[test]
    fn init_creates_root_attempt_and_snapshot() {
        let (_tmp, space) = repo();
        let root = space.attempt(ROOT_ATTEMPT).unwrap();
        assert_eq!(root.status, Status::Exploring);
        assert_eq!(space.snapshots(ROOT_ATTEMPT).unwrap().len(), 1);
        assert!(matches!(
            Space::init(space.workdir()),
            Err(Error::SpaceExists(_))
        ));
    }

    #[test]
    fn fork_is_warm_and_isolated() {
        let (_tmp, space) = repo();
        let task = space.create_task("speed up parser", None).unwrap();
        let req = ForkRequest {
            task_id: Some(task.id),
            actor: "agent:test".into(),
            ..Default::default()
        };
        let a = space.fork(&req).unwrap();
        let view = &a.attempt.view_path;
        // Ignored build state is carried over.
        assert_eq!(
            fs::read_to_string(view.join("target/cache")).unwrap(),
            "warm"
        );
        fs::write(view.join("main.rs"), "fn main() { fast() }\n").unwrap();
        let s = space.snapshot(a.attempt.id, "tool", None).unwrap();
        assert!(!s.unchanged);
        assert!(
            space
                .snapshot(a.attempt.id, "tool", None)
                .unwrap()
                .unchanged
        );

        let changes = space
            .diff(Ref::Attempt(ROOT_ATTEMPT), Ref::Attempt(a.attempt.id))
            .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "main.rs");
        // The parent working directory is untouched.
        assert_eq!(
            fs::read_to_string(space.workdir().join("main.rs")).unwrap(),
            "fn main() {}\n"
        );
        assert_eq!(space.attempt_for_path(view).unwrap().id, a.attempt.id);
        assert_eq!(
            space.attempt_for_path(space.workdir()).unwrap().id,
            ROOT_ATTEMPT
        );
        // A fork of a task's attempt stays on that task unless told otherwise.
        let child = space
            .fork(&ForkRequest {
                from: Some(Ref::Attempt(a.attempt.id)),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(child.attempt.task_id, Some(task.id));
    }

    #[test]
    fn fork_from_old_snapshot_materializes() {
        let (_tmp, space) = repo();
        let first = space.head_snapshot(ROOT_ATTEMPT).unwrap();
        fs::write(space.workdir().join("main.rs"), "changed\n").unwrap();
        space.snapshot(ROOT_ATTEMPT, "manual", None).unwrap();
        let req = ForkRequest {
            from: Some(Ref::Snapshot(first.id)),
            actor: "human".into(),
            ..Default::default()
        };
        let a = space.fork(&req).unwrap();
        assert_eq!(a.backend, Backend::Materialize);
        assert_eq!(
            fs::read_to_string(a.attempt.view_path.join("main.rs")).unwrap(),
            "fn main() {}\n"
        );
        // Materialized views are cold: ignored build output is not stored.
        assert!(!a.attempt.view_path.join("target").exists());
    }

    #[test]
    fn promote_closes_siblings() {
        let (_tmp, space) = repo();
        let task = space.create_task("t", None).unwrap();
        let mk = || {
            space
                .fork(&ForkRequest {
                    task_id: Some(task.id),
                    actor: "agent".into(),
                    ..Default::default()
                })
                .unwrap()
                .attempt
                .id
        };
        let (a, b, c) = (mk(), mk(), mk());
        space.set_status(c, Status::Failed, None).unwrap();
        space.set_status(a, Status::Promoted, None).unwrap();
        assert_eq!(space.attempt(a).unwrap().status, Status::Promoted);
        let lost = space.attempt(b).unwrap();
        assert_eq!(lost.status, Status::Abandoned);
        assert_eq!(lost.reason.as_deref(), Some(&*format!("lost to a{a}")));
        assert_eq!(space.attempt(c).unwrap().status, Status::Abandoned);
        assert!(matches!(
            space.set_status(b, Status::Passed, None),
            Err(Error::AttemptClosed { .. })
        ));
    }

    #[test]
    fn status_changes_record_the_actor() {
        let (_tmp, space) = repo();
        let a = space
            .fork(&ForkRequest {
                actor: "agent".into(),
                ..Default::default()
            })
            .unwrap()
            .attempt
            .id;
        space.set_status(a, Status::Passed, None).unwrap();
        space
            .set_status_as("human:ana", a, Status::Abandoned, Some("no"))
            .unwrap();
        let actors: Vec<(String, String)> = space
            .db
            .prepare("SELECT actor, kind FROM operations WHERE kind LIKE 'attempt.%' AND kind != 'attempt.fork' ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            actors,
            [
                ("human".into(), "attempt.passed".into()),
                ("human:ana".into(), "attempt.abandoned".into())
            ]
        );
    }

    #[test]
    fn promote_keeps_ancestors_on_the_winning_path() {
        let (_tmp, space) = repo();
        let task = space.create_task("t", None).unwrap();
        let fork = |from| {
            space
                .fork(&ForkRequest {
                    task_id: Some(task.id),
                    from,
                    actor: "agent".into(),
                    ..Default::default()
                })
                .unwrap()
                .attempt
                .id
        };
        let parent = fork(None);
        let sibling = fork(None);
        let child = fork(Some(Ref::Attempt(parent)));
        space.set_status(child, Status::Promoted, None).unwrap();
        let p = space.attempt(parent).unwrap();
        assert_eq!(p.status, Status::Promoted);
        assert_eq!(
            p.reason.as_deref(),
            Some(&*format!("continued in a{child}"))
        );
        let s = space.attempt(sibling).unwrap();
        assert_eq!(s.status, Status::Abandoned);
        assert_eq!(s.reason.as_deref(), Some(&*format!("lost to a{child}")));
    }

    #[test]
    fn refs_parse() {
        assert_eq!("a12".parse::<Ref>().unwrap(), Ref::Attempt(12));
        assert_eq!("s3".parse::<Ref>().unwrap(), Ref::Snapshot(3));
        assert!("x3".parse::<Ref>().is_err());
        assert!("a".parse::<Ref>().is_err());
    }
}
