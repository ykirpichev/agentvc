//! Rebasing an attempt onto a newer commit of the host repository.
//!
//! An attempt's changes are defined against a git commit: the host HEAD
//! recorded at `agentvc init`, or the commit it was last rebased onto. When
//! the host's main branch moves on, `rebase` three-way merges the attempt's
//! head tree with the new commit, using the old base commit as the merge
//! base, writes the result into the attempt's view and records it as a new
//! snapshot. From then on `export` and `pr` parent on the new commit, and
//! `diff aN` shows the attempt's changes relative to it.
//!
//! The merge itself is `git merge-tree --write-tree` in the host repository,
//! so rename detection and conflict markers behave exactly like git's. It
//! touches no ref, index or working tree there.
//!
//! Bases are recorded per snapshot (table `git_bases`): a snapshot's base is
//! the latest rebase of its attempt at or before it, else the base of the
//! snapshot the attempt was forked from, and so on up to the root attempt,
//! whose base is the HEAD recorded at init. A child forked before its parent
//! was rebased keeps the parent's old base, and a sibling is never affected.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::error::{Error, IoContext, Result};
use crate::export::{copy_tree_objects, HostRepo, IMPORT_BASE_KEY};
use crate::objects::{parse_tree, Mode, ObjectStore, Oid};
use crate::resolve::{merge_file, Resolution, Rules, Sides, Strategy};
use crate::snapshot::{self, ChangeKind, FileChange};
use crate::space::{now, Snapshot, Space, Status, ROOT_ATTEMPT, SPACE_DIR};

#[derive(Clone, Debug, Default)]
pub struct RebaseOptions {
    /// Git revision to rebase onto. Defaults to the remote's default branch,
    /// fetched first.
    pub onto: Option<String>,
    /// Remote whose default branch is the default target. Defaults to `origin`.
    pub remote: Option<String>,
    /// Do not fetch the remote before resolving the default target.
    pub no_fetch: bool,
    /// On conflicts, write files with conflict markers into the view and mark
    /// the attempt `exploring` instead of failing.
    pub keep_conflicts: bool,
    /// Report git's conflicts as they are, without resolution strategies.
    pub no_auto_resolve: bool,
    /// Work out the result and report it, changing nothing: which files
    /// merge cleanly, which a strategy resolves and which conflict.
    pub preview: bool,
    /// Who rebases, for the operation log. Defaults to the attempt's actor.
    pub actor: Option<String>,
}

/// The git commit a snapshot's tree is based on.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct GitBase {
    /// The attempt that was rebased.
    pub attempt_id: i64,
    /// The snapshot that the rebase recorded.
    pub snapshot_id: i64,
    pub commit: String,
    /// The commit's tree, also stored in the space, so diffs against the base
    /// need no git.
    pub tree: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RebaseResult {
    pub attempt: i64,
    /// The commit rebased onto.
    pub onto: String,
    /// The revision `onto` was resolved from, such as `origin/main`.
    pub onto_ref: String,
    /// The commit the attempt was based on before.
    pub from: Option<String>,
    /// False when the attempt was already based on `onto` and nothing changed.
    pub rebased: bool,
    /// The snapshot recording the rebased tree.
    pub snapshot: Option<Snapshot>,
    /// Files the rebase changed in the attempt's tree (what the new base
    /// brought in, plus merged files).
    pub changes: Vec<FileChange>,
    /// Files left with conflict markers (only with `keep_conflicts`).
    pub conflicts: Vec<String>,
    /// Files whose conflicts a resolution strategy resolved (see `resolve`).
    pub resolved: Vec<Resolution>,
    /// For a preview: hunks left per conflicted file.
    pub conflict_files: Vec<Resolution>,
    /// True for `preview`: nothing was changed.
    pub preview: bool,
    /// Whether the view on disk was updated. False for a closed attempt whose
    /// view is gone; history is updated either way.
    pub view_updated: bool,
    pub status: Status,
}

const BASE_COLS: &str = "attempt_id, snapshot_id, commit_id, tree, created_at";

fn row_to_base(r: &rusqlite::Row<'_>) -> rusqlite::Result<GitBase> {
    Ok(GitBase {
        attempt_id: r.get(0)?,
        snapshot_id: r.get(1)?,
        commit: r.get(2)?,
        tree: r.get(3)?,
        created_at: r.get(4)?,
    })
}

impl Space {
    /// The rebase that `snapshot` is based on, if it or one of the snapshots
    /// it descends from was recorded by a rebase.
    pub fn git_base(&self, snapshot: &Snapshot) -> Result<Option<GitBase>> {
        let mut snap = snapshot.clone();
        for _ in 0..100_000 {
            let found = self
                .db
                .query_row(
                    &format!(
                        "SELECT {BASE_COLS} FROM git_bases
                         WHERE attempt_id = ?1 AND snapshot_id <= ?2
                         ORDER BY snapshot_id DESC, id DESC LIMIT 1"
                    ),
                    params![snap.attempt_id, snap.id],
                    row_to_base,
                )
                .optional()?;
            if found.is_some() {
                return Ok(found);
            }
            match self.attempt(snap.attempt_id)?.parent_snapshot {
                Some(p) => snap = self.snapshot_by_id(p)?,
                None => return Ok(None),
            }
        }
        Ok(None)
    }

    /// The git commit `snapshot` is based on: its rebase target, else the
    /// host HEAD recorded at init.
    pub fn git_base_commit(&self, snapshot: &Snapshot) -> Result<Option<String>> {
        if let Some(b) = self.git_base(snapshot)? {
            return Ok(Some(b.commit));
        }
        let import = self.meta(IMPORT_BASE_KEY)?;
        // Snapshots of the root taken before the root recorded its own bases
        // (#96): if the root snapshot this one descends from is not the
        // init commit's tree but is exactly the checkout's HEAD tree, it was
        // taken from that HEAD.
        let mut snap = snapshot.clone();
        while snap.attempt_id != ROOT_ATTEMPT {
            match self.attempt(snap.attempt_id)?.parent_snapshot {
                Some(p) => snap = self.snapshot_by_id(p)?,
                None => return Ok(import),
            }
        }
        let Ok(repo) = HostRepo::open(self.workdir()) else {
            return Ok(import);
        };
        let tree_of = |c: &str| {
            repo.git(&["rev-parse", &format!("{c}^{{tree}}")], None)
                .ok()
        };
        if let Some(i) = &import {
            if tree_of(i).as_deref() == Some(snap.tree.as_str()) {
                return Ok(import);
            }
        }
        match repo.rev("HEAD") {
            Some(head) if tree_of(&head).as_deref() == Some(snap.tree.as_str()) => Ok(Some(head)),
            _ => Ok(import),
        }
    }

