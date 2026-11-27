//! Review comments on diffs (#165, #166).
//!
//! A *thread* is a comment, with its replies, on one *target*: a branch of a
//! workspace's stack (`branch:WS/NAME`), an agent (`agent:agN`) or a try
//! (`attempt:aN`). It is usually pinned to lines of a file: an [`Anchor`]
//! keeps the path, the side of the diff (`old` or `new`), the line range,
//! the quoted lines and three lines of context on each side, and a hash of
//! them. The branch moves on (new commits, a restack, an agent's edits), so
//! a thread is re-anchored on every read by looking for its quoted lines in
//! the file as it is now, the context deciding between repeats. When the
//! lines are gone the thread is `outdated`: still listed, shown with its
//! quote.
//!
//! States: `pending` (a draft of a review not sent yet), `open`, `addressed`
//! (an agent says it is done) and `resolved` (a person closed it). Agents
//! may reply and mark `addressed`; only a person resolves.
//!
//! Sending a review ([`submit`]) turns a target's drafts into open threads
//! and, when asked, hands them to the agent working on it: an agent that
//! stopped gets them as its next prompt, a running one at its next tool call
//! (the hook calls [`inbox`]) or, failing that, when its turn ends.
//!
//! The store is one JSON file per space (#162), so every checkout of the
//! repo on this machine sees the same threads:
//! `$AGENTVC_HOME/spaces/<space id>/comments.json`, or the repo's
//! `<git-common-dir>/agentvc/comments.json` when it joins no space.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::error::{Error, IoContext, Result};
use crate::export::HostRepo;
use crate::space::now;
use crate::stack::Window;

/// Version of `comments.json`.
pub const STORE_VERSION: u32 = 1;
/// Lines of context kept on each side of an anchor.
pub const CONTEXT: usize = 3;
/// The longest comment body accepted.
pub const MAX_BODY: usize = 64 * 1024;
const LOCK_STALE: Duration = Duration::from_secs(60);
const LOCK_WAIT: Duration = Duration::from_secs(10);

fn err(code: &'static str, message: impl Into<String>) -> Error {
    Error::Stack {
        code,
        message: message.into(),
        details: None,
    }
}

fn err_with(code: &'static str, message: impl Into<String>, details: Value) -> Error {
    Error::Stack {
        code,
        message: message.into(),
        details: Some(details),
    }
}

// ---- model -------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Branch,
    Agent,
    Attempt,
}

/// What a thread is about.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Target {
    pub kind: TargetKind,
    /// The workspace id, for a branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The branch name, `agN` or `aN`.
    pub name: String,
}

impl Target {
    pub fn branch(workspace: &str, name: &str) -> Target {
        Target {
            kind: TargetKind::Branch,
            workspace: Some(workspace.to_string()),
            name: name.to_string(),
        }
    }

    pub fn agent(id: &str) -> Target {
        Target {
            kind: TargetKind::Agent,
            workspace: None,
            name: id.to_string(),
        }
    }

    pub fn attempt(id: i64) -> Target {
        Target {
            kind: TargetKind::Attempt,
            workspace: None,
            name: format!("a{id}"),
        }
    }

    /// `branch:WS/NAME`, `agent:agN` or `attempt:aN`: the same keys as the
    /// review UI's refs.
    pub fn key(&self) -> String {
        match self.kind {
            TargetKind::Branch => format!(
                "branch:{}/{}",
                self.workspace.as_deref().unwrap_or(""),
                self.name
            ),
            TargetKind::Agent => format!("agent:{}", self.name),
            TargetKind::Attempt => format!("attempt:{}", self.name),
        }
    }

    /// Parse a key written by [`Target::key`].
    pub fn parse_key(key: &str) -> Result<Target> {
        let bad = || {
            err_with(
                "bad_target",
                format!(
                    "{key:?} is not a comment target (branch:WS/NAME, agent:agN or attempt:aN)"
                ),
                json!({ "target": key }),
            )
        };
        let (kind, rest) = key.split_once(':').ok_or_else(bad)?;
        match kind {
            "branch" => {
                let (ws, name) = rest.split_once('/').ok_or_else(bad)?;
                if ws.is_empty() || name.is_empty() {
                    return Err(bad());
                }
                Ok(Target::branch(ws, name))
            }
            "agent" if is_agent_id(rest) => Ok(Target::agent(rest)),
            "attempt" => {
                let n = rest
                    .strip_prefix('a')
                    .and_then(|n| n.parse::<i64>().ok())
                    .ok_or_else(bad)?;
                Ok(Target::attempt(n))
            }
            _ => Err(bad()),
        }
    }
}

fn is_agent_id(s: &str) -> bool {
    s.strip_prefix("ag")
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// The side of a diff a comment is on.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The target's base: removed or unchanged lines, old numbers.
    Old,
    /// The target as it is: added or unchanged lines, new numbers.
    #[default]
    New,
}

/// Where a thread was written.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Anchor {
    pub path: String,
    pub side: Side,
    /// 1-based, inclusive.
    pub start: usize,
    pub end: usize,
    /// The commit (or `aN`/`sN`) the lines were read from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// The lines commented on.
    pub lines: Vec<String>,
    /// Up to [`CONTEXT`] lines before and after them.
    #[serde(default)]
    pub before: Vec<String>,
    #[serde(default)]
    pub after: Vec<String>,
    /// SHA-1 of the lines and their context.
    pub hash: String,
}

/// Where an anchor is now.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Position {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// A draft of a review that was not sent yet.
    Pending,
    Open,
    /// An agent says it dealt with it.
    Addressed,
    /// A person closed it.
    Resolved,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Open => "open",
            State::Addressed => "addressed",
            State::Resolved => "resolved",
        }
    }
}

/// One comment of a thread.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comment {
    /// 1 for the first, counting up within the thread.
    pub id: u32,
    /// `human:NAME`, `agent:agN` (or `agent:NAME`), `github:LOGIN`.
    pub author: String,
    pub body: String,
    pub at: i64,
    /// The comment's id on GitHub, once posted or imported (#170).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_id: Option<u64>,
}

