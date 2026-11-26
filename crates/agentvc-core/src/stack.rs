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
use serde_json::json;

use crate::error::{Error, IoContext, Result};
use crate::export::HostRepo;
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
    repo: HostRepo,
    /// The worktree's own git directory.
    pub git_dir: PathBuf,
    /// The git directory shared by all worktrees.
    pub common_dir: PathBuf,
    pub state: State,
    rules: Rules,
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

fn state_path(git_dir: &Path) -> PathBuf {
    git_dir.join("agentvc").join("stack.json")
}

fn read_state(git_dir: &Path) -> Result<Option<State>> {
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
        let path = state_path(&self.git_dir);
        let dir = path.parent().expect("has a parent");
        fs::create_dir_all(dir).at(dir)?;
        let tmp = dir.join(format!("stack.json.{}.tmp", std::process::id()));
        let text = serde_json::to_string_pretty(&self.state).expect("state serializes");
        fs::write(&tmp, text + "\n").at(&tmp)?;
        fs::rename(&tmp, &path).at(&path)?;
        Ok(())
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

    fn branch_tip(&self, name: &str) -> Option<String> {
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
        let mut out = Vec::new();
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
        for (g, p) in dirs {
            if g != self.git_dir {
                out.push((g, p));
            }
        }
        out
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

    fn rebase_in_progress(&self) -> bool {
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
        loop {
            let mut found = None;
            for b in &self.state.branches {
                if b.parent == self.state.trunk && self.is_merged(b, &base)? {
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
}
