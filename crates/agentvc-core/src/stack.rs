//! Per-window stacks (#107): stacked branches whose state belongs to one
//! worktree, so several editor windows on one repository never share or
//! fight over one stack tree.
//!
//! Graphite keeps one stack tree per repository, in the git directory every
//! worktree shares, so with a Cursor window per worktree each window sees
//! every other window's branches, and `gt sync` in one window moves trunk and
//! deletes branches under all of them. Here:
//!
//! - **State lives in the worktree's own git directory**
//!   (`<git-dir>/agentvc/stack.json`, where `<git-dir>` is
//!   `git rev-parse --absolute-git-dir`: `.git/worktrees/<name>` for a linked
//!   worktree, `.git` for the main one). Objects, refs and the remote stay
//!   shared. No agentvc space is needed: it works in any git worktree.
//! - **A branch belongs to at most one window.** Commands only rewrite,
//!   check out or delete branches tracked by the current window. A branch
//!   another window tracks can be a parent (a read-only base), and restack
//!   follows it, but it is never rewritten from here.
//! - **The base is the remote trunk** (`origin/main`), never the local trunk
//!   branch, which may be checked out in another worktree. `sync` fetches
//!   only that ref.
//! - Each tracked branch records its parent and `parent_rev`, the parent
//!   commit it sits on (Graphite's `parentBranchRevision`). Restack replays
//!   exactly `parent_rev..branch` onto the parent's new tip, so long chains
//!   restack correctly after a parent is amended, rebased or squash-merged.
//! - **Restack works in memory**: each commit is cherry-picked with
//!   `git merge-tree` plus agentvc's conflict auto-resolution (#91), and all
//!   refs move in one transaction at the end. Nothing is checked out, so it
//!   is fast for 10+ branches, and a conflict changes nothing. With
//!   `keep_conflicts` the conflicting branch is rebased in the worktree with
//!   `git rebase` instead, for the editor's conflict UI; the next command
//!   picks up where it stopped.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, IoContext, Result};
use crate::export::HostRepo;
use crate::github::Api;
use crate::rebase::merge_commits;
use crate::resolve::{Rules, Sides};

/// Version of `stack.json`.
pub const STATE_VERSION: u32 = 1;

/// One branch tracked by this window.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tracked {
    pub name: String,
    /// The parent branch; the trunk name means the remote trunk.
    pub parent: String,
    /// The parent commit this branch sits on: what restack replays from.
    pub parent_rev: String,
    /// The branch's pull request, once submitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    /// The PR's state when this window last looked: `open`, `draft`,
    /// `closed` or `merged`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_state: Option<String>,
}

/// PR state as `pr_state` records it.
fn pr_state_of(v: &Value) -> String {
    if v["merged"].as_bool() == Some(true) || v["merged_at"].is_string() {
        "merged".into()
    } else if v["state"].as_str() == Some("closed") {
        "closed".into()
    } else if v["draft"].as_bool() == Some(true) {
        "draft".into()
    } else {
        "open".into()
    }
}

/// A rebase left in the worktree for the user to finish (`keep_conflicts`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pending {
    pub branch: String,
    /// The commit the branch is being rebased onto.
    pub onto: String,
}

/// The contents of `stack.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    /// Trunk branch name, e.g. `main`.
    pub trunk: String,
    /// Remote whose trunk is the base, e.g. `origin`; empty for none.
    pub remote: String,
    /// Tracked branches; parents come before their children.
    pub branches: Vec<Tracked>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
}

/// A branch moved by restack.
#[derive(Clone, Debug, Serialize)]
pub struct BranchUpdate {
    pub branch: String,
    pub old: String,
    pub new: String,
    /// The parent commit it now sits on.
    pub onto: String,
    /// Commits replayed onto the new parent.
    pub commits: usize,
    /// Commits that became empty (their change is already in the parent).
    pub dropped: usize,
}