/// A thread on GitHub (#170).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GithubLink {
    pub pr: u64,
    /// The review thread's node id (for resolving it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// The first comment's id (for replies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Thread {
    /// `cN`.
    pub id: String,
    pub target: Target,
    /// `None` for a comment on the whole target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
    pub state: State,
    pub comments: Vec<Comment>,
    /// `local` or `github`.
    #[serde(default = "local")]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<GithubLink>,
    /// The review that sent it (`rN`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_by: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Computed on read: where its lines are now, `None` when outdated or
    /// not on lines.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub current: Option<Position>,
    /// Computed on read: its lines are gone from the file.
    #[serde(default, skip_deserializing)]
    pub outdated: bool,
}

fn local() -> String {
    "local".into()
}

impl Thread {
    pub fn author(&self) -> &str {
        self.comments
            .first()
            .map(|c| c.author.as_str())
            .unwrap_or("")
    }

    /// Open or addressed: still shown as something to look at.
    pub fn is_active(&self) -> bool {
        matches!(self.state, State::Open | State::Addressed)
    }
}

/// A sent batch of comments.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Review {
    /// `rN`.
    pub id: String,
    pub target: Target,
    pub threads: Vec<String>,
    pub by: String,
    pub at: i64,
    /// Sent to the agent working on the target.
    #[serde(default)]
    pub to_agent: bool,
    /// Posted as a GitHub PR review (#170).
    #[serde(default)]
    pub to_github: bool,
    /// The agent it went to, when one was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// When the agent got it, and how (`prompt`, `hook`, `turn_end`, `mcp`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_via: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_review: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    version: u32,
    #[serde(default)]
    next_thread: u64,
    #[serde(default)]
    next_review: u64,
    #[serde(default)]
    threads: Vec<Thread>,
    #[serde(default)]
    reviews: Vec<Review>,
}

// ---- the store file ----------------------------------------------------------

/// The comments file of the space the repo at `cwd` belongs to.
pub fn store_path(cwd: &Path) -> Result<PathBuf> {
    let w = Window::open(cwd)?;
    Ok(store_path_for(&w))
}

fn store_path_for(w: &Window) -> PathBuf {
    if let (Some(sp), Some(home)) = (crate::checkouts::describe(w), crate::checkouts::home()) {
        return home.join("spaces").join(sp.id).join("comments.json");
    }
    w.common_dir.join("agentvc").join("comments.json")
}

struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn lock(file: &Path) -> Result<Lock> {
    let dir = file.parent().expect("has a parent");
    fs::create_dir_all(dir).at(dir)?;
    let path = dir.join("comments.lock");
    let start = Instant::now();
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(Lock(path)),
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
                    return Err(err(
                        "comments_locked",
                        format!("{} is locked by another command; try again", path.display()),
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e).at(&path),
        }
    }
}

fn read_store(file: &Path) -> Result<Store> {
    match fs::read_to_string(file) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| {
            err(
                "bad_comments_file",
                format!("{} is not a comments file: {e}", file.display()),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store {
            version: STORE_VERSION,
            ..Default::default()
        }),
        Err(e) => Err(e).at(file),
    }
}

fn write_store(file: &Path, s: &Store) -> Result<()> {
    let dir = file.parent().expect("has a parent");
    let tmp = dir.join(format!(".comments.json.{}.tmp", std::process::id()));
    let text = serde_json::to_string_pretty(s).expect("serializes") + "\n";
    fs::write(&tmp, text).at(&tmp)?;
    fs::rename(&tmp, file).at(file)
}

/// Change the store under its lock.
fn change<T>(file: &Path, f: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
    let _l = lock(file)?;
    let mut s = read_store(file)?;
    let out = f(&mut s)?;
    s.version = STORE_VERSION;
    write_store(file, &s)?;
    Ok(out)
}

fn find<'a>(s: &'a mut Store, id: &str) -> Result<&'a mut Thread> {
    s.threads
        .iter_mut()
        .find(|t| t.id == id)
        .ok_or_else(|| Error::NotFound {
            kind: "comment",
            id: id.to_string(),
        })
}

fn check_body(body: &str) -> Result<String> {
    let b = body.trim();
    if b.is_empty() {
        return Err(err("comment_required", "the comment is empty"));
    }
    if b.len() > MAX_BODY {
        return Err(err(
            "comment_too_long",
            format!("comments are at most {MAX_BODY} bytes"),
        ));
    }
    Ok(b.to_string())
}

// ---- reading files at a target -----------------------------------------------

/// Where a target's files are read from.
#[derive(Clone, Debug)]
pub enum Rev {
    /// A commit in the repo at `dir`.
    Git { dir: PathBuf, commit: String },
    /// A stored tree of the agentvc space at `workdir`.
    Tree {
        workdir: PathBuf,
        tree: String,
        label: String,
    },
}

impl Rev {
    /// What an anchor records it as: the commit, or `aN`/`sN`.
    pub fn label(&self) -> String {
        match self {
            Rev::Git { commit, .. } => commit.clone(),
            Rev::Tree { label, .. } => label.clone(),
        }
    }

    /// The text of `path`, `None` when it is missing, binary or not UTF-8.
    pub fn read(&self, path: &str) -> Option<String> {
        match self {
            Rev::Git { dir, commit } => {
                let repo = HostRepo {
                    workdir: dir.clone(),
                };
                repo.git_raw(&["show", &format!("{commit}:{path}")]).ok()
            }
            Rev::Tree { workdir, tree, .. } => {
                let space = crate::Space::discover(workdir).ok()?;
                let mut cur = crate::objects::Oid::from_hex(tree).ok()?;
                let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
                for (i, part) in parts.iter().enumerate() {
                    let e = space
                        .objects()
                        .read_tree(&cur)
                        .ok()?
                        .into_iter()
                        .find(|e| e.name == *part)?;
                    if i + 1 == parts.len() {
                        let data = space.objects().read_blob(&e.oid).ok()?;
                        return String::from_utf8(data).ok();
                    }
                    cur = e.oid;
                }
                None
            }
        }
    }
}