    /// Record the host checkout's HEAD as the git base of a snapshot of the
    /// root attempt (the checkout itself) when it differs from the base the
    /// snapshot would otherwise get. The root's tree follows the checkout, so
    /// after a `git pull`, or in a space restored into a newer checkout
    /// (#96), the commit recorded at init no longer describes it, and a
    /// rebase from that old commit would conflict in every file main
    /// changed since. Best effort: outside git nothing is recorded.
    pub(crate) fn note_root_base(&self, snap: &Snapshot) {
        let _ = (|| -> Result<()> {
            let Ok(repo) = HostRepo::open(self.workdir()) else {
                return Ok(());
            };
            let Some(head) = repo.rev("HEAD") else {
                return Ok(());
            };
            let recorded = match self.git_base(snap)? {
                Some(b) => Some(b.commit),
                None => self.meta(IMPORT_BASE_KEY)?,
            };
            if recorded.as_deref() == Some(head.as_str()) {
                return Ok(());
            }
            let tree = repo.git(&["rev-parse", &format!("{head}^{{tree}}")], None)?;
            let mut git = CatFile::start(&repo)?;
            import_tree(self.objects(), &mut git, &Oid::from_hex(&tree)?)?;
            self.db.execute(
                "INSERT INTO git_bases (attempt_id, snapshot_id, commit_id, tree, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![snap.attempt_id, snap.id, head, tree, now()],
            )?;
            Ok(())
        })();
    }

    /// The latest rebase of this attempt itself, which `diff aN` compares
    /// against instead of the parent snapshot.
    pub fn own_git_base(&self, attempt_id: i64) -> Result<Option<GitBase>> {
        Ok(self
            .db
            .query_row(
                &format!(
                    "SELECT {BASE_COLS} FROM git_bases WHERE attempt_id = ?1
                     ORDER BY snapshot_id DESC, id DESC LIMIT 1"
                ),
                params![attempt_id],
                row_to_base,
            )
            .optional()?)
    }

    /// A recorded rebase onto `commit` (full id), from any attempt. Its
    /// tree is stored in the space, so it can be read like a snapshot's.
    pub fn git_base_by_commit(&self, commit: &str) -> Result<Option<GitBase>> {
        Ok(self
            .db
            .query_row(
                &format!(
                    "SELECT {BASE_COLS} FROM git_bases WHERE commit_id = ?1
                     ORDER BY id DESC LIMIT 1"
                ),
                params![commit],
                row_to_base,
            )
            .optional()?)
    }

    /// Files the attempt changed: against the commit it was last rebased
    /// onto, else against the snapshot it was forked from. Empty for an
    /// attempt with no parent.
    pub fn own_changes(&self, attempt_id: i64) -> Result<Vec<FileChange>> {
        let head = Oid::from_hex(&self.head_snapshot(attempt_id)?.tree)?;
        let base = match self.own_git_base(attempt_id)? {
            Some(b) => b.tree,
            None => match self.attempt(attempt_id)?.parent_snapshot {
                Some(p) => self.snapshot_by_id(p)?.tree,
                None => return Ok(Vec::new()),
            },
        };
        snapshot::diff(self.objects(), &Oid::from_hex(&base)?, &head)
    }
}

/// Resolve the commit to rebase onto, fetching the remote for the default.
pub(crate) fn resolve_onto(repo: &HostRepo, opts: &RebaseOptions) -> Result<(String, String)> {
    if let Some(rev) = &opts.onto {
        let c = repo.rev(rev).ok_or_else(|| Error::NotFound {
            kind: "git revision",
            id: rev.clone(),
        })?;
        return Ok((c, rev.clone()));
    }
    let remote = opts.remote.as_deref().unwrap_or("origin");
    if remote.starts_with('-') {
        return Err(Error::NotFound {
            kind: "git remote",
            id: remote.to_string(),
        });
    }
    if !opts.no_fetch {
        repo.git(&["fetch", "--quiet", remote], None)?;
    }
    // origin/HEAD names the default branch in clones; plain `remote add`
    // remotes lack it, so fall back to the usual names.
    let head = repo
        .git(
            &[
                "symbolic-ref",
                "--quiet",
                &format!("refs/remotes/{remote}/HEAD"),
            ],
            None,
        )
        .ok()
        .and_then(|r| r.strip_prefix("refs/remotes/").map(str::to_string));
    let candidates = head
        .into_iter()
        .chain([format!("{remote}/main"), format!("{remote}/master")]);
    for name in candidates {
        if let Some(c) = repo.rev(&format!("refs/remotes/{name}")) {
            return Ok((c, name));
        }
    }
    Err(Error::NotFound {
        kind: "default branch of remote",
        id: remote.to_string(),
    })
}

/// The result of a three-way merge after conflict resolution.
pub(crate) struct Merge {
    pub tree: String,
    /// Paths that still conflict.
    pub conflicts: Vec<String>,
    /// Every path git reported as conflicted, with what resolution did.
    pub(crate) files: Vec<Resolution>,
    /// The merged blob of each path resolution rewrote.
    pub(crate) blobs: BTreeMap<String, String>,
    /// Blob ids of the base, ours and theirs sides of each conflicted path.
    pub(crate) stages: BTreeMap<String, [Option<String>; 3]>,
}

