//! Workspaces (#107 follow-up): every worktree of a repo as a first-class
//! thing with an id, a name, its own stack and an activity state, so one
//! window, the review UI or an agent can see all of them and change any.
//!
//! Each workspace keeps its files in its own git dir, next to `stack.json`:
//!
//! - `agentvc/workspace.json`: id and name ([`Record`]), created on first
//!   sight by whoever lists the workspaces.
//! - `agentvc/busy.json`: an agent is working here ([`Busy`]); written by the
//!   agent hooks (or `agentvc workspace state`), stale after [`BUSY_TTL`].
//! - `agentvc/requests/<id>.json`: stack edits asked for from elsewhere
//!   ([`Request`]). An edit is applied at once when the workspace is idle and
//!   has no rebase in progress; otherwise it waits here and the workspace
//!   applies it at its next stack command or when its agent stops.
//! - `agentvc/stack.lock`: held by whoever changes the stack, so an edit from
//!   another window never interleaves with one made here.
//!
//! Workspaces on other machines are published as `refs/agentvc/workspaces/<id>`
//! (a commit holding `workspace.json` and `stack.json`) and listed read-only.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::error::{Error, IoContext, Result};
use crate::export::HostRepo;
use crate::space::{now, Space};
use crate::stack::{canon, read_state, state_path, ParentKind, Pending, State, Window};

/// Version of `workspace.json`.
pub const RECORD_VERSION: u32 = 1;
/// An agent that has not been heard from for this long is taken as gone.
pub const BUSY_TTL: i64 = 30 * 60;
/// A lock older than this is taken as left by a crashed process.
const LOCK_STALE: Duration = Duration::from_secs(120);
/// How long to wait for another process's lock.
const LOCK_WAIT: Duration = Duration::from_secs(10);
/// Finished requests kept per workspace (for the UI's history).
const KEEP_FINISHED: usize = 50;
/// Where published workspaces of other machines are fetched to.
pub const REMOTE_PREFIX: &str = "refs/agentvc/remote-workspaces/";
/// Where a workspace is published on the remote.
pub const PUBLISH_PREFIX: &str = "refs/agentvc/workspaces/";

fn ws_err(code: &'static str, message: impl Into<String>) -> Error {
    Error::Stack {
        code,
        message: message.into(),
        details: None,
    }
}

fn ws_err_with(code: &'static str, message: impl Into<String>, details: Value) -> Error {
    Error::Stack {
        code,
        message: message.into(),
        details: Some(details),
    }
}

fn agentvc_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("agentvc")
}

/// Write `text` to `path` atomically (temp file + rename).
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().expect("has a parent");
    fs::create_dir_all(dir).at(dir)?;
    let tmp = dir.join(crate::checkouts::tmp_name(
        path.file_name().and_then(|n| n.to_str()).unwrap_or("f"),
    ));
    fs::write(&tmp, text).at(&tmp)?;
    fs::rename(&tmp, path).at(path)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// This machine's host name.
#[cfg(unix)]
pub fn machine() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname NUL-terminates
    // on success (and we cap at the buffer otherwise).
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "localhost".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// This machine's host name.
#[cfg(not(unix))]
pub fn machine() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "localhost".into())
}

// ---- records ----------------------------------------------------------------

/// `workspace.json`: who a workspace is.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Record {
    pub version: u32,
    /// Stable id, `w` + 8 hex digits.
    pub id: String,
    /// Short name people use; unique among this repo's workspaces.
    pub name: String,
    pub created_at: i64,
    /// Nanoseconds past `created_at`, so that records made in the same
    /// second still sort by age; 0 in records written before #180.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub created_ns: u32,
    /// Machine it lives on.
    pub machine: String,
    /// Its directory there.
    pub path: PathBuf,
    /// The account that created it there (OS user name); empty in records
    /// written before #162.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// This process's OS user name, or empty.
pub fn user() -> String {
    ["USER", "USERNAME", "LOGNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default()
}

/// `path` with the home directory written as `~`, for showing where a
/// workspace lives.
pub fn tilde(path: &Path) -> String {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    if let Some(h) = home {
        if let Ok(rest) = path.strip_prefix(&h) {
            if rest.as_os_str().is_empty() {
                return "~".into();
            }
            return format!("~/{}", rest.to_string_lossy());
        }
    }
    path.to_string_lossy().into_owned()
}

fn new_id(git_dir: &Path) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha1::new();
    h.update(machine().as_bytes());
    h.update(git_dir.to_string_lossy().as_bytes());
    h.update(nanos.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    format!("w{}", &hex::encode(h.finalize())[..8])
}

/// The workspace record of `git_dir`, creating it (named after the
/// directory, made unique against `taken`) on first sight.
fn ensure_record(git_dir: &Path, path: &Path, taken: &[String]) -> Result<Record> {
    let file = agentvc_dir(git_dir).join("workspace.json");
    for _ in 0..3 {
        if let Some(r) = read_json::<Record>(&file) {
            return Ok(r);
        }
        if !file.exists() {
            break;
        }
        // Being written by another process right now.
        std::thread::sleep(Duration::from_millis(30));
    }
    let base = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();
    let mut name = base.clone();
    let mut n = 2;
    while taken.contains(&name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let rec = Record {
        version: RECORD_VERSION,
        id: new_id(git_dir),
        name,
        created_at: t.as_secs() as i64,
        created_ns: t.subsec_nanos(),
        machine: machine(),
        path: path.to_path_buf(),
        user: user(),
    };
    let dir = agentvc_dir(git_dir);
    fs::create_dir_all(&dir).at(&dir)?;
    let text = serde_json::to_string_pretty(&rec).expect("serializes") + "\n";
    // Publish the whole file at once, only if there is none yet: when two
    // processes race, the first record wins and the other reads it back, and
    // nobody ever reads a half-written record.
    let tmp = dir.join(crate::checkouts::tmp_name("workspace.json"));
    fs::write(&tmp, &text).at(&tmp)?;
    let linked = fs::hard_link(&tmp, &file);
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => return Ok(rec),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return read_json(&file).ok_or_else(|| {
                ws_err("bad_workspace", format!("{} is not valid", file.display()))
            });
        }
        // No hard links on this file system: create in place instead.
        Err(_) => {}
    }
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file)
    {
        Ok(mut f) => {
            f.write_all(text.as_bytes()).at(&file)?;
            Ok(rec)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::thread::sleep(Duration::from_millis(30));
            read_json(&file)
                .ok_or_else(|| ws_err("bad_workspace", format!("{} is not valid", file.display())))
        }
        Err(e) => Err(e).at(&file),
    }
}

fn write_record(git_dir: &Path, rec: &Record) -> Result<()> {
    write_atomic(
        &agentvc_dir(git_dir).join("workspace.json"),
        &(serde_json::to_string_pretty(rec).expect("serializes") + "\n"),
    )
}

// ---- activity ---------------------------------------------------------------

/// `busy.json`: an agent is working in this workspace.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Busy {
    pub since: i64,
    /// Last time the agent was heard from (each hook call).
    pub seen: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Idle,
    AgentRunning,
}

fn busy_path(git_dir: &Path) -> PathBuf {
    agentvc_dir(git_dir).join("busy.json")
}

/// The live busy record of `git_dir`, if an agent is working there.
fn busy(git_dir: &Path) -> Option<Busy> {
    let b: Busy = read_json(&busy_path(git_dir))?;
    (now() - b.seen <= BUSY_TTL).then_some(b)
}

/// Mark the workspace containing `cwd` busy (an agent is working) or idle.
/// Going idle applies the edits queued while it was busy.
pub fn set_activity(cwd: &Path, activity: Activity, by: Option<&str>) -> Result<Vec<Request>> {
    let w = Window::open(cwd)?;
    let path = busy_path(&w.git_dir);
    match activity {
        Activity::AgentRunning => {
            let t = now();
            let since = busy(&w.git_dir).map_or(t, |b| b.since);
            let b = Busy {
                since,
                seen: t,
                by: by.map(str::to_string),
            };
            write_atomic(
                &path,
                &(serde_json::to_string(&b).expect("serializes") + "\n"),
            )?;
            Ok(Vec::new())
        }
        Activity::Idle => {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).at(&path),
            }
            drain(cwd)
        }
    }
}

// ---- lock -------------------------------------------------------------------