/// A target's base and head.
#[derive(Clone, Debug)]
pub struct Heads {
    pub base: Option<Rev>,
    pub head: Rev,
}

impl Heads {
    pub fn side(&self, side: Side) -> Option<&Rev> {
        match side {
            Side::New => Some(&self.head),
            Side::Old => self.base.as_ref(),
        }
    }
}

/// Where `target`'s changes are now: its base and its tip.
pub fn heads(cwd: &Path, target: &Target) -> Result<Heads> {
    match target.kind {
        TargetKind::Branch => {
            let here = Window::open(cwd)?;
            let ws =
                crate::workspace::find_local(&here, target.workspace.as_deref().unwrap_or(""))?;
            let w = Window::open(&ws.path)?;
            let tip = w.branch_tip(&target.name).ok_or_else(|| {
                err_with(
                    "no_branch",
                    format!("branch {} does not exist", target.name),
                    json!({ "branch": target.name }),
                )
            })?;
            let base = w
                .state
                .branches
                .iter()
                .find(|b| b.name == target.name)
                .map(|b| b.parent_rev.clone())
                .or_else(|| {
                    let b = w.base()?;
                    w.repo.git(&["merge-base", &b, &tip], None).ok()
                });
            let dir = ws.path.clone();
            Ok(Heads {
                base: base.map(|commit| Rev::Git {
                    dir: dir.clone(),
                    commit,
                }),
                head: Rev::Git { dir, commit: tip },
            })
        }
        TargetKind::Agent => {
            let a = crate::agent::show(cwd, &target.name)?.agent;
            let repo = HostRepo {
                workdir: a.worktree.clone(),
            };
            let dir = if a.worktree.is_dir() {
                a.worktree.clone()
            } else {
                cwd.to_path_buf()
            };
            let tip = repo
                .rev(&format!("refs/heads/{}", a.branch))
                .or_else(|| a.result.as_ref().and_then(|r| r.commit.clone()))
                .unwrap_or_else(|| a.start.commit.clone());
            Ok(Heads {
                base: Some(Rev::Git {
                    dir: dir.clone(),
                    commit: a.start.commit.clone(),
                }),
                head: Rev::Git { dir, commit: tip },
            })
        }
        TargetKind::Attempt => {
            let space = crate::Space::discover(cwd)?;
            let id: i64 = target.name.trim_start_matches('a').parse().unwrap_or(0);
            let head = space.head_snapshot(id)?;
            let base = space
                .attempt(id)?
                .parent_snapshot
                .and_then(|p| space.snapshot_by_id(p).ok());
            let workdir = space.workdir().to_path_buf();
            Ok(Heads {
                base: base.map(|b| Rev::Tree {
                    workdir: workdir.clone(),
                    tree: b.tree.clone(),
                    label: format!("s{}", b.id),
                }),
                head: Rev::Tree {
                    workdir,
                    tree: head.tree.clone(),
                    label: format!("s{}", head.id),
                },
            })
        }
    }
}

// ---- anchors -----------------------------------------------------------------

fn split_lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

fn same(a: &str, b: &str) -> bool {
    a.trim_end() == b.trim_end()
}

fn hash(before: &[String], lines: &[String], after: &[String]) -> String {
    let mut h = Sha1::new();
    for l in before.iter().chain(["\u{1}".to_string()].iter()) {
        h.update(l.trim_end().as_bytes());
        h.update(b"\n");
    }
    for l in lines.iter().chain(["\u{1}".to_string()].iter()) {
        h.update(l.trim_end().as_bytes());
        h.update(b"\n");
    }
    for l in after {
        h.update(l.trim_end().as_bytes());
        h.update(b"\n");
    }
    hex::encode(h.finalize())[..16].to_string()
}

/// An anchor on lines `start..=end` (1-based) of `text`.
pub fn make_anchor(
    path: &str,
    side: Side,
    start: usize,
    end: usize,
    text: &str,
    rev: Option<String>,
) -> Result<Anchor> {
    let all = split_lines(text);
    if start == 0 || end < start || end > all.len() {
        return Err(err_with(
            "bad_line",
            format!(
                "lines {start}-{end} are not in {path} ({} lines)",
                all.len()
            ),
            json!({ "path": path, "start": start, "end": end, "lines": all.len() }),
        ));
    }
    let own = |r: &[&str]| r.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let lines = own(&all[start - 1..end]);
    let before = own(&all[start.saturating_sub(1 + CONTEXT)..start - 1]);
    let after = own(&all[end..(end + CONTEXT).min(all.len())]);
    Ok(Anchor {
        path: path.to_string(),
        side,
        start,
        end,
        rev,
        hash: hash(&before, &lines, &after),
        lines,
        before,
        after,
    })
}

/// Where `a`'s lines are in `text` now, or `None` when they are gone. The
/// quoted lines must all be there; among several places, the one whose
/// context matches best wins, then the one nearest the old line.
pub fn locate(a: &Anchor, text: &str) -> Option<Position> {
    let all = split_lines(text);
    let n = a.lines.len();
    if n == 0 || n > all.len() {
        return None;
    }
    let mut best: Option<(i64, usize)> = None;
    for i in 0..=all.len() - n {
        if !(0..n).all(|k| same(all[i + k], &a.lines[k])) {
            continue;
        }
        let mut ctx = 0i64;
        for (k, l) in a.before.iter().rev().enumerate() {
            if i > k && same(all[i - 1 - k], l) {
                ctx += 1;
            }
        }
        for (k, l) in a.after.iter().enumerate() {
            if all.get(i + n + k).is_some_and(|x| same(x, l)) {
                ctx += 1;
            }
        }
        let dist = (i as i64 + 1 - a.start as i64).abs();
        let score = ctx * 1_000_000 - dist;
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, i));
        }
    }
    best.map(|(_, i)| Position {
        start: i + 1,
        end: i + n,
    })
}