/// Where a restack stopped.
#[derive(Clone, Debug, Serialize)]
pub struct StackConflict {
    pub branch: String,
    pub commit: String,
    pub subject: String,
    pub onto: String,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RestackResult {
    pub updated: Vec<BranchUpdate>,
    pub unchanged: Vec<String>,
    /// Set when a branch could not be replayed without conflicts. Branches
    /// up to it were restacked only when `kept_conflicts` is true.
    pub conflict: Option<StackConflict>,
    /// The conflicting branch is being rebased in the worktree: resolve the
    /// files, `git rebase --continue`, then run restack again.
    pub kept_conflicts: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct SyncResult {
    pub fetched: bool,
    /// The base commit after the fetch.
    pub base: String,
    /// Branches found merged into the base and removed from the stack.
    pub merged: Vec<String>,
    /// Merged branches whose local branch was deleted.
    pub deleted: Vec<String>,
    /// Where the worktree was switched to when its branch was merged.
    pub switched_to: Option<String>,
    pub restack: RestackResult,
}

/// What a branch's parent is.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParentKind {
    /// The remote trunk.
    Trunk,
    /// Another branch of this window.
    Stack,
    /// A branch another window tracks (read-only here).
    OtherWindow,
    /// A local branch no window tracks.
    Branch,
}

/// One branch as `log` shows it.
#[derive(Clone, Debug, Serialize)]
pub struct BranchInfo {
    pub name: String,
    pub parent: String,
    pub parent_kind: ParentKind,
    /// The window tracking the parent, for `other_window`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_window: Option<PathBuf>,
    pub parent_rev: String,
    /// The branch tip; `None` if the branch was deleted outside agentvc.
    pub tip: Option<String>,
    /// Commits on the branch, `parent_rev..tip`.
    pub commits: usize,
    /// The parent moved since this branch was last restacked.
    pub needs_restack: bool,
    pub current: bool,
    /// 0 for branches on trunk or on a branch outside this window.
    pub depth: usize,
    pub children: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    /// Another worktree that has this branch checked out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_out_elsewhere: Option<PathBuf>,
}

/// Another window's stack, for `log --all`.
#[derive(Clone, Debug, Serialize)]
pub struct OtherStack {
    /// The worktree's directory.
    pub path: PathBuf,
    pub branches: Vec<OtherBranch>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OtherBranch {
    pub name: String,
    pub parent: String,
    pub tip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogResult {
    /// This worktree's directory.
    pub window: PathBuf,
    pub git_dir: PathBuf,
    pub trunk: String,
    /// The ref stacks are based on, e.g. `origin/main`.
    pub base_ref: String,
    pub base: Option<String>,
    pub current: Option<String>,
    /// This window's branches, parents before children (depth-first).
    pub branches: Vec<BranchInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
    /// Other windows' stacks (only with `all`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub others: Vec<OtherStack>,
}

fn stack_err(code: &'static str, message: impl Into<String>) -> Error {
    Error::Stack {
        code,
        message: message.into(),
        details: None,
    }
}

fn stack_err_with(
    code: &'static str,
    message: impl Into<String>,
    details: serde_json::Value,
) -> Error {
    Error::Stack {
        code,
        message: message.into(),
        details: Some(details),
    }
}

/// A commit as replay needs it.
struct CommitInfo {
    id: String,
    parents: Vec<String>,
    author_name: String,
    author_email: String,
    author_date: String,
    message: String,
}

impl CommitInfo {
    fn subject(&self) -> String {
        self.message.lines().next().unwrap_or_default().to_string()
    }
}

enum Replayed {
    Done {
        tip: String,
        replayed: usize,
        dropped: usize,
        kept: usize,
    },
    Conflict {
        commit: String,
        subject: String,
        paths: Vec<String>,
    },
}

/// The current worktree and its stack.
pub struct Window {
    pub(crate) repo: HostRepo,
    /// The worktree's own git directory.
    pub git_dir: PathBuf,
    /// The git directory shared by all worktrees.
    pub common_dir: PathBuf,
    pub state: State,
    rules: Rules,
}

pub(crate) fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

pub(crate) fn state_path(git_dir: &Path) -> PathBuf {
    git_dir.join("agentvc").join("stack.json")
}

pub(crate) fn read_state(git_dir: &Path) -> Result<Option<State>> {
    let path = state_path(git_dir);
    match fs::read_to_string(&path) {
        Ok(s) => {
            let state: State = serde_json::from_str(&s).map_err(|e| {
                stack_err(
                    "bad_stack_state",
                    format!("{} is not valid: {e}", path.display()),
                )
            })?;
            Ok(Some(state))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).at(path),
    }
}

fn write_state(git_dir: &Path, state: &State) -> Result<()> {
    let path = state_path(git_dir);
    let dir = path.parent().expect("has a parent");
    fs::create_dir_all(dir).at(dir)?;
    let tmp = dir.join(format!("stack.json.{}.tmp", std::process::id()));
    let text = serde_json::to_string_pretty(state).expect("state serializes");
    fs::write(&tmp, text + "\n").at(&tmp)?;
    fs::rename(&tmp, &path).at(&path)?;
    Ok(())
}

/// Markers around the stack list `submit` keeps in each PR body.
pub const STACK_BEGIN: &str = "<!-- agentvc:stack:begin -->";
pub const STACK_END: &str = "<!-- agentvc:stack:end -->";

/// `body` with its stack section replaced by `section` (or added on top).
pub fn with_stack_section(body: &str, section: &str) -> String {
    if let (Some(a), Some(b)) = (body.find(STACK_BEGIN), body.find(STACK_END)) {
        if a < b {
            return format!("{}{section}{}", &body[..a], &body[b + STACK_END.len()..]);
        }
    }
    if body.trim().is_empty() {
        section.to_string()
    } else {
        format!("{section}\n\n{body}")
    }
}

/// Options for [`Window::submit`].
#[derive(Clone, Debug, Default)]
pub struct SubmitOptions {
    /// Only this branch and the ones below it. Default: the whole stack
    /// the current branch is in.
    pub branch: Option<String>,
    /// Every branch of this window.
    pub all: bool,
    /// Open new PRs as drafts.
    pub draft: bool,
    /// Overwrite remote branches even if they changed since our last push.
    pub force: bool,
    /// GitHub `owner/name`; defaults to the remote's.
    pub repo: Option<String>,
    /// Push only.
    pub no_pr: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct SubmittedPr {
    pub branch: String,
    pub number: u64,
    pub url: String,
    /// The PR's base branch: the parent, or the trunk.
    pub base: String,
    pub created: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct SubmitResult {
    pub repo: String,
    /// Branches pushed (the others were already up to date).
    pub pushed: Vec<String>,
    pub prs: Vec<SubmittedPr>,
}

/// A branch name from a commit message: `Fix the parser!` -> `fix-the-parser`.
pub fn slug(message: &str) -> String {
    let mut out = String::new();
    for c in message.lines().next().unwrap_or_default().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

impl Window {
    /// The worktree containing `cwd` and its stack (empty if none yet).
    pub fn open(cwd: &Path) -> Result<Window> {
        let probe = HostRepo {
            workdir: cwd.to_path_buf(),
        };
        let top = probe
            .git(&["rev-parse", "--show-toplevel"], None)
            .map_err(|_| Error::NotGitRepo(cwd.to_path_buf()))?;
        let repo = HostRepo::open(Path::new(&top))?;
        let git_dir = canon(Path::new(
            &repo.git(&["rev-parse", "--absolute-git-dir"], None)?,
        ));
        let common = repo.git(&["rev-parse", "--git-common-dir"], None)?;
        let common_dir = canon(&repo.workdir.join(common));
        let state = match read_state(&git_dir)? {
            Some(s) => s,
            None => {
                let (trunk, remote) = detect_trunk(&repo);
                State {
                    version: STATE_VERSION,
                    trunk,
                    remote,
                    branches: Vec::new(),
                    pending: None,
                }
            }
        };
        Ok(Window {
            repo,
            git_dir,
            common_dir,
            state,
            rules: Rules::new(),
        })
    }

    /// This worktree's directory.
    pub fn path(&self) -> &Path {
        &self.repo.workdir
    }

    pub fn save(&self) -> Result<()> {
        write_state(&self.git_dir, &self.state)
    }

    /// Set the trunk branch and remote (`agentvc stack init`).
    pub fn set_trunk(&mut self, trunk: Option<&str>, remote: Option<&str>) -> Result<()> {
        if let Some(t) = trunk {
            self.state.trunk = t.to_string();
        }
        if let Some(r) = remote {
            self.state.remote = r.to_string();
        }
        if self.base().is_none() {
            return Err(stack_err(
                "no_trunk",
                format!(
                    "neither {} nor refs/heads/{} exists",
                    self.base_ref(),
                    self.state.trunk
                ),
            ));
        }
        self.save()
    }

    // ---- refs and helpers ----

    /// The full ref stacks are based on: the remote trunk when it exists,
    /// else the local trunk branch (read, never moved).
    fn base_full_ref(&self) -> String {
        if !self.state.remote.is_empty() {
            let r = format!("refs/remotes/{}/{}", self.state.remote, self.state.trunk);
            if self.repo.rev(&r).is_some() {
                return r;
            }
        }
        format!("refs/heads/{}", self.state.trunk)
    }

    /// `origin/main`, or `main` without a remote trunk.
    pub fn base_ref(&self) -> String {
        let r = self.base_full_ref();
        r.strip_prefix("refs/remotes/")
            .or_else(|| r.strip_prefix("refs/heads/"))
            .unwrap_or(&r)
            .to_string()
    }

    pub fn base(&self) -> Option<String> {
        self.repo.rev(&self.base_full_ref())
    }

    fn require_base(&self) -> Result<String> {
        self.base().ok_or_else(|| {
            stack_err(
                "no_trunk",
                format!(
                    "the trunk {} does not exist; set it with `agentvc stack init --trunk <branch>`",
                    self.base_ref()
                ),
            )
        })
    }

    pub(crate) fn branch_tip(&self, name: &str) -> Option<String> {
        self.repo.rev(&format!("refs/heads/{name}"))
    }

    /// The commit a parent name stands for now.
    fn parent_tip(&self, parent: &str) -> Result<String> {
        if parent == self.state.trunk {
            return self.require_base();
        }
        self.branch_tip(parent).ok_or_else(|| {
            stack_err(
                "parent_missing",
                format!("parent branch {parent} no longer exists; re-parent its children with `agentvc stack adopt <branch> --parent <branch>`"),
            )
        })
    }

    /// `tip` already sits on `onto`, which moved forward from `parent_rev`.
    fn on_top(&self, parent_rev: &str, onto: &str, tip: &str) -> bool {
        self.repo.is_ancestor(parent_rev, onto) && self.repo.is_ancestor(onto, tip)
    }

    fn merge_base(&self, a: &str, b: &str) -> Option<String> {
        self.repo
            .git(&["merge-base", a, b], None)
            .ok()
            .filter(|s| !s.is_empty())
    }

    pub fn current_branch(&self) -> Option<String> {
        self.repo.checked_out_branch()
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.state.branches.iter().position(|b| b.name == name)
    }

    pub fn tracks(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    fn children_of(&self, name: &str) -> Vec<String> {
        self.state
            .branches
            .iter()
            .filter(|b| b.parent == name)
            .map(|b| b.name.clone())
            .collect()
    }

    /// Branches whose parent is not in this window: trunk or a foreign base.
    fn roots(&self) -> Vec<String> {
        self.state
            .branches
            .iter()
            .filter(|b| !self.tracks(&b.parent))
            .map(|b| b.name.clone())
            .collect()
    }

    /// Tracked branches depth-first, parents before children, with depth.
    fn ordered(&self) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        fn walk(
            w: &Window,
            name: &str,
            depth: usize,
            out: &mut Vec<(String, usize)>,
            seen: &mut HashSet<String>,
        ) {
            if !seen.insert(name.to_string()) {
                return;
            }
            out.push((name.to_string(), depth));
            for c in w.children_of(name) {
                walk(w, &c, depth + 1, out, seen);
            }
        }
        for r in self.roots() {
            walk(self, &r, 0, &mut out, &mut seen);
        }
        // A parent cycle (only from a hand-edited file) leaves branches
        // unvisited; list them last rather than lose them.
        for b in &self.state.branches {
            if !seen.contains(&b.name) {
                out.push((b.name.clone(), 0));
            }
        }
        out
    }

    /// `name` and everything stacked on it.
    pub fn upstack(&self, name: &str) -> HashSet<String> {
        let mut set = HashSet::new();
        let mut todo = vec![name.to_string()];
        while let Some(n) = todo.pop() {
            if set.insert(n.clone()) {
                todo.extend(self.children_of(&n));
            }
        }
        set
    }

    /// Worktree directory -> checked-out branch, for the other worktrees.
    fn checkouts_elsewhere(&self) -> HashMap<String, PathBuf> {
        let mut map = HashMap::new();
        let Ok(out) = self.repo.git(&["worktree", "list", "--porcelain"], None) else {
            return map;
        };
        let me = canon(&self.repo.workdir);
        let mut path: Option<PathBuf> = None;
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                if let Some(p) = &path {
                    if canon(p) != me {
                        map.insert(b.to_string(), p.clone());
                    }
                }
            }
        }
        map
    }

    /// The other worktrees' git dirs and directories.
    fn other_git_dirs(&self) -> Vec<(PathBuf, PathBuf)> {
        self.worktrees()
            .into_iter()
            .filter(|(g, _)| *g != self.git_dir)
            .collect()
    }

    /// Every worktree of the repo, this one included: (git dir, directory),
    /// the main worktree first. Worktrees whose directory is gone are left
    /// out.
    pub(crate) fn worktrees(&self) -> Vec<(PathBuf, PathBuf)> {
        let main_path = self
            .repo
            .git(&["worktree", "list", "--porcelain"], None)
            .ok()
            .and_then(|o| {
                o.lines()
                    .next()
                    .and_then(|l| l.strip_prefix("worktree "))
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| self.common_dir.parent().unwrap_or(&self.common_dir).into());
        let mut dirs = vec![(self.common_dir.clone(), main_path)];
        if let Ok(rd) = fs::read_dir(self.common_dir.join("worktrees")) {
            for e in rd.flatten() {
                let d = e.path();
                let path = fs::read_to_string(d.join("gitdir"))
                    .ok()
                    .map(|g| {
                        let g = PathBuf::from(g.trim());
                        g.parent().map(Path::to_path_buf).unwrap_or(g)
                    })
                    .unwrap_or_else(|| d.clone());
                dirs.push((canon(&d), path));
            }
        }
        dirs.retain(|(g, p)| *g == self.git_dir || p.exists());
        dirs
    }

    /// Other windows that have a stack, with their state.
    pub fn other_windows(&self) -> Vec<(PathBuf, State)> {
        self.other_git_dirs()
            .into_iter()
            .filter_map(|(g, p)| read_state(&g).ok().flatten().map(|s| (p, s)))
            .collect()
    }

    /// The other window tracking `branch`, if any.
    pub fn owner_elsewhere(&self, branch: &str) -> Option<PathBuf> {
        self.other_windows()
            .into_iter()
            .find(|(_, s)| s.branches.iter().any(|b| b.name == branch))
            .map(|(p, _)| p)
    }

    fn require_tracked(&self, name: &str) -> Result<usize> {
        if let Some(i) = self.find(name) {
            return Ok(i);
        }
        if let Some(p) = self.owner_elsewhere(name) {
            return Err(stack_err_with(
                "owned_by_other_window",
                format!(
                    "branch {name} belongs to the window at {}; run this there",
                    p.display()
                ),
                json!({ "branch": name, "window": p }),
            ));
        }
        Err(stack_err_with(
            "not_in_stack",
            format!("branch {name} is not in this window's stack; track it with `agentvc stack adopt {name}`"),
            json!({ "branch": name }),
        ))
    }

    fn require_current(&self) -> Result<String> {
        let cur = self.current_branch().ok_or_else(|| {
            stack_err(
                "not_in_stack",
                "HEAD is detached; check out a branch of this window's stack",
            )
        })?;
        self.require_tracked(&cur)?;
        Ok(cur)
    }

    pub(crate) fn rebase_in_progress(&self) -> bool {
        self.git_dir.join("rebase-merge").exists() || self.git_dir.join("rebase-apply").exists()
    }

    /// Finish a rebase left for the user by `keep_conflicts`, or stop if it
    /// is still running. Every changing command calls this first.
    pub fn settle(&mut self) -> Result<()> {
        let Some(p) = self.state.pending.clone() else {
            if self.rebase_in_progress() {
                return Err(stack_err(
                    "rebase_in_progress",
                    "a git rebase is in progress in this worktree; finish it with `git rebase --continue` or `--abort` first",
                ));
            }
            return Ok(());
        };
        if self.rebase_in_progress() {
            return Err(stack_err_with(
                "rebase_in_progress",
                format!(
                    "{} is being rebased; resolve the conflicts, `git add` the files and `git rebase --continue` (or `--abort`), then run `agentvc stack restack`",
                    p.branch
                ),
                json!({ "branch": p.branch }),
            ));
        }
        if let (Some(i), Some(tip)) = (self.find(&p.branch), self.branch_tip(&p.branch)) {
            if self.repo.is_ancestor(&p.onto, &tip) {
                self.state.branches[i].parent_rev = p.onto.clone();
            }
        }
        self.state.pending = None;
        self.save()
    }

    // ---- replay ----

    fn commits_between(&self, from: &str, to: &str) -> Result<Vec<CommitInfo>> {
        let out = self.repo.git_raw(&[
            "log",
            "--reverse",
            "--topo-order",
            "--date=raw",
            "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%ad%x1f%B%x1e",
            &format!("{from}..{to}"),
        ])?;
        let mut v = Vec::new();
        for rec in out.split('\x1e') {
            let rec = rec.trim_start_matches('\n');
            if rec.trim().is_empty() {
                continue;
            }
            let f: Vec<&str> = rec.splitn(6, '\x1f').collect();
            if f.len() < 6 {
                continue;
            }
            v.push(CommitInfo {
                id: f[0].to_string(),
                parents: f[1].split_whitespace().map(str::to_string).collect(),
                author_name: f[2].to_string(),
                author_email: f[3].to_string(),
                author_date: f[4].to_string(),
                message: f[5].to_string(),
            });
        }
        Ok(v)
    }

    fn tree_of(&self, commit: &str) -> Result<String> {
        self.repo
            .git(&["rev-parse", &format!("{commit}^{{tree}}")], None)
    }

    /// Cherry-pick `commits` onto `onto`, in memory.
    fn replay(&self, branch: &str, commits: &[CommitInfo], onto: &str) -> Result<Replayed> {
        let mut tip = onto.to_string();
        let (mut replayed, mut dropped, mut kept) = (0, 0, 0);
        for c in commits {
            if c.parents.len() != 1 {
                return Err(stack_err_with(
                    "merge_commit",
                    format!(
                        "{branch} contains merge commit {}; restack replays linear branches only (rebase it by hand once)",
                        &c.id[..12.min(c.id.len())]
                    ),
                    json!({ "branch": branch, "commit": c.id }),
                ));
            }
            if c.parents[0] == tip {
                // Already on the new parent: keep the commit as it is.
                tip = c.id.clone();
                kept += 1;
                continue;
            }
            let sides = Sides {
                ours: onto[..12.min(onto.len())].to_string(),
                theirs: format!("{branch} {}", &c.id[..12.min(c.id.len())]),
                theirs_first: false,
            };
            let m = merge_commits(
                &self.repo,
                Some(&c.parents[0]),
                &tip,
                &c.id,
                &self.rules,
                &sides,
            )?;
            if !m.conflicts.is_empty() {
                return Ok(Replayed::Conflict {
                    commit: c.id.clone(),
                    subject: c.subject(),
                    paths: m.conflicts,
                });
            }
            if m.tree == self.tree_of(&tip)? {
                dropped += 1;
                continue;
            }
            let new = self.repo.git_env(
                &["commit-tree", &m.tree, "-p", &tip, "-F", "-"],
                Some(&c.message),
                &[
                    ("GIT_AUTHOR_NAME", c.author_name.as_str()),
                    ("GIT_AUTHOR_EMAIL", c.author_email.as_str()),
                    ("GIT_AUTHOR_DATE", c.author_date.as_str()),
                ],
            )?;
            tip = new;
            replayed += 1;
        }
        Ok(Replayed::Done {
            tip,
            replayed,
            dropped,
            kept,
        })
    }

    /// Where `b`'s own commits start: its recorded parent commit, or the
    /// merge base with `onto` if history was rewritten outside agentvc.
    fn replay_from(&self, b: &Tracked, tip: &str, onto: &str) -> String {
        if self.repo.is_ancestor(&b.parent_rev, tip) {
            b.parent_rev.clone()
        } else {
            self.merge_base(tip, onto)
                .unwrap_or_else(|| onto.to_string())
        }
    }

    // ---- restack ----

    /// Replay every tracked branch (or only `scope`) onto its parent's
    /// current tip, then move all refs at once. On a conflict nothing
    /// changes, unless `keep_conflicts`: then the branches before it are
    /// restacked and the conflicting one is left in a `git rebase` in this
    /// worktree.
    pub fn restack(
        &mut self,
        scope: Option<&HashSet<String>>,
        keep_conflicts: bool,
    ) -> Result<RestackResult> {
        self.settle()?;
        let mut result = RestackResult::default();
        let mut new_tips: HashMap<String, String> = HashMap::new();
        let mut new_parent_revs: Vec<(String, String)> = Vec::new();
        let mut stop: Option<(StackConflict, String)> = None;
        let mut skip: HashSet<String> = HashSet::new();
        for (name, _) in self.ordered() {
            let b = self.state.branches[self.find(&name).expect("tracked")].clone();
            let old = self.branch_tip(&name).ok_or_else(|| {
                stack_err_with(
                    "branch_missing",
                    format!(
                        "branch {name} was deleted; drop it with `agentvc stack untrack {name}`"
                    ),
                    json!({ "branch": name }),
                )
            })?;
            let in_scope = scope.is_none_or(|s| s.contains(&name));
            if !in_scope || skip.contains(&b.parent) {
                new_tips.insert(name.clone(), old);
                if skip.contains(&b.parent) {
                    skip.insert(name);
                }
                continue;
            }
            let onto = match new_tips.get(&b.parent) {
                Some(t) if self.tracks(&b.parent) => t.clone(),
                _ => self.parent_tip(&b.parent)?,
            };
            // Up to date: on its recorded parent commit, or the parent only
            // moved forward and the branch already contains its new tip.
            if onto == b.parent_rev || self.on_top(&b.parent_rev, &onto, &old) {
                if onto != b.parent_rev {
                    new_parent_revs.push((name.clone(), onto.clone()));
                }
                new_tips.insert(name.clone(), old);
                result.unchanged.push(name);
                continue;
            }
            let from = self.replay_from(&b, &old, &onto);
            let commits = self.commits_between(&from, &old)?;
            match self.replay(&name, &commits, &onto)? {
                Replayed::Done {
                    tip,
                    replayed,
                    dropped,
                    ..
                } => {
                    new_parent_revs.push((name.clone(), onto.clone()));
                    if tip != old {
                        result.updated.push(BranchUpdate {
                            branch: name.clone(),
                            old: old.clone(),
                            new: tip.clone(),
                            onto: onto.clone(),
                            commits: replayed,
                            dropped,
                        });
                    } else {
                        result.unchanged.push(name.clone());
                    }
                    new_tips.insert(name, tip);
                }
                Replayed::Conflict {
                    commit,
                    subject,
                    paths,
                } => {
                    let c = StackConflict {
                        branch: name.clone(),
                        commit,
                        subject,
                        onto: onto.clone(),
                        paths,
                    };
                    if !keep_conflicts {
                        let msg = format!(
                            "restacking {} onto {} conflicts in {} (commit \"{}\"); nothing was changed. Run `agentvc stack restack --keep-conflicts` to resolve it in this worktree",
                            c.branch,
                            &c.onto[..12.min(c.onto.len())],
                            c.paths.join(", "),
                            c.subject
                        );
                        return Err(stack_err_with(
                            "conflict",
                            msg,
                            serde_json::to_value(&c).expect("serializes"),
                        ));
                    }
                    stop = Some((c, from));
                    new_tips.insert(name.clone(), old);
                    skip.insert(name);
                }
            }
        }
        self.apply(&result.updated)?;
        for (name, rev) in new_parent_revs {
            if let Some(i) = self.find(&name) {
                self.state.branches[i].parent_rev = rev;
            }
        }
        self.save()?;
        if let Some((c, from)) = stop {
            self.rebase_in_worktree(&c, &from)?;
            result.conflict = Some(c);
            result.kept_conflicts = true;
        }
        Ok(result)
    }

    /// Move the refs of `updates` in one transaction. The branch checked out
    /// here moves with `git reset --keep`, so local edits stay.
    fn apply(&self, updates: &[BranchUpdate]) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let elsewhere = self.checkouts_elsewhere();
        for u in updates {
            if let Some(p) = elsewhere.get(&u.branch) {
                return Err(stack_err_with(
                    "checked_out_elsewhere",
                    format!(
                        "branch {} is checked out in {}; nothing was changed. Switch that worktree to another branch first",
                        u.branch,
                        p.display()
                    ),
                    json!({ "branch": u.branch, "worktree": p }),
                ));
            }
        }
        let current = self.current_branch();
        let mut moved_current: Option<&BranchUpdate> = None;
        if let Some(u) = updates.iter().find(|u| Some(&u.branch) == current.as_ref()) {
            self.repo.git(&["reset", "--keep", &u.new], None)?;
            moved_current = Some(u);
        }
        let mut tx = String::new();
        for u in updates {
            if Some(&u.branch) != current.as_ref() {
                tx += &format!("update refs/heads/{} {} {}\n", u.branch, u.new, u.old);
            }
        }
        if tx.is_empty() {
            return Ok(());
        }
        let r = self.repo.git(
            &["update-ref", "-m", "agentvc stack restack", "--stdin"],
            Some(&tx),
        );
        if let Err(e) = r {
            if let Some(u) = moved_current {
                let _ = self.repo.git(&["reset", "--keep", &u.old], None);
            }
            return Err(e);
        }
        Ok(())
    }

    fn rebase_in_worktree(&mut self, c: &StackConflict, from: &str) -> Result<()> {
        if self.current_branch().as_deref() != Some(c.branch.as_str()) {
            self.repo.git(&["switch", &c.branch], None)?;
        }
        self.state.pending = Some(Pending {
            branch: c.branch.clone(),
            onto: c.onto.clone(),
        });
        self.save()?;
        // Expected to stop on the conflict; git leaves the state in the
        // worktree for the editor.
        let r = self
            .repo
            .git(&["rebase", "--onto", &c.onto, from, &c.branch], None);
        if r.is_ok() || self.rebase_in_progress() {
            if r.is_ok() {
                self.settle()?;
            }
            return Ok(());
        }
        self.state.pending = None;
        self.save()?;
        r.map(|_| ())
    }

    // ---- sync ----

    /// Whether `b` is merged into `base`: its tip is in the base, or its
    /// commits replay onto the base as no change (squash or rebase merges).
    fn is_merged(&self, b: &Tracked, base: &str) -> Result<bool> {
        let Some(tip) = self.branch_tip(&b.name) else {
            return Ok(false);
        };
        if self.repo.is_ancestor(&tip, base) {
            // A branch with no commits of its own is new, not merged.
            return Ok(tip != b.parent_rev);
        }
        let from = self.replay_from(b, &tip, base);
        let commits = self.commits_between(&from, &tip)?;
        if commits.is_empty() {
            return Ok(false);
        }
        Ok(match self.replay(&b.name, &commits, base) {
            Ok(Replayed::Done { replayed, kept, .. }) => replayed == 0 && kept == 0,
            _ => false,
        })
    }

    /// Fetch the trunk, drop branches merged into it (bottom up), then
    /// restack everything onto it.
    pub fn sync(&mut self, fetch: bool, delete: bool, keep_conflicts: bool) -> Result<SyncResult> {
        // PRs merged on GitHub count too, when a token is at hand; offline,
        // the history check alone decides.
        let api = if self.state.branches.iter().any(|b| b.pr.is_some()) {
            crate::github::token().ok().map(Api::new)
        } else {
            None
        };
        self.sync_with(fetch, delete, keep_conflicts, api.as_ref())
    }

    /// Branches whose PR GitHub reports merged, and whose local tip is what
    /// was pushed (so no local work is lost by dropping them).
    /// Branches whose PR GitHub reports merged (and whose local branch is
    /// what was pushed); records every PR's state on the way.
    fn merged_on_github(&mut self, api: &Api) -> HashSet<String> {
        let mut out = HashSet::new();
        let Ok(repo) = crate::github::repo_for(&self.repo.workdir, &self.state.remote, None) else {
            return out;
        };
        for i in 0..self.state.branches.len() {
            let b = self.state.branches[i].clone();
            let Some(n) = b.pr else { continue };
            let Ok(v) = api.call("GET", &format!("/repos/{repo}/pulls/{n}"), None) else {
                continue;
            };
            self.state.branches[i].pr_state = Some(pr_state_of(&v));
            if v["merged"].as_bool() != Some(true) {
                continue;
            }
            let Some(tip) = self.branch_tip(&b.name) else {
                continue;
            };
            let pushed = self
                .repo
                .rev(&format!("refs/remotes/{}/{}", self.state.remote, b.name));
            let head = v["head"]["sha"].as_str().unwrap_or_default();
            if tip == head || pushed.is_some_and(|p| self.repo.is_ancestor(&tip, &p)) {
                out.insert(b.name.clone());
            }
        }
        out
    }

    pub(crate) fn sync_with(
        &mut self,
        fetch: bool,
        delete: bool,
        keep_conflicts: bool,
        api: Option<&Api>,
    ) -> Result<SyncResult> {
        self.settle()?;
        let mut fetched = false;
        if fetch && !self.state.remote.is_empty() {
            let spec = format!(
                "+refs/heads/{t}:refs/remotes/{r}/{t}",
                t = self.state.trunk,
                r = self.state.remote
            );
            self.repo
                .git(&["fetch", "--quiet", &self.state.remote, &spec], None)?;
            fetched = true;
        }
        let base = self.require_base()?;
        let mut merged: Vec<String> = Vec::new();
        let merged_prs = match api {
            Some(a) => self.merged_on_github(a),
            None => HashSet::new(),
        };
        loop {
            let mut found = None;
            for b in &self.state.branches {
                if b.parent == self.state.trunk
                    && (merged_prs.contains(&b.name) || self.is_merged(b, &base)?)
                {
                    found = Some(b.clone());
                    break;
                }
            }
            let Some(b) = found else { break };
            // Children keep their parent_rev (a commit of the merged branch),
            // so restack replays only their own commits onto the base.
            for c in self.state.branches.iter_mut() {
                if c.parent == b.name {
                    c.parent = self.state.trunk.clone();
                }
            }
            self.state.branches.retain(|x| x.name != b.name);
            merged.push(b.name);
        }
        let mut deleted = Vec::new();
        let mut switched_to = None;
        if !merged.is_empty() {
            let current = self.current_branch();
            if let Some(cur) = current.filter(|c| merged.contains(c)) {
                let target = self.roots().into_iter().next();
                match &target {
                    Some(t) => self.repo.git(&["switch", t], None)?,
                    None => self.repo.git(&["switch", "--detach", &base], None)?,
                };
                switched_to = Some(target.unwrap_or_else(|| self.base_ref()));
                let _ = cur;
            }
            if delete {
                let elsewhere = self.checkouts_elsewhere();
                for m in &merged {
                    if !elsewhere.contains_key(m)
                        && self.current_branch().as_deref() != Some(m.as_str())
                        && self.repo.git(&["branch", "-D", m], None).is_ok()
                    {
                        deleted.push(m.clone());
                    }
                }
            }
            self.save()?;
        }
        let restack = match self.restack(None, keep_conflicts) {
            Ok(r) => r,
            Err(Error::Stack {
                code,
                message,
                details,
            }) if !merged.is_empty() => {
                return Err(Error::Stack {
                    code,
                    message: format!("removed merged {}; {message}", merged.join(", ")),
                    details,
                })
            }
            Err(e) => return Err(e),
        };
        Ok(SyncResult {
            fetched,
            base,
            merged,
            deleted,
            switched_to,
            restack,
        })
    }

    // ---- creating and editing ----

    /// Create a branch on top of the current one (or on the trunk) and
    /// commit the staged changes to it when `message` is given.
    pub fn create(
        &mut self,
        name: Option<&str>,
        message: Option<&str>,
        all: bool,
    ) -> Result<(Tracked, Option<String>)> {
        self.settle()?;
        let name = match (name, message) {
            (Some(n), _) => n.to_string(),
            (None, Some(m)) if !slug(m).is_empty() => slug(m),
            _ => {
                return Err(stack_err(
                    "bad_branch",
                    "give a branch name, or a message (-m) to name it after",
                ))
            }
        };
        if self
            .repo
            .git(&["check-ref-format", "--branch", &name], None)
            .is_err()
            || name.starts_with('-')
        {
            return Err(Error::BadBranch(name));
        }
        if self.branch_tip(&name).is_some() {
            return Err(Error::BranchExists(name));
        }
        let head = self
            .repo
            .rev("HEAD")
            .ok_or_else(|| stack_err("no_commits", "this repository has no commits yet"))?;
        let parent = match self.current_branch() {
            Some(c) if c != self.state.trunk => c,
            _ => self.state.trunk.clone(),
        };
        let parent_rev = if parent == self.state.trunk {
            let base = self.require_base()?;
            self.merge_base(&head, &base).unwrap_or(head.clone())
        } else {
            head.clone()
        };
        self.repo.git(&["switch", "-c", &name], None)?;
        let t = Tracked {
            name: name.clone(),
            parent,
            parent_rev,
            pr: None,
            pr_state: None,
        };
        self.state.branches.push(t.clone());
        self.save()?;
        let mut commit = None;
        if let Some(m) = message {
            if all {
                self.repo.git(&["add", "-A"], None)?;
            }
            if self
                .repo
                .git(&["diff", "--cached", "--quiet"], None)
                .is_err()
            {
                self.repo.git(&["commit", "--quiet", "-m", m], None)?;
                commit = self.repo.rev("HEAD");
            }
        }
        Ok((t, commit))
    }

    /// Amend the current branch's last commit (or add a commit with
    /// `new_commit`), then restack the branches above it.
    pub fn modify(
        &mut self,
        message: Option<&str>,
        all: bool,
        new_commit: bool,
        keep_conflicts: bool,
    ) -> Result<(String, RestackResult)> {
        self.settle()?;
        let cur = self.require_current()?;
        if all {
            self.repo.git(&["add", "-A"], None)?;
        }
        let i = self.find(&cur).expect("tracked");
        let tip = self.branch_tip(&cur).expect("checked out");
        let own_commits = tip != self.state.branches[i].parent_rev
            && !self
                .repo
                .is_ancestor(&tip, &self.state.branches[i].parent_rev);
        if new_commit || !own_commits {
            let m = message.ok_or_else(|| {
                stack_err(
                    "message_required",
                    if new_commit {
                        "a new commit needs a message (-m)"
                    } else {
                        "this branch has no commit of its own yet; give a message (-m) for its first commit"
                    },
                )
            })?;
            self.repo.git(&["commit", "--quiet", "-m", m], None)?;
        } else {
            match message {
                Some(m) => self
                    .repo
                    .git(&["commit", "--quiet", "--amend", "-m", m], None)?,
                None => self
                    .repo
                    .git(&["commit", "--quiet", "--amend", "--no-edit"], None)?,
            };
        }
        let commit = self.repo.rev("HEAD").expect("just committed");
        let mut scope = self.upstack(&cur);
        scope.remove(&cur);
        let r = self.restack(Some(&scope), keep_conflicts)?;
        Ok((commit, r))
    }

    /// Track an existing branch (and, with `chain`, the untracked branches
    /// below it). The parent comes from `parent`, Graphite's metadata, or
    /// the nearest local branch the branch contains.
    pub fn adopt(
        &mut self,
        branch: &str,
        parent: Option<&str>,
        chain: bool,
    ) -> Result<Vec<Tracked>> {
        self.settle()?;
        if branch == self.state.trunk {
            return Err(stack_err("bad_branch", "the trunk cannot be tracked"));
        }
        let base = self.require_base()?;
        let mut added: Vec<Tracked> = Vec::new();
        let mut next = Some((branch.to_string(), parent.map(str::to_string)));
        while let Some((name, explicit)) = next.take() {
            let tip = self.branch_tip(&name).ok_or_else(|| Error::NotFound {
                kind: "branch",
                id: name.clone(),
            })?;
            if let Some(p) = self.owner_elsewhere(&name) {
                if name == branch {
                    return Err(stack_err_with(
                        "owned_by_other_window",
                        format!("branch {name} belongs to the window at {}; hand it over from there with `agentvc stack give`", p.display()),
                        json!({ "branch": name, "window": p }),
                    ));
                }
                break;
            }
            if let Some(i) = self.find(&name) {
                if explicit.is_none() {
                    break;
                }
                self.state.branches.remove(i);
            }
            let gt = graphite_parent(&self.repo, &name);
            let parent = match explicit
                .clone()
                .or_else(|| gt.as_ref().map(|g| g.0.clone()))
            {
                Some(p) => p,
                None => self.nearest_parent(&name, &tip, &base)?,
            };
            let parent_tip = self.parent_tip(&parent)?;
            let parent_rev = match gt.and_then(|g| g.1) {
                Some(r) if explicit.is_none() && self.repo.is_ancestor(&r, &tip) => r,
                _ if self.repo.is_ancestor(&parent_tip, &tip) => parent_tip,
                _ => self.merge_base(&tip, &parent_tip).unwrap_or(parent_tip),
            };
            let t = Tracked {
                name: name.clone(),
                parent: parent.clone(),
                parent_rev,
                pr: None,
                pr_state: None,
            };
            added.push(t);
            if chain && parent != self.state.trunk && !self.tracks(&parent) {
                next = Some((parent, None));
            }
        }
        // Parents first, so the file reads bottom-up.
        for t in added.iter().rev() {
            self.state.branches.push(t.clone());
        }
        self.save()?;
        added.reverse();
        Ok(added)
    }

    /// The local branch nearest below `tip`, or the trunk.
    fn nearest_parent(&self, name: &str, tip: &str, base: &str) -> Result<String> {
        let list = |args: &[&str]| -> Result<HashSet<String>> {
            Ok(self
                .repo
                .git(args, None)?
                .lines()
                .map(str::to_string)
                .collect())
        };
        let below = list(&[
            "for-each-ref",
            "--format=%(refname:short)",
            "--merged",
            tip,
            "refs/heads",
        ])?;
        let in_base = list(&[
            "for-each-ref",
            "--format=%(refname:short)",
            "--merged",
            base,
            "refs/heads",
        ])?;
        let mut cands: Vec<(String, String)> = below
            .into_iter()
            .filter(|b| b != name && *b != self.state.trunk && !in_base.contains(b))
            .filter_map(|b| self.branch_tip(&b).map(|t| (b, t)))
            .filter(|(_, t)| t != tip)
            .collect();
        cands.sort();
        let nearest = cands
            .iter()
            .find(|(_, t)| {
                cands
                    .iter()
                    .all(|(_, o)| o == t || self.repo.is_ancestor(o, t))
            })
            .map(|(b, _)| b.clone());
        Ok(nearest.unwrap_or_else(|| self.state.trunk.clone()))
    }

    /// Stop tracking `branch`; its children sit on its parent and keep its
    /// commits. The git branch stays.
    pub fn untrack(&mut self, branch: &str) -> Result<Tracked> {
        self.settle()?;
        let i = self.require_tracked(branch)?;
        let t = self.state.branches.remove(i);
        for c in self.state.branches.iter_mut() {
            if c.parent == t.name {
                c.parent = t.parent.clone();
                c.parent_rev = t.parent_rev.clone();
            }
        }
        if self
            .state
            .pending
            .as_ref()
            .is_some_and(|p| p.branch == t.name)
        {
            self.state.pending = None;
        }
        self.save()?;
        Ok(t)
    }

    /// Put `branch` (and what is stacked on it) on `onto`: another branch,
    /// or the trunk. Only the branch's own commits move; then it and its
    /// upstack are restacked.
    pub fn move_onto(
        &mut self,
        branch: &str,
        onto: &str,
        keep_conflicts: bool,
    ) -> Result<RestackResult> {
        self.settle()?;
        let i = self.require_tracked(branch)?;
        let onto = if onto == self.base_ref() {
            self.state.trunk.clone()
        } else {
            onto.to_string()
        };
        let up = self.upstack(branch);
        if up.contains(&onto) {
            return Err(stack_err_with(
                "bad_move",
                format!("{onto} is {branch} or stacked on it"),
                json!({ "branch": branch, "onto": onto }),
            ));
        }
        self.parent_tip(&onto)?;
        if let Some(p) = (onto != self.state.trunk && !self.tracks(&onto))
            .then(|| self.owner_elsewhere(&onto))
            .flatten()
        {
            let _ = p; // a read-only base in another window is fine
        }
        let old = self.state.branches[i].parent.clone();
        self.state.branches[i].parent = onto.clone();
        match self.restack(Some(&up), keep_conflicts) {
            Ok(r) => Ok(r),
            Err(e) => {
                // Nothing moved: put the parent back.
                if let Some(i) = self.find(branch) {
                    self.state.branches[i].parent = old;
                    let _ = self.save();
                }
                Err(e)
            }
        }
    }

    /// Rename a tracked branch; branches on it follow. Its PR (GitHub
    /// cannot rename a PR's branch) is forgotten: the next submit opens one
    /// for the new name.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<Tracked> {
        self.settle()?;
        let i = self.require_tracked(old)?;
        if self
            .repo
            .git(&["check-ref-format", "--branch", new], None)
            .is_err()
            || new.starts_with('-')
        {
            return Err(Error::BadBranch(new.to_string()));
        }
        if self.branch_tip(new).is_some() {
            return Err(Error::BranchExists(new.to_string()));
        }
        if let Some(p) = self.checkouts_elsewhere().get(old) {
            return Err(stack_err_with(
                "checked_out_elsewhere",
                format!("branch {old} is checked out in {}", p.display()),
                json!({ "branch": old, "worktree": p }),
            ));
        }
        self.repo.git(&["branch", "-m", old, new], None)?;
        self.state.branches[i].name = new.to_string();
        self.state.branches[i].pr = None;
        self.state.branches[i].pr_state = None;
        for c in self.state.branches.iter_mut() {
            if c.parent == old {
                c.parent = new.to_string();
            }
        }
        self.save()?;
        Ok(self.state.branches[i].clone())
    }

    /// Delete a tracked branch and its commits from the stack: the branches
    /// on it move to its parent without its commits.
    pub fn delete(&mut self, branch: &str, keep_conflicts: bool) -> Result<RestackResult> {
        self.settle()?;
        let i = self.require_tracked(branch)?;
        if let Some(p) = self.checkouts_elsewhere().get(branch) {
            return Err(stack_err_with(
                "checked_out_elsewhere",
                format!("branch {branch} is checked out in {}", p.display()),
                json!({ "branch": branch, "worktree": p }),
            ));
        }
        let t = self.state.branches[i].clone();
        let kids = self.children_of(branch);
        if self.current_branch().as_deref() == Some(branch) {
            if self.tracks(&t.parent) {
                self.repo.git(&["switch", "--quiet", &t.parent], None)?;
            } else {
                let at = self.parent_tip(&t.parent)?;
                self.repo
                    .git(&["switch", "--quiet", "--detach", &at], None)?;
            }
        }
        // Children keep parent_rev (a commit of the deleted branch), so
        // restack replays only their own commits.
        let mut scope = HashSet::new();
        for k in &kids {
            scope.extend(self.upstack(k));
        }
        for c in self.state.branches.iter_mut() {
            if c.parent == branch {
                c.parent = t.parent.clone();
            }
        }
        self.state.branches.remove(i);
        self.save()?;
        self.repo.git(&["branch", "-D", branch], None)?;
        if scope.is_empty() {
            return Ok(RestackResult::default());
        }
        self.restack(Some(&scope), keep_conflicts)
    }

    /// Squash the current branch's commits into one (the first commit's
    /// message, or `message`), then restack the branches above it.
    pub fn squash(
        &mut self,
        message: Option<&str>,
        keep_conflicts: bool,
    ) -> Result<(String, RestackResult)> {
        self.settle()?;
        let cur = self.require_current()?;
        let b = self.state.branches[self.find(&cur).expect("tracked")].clone();
        let tip = self.branch_tip(&cur).expect("checked out");
        let commits = self.commits_between(&b.parent_rev, &tip)?;
        if commits.len() <= 1 && message.is_none() {
            return Ok((tip, RestackResult::default()));
        }
        let first = commits.first().ok_or_else(|| {
            stack_err(
                "message_required",
                format!("{cur} has no commits to squash"),
            )
        })?;
        let tree = self.tree_of(&tip)?;
        let msg = message
            .map(str::to_string)
            .unwrap_or_else(|| first.message.clone());
        let new = self.repo.git_env(
            &["commit-tree", &tree, "-p", &b.parent_rev, "-F", "-"],
            Some(&msg),
            &[
                ("GIT_AUTHOR_NAME", first.author_name.as_str()),
                ("GIT_AUTHOR_EMAIL", first.author_email.as_str()),
                ("GIT_AUTHOR_DATE", first.author_date.as_str()),
            ],
        )?;
        self.repo.git(&["reset", "--keep", &new], None)?;
        let mut scope = self.upstack(&cur);
        scope.remove(&cur);
        let r = self.restack(Some(&scope), keep_conflicts)?;
        Ok((new, r))
    }

    // ---- navigation ----

    fn switch(&self, branch: &str) -> Result<String> {
        if let Some(p) = self.checkouts_elsewhere().get(branch) {
            return Err(stack_err_with(
                "checked_out_elsewhere",
                format!("branch {branch} is checked out in {}", p.display()),
                json!({ "branch": branch, "worktree": p }),
            ));
        }
        self.repo.git(&["switch", "--quiet", branch], None)?;
        Ok(branch.to_string())
    }

    fn one_child(&self, of: &str, list: Vec<String>) -> Result<String> {
        match list.len() {
            0 => Err(stack_err(
                "at_top",
                format!("{of} has nothing stacked on it"),
            )),
            1 => Ok(list.into_iter().next().expect("one")),
            _ => Err(stack_err_with(
                "ambiguous",
                format!(
                    "{of} has several branches on it: {}; switch with `git switch <branch>`",
                    list.join(", ")
                ),
                json!({ "branches": list }),
            )),
        }
    }

    /// Check out the branch stacked on the current one (from the trunk or a
    /// detached HEAD: this window's bottom branch).
    pub fn up(&self) -> Result<String> {
        let cur = self.current_branch();
        let (of, kids) = match &cur {
            Some(c) if self.tracks(c) => (c.clone(), self.children_of(c)),
            _ => ("the trunk".to_string(), self.roots()),
        };
        let next = self.one_child(&of, kids)?;
        self.switch(&next)
    }

    pub fn down(&self) -> Result<String> {
        let cur = self.require_current()?;
        let parent = self.state.branches[self.find(&cur).expect("tracked")]
            .parent
            .clone();
        if !self.tracks(&parent) {
            return Err(stack_err_with(
                "at_bottom",
                format!(
                    "{cur} is the bottom of this window's stack (it sits on {}); `git switch --detach {}` to look at it",
                    if parent == self.state.trunk { self.base_ref() } else { parent.clone() },
                    if parent == self.state.trunk { self.base_ref() } else { parent.clone() }
                ),
                json!({ "parent": parent }),
            ));
        }
        self.switch(&parent)
    }

    pub fn top(&self) -> Result<String> {
        let mut at = match self.current_branch() {
            Some(c) if self.tracks(&c) => c,
            _ => self.one_child("the trunk", self.roots())?,
        };
        loop {
            let kids = self.children_of(&at);
            if kids.is_empty() {
                break;
            }
            at = self.one_child(&at, kids)?;
        }
        self.switch(&at)
    }

    pub fn bottom(&self) -> Result<String> {
        let mut at = self.require_current()?;
        loop {
            let parent = self.state.branches[self.find(&at).expect("tracked")]
                .parent
                .clone();
            if !self.tracks(&parent) {
                break;
            }
            at = parent;
        }
        self.switch(&at)
    }

    // ---- submit ----

    /// The branches `submit` works on, parents first.
    fn submit_set(&self, opts: &SubmitOptions) -> Result<Vec<String>> {
        let set: HashSet<String> = if opts.all {
            self.state.branches.iter().map(|b| b.name.clone()).collect()
        } else {
            let start = match &opts.branch {
                Some(b) => {
                    self.require_tracked(b)?;
                    b.clone()
                }
                None => self.require_current()?,
            };
            if opts.branch.is_some() {
                // The branch and what it sits on.
                let mut set = HashSet::new();
                let mut at = start;
                loop {
                    set.insert(at.clone());
                    let parent = self.state.branches[self.find(&at).expect("tracked")]
                        .parent
                        .clone();
                    if !self.tracks(&parent) {
                        break;
                    }
                    at = parent;
                }
                set
            } else {
                // The whole stack the current branch is in.
                let mut root = start;
                loop {
                    let parent = self.state.branches[self.find(&root).expect("tracked")]
                        .parent
                        .clone();
                    if !self.tracks(&parent) {
                        break;
                    }
                    root = parent;
                }
                self.upstack(&root)
            }
        };
        Ok(self
            .ordered()
            .into_iter()
            .map(|(n, _)| n)
            .filter(|n| set.contains(n))
            .collect())
    }

    /// Push this window's branches and open or update one PR per branch,
    /// based on its parent branch (the trunk for the bottom one), with the
    /// stack listed in every PR body.
    pub fn submit(&mut self, opts: &SubmitOptions) -> Result<SubmitResult> {
        let api = if opts.no_pr {
            None
        } else {
            Some(Api::new(crate::github::token()?))
        };
        self.submit_with(opts, api.as_ref())
    }

    pub(crate) fn submit_with(
        &mut self,
        opts: &SubmitOptions,
        api: Option<&Api>,
    ) -> Result<SubmitResult> {
        self.settle()?;
        if self.state.remote.is_empty() {
            return Err(stack_err(
                "no_remote",
                "this repository has no remote to push to",
            ));
        }
        let remote = self.state.remote.clone();
        let names = self.submit_set(opts)?;
        let stale: Vec<String> = names
            .iter()
            .filter(|n| {
                let b = &self.state.branches[self.find(n).expect("tracked")];
                match (self.parent_tip(&b.parent).ok(), self.branch_tip(n)) {
                    (Some(p), Some(t)) => p != b.parent_rev && !self.on_top(&b.parent_rev, &p, &t),
                    _ => true,
                }
            })
            .cloned()
            .collect();
        if !stale.is_empty() {
            return Err(stack_err_with(
                "needs_restack",
                format!(
                    "{} need{} a restack first (`agentvc stack restack`); nothing was pushed",
                    stale.join(", "),
                    if stale.len() == 1 { "s" } else { "" }
                ),
                json!({ "branches": stale }),
            ));
        }
        // A parent from another window must be on the remote to be a PR base.
        for n in &names {
            let parent = &self.state.branches[self.find(n).expect("tracked")].parent;
            if parent != &self.state.trunk && !self.tracks(parent) {
                let found = self
                    .repo
                    .git(&["ls-remote", "--heads", &remote, parent], None)
                    .unwrap_or_default();
                if found.is_empty() {
                    return Err(stack_err_with(
                        "parent_not_pushed",
                        format!("{n} sits on {parent}, which is not on {remote} yet; submit it from its window first"),
                        json!({ "branch": n, "parent": parent }),
                    ));
                }
            }
        }
        // Push what changed, all or nothing, with a lease on what we last
        // saw on the remote so nobody else's push is overwritten.
        let mut pushed = Vec::new();
        let mut args: Vec<String> = vec!["push".into(), "--quiet".into(), "--atomic".into()];
        let mut specs = Vec::new();
        for n in &names {
            let tip = self.branch_tip(n).expect("checked");
            let tracking = self.repo.rev(&format!("refs/remotes/{remote}/{n}"));
            if tracking.as_deref() == Some(tip.as_str()) {
                continue;
            }
            if opts.force {
                specs.push(format!("+refs/heads/{n}:refs/heads/{n}"));
            } else {
                args.push(format!(
                    "--force-with-lease=refs/heads/{n}:{}",
                    tracking.unwrap_or_default()
                ));
                specs.push(format!("refs/heads/{n}:refs/heads/{n}"));
            }
            pushed.push(n.clone());
        }
        if !specs.is_empty() {
            args.push(remote.clone());
            args.extend(specs);
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            if let Err(e) = self.repo.git(&refs, None) {
                let stderr = match &e {
                    Error::Git { stderr, .. } => stderr.clone(),
                    _ => String::new(),
                };
                if stderr.contains("stale info") || stderr.contains("rejected") {
                    return Err(stack_err_with(
                        "push_rejected",
                        format!("{remote} has changes on these branches that this window has not seen (another push, or the restack Action); nothing was pushed. Look with `git fetch {remote}` and `git log`, then `agentvc stack submit --force` to overwrite: {stderr}"),
                        json!({ "branches": pushed }),
                    ));
                }
                return Err(crate::github::push_error(e, &remote, &pushed.join(" ")));
            }
        }
        let mut result = SubmitResult {
            repo: String::new(),
            pushed,
            prs: Vec::new(),
        };
        let Some(api) = api else {
            return Ok(result);
        };
        let repo = crate::github::repo_for(&self.repo.workdir, &remote, opts.repo.as_deref())?;
        let owner = repo.split('/').next().unwrap_or_default().to_string();
        result.repo = repo.clone();
        let mut bodies: Vec<(u64, String)> = Vec::new();
        for n in &names {
            let i = self.find(n).expect("tracked");
            let b = self.state.branches[i].clone();
            let base_branch = b.parent.clone();
            let mut existing: Option<serde_json::Value> = None;
            if let Some(pr) = b.pr {
                if let Ok(v) = api.call("GET", &format!("/repos/{repo}/pulls/{pr}"), None) {
                    if v["state"] == "open" {
                        existing = Some(v);
                    }
                }
            }
            if existing.is_none() {
                let open = api.call(
                    "GET",
                    &format!("/repos/{repo}/pulls?state=open&head={owner}:{n}"),
                    None,
                )?;
                existing = open.as_array().and_then(|a| a.first().cloned());
            }
            let (v, created) = match existing {
                Some(v) => {
                    let number = v["number"].as_u64().unwrap_or(0);
                    let v = if v["base"]["ref"].as_str() != Some(base_branch.as_str()) {
                        api.call(
                            "PATCH",
                            &format!("/repos/{repo}/pulls/{number}"),
                            Some(json!({ "base": base_branch })),
                        )?
                    } else {
                        v
                    };
                    (v, false)
                }
                None => {
                    let tip = self.branch_tip(n).expect("checked");
                    let commits = self.commits_between(&b.parent_rev, &tip)?;
                    let title = commits
                        .first()
                        .map(|c| c.subject())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| n.clone());
                    let mut body = String::new();
                    for c in &commits {
                        let rest = c.message.split_once('\n').map(|x| x.1.trim()).unwrap_or("");
                        if commits.len() > 1 {
                            body += &format!("- {}\n", c.subject());
                        } else if !rest.is_empty() {
                            body += rest;
                            body.push('\n');
                        }
                    }
                    let v = api.call(
                        "POST",
                        &format!("/repos/{repo}/pulls"),
                        Some(json!({
                            "title": title, "head": n, "base": base_branch,
                            "body": body, "draft": opts.draft,
                        })),
                    )?;
                    let number = v["number"].as_u64().unwrap_or(0);
                    // Local `stack sync` keeps stacked branches current; the
                    // restack Action would rewrite them under this window.
                    let _ = api.call(
                        "POST",
                        &format!("/repos/{repo}/issues/{number}/labels"),
                        Some(json!({ "labels": [crate::restack::SKIP_LABEL] })),
                    );
                    (v, true)
                }
            };
            let number = v["number"].as_u64().unwrap_or(0);
            self.state.branches[i].pr = Some(number);
            self.state.branches[i].pr_state = Some(pr_state_of(&v));
            bodies.push((number, v["body"].as_str().unwrap_or_default().to_string()));
            result.prs.push(SubmittedPr {
                branch: n.clone(),
                number,
                url: v["html_url"].as_str().unwrap_or_default().to_string(),
                base: base_branch,
                created,
            });
        }
        self.save()?;
        // Every PR lists the whole stack.
        let depth: HashMap<String, usize> = self.ordered().into_iter().collect();
        for (number, body) in bodies {
            let mut section = format!(
                "{STACK_BEGIN}\n**Stack** (bottom first, kept up to date by `agentvc stack submit`)\n\n"
            );
            for p in &result.prs {
                section += &format!(
                    "{}- #{} `{}`{}\n",
                    "  ".repeat(depth.get(&p.branch).copied().unwrap_or(0)),
                    p.number,
                    p.branch,
                    if p.number == number {
                        " 👈 this PR"
                    } else {
                        ""
                    }
                );
            }
            section += &format!("\nOn `{}`.\n{STACK_END}", self.state.trunk);
            let new_body = with_stack_section(&body, &section);
            if new_body != body {
                api.call(
                    "PATCH",
                    &format!("/repos/{repo}/pulls/{number}"),
                    Some(json!({ "body": new_body })),
                )?;
            }
        }
        Ok(result)
    }

    // ---- give ----

    /// Hand `branch` and the branches above it to the window at `to`. They
    /// keep their parents; a parent staying here becomes a read-only base
    /// there.
    pub fn give(&mut self, branch: &str, to: &Path) -> Result<Vec<String>> {
        self.settle()?;
        self.require_tracked(branch)?;
        let probe = HostRepo {
            workdir: to.to_path_buf(),
        };
        let their_git = probe
            .git(&["rev-parse", "--absolute-git-dir"], None)
            .map_err(|_| Error::NotGitRepo(to.to_path_buf()))?;
        let their_git = canon(Path::new(&their_git));
        let their_common = canon(&to.join(probe.git(&["rev-parse", "--git-common-dir"], None)?));
        if their_common != self.common_dir {
            return Err(stack_err(
                "other_repository",
                format!("{} is a worktree of another repository", to.display()),
            ));
        }
        if their_git == self.git_dir {
            return Err(stack_err("same_window", "that is this window"));
        }
        let moving: Vec<String> = {
            let set = self.upstack(branch);
            self.ordered()
                .into_iter()
                .map(|(n, _)| n)
                .filter(|n| set.contains(n))
                .collect()
        };
        let mut theirs = read_state(&their_git)?.unwrap_or_else(|| State {
            version: STATE_VERSION,
            trunk: self.state.trunk.clone(),
            remote: self.state.remote.clone(),
            branches: Vec::new(),
            pending: None,
        });
        for n in &moving {
            let t = self.state.branches[self.find(n).expect("tracked")].clone();
            theirs.branches.retain(|x| x.name != t.name);
            theirs.branches.push(t);
        }
        write_state(&their_git, &theirs)?;
        self.state.branches.retain(|b| !moving.contains(&b.name));
        self.save()?;
        Ok(moving)
    }

    // ---- log ----

    pub fn log(&self, all: bool) -> Result<LogResult> {
        let current = self.current_branch();
        let elsewhere = self.checkouts_elsewhere();
        let others = self.other_windows();
        let other_owner = |b: &str| {
            others
                .iter()
                .find(|(_, s)| s.branches.iter().any(|x| x.name == b))
                .map(|(p, _)| p.clone())
        };
        let mut branches = Vec::new();
        for (name, depth) in self.ordered() {
            let b = &self.state.branches[self.find(&name).expect("tracked")];
            let tip = self.branch_tip(&name);
            let (parent_kind, parent_window) = if b.parent == self.state.trunk {
                (ParentKind::Trunk, None)
            } else if self.tracks(&b.parent) {
                (ParentKind::Stack, None)
            } else if let Some(p) = other_owner(&b.parent) {
                (ParentKind::OtherWindow, Some(p))
            } else {
                (ParentKind::Branch, None)
            };
            let parent_now = self.parent_tip(&b.parent).ok();
            let commits = tip
                .as_ref()
                .and_then(|t| {
                    self.repo
                        .git(
                            &["rev-list", "--count", &format!("{}..{t}", b.parent_rev)],
                            None,
                        )
                        .ok()
                })
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            let needs_restack = match (&parent_now, &tip) {
                (Some(p), Some(t)) => *p != b.parent_rev && !self.on_top(&b.parent_rev, p, t),
                _ => false,
            };
            branches.push(BranchInfo {
                name: name.clone(),
                parent: b.parent.clone(),
                parent_kind,
                parent_window,
                parent_rev: b.parent_rev.clone(),
                tip,
                commits,
                needs_restack,
                current: current.as_deref() == Some(name.as_str()),
                depth,
                children: self.children_of(&name),
                pr: b.pr,
                checked_out_elsewhere: elsewhere.get(&name).cloned(),
            });
        }
        let others = if all {
            others
                .into_iter()
                .map(|(path, s)| OtherStack {
                    path,
                    branches: s
                        .branches
                        .iter()
                        .map(|b| OtherBranch {
                            name: b.name.clone(),
                            parent: b.parent.clone(),
                            tip: self.branch_tip(&b.name),
                            pr: b.pr,
                        })
                        .collect(),
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(LogResult {
            window: self.repo.workdir.clone(),
            git_dir: self.git_dir.clone(),
            trunk: self.state.trunk.clone(),
            base_ref: self.base_ref(),
            base: self.base(),
            current,
            branches,
            pending: self.state.pending.clone(),
            others,
        })
    }
}

/// The trunk name and remote: `origin/HEAD`'s target, else `main` or
/// `master`.
fn detect_trunk(repo: &HostRepo) -> (String, String) {
    let remotes: Vec<String> = repo
        .git(&["remote"], None)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let remote = if remotes.iter().any(|r| r == "origin") {
        "origin".to_string()
    } else {
        remotes.first().cloned().unwrap_or_default()
    };
    if !remote.is_empty() {
        if let Ok(h) = repo.git(
            &[
                "symbolic-ref",
                "--quiet",
                &format!("refs/remotes/{remote}/HEAD"),
            ],
            None,
        ) {
            if let Some(t) = h.strip_prefix(&format!("refs/remotes/{remote}/")) {
                return (t.to_string(), remote);
            }
        }
    }
    for t in ["main", "master"] {
        if repo.rev(&format!("refs/heads/{t}")).is_some()
            || (!remote.is_empty() && repo.rev(&format!("refs/remotes/{remote}/{t}")).is_some())
        {
            return (t.to_string(), remote);
        }
    }
    ("main".to_string(), remote)
}

/// Graphite's recorded parent of `branch` (and parent revision), from the
/// `refs/branch-metadata/<branch>` blobs gt wrote before 1.8.0.
fn graphite_parent(repo: &HostRepo, branch: &str) -> Option<(String, Option<String>)> {
    let text = repo
        .git(
            &["cat-file", "-p", &format!("refs/branch-metadata/{branch}")],
            None,
        )
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let parent = v.get("parentBranchName")?.as_str()?.to_string();
    let rev = v
        .get("parentBranchRevision")
        .and_then(|r| r.as_str())
        .map(str::to_string);
    Some((parent, rev))
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn rev(dir: &Path, r: &str) -> String {
        git(dir, &["rev-parse", r])
    }

    /// A bare "origin" with main, a clone, and a linked worktree `w2`.
    struct Fixture {
        _t: crate::testutil::TestDir,
        origin: PathBuf,
        w1: PathBuf,
        w2: PathBuf,
    }

    fn setup() -> Fixture {
        let t = crate::testutil::tempdir();
        let root = t.path().to_path_buf();
        let seed = root.join("seed");
        fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        git(&seed, &["config", "user.name", "t"]);
        git(&seed, &["config", "user.email", "t@t"]);
        fs::write(seed.join("a.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n9\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "init"]);
        let origin = root.join("origin.git");
        git(
            &root,
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                origin.to_str().unwrap(),
            ],
        );
        let w1 = root.join("w1");
        git(
            &root,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                w1.to_str().unwrap(),
            ],
        );
        git(&w1, &["config", "user.name", "t"]);
        git(&w1, &["config", "user.email", "t@t"]);
        let w2 = root.join("w2");
        git(
            &w1,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                w2.to_str().unwrap(),
                "origin/main",
            ],
        );
        Fixture {
            _t: t,
            origin,
            w1,
            w2,
        }
    }

    fn commit_file(dir: &Path, file: &str, text: &str, msg: &str) {
        fs::write(dir.join(file), text).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", msg]);
    }

    /// Push a commit to origin/main from a scratch clone.
    fn advance_origin(f: &Fixture, file: &str, text: &str) {
        let tmp = f.origin.parent().unwrap().join(format!("scratch-{file}"));
        let _ = fs::remove_dir_all(&tmp);
        git(
            f.origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                f.origin.to_str().unwrap(),
                tmp.to_str().unwrap(),
            ],
        );
        commit_file(&tmp, file, text, &format!("upstream {file}"));
        git(&tmp, &["push", "-q", "origin", "main"]);
    }

    #[test]
    fn windows_have_separate_stacks() {
        let f = setup();
        let mut a = Window::open(&f.w1).unwrap();
        assert_eq!(a.state.trunk, "main");
        assert_eq!(a.base_ref(), "origin/main");
        fs::write(f.w1.join("b.txt"), "b\n").unwrap();
        a.create(Some("feat-a"), Some("add b"), true).unwrap();

        let mut b = Window::open(&f.w2).unwrap();
        assert_ne!(a.git_dir, b.git_dir);
        assert!(b.state.branches.is_empty(), "w2 must not see w1's stack");
        fs::write(f.w2.join("c.txt"), "c\n").unwrap();
        b.create(None, Some("Add C file!"), true).unwrap();
        assert_eq!(b.state.branches[0].name, "add-c-file");

        let la = Window::open(&f.w1).unwrap().log(true).unwrap();
        assert_eq!(la.branches.len(), 1);
        assert_eq!(la.branches[0].name, "feat-a");
        assert_eq!(la.branches[0].commits, 1);
        assert_eq!(la.others.len(), 1);
        assert_eq!(la.others[0].branches[0].name, "add-c-file");

        // A branch another window tracks cannot be adopted here.
        let e = Window::open(&f.w1)
            .unwrap()
            .adopt("add-c-file", None, true)
            .unwrap_err();
        assert_eq!(e.code(), "owned_by_other_window");
    }

    #[test]
    fn long_chain_restacks_after_amend_and_sync() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        // 12 branches, each editing its own line of a.txt.
        for i in 1..=12 {
            fs::write(f.w1.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
            w.create(Some(&format!("s{i}")), Some(&format!("step {i}")), true)
                .unwrap();
        }
        assert_eq!(w.state.branches.len(), 12);
        assert_eq!(w.log(false).unwrap().branches[11].depth, 11);

        // Amend the bottom branch: everything above restacks.
        assert_eq!(w.bottom().unwrap(), "s1");
        fs::write(f.w1.join("f1.txt"), "1 amended\n").unwrap();
        let (_, r) = w.modify(None, true, false, false).unwrap();
        assert_eq!(r.updated.len(), 11);
        let s1 = rev(&f.w1, "s1");
        assert_eq!(rev(&f.w1, "s2~1"), s1);
        assert_eq!(rev(&f.w1, "s12~11"), s1);
        assert!(w
            .log(false)
            .unwrap()
            .branches
            .iter()
            .all(|b| !b.needs_restack));

        // main moves upstream; s1 gets squash-merged upstream as well.
        advance_origin(&f, "up.txt", "up\n");
        let tmp = f.origin.parent().unwrap().join("squash");
        git(
            f.origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                f.origin.to_str().unwrap(),
                tmp.to_str().unwrap(),
            ],
        );
        commit_file(&tmp, "f1.txt", "1 amended\n", "step 1 (#1)");
        git(&tmp, &["push", "-q", "origin", "main"]);

        git(&f.w1, &["switch", "-q", "s5"]);
        let s = w.sync(true, true, false).unwrap();
        assert_eq!(s.merged, vec!["s1".to_string()]);
        assert_eq!(s.deleted, vec!["s1".to_string()]);
        assert_eq!(s.restack.updated.len(), 11);
        let base = rev(&f.w1, "origin/main");
        assert_eq!(rev(&f.w1, "s2~1"), base);
        assert_eq!(rev(&f.w1, "s12~11"), base);
        assert_eq!(w.state.branches[0].parent, "main");
        // The checked-out branch moved with the worktree.
        assert_eq!(rev(&f.w1, "HEAD"), rev(&f.w1, "s5"));
        assert_eq!(fs::read_to_string(f.w1.join("up.txt")).unwrap(), "up\n");
        // Local main was never touched.
        assert_ne!(rev(&f.w1, "main"), base);
    }

    #[test]
    fn conflicts_change_nothing_or_go_to_the_worktree() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("a.txt"), "1\n2\nmine\n4\n5\n6\n7\n8\n9\n").unwrap();
        w.create(Some("x"), Some("mine"), true).unwrap();
        fs::write(f.w1.join("y.txt"), "y\n").unwrap();
        w.create(Some("y"), Some("y"), true).unwrap();
        advance_origin(&f, "a.txt", "1\n2\ntheirs\n4\n5\n6\n7\n8\n9\n");
        let (x0, y0) = (rev(&f.w1, "x"), rev(&f.w1, "y"));
        let e = w.sync(true, true, false).unwrap_err();
        assert_eq!(e.code(), "conflict");
        assert_eq!((rev(&f.w1, "x"), rev(&f.w1, "y")), (x0, y0));