/// Exclusive right to change one workspace's stack; released on drop.
pub struct Lock {
    path: PathBuf,
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Take the stack lock of the workspace whose git dir is `git_dir`, waiting
/// up to [`LOCK_WAIT`] for another process to finish.
pub fn lock(git_dir: &Path) -> Result<Lock> {
    let dir = agentvc_dir(git_dir);
    fs::create_dir_all(&dir).at(&dir)?;
    let path = dir.join("stack.lock");
    let start = Instant::now();
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "{}", std::process::id());
                return Ok(Lock { path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > LOCK_STALE);
                if stale {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if start.elapsed() > LOCK_WAIT {
                    return Err(ws_err_with(
                        "stack_locked",
                        format!(
                            "another agentvc command is changing this stack ({}); try again",
                            path.display()
                        ),
                        json!({ "lock": path }),
                    ));
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(e) => return Err(e).at(&path),
        }
    }
}

// ---- requests ---------------------------------------------------------------

/// A stack edit, run in the workspace that owns the branch.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Re-parent `branch` (with its upstack) onto `onto`.
    Move {
        branch: String,
        onto: String,
    },
    /// Restack `branch` and its upstack, or the whole stack.
    Restack {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
    Rename {
        branch: String,
        new_name: String,
    },
    Delete {
        branch: String,
    },
    /// Start a new branch on `parent` (the trunk or a branch of the stack;
    /// the current branch when left out) and check it out there. Edits in
    /// the worktree come along, as with `agentvc stack create NAME`.
    Create {
        branch: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    /// Fetch the trunk, drop merged branches, restack.
    Sync,
    /// Check out a branch in that workspace.
    Checkout {
        branch: String,
    },
    /// Undo that workspace's latest stack command (`agentvc stack undo`).
    Undo,
    /// Merge the stack's PRs up to `branch` on GitHub (`agentvc stack merge`).
    Merge {
        branch: String,
    },
    /// Push every branch of the stack and open or update one PR per branch
    /// (`agentvc stack submit --all`).
    Submit,
    /// Track an existing branch as a child of `parent` (an agent's result
    /// landing as a new branch).
    Adopt {
        branch: String,
        parent: String,
    },
    /// Move `branch` from `base` to `commit`, a descendant of it, and
    /// restack the branches above (an agent's result landing as a new
    /// version of `branch`).
    Revise {
        branch: String,
        commit: String,
        base: String,
    },
    /// Put an agent's resolution of a conflicting restack on the stack:
    /// `tree` resolves `conflict` (see [`crate::stack::Window::finish_hand_off`]).
    Resolve {
        branch: String,
        tree: String,
        conflict: crate::stack::HandOff,
    },
}

impl Op {
    pub fn branch(&self) -> Option<&str> {
        match self {
            Op::Move { branch, .. }
            | Op::Rename { branch, .. }
            | Op::Delete { branch }
            | Op::Checkout { branch }
            | Op::Create { branch, .. }
            | Op::Merge { branch }
            | Op::Adopt { branch, .. }
            | Op::Revise { branch, .. }
            | Op::Resolve { branch, .. } => Some(branch),
            Op::Restack { branch } => branch.as_deref(),
            Op::Sync | Op::Undo | Op::Submit => None,
        }
    }

    /// The command line this edit stands for, as the undo log shows it.
    pub fn label(&self) -> String {
        match self {
            Op::Move { branch, onto } => format!("move {branch} --onto {onto}"),
            Op::Restack { branch: Some(b) } => format!("restack --branch {b}"),
            Op::Restack { branch: None } => "restack".into(),
            Op::Rename { branch, new_name } => format!("rename {branch} {new_name}"),
            Op::Delete { branch } => format!("delete {branch}"),
            Op::Sync => "sync".into(),
            Op::Checkout { branch } => format!("checkout {branch}"),
            Op::Create {
                branch,
                parent: Some(p),
            } => format!("create {branch} --on {p}"),
            Op::Create {
                branch,
                parent: None,
            } => format!("create {branch}"),
            Op::Undo => "undo".into(),
            Op::Merge { branch } => format!("merge --branch {branch}"),
            Op::Submit => "submit --all".into(),
            Op::Adopt { branch, parent } => format!("adopt {branch} --parent {parent}"),
            Op::Revise { branch, commit, .. } => {
                format!("revise {branch} --to {}", &commit[..commit.len().min(10)])
            }
            Op::Resolve { branch, .. } => format!("resolve {branch}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Queued,
    Applied,
    Failed,
    Cancelled,
}

/// One edit asked of a workspace, kept in its `requests/` directory.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Request {
    /// `r` + 8 hex digits.
    pub id: String,
    /// The workspace it changes.
    pub workspace: String,
    #[serde(flatten)]
    pub op: Op,
    /// Leave a conflicting restack in the worktree to resolve (`git rebase`
    /// stopped there) instead of failing with nothing changed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_conflicts: bool,
    /// Resolve conflicts the way the workspace's `conflicts` setting says:
    /// hand them to an agent (the default), or as `keep_conflicts`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resolve: bool,
    /// Who asked: a workspace id, `api`, or an actor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub created_at: i64,
    pub status: RequestStatus,
    /// Why it failed, or was queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The error code when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// What the edit did (the stack command's JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
}

fn requests_dir(git_dir: &Path) -> PathBuf {
    agentvc_dir(git_dir).join("requests")
}

fn write_request(git_dir: &Path, r: &Request) -> Result<()> {
    write_atomic(
        &requests_dir(git_dir).join(format!("{}.json", r.id)),
        &(serde_json::to_string_pretty(r).expect("serializes") + "\n"),
    )
}

/// Requests of the workspace at `git_dir`, oldest first.
pub fn requests(git_dir: &Path) -> Vec<Request> {
    let mut out: Vec<Request> = fs::read_dir(requests_dir(git_dir))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                .filter_map(|e| read_json(&e.path()))
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
    out
}

/// Drop the oldest finished requests beyond [`KEEP_FINISHED`].
fn prune_requests(git_dir: &Path) {
    let done: Vec<Request> = requests(git_dir)
        .into_iter()
        .filter(|r| r.status != RequestStatus::Queued)
        .collect();
    if done.len() > KEEP_FINISHED {
        for r in &done[..done.len() - KEEP_FINISHED] {
            let _ = fs::remove_file(requests_dir(git_dir).join(format!("{}.json", r.id)));
        }
    }
}

fn new_request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha1::new();
    h.update(nanos.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(machine().as_bytes());
    format!("r{}", &hex::encode(h.finalize())[..8])
}

/// Run `op` on `w`. A restack that stops at a conflict is an error here
/// (nothing changed), so a queued edit reports it as failed; with `keep`
/// the conflict is left in the worktree to resolve and reported in the
/// result.
/// [`run_op`], then the agents for what it handed off.
fn run_op_with(w: &mut Window, op: &Op, how: OnConflict, workspace: &str) -> Result<Value> {
    let agent = how == OnConflict::Resolve && crate::agent::resolves_conflicts(w);
    let keep = how == OnConflict::Keep || (how == OnConflict::Resolve && !agent);
    w.hand_off = agent;
    let mut v = run_op(w, op, keep)?;
    let handed: Vec<crate::stack::HandOff> =
        [v.pointer("/handed_off"), v.pointer("/restack/handed_off")]
            .into_iter()
            .flatten()
            .filter_map(|h| serde_json::from_value::<Vec<crate::stack::HandOff>>(h.clone()).ok())
            .flatten()
            .collect();
    if !handed.is_empty() {
        let exe = crate::agent::agentvc_program();
        let started: Vec<Value> = handed
            .iter()
            .map(
                |h| match crate::agent::start_resolve(w.path(), workspace, h, &exe) {
                    Ok(a) => json!({ "branch": h.branch, "agent": a.id, "paths": h.paths }),
                    Err(e) => json!({
                        "branch": h.branch,
                        "paths": h.paths,
                        "error": e.to_string(),
                        "code": e.code(),
                    }),
                },
            )
            .collect();
        if let Some(o) = v.as_object_mut() {
            o.insert("resolving".into(), Value::Array(started));
        }
    }
    Ok(v)
}

fn run_op(w: &mut Window, op: &Op, keep: bool) -> Result<Value> {
    let restacked = |r: crate::stack::RestackResult| -> Result<Value> {
        if let Some(c) = r.conflict.as_ref().filter(|_| !keep) {
            return Err(ws_err_with(
                "conflict",
                format!(
                    "{} conflicts in {} at \"{}\"; nothing changed",
                    c.branch,
                    c.paths.join(", "),
                    c.subject
                ),
                json!({ "conflict": c }),
            ));
        }
        Ok(json!(r))
    };
    match op {
        Op::Move { branch, onto } => restacked(w.move_onto(branch, onto, keep)?),
        Op::Restack { branch } => {
            let scope = match branch {
                Some(b) => {
                    if !w.tracks(b) {
                        return Err(ws_err_with(
                            "not_in_stack",
                            format!("branch {b} is not in this workspace's stack"),
                            json!({ "branch": b }),
                        ));
                    }
                    Some(w.upstack(b))
                }
                None => None,
            };
            restacked(w.restack(scope.as_ref(), keep)?)
        }
        Op::Rename { branch, new_name } => Ok(json!(w.rename(branch, new_name)?)),
        Op::Delete { branch } => restacked(w.delete(branch, keep)?),
        Op::Checkout { branch } => {
            w.settle()?;
            Ok(json!({ "checked_out": w.switch(branch)? }))
        }
        Op::Create { branch, parent } => {
            w.settle()?;
            if let Some(p) = parent {
                let on_trunk = p == &w.state.trunk || p == &w.base_ref();
                let here = w.current_branch();
                let there = if on_trunk {
                    here.is_some_and(|c| c != w.state.trunk)
                } else {
                    here.as_deref() != Some(p.as_str())
                };
                if there {
                    w.checkout(p)?;
                }
            }
            let (t, _) = w.create(Some(branch), None, false)?;
            Ok(json!({ "created": t }))
        }
        Op::Undo => Ok(json!(crate::stackundo::undo(w, None)?)),
        Op::Merge { branch } => Ok(json!(w.merge_stack(&crate::stack::MergeOptions {
            branch: Some(branch.clone()),
            ..Default::default()
        })?)),
        Op::Submit => Ok(json!(w.submit(&crate::stack::SubmitOptions {
            all: true,
            ..Default::default()
        })?)),
        Op::Adopt { branch, parent } => Ok(json!(w.adopt(branch, Some(parent), false)?)),
        Op::Revise {
            branch,
            commit,
            base,
        } => {
            w.settle()?;
            if !w.tracks(branch) {
                return Err(ws_err_with(
                    "not_in_stack",
                    format!("branch {branch} is not in this workspace's stack"),
                    json!({ "branch": branch }),
                ));
            }
            let tip = w.branch_tip(branch).ok_or_else(|| {
                ws_err_with(
                    "no_branch",
                    format!("branch {branch} is gone"),
                    json!({ "branch": branch }),
                )
            })?;
            if tip != *base {
                return Err(ws_err_with(
                    "branch_moved",
                    format!("{branch} changed since the agent started; start a new agent from it or land as a new branch"),
                    json!({ "branch": branch, "tip": tip, "base": base }),
                ));
            }
            let checked_out = w.current_branch().as_deref() == Some(branch.as_str());
            if checked_out {
                w.repo
                    .git(&["merge", "--ff-only", "--quiet", commit], None)?;
            } else {
                w.repo.git(
                    &["update-ref", &format!("refs/heads/{branch}"), commit, &tip],
                    None,
                )?;
            }
            let mut scope = w.upstack(branch);
            scope.remove(branch);
            match w.restack(Some(&scope), keep) {
                Ok(r) => restacked(r),
                Err(e) => {
                    // The restack above failed with nothing changed: put the
                    // branch back too, so the edit is all or nothing (#203).
                    if checked_out {
                        let _ = w.repo.git(&["reset", "--keep", "--quiet", &tip], None);
                    } else {
                        let _ = w.repo.git(
                            &["update-ref", &format!("refs/heads/{branch}"), &tip, commit],
                            None,
                        );
                    }
                    Err(e)
                }
            }
        }
        Op::Resolve {
            branch,
            tree,
            conflict,
        } => {
            if branch != &conflict.branch {
                return Err(ws_err(
                    "bad_request",
                    "the resolution is for another branch",
                ));
            }
            restacked(w.finish_hand_off(conflict, tree)?)
        }
        Op::Sync => {
            let s = w.sync(true, true, keep)?;
            if s.restack.conflict.is_some() {
                restacked(s.restack.clone())?;
            }
            Ok(json!(s))
        }
    }
}

/// Run `req` in `w` and record the outcome in the request file.
fn settle_request(w: &mut Window, mut req: Request) -> (Request, Result<Value>) {
    let path = w.path().to_path_buf();
    // Undo and submit are not recorded (a push cannot be taken back).
    let recorded = !matches!(req.op, Op::Undo | Op::Submit);
    let begun = recorded.then(|| crate::stackundo::begin(&path)).flatten();
    let on_conflict = if req.resolve {
        OnConflict::Resolve
    } else if req.keep_conflicts {
        OnConflict::Keep
    } else {
        OnConflict::Fail
    };
    let res = run_op_with(w, &req.op, on_conflict, &req.workspace);
    if recorded {
        crate::stackundo::finish(&path, &req.op.label(), begun);
    }
    req.finished_at = Some(now());
    match &res {
        Ok(v) => {
            req.status = RequestStatus::Applied;
            req.reason = None;
            req.result = Some(v.clone());
        }
        Err(e) => {
            req.status = RequestStatus::Failed;
            req.code = Some(e.code().to_string());
            req.reason = Some(e.to_string());
        }
    }
    let _ = write_request(&w.git_dir, &req);
    (req, res)
}

/// Why edits to `w` have to wait, if they do.
fn must_queue(w: &Window) -> Option<String> {
    if let Some(b) = busy(&w.git_dir) {
        return Some(format!(
            "an agent{} is working in this workspace",
            b.by.map(|s| format!(" ({s})")).unwrap_or_default()
        ));
    }
    if w.state.pending.is_some() || w.rebase_in_progress() {
        return Some("a rebase is in progress in this workspace".into());
    }
    None
}

/// How [`edit`] went.
#[derive(Clone, Debug, Serialize)]
pub struct EditOutcome {
    /// `applied` or `queued`.
    pub outcome: RequestStatus,
    pub request_id: String,
    pub workspace: String,
    /// Why it was queued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What it did, when applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

/// Change the stack of workspace `target` (id, name or directory) from
/// anywhere in the repo. Applied at once when that workspace is idle;
/// queued when an agent is working there (or `queue` asks for it). An edit
/// applied at once that fails returns the error and is recorded as failed.
pub fn edit(
    cwd: &Path,
    target: &str,
    op: Op,
    from: Option<&str>,
    queue: bool,
) -> Result<EditOutcome> {
    edit_with(cwd, target, op, from, queue, false)
}

/// What a stack edit does when a branch conflicts with its parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    /// Fail with code `conflict`; nothing changes.
    Fail,
    /// Stop in the workspace's worktree for a person (`keep_conflicts`).
    Keep,
    /// What the workspace's `conflicts` setting says: start an agent that
    /// resolves each conflicting branch in its own worktree (`agent`, the
    /// default), or [`OnConflict::Keep`] (`manual`). The other branches
    /// restack either way.
    Resolve,
}

/// [`edit`], leaving a conflicting restack in the worktree when
/// `keep_conflicts` (see [`conflicts`], [`resolve`] and [`finish`]).
pub fn edit_with(
    cwd: &Path,
    target: &str,
    op: Op,
    from: Option<&str>,
    queue: bool,
    keep_conflicts: bool,
) -> Result<EditOutcome> {
    let how = if keep_conflicts {
        OnConflict::Keep
    } else {
        OnConflict::Fail
    };
    edit_full(cwd, target, op, from, queue, how)
}

/// [`edit`] with a choice of what conflicts do.
pub fn edit_full(
    cwd: &Path,
    target: &str,
    op: Op,
    from: Option<&str>,
    queue: bool,
    on_conflict: OnConflict,
) -> Result<EditOutcome> {
    let here = Window::open(cwd)?;
    let ws = find_local(&here, target)?;
    let _lock = lock(&ws.git_dir)?;
    let mut w = Window::open(&ws.path)?;
    let mut req = Request {
        id: new_request_id(),
        workspace: ws.record.id.clone(),
        op,
        keep_conflicts: on_conflict == OnConflict::Keep,
        resolve: on_conflict == OnConflict::Resolve,
        from: from.map(str::to_string),
        created_at: now(),
        status: RequestStatus::Queued,
        reason: None,
        code: None,
        result: None,
        finished_at: None,
    };
    let why = must_queue(&w).or_else(|| queue.then(|| "queued on request".to_string()));
    if let Some(why) = why {
        req.reason = Some(why.clone());
        write_request(&w.git_dir, &req)?;
        return Ok(EditOutcome {
            outcome: RequestStatus::Queued,
            request_id: req.id,
            workspace: ws.record.id,
            reason: Some(why),
            result: None,
        });
    }
    let id = req.id.clone();
    let (_, res) = settle_request(&mut w, req);
    prune_requests(&w.git_dir);
    let v = res?;
    Ok(EditOutcome {
        outcome: RequestStatus::Applied,
        request_id: id,
        workspace: ws.record.id,
        reason: None,
        result: Some(v),
    })
}

/// Refresh the PR checks and review of every local workspace's stack, so
/// `workspace ls` and the review UI show them (the daemon's PR poller).
/// Returns how many PRs were refreshed. Skips a workspace whose stack is
/// locked by a command in progress.
pub fn refresh_prs(cwd: &Path) -> Result<usize> {
    let here = Window::open(cwd)?;
    let mut n = 0;
    for l in space_locals(&here)? {
        let Ok(w) = Window::open(&l.path) else {
            continue;
        };
        let open =
            w.state.branches.iter().any(|b| {
                b.pr.is_some() && !matches!(b.pr_state.as_deref(), Some("merged" | "closed"))
            });
        if !open {
            continue;
        }
        let Ok(_lock) = lock(&l.git_dir) else {
            continue;
        };
        let mut w = Window::open(&l.path)?;
        n += w.refresh_prs(None)?.len();
    }
    Ok(n)
}

/// Apply the edits queued for the workspace containing `cwd`, oldest first.
/// Takes the stack lock; see [`drain_locked`] when already holding it.
pub fn drain(cwd: &Path) -> Result<Vec<Request>> {
    let w = Window::open(cwd)?;
    if !requests_dir(&w.git_dir).exists() {
        return Ok(Vec::new());
    }
    let _lock = lock(&w.git_dir)?;
    drain_locked(cwd)
}

/// [`drain`] for a caller that holds the workspace's stack lock. Stops
/// early (leaving the rest queued) while a rebase is in progress.
pub fn drain_locked(cwd: &Path) -> Result<Vec<Request>> {
    let probe = Window::open(cwd)?;
    let queued: Vec<Request> = requests(&probe.git_dir)
        .into_iter()
        .filter(|r| r.status == RequestStatus::Queued)
        .collect();
    let mut done = Vec::new();
    for req in queued {
        let mut w = Window::open(cwd)?;
        if w.state.pending.is_some() || w.rebase_in_progress() {
            break;
        }
        let (req, _) = settle_request(&mut w, req);
        done.push(req);
    }
    if !done.is_empty() {
        prune_requests(&probe.git_dir);
    }
    Ok(done)
}

/// Cancel a queued request, in whichever workspace of the repo holds it.
pub fn cancel(cwd: &Path, request_id: &str) -> Result<Request> {
    let here = Window::open(cwd)?;
    let (_, sib) = crate::checkouts::space_and_siblings(&here);
    let mut dirs = here.worktrees();
    for (sw, _) in sibling_locals(&sib) {
        dirs.extend(sw.worktrees());
    }
    for (git_dir, _) in dirs {
        let file = requests_dir(&git_dir).join(format!("{request_id}.json"));
        if !file.exists() {
            continue;
        }
        let _lock = lock(&git_dir)?;
        let mut r: Request = read_json(&file)
            .ok_or_else(|| ws_err("bad_request", format!("{} is not valid", file.display())))?;
        if r.status != RequestStatus::Queued {
            return Err(ws_err_with(
                "request_not_queued",
                format!("request {request_id} is already {:?}", r.status).to_lowercase(),
                json!({ "request": r }),
            ));
        }
        r.status = RequestStatus::Cancelled;
        r.finished_at = Some(now());
        write_request(&git_dir, &r)?;
        return Ok(r);
    }
    Err(ws_err_with(
        "no_request",
        format!("no request {request_id} in this space's workspaces"),
        json!({ "request_id": request_id }),
    ))
}

// ---- listing ----------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The repo's main worktree.
    Main,
    /// A linked worktree (`git worktree add`).
    Worktree,
    /// Published from another machine; read-only here.
    Remote,
}

/// A workspace found on this machine.
#[derive(Clone, Debug)]
pub struct Local {
    pub record: Record,
    pub git_dir: PathBuf,
    pub path: PathBuf,
    pub kind: Kind,
    /// The main worktree of the checkout (clone) it belongs to.
    pub checkout: PathBuf,
    /// It belongs to another checkout of the same space (#160), not to the
    /// repo it was looked up from.
    pub other_checkout: bool,
}

/// Every workspace of the repo `w` belongs to, records created as needed.
pub fn locals(w: &Window) -> Result<Vec<Local>> {
    let mut out: Vec<Local> = Vec::new();
    let trees = w.worktrees();
    let checkout = trees
        .first()
        .map(|(_, p)| p.clone())
        .unwrap_or_else(|| w.path().to_path_buf());
    for (i, (git_dir, path)) in trees.into_iter().enumerate() {
        let taken: Vec<String> = out.iter().map(|l| l.record.name.clone()).collect();
        let record = ensure_record(&git_dir, &path, &taken)?;
        out.push(Local {
            record,
            git_dir,
            path,
            kind: if i == 0 { Kind::Main } else { Kind::Worktree },
            checkout: checkout.clone(),
            other_checkout: false,
        });
    }
    Ok(out)
}

/// The workspaces of the other checkouts on this machine in the same space
/// as `w` (#160), each with the window of its checkout.
fn sibling_locals(siblings: &[PathBuf]) -> Vec<(Window, Vec<Local>)> {
    let mut out = Vec::new();
    for p in siblings {
        let Ok(sw) = Window::open(p) else { continue };
        let Ok(mut ls) = locals(&sw) else { continue };
        for l in &mut ls {
            l.other_checkout = true;
        }
        out.push((sw, ls));
    }
    out
}

/// Every workspace of the space `w` belongs to on this machine: its own
/// repo's first, then those of the other checkouts in its space (#160).
pub fn space_locals(w: &Window) -> Result<Vec<Local>> {
    let mut groups = vec![locals(w)?];
    let (_, sib) = crate::checkouts::space_and_siblings(w);
    for (_, ls) in sibling_locals(&sib) {
        groups.push(ls);
    }
    dedupe_names(&mut groups);
    Ok(groups.into_iter().flatten().collect())
}

/// Workspace names must be unique in a space, but each checkout names its
/// own: two clones in folders both called `mono` start out as two `mono`s.
/// The older keeps the name; the newer becomes `mono-2` (saved, so every
/// checkout sees the same names from then on).
///
/// Age is `created_at` to the nanosecond; the id only breaks ties between
/// old records that kept whole seconds. A new name is unique against every
/// name in the space, not only the older ones, so settling one clash never
/// makes another.
fn dedupe_names(groups: &mut [Vec<Local>]) {
    let mut order: Vec<(i64, u32, String, usize, usize)> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for (g, ls) in groups.iter().enumerate() {
        for (i, l) in ls.iter().enumerate() {
            let r = &l.record;
            order.push((r.created_at, r.created_ns, r.id.clone(), g, i));
            names.push(r.name.clone());
        }
    }
    order.sort();
    let mut seen: Vec<String> = Vec::new();
    for (.., g, i) in order {
        let l = &mut groups[g][i];
        if seen.contains(&l.record.name) {
            let all: Vec<&str> = names.iter().map(String::as_str).collect();
            let name = unique_name(&l.record.name, &all);
            names.push(name.clone());
            l.record.name = name;
            let _ = write_record(&l.git_dir, &l.record);
        }
        seen.push(l.record.name.clone());
    }
}

/// The workspace `target` names, in this repo or another checkout of its
/// space on this machine: an id, a name, or a directory. This repo's
/// workspaces win when a name is ambiguous.
pub fn find_local(w: &Window, target: &str) -> Result<Local> {
    let as_path = canon(Path::new(target));
    let pick = |all: &[Local]| -> Option<Local> {
        all.iter()
            .find(|l| l.record.id == target)
            .or_else(|| all.iter().find(|l| l.record.name == target))
            .or_else(|| all.iter().find(|l| canon(&l.path) == as_path))
            .cloned()
    };
    let own = locals(w)?;
    let found = pick(&own).or_else(|| {
        let (_, sib) = crate::checkouts::space_and_siblings(w);
        sibling_locals(&sib)
            .into_iter()
            .find_map(|(_, ls)| pick(&ls))
    });
    found
        .ok_or_else(|| {
            let remote = w.repo.rev(&format!("{REMOTE_PREFIX}{target}")).is_some();
            if remote {
                ws_err_with(
                    "remote_workspace",
                    format!("workspace {target} is on another machine; change it there"),
                    json!({ "workspace": target }),
                )
            } else {
                ws_err_with(
                    "no_workspace",
                    format!(
                        "no workspace {target} in this repo or its space; `agentvc workspace ls` lists them"
                    ),
                    json!({ "workspace": target }),
                )
            }
        })
}

/// Rename the workspace containing `cwd`.
pub fn set_name(cwd: &Path, name: &str) -> Result<Record> {
    let name = name.trim();
    check_name(name)?;
    let w = Window::open(cwd)?;
    // Names are unique in the whole space, so they work from any checkout.
    let all = space_locals(&w)?;
    if all
        .iter()
        .any(|l| l.record.name == name && l.git_dir != w.git_dir)
    {
        return Err(ws_err_with(
            "name_taken",
            format!("another workspace in this space is already named {name}"),
            json!({ "name": name }),
        ));
    }
    let mut rec = all
        .into_iter()
        .find(|l| l.git_dir == w.git_dir)
        .map(|l| l.record)
        .expect("this workspace is listed");
    rec.name = name.to_string();
    write_record(&w.git_dir, &rec)?;
    Ok(rec)
}

/// How [`create`] makes a workspace.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NewKind {
    /// `git worktree add`: shares this checkout's git objects.
    Worktree,
    /// A fresh clone in its own folder, for builds that cannot run twice in
    /// one checkout (#160); it joins the same space.
    Clone,
}

/// How [`create`] fills a new workspace's build and dependency folders.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SeedMode {
    /// Copy-on-write clones when the filesystem can (APFS, btrfs, XFS);
    /// otherwise leave them out.
    #[default]
    Auto,
    /// Like `Auto`, but copy byte by byte when there is no copy-on-write.
    Copy,
    /// Start with none: the first build and install run from scratch.
    Off,
}

/// What [`create`] makes.
#[derive(Clone, Debug, Default)]
pub struct NewOptions {
    /// Its name (unique in the space); defaults to its folder's name.
    pub name: Option<String>,
    /// A clone instead of a worktree.
    pub clone: bool,
    /// Join this space instead of the source checkout's (`none`: no space).
    /// Only for a clone: worktrees share their clone's git config.
    pub space: Option<String>,
    pub seed: SeedMode,
    /// Folders to seed (relative to the workspace). Empty: git config
    /// `agentvc.seed` (multi-valued), else the usual build and dependency
    /// folders that exist and are git-ignored.
    pub seed_paths: Vec<String>,
}

/// One build or dependency folder [`create`] seeded (or could not).
#[derive(Clone, Debug, Serialize)]
pub struct Seeded {
    pub path: String,
    /// `reflink`, `mixed`, `copy`, or `skipped`.
    pub how: String,
    pub files: u64,
    /// Bytes actually copied (0 when every file was a copy-on-write clone).
    pub bytes_copied: u64,
    pub millis: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A workspace just made by [`create`].
#[derive(Clone, Debug, Serialize)]
pub struct Created {
    pub workspace: Record,
    pub kind: NewKind,
    pub folder: String,
    /// The space it joined, if any.
    pub space: Option<crate::checkouts::SpaceRef>,
    /// The workspace its build and dependency folders came from.
    pub seeded_from: String,
    pub seeded: Vec<Seeded>,
}

/// Build and dependency folders worth carrying into a new workspace, when
/// they exist and git ignores them. `*/` entries look one and two levels
/// down (monorepo packages).
pub const DEFAULT_SEEDS: &[&str] = &[
    "target",
    "node_modules",
    "*/node_modules",
    "*/*/node_modules",
    ".venv",
    "venv",
    "build",
    "dist",
    ".next",
    ".turbo",
    ".gradle",
    ".angular",
    ".nuxt",
    ".svelte-kit",
];

/// The seed folders of workspace `src` (relative paths that exist there).
fn seed_paths(w: &Window, src: &Path, wanted: &[String]) -> Vec<String> {
    let explicit = !wanted.is_empty();
    let mut pats: Vec<String> = wanted.to_vec();
    if pats.is_empty() {
        pats = w
            .repo
            .git(&["config", "--get-all", "agentvc.seed"], None)
            .map(|o| {
                o.lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect()
            })
            .unwrap_or_default();
    }
    let configured = !pats.is_empty();
    if pats.is_empty() {
        pats = DEFAULT_SEEDS.iter().map(|s| s.to_string()).collect();
    }
    let mut out: Vec<String> = Vec::new();
    for p in &pats {
        for rel in expand(src, p) {
            if !src.join(&rel).is_dir() || out.contains(&rel) {
                continue;
            }
            // Never seed what git tracks: the checkout has it already.
            // Guessed folders must also be ignored, not just untracked.
            let ignored = HostRepo {
                workdir: src.to_path_buf(),
            }
            .git(&["check-ignore", "-q", &rel], None)
            .is_ok();
            if ignored || ((explicit || configured) && !tracked(src, &rel)) {
                out.push(rel);
            }
        }
    }
    out
}

fn tracked(src: &Path, rel: &str) -> bool {
    HostRepo {
        workdir: src.to_path_buf(),
    }
    .git(&["ls-files", "--", rel], None)
    .map(|o| !o.trim().is_empty())
    .unwrap_or(false)
}

/// `a/*/b` against the folders under `root` (only `*` as a whole part).
fn expand(root: &Path, pat: &str) -> Vec<String> {
    let mut found = vec![String::new()];
    for part in pat.split('/').filter(|p| !p.is_empty()) {
        let mut next = Vec::new();
        for base in &found {
            let dir = if base.is_empty() {
                root.to_path_buf()
            } else {
                root.join(base)
            };
            let join = |n: &str| {
                if base.is_empty() {
                    n.to_string()
                } else {
                    format!("{base}/{n}")
                }
            };
            if part == "*" {
                if let Ok(rd) = fs::read_dir(&dir) {
                    let mut names: Vec<String> = rd
                        .flatten()
                        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| n != ".git" && n != ".agentvc" && n != "node_modules")
                        .collect();
                    names.sort();
                    next.extend(names.iter().map(|n| join(n)));
                }
            } else if part != ".." {
                next.push(join(part));
            }
        }
        found = next;
    }
    found.into_iter().filter(|f| !f.is_empty()).collect()
}

/// Copy the build and dependency folders of `src` into `dst` with
/// copy-on-write clones (or plain copies with [`SeedMode::Copy`]).
fn seed(w: &Window, src: &Path, dst: &Path, o: &NewOptions) -> Vec<Seeded> {
    if o.seed == SeedMode::Off {
        return Vec::new();
    }
    let paths = seed_paths(w, src, &o.seed_paths);
    if paths.is_empty() {
        return Vec::new();
    }
    // macOS clones whole folders with clonefile; elsewhere probe reflinks.
    let cow = cfg!(target_os = "macos") || crate::view::reflinks_supported(dst).is_ok();
    let mut out = Vec::new();
    for rel in paths {
        let to = dst.join(&rel);
        if !cow && o.seed == SeedMode::Auto {
            out.push(Seeded {
                path: rel,
                how: "skipped".into(),
                files: 0,
                bytes_copied: 0,
                millis: 0,
                reason: Some("no copy-on-write on this filesystem; --seed copy copies it".into()),
            });
            continue;
        }
        if to.exists() {
            continue;
        }
        let res = to
            .parent()
            .map(|p| fs::create_dir_all(p).at(p))
            .unwrap_or(Ok(()))
            .and_then(|_| crate::view::fork_dir(&src.join(&rel), &to, !cow));
        out.push(match res {
            Ok(st) => Seeded {
                path: rel,
                how: st.backend().as_str().into(),
                files: st.files,
                bytes_copied: st.bytes_copied,
                millis: st.millis,
                reason: None,
            },
            Err(e) => {
                let _ = fs::remove_dir_all(&to);
                Seeded {
                    path: rel,
                    how: "skipped".into(),
                    files: 0,
                    bytes_copied: 0,
                    millis: 0,
                    reason: Some(e.to_string()),
                }
            }
        });
    }
    out
}

/// Make a new workspace at `dir` from the checkout containing `cwd`.
///
/// Cheap by design (docs/spaces.md, "Cheap workspaces"):
/// - a worktree shares the clone's git objects;
/// - a clone is cloned from this checkout on disk (git hardlinks its
///   objects), then pointed at the real remote and fetched, so it is
///   independent yet costs almost no disk or network;
/// - build and dependency folders of the workspace at `cwd` are seeded with
///   copy-on-write clones, so the first build and install are incremental.
pub fn create(cwd: &Path, dir: &Path, o: &NewOptions) -> Result<Created> {
    let w = Window::open(cwd)?;
    let dir = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        crate::view::canonical(cwd)
            .unwrap_or_else(|_| cwd.to_path_buf())
            .join(dir)
    };
    // Tidy `../x` so the folder shown (and recorded) is the real one. Not
    // `canon`: git rejects Windows verbatim (`\\?\`) paths.
    let dir = match (dir.parent(), dir.file_name()) {
        (Some(p), Some(n)) => crate::view::canonical(p)
            .map(|p| p.join(n))
            .unwrap_or(dir.clone()),
        _ => dir,
    };
    if dir.exists()
        && fs::read_dir(&dir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(true)
    {
        return Err(ws_err_with(
            "dir_exists",
            format!("{} already exists and is not empty", dir.display()),
            json!({ "path": dir }),
        ));
    }
    if o.space.is_some() && !o.clone {
        return Err(ws_err_with(
            "worktree_space",
            "a worktree is in its clone's space; use --clone to start a workspace in another space",
            json!({ "space": o.space }),
        ));
    }
    if let Some(n) = o.name.as_deref() {
        check_name(n)?;
        if space_locals(&w)?.iter().any(|l| l.record.name == n) {
            return Err(ws_err_with(
                "name_taken",
                format!("another workspace in this space is already named {n}"),
                json!({ "name": n }),
            ));
        }
    }
    let dir_s = dir.to_string_lossy().into_owned();
    let (key, source) = crate::checkouts::space_of(&w);
    let kind = if o.clone {
        NewKind::Clone
    } else {
        NewKind::Worktree
    };
    let remote = if w.state.remote.is_empty() {
        "origin".to_string()
    } else {
        w.state.remote.clone()
    };
    match kind {
        NewKind::Worktree => {
            let at = w.base().unwrap_or_else(|| "HEAD".into());
            w.repo
                .git(&["worktree", "add", "--detach", &dir_s, &at], None)?;
        }
        NewKind::Clone => {
            let url = w
                .repo
                .git(&["remote", "get-url", &remote], None)
                .map_err(|_| {
                    ws_err_with(
                        "no_remote",
                        format!("this checkout has no remote {remote} to clone"),
                        json!({ "remote": remote }),
                    )
                })?;
            let url = url.trim().to_string();
            // From this checkout on disk: git hardlinks the objects (copies
            // them across filesystems), so nothing is downloaded twice.
            let main = crate::checkouts::main_path(&w);
            w.repo.git(
                &[
                    "clone",
                    "--quiet",
                    "--no-checkout",
                    "--origin",
                    &remote,
                    &main.to_string_lossy(),
                    &dir_s,
                ],
                None,
            )?;
            let nr = HostRepo {
                workdir: dir.clone(),
            };
            // The clone's remote branches are this checkout's *local* ones;
            // swap them for this checkout's copy of the real remote's (a
            // local fetch), then point at the real remote and catch up.
            let stale = nr
                .git(
                    &[
                        "for-each-ref",
                        "--format=%(refname)",
                        &format!("refs/remotes/{remote}/"),
                    ],
                    None,
                )
                .unwrap_or_default();
            for r in stale.lines().filter(|l| !l.is_empty()) {
                let _ = nr.git(&["update-ref", "-d", r], None);
            }
            let spec = format!("+refs/remotes/{remote}/*:refs/remotes/{remote}/*");
            let _ = nr.git(
                &[
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    &main.to_string_lossy(),
                    &spec,
                ],
                None,
            );
            nr.git(&["remote", "set-url", &remote, &url], None)?;
            // Offline is fine: the local copy is already complete.
            let _ = nr.git(&["fetch", "--quiet", "--prune", &remote], None);
            let trunk = w.state.trunk.clone();
            let head = nr
                .git(&["symbolic-ref", "--quiet", "--short", "HEAD"], None)
                .unwrap_or_default();
            let start = format!("{remote}/{trunk}");
            if nr.rev(&format!("refs/remotes/{start}")).is_some() {
                nr.git(&["checkout", "--quiet", "-B", &trunk, &start], None)?;
            } else {
                nr.git(&["reset", "--quiet", "--hard", "HEAD"], None)?;
            }
            let head = head.trim();
            if !head.is_empty() && head != trunk {
                // This checkout's current branch is not the new clone's.
                let _ = nr.git(&["branch", "-D", head], None);
            }
        }
    }
    let nw = Window::open(&dir)?;
    if kind == NewKind::Clone {
        match &o.space {
            Some(s) => crate::checkouts::set_space(&nw, Some(s))?,
            None if source == crate::checkouts::Source::Config => {
                crate::checkouts::set_space(&nw, key.as_deref().or(Some(crate::checkouts::NONE)))?
            }
            None => {}
        }
    }
    let seeded = seed(&w, w.path(), &dir, o);
    // Name it before anything lists the space: listing settles clashing
    // names by age, and the newest record would lose its name. Hold the
    // registry lock until it is registered, so that two workspaces made at
    // once each see the other's name.
    let lock = crate::checkouts::lock_registry()?;
    let own = locals(&nw)?;
    let mut rec = own
        .iter()
        .find(|l| l.git_dir == nw.git_dir)
        .map(|l| l.record.clone())
        .expect("the new workspace is listed");
    let (sp, _) = crate::checkouts::space_of(&nw);
    let sib = crate::checkouts::siblings(&nw, sp.as_deref());
    let mut taken: Vec<String> = own
        .iter()
        .filter(|l| l.git_dir != nw.git_dir)
        .map(|l| l.record.name.clone())
        .collect();
    for (_, ls) in sibling_locals(&sib) {
        taken.extend(ls.into_iter().map(|l| l.record.name));
    }
    let taken: Vec<&str> = taken.iter().map(String::as_str).collect();
    let base = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();
    let want = match o.name.as_deref() {
        Some(n) => n.to_string(),
        None => unique_name(&base, &taken),
    };
    if want != rec.name {
        rec.name = want;
        write_record(&nw.git_dir, &rec)?;
    }
    // Now join the registry, so the space lists it.
    crate::checkouts::register_locked(&nw, lock.as_ref());
    drop(lock);
    let from = locals(&w)?
        .into_iter()
        .find(|l| l.git_dir == w.git_dir)
        .map(|l| l.record.name)
        .unwrap_or_default();
    Ok(Created {
        folder: tilde(nw.path()),
        workspace: rec,
        kind,
        space: crate::checkouts::describe(&nw),
        seeded_from: from,
        seeded,
    })
}

/// `base`, else `base-2`, `base-3`, ... whichever is not taken.
fn unique_name(base: &str, taken: &[&str]) -> String {
    let mut n = base.to_string();
    let mut i = 2;
    while taken.contains(&n.as_str()) {
        n = format!("{base}-{i}");
        i += 1;
    }
    n
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains(['/', '\\']) || name.chars().any(char::is_whitespace) {
        return Err(ws_err_with(
            "bad_name",
            format!("{name:?} is not a workspace name: use letters, digits, - and _"),
            json!({ "name": name }),
        ));
    }
    Ok(())
}

/// A pull request as the workspace list shows it.
#[derive(Clone, Debug, Serialize)]
pub struct PrRef {
    pub number: u64,
    /// `open`, `draft`, `closed` or `merged`, as last seen; `None` if never
    /// looked up.
    pub state: Option<String>,
    /// Checks and review as last fetched (`stack log --prs`, `stack merge`,
    /// the daemon's PR poller).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<crate::stack::PrBrief>,
}

/// One branch of a workspace's stack: everything a graph row needs.
#[derive(Clone, Debug, Serialize)]
pub struct WsBranch {
    pub name: String,
    pub workspace: String,
    pub parent: String,
    pub parent_kind: ParentKind,
    /// The workspace owning the parent, for `other_window`.
    pub parent_workspace: Option<String>,
    /// The parent commit it sits on (a trunk commit for bottom branches).
    pub parent_rev: String,
    pub tip: Option<String>,
    /// Its own commits, `parent_rev..tip`.
    pub commits: usize,
    pub needs_restack: bool,
    /// Commits on the base (e.g. `origin/main`) it does not have.
    pub behind: usize,
    /// Files with conflicts, while this branch is being rebased in its
    /// workspace.
    pub conflicts: usize,
    pub current: bool,
    pub depth: usize,
    pub children: Vec<String>,
    pub pr: Option<PrRef>,
    /// The agentvc attempt this branch was exported from, and its task.
    pub attempt: Option<i64>,
    pub task: Option<i64>,
    /// `running` (a run is active), or the attempt's status.
    pub status: Option<String>,
    /// Open review comments on it (#167).
    pub comments: usize,
}

/// One workspace with its stack.
#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceInfo {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    /// The machine it lives on.
    pub machine: String,
    /// Its folder there.
    pub path: PathBuf,
    /// [`Self::path`] for people: `~/...` when under the home directory.
    pub folder: String,
    /// It is on the machine that listed it (not published from another).
    pub this_machine: bool,
    /// Who works there: the OS account that created it, when known.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// The workspace `ls` was run from.
    pub here: bool,
    pub state: Activity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<Busy>,
    /// Last change to its stack file (unix seconds).
    pub last_seen: Option<i64>,
    /// The checked-out branch.
    pub current: Option<String>,
    pub trunk: String,
    pub base_ref: String,
    pub base: Option<String>,
    pub pending: Option<Pending>,
    pub branches: Vec<WsBranch>,
    /// Edits asked of it (queued ones and recent history).
    pub requests: Vec<Request>,
    /// The main worktree of the checkout (clone) it belongs to (#160).
    pub checkout: PathBuf,
    /// It belongs to another checkout of this space on this machine.
    #[serde(default)]
    pub other_checkout: bool,
    /// How agents started here run (`agentvc workspace agents`).
    pub agent_settings: crate::agent::AgentSettings,
    /// The coding tool that made this worktree for one of its own agents
    /// (e.g. `Claude Code` for `.claude/worktrees/agent-…`), when it is one.
    /// Such worktrees are listed under [`Self::nested_in`], not on their own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The workspace a tool worktree belongs to: the one whose folder holds
    /// it, else its checkout's main worktree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nested_in: Option<String>,
}