/// Fill in `current` and `outdated` of `threads` against their targets as
/// they are now. Targets that can't be read leave threads where they were
/// written.
pub fn reanchor(cwd: &Path, threads: &mut [Thread]) {
    let mut heads_of: HashMap<String, Option<Heads>> = HashMap::new();
    let mut texts: HashMap<(String, String, Side), Option<String>> = HashMap::new();
    for t in threads.iter_mut() {
        let Some(a) = &t.anchor else { continue };
        let key = t.target.key();
        let h = heads_of
            .entry(key.clone())
            .or_insert_with(|| heads(cwd, &t.target).ok());
        let Some(h) = h else {
            t.current = Some(Position {
                start: a.start,
                end: a.end,
            });
            continue;
        };
        let text = texts
            .entry((key, a.path.clone(), a.side))
            .or_insert_with(|| h.side(a.side).and_then(|r| r.read(&a.path)));
        match text {
            Some(text) => {
                t.current = locate(a, text);
                t.outdated = t.current.is_none();
            }
            None => {
                t.current = None;
                t.outdated = true;
            }
        }
    }
}

// ---- operations --------------------------------------------------------------

/// Lines to comment on.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LineRange {
    pub path: String,
    #[serde(default)]
    pub side: Side,
    pub start: usize,
    #[serde(default)]
    pub end: Option<usize>,
}

/// A new thread.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddRequest {
    pub target: Target,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineRange>,
    pub body: String,
    pub author: String,
    /// A draft of a review, sent later with [`submit`].
    #[serde(default)]
    pub pending: bool,
}

/// Start a thread.
pub fn add(cwd: &Path, req: &AddRequest) -> Result<Thread> {
    let body = check_body(&req.body)?;
    let anchor = match &req.lines {
        None => None,
        Some(l) => {
            let path = l.path.trim_start_matches("./").to_string();
            let h = heads(cwd, &req.target)?;
            let rev = h.side(l.side).ok_or_else(|| {
                err_with(
                    "no_base",
                    format!("{} has no base to comment on", req.target.key()),
                    json!({ "target": req.target.key() }),
                )
            })?;
            let text = rev.read(&path).ok_or_else(|| {
                err_with(
                    "no_file",
                    format!("{path} is not a text file of {}", req.target.key()),
                    json!({ "path": path, "target": req.target.key() }),
                )
            })?;
            let end = l.end.unwrap_or(l.start);
            Some(make_anchor(
                &path,
                l.side,
                l.start,
                end,
                &text,
                Some(rev.label()),
            )?)
        }
    };
    let file = store_path(cwd)?;
    let at = now();
    let mut t = change(&file, |s| {
        s.next_thread += 1;
        let t = Thread {
            id: format!("c{}", s.next_thread),
            target: req.target.clone(),
            anchor,
            state: if req.pending {
                State::Pending
            } else {
                State::Open
            },
            comments: vec![Comment {
                id: 1,
                author: req.author.clone(),
                body,
                at,
                github_id: None,
            }],
            source: local(),
            github: None,
            review: None,
            state_by: None,
            created_at: at,
            updated_at: at,
            current: None,
            outdated: false,
        };
        s.threads.push(t.clone());
        Ok(t)
    })?;
    reanchor(cwd, std::slice::from_mut(&mut t));
    Ok(t)
}

/// Which threads to list.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub target: Option<Target>,
    /// Open and addressed only (drafts are left out).
    pub active: bool,
    /// Drafts too.
    pub pending: bool,
}

/// Threads, oldest first, re-anchored.
pub fn list(cwd: &Path, f: &Filter) -> Result<Vec<Thread>> {
    let s = read_store(&store_path(cwd)?)?;
    let mut out: Vec<Thread> = s
        .threads
        .into_iter()
        .filter(|t| f.target.as_ref().is_none_or(|x| *x == t.target))
        .filter(|t| f.pending || t.state != State::Pending)
        .filter(|t| !f.active || t.is_active())
        .collect();
    reanchor(cwd, &mut out);
    Ok(out)
}

/// One thread, re-anchored.
pub fn get(cwd: &Path, id: &str) -> Result<Thread> {
    let s = read_store(&store_path(cwd)?)?;
    let mut t = s
        .threads
        .into_iter()
        .find(|t| t.id == id)
        .ok_or_else(|| Error::NotFound {
            kind: "comment",
            id: id.to_string(),
        })?;
    reanchor(cwd, std::slice::from_mut(&mut t));
    Ok(t)
}

fn is_agent(author: &str) -> bool {
    author.starts_with("agent:")
}

/// Add a reply. A person's reply reopens a thread an agent marked
/// addressed.
pub fn reply(cwd: &Path, id: &str, body: &str, author: &str) -> Result<Thread> {
    let body = check_body(body)?;
    let at = now();
    let mut t = change(&store_path(cwd)?, |s| {
        let t = find(s, id)?;
        let next = t.comments.iter().map(|c| c.id).max().unwrap_or(0) + 1;
        t.comments.push(Comment {
            id: next,
            author: author.to_string(),
            body,
            at,
            github_id: None,
        });
        if t.state == State::Addressed && !is_agent(author) {
            t.state = State::Open;
            t.state_by = Some(author.to_string());
        }
        t.updated_at = at;
        Ok(t.clone())
    })?;
    reanchor(cwd, std::slice::from_mut(&mut t));
    Ok(t)
}

/// Close a thread: a person resolves it, an agent marks it addressed.
pub fn resolve(cwd: &Path, id: &str, by: &str) -> Result<Thread> {
    let to = if is_agent(by) {
        State::Addressed
    } else {
        State::Resolved
    };
    set_state(cwd, id, to, by)
}

/// Open a resolved or addressed thread again.
pub fn reopen(cwd: &Path, id: &str, by: &str) -> Result<Thread> {
    set_state(cwd, id, State::Open, by)
}