        let r = w.restack(None, true).unwrap();
        assert!(r.kept_conflicts);
        assert_eq!(r.conflict.unwrap().branch, "x");
        assert_eq!(
            w.restack(None, false).unwrap_err().code(),
            "rebase_in_progress"
        );
        // Resolve in the worktree as an editor would.
        fs::write(f.w1.join("a.txt"), "1\n2\nboth\n4\n5\n6\n7\n8\n9\n").unwrap();
        git(&f.w1, &["add", "a.txt"]);
        let out = Command::new("git")
            .arg("-C")
            .arg(&f.w1)
            .args(["-c", "core.editor=true", "rebase", "--continue"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let r = w.restack(None, false).unwrap();
        assert_eq!(r.updated.len(), 1);
        assert_eq!(rev(&f.w1, "y~1"), rev(&f.w1, "x"));
        assert_eq!(rev(&f.w1, "x~1"), rev(&f.w1, "origin/main"));
        assert!(w.state.pending.is_none());
    }

    #[test]
    fn adopt_infers_parents_and_foreign_bases_are_read_only() {
        let f = setup();
        git(&f.w1, &["switch", "-q", "-c", "p1", "origin/main"]);
        commit_file(&f.w1, "p1.txt", "1\n", "p1");
        git(&f.w1, &["switch", "-q", "-c", "p2"]);
        commit_file(&f.w1, "p2.txt", "2\n", "p2");
        git(&f.w1, &["switch", "-q", "-c", "p3"]);
        commit_file(&f.w1, "p3.txt", "3\n", "p3");
        let mut w = Window::open(&f.w1).unwrap();
        let added = w.adopt("p3", None, true).unwrap();
        let names: Vec<_> = added
            .iter()
            .map(|t| (t.name.as_str(), t.parent.as_str()))
            .collect();
        assert_eq!(names, vec![("p1", "main"), ("p2", "p1"), ("p3", "p2")]);

        // Window 2 builds on w1's p3: it follows p3 but never rewrites it.
        let mut w2 = Window::open(&f.w2).unwrap();
        git(&f.w2, &["switch", "-q", "--detach", "p3"]);
        fs::write(f.w2.join("q.txt"), "q\n").unwrap();
        git(&f.w2, &["switch", "-q", "-c", "q"]);
        git(&f.w2, &["add", "-A"]);
        git(&f.w2, &["commit", "-q", "-m", "q"]);
        w2.adopt("q", Some("p3"), false).unwrap();
        assert_eq!(
            w2.log(false).unwrap().branches[0].parent_kind,
            ParentKind::OtherWindow
        );
        // w1 amends p3; w2's restack follows it without touching p3.
        git(&f.w1, &["switch", "-q", "p3"]);
        fs::write(f.w1.join("p3.txt"), "3!\n").unwrap();
        w.modify(None, true, false, false).unwrap();
        let p3 = rev(&f.w1, "p3");
        let r = w2.restack(None, false).unwrap();
        assert_eq!(r.updated.len(), 1);
        assert_eq!(rev(&f.w1, "q~1"), p3);
        assert_eq!(rev(&f.w1, "p3"), p3);
        assert_eq!(w2.down().unwrap_err().code(), "at_bottom");
    }

    /// A tiny in-memory GitHub: pulls (create, list by head, get, patch)
    /// and labels.
    fn fake_github() -> (
        Api,
        std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::sync::{Arc, Mutex};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let prs: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let store = prs.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                reader.read_exact(&mut body).unwrap();
                let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let mut prs = store.lock().unwrap();
                let num =
                    |p: &str| -> Option<usize> { p.split('/').nth(5).and_then(|n| n.parse().ok()) };
                let out = if method == "POST" && path == "/repos/o/r/pulls" {
                    let n = prs.len() + 1;
                    let pr = json!({
                        "number": n, "html_url": format!("https://github.com/o/r/pull/{n}"),
                        "state": "open", "merged": false, "body": req["body"],
                        "title": req["title"], "head": { "ref": req["head"], "sha": "" },
                        "base": { "ref": req["base"] }, "labels": [],
                    });
                    prs.push(pr.clone());
                    pr
                } else if method == "GET" && path.starts_with("/repos/o/r/pulls?") {
                    let head = path.split("head=o:").nth(1).unwrap_or_default().to_string();
                    json!(prs
                        .iter()
                        .filter(|p| p["head"]["ref"] == head.as_str() && p["state"] == "open")
                        .cloned()
                        .collect::<Vec<_>>())
                } else if path.starts_with("/repos/o/r/pulls/") {
                    let i = num(&path).unwrap() - 1;
                    if method == "PATCH" {
                        if let Some(b) = req["base"].as_str() {
                            prs[i]["base"]["ref"] = json!(b);
                        }
                        if let Some(b) = req["body"].as_str() {
                            prs[i]["body"] = json!(b);
                        }
                    }
                    prs[i].clone()
                } else if path.contains("/labels") {
                    let i = num(&path).unwrap() - 1;
                    prs[i]["labels"] = req["labels"].clone();
                    json!([])
                } else {
                    json!({ "message": "not found" })
                };
                let text = out.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        let mut api = Api::new("t".into());
        api.set_base(&base);
        (api, prs)
    }

    #[test]
    fn submit_opens_one_pr_per_branch_on_its_parent() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        for i in 1..=3 {
            fs::write(f.w1.join(format!("s{i}.txt")), "x\n").unwrap();
            w.create(
                Some(&format!("s{i}")),
                Some(&format!("Step {i}\n\nWhy {i}.")),
                true,
            )
            .unwrap();
        }
        let (api, prs) = fake_github();
        let opts = SubmitOptions {
            repo: Some("o/r".into()),
            ..Default::default()
        };
        let r = w.submit_with(&opts, Some(&api)).unwrap();
        assert_eq!(r.pushed, vec!["s1", "s2", "s3"]);
        let bases: Vec<_> = r.prs.iter().map(|p| (p.number, p.base.as_str())).collect();
        assert_eq!(bases, vec![(1, "main"), (2, "s1"), (3, "s2")]);
        {
            let prs = prs.lock().unwrap();
            assert_eq!(prs[0]["title"], "Step 1");
            assert_eq!(prs[0]["labels"], json!([crate::restack::SKIP_LABEL]));
            let body = prs[1]["body"].as_str().unwrap();
            assert!(body.contains("- #2 `s2` 👈 this PR"), "{body}");
            assert!(body.contains("- #1 `s1`\n"), "{body}");
            assert!(body.contains("Why 2."), "{body}");
        }
        assert_eq!(git(&f.origin, &["rev-parse", "s3"]), rev(&f.w1, "s3"));
        assert_eq!(w.state.branches[2].pr, Some(3));

        // Amend the middle: only s2 and s3 are pushed again, no new PRs.
        git(&f.w1, &["switch", "-q", "s2"]);
        fs::write(f.w1.join("s2.txt"), "y\n").unwrap();
        w.modify(None, true, false, false).unwrap();
        let r = w.submit_with(&opts, Some(&api)).unwrap();
        assert_eq!(r.pushed, vec!["s2", "s3"]);
        assert!(r.prs.iter().all(|p| !p.created));
        assert_eq!(prs.lock().unwrap().len(), 3);

        // Someone else pushed to s3: the lease refuses, nothing is pushed.
        let tmp = f.origin.parent().unwrap().join("other");
        git(
            f.origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                "-b",
                "s3",
                f.origin.to_str().unwrap(),
                tmp.to_str().unwrap(),
            ],
        );
        commit_file(&tmp, "z.txt", "z\n", "theirs");
        git(&tmp, &["push", "-q", "origin", "s3"]);
        git(&f.w1, &["switch", "-q", "s3"]);
        fs::write(f.w1.join("s3.txt"), "mine\n").unwrap();
        w.modify(None, true, false, false).unwrap();
        let e = w.submit_with(&opts, Some(&api)).unwrap_err();
        assert_eq!(e.code(), "push_rejected");
    }