impl Merge {
    /// The files a strategy resolved completely.
    pub(crate) fn resolved(&self) -> Vec<Resolution> {
        self.files
            .iter()
            .filter(|f| f.remaining == 0 && f.hunks > 0)
            .cloned()
            .collect()
    }
}

/// Three-way merge tree `ours` (wrapped in a commit on `base`) with commit
/// `theirs`, then resolve what conflicts with `rules`.
pub(crate) fn merge_trees(
    repo: &HostRepo,
    base: &str,
    ours: &str,
    theirs: &str,
    rules: &Rules,
    sides: &Sides,
) -> Result<Merge> {
    // merge-tree in git < 2.45 only merges commits, so wrap the attempt's tree
    // in a dangling commit on the old base. No ref points at it.
    let ident = [
        ("GIT_AUTHOR_NAME", "agentvc"),
        ("GIT_AUTHOR_EMAIL", "agentvc@localhost"),
        ("GIT_COMMITTER_NAME", "agentvc"),
        ("GIT_COMMITTER_EMAIL", "agentvc@localhost"),
    ];
    let ours = repo.git_env(
        &["commit-tree", ours, "-p", base, "-m", "agentvc rebase"],
        None,
        &ident,
    )?;
    merge_commits(repo, Some(base), &ours, theirs, rules, sides)
}

fn is_file_mode(mode: &str) -> bool {
    mode == "100644" || mode == "100755"
}

