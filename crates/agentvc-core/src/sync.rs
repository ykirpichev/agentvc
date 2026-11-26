//! Keeping a space on a git remote, so its history outlives the machine.
//!
//! Cloud sessions run in throwaway containers: without this, the attempt
//! tree, the losing attempts and their reasons die with `.agentvc/`.
//! `agentvc sync` writes the whole space as one commit on
//! `refs/agentvc/spaces/<space id>` and pushes it:
//!
//! ```text
//! space.json     tasks, attempts, snapshots, runs, exports, git bases,
//!                operations and metadata, one JSON object per row
//! snapshots/sN   the tree of snapshot sN, as a git tree entry, so git
//!                pushes the snapshot's files with it (deduplicated)
//! logs/rN.log    the last 256 KiB of each run's output
//! ```
//!
//! Every space has its own random id, so spaces from different sessions
//! never share a ref. Each sync is parented on the previous one, so the ref
//! only fast-forwards and older states stay reachable. A remote ref that moved
//! since this space last synced (another machine continued the space) is a
//! `sync_conflict` unless forced.
//!
//! `agentvc init --from-remote` fetches such a ref and rebuilds the space with
//! the same ids: `log`, `show`, `diff` and the review UI see every attempt,
//! losers included. Open attempts get their view written out from their
//! latest snapshot; closed ones keep only history.

use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};

use crate::error::{Error, IoContext, Result};
use crate::export::HostRepo;
use crate::objects::{parse_tree, Mode, Oid, TreeEntry};
use crate::space::{Space, ROOT_ATTEMPT, SPACE_DIR};

/// Space metadata key holding the space's id on remotes.
pub const SPACE_ID_KEY: &str = "space_id";
/// Space metadata key holding the commit this space last synced (or was
/// cloned from).
pub const SYNC_COMMIT_KEY: &str = "sync_commit";
/// Space metadata key naming the remote to sync to after commands that
/// change history (`agentvc sync --auto`).
pub const AUTO_SYNC_KEY: &str = "sync_auto";
/// Where synced spaces live on a remote.
pub const REF_PREFIX: &str = "refs/agentvc/spaces/";

const DUMP_FORMAT: u32 = 1;
/// Tables that make up a space's history, in an order that keeps foreign
/// keys happy. Layers and overlays are machine-local and stay behind.
pub(crate) const TABLES: &[&str] = &[
    "tasks",
    "attempts",
    "snapshots",
    "tags",
    "runs",
    "exports",
    "git_bases",
    "story_changes",
    "operations",
];
/// Metadata keys that describe this machine's sync state, not the space.
/// `user.name` is the person on this machine, so it stays here too.
const LOCAL_META: &[&str] = &[SYNC_COMMIT_KEY, AUTO_SYNC_KEY, "config.user.name"];
pub(crate) fn is_local_meta(key: &str) -> bool {
    LOCAL_META.contains(&key)
}

/// Give rows created since the last sync (and rows from before uids
/// existed) their uid; see [`crate::syncmerge`].
pub(crate) fn ensure_uids(space: &Space) -> Result<()> {
    let base = match (
        space.meta(SYNC_COMMIT_KEY)?,
        HostRepo::open(space.workdir()),
    ) {
        (Some(c), Ok(repo)) => crate::syncmerge::read_doc(&repo, &c).ok(),
        _ => None,
    };
    crate::syncmerge::assign_uids(space, base.as_ref())
}

pub(crate) const LOG_TAIL: u64 = 256 * 1024;

#[derive(Clone, Debug, Default)]
pub struct SyncOptions {
    /// Git remote. Defaults to `origin`.
    pub remote: Option<String>,
    /// Push even if the remote ref moved since this space last synced,
    /// replacing what the other machine added.
    pub force: bool,
    /// Settle values both machines changed (an attempt's status) this way
    /// instead of failing with `sync_conflict`.
    pub prefer: Option<crate::syncmerge::Prefer>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SyncResult {
    pub space_id: String,
    pub remote: String,
    #[serde(rename = "ref")]
    pub refname: String,
    pub commit: String,
    /// The remote's previous commit for this space, if any.
    pub parent: Option<String>,
    /// True when nothing changed since the last sync, so nothing was pushed.
    pub unchanged: bool,
    /// What another machine had synced since this one last did, now merged
    /// in here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged: Option<crate::syncmerge::MergeStats>,
    pub tasks: usize,
    pub attempts: usize,
    pub snapshots: usize,
    pub objects_copied: usize,
}

/// A space found on a remote.
#[derive(Clone, Debug, Serialize)]
pub struct RemoteSpace {
    pub space_id: String,
    #[serde(rename = "ref")]
    pub refname: String,
    pub commit: String,
    /// Unix time of the last sync.
    pub synced_at: i64,
    /// The branch checked out where the space was synced from.
    pub branch: Option<String>,
    pub tasks: Vec<String>,
    pub attempts: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct CloneResult {
    pub space_id: String,
    pub remote: String,
    pub commit: String,
    pub tasks: usize,
    pub attempts: usize,
    pub snapshots: usize,
    /// Open attempts whose views were written out from their latest snapshot.
    pub views: Vec<i64>,
    pub objects_imported: usize,
}

pub(crate) fn remote_name(r: &Option<String>) -> String {
    r.clone().unwrap_or_else(|| "origin".into())
}

/// The space's id, created on first use.
pub fn space_id(space: &Space) -> Result<String> {
    if let Some(id) = space.meta(SPACE_ID_KEY)? {
        return Ok(id);
    }
    let mut h = Sha1::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    h.update(nanos.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(space.workdir().as_os_str().as_encoded_bytes());
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        let mut buf = [0u8; 16];
        if f.read_exact(&mut buf).is_ok() {
            h.update(buf);
        }
    }
    let id = hex::encode(&h.finalize()[..8]);
    space.set_meta(SPACE_ID_KEY, &id)?;
    Ok(id)
}

fn sql_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(hex::encode(b)),
    }
}