fn set_state(cwd: &Path, id: &str, to: State, by: &str) -> Result<Thread> {
    let mut t = change(&store_path(cwd)?, |s| {
        let t = find(s, id)?;
        if t.state == State::Pending {
            return Err(err_with(
                "comment_pending",
                format!("{id} is a draft; send the review first"),
                json!({ "id": id }),
            ));
        }
        if is_agent(by) && t.state == State::Resolved {
            return Err(err_with(
                "not_allowed",
                format!("{id} was resolved by a person; agents can't reopen it"),
                json!({ "id": id }),
            ));
        }
        t.state = to;
        t.state_by = Some(by.to_string());
        t.updated_at = now();
        Ok(t.clone())
    })?;
    reanchor(cwd, std::slice::from_mut(&mut t));
    Ok(t)
}

/// Delete a draft (only its author's, only while pending).
pub fn discard(cwd: &Path, id: &str, by: &str) -> Result<Thread> {
    change(&store_path(cwd)?, |s| {
        let t = find(s, id)?.clone();
        if t.state != State::Pending {
            return Err(err_with(
                "not_a_draft",
                format!("{id} was sent; resolve it instead"),
                json!({ "id": id }),
            ));
        }
        if t.author() != by {
            return Err(err_with(
                "not_allowed",
                format!("{id} is {}'s draft", t.author()),
                json!({ "id": id }),
            ));
        }
        s.threads.retain(|x| x.id != id);
        Ok(t)
    })
}

/// Delete every draft of `by` on `target`.
pub fn discard_all(cwd: &Path, target: &Target, by: &str) -> Result<Vec<String>> {
    change(&store_path(cwd)?, |s| {
        let gone: Vec<String> = s
            .threads
            .iter()
            .filter(|t| t.target == *target && t.state == State::Pending && t.author() == by)
            .map(|t| t.id.clone())
            .collect();
        s.threads.retain(|t| !gone.contains(&t.id));
        Ok(gone)
    })
}

// ---- reviews and agents ------------------------------------------------------

/// Where a review goes.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Deliver {
    #[serde(default)]
    pub agent: bool,
    #[serde(default)]
    pub github: bool,
}

/// What happened to a review sent to an agent.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// The agent got it as its next prompt.
    Prompt,
    /// It is working; it gets it at its next tool call or when it stops.
    Queued,
    /// No agent works on the target; one that does gets it (hook, MCP).
    Waiting,
    /// Not sent to an agent.
    None,
}

#[derive(Clone, Debug, Serialize)]
pub struct Submitted {
    pub review: Review,
    pub threads: Vec<Thread>,
    pub delivery: Delivery,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<crate::agent::Agent>,
}

/// The agent working on `target`, if any: the agent itself, or the newest
/// agent (not landed or discarded) whose branch, start branch (when it
/// revises it) or landing is this branch.
pub fn agent_for(cwd: &Path, target: &Target) -> Option<crate::agent::Agent> {
    match target.kind {
        TargetKind::Agent => crate::agent::show(cwd, &target.name).ok().map(|d| d.agent),
        TargetKind::Attempt => None,
        TargetKind::Branch => {
            let ws = target.workspace.as_deref()?;
            let mut all = crate::agent::list(cwd, true).ok()?.agents;
            all.sort_by_key(|a| std::cmp::Reverse(a.created_at));
            all.into_iter().find(|a| {
                !a.state.is_closed()
                    && a.workspace == ws
                    && (a.branch == target.name
                        || (a.mode == crate::agent::Mode::Revise && a.start.branch == target.name))
            })
        }
    }
}