    #[test]
    fn give_moves_a_branch_and_its_upstack_to_another_window() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        for n in ["g1", "g2", "g3"] {
            fs::write(f.w1.join(format!("{n}.txt")), "x\n").unwrap();
            w.create(Some(n), Some(n), true).unwrap();
        }
        git(&f.w1, &["switch", "-q", "g1"]);
        let moved = w.give("g2", &f.w2).unwrap();
        assert_eq!(moved, vec!["g2", "g3"]);
        assert_eq!(w.state.branches.len(), 1);
        let w2 = Window::open(&f.w2).unwrap();
        let l = w2.log(false).unwrap();
        assert_eq!(l.branches[0].name, "g2");
        assert_eq!(l.branches[0].parent_kind, ParentKind::OtherWindow);
        assert_eq!(l.branches[1].name, "g3");
        assert_eq!(w.give("g1", &f.w1).unwrap_err().code(), "same_window");
    }

    #[test]
    fn move_onto_takes_only_the_branch_commits() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        for n in ["x", "a"] {
            fs::write(f.w1.join(format!("{n}.txt")), "x\n").unwrap();
            w.create(Some(n), Some(n), true).unwrap();
        }
        git(&f.w1, &["switch", "-q", "--detach", "origin/main"]);
        fs::write(f.w1.join("y.txt"), "y\n").unwrap();
        w.create(Some("y"), Some("y"), true).unwrap();
        // a sits on x; move it onto y: x's commit must not come along.
        let r = w.move_onto("a", "y", false).unwrap();
        assert_eq!(r.updated.len(), 1);
        assert_eq!(rev(&f.w1, "a~1"), rev(&f.w1, "y"));
        let files = git(&f.w1, &["ls-tree", "--name-only", "a"]);
        assert!(files.contains("a.txt") && files.contains("y.txt") && !files.contains("x.txt"));
        assert_eq!(w.move_onto("x", "x", false).unwrap_err().code(), "bad_move");
        // Back onto the trunk.
        w.move_onto("a", "origin/main", false).unwrap();
        assert_eq!(rev(&f.w1, "a~1"), rev(&f.w1, "origin/main"));
    }

    #[test]
    fn a_parent_reset_backwards_drops_its_commit_from_children() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("p1.txt"), "1\n").unwrap();
        w.create(Some("p"), Some("p1"), true).unwrap();
        fs::write(f.w1.join("p2.txt"), "2\n").unwrap();
        w.modify(Some("p2"), true, true, false).unwrap();
        fs::write(f.w1.join("c.txt"), "c\n").unwrap();
        w.create(Some("c"), Some("c"), true).unwrap();
        // Drop p's last commit by hand; c must lose it too.
        git(&f.w1, &["branch", "-f", "p", "p~1"]);
        let r = w.restack(None, false).unwrap();
        assert_eq!(r.updated.len(), 1);
        assert_eq!(rev(&f.w1, "c~1"), rev(&f.w1, "p"));
        assert!(!git(&f.w1, &["ls-tree", "--name-only", "c"]).contains("p2.txt"));
    }

    #[test]
    fn rename_delete_and_squash_keep_the_stack_whole() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        for n in ["d1", "d2", "d3"] {
            fs::write(f.w1.join(format!("{n}.txt")), "x\n").unwrap();
            w.create(Some(n), Some(n), true).unwrap();
        }
        // Two more commits on d3, then squash them into one.
        for i in 0..2 {
            fs::write(f.w1.join(format!("d3-{i}.txt")), "y\n").unwrap();
            w.modify(Some("more"), true, true, false).unwrap();
        }
        let (_, r) = w.squash(None, false).unwrap();
        assert!(r.updated.is_empty());
        assert_eq!(rev(&f.w1, "d3~1"), rev(&f.w1, "d2"));
        assert_eq!(git(&f.w1, &["log", "-1", "--format=%s", "d3"]), "d3");
        assert!(git(&f.w1, &["ls-tree", "--name-only", "d3"]).contains("d3-1.txt"));

        w.rename("d2", "middle").unwrap();
        assert_eq!(
            w.state
                .branches
                .iter()
                .find(|b| b.name == "d3")
                .unwrap()
                .parent,
            "middle"
        );
        assert!(w
            .log(false)
            .unwrap()
            .branches
            .iter()
            .all(|b| !b.needs_restack));

        // Deleting the middle drops its commit from d3.
        let r = w.delete("middle", false).unwrap();
        assert_eq!(r.updated.len(), 1);
        assert_eq!(rev(&f.w1, "d3~1"), rev(&f.w1, "d1"));
        let files = git(&f.w1, &["ls-tree", "--name-only", "d3"]);
        assert!(!files.contains("d2.txt") && files.contains("d3.txt"));
        assert_eq!(w.state.branches.len(), 2);
    }
}