pub(crate) fn json_to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::Integer(i),
            None => SqlValue::Real(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

fn dump_table(db: &Connection, table: &str) -> Result<Vec<Value>> {
    let mut stmt = db.prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let rows = stmt.query_map([], |r| {
        let mut m = Map::new();
        for (i, n) in names.iter().enumerate() {
            m.insert(n.clone(), sql_to_json(r.get_ref(i)?));
        }
        Ok(Value::Object(m))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub(crate) fn columns(db: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut stmt = db.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// The space's history as one JSON document. Deterministic for the same
/// state, so an unchanged space syncs to the same tree.
pub(crate) fn dump(space: &Space, id: &str) -> Result<Value> {
    let mut tables = Map::new();
    for t in TABLES {
        tables.insert(t.to_string(), Value::Array(dump_table(&space.db, t)?));
    }
    let mut meta = Map::new();
    for row in dump_table_meta(&space.db)? {
        if !LOCAL_META.contains(&row.0.as_str()) {
            meta.insert(row.0, Value::String(row.1));
        }
    }
    let branch = HostRepo::open(space.workdir())
        .ok()
        .and_then(|r| r.git(&["symbolic-ref", "--short", "-q", "HEAD"], None).ok())
        .filter(|b| !b.is_empty());
    Ok(json!({
        "format": DUMP_FORMAT,
        "space_id": id,
        "branch": branch,
        "workdir": space.workdir(),
        "meta": meta,
        "tables": tables,
    }))
}

fn dump_table_meta(db: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = db.prepare("SELECT key, value FROM meta ORDER BY key")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub(crate) fn tail(path: &Path, max: u64) -> Option<Vec<u8>> {
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len > max {
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(len - max)).ok()?;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Build the sync tree in the space's object store and return its id.
fn build_tree(space: &Space, doc: &Value) -> Result<Oid> {
    let store = space.objects();
    let text = serde_json::to_string_pretty(doc).expect("json serializes") + "\n";
    let mut root = vec![TreeEntry {
        name: "space.json".into(),
        mode: Mode::File,
        oid: store.write_blob(text.as_bytes())?,
    }];
    let rows = |t: &str| doc["tables"][t].as_array().cloned().unwrap_or_default();

    let mut snaps = Vec::new();
    for s in rows("snapshots") {
        let (Some(id), Some(tree)) = (s["id"].as_i64(), s["tree"].as_str()) else {
            continue;
        };
        // A tree `agentvc gc` pruned stays reachable from the earlier sync
        // commits this one is parented on.
        if !s["pruned_at"].is_null() {
            continue;
        }
        snaps.push(TreeEntry {
            name: format!("s{id}"),
            mode: Mode::Tree,
            oid: Oid::from_hex(tree)?,
        });
    }
    if !snaps.is_empty() {
        root.push(TreeEntry {
            name: "snapshots".into(),
            mode: Mode::Tree,
            oid: store.write_tree(&mut snaps)?,
        });
    }

    // Story trees (#21) are not snapshots, so they ride along on their own.
    let mut story = Vec::new();
    for c in rows("story_changes") {
        let (Some(a), Some(p), Some(tree), Some(base)) = (
            c["attempt_id"].as_i64(),
            c["position"].as_i64(),
            c["tree"].as_str(),
            c["base_tree"].as_str(),
        ) else {
            continue;
        };
        story.push(TreeEntry {
            name: format!("a{a}.{p}"),
            mode: Mode::Tree,
            oid: Oid::from_hex(tree)?,
        });
        if p == 1 {
            story.push(TreeEntry {
                name: format!("a{a}.base"),
                mode: Mode::Tree,
                oid: Oid::from_hex(base)?,
            });
        }
    }
    if !story.is_empty() {
        root.push(TreeEntry {
            name: "story".into(),
            mode: Mode::Tree,
            oid: store.write_tree(&mut story)?,
        });
    }

    let mut logs = Vec::new();
    for r in rows("runs") {
        let (Some(id), Some(p)) = (r["id"].as_i64(), r["log_path"].as_str()) else {
            continue;
        };
        if let Some(data) = tail(Path::new(p), LOG_TAIL) {
            logs.push(TreeEntry {
                name: format!("r{id}.log"),
                mode: Mode::File,
                oid: store.write_blob(&data)?,
            });
        }
    }
    if !logs.is_empty() {
        root.push(TreeEntry {
            name: "logs".into(),
            mode: Mode::Tree,
            oid: store.write_tree(&mut logs)?,
        });
    }
    store.write_tree(&mut root)
}

/// Commit identity for agentvc's own commits when git doesn't know the user.
fn identity_env(repo: &HostRepo) -> Vec<(&'static str, &'static str)> {
    if repo.git(&["config", "user.email"], None).is_ok() {
        return vec![];
    }
    vec![
        ("GIT_AUTHOR_NAME", "agentvc"),
        ("GIT_AUTHOR_EMAIL", "agentvc@localhost"),
        ("GIT_COMMITTER_NAME", "agentvc"),
        ("GIT_COMMITTER_EMAIL", "agentvc@localhost"),
    ]
}

/// The commit `refname` points to on `remote`, if it exists there.
pub(crate) fn remote_tip(repo: &HostRepo, remote: &str, refname: &str) -> Result<Option<String>> {
    let out = repo.git(&["ls-remote", remote, refname], None)?;
    Ok(out
        .lines()
        .find_map(|l| l.split_whitespace().next())
        .map(str::to_string))
}

/// Push the space to `opts.remote` as `refs/agentvc/spaces/<space id>`.
pub fn sync(space: &Space, opts: &SyncOptions) -> Result<SyncResult> {
    let repo = HostRepo::open(space.workdir())?;
    let remote = remote_name(&opts.remote);
    let id = space_id(space)?;
    let refname = format!("{REF_PREFIX}{id}");

    let parent = remote_tip(&repo, &remote, &refname)?;
    let last = space.meta(SYNC_COMMIT_KEY)?;
    ensure_uids(space)?;
    let mut merged = None;
    // The previous sync, as a second parent when we merged past it.
    let mut also = None;
    if let Some(tip) = &parent {
        repo.git(&["fetch", "--quiet", "--no-tags", &remote, &refname], None)?;
        // The remote moved past what we last synced: another machine
        // continued this space. Merge its additions in rather than hide them.
        let ours = last.as_ref().is_some_and(|l| {
            l == tip
                || repo
                    .git(&["merge-base", "--is-ancestor", tip, l], None)
                    .is_ok()
        });
        if !ours && !opts.force {
            let base = match &last {
                Some(l) => Some(crate::syncmerge::read_doc(&repo, l)?),
                None => None,
            };
            let theirs = crate::syncmerge::read_doc(&repo, tip)?;
            if theirs["format"].as_u64() != Some(DUMP_FORMAT as u64) {
                return Err(Error::BadObject(format!(
                    "{tip}:space.json (unknown format {})",
                    theirs["format"]
                )));
            }
            merged = Some(crate::syncmerge::merge(
                space,
                &repo,
                tip,
                base.as_ref(),
                &theirs,
                opts.prefer,
            )?);
            also = last.clone();
        }
    }

    let doc = dump(space, &id)?;
    let count = |t: &str| doc["tables"][t].as_array().map_or(0, Vec::len);
    let (tasks, attempts, snapshots) = (count("tasks"), count("attempts"), count("snapshots"));
    let tree = build_tree(space, &doc)?;
    let objects_copied = crate::export::copy_tree_objects(space, &tree, &repo.objects_dir()?)?;

    let tree_hex = tree.to_hex();
    // The merge brought nothing new and nothing changed here since the last
    // sync: the remote already has everything. Adopt its commit rather than
    // push one that differs only in this machine's ids and paths.
    if let (Some(m), Some(l), Some(tip)) = (&merged, &last, &parent) {
        let same_here = repo
            .git(&["rev-parse", &format!("{l}^{{tree}}")], None)
            .ok()
            == Some(tree_hex.clone());
        if m.rows == 0 && m.updated == 0 && same_here {
            repo.git(&["update-ref", &refname, tip], None)?;
            space.set_meta(SYNC_COMMIT_KEY, tip)?;
            return Ok(SyncResult {
                space_id: id,
                remote,
                refname,
                commit: tip.clone(),
                parent: parent.clone(),
                unchanged: true,
                merged,
                tasks,
                attempts,
                snapshots,
                objects_copied,
            });
        }
    }
    if let Some(tip) = &parent {
        if repo
            .git(&["rev-parse", &format!("{tip}^{{tree}}")], None)
            .ok()
            == Some(tree_hex.clone())
        {
            repo.git(&["update-ref", &refname, tip], None)?;
            space.set_meta(SYNC_COMMIT_KEY, tip)?;
            return Ok(SyncResult {
                space_id: id,
                remote,
                refname,
                commit: tip.clone(),
                parent: parent.clone(),
                unchanged: true,
                merged,
                tasks,
                attempts,
                snapshots,
                objects_copied,
            });
        }
    }

    let message = format!(
        "agentvc sync: {tasks} tasks, {attempts} attempts, {snapshots} snapshots\n\n\
         Agentvc-Space: {id}\n"
    );
    let mut args = vec!["commit-tree", tree_hex.as_str()];
    if let Some(p) = &parent {
        args.extend(["-p", p.as_str()]);
    }
    if let Some(l) = &also {
        args.extend(["-p", l.as_str()]);
    }
    args.extend(["-F", "-"]);
    let commit = repo.git_env(&args, Some(&message), &identity_env(&repo))?;
    let spec = format!("{}{commit}:{refname}", if opts.force { "+" } else { "" });
    repo.git(&["push", "--quiet", &remote, &spec], None)?;
    repo.git(&["update-ref", &refname, &commit], None)?;
    // Not recorded as an operation: that would change the next dump, so an
    // unchanged space could never sync as unchanged.
    space.set_meta(SYNC_COMMIT_KEY, &commit)?;
    Ok(SyncResult {
        space_id: id,
        remote,
        refname,
        commit,
        parent,
        unchanged: false,
        merged,
        tasks,
        attempts,
        snapshots,
        objects_copied,
    })
}

/// Local refs that mirror a remote's synced spaces.
fn mirror_prefix(remote: &str) -> String {
    format!("refs/agentvc/remotes/{remote}/")
}

/// Fetch and list the spaces synced to `remote`, newest first.
pub fn remote_spaces(workdir: &Path, remote: Option<&str>) -> Result<Vec<RemoteSpace>> {
    let repo = HostRepo::open(workdir)?;
    let remote = remote.unwrap_or("origin");
    let mirror = mirror_prefix(remote);
    let spec = format!("+{REF_PREFIX}*:{mirror}*");
    repo.git(
        &["fetch", "--quiet", "--no-tags", "--prune", remote, &spec],
        None,
    )?;
    let listing = repo.git(
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(committerdate:unix)",
            &mirror,
        ],
        None,
    )?;
    let mut out = Vec::new();
    for line in listing.lines() {
        let mut parts = line.split_whitespace();
        let (Some(local), Some(commit), Some(at)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let space_id = local.trim_start_matches(&mirror).to_string();
        let doc: Value = repo
            .git(&["cat-file", "blob", &format!("{commit}:space.json")], None)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let tasks = doc["tables"]["tasks"]
            .as_array()
            .map(|ts| {
                ts.iter()
                    .map(|t| {
                        format!(
                            "t{} {}",
                            t["id"].as_i64().unwrap_or(0),
                            t["goal"].as_str().unwrap_or("")
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(RemoteSpace {
            refname: format!("{REF_PREFIX}{space_id}"),
            space_id,
            commit: commit.to_string(),
            synced_at: at.parse().unwrap_or(0),
            branch: doc["branch"].as_str().map(str::to_string),
            tasks,
            attempts: doc["tables"]["attempts"].as_array().map_or(0, Vec::len),
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.synced_at));
    Ok(out)
}

/// `git cat-file --batch`, for copying many objects out of the host repo.
pub(crate) struct CatFile {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl CatFile {
    pub(crate) fn start(workdir: &Path) -> Result<CatFile> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(workdir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .at("git cat-file")?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = BufReader::new(child.stdout.take().expect("piped"));
        Ok(CatFile {
            child,
            stdin,
            stdout,
        })
    }

    pub(crate) fn read(&mut self, oid: &Oid) -> Result<(String, Vec<u8>)> {
        writeln!(self.stdin, "{oid}").at("git cat-file")?;
        self.stdin.flush().at("git cat-file")?;
        let mut header = String::new();
        self.stdout.read_line(&mut header).at("git cat-file")?;
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() != 3 {
            return Err(Error::NotFound {
                kind: "object",
                id: oid.to_hex(),
            });
        }
        let size: usize = parts[2]
            .parse()
            .map_err(|_| Error::BadObject(oid.to_hex()))?;
        let mut data = vec![0; size + 1];
        self.stdout.read_exact(&mut data).at("git cat-file")?;
        data.pop();
        Ok((parts[1].to_string(), data))
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Copy tree `oid` and everything under it from the host repo into the
/// space's object store. Children are written before their tree, so a tree
/// that is present is always complete.
pub(crate) fn import_tree(space: &Space, git: &mut CatFile, oid: &Oid) -> Result<usize> {
    let store = space.objects();
    if store.contains(oid) {
        return Ok(0);
    }
    let (kind, data) = git.read(oid)?;
    if kind != "tree" {
        return Err(Error::BadObject(oid.to_hex()));
    }
    let mut n = 0;
    for e in parse_tree(oid, &data)? {
        if e.mode == Mode::Tree {
            n += import_tree(space, git, &e.oid)?;
        } else if !store.contains(&e.oid) {
            let (k, body) = git.read(&e.oid)?;
            store.write_raw(&k, &body)?;
            n += 1;
        }
    }
    store.write_raw("tree", &data)?;
    Ok(n + 1)
}

/// Which synced space [`clone_from_remote`] rebuilds.
#[derive(Clone, Debug, Default)]
pub struct CloneOptions {
    /// Git remote. Defaults to `origin`.
    pub remote: Option<String>,
    /// Space id or a prefix of it.
    pub space: Option<String>,
    /// Only take a space synced from the branch checked out here (newest
    /// first), never another branch's only space.
    pub this_branch: bool,
}

/// Pick the remote space to clone: by id (or id prefix), else the newest
/// synced from the branch checked out here, else the only one.
fn choose(
    spaces: &[RemoteSpace],
    want: Option<&str>,
    branch: Option<&str>,
    this_branch: bool,
) -> Result<usize> {
    if let Some(w) = want {
        let hits: Vec<usize> = (0..spaces.len())
            .filter(|&i| spaces[i].space_id.starts_with(w))
            .collect();
        return match hits.as_slice() {
            [i] => Ok(*i),
            [] => Err(Error::NotFound {
                kind: "remote space",
                id: w.to_string(),
            }),
            _ => Err(Error::AmbiguousSpace {
                ids: hits.iter().map(|&i| spaces[i].space_id.clone()).collect(),
            }),
        };
    }
    if let Some(b) = branch {
        // Newest first, so the first match is the latest sync from this branch.
        if let Some(i) = spaces.iter().position(|s| s.branch.as_deref() == Some(b)) {
            return Ok(i);
        }
    }
    if this_branch || spaces.is_empty() {
        return Err(Error::NotFound {
            kind: "remote space",
            id: match (this_branch, branch) {
                (true, Some(b)) => format!("synced from branch {b}"),
                _ => "any".into(),
            },
        });
    }
    if spaces.len() == 1 {
        return Ok(0);
    }
    Err(Error::AmbiguousSpace {
        ids: spaces.iter().map(|s| s.space_id.clone()).collect(),
    })
}

/// Rebuild a space in `workdir` from one synced to a remote, keeping its ids.
/// See [`CloneOptions`] and [`choose`] for which space.
pub fn clone_from_remote(workdir: &Path, opts: &CloneOptions) -> Result<(Space, CloneResult)> {
    let workdir = fs::canonicalize(workdir).at(workdir)?;
    let dir = workdir.join(SPACE_DIR);
    if dir.join("meta.db").exists() {
        return Err(Error::SpaceExists(workdir));
    }
    let remote = remote_name(&opts.remote);
    let spaces = remote_spaces(&workdir, Some(&remote))?;
    let repo = HostRepo::open(&workdir)?;
    let branch = repo
        .git(&["symbolic-ref", "--short", "-q", "HEAD"], None)
        .ok();
    let picked = spaces[choose(
        &spaces,
        opts.space.as_deref(),
        branch.as_deref(),
        opts.this_branch,
    )?]
    .clone();
    let doc: Value = serde_json::from_str(&repo.git(
        &["cat-file", "blob", &format!("{}:space.json", picked.commit)],
        None,
    )?)
    .map_err(|_| Error::BadObject(format!("{}:space.json", picked.commit)))?;
    if doc["format"].as_u64() != Some(DUMP_FORMAT as u64) {
        return Err(Error::BadObject(format!(
            "{}:space.json (unknown format {})",
            picked.commit, doc["format"]
        )));
    }

    let restored = restore(&workdir, &repo, &picked, &doc, remote);
    if restored.is_err() {
        // A half-built space is worse than none: let the user retry.
        let _ = fs::remove_dir_all(&dir);
    }
    restored
}

/// Write the space described by `doc` (from `picked`) into `workdir`.
fn restore(
    workdir: &Path,
    repo: &HostRepo,
    picked: &RemoteSpace,
    doc: &Value,
    remote: String,
) -> Result<(Space, CloneResult)> {
    let workdir = workdir.to_path_buf();
    let dir = workdir.join(SPACE_DIR);
    let space = empty_space(&workdir)?;

    // Objects first: a space whose rows point at missing trees is worse than
    // no space.
    let mut git = CatFile::start(&workdir)?;
    let mut objects_imported = 0;
    let mut seen = HashSet::new();
    let rows = |t: &str| doc["tables"][t].as_array().cloned().unwrap_or_default();
    for s in rows("snapshots") {
        if !s["pruned_at"].is_null() {
            continue;
        }
        if let Some(tree) = s["tree"].as_str() {
            if seen.insert(tree.to_string()) {
                objects_imported += import_tree(&space, &mut git, &Oid::from_hex(tree)?)?;
            }
        }
    }
    for c in rows("story_changes") {
        for k in ["tree", "base_tree"] {
            if let Some(tree) = c[k].as_str() {
                if seen.insert(tree.to_string()) {
                    objects_imported += import_tree(&space, &mut git, &Oid::from_hex(tree)?)?;
                }
            }
        }
    }

    let runs_dir = dir.join("runs");
    insert_rows(&space, doc)?;
    // Rows synced before uids existed get the uid every machine derives.
    crate::syncmerge::assign_uids(&space, Some(doc))?;
    space.set_meta(SPACE_ID_KEY, &picked.space_id)?;
    space.set_meta(SYNC_COMMIT_KEY, &picked.commit)?;
    repo.git(&["update-ref", &picked.refname, &picked.commit], None)?;

    // Run logs.
    if let Ok(listing) = repo.git(&["ls-tree", &format!("{}:logs", picked.commit)], None) {
        fs::create_dir_all(&runs_dir).at(&runs_dir)?;
        for line in listing.lines() {
            let Some((info, name)) = line.split_once('\t') else {
                continue;
            };
            let Some(oid) = info.split_whitespace().nth(2) else {
                continue;
            };
            let (_, body) = git.read(&Oid::from_hex(oid)?)?;
            fs::write(runs_dir.join(name), body).at(runs_dir.join(name))?;
        }
    }

    let views = open_views(&space)?;

    let count = |t: &str| doc["tables"][t].as_array().map_or(0, Vec::len);
    space.record(
        &space.human(),
        "space.clone",
        json!({ "remote": remote, "ref": picked.refname, "commit": picked.commit }),
    )?;
    let result = CloneResult {
        space_id: picked.space_id.clone(),
        remote,
        commit: picked.commit.clone(),
        tasks: count("tasks"),
        attempts: count("attempts"),
        snapshots: count("snapshots"),
        views,
        objects_imported,
    };
    Ok((space, result))
}

/// A new, empty space in `workdir`, for rebuilding one from elsewhere.
pub(crate) fn empty_space(workdir: &Path) -> Result<Space> {
    let dir = workdir.join(SPACE_DIR);
    for sub in ["objects", "views"] {
        fs::create_dir_all(dir.join(sub)).at(dir.join(sub))?;
    }
    fs::write(dir.join(".gitignore"), "*\n").at(dir.join(".gitignore"))?;
    Space::open_at(workdir.to_path_buf())
}

/// Insert the rows and metadata of a dumped space (see [`dump`]), rewriting
/// paths from the other machine to this one.
pub(crate) fn insert_rows(space: &Space, doc: &Value) -> Result<()> {
    let workdir = space.workdir().to_path_buf();
    let dir = workdir.join(SPACE_DIR);
    let rows = |t: &str| doc["tables"][t].as_array().cloned().unwrap_or_default();
    let views_dir = dir.join("views");
    let runs_dir = dir.join("runs");
    space.db.pragma_update(None, "foreign_keys", "OFF")?;
    let tx = space.db.unchecked_transaction()?;
    for t in TABLES {
        let cols = columns(&space.db, t)?;
        for row in rows(t) {
            let Some(obj) = row.as_object() else { continue };
            let mut obj = obj.clone();
            let id = obj.get("id").and_then(Value::as_i64).unwrap_or(0);
            // Paths from the other machine are rewritten to this one.
            match *t {
                "attempts" if id == ROOT_ATTEMPT => {
                    obj.insert("view_path".into(), json!(workdir));
                    obj.insert("backend".into(), json!("workdir"));
                }
                "attempts" => {
                    obj.insert("view_path".into(), json!(views_dir.join(format!("a{id}"))));
                    obj.insert("backend".into(), json!("copy"));
                }
                "runs" if !obj["log_path"].is_null() => {
                    obj.insert(
                        "log_path".into(),
                        json!(runs_dir.join(format!("r{id}.log"))),
                    );
                }
                _ => {}
            }
            let (names, values): (Vec<String>, Vec<SqlValue>) = obj
                .iter()
                .filter(|(k, _)| cols.contains(*k))
                .map(|(k, v)| (k.clone(), json_to_sql(v)))
                .unzip();
            let marks: Vec<String> = (1..=names.len()).map(|i| format!("?{i}")).collect();
            tx.execute(
                &format!(
                    "INSERT INTO {t} ({}) VALUES ({})",
                    names.join(", "),
                    marks.join(", ")
                ),
                rusqlite::params_from_iter(values),
            )?;
        }
    }
    if let Some(meta) = doc["meta"].as_object() {
        for (k, v) in meta {
            if let Some(v) = v.as_str() {
                tx.execute(
                    "INSERT INTO meta (key, value) VALUES (?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    rusqlite::params![k, v],
                )?;
            }
        }
    }
    tx.commit()?;
    space.db.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Write views for open attempts whose latest tree is in the store, so
/// work can continue in them. Returns their ids.
pub(crate) fn open_views(space: &Space) -> Result<Vec<i64>> {
    // Open attempts get a view to keep working in.
    let mut views = Vec::new();
    for a in space.attempts(None)? {
        if a.id == ROOT_ATTEMPT || !a.status.is_open() || a.view_path.exists() {
            continue;
        }
        let head = Oid::from_hex(&space.head_snapshot(a.id)?.tree)?;
        if !space.objects().contains(&head) {
            continue;
        }
        crate::snapshot::materialize(space.objects(), &head, &a.view_path)?;
        let _ = space.link_view_git(&a);
        views.push(a.id);
    }

    Ok(views)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::{ForkRequest, Status};
    use crate::testutil::{tempdir, TestDir};

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
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bare remote with one commit on main, and a clone of it at `name`.
    fn remote() -> (TestDir, std::path::PathBuf) {
        let tmp = tempdir();
        let bare = tmp.path().join("remote.git");
        git(
            tmp.path(),
            &["init", "-q", "--bare", "-b", "main", "remote.git"],
        );
        let seed = tmp.path().join("seed");
        git(tmp.path(), &["init", "-q", "-b", "main", "seed"]);
        fs::write(seed.join("lib.rs"), "fn f() {}\n").unwrap();
        fs::write(seed.join(".gitignore"), "target/\n").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "-q", "-m", "base"]);
        git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&seed, &["push", "-q", "origin", "main"]);
        (tmp, bare)
    }

    fn checkout(tmp: &TestDir, bare: &Path, name: &str) -> std::path::PathBuf {
        git(tmp.path(), &["clone", "-q", bare.to_str().unwrap(), name]);
        tmp.path().join(name)
    }

    /// A space with a task, a winner, a loser with a reason and a run log.
    fn busy_space(dir: &Path) -> Space {
        let space = Space::init(dir).unwrap();
        let task = space.create_task("goal", Some("tests pass")).unwrap();
        let fork = |actor: &str| {
            space
                .fork(&ForkRequest {
                    task_id: Some(task.id),
                    actor: actor.into(),
                    backend: crate::view::BackendChoice::Copy,
                    ..Default::default()
                })
                .unwrap()
                .attempt
        };
        let win = fork("agent:a");
        let lose = fork("agent:b");
        fs::write(win.view_path.join("lib.rs"), "fn f() { 1; }\n").unwrap();
        space
            .snapshot(win.id, "manual", Some(json!({ "tests": "pass" })))
            .unwrap();
        // Tags travel with the space (#74).
        space
            .create_tag(
                "demo",
                crate::space::Ref::Attempt(win.id),
                Some("shown"),
                "human",
            )
            .unwrap();
        fs::write(lose.view_path.join("new.rs"), "x\n").unwrap();
        let run = space
            .create_run(
                lose.id,
                &["sh".into(), "-c".into(), "echo hello-log".into()],
                None,
                true,
            )
            .unwrap();
        crate::run::execute(&space, run.id).unwrap();
        space
            .set_status(lose.id, Status::Abandoned, Some("too slow"))
            .unwrap();
        space
    }

    /// History as `log --json` and `show --json` would print it, minus the
    /// machine-local view paths.
    fn history(space: &Space) -> Value {
        let mut attempts = serde_json::to_value(space.attempts(None).unwrap()).unwrap();
        for a in attempts.as_array_mut().unwrap() {
            a.as_object_mut().unwrap().remove("view_path");
            a.as_object_mut().unwrap().remove("backend");
        }
        let snaps: Vec<_> = space
            .attempts(None)
            .unwrap()
            .iter()
            .map(|a| serde_json::to_value(space.snapshots(a.id).unwrap()).unwrap())
            .collect();
        json!({ "tasks": space.tasks().unwrap(), "attempts": attempts, "snapshots": snaps })
    }

    #[test]
    fn sync_then_clone_rebuilds_the_same_tree() {
        let (tmp, bare) = remote();
        let a = checkout(&tmp, &bare, "a");
        let space = busy_space(&a);
        let r = sync(&space, &SyncOptions::default()).unwrap();
        assert!(!r.unchanged);
        assert_eq!((r.tasks, r.attempts), (1, 3));
        assert!(git(&bare, &["for-each-ref", REF_PREFIX]).contains(&r.commit));

        // Nothing changed: same commit, nothing pushed.
        let again = sync(&space, &SyncOptions::default()).unwrap();
        assert!(again.unchanged);
        assert_eq!(again.commit, r.commit);

        let b = checkout(&tmp, &bare, "b");
        let (copy, c) = clone_from_remote(&b, &CloneOptions::default()).unwrap();
        assert_eq!(c.space_id, r.space_id);
        assert_eq!(history(&copy), history(&space));

        // The loser's reason, files and run log came along.
        let lose = copy.attempt(3).unwrap();
        assert_eq!(lose.status, Status::Abandoned);
        assert_eq!(lose.reason.as_deref(), Some("too slow"));
        let changes = copy.diff(
            crate::space::Ref::Snapshot(1),
            crate::space::Ref::Attempt(3),
        );
        assert_eq!(changes.unwrap()[0].path, "new.rs");
        let run = &copy.runs(false).unwrap()[0];
        let log = fs::read_to_string(run.log_path.as_ref().unwrap()).unwrap();
        assert!(log.contains("hello-log"));
        // The open winner got a view to keep working in.
        assert_eq!(c.views, vec![2]);
        let view = copy.attempt(2).unwrap().view_path;
        assert_eq!(
            fs::read_to_string(view.join("lib.rs")).unwrap(),
            "fn f() { 1; }\n"
        );

        // The copy continues the same space: its sync fast-forwards...
        copy.create_task("more", None).unwrap();
        let next = sync(&copy, &SyncOptions::default()).unwrap();
        assert_eq!(next.parent.as_deref(), Some(&*r.commit));
        // ...and the original, now behind, merges it in instead of
        // overwriting it (#77).
        space.create_task("stale", None).unwrap();
        let merged = sync(&space, &SyncOptions::default()).unwrap();
        assert_eq!(merged.merged.as_ref().unwrap().tasks, 1);
        assert_eq!(goals(&space), ["goal", "more", "stale"]);
    }

    /// Attempts as (actor, status), sorted: what both machines must agree on
    /// whatever ids they gave them.
    fn outline(space: &Space) -> Vec<(String, String)> {
        let mut v: Vec<_> = space
            .attempts(None)
            .unwrap()
            .into_iter()
            .map(|a| (a.actor, a.status.as_str().to_string()))
            .collect();
        v.sort();
        v
    }

    fn goals(space: &Space) -> Vec<String> {
        let mut v: Vec<_> = space.tasks().unwrap().into_iter().map(|t| t.goal).collect();
        v.sort();
        v
    }

    fn fork_as(space: &Space, task: i64, actor: &str, file: &str) -> i64 {
        let a = space
            .fork(&ForkRequest {
                task_id: Some(task),
                actor: actor.into(),
                backend: crate::view::BackendChoice::Copy,
                ..Default::default()
            })
            .unwrap()
            .attempt;
        fs::write(a.view_path.join(file), format!("{actor}\n")).unwrap();
        space.snapshot(a.id, "manual", None).unwrap();
        a.id
    }

    /// The id `space` gives the attempt by `actor`.
    fn by_actor(space: &Space, actor: &str) -> i64 {
        space
            .attempts(None)
            .unwrap()
            .into_iter()
            .find(|a| a.actor == actor)
            .unwrap()
            .id
    }

    /// Two machines restore one space; each adds attempts and a task and
    /// abandons a different attempt; they sync in either order. Both end
    /// with the union, no --force.
    #[test]
    fn two_machines_merge_in_either_order() {
        for a_first in [true, false] {
            let (tmp, bare) = remote();
            let a = checkout(&tmp, &bare, "a");
            let sa = busy_space(&a);
            fork_as(&sa, 1, "agent:c", "c.rs");
            fork_as(&sa, 1, "agent:d", "d.rs");
            sync(&sa, &SyncOptions::default()).unwrap();
            let b = checkout(&tmp, &bare, "b");
            let (sb, _) = clone_from_remote(&b, &CloneOptions::default()).unwrap();

            // Same ids on both machines for the new work: they collide.
            let on_a = fork_as(&sa, 1, "agent:from-a", "a.rs");
            sa.set_status(
                by_actor(&sa, "agent:c"),
                Status::Abandoned,
                Some("a says no"),
            )
            .unwrap();
            let on_b = fork_as(&sb, 1, "agent:from-b", "b.rs");
            assert_eq!(on_a, on_b);
            sb.set_status(
                by_actor(&sb, "agent:d"),
                Status::Abandoned,
                Some("b says no"),
            )
            .unwrap();
            sb.create_task("b's task", None).unwrap();

            let (first, second) = if a_first { (&sa, &sb) } else { (&sb, &sa) };
            assert!(sync(first, &SyncOptions::default())
                .unwrap()
                .merged
                .is_none());
            let r = sync(second, &SyncOptions::default()).unwrap();
            let m = r.merged.unwrap();
            assert_eq!(m.attempts, 1, "{m:?}");
            let again = sync(first, &SyncOptions::default()).unwrap();
            assert!(again.merged.is_some());
            // Settled: both are up to date now.
            assert!(sync(second, &SyncOptions::default()).unwrap().unchanged);

            assert_eq!(outline(&sa), outline(&sb));
            assert_eq!(goals(&sa), goals(&sb));
            assert_eq!(goals(&sa), ["b's task", "goal"]);
            // The other machine's operations are history here, not undoable.
            let foreign = sa
                .operations(None)
                .unwrap()
                .into_iter()
                .filter(|o| o.payload.get("merged_from").is_some())
                .collect::<Vec<_>>();
            assert!(!foreign.is_empty() && foreign.iter().all(|o| !o.undoable));
            let o = outline(&sa);
            assert!(o.contains(&("agent:c".into(), "abandoned".into())), "{o:?}");
            assert!(o.contains(&("agent:d".into(), "abandoned".into())), "{o:?}");
            assert_eq!(o.len(), 7, "{o:?}");
            // The other machine's attempt came with its files and a view.
            for (space, actor, file) in
                [(&sa, "agent:from-b", "b.rs"), (&sb, "agent:from-a", "a.rs")]
            {
                let id = by_actor(space, actor);
                assert_ne!(id, on_a, "renumbered past the local attempt");
                let v = space.attempt(id).unwrap().view_path;
                assert_eq!(
                    fs::read_to_string(v.join(file)).unwrap(),
                    format!("{actor}\n")
                );
                let changes = space
                    .diff(
                        crate::space::Ref::Snapshot(1),
                        crate::space::Ref::Attempt(id),
                    )
                    .unwrap();
                assert!(changes.iter().any(|c| c.path == file));
            }
        }
    }

    #[test]
    fn same_attempt_status_conflict_names_both_values() {
        let (tmp, bare) = remote();
        let a = checkout(&tmp, &bare, "a");
        let sa = busy_space(&a);
        let c = fork_as(&sa, 1, "agent:c", "c.rs");
        sync(&sa, &SyncOptions::default()).unwrap();
        let b = checkout(&tmp, &bare, "b");
        let (sb, _) = clone_from_remote(&b, &CloneOptions::default()).unwrap();

        sa.set_status(c, Status::Failed, None).unwrap();
        fork_as(&sa, 1, "agent:more", "m.rs");
        sb.set_status(c, Status::Abandoned, Some("dup")).unwrap();
        sync(&sa, &SyncOptions::default()).unwrap();

        let err = sync(&sb, &SyncOptions::default()).unwrap_err();
        assert_eq!(err.code(), "sync_conflict");
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("a{c} status is abandoned here but failed")),
            "{msg}"
        );
        let d = err.details().unwrap();
        assert_eq!(d["conflicts"][0]["what"], format!("a{c}"));
        assert_eq!(d["conflicts"][0]["ours"], "abandoned");
        assert_eq!(d["conflicts"][0]["theirs"], "failed");
        // Nothing changed here.
        assert_eq!(sb.attempts(None).unwrap().len(), 4);

        let r = sync(
            &sb,
            &SyncOptions {
                prefer: Some(crate::syncmerge::Prefer::Theirs),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.merged.unwrap().resolved.len(), 1);
        assert_eq!(sb.attempt(c).unwrap().status, Status::Failed);
        assert_eq!(sb.attempts(None).unwrap().len(), 5);
        sync(&sa, &SyncOptions::default()).unwrap();
        assert_eq!(outline(&sa), outline(&sb));
    }

    /// A space synced before uids existed: machines that restored it derive
    /// the same uids for its rows, so merging doesn't duplicate them.
    #[test]
    fn spaces_synced_before_uids_migrate() {
        let (tmp, bare) = remote();
        let a = checkout(&tmp, &bare, "a");
        let sa = busy_space(&a);
        // What an older agentvc pushed: no uid anywhere.
        let id = space_id(&sa).unwrap();
        let mut doc = dump(&sa, &id).unwrap();
        for t in crate::syncmerge::UID_TABLES {
            for row in doc["tables"][*t].as_array_mut().unwrap() {
                row.as_object_mut().unwrap().remove("uid");
            }
        }
        let repo = HostRepo::open(&a).unwrap();
        let tree = build_tree(&sa, &doc).unwrap();
        crate::export::copy_tree_objects(&sa, &tree, &repo.objects_dir().unwrap()).unwrap();
        let commit = repo
            .git_env(
                &["commit-tree", &tree.to_hex(), "-m", "old sync"],
                None,
                &identity_env(&repo),
            )
            .unwrap();
        let refname = format!("{REF_PREFIX}{id}");
        repo.git(
            &["push", "-q", "origin", &format!("{commit}:{refname}")],
            None,
        )
        .unwrap();
        sa.set_meta(SYNC_COMMIT_KEY, &commit).unwrap();

        let b = checkout(&tmp, &bare, "b");
        let (sb, _) = clone_from_remote(&b, &CloneOptions::default()).unwrap();
        fork_as(&sa, 1, "agent:new-a", "a.rs");
        fork_as(&sb, 1, "agent:new-b", "b.rs");
        sync(&sa, &SyncOptions::default()).unwrap();
        sync(&sb, &SyncOptions::default()).unwrap();
        sync(&sa, &SyncOptions::default()).unwrap();
        assert_eq!(outline(&sa), outline(&sb));
        assert_eq!(sa.attempts(None).unwrap().len(), 5);
        assert_eq!(goals(&sa), ["goal"]);
        assert_eq!(sa.tags().unwrap().len(), 1);
    }

    #[test]
    fn spaces_from_two_sessions_live_side_by_side() {
        let (tmp, bare) = remote();
        let a = checkout(&tmp, &bare, "a");
        let b = checkout(&tmp, &bare, "b");
        git(&b, &["checkout", "-q", "-b", "thread-b"]);
        let sa = busy_space(&a);
        let sb = Space::init(&b).unwrap();
        sb.create_task("other task", None).unwrap();
        let ra = sync(&sa, &SyncOptions::default()).unwrap();
        let rb = sync(&sb, &SyncOptions::default()).unwrap();
        assert_ne!(ra.refname, rb.refname);

        let spaces = remote_spaces(&a, None).unwrap();
        assert_eq!(spaces.len(), 2);
        assert!(spaces.iter().any(|s| s.tasks == vec!["t1 other task"]));

        // Two candidates: the branch picks one, else --space is needed.
        let e = checkout(&tmp, &bare, "e");
        git(&e, &["checkout", "-q", "-b", "elsewhere"]);
        let err = clone_from_remote(&e, &CloneOptions::default())
            .err()
            .unwrap();
        assert_eq!(err.code(), "ambiguous_space");
        let strict = CloneOptions {
            this_branch: true,
            ..Default::default()
        };
        let err = clone_from_remote(&e, &strict).err().unwrap();
        assert_eq!(err.code(), "not_found");
        assert!(!e.join(SPACE_DIR).exists());
        let c = checkout(&tmp, &bare, "c");
        git(&c, &["checkout", "-q", "-b", "thread-b"]);
        let (copy, r) = clone_from_remote(&c, &CloneOptions::default()).unwrap();
        assert_eq!(r.space_id, rb.space_id);
        assert_eq!(copy.tasks().unwrap()[0].goal, "other task");

        let d = checkout(&tmp, &bare, "d");
        let (copy, _) = clone_from_remote(
            &d,
            &CloneOptions {
                space: Some(ra.space_id[..6].to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(history(&copy), history(&sa));
        assert_eq!(
            clone_from_remote(&d, &CloneOptions::default())
                .err()
                .unwrap()
                .code(),
            "space_exists"
        );
    }
}