fn quote(a: &Anchor) -> String {
    let mut out = String::new();
    for l in &a.lines {
        out.push_str("    > ");
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// A review as a prompt: each thread as `path:line`, the quoted code and the
/// comment, then how to answer.
pub fn format_review(threads: &[Thread]) -> String {
    let mut out = format!(
        "Review comments ({}). Address each one, then reply in its thread with the agentvc \
         `reply_comment` tool and mark it done with `resolve_comment` (ids below).\n",
        threads.len()
    );
    for t in threads {
        out.push('\n');
        let first = t.comments.first().map(|c| c.body.as_str()).unwrap_or("");
        match &t.anchor {
            Some(a) => {
                let pos = t.current.unwrap_or(Position {
                    start: a.start,
                    end: a.end,
                });
                let lines = if pos.start == pos.end {
                    format!("{}", pos.start)
                } else {
                    format!("{}-{}", pos.start, pos.end)
                };
                let side = if a.side == Side::Old {
                    " (removed lines)"
                } else {
                    ""
                };
                out.push_str(&format!("[{}] {}:{lines}{side}\n", t.id, a.path));
                out.push_str(&quote(a));
            }
            None => out.push_str(&format!("[{}] (on the whole change)\n", t.id)),
        }
        for l in first.lines() {
            out.push_str("  ");
            out.push_str(l);
            out.push('\n');
        }
        for c in t.comments.iter().skip(1) {
            out.push_str(&format!("  reply from {}: {}\n", c.author, c.body));
        }
    }
    out
}

/// Send the drafts of `by` on `target` as one review. With `to.agent`, the
/// agent working on it gets them: as its next prompt when it has stopped
/// (`exe` starts its worker), queued while it works.
pub fn submit(
    cwd: &Path,
    target: &Target,
    to: Deliver,
    by: &str,
    exe: Option<&Path>,
) -> Result<Submitted> {
    let file = store_path(cwd)?;
    let agent = if to.agent {
        agent_for(cwd, target)
    } else {
        None
    };
    let at = now();
    let review = change(&file, |s| {
        let ids: Vec<String> = s
            .threads
            .iter()
            .filter(|t| t.target == *target && t.state == State::Pending && t.author() == by)
            .map(|t| t.id.clone())
            .collect();
        if ids.is_empty() {
            return Err(err_with(
                "no_drafts",
                format!("no comments to send on {}", target.key()),
                json!({ "target": target.key() }),
            ));
        }
        s.next_review += 1;
        let rid = format!("r{}", s.next_review);
        for t in s.threads.iter_mut().filter(|t| ids.contains(&t.id)) {
            t.state = State::Open;
            t.review = Some(rid.clone());
            t.updated_at = at;
        }
        let r = Review {
            id: rid,
            target: target.clone(),
            threads: ids,
            by: by.to_string(),
            at,
            to_agent: to.agent,
            to_github: to.github,
            agent: agent.as_ref().map(|a| a.id.clone()),
            delivered_at: None,
            delivered_via: None,
            github_review: None,
        };
        s.reviews.push(r.clone());
        Ok(r)
    })?;
    let mut threads = list(
        cwd,
        &Filter {
            target: Some(target.clone()),
            ..Default::default()
        },
    )?;
    threads.retain(|t| review.threads.contains(&t.id));
    if !to.agent {
        return Ok(Submitted {
            review,
            threads,
            delivery: Delivery::None,
            agent: None,
        });
    }
    let Some(a) = agent else {
        return Ok(Submitted {
            review,
            threads,
            delivery: Delivery::Waiting,
            agent: None,
        });
    };
    if a.state.is_done() {
        if let Some(exe) = exe {
            let text = format_review(&threads);
            let started = crate::agent::reply(cwd, &a.id, &text, exe)?;
            let review = mark_delivered(&file, std::slice::from_ref(&review.id), "prompt")?
                .pop()
                .unwrap_or(review);
            return Ok(Submitted {
                review,
                threads,
                delivery: Delivery::Prompt,
                agent: Some(started),
            });
        }
    }
    Ok(Submitted {
        review,
        threads,
        delivery: Delivery::Queued,
        agent: Some(a),
    })
}

fn mark_delivered(file: &Path, ids: &[String], via: &str) -> Result<Vec<Review>> {
    let at = now();
    change(file, |s| {
        let mut out = Vec::new();
        for r in s.reviews.iter_mut().filter(|r| ids.contains(&r.id)) {
            r.delivered_at = Some(at);
            r.delivered_via = Some(via.to_string());
            out.push(r.clone());
        }
        Ok(out)
    })
}

/// Reviews sent to an agent that it has not got yet, for the targets the
/// agent works on.
fn undelivered(s: &Store, targets: &[Target], agent: Option<&str>) -> Vec<Review> {
    s.reviews
        .iter()
        .filter(|r| r.to_agent && r.delivered_at.is_none())
        .filter(|r| targets.contains(&r.target) || (agent.is_some() && r.agent.as_deref() == agent))
        .cloned()
        .collect()
}

/// What an agent working in `cwd` works on: its agent id (when it is a
/// control-center agent) and its targets (the agent, and the checked-out
/// branch of the workspace).
pub fn targets_at(cwd: &Path) -> (Option<String>, Vec<Target>) {
    let Ok(w) = Window::open(cwd) else {
        return (None, Vec::new());
    };
    let mut out = Vec::new();
    let top = crate::stack::canon(w.path());
    let agent = crate::agent::list(cwd, true).ok().and_then(|l| {
        l.agents
            .into_iter()
            .find(|a| crate::stack::canon(&a.worktree) == top)
    });
    if let Some(a) = &agent {
        out.push(Target::agent(&a.id));
    }
    if let Some(b) = w.current_branch() {
        if let Ok(ws) = crate::workspace::find_local(&w, &w.path().to_string_lossy()) {
            out.push(Target::branch(&ws.record.id, &b));
        }
        if let Some(a) = &agent {
            out.push(Target::branch(&a.workspace, &a.branch));
        }
    }
    (agent.map(|a| a.id), out)
}

/// For an agent's hook: the reviews sent its way that it has not seen, as
/// one text, marked delivered (`via`). `None` when there are none.
pub fn inbox(cwd: &Path, via: &str) -> Result<Option<String>> {
    let (agent, targets) = targets_at(cwd);
    if targets.is_empty() {
        return Ok(None);
    }
    let file = store_path(cwd)?;
    if !file.exists() {
        return Ok(None);
    }
    let pending = undelivered(&read_store(&file)?, &targets, agent.as_deref());
    if pending.is_empty() {
        return Ok(None);
    }
    let ids: Vec<String> = pending.iter().map(|r| r.id.clone()).collect();
    let wanted: Vec<String> = pending.iter().flat_map(|r| r.threads.clone()).collect();
    let mut threads = list(cwd, &Filter::default())?;
    threads.retain(|t| wanted.contains(&t.id) && t.is_active());
    mark_delivered(&file, &ids, via)?;
    if threads.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "A person sent you review comments while you were working.\n{}",
        format_review(&threads)
    )))
}

/// When agent `id` stops: the reviews queued for it while it worked, as
/// the prompt of its next turn, marked delivered. `None` when there are
/// none.
pub fn queued_for_agent(cwd: &Path, a: &crate::agent::Agent) -> Result<Option<String>> {
    let file = store_path(cwd)?;
    if !file.exists() {
        return Ok(None);
    }
    let targets = vec![
        Target::agent(&a.id),
        Target::branch(&a.workspace, &a.branch),
    ];
    let pending = undelivered(&read_store(&file)?, &targets, Some(&a.id));
    if pending.is_empty() {
        return Ok(None);
    }
    let ids: Vec<String> = pending.iter().map(|r| r.id.clone()).collect();
    let wanted: Vec<String> = pending.iter().flat_map(|r| r.threads.clone()).collect();
    let mut threads = list(cwd, &Filter::default())?;
    threads.retain(|t| wanted.contains(&t.id) && t.is_active());
    mark_delivered(&file, &ids, "turn_end")?;
    Ok((!threads.is_empty()).then(|| format_review(&threads)))
}

/// Open (and addressed) threads per target key, for badges.
pub fn active_counts(cwd: &Path) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    let Ok(file) = store_path(cwd) else {
        return out;
    };
    let Ok(s) = read_store(&file) else {
        return out;
    };
    for t in s.threads.iter().filter(|t| t.state == State::Open) {
        *out.entry(t.target.key()).or_insert(0) += 1;
    }
    out
}

/// A fingerprint of the store, for pollers: changes whenever it is written.
pub fn fingerprint(cwd: &Path) -> Option<(u64, i64)> {
    let file = store_path(cwd).ok()?;
    let m = fs::metadata(&file).ok()?;
    let t = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as i64;
    Some((m.len(), t))
}