/// Where coding tools put the worktrees they make for their own agents.
const TOOL_WORKTREES: &[(&str, &str)] = &[
    (".claude/worktrees", "Claude Code"),
    (".cursor/worktrees", "Cursor"),
    (".codex/worktrees", "Codex"),
];

/// The tool that made the worktree at `path` for one of its agents, and
/// the folder it was made from when the tool keeps it inside that folder.
pub fn tool_worktree(path: &Path) -> Option<(&'static str, Option<PathBuf>)> {
    let p = path.to_string_lossy().replace('\\', "/");
    TOOL_WORKTREES.iter().find_map(|(dir, tool)| {
        let at = p.find(&format!("/{dir}/"))?;
        let owner = PathBuf::from(&p[..at]);
        // `~/.cursor/worktrees/...` is the tool's own folder, not a repo's.
        let home = dirs_home().map(|h| canon(&h));
        let owner = (Some(canon(&owner)) != home).then_some(owner);
        Some((*tool, owner))
    })
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Mark the tool worktrees in `out` and nest each under its workspace.
fn nest_tool_worktrees(out: &mut [WorkspaceInfo]) {
    let places: Vec<(String, PathBuf, PathBuf, Kind)> = out
        .iter()
        .map(|w| (w.id.clone(), canon(&w.path), canon(&w.checkout), w.kind))
        .collect();
    for w in out.iter_mut() {
        if w.kind != Kind::Worktree {
            continue;
        }
        let Some((tool, owner)) = tool_worktree(&w.path) else {
            continue;
        };
        let owner = owner.map(|o| canon(&o));
        let me = canon(&w.path);
        let parent = owner
            .as_ref()
            .and_then(|o| {
                places
                    .iter()
                    .filter(|(id, p, _, _)| *id != w.id && p == o)
                    .map(|(id, ..)| id.clone())
                    .next()
            })
            .or_else(|| {
                let co = canon(&w.checkout);
                places
                    .iter()
                    .find(|(id, p, c, k)| *id != w.id && *k == Kind::Main && *c == co && *p != me)
                    .map(|(id, ..)| id.clone())
            });
        w.tool = Some(tool.to_string());
        w.nested_in = parent;
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceList {
    pub here: Option<String>,
    /// The key of the space this checkout belongs to (#160): workspaces of
    /// every checkout in it on this machine are listed.
    pub space: Option<String>,
    /// That space with its id and name (#162).
    pub space_info: Option<crate::checkouts::SpaceRef>,
    /// The machine that listed them.
    pub machine: String,
    pub workspaces: Vec<WorkspaceInfo>,
}

fn mtime(p: &Path) -> Option<i64> {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// What an agentvc space knows about a branch: its attempt, task, status.
type AttemptLink = (i64, Option<i64>, String);

fn attempt_link(space: &Space, branch: &str, running: &[i64]) -> Option<AttemptLink> {
    let e = space.last_export_to(branch).ok()??;
    let a = space.attempt(e.attempt_id).ok()?;
    let status = if running.contains(&a.id) {
        "running".to_string()
    } else {
        a.status.as_str().to_string()
    };
    Some((a.id, a.task_id, status))
}

fn count(repo: &HostRepo, range: &str) -> usize {
    repo.git(&["rev-list", "--count", range], None)
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn unmerged_files(dir: &Path) -> usize {
    let repo = HostRepo {
        workdir: dir.to_path_buf(),
    };
    repo.git(&["diff", "--name-only", "--diff-filter=U"], None)
        .map(|o| o.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0)
}

/// Every workspace of the repo containing `cwd`, with its stack. With a
/// `space`, branches exported from attempts carry their attempt, task and
/// status. With `remote`, workspaces published from other machines are
/// fetched and listed too.
pub fn list(cwd: &Path, space: Option<&Space>, remote: bool) -> Result<WorkspaceList> {
    let here = Window::open(cwd)?;
    let own = locals(&here)?;
    let (space_name, sib) = crate::checkouts::space_and_siblings(&here);
    let mut named: Vec<Vec<Local>> = vec![own.clone()];
    for (_, ls) in sibling_locals(&sib) {
        named.push(ls);
    }
    dedupe_names(&mut named);
    let mut groups: Vec<(Vec<Local>, Option<&Space>)> = Vec::new();
    for (i, ls) in named.into_iter().enumerate() {
        groups.push((ls, if i == 0 { space } else { None }));
    }
    let running: Vec<i64> = space
        .and_then(|s| s.runs(true).ok())
        .map(|r| r.into_iter().map(|r| r.attempt_id).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    let mut here_id = None;
    for (all, space) in &groups {
        let space = *space;
        // Branch -> owning workspace id, for parents in other workspaces of
        // the same checkout (branches do not cross clones).
        let mut owner: HashMap<String, String> = HashMap::new();
        for l in all {
            if let Ok(Some(s)) = read_state(&l.git_dir) {
                for b in s.branches {
                    owner.insert(b.name, l.record.id.clone());
                }
            }
        }
        list_group(all, space, &running, &owner, &here, &mut here_id, &mut out)?;
    }
    nest_tool_worktrees(&mut out);
    if remote {
        let mut rest = remote_workspaces(&here, &own)?;
        rest.retain(|r| !out.iter().any(|o| o.id == r.id));
        out.extend(rest);
    }
    let counts = crate::comments::active_counts(cwd);
    if !counts.is_empty() {
        for ws in &mut out {
            for b in &mut ws.branches {
                let key = crate::comments::Target::branch(&ws.id, &b.name).key();
                b.comments = counts.get(&key).copied().unwrap_or(0);
            }
        }
    }
    Ok(WorkspaceList {
        here: here_id,
        space_info: space_name.as_ref().and(crate::checkouts::describe(&here)),
        space: space_name,
        machine: machine(),
        workspaces: out,
    })
}

fn list_group(
    all: &[Local],
    space: Option<&Space>,
    running: &[i64],
    owner: &HashMap<String, String>,
    here: &Window,
    here_id: &mut Option<String>,
    out: &mut Vec<WorkspaceInfo>,
) -> Result<()> {
    for l in all {
        let w = match Window::open(&l.path) {
            Ok(w) => w,
            Err(_) => continue,
        };
        let is_here = l.git_dir == here.git_dir;
        if is_here {
            *here_id = Some(l.record.id.clone());
        }
        let log = w.log(false)?;
        let base = log.base.clone();
        let pending_branch = log.pending.as_ref().map(|p| p.branch.clone()).or_else(|| {
            w.rebase_in_progress()
                .then(|| log.current.clone())
                .flatten()
        });
        let branches = log
            .branches
            .iter()
            .map(|b| {
                let link = space.and_then(|s| attempt_link(s, &b.name, running));
                let behind = match (&b.tip, &base) {
                    (Some(t), Some(base)) => count(&w.repo, &format!("{t}..{base}")),
                    _ => 0,
                };
                let conflicts = if pending_branch.as_deref() == Some(b.name.as_str()) {
                    unmerged_files(&l.path)
                } else {
                    0
                };
                let tracked = w.state.branches.iter().find(|t| t.name == b.name);
                WsBranch {
                    name: b.name.clone(),
                    workspace: l.record.id.clone(),
                    parent: b.parent.clone(),
                    parent_kind: b.parent_kind,
                    parent_workspace: match b.parent_kind {
                        ParentKind::OtherWindow => owner.get(&b.parent).cloned(),
                        ParentKind::Stack => Some(l.record.id.clone()),
                        _ => None,
                    },
                    parent_rev: b.parent_rev.clone(),
                    tip: b.tip.clone(),
                    commits: b.commits,
                    needs_restack: b.needs_restack,
                    behind,
                    conflicts,
                    current: b.current,
                    depth: b.depth,
                    children: b.children.clone(),
                    pr: b.pr.map(|number| PrRef {
                        number,
                        state: tracked.and_then(|t| t.pr_state.clone()),
                        status: tracked.and_then(|t| t.pr_status.clone()),
                    }),
                    attempt: link.as_ref().map(|l| l.0),
                    task: link.as_ref().and_then(|l| l.1),
                    status: link.map(|l| l.2),
                    comments: 0,
                }
            })
            .collect();
        let busy = busy(&l.git_dir);
        out.push(WorkspaceInfo {
            id: l.record.id.clone(),
            name: l.record.name.clone(),
            kind: l.kind,
            machine: machine(),
            folder: tilde(&l.path),
            this_machine: true,
            user: if l.record.user.is_empty() {
                user()
            } else {
                l.record.user.clone()
            },
            path: l.path.clone(),
            here: is_here,
            state: if busy.is_some() {
                Activity::AgentRunning
            } else {
                Activity::Idle
            },
            busy,
            last_seen: mtime(&state_path(&l.git_dir)),
            current: log.current.clone(),
            trunk: log.trunk.clone(),
            base_ref: log.base_ref.clone(),
            base,
            pending: log.pending.clone(),
            branches,
            requests: requests(&l.git_dir),
            checkout: l.checkout.clone(),
            other_checkout: l.other_checkout,
            agent_settings: w.state.agents.clone(),
            tool: None,
            nested_in: None,
        });
    }
    Ok(())
}

// ---- pruning -----------------------------------------------------------------

/// One worktree `prune` looked at.
#[derive(Clone, Debug, Serialize)]
pub struct PruneItem {
    pub workspace: String,
    pub name: String,
    pub path: PathBuf,
    pub folder: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Why it stays, for a kept one; why it failed, for one that could not
    /// be removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What `prune` removed (or would remove) and what it kept.
#[derive(Clone, Debug, Serialize)]
pub struct PruneReport {
    /// It removed them; otherwise this only says what it would remove.
    pub applied: bool,
    pub remove: Vec<PruneItem>,
    pub keep: Vec<PruneItem>,
}

/// Why the worktree of `w` must stay, or `None` when removing it loses
/// nothing: no uncommitted changes, no unmerged branches, no queued edits,
/// no agent at work and no commits that only it has.
fn keep_reason(w: &WorkspaceInfo) -> Option<String> {
    if w.here {
        return Some("this is the workspace you are in".into());
    }
    if w.state == Activity::AgentRunning {
        return Some("an agent is working there".into());
    }
    if w.pending.is_some() || w.requests.iter().any(|r| r.status == RequestStatus::Queued) {
        return Some("it has edits in progress".into());
    }
    if w.branches
        .iter()
        .any(|b| b.pr.as_ref().and_then(|p| p.state.as_deref()) != Some("merged"))
    {
        return Some("it has unmerged branches".into());
    }
    let repo = HostRepo {
        workdir: w.path.clone(),
    };
    match repo.git(&["status", "--porcelain"], None) {
        Ok(o) if o.is_empty() => {}
        Ok(_) => return Some("it has uncommitted changes".into()),
        Err(_) => return Some("git status failed there".into()),
    }
    let own = repo
        .git(&["symbolic-ref", "-q", "HEAD"], None)
        .ok()
        .filter(|r| !r.is_empty());
    // Every other branch and remote branch; `--exclude` patterns differ
    // between git versions, so the list goes on stdin.
    let others: String = repo
        .git(
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads",
                "refs/remotes",
            ],
            None,
        )
        .unwrap_or_default()
        .lines()
        .filter(|r| Some(*r) != own.as_deref())
        .map(|r| format!("^{r}\n"))
        .collect();
    let args = ["rev-list", "--count", "HEAD", "--stdin"];
    match repo
        .git(&args, Some(&others))
        .ok()
        .and_then(|n| n.parse::<usize>().ok())
    {
        Some(0) => None,
        Some(n) => Some(format!(
            "{n} commit{} there {} on no other branch",
            if n == 1 { "" } else { "s" },
            if n == 1 { "is" } else { "are" }
        )),
        None => Some("could not check its commits".into()),
    }
}

/// Remove this space's leftover worktrees on this machine: by default the
/// ones coding tools made for their own agents (`.claude/worktrees/...`),
/// with `all` every linked worktree. Only worktrees that lose nothing go
/// (see [`keep_reason`]); their branches stay. Without `apply` it only
/// says what it would remove. Main worktrees (clone folders) are never
/// touched. Also forgets worktrees whose folders are gone.
pub fn prune(cwd: &Path, all: bool, apply: bool) -> Result<PruneReport> {
    let l = list(cwd, None, false)?;
    let mut remove = Vec::new();
    let mut keep = Vec::new();
    for w in &l.workspaces {
        if w.kind != Kind::Worktree || !w.this_machine || !(all || w.tool.is_some()) {
            continue;
        }
        let item = PruneItem {
            workspace: w.id.clone(),
            name: w.name.clone(),
            path: w.path.clone(),
            folder: w.folder.clone(),
            tool: w.tool.clone(),
            reason: keep_reason(w),
        };
        if item.reason.is_some() {
            keep.push(item);
            continue;
        }
        if apply {
            let main = HostRepo {
                workdir: w.checkout.clone(),
            };
            let path = w.path.to_string_lossy().into_owned();
            if let Err(e) = main.git(&["worktree", "remove", &path], None) {
                keep.push(PruneItem {
                    reason: Some(format!("git could not remove it: {e}")),
                    ..item
                });
                continue;
            }
        }
        remove.push(item);
    }
    if apply {
        let mut checkouts: Vec<&PathBuf> = l
            .workspaces
            .iter()
            .filter(|w| w.this_machine && w.kind != Kind::Remote)
            .map(|w| &w.checkout)
            .collect();
        checkouts.sort();
        checkouts.dedup();
        for c in checkouts {
            let _ = HostRepo { workdir: c.clone() }.git(&["worktree", "prune"], None);
        }
    }
    Ok(PruneReport {
        applied: apply,
        remove,
        keep,
    })
}

// ---- publishing -------------------------------------------------------------

/// What `publish` pushed.
#[derive(Clone, Debug, Serialize)]
pub struct Published {
    pub workspace: String,
    pub r#ref: String,
    pub commit: String,
    pub remote: String,
}

/// Push this workspace's record and stack as `refs/agentvc/workspaces/<id>`
/// on its remote, so `agentvc workspace ls --remote` on other machines lists
/// it. The branches themselves travel with `stack submit`.
pub fn publish(cwd: &Path) -> Result<Published> {
    let w = Window::open(cwd)?;
    if w.state.remote.is_empty() {
        return Err(ws_err("no_remote", "this repo has no remote to publish to"));
    }
    let me = locals(&w)?
        .into_iter()
        .find(|l| l.git_dir == w.git_dir)
        .expect("this workspace is listed");
    let rec = serde_json::to_string_pretty(&me.record).expect("serializes") + "\n";
    let stack = serde_json::to_string_pretty(&w.state).expect("serializes") + "\n";
    let blob = |text: &str| -> Result<String> {
        w.repo.git(&["hash-object", "-w", "--stdin"], Some(text))
    };
    let (rb, sb) = (blob(&rec)?, blob(&stack)?);
    let tree = w.repo.git(
        &["mktree"],
        Some(&format!(
            "100644 blob {rb}\tworkspace.json\n100644 blob {sb}\tstack.json\n"
        )),
    )?;
    let commit = w.repo.git(
        &[
            "-c",
            "user.name=agentvc",
            "-c",
            "user.email=agentvc@localhost",
            "commit-tree",
            &tree,
            "-m",
            &format!("workspace {} ({})", me.record.name, me.record.id),
        ],
        None,
    )?;
    let r = format!("{PUBLISH_PREFIX}{}", me.record.id);
    w.repo
        .git(
            &[
                "push",
                "--quiet",
                "--force",
                &w.state.remote,
                &format!("{commit}:{r}"),
            ],
            None,
        )
        .map_err(|e| crate::github::push_error(e, &w.state.remote, &r))?;
    Ok(Published {
        workspace: me.record.id,
        r#ref: r,
        commit,
        remote: w.state.remote.clone(),
    })
}

/// `agentvc stack get TARGET`: pick up a stack another machine published
/// (`stack submit` / `workspace publish`) in the worktree at `cwd`. TARGET is
/// a published workspace's id or name (its whole stack), or a branch in one
/// (that branch's stack: the branches below and above it).
pub fn get(cwd: &Path, target: &str) -> Result<(String, crate::stack::GetResult)> {
    let mut w = Window::open(cwd)?;
    if w.state.remote.is_empty() {
        return Err(ws_err("no_remote", "this repo has no remote to fetch from"));
    }
    let spec = format!("+{PUBLISH_PREFIX}*:{REMOTE_PREFIX}*");
    w.repo
        .git(
            &["fetch", "--quiet", "--prune", &w.state.remote, &spec],
            None,
        )
        .map_err(|e| {
            ws_err(
                "no_remote",
                format!("fetching published workspaces failed: {e}"),
            )
        })?;
    let refs = w
        .repo
        .git(
            &["for-each-ref", "--format=%(refname)", REMOTE_PREFIX],
            None,
        )
        .unwrap_or_default();
    let mut found: Vec<(Record, State, Vec<String>)> = Vec::new();
    for r in refs.lines().filter(|l| !l.is_empty()) {
        let show = |f: &str| w.repo.git(&["show", &format!("{r}:{f}")], None).ok();
        let (Some(rec), Some(state)) = (
            show("workspace.json").and_then(|t| serde_json::from_str::<Record>(&t).ok()),
            show("stack.json").and_then(|t| serde_json::from_str::<State>(&t).ok()),
        ) else {
            continue;
        };
        let names: Vec<String> = if rec.id == target || rec.name == target {
            state.branches.iter().map(|b| b.name.clone()).collect()
        } else if state.branches.iter().any(|b| b.name == target) {
            // The branch's stack: down to the trunk, and everything above.
            let parent = |n: &str| {
                state
                    .branches
                    .iter()
                    .find(|b| b.name == n)
                    .map(|b| b.parent.clone())
            };
            let mut root = target.to_string();
            while let Some(p) =
                parent(&root).filter(|p| state.branches.iter().any(|b| &b.name == p))
            {
                root = p;
            }
            let mut set = vec![root];
            let mut i = 0;
            while i < set.len() {
                let kids: Vec<String> = state
                    .branches
                    .iter()
                    .filter(|b| b.parent == set[i] && !set.contains(&b.name))
                    .map(|b| b.name.clone())
                    .collect();
                set.extend(kids);
                i += 1;
            }
            set
        } else {
            continue;
        };
        found.push((rec, state, names));
    }
    // This machine's own workspaces publish too: skip them unless nothing
    // else matches (then `give` is the tool).
    let mine: Vec<String> = locals(&w)?.into_iter().map(|l| l.record.id).collect();
    let (remote_found, local_found): (Vec<_>, Vec<_>) = found
        .into_iter()
        .partition(|(r, _, _)| !mine.contains(&r.id));
    let mut found = remote_found;
    if found.is_empty() && !local_found.is_empty() {
        return Err(ws_err_with(
            "same_machine",
            format!("{target} is in a workspace on this machine; hand it over with `agentvc stack give`"),
            json!({ "target": target }),
        ));
    }
    match found.len() {
        0 => Err(ws_err_with(
            "no_workspace",
            format!("no published workspace or branch {target}; publish it with `agentvc stack submit` or `agentvc workspace publish` on that machine"),
            json!({ "target": target }),
        )),
        1 => {
            let (rec, state, names) = found.remove(0);
            let r = w.get_published(&state, &names)?;
            Ok((rec.id, r))
        }
        _ => Err(ws_err_with(
            "ambiguous",
            format!("{target} matches several published workspaces: {}; give a workspace id", found.iter().map(|f| f.0.id.clone()).collect::<Vec<_>>().join(", ")),
            json!({ "workspaces": found.iter().map(|f| f.0.id.clone()).collect::<Vec<_>>() }),
        )),
    }
}

/// Fetch the published workspaces and list the ones not on this machine's
/// worktrees.
fn remote_workspaces(w: &Window, locals: &[Local]) -> Result<Vec<WorkspaceInfo>> {
    if w.state.remote.is_empty() {
        return Ok(Vec::new());
    }
    let spec = format!("+{PUBLISH_PREFIX}*:{REMOTE_PREFIX}*");
    // Best effort: no published workspaces (or no network) lists none.
    let _ = w.repo.git(
        &["fetch", "--quiet", "--prune", &w.state.remote, &spec],
        None,
    );
    let refs = w
        .repo
        .git(
            &["for-each-ref", "--format=%(refname)", REMOTE_PREFIX],
            None,
        )
        .unwrap_or_default();
    let mut out = Vec::new();
    for r in refs.lines().filter(|l| !l.is_empty()) {
        let show = |f: &str| w.repo.git(&["show", &format!("{r}:{f}")], None).ok();
        let Some(rec) =
            show("workspace.json").and_then(|t| serde_json::from_str::<Record>(&t).ok())
        else {
            continue;
        };
        if locals.iter().any(|l| l.record.id == rec.id) {
            continue;
        }
        let state: Option<State> = show("stack.json").and_then(|t| serde_json::from_str(&t).ok());
        let when = w
            .repo
            .git(&["log", "-1", "--format=%ct", r], None)
            .ok()
            .and_then(|t| t.parse().ok());
        let base_ref = state
            .as_ref()
            .map(|s| format!("{}/{}", s.remote, s.trunk))
            .unwrap_or_default();
        let base = w.repo.rev(&format!("refs/remotes/{base_ref}"));
        let branches = state
            .as_ref()
            .map(|s| remote_branches(w, &rec.id, s, base.as_deref()))
            .unwrap_or_default();
        out.push(WorkspaceInfo {
            id: rec.id.clone(),
            name: rec.name.clone(),
            kind: Kind::Remote,
            machine: rec.machine.clone(),
            folder: rec.path.to_string_lossy().into_owned(),
            this_machine: false,
            user: rec.user.clone(),
            path: rec.path.clone(),
            here: false,
            state: Activity::Idle,
            busy: None,
            last_seen: when,
            current: None,
            trunk: state.as_ref().map(|s| s.trunk.clone()).unwrap_or_default(),
            base_ref,
            base,
            pending: None,
            branches,
            requests: Vec::new(),
            checkout: rec.path.clone(),
            other_checkout: false,
            agent_settings: state.map(|s| s.agents).unwrap_or_default(),
            tool: None,
            nested_in: None,
        });
    }
    Ok(out)
}

/// A published stack's branches, with tips from the remote's branches.
fn remote_branches(w: &Window, id: &str, s: &State, base: Option<&str>) -> Vec<WsBranch> {
    let names: Vec<&str> = s.branches.iter().map(|b| b.name.as_str()).collect();
    let mut children: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for b in &s.branches {
        children
            .entry(b.parent.as_str())
            .or_default()
            .push(b.name.clone());
    }
    let mut depth: HashMap<&str, usize> = HashMap::new();
    for b in &s.branches {
        let d = depth.get(b.parent.as_str()).map_or(0, |d| d + 1);
        depth.insert(b.name.as_str(), d);
    }
    s.branches
        .iter()
        .map(|b| {
            let tip = w.repo.rev(&format!("refs/remotes/{}/{}", s.remote, b.name));
            WsBranch {
                name: b.name.clone(),
                workspace: id.to_string(),
                parent: b.parent.clone(),
                parent_kind: if b.parent == s.trunk {
                    ParentKind::Trunk
                } else if names.contains(&b.parent.as_str()) {
                    ParentKind::Stack
                } else {
                    ParentKind::Branch
                },
                parent_workspace: names.contains(&b.parent.as_str()).then(|| id.to_string()),
                parent_rev: b.parent_rev.clone(),
                commits: tip
                    .as_ref()
                    .map_or(0, |t| count(&w.repo, &format!("{}..{t}", b.parent_rev))),
                behind: match (&tip, base) {
                    (Some(t), Some(base)) => count(&w.repo, &format!("{t}..{base}")),
                    _ => 0,
                },
                tip,
                needs_restack: false,
                conflicts: 0,
                current: false,
                depth: depth.get(b.name.as_str()).copied().unwrap_or(0),
                children: children.get(b.name.as_str()).cloned().unwrap_or_default(),
                pr: b.pr.map(|number| PrRef {
                    number,
                    state: b.pr_state.clone(),
                    status: b.pr_status.clone(),
                }),
                attempt: None,
                task: None,
                status: None,
                comments: 0,
            }
        })
        .collect()
}

// ---- change detection (for agentvcd) ---------------------------------------

/// A cheap summary of every workspace, to tell what changed between two
/// polls without running `list`: per workspace, a fingerprint of its stack
/// file, HEAD, activity and the tips of its branches; and its requests'
/// statuses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fingerprints {
    pub workspaces: BTreeMap<String, String>,
    /// request id -> (workspace id, status, reason)
    pub requests: BTreeMap<String, (String, RequestStatus, Option<String>)>,
}

/// Fingerprint the workspaces of the repo containing `cwd`: one
/// `for-each-ref` plus a few small file reads per workspace.
pub fn fingerprints(cwd: &Path) -> Result<Fingerprints> {
    let w = Window::open(cwd)?;
    let mut out = Fingerprints::default();
    let own = locals(&w)?;
    fingerprint_checkout(&w, own, &mut out);
    // The other checkouts of the space (#160), from the registry alone:
    // this runs every poll.
    let sib = crate::checkouts::registered_siblings(&w);
    for (sw, ls) in sibling_locals(&sib) {
        fingerprint_checkout(&sw, ls, &mut out);
    }
    Ok(out)
}

fn fingerprint_checkout(w: &Window, all: Vec<Local>, out: &mut Fingerprints) {
    let refs = w
        .repo
        .git(
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
                "refs/remotes",
            ],
            None,
        )
        .unwrap_or_default();
    let tips: HashMap<&str, &str> = refs.lines().filter_map(|l| l.split_once(' ')).collect();
    for l in all {
        let mut h = Sha1::new();
        let file = |p: PathBuf| fs::read(p).unwrap_or_default();
        h.update(file(state_path(&l.git_dir)));
        h.update(file(l.git_dir.join("HEAD")));
        h.update(file(agentvc_dir(&l.git_dir).join("workspace.json")));
        h.update(if busy(&l.git_dir).is_some() {
            b"1"
        } else {
            b"0"
        });
        h.update(l.git_dir.join("rebase-merge").exists().to_string());
        if let Ok(Some(s)) = read_state(&l.git_dir) {
            let base = format!("refs/remotes/{}/{}", s.remote, s.trunk);
            h.update(tips.get(base.as_str()).copied().unwrap_or(""));
            for b in &s.branches {
                let r = format!("refs/heads/{}", b.name);
                h.update(tips.get(r.as_str()).copied().unwrap_or(""));
            }
        }
        out.workspaces
            .insert(l.record.id.clone(), hex::encode(h.finalize()));
        for r in requests(&l.git_dir) {
            out.requests
                .insert(r.id.clone(), (l.record.id.clone(), r.status, r.reason));
        }
    }
}

// ---- one branch of a workspace ---------------------------------------------

/// A commit of a stacked branch.
#[derive(Clone, Debug, Serialize)]
pub struct BranchCommit {
    pub commit: String,
    pub subject: String,
    pub author: String,
    pub time: i64,
}

/// A file a stacked branch changes.
#[derive(Clone, Debug, Serialize)]
pub struct BranchFile {
    pub path: String,
    pub kind: crate::snapshot::ChangeKind,
}

/// What one branch of a workspace holds: its own commits and files, for the
/// review UI's branch page.
#[derive(Clone, Debug, Serialize)]
pub struct BranchDetail {
    pub workspace: String,
    pub name: String,
    pub parent: String,
    /// The commit it sits on and its tip; diff `base..tip` for its changes.
    pub base: String,
    pub tip: String,
    pub current: bool,
    pub commits: Vec<BranchCommit>,
    pub files: Vec<BranchFile>,
}

/// Branch `name` of workspace `target`.
pub fn branch_detail(cwd: &Path, target: &str, name: &str) -> Result<BranchDetail> {
    let here = Window::open(cwd)?;
    let ws = find_local(&here, target)?;
    let w = Window::open(&ws.path)?;
    let t = w
        .state
        .branches
        .iter()
        .find(|b| b.name == name)
        .cloned()
        .ok_or_else(|| {
            ws_err_with(
                "not_in_stack",
                format!("branch {name} is not in this workspace's stack"),
                json!({ "branch": name }),
            )
        })?;
    let tip = w.branch_tip(name).ok_or_else(|| {
        ws_err_with(
            "no_branch",
            format!("branch {name} does not exist"),
            json!({ "branch": name }),
        )
    })?;
    let range = format!("{}..{tip}", t.parent_rev);
    let log = w
        .repo
        .git(&["log", "--format=%H%x1f%s%x1f%an%x1f%ct", &range], None)?;
    let commits = log
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\x1f').collect();
            (f.len() == 4).then(|| BranchCommit {
                commit: f[0].into(),
                subject: f[1].into(),
                author: f[2].into(),
                time: f[3].parse().unwrap_or(0),
            })
        })
        .collect();
    let names = w.repo.git(
        &["diff", "--name-status", "--no-renames", &t.parent_rev, &tip],
        None,
    )?;
    let files = names
        .lines()
        .filter_map(|l| {
            let (k, path) = l.split_once('\t')?;
            use crate::snapshot::ChangeKind;
            let kind = match k.chars().next()? {
                'A' => ChangeKind::Added,
                'D' => ChangeKind::Removed,
                _ => ChangeKind::Modified,
            };
            Some(BranchFile {
                path: path.into(),
                kind,
            })
        })
        .collect();
    Ok(BranchDetail {
        workspace: ws.record.id,
        name: name.to_string(),
        parent: t.parent,
        base: t.parent_rev,
        current: w.current_branch().as_deref() == Some(name),
        tip,
        commits,
        files,
    })
}

// ---- conflicts of a restack left in a workspace ----------------------------
//
// `edit_with(.., keep_conflicts: true)` (or `stack restack --keep-conflicts`)
// stops a conflicting restack in that workspace's worktree, as `git rebase`
// does. These let any window, or the review UI, finish it: show the
// conflicts, resolve them hunk by hunk, then continue or abort.
//
// Hunks are shown from the branch's side: `ours` is the branch being
// restacked ("mine"), `theirs` its new parent. In the file git writes them
// the other way round (`HEAD` is the parent), which `resolve` translates.

/// One conflicted file of a workspace.
#[derive(Clone, Debug, Serialize)]
pub struct WsConflictFile {
    pub path: String,
    /// Empty when git left no markers (deleted on one side, binary).
    pub hunks: Vec<crate::conflicts::Hunk>,
    pub text: String,
}

/// Where a workspace's stopped restack stands.
#[derive(Clone, Debug, Serialize)]
pub struct WsConflicts {
    pub workspace: String,
    /// Whether a rebase is stopped in the worktree.
    pub stopped: bool,
    /// The branch being restacked and the branch it goes onto.
    pub branch: Option<String>,
    pub parent: Option<String>,
    pub onto: Option<String>,
    /// The commit being replayed when it stopped.
    pub subject: Option<String>,
    /// Files with conflicts still unresolved.
    pub files: Vec<WsConflictFile>,
}

/// Outcome of [`resolve`].
#[derive(Clone, Debug, Serialize)]
pub struct WsResolve {
    pub path: String,
    pub remaining_hunks: usize,
    /// Files still unresolved; when empty, [`finish`] can continue.
    pub open: Vec<String>,
}

/// Outcome of [`finish`].
#[derive(Clone, Debug, Serialize)]
pub struct WsFinish {
    /// `continued` (all done), `stopped` (the next commit or branch
    /// conflicts too; see `conflicts`) or `aborted`.
    pub outcome: &'static str,
    pub conflicts: Option<WsConflicts>,
    /// The restack of the branches above, once the stopped one finished.
    pub restack: Option<Value>,
}

fn take_from_branch_side(t: &crate::conflicts::Take) -> crate::conflicts::Take {
    use crate::conflicts::Take;
    match t {
        Take::Ours => Take::Theirs,
        Take::Theirs => Take::Ours,
        Take::Both => Take::BothTheirsFirst,
        Take::BothTheirsFirst => Take::Both,
        Take::Base => Take::Base,
        Take::Custom => Take::Custom,
    }
}

fn unmerged(w: &Window) -> Result<Vec<String>> {
    let out = w
        .repo
        .git(&["diff", "--name-only", "--diff-filter=U"], None)?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn read_conflicts(w: &Window, id: &str) -> Result<WsConflicts> {
    let stopped = w.rebase_in_progress();
    let pending = w.state.pending.clone();
    let branch = pending.as_ref().map(|p| p.branch.clone());
    let parent = branch.as_ref().and_then(|b| {
        w.state
            .branches
            .iter()
            .find(|t| &t.name == b)
            .map(|t| t.parent.clone())
    });
    let subject = ["rebase-merge", "rebase-apply"]
        .iter()
        .find_map(|d| fs::read_to_string(w.git_dir.join(d).join("message")).ok())
        .and_then(|m| m.lines().next().map(str::to_string));
    let mut files = Vec::new();
    if stopped {
        for path in unmerged(w)? {
            let full = w.path().join(&path);
            let text = fs::read_to_string(&full).unwrap_or_default();
            let hunks = crate::conflicts::parse_hunks(&text)
                .unwrap_or_default()
                .into_iter()
                .map(|h| crate::conflicts::Hunk {
                    ours: h.theirs,
                    theirs: h.ours,
                    ours_label: branch.clone().unwrap_or(h.theirs_label),
                    theirs_label: parent.clone().unwrap_or(h.ours_label),
                    ..h
                })
                .collect();
            files.push(WsConflictFile { path, hunks, text });
        }
    }
    Ok(WsConflicts {
        workspace: id.to_string(),
        stopped,
        branch,
        parent,
        onto: pending.map(|p| p.onto),
        subject,
        files,
    })
}

/// The conflicts of the restack stopped in workspace `target`.
pub fn conflicts(cwd: &Path, target: &str) -> Result<WsConflicts> {
    let here = Window::open(cwd)?;
    let ws = find_local(&here, target)?;
    let w = Window::open(&ws.path)?;
    read_conflicts(&w, &ws.record.id)
}

/// Resolve hunks of one conflicted file of workspace `target` (choices from
/// the branch's side; see above). A file left with no markers is staged.
pub fn resolve(
    cwd: &Path,
    target: &str,
    path: &str,
    choices: &[crate::conflicts::HunkChoice],
) -> Result<WsResolve> {
    let here = Window::open(cwd)?;
    let ws = find_local(&here, target)?;
    let _lock = lock(&ws.git_dir)?;
    let w = Window::open(&ws.path)?;
    if !w.rebase_in_progress() {
        return Err(ws_err(
            "no_conflicts",
            "no restack is stopped in this workspace",
        ));
    }
    if !unmerged(&w)?.iter().any(|p| p == path) {
        return Err(ws_err_with(
            "not_conflicted",
            format!("{path} has no unresolved conflict here"),
            json!({ "path": path }),
        ));
    }
    let full = w.path().join(path);
    let text = fs::read_to_string(&full).at(&full)?;
    let choices: Vec<_> = choices
        .iter()
        .map(|c| crate::conflicts::HunkChoice {
            take: take_from_branch_side(&c.take),
            ..c.clone()
        })
        .collect();
    let (out, _) = crate::conflicts::apply_choices(&text, &choices)?;
    fs::write(&full, &out).at(&full)?;
    let remaining = crate::conflicts::parse_hunks(&out)
        .map(|h| h.len())
        .unwrap_or(0);
    if remaining == 0 {
        w.repo.git(&["add", "--", path], None)?;
    }
    Ok(WsResolve {
        path: path.to_string(),
        remaining_hunks: remaining,
        open: unmerged(&w)?,
    })
}

/// Continue (or with `abort`, give up) the restack stopped in workspace
/// `target`. Continuing commits the resolved files, finishes that branch
/// and restacks the branches above it, stopping again on the next conflict.
pub fn finish(cwd: &Path, target: &str, abort: bool) -> Result<WsFinish> {
    let here = Window::open(cwd)?;
    let ws = find_local(&here, target)?;
    let _lock = lock(&ws.git_dir)?;
    let mut w = Window::open(&ws.path)?;
    let id = ws.record.id.clone();
    if !w.rebase_in_progress() && w.state.pending.is_none() {
        return Err(ws_err(
            "no_conflicts",
            "no restack is stopped in this workspace",
        ));
    }
    if abort {
        if w.rebase_in_progress() {
            w.repo.git(&["rebase", "--abort"], None)?;
        }
        // Clears the stop, and puts a moved branch back on its old parent.
        w.settle()?;
        return Ok(WsFinish {
            outcome: "aborted",
            conflicts: None,
            restack: None,
        });
    }
    let branch = w.state.pending.as_ref().map(|p| p.branch.clone());
    if w.rebase_in_progress() {
        let open = unmerged(&w)?;
        if !open.is_empty() {
            return Err(ws_err_with(
                "unresolved_conflicts",
                format!("resolve {} first", open.join(", ")),
                json!({ "paths": open }),
            ));
        }
        let r = w.repo.git_env(
            &["-c", "core.editor=true", "rebase", "--continue"],
            None,
            &[("GIT_EDITOR", "true")],
        );
        if w.rebase_in_progress() {
            // The next commit of the branch conflicts too.
            return Ok(WsFinish {
                outcome: "stopped",
                conflicts: Some(read_conflicts(&w, &id)?),
                restack: None,
            });
        }
        r?;
    }
    w.settle()?;
    // Branches above the finished one, onto its new tip.
    let restack = match branch.filter(|b| w.tracks(b)) {
        Some(b) => {
            let up = w.upstack(&b);
            let r = w.restack(Some(&up), true)?;
            let stopped = r.conflict.is_some();
            let v = json!(r);
            if stopped {
                return Ok(WsFinish {
                    outcome: "stopped",
                    conflicts: Some(read_conflicts(&Window::open(&ws.path)?, &id)?),
                    restack: Some(v),
                });
            }
            Some(v)
        }
        None => None,
    };
    let _ = drain_locked(&ws.path);
    Ok(WsFinish {
        outcome: "continued",
        conflicts: None,
        restack,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
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

    #[test]
    fn clashing_names_settle_by_age_to_the_nanosecond() {
        let tmp = crate::testutil::tempdir();
        let mk = |dir: &str, id: &str, name: &str, ns: u32| {
            let git_dir = tmp.path().join(dir);
            fs::create_dir_all(&git_dir).unwrap();
            Local {
                record: Record {
                    version: RECORD_VERSION,
                    id: id.into(),
                    name: name.into(),
                    created_at: 1_000,
                    created_ns: ns,
                    machine: "m".into(),
                    path: tmp.path().join(dir),
                    user: String::new(),
                },
                git_dir,
                path: tmp.path().join(dir),
                kind: Kind::Main,
                checkout: tmp.path().join(dir),
                other_checkout: false,
            }
        };
        // Made in the same second: the older keeps `mono` although its id
        // sorts last, and the newer `mono` does not take `mono-2`, which an
        // even newer workspace already has (that would rename it too, #180).
        let mut groups = vec![
            vec![mk("a", "wffffffff", "mono", 10)],
            vec![mk("b", "w00000000", "mono", 20)],
            vec![mk("c", "w11111111", "mono-2", 30)],
        ];
        dedupe_names(&mut groups);
        let names: Vec<&str> = groups
            .iter()
            .flatten()
            .map(|l| l.record.name.as_str())
            .collect();
        assert_eq!(names, ["mono", "mono-3", "mono-2"]);
        let saved: Record = read_json(&tmp.path().join("b/agentvc/workspace.json")).unwrap();
        assert_eq!(saved.name, "mono-3");
        assert!(!tmp.path().join("a/agentvc/workspace.json").exists());
    }

    struct Fx {
        _tmp: crate::testutil::TestDir,
        w1: PathBuf,
        w2: PathBuf,
    }

    /// origin (bare) <- w1 (clone) + w2 (linked worktree of w1).
    fn setup() -> Fx {
        let tmp = crate::testutil::tempdir();
        let root = tmp.path().to_path_buf();
        let origin = root.join("origin.git");
        fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "--bare", "-q", "-b", "main"]);
        let w1 = root.join("w1");
        git(&root, &["clone", "-q", origin.to_str().unwrap(), "w1"]);
        git(&w1, &["checkout", "-q", "-b", "main"]);
        // Stack commands commit with the repo's identity (CI has none).
        git(&w1, &["config", "user.name", "t"]);
        git(&w1, &["config", "user.email", "t@t"]);
        fs::write(w1.join("a.txt"), "a\n").unwrap();
        git(&w1, &["add", "."]);
        git(&w1, &["commit", "-q", "-m", "init"]);
        git(&w1, &["push", "-q", "origin", "main"]);
        git(&w1, &["fetch", "-q", "origin"]);
        let w2 = root.join("w2");
        git(
            &w1,
            &["worktree", "add", "-q", "--detach", w2.to_str().unwrap()],
        );
        Fx { _tmp: tmp, w1, w2 }
    }

    fn stack_with(dir: &Path, names: &[&str]) {
        let mut w = Window::open(dir).unwrap();
        for n in names {
            fs::write(dir.join(format!("{n}.txt")), format!("{n}\n")).unwrap();
            w.create(Some(n), Some(n), true).unwrap();
        }
    }

    /// w2 stacks b1 (edits a.txt) and b2; main then edits a.txt too.
    fn conflicting_stack() -> (Fx, String) {
        let f = setup();
        fs::write(f.w2.join("a.txt"), "mine\n").unwrap();
        let mut w = Window::open(&f.w2).unwrap();
        w.create(Some("b1"), Some("b1: mine"), true).unwrap();
        fs::write(f.w2.join("b2.txt"), "b2\n").unwrap();
        w.create(Some("b2"), Some("b2"), true).unwrap();
        fs::write(f.w1.join("a.txt"), "theirs\n").unwrap();
        git(&f.w1, &["commit", "-qam", "main edits a"]);
        git(&f.w1, &["push", "-q", "origin", "main"]);
        git(&f.w1, &["fetch", "-q", "origin"]);
        let id = list(&f.w1, None, false).unwrap().workspaces[1].id.clone();
        (f, id)
    }

    #[test]
    fn a_stopped_restack_is_resolved_and_continued_from_another_window() {
        let (f, id) = conflicting_stack();
        let restack = || Op::Restack { branch: None };
        // Without keep_conflicts: refused, nothing changed.
        let e = edit(&f.w1, &id, restack(), None, false).unwrap_err();
        assert_eq!(e.code(), "conflict");
        assert!(!conflicts(&f.w1, &id).unwrap().stopped);

        let o = edit_with(&f.w1, &id, restack(), None, false, true).unwrap();
        assert_eq!(o.outcome, RequestStatus::Applied);
        assert_eq!(o.result.as_ref().unwrap()["conflict"]["branch"], "b1");
        let c = conflicts(&f.w1, &id).unwrap();
        assert!(c.stopped);
        assert_eq!(
            (c.branch.as_deref(), c.parent.as_deref()),
            (Some("b1"), Some("main"))
        );
        assert_eq!(c.subject.as_deref(), Some("b1: mine"));
        assert_eq!(c.files.len(), 1);
        let h = &c.files[0].hunks[0];
        // Shown from the branch's side.
        assert_eq!((h.ours.as_str(), h.theirs.as_str()), ("mine\n", "theirs\n"));
        assert_eq!(
            (h.ours_label.as_str(), h.theirs_label.as_str()),
            ("b1", "main")
        );

        let e = finish(&f.w1, &id, false).unwrap_err();
        assert_eq!(e.code(), "unresolved_conflicts");
        let choice = crate::conflicts::HunkChoice {
            index: 1,
            take: crate::conflicts::Take::Both,
            text: None,
        };
        let r = resolve(&f.w1, &id, "a.txt", &[choice]).unwrap();
        assert_eq!(r.remaining_hunks, 0);
        assert!(r.open.is_empty());
        let done = finish(&f.w1, &id, false).unwrap();
        assert_eq!(done.outcome, "continued");
        // Mine first, then main's; b2 followed onto it.
        assert_eq!(git(&f.w2, &["show", "b1:a.txt"]), "mine\ntheirs");
        assert_eq!(git(&f.w2, &["show", "b2:a.txt"]), "mine\ntheirs");
        let w = Window::open(&f.w2).unwrap();
        assert!(w.state.pending.is_none() && !w.rebase_in_progress());
        let l = list(&f.w1, None, false).unwrap();
        assert!(l.workspaces[1].branches.iter().all(|b| !b.needs_restack));

        let d = branch_detail(&f.w1, &id, "b1").unwrap();
        assert_eq!(d.parent, "main");
        assert_eq!(d.commits.len(), 1);
        assert_eq!(d.commits[0].subject, "b1: mine");
        assert_eq!(d.files.len(), 1);
        assert_eq!(d.files[0].path, "a.txt");
        assert_eq!(
            branch_detail(&f.w1, &id, "nope").unwrap_err().code(),
            "not_in_stack"
        );
        // Check out another branch of that window from here.
        edit(
            &f.w1,
            &id,
            Op::Checkout {
                branch: "b2".into(),
            },
            None,
            false,
        )
        .unwrap();
        assert_eq!(git(&f.w2, &["branch", "--show-current"]), "b2");
    }

    #[test]
    fn an_empty_workspace_starts_its_first_branch_from_another_window() {
        let f = setup();
        // w2 has no stack yet: a detached checkout of main with an edit in it.
        fs::write(f.w2.join("a.txt"), "edited\n").unwrap();
        let id = list(&f.w1, None, false).unwrap().workspaces[1].id.clone();
        let create = |branch: &str, parent: Option<&str>| Op::Create {
            branch: branch.into(),
            parent: parent.map(String::from),
        };
        let o = edit(&f.w1, &id, create("first", Some("main")), None, false).unwrap();
        assert_eq!(o.outcome, RequestStatus::Applied);
        assert_eq!(git(&f.w2, &["branch", "--show-current"]), "first");
        // The edit came along, uncommitted.
        assert_eq!(fs::read_to_string(f.w2.join("a.txt")).unwrap(), "edited\n");
        let l = list(&f.w1, None, false).unwrap();
        let b = &l.workspaces[1].branches;
        assert_eq!(
            (b.len(), b[0].name.as_str(), b[0].parent.as_str()),
            (1, "first", "main")
        );

        // From the trunk again while on a branch: a sibling, not a child.
        git(&f.w2, &["commit", "-qam", "first"]);
        edit(&f.w1, &id, create("second", Some("main")), None, false).unwrap();
        let w = Window::open(&f.w2).unwrap();
        let p = |n: &str| {
            w.state
                .branches
                .iter()
                .find(|t| t.name == n)
                .unwrap()
                .parent
                .clone()
        };
        assert_eq!(p("second"), "main");
        // On top of a branch.
        edit(&f.w1, &id, create("third", Some("first")), None, false).unwrap();
        let w = Window::open(&f.w2).unwrap();
        assert_eq!(
            w.state
                .branches
                .iter()
                .find(|t| t.name == "third")
                .unwrap()
                .parent,
            "first"
        );
        let e = edit(&f.w1, &id, create("third", None), None, false).unwrap_err();
        assert_eq!(e.code(), "branch_exists");
    }

    #[test]
    fn a_stopped_restack_can_be_aborted() {
        let (f, id) = conflicting_stack();
        let before = git(&f.w2, &["rev-parse", "b1"]);
        edit_with(&f.w1, &id, Op::Restack { branch: None }, None, false, true).unwrap();
        assert!(conflicts(&f.w1, &id).unwrap().stopped);
        assert_eq!(finish(&f.w1, &id, true).unwrap().outcome, "aborted");
        assert!(!conflicts(&f.w1, &id).unwrap().stopped);
        assert_eq!(git(&f.w2, &["rev-parse", "b1"]), before);
        assert_eq!(finish(&f.w1, &id, true).unwrap_err().code(), "no_conflicts");
    }

    #[test]
    fn an_aborted_move_puts_the_branch_back_on_its_parent() {
        let (f, id) = conflicting_stack();
        // c sits on b2 and changes the line main changed too.
        let mut w = Window::open(&f.w2).unwrap();
        fs::write(f.w2.join("a.txt"), "c\n").unwrap();
        w.create(Some("c"), Some("c"), true).unwrap();
        let trunk = list(&f.w1, None, false).unwrap().workspaces[1]
            .trunk
            .clone();
        let before = git(&f.w2, &["rev-parse", "c"]);
        let mv = Op::Move {
            branch: "c".into(),
            onto: trunk,
        };
        let o = edit_with(&f.w1, &id, mv, None, false, true).unwrap();
        assert_eq!(o.result.as_ref().unwrap()["conflict"]["branch"], "c");
        assert_eq!(finish(&f.w1, &id, true).unwrap().outcome, "aborted");
        let w = Window::open(&f.w2).unwrap();
        let c = w.state.branches.iter().find(|b| b.name == "c").unwrap();
        assert_eq!(c.parent, "b2");
        assert!(w.state.pending.is_none());
        assert_eq!(git(&f.w2, &["rev-parse", "c"]), before);
    }

    #[test]
    fn lists_every_workspace_with_its_stack() {
        let f = setup();
        stack_with(&f.w1, &["a1", "a2"]);
        stack_with(&f.w2, &["b1"]);
        let l = list(&f.w2, None, false).unwrap();
        assert_eq!(l.workspaces.len(), 2);
        let (one, two) = (&l.workspaces[0], &l.workspaces[1]);
        assert_eq!(one.kind, Kind::Main);
        assert_eq!(one.name, "w1");
        assert_eq!(two.name, "w2");
        assert!(two.here && !one.here);
        assert_eq!(l.here.as_deref(), Some(two.id.as_str()));
        let names: Vec<_> = one.branches.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["a1", "a2"]);
        assert_eq!(
            one.branches[1].parent_workspace.as_deref(),
            Some(one.id.as_str())
        );
        assert_eq!(two.branches[0].workspace, two.id);
        // Ids are stable across listings and from either workspace.
        let again = list(&f.w1, None, false).unwrap();
        assert_eq!(again.workspaces[1].id, two.id);
        // Renaming; names stay unique.
        set_name(&f.w2, "review").unwrap();
        assert!(matches!(
            set_name(&f.w1, "review"),
            Err(Error::Stack {
                code: "name_taken",
                ..
            })
        ));
        let l = list(&f.w1, None, false).unwrap();
        assert_eq!(l.workspaces[1].name, "review");
        assert_eq!(
            find_local(&Window::open(&f.w1).unwrap(), "review")
                .unwrap()
                .record
                .id,
            two.id
        );
    }

    #[test]
    fn edits_apply_now_when_idle_and_queue_while_an_agent_works() {
        let f = setup();
        stack_with(&f.w2, &["b1", "b2", "b3"]);
        git(&f.w2, &["checkout", "-q", "b1"]);
        // From w1: move b3 onto trunk in w2. w2 is idle: applied at once.
        let o = edit(
            &f.w1,
            "w2",
            Op::Move {
                branch: "b3".into(),
                onto: "main".into(),
            },
            Some("test"),
            false,
        )
        .unwrap();
        assert_eq!(o.outcome, RequestStatus::Applied);
        let w = Window::open(&f.w2).unwrap();
        assert_eq!(
            w.state
                .branches
                .iter()
                .find(|b| b.name == "b3")
                .unwrap()
                .parent,
            "main"
        );
        let reqs = requests(&w.git_dir);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].status, RequestStatus::Applied);

        // An agent starts in w2: edits wait.
        set_activity(&f.w2, Activity::AgentRunning, Some("agent:claude")).unwrap();
        let l = list(&f.w1, None, false).unwrap();
        assert_eq!(l.workspaces[1].state, Activity::AgentRunning);
        let q = edit(
            &f.w1,
            "w2",
            Op::Rename {
                branch: "b2".into(),
                new_name: "b2-renamed".into(),
            },
            None,
            false,
        )
        .unwrap();
        assert_eq!(q.outcome, RequestStatus::Queued);
        let q2 = edit(
            &f.w1,
            "w2",
            Op::Delete {
                branch: "b3".into(),
            },
            None,
            false,
        )
        .unwrap();
        // Cancel one; the other is applied when the agent stops.
        let c = cancel(&f.w1, &q2.request_id).unwrap();
        assert_eq!(c.status, RequestStatus::Cancelled);
        assert!(matches!(
            cancel(&f.w1, &q2.request_id),
            Err(Error::Stack {
                code: "request_not_queued",
                ..
            })
        ));
        assert!(Window::open(&f.w2).unwrap().tracks("b2"));
        let done = set_activity(&f.w2, Activity::Idle, None).unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].status, RequestStatus::Applied);
        let w = Window::open(&f.w2).unwrap();
        assert!(w.tracks("b2-renamed") && w.tracks("b3"));

        // A queued edit that cannot apply is recorded as failed.
        set_activity(&f.w2, Activity::AgentRunning, None).unwrap();
        edit(
            &f.w1,
            "w2",
            Op::Delete {
                branch: "nope".into(),
            },
            None,
            false,
        )
        .unwrap();
        let done = drain(&f.w2).unwrap();
        assert_eq!(done[0].status, RequestStatus::Failed);
        assert_eq!(done[0].code.as_deref(), Some("not_in_stack"));

        // Applying at once surfaces the error and records it.
        set_activity(&f.w2, Activity::Idle, None).unwrap();
        assert!(edit(
            &f.w1,
            "w2",
            Op::Delete {
                branch: "nope".into()
            },
            None,
            false
        )
        .is_err());
        assert!(matches!(
            edit(&f.w1, "w9", Op::Sync, None, false),
            Err(Error::Stack {
                code: "no_workspace",
                ..
            })
        ));
    }

    #[test]
    fn submit_is_an_op_the_ui_can_send() {
        let op: Op = serde_json::from_str(r#"{"op":"submit"}"#).unwrap();
        assert_eq!(op, Op::Submit);
        assert_eq!(op.label(), "submit --all");
        assert_eq!(op.branch(), None);
    }

    #[test]
    fn agent_hooks_mark_the_workspace_busy_until_stop() {
        let f = setup();
        stack_with(&f.w2, &["b1"]);
        let gd = Window::open(&f.w2).unwrap().git_dir;
        let input = json!({ "cwd": f.w2 });
        crate::hooks::workspace_activity("user-prompt-submit", &input, &f.w1, "agent:x").unwrap();
        assert_eq!(busy(&gd).unwrap().by.as_deref(), Some("agent:x"));
        let q = edit(&f.w1, "w2", Op::Restack { branch: None }, None, false).unwrap();
        assert_eq!(q.outcome, RequestStatus::Queued);
        crate::hooks::workspace_activity("subagent-stop", &input, &f.w1, "agent:x").unwrap();
        assert!(busy(&gd).is_some());
        let done = crate::hooks::workspace_activity("stop", &input, &f.w1, "agent:x").unwrap();
        assert!(busy(&gd).is_none());
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].status, RequestStatus::Applied);
        // Outside a git repo: nothing, no error.
        let plain = tempfile::tempdir().unwrap();
        let none = json!({ "cwd": plain.path() });
        assert!(
            crate::hooks::workspace_activity("stop", &none, plain.path(), "a")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn fingerprints_change_with_stacks_heads_and_requests() {
        let f = setup();
        stack_with(&f.w2, &["b1"]);
        let a = fingerprints(&f.w1).unwrap();
        let a2 = fingerprints(&f.w1).unwrap();
        assert_eq!(a, a2);
        // A commit on a tracked branch (stack.json unchanged) shows up.
        fs::write(f.w2.join("more.txt"), "x\n").unwrap();
        git(&f.w2, &["add", "."]);
        git(&f.w2, &["commit", "-q", "-m", "more"]);
        let b = fingerprints(&f.w1).unwrap();
        let w2id = list(&f.w1, None, false).unwrap().workspaces[1].id.clone();
        assert_ne!(a.workspaces[&w2id], b.workspaces[&w2id]);
        let w1id = list(&f.w1, None, false).unwrap().workspaces[0].id.clone();
        assert_eq!(a.workspaces[&w1id], b.workspaces[&w1id]);
        set_activity(&f.w2, Activity::AgentRunning, None).unwrap();
        let o = edit(&f.w1, &w2id, Op::Sync, None, false).unwrap();
        let c = fingerprints(&f.w1).unwrap();
        assert_eq!(c.requests[&o.request_id].1, RequestStatus::Queued);
    }

    #[test]
    fn publish_and_list_remote_workspaces() {
        let f = setup();
        stack_with(&f.w1, &["p1"]);
        git(&f.w1, &["push", "-q", "origin", "p1"]);
        let p = publish(&f.w1).unwrap();
        // Another machine: a fresh clone sees w1's workspace, read-only.
        let other = f.w1.parent().unwrap().join("other");
        git(
            f.w1.parent().unwrap(),
            &[
                "clone",
                "-q",
                f.w1.parent().unwrap().join("origin.git").to_str().unwrap(),
                "other",
            ],
        );
        let l = list(&other, None, true).unwrap();
        let r = l.workspaces.iter().find(|w| w.id == p.workspace).unwrap();
        assert_eq!(r.kind, Kind::Remote);
        assert_eq!(r.branches[0].name, "p1");
        assert_eq!(r.branches[0].commits, 1);
        assert!(matches!(
            edit(&other, &p.workspace, Op::Sync, None, false),
            Err(Error::Stack {
                code: "remote_workspace",
                ..
            })
        ));
    }
    #[test]
    fn get_continues_a_published_stack_on_another_machine() {
        let f = setup();
        stack_with(&f.w1, &["g1", "g2"]);
        git(&f.w1, &["push", "-q", "origin", "g1", "g2"]);
        let p = publish(&f.w1).unwrap();
        let root = f.w1.parent().unwrap().to_path_buf();
        git(
            &root,
            &[
                "clone",
                "-q",
                root.join("origin.git").to_str().unwrap(),
                "laptop",
            ],
        );
        let laptop = root.join("laptop");
        git(&laptop, &["config", "user.name", "t"]);
        git(&laptop, &["config", "user.email", "t@t"]);

        // By branch: its whole stack comes along, parents as published.
        let (ws, r) = get(&laptop, "g2").unwrap();
        assert_eq!(ws, p.workspace);
        let names: Vec<_> = r
            .tracked
            .iter()
            .map(|t| (t.name.as_str(), t.parent.as_str()))
            .collect();
        assert_eq!(names, vec![("g1", "main"), ("g2", "g1")]);
        assert_eq!(r.created, vec!["g1", "g2"]);
        let w = Window::open(&laptop).unwrap();
        assert_eq!(
            w.branch_tip("g2"),
            Window::open(&f.w1).unwrap().branch_tip("g2")
        );
        assert!(w
            .log(false)
            .unwrap()
            .branches
            .iter()
            .all(|b| !b.needs_restack));

        // The desktop pushes more; get again fast-forwards.
        let mut d = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("g2.txt"), "more\n").unwrap();
        d.modify(None, true, false, false).unwrap();
        git(&f.w1, &["push", "-q", "-f", "origin", "g2"]);
        publish(&f.w1).unwrap();
        // An amend there replaces the laptop's copy, which has no work of
        // its own.
        let (_, r) = get(&laptop, &p.workspace).unwrap();
        assert_eq!(r.updated, vec!["g2"]);
        assert_eq!(
            Window::open(&laptop).unwrap().branch_tip("g2"),
            Window::open(&f.w1).unwrap().branch_tip("g2")
        );
        // The laptop took the branches over: it submits an amend with the
        // lease `get` recorded (#201), and the desktop (which pushed with
        // plain git) is refused: the remote has commits it never pushed.
        let mut lw = Window::open(&laptop).unwrap();
        git(&laptop, &["switch", "-q", "g1"]);
        fs::write(laptop.join("g1.txt"), "laptop\n").unwrap();
        lw.modify(None, true, false, false).unwrap();
        let no_pr = crate::stack::SubmitOptions {
            no_pr: true,
            all: true,
            ..Default::default()
        };
        let r = lw.submit_with(&no_pr, None).unwrap();
        assert_eq!(r.pushed, vec!["g1", "g2"]);
        assert_eq!(
            git(&root.join("origin.git"), &["rev-parse", "g2"]),
            lw.branch_tip("g2").unwrap()
        );
        fs::write(f.w1.join("g2.txt"), "desk\n").unwrap();
        d.modify(None, true, false, false).unwrap();
        git(&f.w1, &["fetch", "-q", "origin"]);
        assert_eq!(
            d.submit_with(&no_pr, None).unwrap_err().code(),
            "remote_branch_exists"
        );

        // Work made on the laptop is never thrown away.
        git(&laptop, &["switch", "-q", "g2"]);
        fs::write(laptop.join("lap.txt"), "l\n").unwrap();
        git(&laptop, &["add", "."]);
        git(&laptop, &["commit", "-q", "-m", "laptop work"]);
        fs::write(f.w1.join("g2.txt"), "again\n").unwrap();
        d.modify(None, true, false, false).unwrap();
        git(&f.w1, &["push", "-q", "-f", "origin", "g2"]);
        publish(&f.w1).unwrap();
        assert_eq!(get(&laptop, "g2").unwrap_err().code(), "branch_diverged");

        // The same machine's own workspace: that is what `give` is for.
        assert_eq!(get(&f.w2, "g1").unwrap_err().code(), "same_machine");
        assert_eq!(get(&laptop, "nope").unwrap_err().code(), "no_workspace");
    }
}