/// `git merge-tree` of two commits, then deterministic resolution of the
/// conflicted text files (see [`crate::resolve`]). Files git reports as
/// conflicted are merged again line by line; resolved ones get the merged
/// blob, the rest get diff3-style markers. Binary files, symlinks and
/// rename/delete conflicts keep git's result.
pub(crate) fn merge_commits(
    repo: &HostRepo,
    base: Option<&str>,
    ours: &str,
    theirs: &str,
    rules: &Rules,
    sides: &Sides,
) -> Result<Merge> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(&repo.workdir)
        .args(["merge-tree", "--write-tree", "-z"]);
    if let Some(base) = base {
        cmd.arg(format!("--merge-base={base}"));
    }
    let out = cmd
        .args([ours, theirs])
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .output()
        .at("git")?;
    let code = out.status.code();
    if code != Some(0) && code != Some(1) {
        return Err(Error::Git {
            cmd: "merge-tree".into(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    // <tree> NUL (<mode> <oid> <stage> TAB <path> NUL)* NUL <messages>; the
    // conflicted entries only on conflicts.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut fields = stdout.split('\0');
    let mut tree = fields.next().unwrap_or_default().trim().to_string();
    Oid::from_hex(&tree)?;
    let mut paths: Vec<String> = Vec::new();
    // Per path: (mode, oid) of stages 1 (base), 2 (ours) and 3 (theirs).
    let mut stages: BTreeMap<String, [Option<(String, String)>; 3]> = BTreeMap::new();
    if code == Some(1) {
        for f in fields {
            if f.is_empty() {
                break;
            }
            let Some((info, path)) = f.split_once('\t') else {
                continue;
            };
            let parts: Vec<&str> = info.split(' ').collect();
            if paths.last().map(String::as_str) != Some(path) {
                paths.push(path.to_string());
            }
            if let [mode, oid, stage] = parts.as_slice() {
                if let Ok(n @ 1..=3) = stage.parse::<usize>() {
                    stages.entry(path.to_string()).or_default()[n - 1] =
                        Some((mode.to_string(), oid.to_string()));
                }
            }
        }
    }
    let mut merge = Merge {
        tree: tree.clone(),
        conflicts: Vec::new(),
        files: Vec::new(),
        blobs: BTreeMap::new(),
        stages: stages
            .iter()
            .map(|(p, st)| {
                let ids = [0, 1, 2].map(|i| {
                    st[i]
                        .as_ref()
                        .filter(|(m, _)| is_file_mode(m))
                        .map(|(_, o)| o.clone())
                });
                (p.clone(), ids)
            })
            .collect(),
    };
    if paths.is_empty() {
        return Ok(merge);
    }
    if !rules.enabled {
        merge.conflicts = paths;
        return Ok(merge);
    }
    let mut rules = rules.clone();
    rules.load_attributes(&repo.workdir, &paths);
    let mut git = CatFile::start(repo)?;
    let mut text = |oid: &str| -> Result<Option<String>> {
        let (_, data) = git.read(&Oid::from_hex(oid)?)?;
        Ok(String::from_utf8(data).ok().filter(|t| !t.contains('\0')))
    };
    let mut updates = String::new();
    for path in paths {
        let strategy = rules.strategy_for(&path);
        let st = stages.get(&path).cloned().unwrap_or_default();
        let sides_ok = matches!((&st[1], &st[2]), (Some((mo, _)), Some((mt, _))) if is_file_mode(mo) && is_file_mode(mt))
            && st[0].as_ref().is_none_or(|(m, _)| is_file_mode(m));
        let texts = if sides_ok {
            let b = match &st[0] {
                Some((_, oid)) => text(oid)?.map(Some),
                None => Some(None),
            };
            let o = text(&st[1].as_ref().expect("checked").1)?;
            let t = text(&st[2].as_ref().expect("checked").1)?;
            match (b, o, t) {
                (Some(b), Some(o), Some(t)) => Some((b, o, t)),
                _ => None,
            }
        } else {
            None
        };
        let Some((b, o, t)) = texts else {
            merge.files.push(Resolution {
                path: path.clone(),
                strategy: Strategy::None,
                hunks: 0,
                remaining: 1,
            });
            merge.conflicts.push(path);
            continue;
        };
        let fm = merge_file(
            b.as_deref(),
            &o,
            &t,
            strategy,
            rules.formatter_for(&path),
            sides,
        );
        let (mb, mo, mt) = (
            st[0].as_ref().map(|s| s.0.as_str()),
            st[1].as_ref().map(|s| s.0.as_str()).unwrap_or("100644"),
            st[2].as_ref().map(|s| s.0.as_str()).unwrap_or("100644"),
        );
        let mode = if mo == mt || mb == Some(mt) { mo } else { mt };
        let blob = repo.git(&["hash-object", "-w", "--stdin"], Some(&fm.text))?;
        updates.push_str(&format!("{mode} {blob}\t{path}\0"));
        merge.blobs.insert(path.clone(), blob);
        merge.files.push(Resolution {
            path: path.clone(),
            strategy: fm.strategy,
            hunks: fm.resolved,
            remaining: fm.conflicts,
        });
        if fm.conflicts > 0 {
            merge.conflicts.push(path);
        }
    }
    if !updates.is_empty() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let index = std::env::temp_dir().join(format!(
            "agentvc-merge-{}-{nanos}.index",
            std::process::id()
        ));
        let index_s = index.to_string_lossy().to_string();
        let env = [("GIT_INDEX_FILE", index_s.as_str())];
        let written = (|| {
            repo.git_env(&["read-tree", &tree], None, &env)?;
            repo.git_env(
                &["update-index", "-z", "--index-info"],
                Some(&updates),
                &env,
            )?;
            repo.git_env(&["write-tree"], None, &env)
        })();
        let _ = fs::remove_file(&index);
        tree = written?;
        merge.tree = tree;
    }
    Ok(merge)
}

/// A long-running `git cat-file --batch`, for reading many objects.
pub(crate) struct CatFile {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl CatFile {
    pub(crate) fn start(repo: &HostRepo) -> Result<CatFile> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&repo.workdir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .at("git")?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
        Ok(CatFile {
            child,
            stdin,
            stdout,
        })
    }

    pub(crate) fn read(&mut self, oid: &Oid) -> Result<(String, Vec<u8>)> {
        let hex = oid.to_hex();
        writeln!(self.stdin, "{hex}").at("git cat-file")?;
        self.stdin.flush().at("git cat-file")?;
        let mut header = String::new();
        self.stdout.read_line(&mut header).at("git cat-file")?;
        let parts: Vec<&str> = header.split_whitespace().collect();
        let (kind, size) = match parts.as_slice() {
            [_, kind, size] => (kind.to_string(), size.parse::<usize>().ok()),
            _ => (String::new(), None),
        };
        let size = size.ok_or_else(|| Error::NotFound {
            kind: "git object",
            id: hex.clone(),
        })?;
        let mut data = vec![0; size + 1];
        self.stdout.read_exact(&mut data).at("git cat-file")?;
        data.pop();
        Ok((kind, data))
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Copy tree `oid` from the host repo into the space's store. Subtrees that
/// are already stored are skipped: a stored tree always has all its children,
/// because children are written first.
pub(crate) fn import_tree(store: &ObjectStore, git: &mut CatFile, oid: &Oid) -> Result<()> {
    if store.contains(oid) {
        return Ok(());
    }
    let (kind, data) = git.read(oid)?;
    if kind != "tree" {
        return Err(Error::BadObject(oid.to_hex()));
    }
    for e in parse_tree(oid, &data)? {
        if e.mode == Mode::Tree {
            import_tree(store, git, &e.oid)?;
        } else if !store.contains(&e.oid) {
            let (kind, blob) = git.read(&e.oid)?;
            if kind != "blob" {
                return Err(Error::BadObject(e.oid.to_hex()));
            }
            store.write_raw("blob", &blob)?;
        }
    }
    store.write_raw("tree", &data)?;
    Ok(())
}

/// Bring a view from tree `from` to tree `to`, touching only changed paths,
/// so ignored build output in the view stays warm.
pub(crate) fn write_changes(
    store: &ObjectStore,
    view: &Path,
    to: &Oid,
    changes: &[FileChange],
) -> Result<()> {
    let target: BTreeMap<String, (Mode, Oid)> = snapshot::flatten(store, to)?;
    // Removals first, so a file can become a directory and back.
    for c in changes.iter().filter(|c| c.kind == ChangeKind::Removed) {
        let path = view.join(&c.path);
        match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).at(&path),
            _ => {}
        }
        // Prune directories the removal left empty; stops at the first one
        // that still holds something.
        let mut dir = path.parent();
        while let Some(d) = dir {
            if d == view || fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
    for c in changes.iter().filter(|c| c.kind != ChangeKind::Removed) {
        let Some((mode, oid)) = target.get(&c.path) else {
            continue;
        };
        let path = view.join(&c.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).at(parent)?;
        }
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.is_dir() {
                fs::remove_dir_all(&path).at(&path)?;
            } else if meta.file_type().is_symlink() || *mode == Mode::Symlink {
                fs::remove_file(&path).at(&path)?;
            }
        }
        let data = store.read_blob(oid)?;
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
                {
                    use std::os::unix::fs::PermissionsExt;
                    let m = if *mode == Mode::Executable {
                        0o755
                    } else {
                        0o644
                    };
                    fs::set_permissions(&path, fs::Permissions::from_mode(m)).at(&path)?;
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn insert_snapshot(
    space: &Space,
    attempt_id: i64,
    tree: &str,
    trigger: &str,
    meta: Option<serde_json::Value>,
) -> Result<Snapshot> {
    space.db.execute(
        "INSERT INTO snapshots (attempt_id, tree, trigger, meta, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            attempt_id,
            tree,
            trigger,
            meta.map(|m| m.to_string()),
            now()
        ],
    )?;
    space.snapshot_by_id(space.db.last_insert_rowid())
}

/// Rebase attempt `attempt_id` onto a newer commit of the host repository.
///
/// On conflicts without `keep_conflicts`, returns [`Error::Conflict`] and
/// leaves the view and history as they were.
pub fn rebase(space: &Space, attempt_id: i64, opts: &RebaseOptions) -> Result<RebaseResult> {
    if attempt_id == ROOT_ATTEMPT {
        return Err(Error::RootAttempt);
    }
    let attempt = space.attempt(attempt_id)?;
    // A promoted attempt is usually rebased right before `pr`, so it is
    // allowed; an abandoned one is history.
    if attempt.status == Status::Abandoned {
        return Err(Error::AttemptClosed {
            id: attempt_id,
            status: attempt.status.to_string(),
        });
    }
    let repo = HostRepo::open(space.workdir())?;
    let (onto, onto_ref) = resolve_onto(&repo, opts)?;

    let view_ok = space.ensure_view(&attempt)?;
    let head = space.head_snapshot(attempt_id)?;
    // The state to rebase is the view as it is now. Capture it without
    // recording, so a failed rebase leaves history untouched.
    let current = if attempt.status.is_open() && view_ok {
        let index = space
            .workdir()
            .join(SPACE_DIR)
            .join("index")
            .join(format!("a{attempt_id}"));
        snapshot::capture_with_modes(
            space.objects(),
            &attempt.view_path,
            Some(&index),
            head.tree_oid().ok().as_ref(),
        )?
        .to_hex()
    } else {
        head.tree.clone()
    };

    let from = space
        .git_base_commit(&head)?
        .and_then(|c| repo.rev(&c))
        .ok_or_else(|| Error::NotFound {
            kind: "git base commit of",
            id: format!("a{attempt_id}"),
        })?;
    if from == onto {
        return Ok(RebaseResult {
            attempt: attempt_id,
            onto,
            onto_ref,
            from: Some(from),
            rebased: false,
            snapshot: None,
            changes: Vec::new(),
            conflicts: Vec::new(),
            resolved: Vec::new(),
            conflict_files: Vec::new(),
            preview: opts.preview,
            view_updated: false,
            status: attempt.status,
        });
    }

    copy_tree_objects(space, &Oid::from_hex(&current)?, &repo.objects_dir()?)?;
    let rules = if opts.no_auto_resolve {
        Rules::disabled()
    } else {
        Rules::from_space(space)?
    };
    let sides = Sides {
        ours: format!("a{attempt_id}"),
        theirs: onto_ref.clone(),
        theirs_first: false,
    };
    let merge = merge_trees(&repo, &from, &current, &onto, &rules, &sides)?;
    let resolved = merge.resolved();
    let conflict_files: Vec<Resolution> = merge
        .files
        .iter()
        .filter(|f| f.remaining > 0)
        .cloned()
        .collect();
    if opts.preview {
        let changes = snapshot::diff(space.objects(), &Oid::from_hex(&current)?, &{
            let mut git = CatFile::start(&repo)?;
            let merged = Oid::from_hex(&merge.tree)?;
            import_tree(space.objects(), &mut git, &merged)?;
            merged
        })?;
        return Ok(RebaseResult {
            attempt: attempt_id,
            onto,
            onto_ref,
            from: Some(from),
            rebased: false,
            snapshot: None,
            changes,
            conflicts: merge.conflicts,
            resolved,
            conflict_files,
            preview: true,
            view_updated: false,
            status: attempt.status,
        });
    }
    let keep = !merge.conflicts.is_empty();
    if keep && !(opts.keep_conflicts && view_ok) {
        return Err(Error::Conflict {
            attempt: attempt_id,
            onto,
            paths: merge.conflicts,
        });
    }

    let onto_tree = repo.git(&["rev-parse", &format!("{onto}^{{tree}}")], None)?;
    let mut git = CatFile::start(&repo)?;
    let merged = Oid::from_hex(&merge.tree)?;
    import_tree(space.objects(), &mut git, &merged)?;
    import_tree(space.objects(), &mut git, &Oid::from_hex(&onto_tree)?)?;
    drop(git);

    let changes = snapshot::diff(space.objects(), &Oid::from_hex(&current)?, &merged)?;
    if view_ok {
        write_changes(space.objects(), &attempt.view_path, &merged, &changes)?;
    }

    // Record what the view held before, then the rebased state.
    if current != head.tree {
        insert_snapshot(space, attempt_id, &current, "pre-rebase", None)?;
    }
    let mut meta = serde_json::json!({ "onto": onto, "onto_ref": onto_ref, "from": from });
    if keep {
        meta["conflicts"] = serde_json::json!(merge.conflicts);
    }
    if !resolved.is_empty() {
        meta["auto_resolved"] = serde_json::json!(resolved);
    }
    let snap = insert_snapshot(space, attempt_id, &merge.tree, "rebase", Some(meta))?;
    space.record_conflicts(&repo, attempt_id, snap.id, "rebase", &merge, &sides)?;
    space.db.execute(
        "INSERT INTO git_bases (attempt_id, snapshot_id, commit_id, tree, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![attempt_id, snap.id, onto, onto_tree, now()],
    )?;
    if view_ok {
        // Move the view's git HEAD and index to the new base.
        let _ = space.link_view_git(&space.attempt(attempt_id)?);
    }
    if keep && attempt.status != Status::Exploring {
        space.db.execute(
            "UPDATE attempts SET status = 'exploring', reason = ?1 WHERE id = ?2",
            params![
                format!("rebase conflicts in {} files", merge.conflicts.len()),
                attempt_id
            ],
        )?;
    }
    let actor = opts.actor.clone().unwrap_or_else(|| attempt.actor.clone());
    space.record(
        &actor,
        "attempt.rebase",
        serde_json::json!({
            "attempt": attempt_id, "onto": onto, "onto_ref": onto_ref, "from": from,
            "snapshot": snap.id, "conflicts": merge.conflicts,
            "auto_resolved": resolved,
        }),
    )?;
    Ok(RebaseResult {
        attempt: attempt_id,
        onto,
        onto_ref,
        from: Some(from),
        rebased: true,
        snapshot: Some(snap),
        changes,
        conflicts: merge.conflicts,
        resolved,
        conflict_files,
        preview: false,
        view_updated: view_ok,
        status: space.attempt(attempt_id)?.status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{export, ExportOptions};
    use crate::space::{ForkRequest, Ref};
    use crate::view::BackendChoice;

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

    /// A repo with one commit on main and a space on top of it.
    fn repo() -> (crate::testutil::TestDir, Space, String) {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        git(w, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(w.join("src")).unwrap();
        fs::write(w.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
        fs::write(w.join("README.md"), "# demo\n\nline one\n").unwrap();
        fs::write(w.join("old.txt"), "to be removed\n").unwrap();
        fs::write(w.join(".gitignore"), "target/\n").unwrap();
        git(w, &["add", "-A"]);
        git(w, &["commit", "-q", "-m", "base"]);
        let head = git(w, &["rev-parse", "HEAD"]);
        let space = Space::init(w).unwrap();
        (tmp, space, head)
    }

    /// Land a commit on main in the host checkout, as `git pull` would.
    fn advance_main(w: &Path, readme: &str) -> String {
        fs::write(w.join("README.md"), readme).unwrap();
        fs::create_dir_all(w.join("docs")).unwrap();
        fs::write(w.join("docs/new.md"), "from main\n").unwrap();
        fs::remove_file(w.join("old.txt")).unwrap();
        git(w, &["add", "-A"]);
        git(w, &["commit", "-q", "-m", "main moves on"]);
        git(w, &["rev-parse", "HEAD"])
    }

    /// Fork with `backend`, or None when that backend is unavailable here.
    fn fork(space: &Space, backend: BackendChoice) -> Option<crate::space::Attempt> {
        match space.fork(&ForkRequest {
            actor: "agent:test".into(),
            backend,
            ..Default::default()
        }) {
            Ok(r) => Some(r.attempt),
            Err(Error::BackendUnavailable { .. }) => None,
            Err(e) => panic!("fork: {e}"),
        }
    }

    fn onto(rev: &str) -> RebaseOptions {
        RebaseOptions {
            onto: Some(rev.into()),
            ..Default::default()
        }
    }

    const BACKENDS: [BackendChoice; 3] = [
        BackendChoice::Copy,
        BackendChoice::Reflink,
        BackendChoice::Overlay,
    ];

    #[test]
    fn clean_rebase_then_export_has_only_own_changes() {
        for backend in BACKENDS {
            let (tmp, space, c0) = repo();
            let w = tmp.path();
            let Some(a) = fork(&space, backend) else {
                eprintln!("skipping {backend:?}: unavailable here");
                continue;
            };
            eprintln!("{backend:?} -> {}", a.backend);
            let sib = fork(&space, BackendChoice::Copy).unwrap();
            let view = &a.view_path;
            fs::write(view.join("src/lib.rs"), "pub fn f() -> u32 { 2 }\n").unwrap();
            fs::create_dir_all(view.join("target")).unwrap();
            fs::write(view.join("target/warm"), "build cache").unwrap();
            space.snapshot(a.id, "tool", None).unwrap();
            let c1 = advance_main(w, "# demo\n\nline one\nline two\n");

            let r = rebase(&space, a.id, &onto("main")).unwrap();
            assert!(r.rebased, "{backend:?}");
            assert_eq!(r.from.as_deref(), Some(&*c0));
            assert_eq!(r.onto, c1);
            assert!(r.view_updated && r.conflicts.is_empty());
            let snap = r.snapshot.unwrap();
            assert_eq!(snap.trigger, "rebase");
            assert_eq!(snap.meta.as_ref().unwrap()["onto"], c1);

            // The view has main's changes and keeps its own and its build cache.
            let read = |p: &str| fs::read_to_string(view.join(p)).unwrap();
            assert_eq!(read("README.md"), "# demo\n\nline one\nline two\n");
            assert_eq!(read("docs/new.md"), "from main\n");
            assert!(!view.join("old.txt").exists());
            assert_eq!(read("src/lib.rs"), "pub fn f() -> u32 { 2 }\n");
            assert_eq!(read("target/warm"), "build cache");
            // Capturing the view again gives the recorded tree.
            assert!(space.snapshot(a.id, "tool", None).unwrap().unchanged);

            // diff aN shows only the attempt's own change.
            let own: Vec<_> = space
                .own_changes(a.id)
                .unwrap()
                .into_iter()
                .map(|c| c.path)
                .collect();
            assert_eq!(own, ["src/lib.rs"]);

            // The sibling is untouched and still based on c0.
            assert_eq!(
                fs::read_to_string(sib.view_path.join("README.md")).unwrap(),
                "# demo\n\nline one\n"
            );
            let sib_head = space.head_snapshot(sib.id).unwrap();
            assert_eq!(space.git_base_commit(&sib_head).unwrap(), Some(c0.clone()));

            // Export parents on c1, and the commit changes only src/lib.rs.
            let e = export(
                &space,
                a.id,
                &ExportOptions {
                    branch: "agentvc/rebased".into(),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(e.parent.as_deref(), Some(&*c1));
            assert_eq!(
                git(w, &["diff", "--name-only", &c1, &e.commit]),
                "src/lib.rs"
            );

            // A second rebase onto the same commit is a no-op.
            let again = rebase(&space, a.id, &onto("main")).unwrap();
            assert!(!again.rebased && again.snapshot.is_none());

            // A child forked after the rebase inherits the new base.
            let child = space
                .fork(&ForkRequest {
                    from: Some(Ref::Attempt(a.id)),
                    actor: "agent".into(),
                    backend: BackendChoice::Copy,
                    ..Default::default()
                })
                .unwrap()
                .attempt;
            let ch = space.head_snapshot(child.id).unwrap();
            assert_eq!(space.git_base_commit(&ch).unwrap(), Some(c1));
        }
    }

    #[test]
    fn attempts_forked_after_the_checkout_moved_rebase_from_its_new_head() {
        // #96: the checkout moves past the commit recorded at init (a pull,
        // or a space restored into a newer checkout). An attempt forked
        // now must rebase from that new HEAD, not from the init commit.
        let (tmp, space, _) = repo();
        let w = tmp.path();
        fs::write(w.join("src/lib.rs"), "pub fn f() -> u32 { 10 }\n").unwrap();
        git(w, &["commit", "-q", "-am", "pulled"]);
        let a = fork(&space, BackendChoice::Copy).unwrap();
        fs::write(a.view_path.join("notes.txt"), "mine\n").unwrap();
        fs::write(w.join("src/lib.rs"), "pub fn f() -> u32 { 11 }\n").unwrap();
        let c2 = advance_main(w, "# demo\n\nline one\n");

        let r = rebase(&space, a.id, &onto("main")).unwrap();
        assert!(r.conflicts.is_empty() && r.resolved.is_empty(), "{r:?}");
        assert_eq!(r.onto, c2);
        let lib = fs::read_to_string(a.view_path.join("src/lib.rs")).unwrap();
        assert_eq!(lib, "pub fn f() -> u32 { 11 }\n");
    }

    #[test]
    fn root_snapshots_without_a_recorded_base_match_the_checkout_head() {
        // Spaces from before #96 have no base row for later root snapshots.
        let (tmp, space, c0) = repo();
        let w = tmp.path();
        fs::write(w.join("src/lib.rs"), "pub fn f() -> u32 { 10 }\n").unwrap();
        git(w, &["commit", "-q", "-am", "pulled"]);
        let c1 = git(w, &["rev-parse", "HEAD"]);
        let a = fork(&space, BackendChoice::Copy).unwrap();
        space
            .db
            .execute(
                "DELETE FROM git_bases WHERE attempt_id = ?1",
                [ROOT_ATTEMPT],
            )
            .unwrap();
        let head = space.head_snapshot(a.id).unwrap();
        assert_eq!(space.git_base_commit(&head).unwrap(), Some(c1));
        let first = space.snapshots(ROOT_ATTEMPT).unwrap()[0].clone();
        assert_eq!(space.git_base_commit(&first).unwrap(), Some(c0));
    }

    #[test]
    fn conflicts_are_recorded_previewed_and_resolved_per_hunk() {
        let (tmp, space, _) = repo();
        let w = tmp.path();
        let a = fork(&space, BackendChoice::Copy).unwrap();
        fs::write(a.view_path.join("README.md"), "# demo\n\nline ONE\n").unwrap();
        space.snapshot(a.id, "manual", None).unwrap();
        let c1 = advance_main(w, "# demo\n\nline uno\n");

        // Preview: nothing changes, the conflict is named.
        let p = rebase(
            &space,
            a.id,
            &RebaseOptions {
                preview: true,
                ..onto("main")
            },
        )
        .unwrap();
        assert!(p.preview && !p.rebased && p.snapshot.is_none());
        assert_eq!(p.conflicts, ["README.md"]);
        assert_eq!(p.conflict_files[0].remaining, 1);
        assert!(p.changes.iter().any(|c| c.path == "docs/new.md"));
        assert!(space.conflicts(a.id, true).unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(a.view_path.join("README.md")).unwrap(),
            "# demo\n\nline ONE\n"
        );

        let r = rebase(
            &space,
            a.id,
            &RebaseOptions {
                keep_conflicts: true,
                ..onto("main")
            },
        )
        .unwrap();
        assert_eq!(r.conflicts, ["README.md"]);
        let cs = space.conflicts(a.id, false).unwrap();
        assert_eq!(cs.len(), 1);
        let c = &cs[0];
        assert_eq!(
            (c.operation.as_str(), c.ours_label.as_str()),
            ("rebase", "a2")
        );
        assert_eq!(
            space
                .objects()
                .read_blob(&Oid::from_hex(c.ours.as_ref().unwrap()).unwrap())
                .unwrap(),
            b"# demo\n\nline ONE\n"
        );
        assert_eq!(space.attempt_status(a.id).unwrap().conflicts, ["README.md"]);

        // Markers never ship.
        let e = export(
            &space,
            a.id,
            &ExportOptions {
                branch: "x".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(e.code(), "unresolved_conflicts");

        let f = space.conflict_file(a.id, "README.md").unwrap();
        assert_eq!(f.hunks.len(), 1);
        assert_eq!(f.hunks[0].ours, "line ONE\n");
        assert_eq!(f.hunks[0].base.as_deref(), Some("line one\n"));
        assert_eq!(f.hunks[0].theirs, "line uno\n");
        let choice = crate::conflicts::HunkChoice {
            index: 1,
            take: crate::conflicts::Take::Custom,
            text: Some("line ONE uno\n".into()),
        };
        let res = space
            .resolve_conflict(a.id, "README.md", &[choice], None, "human")
            .unwrap();
        assert_eq!((res.resolved_hunks, res.remaining_hunks), (1, 0));
        assert!(res.open.is_empty());
        assert_eq!(res.snapshot.as_ref().unwrap().trigger, "resolve");
        assert_eq!(
            fs::read_to_string(a.view_path.join("README.md")).unwrap(),
            "# demo\n\nline ONE uno\n"
        );
        let all = space.conflicts(a.id, true).unwrap();
        assert_eq!(all[0].resolution.as_deref(), Some("custom"));
        let e = export(
            &space,
            a.id,
            &ExportOptions {
                branch: "x".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(e.parent.as_deref(), Some(c1.as_str()));
    }

    #[test]
    fn both_sides_adding_at_one_point_auto_resolves() {
        let (tmp, space, _) = repo();
        let w = tmp.path();
        let a = fork(&space, BackendChoice::Copy).unwrap();
        let view = &a.view_path;
        fs::write(view.join("README.md"), "# demo\n\nline one\nfrom a2\n").unwrap();
        advance_main(w, "# demo\n\nline one\nfrom main\n");

        // Plain git conflicts, and --no-auto-resolve reports it as before.
        let err = rebase(
            &space,
            a.id,
            &RebaseOptions {
                no_auto_resolve: true,
                ..onto("main")
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), "conflict");

        let r = rebase(&space, a.id, &onto("main")).unwrap();
        assert!(r.conflicts.is_empty());
        assert_eq!(r.resolved.len(), 1);
        assert_eq!(r.resolved[0].path, "README.md");
        assert_eq!(r.resolved[0].strategy, Strategy::Auto);
        assert_eq!(
            fs::read_to_string(view.join("README.md")).unwrap(),
            "# demo\n\nline one\nfrom a2\nfrom main\n"
        );
        let meta = r.snapshot.unwrap().meta.unwrap();
        assert_eq!(meta["auto_resolved"][0]["path"], "README.md");

        // A rule turns it off per path.
        let (tmp, space, _) = repo();
        let w = tmp.path();
        space
            .set_config(crate::config::MERGE_STRATEGY, &["*.md=none".into()])
            .unwrap();
        let a = fork(&space, BackendChoice::Copy).unwrap();
        fs::write(
            a.view_path.join("README.md"),
            "# demo\n\nline one\nfrom a2\n",
        )
        .unwrap();
        advance_main(w, "# demo\n\nline one\nfrom main\n");
        let r = rebase(
            &space,
            a.id,
            &RebaseOptions {
                keep_conflicts: true,
                ..onto("main")
            },
        )
        .unwrap();
        assert_eq!(r.conflicts, ["README.md"]);
        // Markers in diff3 style, with the base section.
        let readme = fs::read_to_string(a.view_path.join("README.md")).unwrap();
        assert!(
            readme
                .contains("<<<<<<< a2\nfrom a2\n||||||| base\n=======\nfrom main\n>>>>>>> main\n"),
            "{readme}"
        );
    }

    #[test]
    fn conflict_changes_nothing_and_keep_conflicts_leaves_markers() {
        for backend in BACKENDS {
            let (tmp, space, _) = repo();
            let w = tmp.path();
            let Some(a) = fork(&space, backend) else {
                continue;
            };
            let view = &a.view_path;
            fs::write(view.join("README.md"), "# demo\n\nline ONE\n").unwrap();
            // Not yet snapshotted: rebase must pick it up, or leave it alone.
            space.set_status(a.id, Status::Passed, None).unwrap();
            fs::write(view.join("src/lib.rs"), "pub fn f() -> u32 { 3 }\n").unwrap();
            let c1 = advance_main(w, "# demo\n\nline uno\n");
            let snaps = space.snapshots(a.id).unwrap().len();

            match rebase(&space, a.id, &onto("main")) {
                Err(Error::Conflict { paths, onto, .. }) => {
                    assert_eq!(paths, ["README.md"]);
                    assert_eq!(onto, c1);
                }
                other => panic!("expected a conflict, got {other:?}"),
            }
            assert_eq!(space.snapshots(a.id).unwrap().len(), snaps);
            assert_eq!(
                fs::read_to_string(view.join("README.md")).unwrap(),
                "# demo\n\nline ONE\n"
            );
            assert!(view.join("old.txt").exists());
            assert_eq!(space.attempt(a.id).unwrap().status, Status::Passed);

            let r = rebase(
                &space,
                a.id,
                &RebaseOptions {
                    keep_conflicts: true,
                    ..onto("main")
                },
            )
            .unwrap();
            assert_eq!(r.conflicts, ["README.md"]);
            assert_eq!(r.status, Status::Exploring);
            let readme = fs::read_to_string(view.join("README.md")).unwrap();
            assert!(readme.contains("<<<<<<<") && readme.contains(">>>>>>>"));
            // The unsnapshotted edit was recorded before the rebase and kept.
            let triggers: Vec<_> = space
                .snapshots(a.id)
                .unwrap()
                .into_iter()
                .map(|s| s.trigger)
                .collect();
            assert_eq!(triggers[triggers.len() - 2..], ["pre-rebase", "rebase"]);
            assert_eq!(
                fs::read_to_string(view.join("src/lib.rs")).unwrap(),
                "pub fn f() -> u32 { 3 }\n"
            );

            // Resolve, snapshot, export: parented on c1 with only our files.
            fs::write(view.join("README.md"), "# demo\n\nline ONE uno\n").unwrap();
            space.snapshot(a.id, "manual", None).unwrap();
            let e = export(
                &space,
                a.id,
                &ExportOptions {
                    branch: "agentvc/resolved".into(),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(e.parent.as_deref(), Some(&*c1));
            assert_eq!(
                git(w, &["diff", "--name-only", &c1, &e.commit]),
                "README.md\nsrc/lib.rs"
            );
        }
    }

    #[test]
    fn promoted_attempts_rebase_but_root_and_abandoned_do_not() {
        let (tmp, space, _) = repo();
        let w = tmp.path();
        let a = fork(&space, BackendChoice::Copy).unwrap();
        let b = fork(&space, BackendChoice::Copy).unwrap();
        fs::write(a.view_path.join("src/lib.rs"), "pub fn f() -> u32 { 2 }\n").unwrap();
        space.set_status(a.id, Status::Promoted, None).unwrap();
        space
            .set_status(b.id, Status::Abandoned, Some("lost"))
            .unwrap();
        let c1 = advance_main(w, "# demo\n\nmain\n");

        let r = rebase(&space, a.id, &onto("main")).unwrap();
        assert!(r.rebased);
        assert_eq!(r.status, Status::Promoted);
        let head = space.head_snapshot(a.id).unwrap();
        assert_eq!(space.git_base_commit(&head).unwrap(), Some(c1));

        assert!(matches!(
            rebase(&space, b.id, &onto("main")),
            Err(Error::AttemptClosed { .. })
        ));
        assert!(matches!(
            rebase(&space, ROOT_ATTEMPT, &onto("main")),
            Err(Error::RootAttempt)
        ));
        assert!(matches!(
            rebase(&space, a.id, &onto("no-such-branch")),
            Err(Error::NotFound { .. })
        ));
    }

    #[test]
    fn default_target_is_the_fetched_remote_default_branch() {
        let (tmp, space, _) = repo();
        let w = tmp.path();
        // A bare "remote" that already has a newer main.
        let remote = tmp.path().join(".agentvc").join("remote.git");
        git(w, &["clone", "-q", "--bare", ".", remote.to_str().unwrap()]);
        git(w, &["remote", "add", "origin", remote.to_str().unwrap()]);
        let a = fork(&space, BackendChoice::Copy).unwrap();
        let c1 = advance_main(w, "# demo\n\nremote\n");
        git(w, &["push", "-q", "origin", "main"]);
        git(w, &["reset", "-q", "--hard", "HEAD~1"]);

        let r = rebase(&space, a.id, &RebaseOptions::default()).unwrap();
        assert_eq!(r.onto, c1);
        assert_eq!(r.onto_ref, "origin/main");
    }
}