/// The targets with threads changed at or after `since` (unix seconds), for
/// a poller to name in its event.
pub fn touched_targets(cwd: &Path, since: i64) -> Vec<String> {
    let Ok(file) = store_path(cwd) else {
        return Vec::new();
    };
    let Ok(s) = read_store(&file) else {
        return Vec::new();
    };
    let mut out: Vec<String> = s
        .threads
        .iter()
        .filter(|t| t.updated_at >= since)
        .map(|t| t.target.key())
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[&str]) -> String {
        lines.join("\n") + "\n"
    }

    #[test]
    fn target_keys_round_trip() {
        for t in [
            Target::branch("w1a2b3c4d", "feat/x"),
            Target::agent("ag3"),
            Target::attempt(7),
        ] {
            assert_eq!(Target::parse_key(&t.key()).unwrap(), t);
        }
        for bad in ["feat", "branch:feat", "agent:x", "attempt:7", "branch:/x"] {
            assert_eq!(Target::parse_key(bad).unwrap_err().code(), "bad_target");
        }
    }

    #[test]
    fn locate_follows_a_block_inserted_above() {
        let old = text(&["a", "b", "fn x() {", "  1", "}", "c"]);
        let a = make_anchor("f.rs", Side::New, 3, 4, &old, None).unwrap();
        assert_eq!(a.lines, vec!["fn x() {", "  1"]);
        assert_eq!(a.before, vec!["a", "b"]);
        assert_eq!(a.after, vec!["}", "c"]);
        let new = text(&[
            "new 1", "new 2", "new 3", "a", "b", "fn x() {", "  1", "}", "c",
        ]);
        assert_eq!(locate(&a, &new), Some(Position { start: 6, end: 7 }));
    }

    #[test]
    fn locate_picks_the_repeat_with_matching_context() {
        let old = text(&["x", "dup", "y", "p", "dup", "q"]);
        let a = make_anchor("f", Side::New, 5, 5, &old, None).unwrap();
        // The second `dup` moved to the top, with its context.
        let new = text(&["p", "dup", "q", "x", "dup", "y"]);
        assert_eq!(locate(&a, &new), Some(Position { start: 2, end: 2 }));
    }

    #[test]
    fn locate_says_gone_when_the_line_is_deleted_or_changed() {
        let old = text(&["a", "b", "c"]);
        let a = make_anchor("f", Side::New, 2, 2, &old, None).unwrap();
        assert_eq!(locate(&a, &text(&["a", "c"])), None);
        assert_eq!(locate(&a, &text(&["a", "B", "c"])), None);
        // Trailing whitespace doesn't count as a change.
        assert_eq!(
            locate(&a, &text(&["a", "b  ", "c"])),
            Some(Position { start: 2, end: 2 })
        );
    }

    #[test]
    fn make_anchor_checks_the_range() {
        let t = text(&["a", "b"]);
        for (s, e) in [(0, 1), (2, 1), (1, 3)] {
            assert_eq!(
                make_anchor("f", Side::New, s, e, &t, None)
                    .unwrap_err()
                    .code(),
                "bad_line"
            );
        }
        let a = make_anchor("f", Side::New, 1, 2, &t, None).unwrap();
        assert!(a.before.is_empty() && a.after.is_empty());
    }

    #[test]
    fn format_review_quotes_lines() {
        let t = text(&["a", "let x = 1;", "c"]);
        let anchor = make_anchor("src/x.rs", Side::New, 2, 2, &t, None).unwrap();
        let th = Thread {
            id: "c4".into(),
            target: Target::agent("ag1"),
            anchor: Some(anchor),
            state: State::Open,
            comments: vec![Comment {
                id: 1,
                author: "human:yury".into(),
                body: "use a const".into(),
                at: 0,
                github_id: None,
            }],
            source: local(),
            github: None,
            review: None,
            state_by: None,
            created_at: 0,
            updated_at: 0,
            current: None,
            outdated: false,
        };
        let s = format_review(&[th]);
        assert!(
            s.contains("[c4] src/x.rs:2\n    > let x = 1;\n  use a const\n"),
            "{s}"
        );
        assert!(s.contains("reply_comment"));
    }

    // These drive git; the store goes to the repo's common dir (no
    // AGENTVC_HOME in unit tests).
    #[cfg(unix)]
    mod repo {
        use super::super::*;
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
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        fn lines(n: usize) -> String {
            (1..=n).map(|i| format!("line {i}\n")).collect()
        }

        /// A repo with origin/main and a stack branch `feat` that adds a
        /// function after line 5 of `a.txt`.
        fn repo() -> (tempfile::TempDir, PathBuf, String) {
            let t = tempfile::tempdir().unwrap();
            let origin = t.path().join("origin.git");
            let work = t.path().join("work");
            git(
                t.path(),
                &[
                    "init",
                    "--quiet",
                    "--bare",
                    "-b",
                    "main",
                    origin.to_str().unwrap(),
                ],
            );
            git(
                t.path(),
                &["init", "--quiet", "-b", "main", work.to_str().unwrap()],
            );
            git(&work, &["config", "user.name", "t"]);
            git(&work, &["config", "user.email", "t@t"]);
            fs::write(work.join("a.txt"), lines(10)).unwrap();
            git(&work, &["add", "."]);
            git(&work, &["commit", "--quiet", "-m", "init"]);
            git(
                &work,
                &["remote", "add", "origin", origin.to_str().unwrap()],
            );
            git(&work, &["push", "--quiet", "origin", "main"]);
            git(&work, &["fetch", "--quiet", "origin"]);
            let mut text = lines(5);
            text.push_str("fn added() {\n    todo!()\n}\n");
            text.push_str(
                &lines(10)
                    .lines()
                    .skip(5)
                    .map(|l| format!("{l}\n"))
                    .collect::<String>(),
            );
            fs::write(work.join("a.txt"), text).unwrap();
            git(&work, &["add", "."]);
            let mut w = Window::open(&work).unwrap();
            w.create(Some("feat"), Some("feat"), false).unwrap();
            let work = crate::stack::canon(&work);
            let ws = crate::workspace::find_local(&w, &work.to_string_lossy())
                .unwrap()
                .record
                .id;
            (t, work, ws)
        }

        fn add_on(work: &Path, target: &Target, start: usize, pending: bool) -> Thread {
            add(
                work,
                &AddRequest {
                    target: target.clone(),
                    lines: Some(LineRange {
                        path: "a.txt".into(),
                        side: Side::New,
                        start,
                        end: Some(start),
                    }),
                    body: "no todo!() here".into(),
                    author: "human:t".into(),
                    pending,
                },
            )
            .unwrap()
        }

        #[test]
        fn threads_follow_their_line_through_a_restack_and_go_outdated() {
            let (_t, work, ws) = repo();
            let feat = Target::branch(&ws, "feat");
            let t = add_on(&work, &feat, 7, false);
            assert_eq!(t.anchor.as_ref().unwrap().lines, vec!["    todo!()"]);
            assert_eq!(t.current, Some(Position { start: 7, end: 7 }));

            // main gets three lines at the top; feat is restacked onto it.
            git(&work, &["checkout", "--quiet", "main"]);
            fs::write(
                work.join("a.txt"),
                format!("top 1\ntop 2\ntop 3\n{}", lines(10)),
            )
            .unwrap();
            git(&work, &["commit", "--quiet", "-am", "main moves"]);
            git(&work, &["push", "--quiet", "origin", "main"]);
            git(&work, &["fetch", "--quiet", "origin"]);
            git(&work, &["checkout", "--quiet", "feat"]);
            let mut w = Window::open(&work).unwrap();
            w.restack(None, false).unwrap();
            assert!(fs::read_to_string(work.join("a.txt"))
                .unwrap()
                .starts_with("top 1"));

            let all = list(
                &work,
                &Filter {
                    target: Some(feat.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].current, Some(Position { start: 10, end: 10 }));
            assert!(!all[0].outdated);

            // The line is replaced: the thread is outdated, still listed.
            let text = fs::read_to_string(work.join("a.txt")).unwrap();
            fs::write(
                work.join("a.txt"),
                text.replace("    todo!()\n", "    42\n"),
            )
            .unwrap();
            git(&work, &["commit", "--quiet", "-am", "fix"]);
            let all = list(
                &work,
                &Filter {
                    target: Some(feat.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(all[0].outdated);
            assert_eq!(all[0].current, None);
            assert_eq!(active_counts(&work).get(&feat.key()), Some(&1));
        }

        #[test]
        fn drafts_replies_and_states() {
            let (_t, work, ws) = repo();
            let feat = Target::branch(&ws, "feat");
            let d1 = add_on(&work, &feat, 6, true);
            let d2 = add_on(&work, &feat, 8, true);
            assert_eq!(d1.state, State::Pending);
            // Drafts are not listed unless asked for, and can't be resolved.
            assert!(list(&work, &Filter::default()).unwrap().is_empty());
            assert_eq!(
                resolve(&work, &d1.id, "human:t").unwrap_err().code(),
                "comment_pending"
            );
            assert_eq!(
                discard(&work, &d2.id, "human:u").unwrap_err().code(),
                "not_allowed"
            );
            discard(&work, &d2.id, "human:t").unwrap();

            let sent = submit(
                &work,
                &feat,
                Deliver {
                    agent: true,
                    github: false,
                },
                "human:t",
                None,
            )
            .unwrap();
            assert_eq!(sent.delivery, Delivery::Waiting, "no agent works on feat");
            assert_eq!(sent.review.threads, vec![d1.id.clone()]);
            assert_eq!(
                submit(&work, &feat, Deliver::default(), "human:t", None)
                    .unwrap_err()
                    .code(),
                "no_drafts"
            );

            // An agent marks it addressed; a person's reply reopens it.
            let t = resolve(&work, &d1.id, "agent:ag1").unwrap();
            assert_eq!(t.state, State::Addressed);
            let t = reply(&work, &d1.id, "still there", "human:t").unwrap();
            assert_eq!((t.state, t.comments.len()), (State::Open, 2));
            let t = resolve(&work, &d1.id, "human:t").unwrap();
            assert_eq!(t.state, State::Resolved);
            assert_eq!(
                reopen(&work, &d1.id, "agent:ag1").unwrap_err().code(),
                "not_allowed"
            );
            assert_eq!(reopen(&work, &d1.id, "human:t").unwrap().state, State::Open);

            // An agent checked out on feat gets the review once, at its next tool call.
            let text = inbox(&work, "hook").unwrap().expect("one review waiting");
            assert!(text.contains("a.txt:6"), "{text}");
            assert!(inbox(&work, "hook").unwrap().is_none(), "delivered once");
        }

        #[test]
        fn bad_lines_and_files_are_refused() {
            let (_t, work, ws) = repo();
            let feat = Target::branch(&ws, "feat");
            let mut req = AddRequest {
                target: feat.clone(),
                lines: Some(LineRange {
                    path: "a.txt".into(),
                    side: Side::New,
                    start: 99,
                    end: None,
                }),
                body: "x".into(),
                author: "human:t".into(),
                pending: false,
            };
            assert_eq!(add(&work, &req).unwrap_err().code(), "bad_line");
            req.lines.as_mut().unwrap().path = "nope.txt".into();
            req.lines.as_mut().unwrap().start = 1;
            assert_eq!(add(&work, &req).unwrap_err().code(), "no_file");
            req.lines = None;
            req.body = "  ".into();
            assert_eq!(add(&work, &req).unwrap_err().code(), "comment_required");
            req.target = Target::branch(&ws, "nope");
            req.body = "x".into();
            req.lines = Some(LineRange {
                path: "a.txt".into(),
                side: Side::Old,
                start: 1,
                end: None,
            });
            assert_eq!(add(&work, &req).unwrap_err().code(), "no_branch");
        }
    }
}
