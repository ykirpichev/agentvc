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
    /// Checks and review of the PR when this window last looked
    /// (`stack log --prs`, `stack merge`, the daemon's poller).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_status: Option<PrBrief>,
    /// The commit this window last pushed to (or took over from) the
    /// remote branch: what `submit`'s lease expects there, so a push from
    /// another checkout with the same branch name is never overwritten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed: Option<String>,
}

/// A PR's checks and review, as `stack log --prs` shows them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrBrief {
    /// `pass`, `fail`, `pending` or `none`.
    pub checks: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failing: Vec<String>,
    /// `approved`, `changes_requested` or `none`.
    pub review: String,
    /// Review threads neither resolved nor outdated.
    pub unresolved: usize,
    /// GitHub's `mergeable` (`None` while it computes it).
    pub mergeable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mergeable_state: Option<String>,
    /// The PR's head commit.
    pub head: Option<String>,
    /// When it was fetched (unix seconds).
    pub at: i64,
}

impl PrBrief {
    pub fn of(f: &crate::feedback::PrFeedback) -> PrBrief {
        PrBrief {
            checks: f.checks.state.clone(),
            failing: f.checks.failing.clone(),
            review: f.decision.clone(),
            unresolved: f.unresolved,
            mergeable: f.mergeable,
            mergeable_state: f.mergeable_state.clone(),
            head: f.head_sha.clone(),
            at: f.refreshed_at,
        }
    }
}

/// What `stack get` did.
#[derive(Clone, Debug, Serialize)]
pub struct GetResult {
    /// The branches now tracked here, parents first.
    pub tracked: Vec<Tracked>,
    /// Local branches created from the remote.
    pub created: Vec<String>,
    /// Local branches fast-forwarded to the remote.
    pub updated: Vec<String>,
}

/// Options for `stack merge`.
#[derive(Clone, Debug, Default)]
pub struct MergeOptions {
    /// Merge up to this branch (default: the current one).
    pub branch: Option<String>,
    /// `squash` (default), `merge` or `rebase`.
    pub method: Option<String>,
    /// GitHub `owner/name`; defaults to the remote's.
    pub repo: Option<String>,
    /// Say whether the bottom PR could merge, change nothing.
    pub dry_run: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct MergedPr {
    pub branch: String,
    pub number: u64,
    /// The merge commit GitHub reported.
    pub sha: Option<String>,
}

/// Why `stack merge` stopped before the target.
#[derive(Clone, Debug, Serialize)]
pub struct MergeStop {
    pub branch: String,
    pub number: Option<u64>,
    /// `checks_pending`, `checks_failed`, `changes_requested`, `pr_draft`,
    /// `not_mergeable`, `blocked`, `not_pushed`, `no_pr`, `pr_closed`,
    /// `merge_failed`, `conflict`.
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct MergeResult {
    pub repo: String,
    pub merged: Vec<MergedPr>,
    /// Set when it stopped before merging everything up to the target;
    /// run it again once that is fixed (or checks are done).
    pub stopped: Option<MergeStop>,
    /// With `dry_run`: the bottom PR would merge now.
    pub ready: bool,
    pub statuses: Vec<(String, PrBrief)>,
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
    /// The parent the branch had before a move that stopped here: an
    /// aborted rebase puts it back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_from: Option<String>,
}

/// The contents of `stack.json`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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
    /// How agents started in this workspace run (`agentvc workspace agents`).
    #[serde(default, skip_serializing_if = "crate::agent::AgentSettings::is_empty")]
    pub agents: crate::agent::AgentSettings,
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
    /// Branches left for an agent to resolve ([`Window::hand_off`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handed_off: Vec<HandOff>,
}

/// A branch whose restack conflicts, left as it was for an agent (or a
/// person) to resolve: the replay of `old` onto `onto` stopped at `commit`.
/// [`Window::finish_hand_off`] puts the resolution on the stack.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandOff {
    pub branch: String,
    /// The parent branch (or trunk) it was being restacked onto.
    pub parent: String,
    /// The branch's tip before the restack.
    pub old: String,
    /// The parent's tip it was being restacked onto.
    pub onto: String,
    /// The commits before `commit`, already replayed onto `onto`.
    pub partial: String,
    /// The branch's commit that conflicts.
    pub commit: String,
    pub subject: String,
    /// Files that still conflict after auto-resolution.
    pub paths: Vec<String>,
    /// `commit` merged onto `partial`, with conflict markers in `paths`.
    pub tree: String,
    /// Commits of the branch after `commit`, replayed once it is resolved.
    #[serde(default)]
    pub rest: usize,
}

/// What `fold` did.
#[derive(Clone, Debug, Serialize)]
pub struct FoldResult {
    pub folded: String,
    pub into: String,
    /// The folded branch's PR, now superseded by the parent's: close it.
    pub closed_pr: Option<u64>,
    pub restack: RestackResult,
}

/// Staged files `absorb` amended into one branch.
#[derive(Clone, Debug, Serialize)]
pub struct AbsorbedInto {
    pub branch: String,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AbsorbResult {
    pub absorbed: Vec<AbsorbedInto>,
    /// Staged files no branch of the stack changed: still staged.
    pub left: Vec<String>,
    pub restack: RestackResult,
}

#[derive(Clone, Debug, Serialize)]
pub struct SyncResult {
    pub fetched: bool,
    /// Remote-tracking refs deleted because the remote no longer has the
    /// branch (like `git fetch --prune`).
    pub pruned: Vec<String>,
    /// The base commit after the fetch.
    pub base: String,
    /// Branches found merged into the base and removed from the stack.
    pub merged: Vec<String>,
    /// Merged branches whose local branch was deleted.
    pub deleted: Vec<String>,
    /// Where each deleted branch's old tip is kept, and how to restore it.
    pub trashed: Vec<Trashed>,
    /// Where the worktree was switched to when its branch was merged.
    pub switched_to: Option<String>,
    pub restack: RestackResult,
}

/// A branch `stack sync` deleted, kept under [`trash_ref`].
#[derive(Clone, Debug, Serialize)]
pub struct Trashed {
    pub branch: String,
    /// Its tip when it was deleted.
    pub tip: String,
    /// `refs/agentvc/trash/<branch>`.
    pub trash_ref: String,
    /// The command that brings the branch back.
    pub restore: String,
}

/// Where `stack sync` keeps the tip of a merged branch it deletes (#200).
pub fn trash_ref(branch: &str) -> String {
    format!("refs/agentvc/trash/{branch}")
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_state: Option<String>,
    /// Checks and review as last fetched (`log --prs`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_status: Option<PrBrief>,
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
        /// The replay so far: the commits before `commit`, on the new parent.
        partial: String,
        /// The merge of `commit` onto `partial`, conflict markers included.
        tree: String,
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
    /// Restack hands conflicting branches off (see [`HandOff`]) instead of
    /// failing or stopping in the worktree: they and their upstack stay as
    /// they were, everything else restacks.
    pub hand_off: bool,
}

pub(crate) fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

pub(crate) fn state_path(git_dir: &Path) -> PathBuf {
    git_dir.join("agentvc").join("stack.json")
}

/// The previous good `stack.json`, kept by every save (#204).
pub(crate) fn backup_path(git_dir: &Path) -> PathBuf {
    git_dir.join("agentvc").join("stack.json.bak")
}

fn parse_state(text: &str) -> std::result::Result<State, serde_json::Error> {
    serde_json::from_str(text)
}

fn read_backup(git_dir: &Path) -> Option<State> {
    parse_state(&fs::read_to_string(backup_path(git_dir)).ok()?).ok()
}

/// The stack state of the worktree whose git dir is `git_dir`. A
/// `stack.json` that does not parse (cut short by a crash or a full disk)
/// falls back to `stack.json.bak` with a warning; `agentvc doctor --repair`
/// puts the backup back.
pub(crate) fn read_state(git_dir: &Path) -> Result<Option<State>> {
    let path = state_path(git_dir);
    let text = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).at(path),
    };
    match parse_state(&text) {
        Ok(state) => Ok(Some(state)),
        Err(e) => match read_backup(git_dir) {
            Some(state) => {
                eprintln!(
                    "agentvc: warning: {} is not valid ({e}); using the previous state from {} (`agentvc doctor --repair` restores it)",
                    path.display(),
                    backup_path(git_dir).display()
                );
                Ok(Some(state))
            }
            None => Err(stack_err(
                "bad_stack_state",
                format!(
                    "{} is not valid: {e} (and there is no usable {})",
                    path.display(),
                    backup_path(git_dir).display()
                ),
            )),
        },
    }
}

/// Write `text` to `path` so that a crash leaves either the old or the new
/// contents: temp file, fsync, rename, fsync of the directory (Unix).
pub(crate) fn write_durable(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().expect("has a parent");
    fs::create_dir_all(dir).at(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("f");
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    let written = fs::File::create(&tmp)
        .and_then(|mut f| {
            f.write_all(text.as_bytes())?;
            f.sync_all()
        })
        .and_then(|()| fs::rename(&tmp, path));
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e).at(path);
    }
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

pub(crate) fn write_state(git_dir: &Path, state: &State) -> Result<()> {
    let path = state_path(git_dir);
    let text = serde_json::to_string_pretty(state).expect("state serializes") + "\n";
    // Keep the state being replaced, if it is good, as the backup.
    if let Ok(old) = fs::read_to_string(&path) {
        if old != text && parse_state(&old).is_ok() {
            write_durable(&backup_path(git_dir), &old)?;
        }
    }
    write_durable(&path, &text)
}

/// What `agentvc doctor` finds in one worktree's `stack.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateHealth {
    /// No stack here.
    Missing,
    Ok,
    /// Does not parse; `backup` says whether `stack.json.bak` does.
    Bad {
        error: String,
        backup: bool,
    },
}

/// Check the `stack.json` of the worktree whose git dir is `git_dir`.
pub fn check_state(git_dir: &Path) -> StateHealth {
    match fs::read_to_string(state_path(git_dir)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => StateHealth::Missing,
        Err(e) => StateHealth::Bad {
            error: e.to_string(),
            backup: read_backup(git_dir).is_some(),
        },
        Ok(t) => match parse_state(&t) {
            Ok(_) => StateHealth::Ok,
            Err(e) => StateHealth::Bad {
                error: e.to_string(),
                backup: read_backup(git_dir).is_some(),
            },
        },
    }
}

/// Put `stack.json.bak` back as `stack.json` when `stack.json` is bad and
/// the backup is good; the bad file is kept as `stack.json.bad`. Returns
/// whether it restored anything.
pub fn restore_backup(git_dir: &Path) -> Result<bool> {
    if !matches!(check_state(git_dir), StateHealth::Bad { backup: true, .. }) {
        return Ok(false);
    }
    let bak = backup_path(git_dir);
    let text = fs::read_to_string(&bak).at(&bak)?;
    let path = state_path(git_dir);
    let _ = fs::copy(&path, path.with_extension("json.bad"));
    write_durable(&path, &text)?;
    Ok(true)
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

/// The open PR whose head is `branch` in `repo` (`owner/name`), if any.
fn find_open_pr(api: &Api, repo: &str, branch: &str) -> Result<Option<u64>> {
    let owner = repo.split('/').next().unwrap_or_default();
    let open = api.call(
        "GET",
        &format!("/repos/{repo}/pulls?state=open&head={owner}:{branch}"),
        None,
    )?;
    Ok(open
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v["number"].as_u64()))
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
                    agents: Default::default(),
                }
            }
        };
        Ok(Window {
            repo,
            git_dir,
            common_dir,
            state,
            rules: Rules::new(),
            hand_off: false,
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

    pub fn branch_tip(&self, name: &str) -> Option<String> {
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

    /// The remote's branch heads (all of them, or only `names`), by name;
    /// `None` when the remote cannot be asked (offline, no remote).
    fn remote_heads(&self, names: &[String]) -> Option<HashMap<String, String>> {
        if self.state.remote.is_empty() {
            return None;
        }
        let mut args = vec!["ls-remote".to_string(), "--heads".into()];
        args.push(self.state.remote.clone());
        args.extend(names.iter().map(|n| format!("refs/heads/{n}")));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.repo.git(&refs, None).ok()?;
        let all = names.is_empty();
        Some(
            out.lines()
                .filter_map(|l| l.split_once('\t'))
                .filter_map(|(sha, r)| {
                    r.strip_prefix("refs/heads/")
                        .map(|n| (n.to_string(), sha.to_string()))
                })
                .filter(|(n, _)| all || names.contains(n))
                .collect(),
        )
    }

    /// Delete `refs/remotes/<remote>/<name>` if it still points at `old`.
    fn drop_tracking_ref(&self, name: &str, old: &str) -> bool {
        let r = format!("refs/remotes/{}/{name}", self.state.remote);
        self.repo.git(&["update-ref", "-d", &r, old], None).is_ok()
    }

    /// Delete the remote-tracking refs of branches the remote no longer has
    /// (`git fetch --prune` without fetching anything else). Returns the
    /// pruned branch names; nothing when the remote cannot be reached.
    pub fn prune_tracking_refs(&self) -> Vec<String> {
        let Some(heads) = self.remote_heads(&[]) else {
            return Vec::new();
        };
        let prefix = format!("refs/remotes/{}/", self.state.remote);
        let local = self
            .repo
            .git(
                &["for-each-ref", "--format=%(refname) %(objectname)", &prefix],
                None,
            )
            .unwrap_or_default();
        let mut pruned = Vec::new();
        for line in local.lines() {
            let Some((r, sha)) = line.split_once(' ') else {
                continue;
            };
            let Some(name) = r.strip_prefix(&prefix) else {
                continue;
            };
            if name == "HEAD" || heads.contains_key(name) {
                continue;
            }
            if self.drop_tracking_ref(name, sha) {
                pruned.push(name.to_string());
            }
        }
        pruned
    }

    /// Fetch the remote trunk into `origin/<trunk>` (only that ref, so the
    /// leases of other windows' branches stay as they are). Returns whether
    /// there was a remote to fetch from.
    pub fn fetch_trunk(&self) -> Result<bool> {
        if self.state.remote.is_empty() {
            return Ok(false);
        }
        let spec = format!(
            "+refs/heads/{t}:refs/remotes/{r}/{t}",
            t = self.state.trunk,
            r = self.state.remote
        );
        self.repo
            .git(&["fetch", "--quiet", &self.state.remote, &spec], None)?;
        Ok(true)
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
    pub(crate) fn checkouts_elsewhere(&self) -> HashMap<String, PathBuf> {
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
        // Agents' worktrees (see `agent`) are not workspaces.
        dirs.retain(|(g, p)| {
            *g == self.git_dir || (p.exists() && !g.join(crate::agent::AGENT_MARKER).exists())
        });
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
                    "{} is being rebased; resolve the conflicts, `git add` the files, then run `agentvc stack continue` (or `agentvc stack abort`)",
                    p.branch
                ),
                json!({ "branch": p.branch }),
            ));
        }
        if let (Some(i), Some(tip)) = (self.find(&p.branch), self.branch_tip(&p.branch)) {
            if self.repo.is_ancestor(&p.onto, &tip) {
                self.state.branches[i].parent_rev = p.onto.clone();
            } else if let Some(parent) = &p.moved_from {
                // The rebase was aborted: the move did not happen.
                self.state.branches[i].parent = parent.clone();
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

    /// Cherry-pick `commits` onto `onto`, in memory, resolving conflicts
    /// with `rules`. A commit that turns empty only because a resolution
    /// rule picked the other side (an indentation-only change against a
    /// trunk edit of the same line, #200) is a conflict, never dropped:
    /// in Python, YAML or Makefiles indentation is a real change.
    fn replay(
        &self,
        branch: &str,
        commits: &[CommitInfo],
        onto: &str,
        rules: &Rules,
    ) -> Result<Replayed> {
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
            let m = merge_commits(&self.repo, Some(&c.parents[0]), &tip, &c.id, rules, &sides)?;
            if !m.conflicts.is_empty() {
                return Ok(Replayed::Conflict {
                    commit: c.id.clone(),
                    subject: c.subject(),
                    paths: m.conflicts,
                    partial: tip,
                    tree: m.tree,
                });
            }
            if m.tree == self.tree_of(&tip)? {
                if !m.files.is_empty() {
                    // Empty only after resolving a conflict: the rules
                    // threw this commit's change away. Hand back the
                    // unresolved merge, markers included.
                    let raw = merge_commits(
                        &self.repo,
                        Some(&c.parents[0]),
                        &tip,
                        &c.id,
                        &Rules::disabled(),
                        &sides,
                    )?;
                    return Ok(Replayed::Conflict {
                        commit: c.id.clone(),
                        subject: c.subject(),
                        paths: m.files.iter().map(|f| f.path.clone()).collect(),
                        partial: tip,
                        tree: raw.tree,
                    });
                }
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
            match self.replay(&name, &commits, &onto, &self.rules)? {
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
                    partial,
                    tree,
                } => {
                    if self.hand_off {
                        let rest = commits
                            .iter()
                            .skip_while(|x| x.id != commit)
                            .skip(1)
                            .count();
                        result.handed_off.push(HandOff {
                            branch: name.clone(),
                            parent: b.parent.clone(),
                            old: old.clone(),
                            onto: onto.clone(),
                            partial,
                            commit,
                            subject,
                            paths,
                            tree,
                            rest,
                        });
                        new_tips.insert(name.clone(), old);
                        skip.insert(name);
                        continue;
                    }
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
            moved_from: None,
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

    /// Put the resolution of a [`HandOff`] on the stack: `tree` is the
    /// resolved merge of `h.commit` onto `h.partial`. The commit keeps the
    /// original message and author, the branch's later commits are replayed
    /// on it, the branch moves, and the branches above it restack. Refuses
    /// with `branch_moved` when the branch changed since the hand-off. A
    /// later commit that conflicts too is handed off again (with
    /// [`Window::hand_off`]) or fails with `conflict`.
    pub fn finish_hand_off(&mut self, h: &HandOff, tree: &str) -> Result<RestackResult> {
        self.settle()?;
        self.require_tracked(&h.branch)?;
        let tip = self.branch_tip(&h.branch).ok_or_else(|| {
            stack_err_with(
                "branch_missing",
                format!("branch {} is gone", h.branch),
                json!({ "branch": h.branch }),
            )
        })?;
        if tip != h.old {
            return Err(stack_err_with(
                "branch_moved",
                format!(
                    "{} changed since its conflict was handed off; restack it again",
                    h.branch
                ),
                json!({ "branch": h.branch, "tip": tip, "old": h.old }),
            ));
        }
        let info = self
            .commits_between(&format!("{}^", h.commit), &h.commit)?
            .pop()
            .ok_or_else(|| stack_err("bad_ref", format!("no commit {}", h.commit)))?;
        let resolved = if tree == self.tree_of(&h.partial)? {
            // The change is already in the parent.
            h.partial.clone()
        } else {
            self.repo.git_env(
                &["commit-tree", tree, "-p", &h.partial, "-F", "-"],
                Some(&info.message),
                &[
                    ("GIT_AUTHOR_NAME", info.author_name.as_str()),
                    ("GIT_AUTHOR_EMAIL", info.author_email.as_str()),
                    ("GIT_AUTHOR_DATE", info.author_date.as_str()),
                ],
            )?
        };
        let rest = self.commits_between(&h.commit, &h.old)?;
        let mut result = RestackResult::default();
        match self.replay(&h.branch, &rest, &resolved, &self.rules)? {
            Replayed::Done { tip, replayed, .. } => {
                let update = BranchUpdate {
                    branch: h.branch.clone(),
                    old: h.old.clone(),
                    new: tip,
                    onto: h.onto.clone(),
                    commits: replayed + 1,
                    dropped: 0,
                };
                self.apply(std::slice::from_ref(&update))?;
                if let Some(i) = self.find(&h.branch) {
                    self.state.branches[i].parent_rev = h.onto.clone();
                }
                self.save()?;
                result.updated.push(update);
            }
            Replayed::Conflict {
                commit,
                subject,
                paths,
                partial,
                tree,
            } => {
                let next = HandOff {
                    rest: rest.iter().skip_while(|x| x.id != commit).skip(1).count(),
                    commit,
                    subject,
                    paths,
                    partial,
                    tree,
                    ..h.clone()
                };
                if !self.hand_off {
                    let c = StackConflict {
                        branch: next.branch.clone(),
                        commit: next.commit.clone(),
                        subject: next.subject.clone(),
                        onto: next.onto.clone(),
                        paths: next.paths.clone(),
                    };
                    return Err(stack_err_with(
                        "conflict",
                        format!(
                            "{} conflicts again in {} (commit \"{}\"); nothing was changed",
                            c.branch,
                            c.paths.join(", "),
                            c.subject
                        ),
                        serde_json::to_value(&c).expect("serializes"),
                    ));
                }
                result.handed_off.push(next);
                return Ok(result);
            }
        }
        // The branches above it, and the branch itself if its parent moved
        // on again in the meantime.
        let scope = self.upstack(&h.branch);
        let more = self.restack(Some(&scope), false)?;
        result.updated.extend(more.updated);
        result.unchanged.extend(more.unchanged);
        result.handed_off.extend(more.handed_off);
        result.conflict = more.conflict;
        Ok(result)
    }

    // ---- sync ----

    /// Whether `b` is merged into `base`: its tip is in the base and the
    /// branch had commits of its own there, or its commits replay onto the
    /// base as no change (squash or rebase merges). The replay uses no
    /// resolution rules: any conflict means not merged (#200).
    fn is_merged(&self, b: &Tracked, base: &str) -> Result<bool> {
        let Some(tip) = self.branch_tip(&b.name) else {
            return Ok(false);
        };
        if self.repo.is_ancestor(&tip, base) {
            // A branch with no commits of its own is new, not merged; so is
            // one only fast-forwarded to trunk commits (#200).
            return Ok(tip != b.parent_rev && self.had_own_commits(&b.name, &b.parent_rev, &tip));
        }
        let from = self.replay_from(b, &tip, base);
        let commits = self.commits_between(&from, &tip)?;
        if commits.is_empty() {
            return Ok(false);
        }
        Ok(
            match self.replay(&b.name, &commits, base, &Rules::disabled()) {
                Ok(Replayed::Done { replayed, kept, .. }) => replayed == 0 && kept == 0,
                _ => false,
            },
        )
    }

    /// Whether some commit in `from..tip` was made on `branch` (committed,
    /// amended, cherry-picked, rebased or restacked there) rather than only
    /// reached by moving the branch to existing commits (fast-forward,
    /// reset, `branch -f`), going by the branch's reflog. Without a reflog
    /// it cannot tell, and answers yes.
    fn had_own_commits(&self, branch: &str, from: &str, tip: &str) -> bool {
        let Ok(log) = self.repo.git_raw(&[
            "log",
            "-g",
            "--format=%H%x1f%gs",
            &format!("refs/heads/{branch}"),
            "--",
        ]) else {
            return true;
        };
        let entries: Vec<(&str, &str)> = log.lines().filter_map(|l| l.split_once('\x1f')).collect();
        if entries.is_empty() {
            return true;
        }
        let Ok(range) = self.repo.git_raw(&["rev-list", &format!("{from}..{tip}")]) else {
            return true;
        };
        let range: HashSet<&str> = range.lines().map(str::trim).collect();
        entries.iter().any(|(id, subject)| {
            let s = subject.to_ascii_lowercase();
            range.contains(id)
                && !s.is_empty()
                && !s.starts_with("branch:")
                && !s.starts_with("reset:")
                && !s.contains("fast-forward")
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
        let mut pruned = Vec::new();
        if fetch {
            fetched = self.fetch_trunk()?;
            if fetched {
                pruned = self.prune_tracking_refs();
            }
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
        let mut trashed = Vec::new();
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
                    if elsewhere.contains_key(m)
                        || self.current_branch().as_deref() == Some(m.as_str())
                    {
                        continue;
                    }
                    // Keep the old tip first, so a wrong call is one
                    // command to undo: `git branch <m> refs/agentvc/trash/<m>`.
                    let Some(tip) = self.branch_tip(m) else {
                        continue;
                    };
                    let trash = trash_ref(m);
                    if self
                        .repo
                        .git(
                            &[
                                "update-ref",
                                "-m",
                                "agentvc stack sync: deleted merged branch",
                                &trash,
                                &tip,
                            ],
                            None,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    if self.repo.git(&["branch", "-D", m], None).is_ok() {
                        trashed.push(Trashed {
                            branch: m.clone(),
                            tip,
                            restore: format!("git branch {m} {trash}"),
                            trash_ref: trash,
                        });
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
            pruned,
            base,
            merged,
            deleted,
            trashed,
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
        let name = self.new_branch_name(name, message)?;
        if self.branch_tip(&name).is_some() {
            return Err(Error::BranchExists(name));
        }
        let head = self
            .repo
            .rev("HEAD")
            .ok_or_else(|| stack_err("no_commits", "this repository has no commits yet"))?;
        // `-m` without `-a` commits only what is staged. With changes in the
        // worktree but nothing staged that would make an empty branch the
        // user then cannot recreate with `-am` (`branch_exists`): refuse.
        if message.is_some()
            && !all
            && self
                .repo
                .git(&["diff", "--cached", "--quiet"], None)
                .is_ok()
        {
            let dirty = self
                .repo
                .git(&["status", "--porcelain", "--untracked-files=normal"], None)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if dirty {
                return Err(stack_err_with(
                    "nothing_staged",
                    format!(
                        "nothing is staged, so -m would make {name} without a commit; add -a to commit every change (`agentvc stack create {name} -am <message>`), or stage files first"
                    ),
                    json!({ "branch": name, "hint": "add -a" }),
                ));
            }
        }
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
            pr_status: None,
            pushed: None,
        };
        self.state.branches.push(t.clone());
        self.save()?;
        let commit = self.commit_staged(message, all)?;
        Ok((t, commit))
    }

    /// The branch name `create` uses: `name`, or one made from `message`.
    fn new_branch_name(&self, name: Option<&str>, message: Option<&str>) -> Result<String> {
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
            || name == self.state.trunk
        {
            return Err(Error::BadBranch(name));
        }
        Ok(name)
    }

    /// Commit the staged changes (all changes with `all`) when `message`
    /// is given and something is staged.
    fn commit_staged(&self, message: Option<&str>, all: bool) -> Result<Option<String>> {
        let Some(m) = message else { return Ok(None) };
        if all {
            self.repo.git(&["add", "-A"], None)?;
        }
        if self
            .repo
            .git(&["diff", "--cached", "--quiet"], None)
            .is_err()
        {
            self.repo.git(&["commit", "--quiet", "-m", m], None)?;
            return Ok(self.repo.rev("HEAD"));
        }
        Ok(None)
    }

    /// `create --reset`: start `name` afresh on the trunk's tip, even if the
    /// branch exists (say, a thread branch whose PR was merged), check it
    /// out and track it on the trunk; then commit like `create`. Local edits
    /// stay. Refuses a branch with commits the trunk does not have
    /// (`unmerged_commits`), and branches of other windows.
    pub fn create_reset(
        &mut self,
        name: Option<&str>,
        message: Option<&str>,
        all: bool,
    ) -> Result<(Tracked, Option<String>)> {
        self.settle()?;
        let name = self.new_branch_name(name, message)?;
        let base = self.require_base()?;
        if let Some(p) = self.owner_elsewhere(&name) {
            return Err(stack_err_with(
                "owned_by_other_window",
                format!("branch {name} belongs to the window at {}", p.display()),
                json!({ "branch": name, "window": p }),
            ));
        }
        if let Some(p) = self.checkouts_elsewhere().get(&name) {
            return Err(stack_err_with(
                "checked_out_elsewhere",
                format!("branch {name} is checked out in {}", p.display()),
                json!({ "branch": name, "worktree": p }),
            ));
        }
        let old_tip = self.branch_tip(&name);
        if let Some(tip) = old_tip.clone() {
            if !self.repo.is_ancestor(&tip, &base) {
                let probe = Tracked {
                    name: name.clone(),
                    parent: self.state.trunk.clone(),
                    parent_rev: self.merge_base(&tip, &base).unwrap_or(base.clone()),
                    pr: None,
                    pr_state: None,
                    pr_status: None,
                    pushed: None,
                };
                if !self.is_merged(&probe, &base)? {
                    return Err(stack_err_with(
                        "unmerged_commits",
                        format!(
                            "{name} has commits that {} does not have; nothing changed. Rename it (`agentvc stack rename`) or delete it (`agentvc stack delete`) first",
                            self.base_ref()
                        ),
                        json!({ "branch": name }),
                    ));
                }
            }
        }
        if self.current_branch().as_deref() == Some(name.as_str()) {
            self.repo
                .git(&["reset", "--quiet", "--keep", &base], None)?;
        } else {
            self.repo
                .git(&["switch", "--quiet", "-C", &name, &base], None)?;
        }
        // Keep what this window last pushed: the remote branch is usually
        // still the merged PR's head, and the next submit replaces it under
        // that lease instead of refusing it as another checkout's branch.
        // A branch this window never tracked (a fresh clone of a thread
        // branch) whose remote copy is the merged tip it had: that is ours
        // to replace too.
        let pushed = self
            .find(&name)
            .and_then(|i| self.state.branches[i].pushed.clone())
            .or_else(|| {
                let tip = old_tip?;
                let heads = self.remote_heads(std::slice::from_ref(&name))?;
                (heads.get(&name) == Some(&tip)).then_some(tip)
            });
        let t = Tracked {
            name: name.clone(),
            parent: self.state.trunk.clone(),
            parent_rev: base,
            pr: None,
            pr_state: None,
            pr_status: None,
            pushed,
        };
        match self.find(&name) {
            Some(i) => self.state.branches[i] = t.clone(),
            None => self.state.branches.push(t.clone()),
        }
        self.save()?;
        let commit = self.commit_staged(message, all)?;
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
            // A remote copy (as last fetched) that the branch contains is
            // taken over: a later submit's lease expects it. One with commits
            // the branch lacks is left to submit's check (#201).
            let pushed = self
                .repo
                .rev(&format!("refs/remotes/{}/{name}", self.state.remote))
                .filter(|r| self.repo.is_ancestor(r, &tip));
            let t = Tracked {
                name: name.clone(),
                parent: parent.clone(),
                parent_rev,
                pr: None,
                pr_state: None,
                pr_status: None,
                pushed,
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
            Ok(r) => {
                // Stopped on the moved branch itself: remember where it came
                // from, so an abort puts it back.
                if let Some(p) = self.state.pending.as_mut().filter(|p| p.branch == branch) {
                    p.moved_from = Some(old);
                    self.save()?;
                }
                Ok(r)
            }
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
        self.state.branches[i].pushed = None;
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

    /// `create` with `--insert`: the new branch goes between the current
    /// branch and the branches stacked on it, which are restacked onto it.
    pub fn insert(
        &mut self,
        name: Option<&str>,
        message: Option<&str>,
        all: bool,
        keep_conflicts: bool,
    ) -> Result<(Tracked, Option<String>, RestackResult)> {
        self.settle()?;
        let cur = self.require_current()?;
        let kids = self.children_of(&cur);
        let (t, commit) = self.create(name, message, all)?;
        let mut scope = HashSet::new();
        for c in self.state.branches.iter_mut() {
            if kids.contains(&c.name) {
                // parent_rev stays the old tip of `cur`, which is where the
                // new branch starts: restack replays only the kids' commits.
                c.parent = t.name.clone();
            }
        }
        self.save()?;
        for k in &kids {
            scope.extend(self.upstack(k));
        }
        let r = if scope.is_empty() {
            RestackResult::default()
        } else {
            self.restack(Some(&scope), keep_conflicts)?
        };
        Ok((t, commit, r))
    }

    /// Fold the current branch into its parent (`gt fold`): the parent's
    /// ref moves to this branch's tip, branches on this one move to the
    /// parent, this branch is deleted and the parent checked out.
    pub fn fold(&mut self, keep_conflicts: bool) -> Result<FoldResult> {
        self.settle()?;
        let cur = self.require_current()?;
        let b = self.state.branches[self.find(&cur).expect("tracked")].clone();
        if !self.tracks(&b.parent) {
            return Err(stack_err_with(
                "at_bottom",
                format!(
                    "{cur} sits on {}, not on a branch of this window; land it through its PR instead",
                    if b.parent == self.state.trunk { self.base_ref() } else { b.parent.clone() }
                ),
                json!({ "branch": cur, "parent": b.parent }),
            ));
        }
        let parent_tip = self.parent_tip(&b.parent)?;
        let tip = self.branch_tip(&cur).expect("checked out");
        if b.parent_rev != parent_tip || !self.repo.is_ancestor(&parent_tip, &tip) {
            return Err(stack_err_with(
                "needs_restack",
                format!(
                    "{cur} is not on {}'s tip; run `agentvc stack restack` first",
                    b.parent
                ),
                json!({ "branch": cur, "parent": b.parent }),
            ));
        }
        if let Some(p) = self.checkouts_elsewhere().get(&b.parent) {
            return Err(stack_err_with(
                "checked_out_elsewhere",
                format!("branch {} is checked out in {}", b.parent, p.display()),
                json!({ "branch": b.parent, "worktree": p }),
            ));
        }
        self.repo.git(
            &[
                "update-ref",
                "-m",
                "agentvc stack fold",
                &format!("refs/heads/{}", b.parent),
                &tip,
                &parent_tip,
            ],
            None,
        )?;
        self.repo.git(&["switch", "--quiet", &b.parent], None)?;
        self.repo.git(&["branch", "-D", &cur], None)?;
        for c in self.state.branches.iter_mut() {
            if c.parent == cur {
                c.parent = b.parent.clone();
            }
        }
        let i = self.find(&cur).expect("tracked");
        self.state.branches.remove(i);
        self.save()?;
        let mut scope = self.upstack(&b.parent);
        scope.remove(&b.parent);
        let restack = if scope.is_empty() {
            RestackResult::default()
        } else {
            self.restack(Some(&scope), keep_conflicts)?
        };
        Ok(FoldResult {
            folded: cur,
            into: b.parent,
            closed_pr: b.pr,
            restack,
        })
    }

    /// Split the current branch by commit (`gt split --by-commit`): every
    /// commit but the last gets its own branch below it, named by `names`
    /// (bottom first) or `<branch>-1`, `<branch>-2`, ... No commit changes.
    pub fn split_by_commit(&mut self, names: &[String]) -> Result<Vec<Tracked>> {
        self.settle()?;
        let cur = self.require_current()?;
        let i = self.find(&cur).expect("tracked");
        let b = self.state.branches[i].clone();
        let tip = self.branch_tip(&cur).expect("checked out");
        let onto = self.parent_tip(&b.parent)?;
        let from = self.replay_from(&b, &tip, &onto);
        let commits = self.commits_between(&from, &tip)?;
        if let Some(c) = commits.iter().find(|c| c.parents.len() != 1) {
            return Err(stack_err_with(
                "merge_commit",
                format!(
                    "{cur} contains merge commit {}; split works on linear branches",
                    &c.id[..12]
                ),
                json!({ "branch": cur, "commit": c.id }),
            ));
        }
        if commits.len() < 2 {
            return Err(stack_err_with(
                "nothing_to_split",
                format!(
                    "{cur} has {} commit(s); split needs two or more",
                    commits.len()
                ),
                json!({ "branch": cur }),
            ));
        }
        let n = commits.len() - 1;
        let names: Vec<String> = if names.is_empty() {
            (1..=n).map(|k| format!("{cur}-{k}")).collect()
        } else if names.len() == n {
            names.to_vec()
        } else {
            return Err(stack_err_with(
                "bad_names",
                format!(
                    "{cur} has {} commits: give {n} names for the branches below it (bottom first)",
                    n + 1
                ),
                json!({ "branch": cur, "expected": n }),
            ));
        };
        for name in &names {
            if self
                .repo
                .git(&["check-ref-format", "--branch", name], None)
                .is_err()
                || name.starts_with('-')
            {
                return Err(Error::BadBranch(name.clone()));
            }
            if self.branch_tip(name).is_some() {
                return Err(Error::BranchExists(name.clone()));
            }
        }
        let mut added = Vec::new();
        let (mut parent, mut parent_rev) = (b.parent.clone(), from.clone());
        for (name, c) in names.iter().zip(&commits) {
            self.repo.git(&["branch", name, &c.id], None)?;
            added.push(Tracked {
                name: name.clone(),
                parent: parent.clone(),
                parent_rev: parent_rev.clone(),
                pr: None,
                pr_state: None,
                pr_status: None,
                pushed: None,
            });
            parent = name.clone();
            parent_rev = c.id.clone();
        }
        self.state.branches[i].parent = parent;
        self.state.branches[i].parent_rev = parent_rev;
        for (k, t) in added.iter().enumerate() {
            self.state.branches.insert(i + k, t.clone());
        }
        self.save()?;
        Ok(added)
    }

    /// The current branch and the tracked branches below it, top first.
    fn downstack(&self, from: &str) -> Vec<String> {
        let mut out = vec![from.to_string()];
        let mut at = from.to_string();
        while let Some(i) = self.find(&at) {
            let p = self.state.branches[i].parent.clone();
            if !self.tracks(&p) || out.contains(&p) {
                break;
            }
            out.push(p.clone());
            at = p;
        }
        out
    }

    /// Files a branch's own commits change.
    fn own_files(&self, b: &Tracked) -> Result<HashSet<String>> {
        let Some(tip) = self.branch_tip(&b.name) else {
            return Ok(HashSet::new());
        };
        let from = match self.parent_tip(&b.parent) {
            Ok(onto) => self.replay_from(b, &tip, &onto),
            Err(_) => b.parent_rev.clone(),
        };
        Ok(self
            .repo
            .git(&["diff", "--name-only", "--no-renames", &from, &tip], None)?
            .lines()
            .map(str::to_string)
            .collect())
    }

    /// Amend each staged file into the branch of the current stack whose
    /// commits last changed it (`gt absorb`, by file), then restack. Files
    /// no branch below changed stay staged. On a conflict nothing changes.
    pub fn absorb(&mut self, all: bool, keep_conflicts: bool) -> Result<AbsorbResult> {
        self.settle()?;
        let cur = self.require_current()?;
        if all {
            self.repo.git(&["add", "-A"], None)?;
        }
        let staged: Vec<String> = self
            .repo
            .git(&["diff", "--cached", "--name-only", "--no-renames"], None)?
            .lines()
            .map(str::to_string)
            .collect();
        if staged.is_empty() {
            return Err(stack_err(
                "nothing_staged",
                "nothing is staged; `git add` the changes to absorb (or pass -a)",
            ));
        }
        let down = self.downstack(&cur);
        let mut owners: Vec<(String, Vec<String>)> = Vec::new();
        let mut left = Vec::new();
        let files_of: Vec<(String, HashSet<String>)> = down
            .iter()
            .map(|n| {
                let b = self.state.branches[self.find(n).expect("tracked")].clone();
                self.own_files(&b).map(|f| (n.clone(), f))
            })
            .collect::<Result<_>>()?;
        for f in &staged {
            match files_of.iter().find(|(_, fs)| fs.contains(f)) {
                Some((n, _)) => match owners.iter_mut().find(|(o, _)| o == n) {
                    Some((_, v)) => v.push(f.clone()),
                    None => owners.push((n.clone(), vec![f.clone()])),
                },
                None => left.push(f.clone()),
            }
        }
        if owners.is_empty() {
            return Err(stack_err_with(
                "nothing_to_absorb",
                "no branch of this stack changed the staged files; commit them with `agentvc stack modify` or `create`",
                json!({ "paths": left }),
            ));
        }
        let absorbed: Vec<String> = owners.iter().flat_map(|(_, v)| v.clone()).collect();
        let mut args = vec!["diff", "--name-only", "--"];
        args.extend(absorbed.iter().map(String::as_str));
        let unstaged = self.repo.git(&args, None)?;
        if !unstaged.is_empty() {
            return Err(stack_err_with(
                "dirty_worktree",
                format!(
                    "{} also have unstaged changes; stage or stash them first",
                    unstaged.lines().collect::<Vec<_>>().join(", ")
                ),
                json!({ "paths": unstaged.lines().collect::<Vec<_>>() }),
            ));
        }
        // New commits for each owner, made with a scratch index.
        // Ask git for the path: on Windows `git_dir` is a `\\?\` path,
        // which git cannot lock files under.
        let index_s = self
            .repo
            .git(&["rev-parse", "--git-path", "agentvc/absorb.index"], None)?;
        let index = self.repo.workdir.join(&index_s);
        fs::create_dir_all(index.parent().expect("has a parent")).at(&index)?;
        let mut updates = Vec::new();
        let mut targets = Vec::new();
        for (owner, files) in &owners {
            let mut a = vec!["diff", "--cached", "--no-renames", "--full-index", "--"];
            a.extend(files.iter().map(String::as_str));
            let patch = self.repo.git_raw(&a)?;
            let old = self.branch_tip(owner).expect("tracked branch exists");
            let env = [("GIT_INDEX_FILE", index_s.as_str())];
            self.repo.git_env(&["read-tree", &old], None, &env)?;
            if self
                .repo
                .git_env(&["apply", "--cached", "-"], Some(&patch), &env)
                .is_err()
            {
                let _ = fs::remove_file(&index);
                return Err(stack_err_with(
                    "conflict",
                    format!(
                        "the staged changes to {} do not apply to {owner}; nothing was changed",
                        files.join(", ")
                    ),
                    json!({ "branch": owner, "paths": files }),
                ));
            }
            let tree = self.repo.git_env(&["write-tree"], None, &env)?;
            let c = &self.commits_between(&format!("{old}~1"), &old)?[0];
            let new = self.repo.git_env(
                &["commit-tree", &tree, "-p", &c.parents[0], "-F", "-"],
                Some(&c.message),
                &[
                    ("GIT_AUTHOR_NAME", c.author_name.as_str()),
                    ("GIT_AUTHOR_EMAIL", c.author_email.as_str()),
                    ("GIT_AUTHOR_DATE", c.author_date.as_str()),
                ],
            )?;
            updates.push(BranchUpdate {
                branch: owner.clone(),
                old: old.clone(),
                new,
                onto: c.parents[0].clone(),
                commits: 1,
                dropped: 0,
            });
            targets.push(owner.clone());
        }
        let _ = fs::remove_file(&index);
        // Take the absorbed changes out of the worktree (they now live in
        // the owners' commits), move the owners, then restack above them.
        let mut a = vec![
            "diff",
            "--cached",
            "--no-renames",
            "--full-index",
            "--binary",
            "--",
        ];
        a.extend(absorbed.iter().map(String::as_str));
        let saved = self.repo.git_raw(&a)?;
        let mut r = vec!["restore", "--staged", "--worktree", "--"];
        r.extend(absorbed.iter().map(String::as_str));
        self.repo.git(&r, None)?;
        let undo = |w: &Window| {
            let _ = w.repo.git(&["apply", "--index", "-"], Some(&saved));
        };
        if let Err(e) = self.apply(&updates) {
            undo(self);
            return Err(e);
        }
        let mut scope = HashSet::new();
        for t in &targets {
            scope.extend(self.upstack(t));
        }
        let restacked = self.restack(Some(&scope), keep_conflicts);
        if !left.is_empty() {
            // Moving the checked-out branch resets the index: stage them again.
            let mut a = vec!["add", "--"];
            a.extend(left.iter().map(String::as_str));
            let _ = self.repo.git(&a, None);
        }
        match restacked {
            Ok(restack) => Ok(AbsorbResult {
                absorbed: owners
                    .into_iter()
                    .map(|(branch, paths)| AbsorbedInto { branch, paths })
                    .collect(),
                left,
                restack,
            }),
            Err(e) => {
                let back: Vec<BranchUpdate> = updates
                    .iter()
                    .map(|u| BranchUpdate {
                        old: u.new.clone(),
                        new: u.old.clone(),
                        ..u.clone()
                    })
                    .collect();
                let _ = self.apply(&back);
                undo(self);
                Err(e)
            }
        }
    }

    /// Check out a branch of this window by name, or the trunk (detached at
    /// the remote trunk, since the local trunk may be checked out
    /// elsewhere).
    pub fn checkout(&self, branch: &str) -> Result<String> {
        if branch == self.state.trunk || branch == self.base_ref() {
            let base = self.require_base()?;
            self.repo
                .git(&["switch", "--quiet", "--detach", &base], None)?;
            return Ok(self.base_ref());
        }
        self.require_tracked(branch)?;
        self.switch(branch)
    }

    // ---- PR status and merge ----

    fn github_repo(&self, repo: Option<&str>) -> Result<String> {
        if self.state.remote.is_empty() {
            return Err(stack_err("no_remote", "this repository has no remote"));
        }
        crate::github::repo_for(&self.repo.workdir, &self.state.remote, repo)
    }

    /// Fetch checks and review of every tracked branch's PR and record them
    /// in the stack (`stack log --prs`).
    pub fn refresh_prs(&mut self, repo: Option<&str>) -> Result<Vec<(String, PrBrief)>> {
        if !self.state.branches.iter().any(|b| b.pr.is_some()) {
            return Ok(Vec::new());
        }
        let api = Api::new(crate::github::token()?);
        let repo = self.github_repo(repo)?;
        self.refresh_prs_with(&api, &repo)
    }

    pub(crate) fn refresh_prs_with(
        &mut self,
        api: &Api,
        repo: &str,
    ) -> Result<Vec<(String, PrBrief)>> {
        let mut out = Vec::new();
        for i in 0..self.state.branches.len() {
            if self.state.branches[i].pr.is_none() {
                // Opened outside agentvc (GitHub UI, gh): pick it up.
                let name = self.state.branches[i].name.clone();
                self.state.branches[i].pr = find_open_pr(api, repo, &name).ok().flatten();
            }
            let Some(n) = self.state.branches[i].pr else {
                continue;
            };
            if matches!(
                self.state.branches[i].pr_state.as_deref(),
                Some("merged" | "closed")
            ) {
                continue;
            }
            let f = crate::feedback::fetch_with(api, repo, n)?;
            let brief = PrBrief::of(&f);
            let b = &mut self.state.branches[i];
            b.pr_state = Some(if f.state == "open" && f.draft {
                "draft".into()
            } else {
                f.state.clone()
            });
            b.pr_status = Some(brief.clone());
            out.push((b.name.clone(), brief));
        }
        self.save()?;
        Ok(out)
    }

    /// Merge the stack's PRs on GitHub from the bottom up to `opts.branch`
    /// (default: the current branch), like Graphite's merge. Each PR must
    /// be pushed, out of draft, mergeable, with passing checks and no
    /// change requests. After each merge the stack is synced and the rest
    /// re-submitted, so the next PR targets the trunk; that usually starts
    /// its checks again, and the command stops there (`checks_pending`).
    /// Run it again when they pass. Never waits.
    pub fn merge_stack(&mut self, opts: &MergeOptions) -> Result<MergeResult> {
        let api = Api::new(crate::github::token()?);
        self.merge_stack_with(opts, &api)
    }

    pub(crate) fn merge_stack_with(
        &mut self,
        opts: &MergeOptions,
        api: &Api,
    ) -> Result<MergeResult> {
        self.settle()?;
        let repo = self.github_repo(opts.repo.as_deref())?;
        let target = match &opts.branch {
            Some(b) => {
                self.require_tracked(b)?;
                b.clone()
            }
            None => self.require_current()?,
        };
        let method = opts.method.clone().unwrap_or_else(|| "squash".into());
        if !matches!(method.as_str(), "squash" | "merge" | "rebase") {
            return Err(stack_err(
                "bad_method",
                format!("merge method {method}: use squash, merge or rebase"),
            ));
        }
        let mut chain = self.downstack(&target);
        chain.reverse();
        let bottom = &self.state.branches[self.find(&chain[0]).expect("tracked")];
        if bottom.parent != self.state.trunk {
            return Err(stack_err_with(
                "not_on_trunk",
                format!(
                    "{} sits on {}, another window's branch; merge that one first",
                    bottom.name, bottom.parent
                ),
                json!({ "branch": bottom.name, "parent": bottom.parent }),
            ));
        }
        let mut result = MergeResult {
            repo: repo.clone(),
            merged: Vec::new(),
            stopped: None,
            ready: false,
            statuses: Vec::new(),
        };
        let stop = |b: &str, n: Option<u64>, code: &str, msg: String| MergeStop {
            branch: b.to_string(),
            number: n,
            code: code.to_string(),
            message: msg,
        };
        for name in chain {
            let Some(i) = self.find(&name) else {
                continue; // merged and dropped by an earlier sync
            };
            // A PR opened elsewhere (the GitHub UI, gh, another tool) for
            // this branch counts: find it by head branch and remember it.
            if self.state.branches[i].pr.is_none() {
                if let Some(n) = find_open_pr(api, &repo, &name)? {
                    self.state.branches[i].pr = Some(n);
                    self.save()?;
                }
            }
            let Some(n) = self.state.branches[i].pr else {
                result.stopped = Some(stop(
                    &name,
                    None,
                    "no_pr",
                    format!("{name} has no PR yet; run `agentvc stack submit`"),
                ));
                break;
            };
            let f = crate::feedback::fetch_with(api, &repo, n)?;
            let brief = PrBrief::of(&f);
            self.state.branches[i].pr_status = Some(brief.clone());
            self.state.branches[i].pr_state = Some(f.state.clone());
            self.save()?;
            result.statuses.push((name.clone(), brief.clone()));
            let tip = self.branch_tip(&name);
            let why: Option<(&str, String)> = if f.state == "merged" {
                None
            } else if f.state == "closed" {
                Some(("pr_closed", format!("PR #{n} of {name} is closed")))
            } else if f.draft {
                Some((
                    "pr_draft",
                    format!("PR #{n} of {name} is a draft; mark it ready for review"),
                ))
            } else if f.head_sha != tip {
                Some((
                    "not_pushed",
                    format!("{name} has local changes not on PR #{n}; run `agentvc stack submit`"),
                ))
            } else if brief.checks == "fail" {
                Some((
                    "checks_failed",
                    format!(
                        "checks fail on PR #{n} of {name}: {}",
                        brief.failing.join(", ")
                    ),
                ))
            } else if brief.checks == "pending" {
                Some(("checks_pending", format!("checks are still running on PR #{n} of {name}; run this again when they pass")))
            } else if brief.review == "changes_requested" {
                Some((
                    "changes_requested",
                    format!("changes were requested on PR #{n} of {name}"),
                ))
            } else if brief.mergeable == Some(false)
                || brief.mergeable_state.as_deref() == Some("dirty")
            {
                Some(("not_mergeable", format!("PR #{n} of {name} conflicts with its base; run `agentvc stack sync` and submit")))
            } else if brief.mergeable_state.as_deref() == Some("blocked") {
                Some(("blocked", format!("PR #{n} of {name} is blocked by branch protection (a required review or check)")))
            } else {
                None
            };
            if let Some((code, msg)) = why {
                result.stopped = Some(stop(&name, Some(n), code, msg));
                break;
            }
            if opts.dry_run {
                result.ready = true;
                break;
            }
            let mut sha = f.merge_commit.clone();
            if f.state != "merged" {
                let r = api.call(
                    "PUT",
                    &format!("/repos/{repo}/pulls/{n}/merge"),
                    Some(json!({ "merge_method": method, "sha": f.head_sha })),
                );
                match r {
                    Ok(v) if v["merged"].as_bool() != Some(false) => {
                        sha = v["sha"].as_str().map(str::to_string);
                    }
                    Ok(v) => {
                        result.stopped = Some(stop(
                            &name,
                            Some(n),
                            "merge_failed",
                            format!(
                                "GitHub did not merge PR #{n}: {}",
                                v["message"].as_str().unwrap_or("no reason given")
                            ),
                        ));
                        break;
                    }
                    Err(e) => {
                        result.stopped = Some(stop(
                            &name,
                            Some(n),
                            "merge_failed",
                            format!("GitHub did not merge PR #{n}: {e}"),
                        ));
                        break;
                    }
                }
            }
            if let Some(i) = self.find(&name) {
                self.state.branches[i].pr_state = Some("merged".into());
                self.save()?;
            }
            result.merged.push(MergedPr {
                branch: name.clone(),
                number: n,
                sha,
            });
            // Drop it, put the rest on the trunk, retarget their PRs, then
            // remove the merged branch from the remote.
            if let Err(e) = self.sync_with(true, true, false, Some(api)) {
                result.stopped = Some(stop(
                    &name,
                    Some(n),
                    e.code(),
                    format!("merged PR #{n}, but syncing the stack failed: {e}"),
                ));
                break;
            }
            if self.tracks(&target) {
                let sub = SubmitOptions {
                    branch: Some(target.clone()),
                    repo: Some(repo.clone()),
                    ..Default::default()
                };
                if let Err(e) = self.submit_with(&sub, Some(api)) {
                    result.stopped = Some(stop(
                        &name,
                        Some(n),
                        e.code(),
                        format!("merged PR #{n}, but re-submitting the rest failed: {e}"),
                    ));
                    break;
                }
            }
            let remote = self.state.remote.clone();
            let _ = self
                .repo
                .git(&["push", "--quiet", &remote, "--delete", &name], None);
        }
        if result.merged.is_empty() && !opts.dry_run {
            if let Some(s) = &result.stopped {
                let code: &'static str = match s.code.as_str() {
                    "checks_pending" => "checks_pending",
                    "checks_failed" => "checks_failed",
                    "changes_requested" => "changes_requested",
                    "pr_draft" => "pr_draft",
                    "not_mergeable" => "not_mergeable",
                    "blocked" => "blocked",
                    "not_pushed" => "not_pushed",
                    "no_pr" => "no_pr",
                    "pr_closed" => "pr_closed",
                    _ => "merge_failed",
                };
                return Err(stack_err_with(code, s.message.clone(), json!(s)));
            }
        }
        Ok(result)
    }

    /// Pick up `names` of a stack published from another machine (`state`,
    /// from `refs/agentvc/workspaces/<id>`): fetch the branches, create or
    /// fast-forward them locally and track them here with the published
    /// parents (`stack get`, like `gt get`). Running it again brings later
    /// pushes from that machine. Nothing changes when a local branch has
    /// commits the remote one lacks (`branch_diverged`) or another window
    /// here tracks it.
    pub fn get_published(&mut self, state: &State, names: &[String]) -> Result<GetResult> {
        self.settle()?;
        if self.state.remote.is_empty() {
            return Err(stack_err(
                "no_remote",
                "this repository has no remote to fetch from",
            ));
        }
        let remote = self.state.remote.clone();
        let picked: Vec<Tracked> = state
            .branches
            .iter()
            .filter(|b| names.contains(&b.name))
            .cloned()
            .collect();
        // Parents outside the set (another window there): fetched as plain
        // branches, read-only bases here.
        let mut fetch: Vec<String> = picked.iter().map(|b| b.name.clone()).collect();
        for b in &picked {
            if b.parent != state.trunk && !names.contains(&b.parent) && !fetch.contains(&b.parent) {
                fetch.push(b.parent.clone());
            }
        }
        let mut args = vec!["fetch".to_string(), "--quiet".into(), remote.clone()];
        args.push(format!(
            "+refs/heads/{0}:refs/remotes/{remote}/{0}",
            state.trunk
        ));
        for n in &fetch {
            args.push(format!("+refs/heads/{n}:refs/remotes/{remote}/{n}"));
        }
        // What this machine saw of them before: a local branch still there
        // has no work of its own, so a rewritten remote branch may replace it.
        let seen: HashMap<String, Option<String>> = fetch
            .iter()
            .map(|n| {
                (
                    n.clone(),
                    self.repo.rev(&format!("refs/remotes/{remote}/{n}")),
                )
            })
            .collect();
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        self.repo.git(&a, None).map_err(|e| {
            stack_err_with(
                "branch_missing",
                format!("fetching {} from {remote} failed: {e}", fetch.join(", ")),
                json!({ "branches": fetch }),
            )
        })?;
        // Check everything before changing anything.
        let elsewhere = self.checkouts_elsewhere();
        let current = self.current_branch();
        let mut moves: Vec<(String, Option<String>, String)> = Vec::new();
        for n in &fetch {
            let theirs = self
                .repo
                .rev(&format!("refs/remotes/{remote}/{n}"))
                .ok_or_else(|| {
                    stack_err_with(
                        "branch_missing",
                        format!("{n} is not on {remote}"),
                        json!({ "branch": n }),
                    )
                })?;
            let tracked_here = names.contains(n);
            if tracked_here {
                if let Some(p) = self.owner_elsewhere(n) {
                    return Err(stack_err_with(
                        "owned_by_other_window",
                        format!(
                            "branch {n} belongs to the window at {}; nothing was changed",
                            p.display()
                        ),
                        json!({ "branch": n, "window": p }),
                    ));
                }
            }
            let mine = self.branch_tip(n);
            match &mine {
                Some(m) if *m == theirs => continue,
                Some(m)
                    if !self.repo.is_ancestor(m, &theirs)
                        && seen.get(n).cloned().flatten().as_deref() != Some(m.as_str()) =>
                {
                    if !tracked_here {
                        continue; // a base we only read: keep the local one
                    }
                    return Err(stack_err_with(
                        "branch_diverged",
                        format!("local {n} has commits that {remote}/{n} lacks; push or drop them first. Nothing was changed"),
                        json!({ "branch": n }),
                    ));
                }
                _ => {}
            }
            if let Some(p) = elsewhere.get(n) {
                return Err(stack_err_with(
                    "checked_out_elsewhere",
                    format!(
                        "branch {n} is checked out in {}; nothing was changed",
                        p.display()
                    ),
                    json!({ "branch": n, "worktree": p }),
                ));
            }
            moves.push((n.clone(), mine, theirs));
        }
        let mut created = Vec::new();
        let mut updated = Vec::new();
        for (n, old, new) in &moves {
            if current.as_deref() == Some(n.as_str()) {
                self.repo.git(&["reset", "--keep", new], None)?;
            } else {
                let mut a = vec!["update-ref", "-m", "agentvc stack get"];
                let r = format!("refs/heads/{n}");
                a.push(&r);
                a.push(new);
                if let Some(o) = old {
                    a.push(o);
                }
                self.repo.git(&a, None)?;
            }
            if old.is_some() {
                updated.push(n.clone());
            } else {
                created.push(n.clone());
            }
        }
        let mut tracked = Vec::new();
        for b in &picked {
            let t = Tracked {
                name: b.name.clone(),
                parent: if b.parent == state.trunk {
                    self.state.trunk.clone()
                } else {
                    b.parent.clone()
                },
                parent_rev: b.parent_rev.clone(),
                pr: b.pr,
                pr_state: b.pr_state.clone(),
                pr_status: b.pr_status.clone(),
                // Taken over at the remote tip just fetched: later submits
                // from here lease against it.
                pushed: self.repo.rev(&format!("refs/remotes/{remote}/{}", b.name)),
            };
            match self.find(&b.name) {
                Some(i) => self.state.branches[i] = t.clone(),
                None => self.state.branches.push(t.clone()),
            }
            tracked.push(t);
        }
        self.save()?;
        Ok(GetResult {
            tracked,
            created,
            updated,
        })
    }

    // ---- navigation ----

    pub(crate) fn switch(&self, branch: &str) -> Result<String> {
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
    /// What `submit`'s lease expects on the remote for each of `names`
    /// (`None`: the branch must not exist there). That is the commit this
    /// window last pushed. A branch it never pushed may only be pushed when
    /// the remote has no such branch, or one the local branch contains (a
    /// fast-forward loses nothing); else another checkout owns that name and
    /// this refuses with `remote_branch_exists`. Stacks from before the
    /// `pushed` field that recorded a PR lease against the tracking ref once.
    fn submit_leases(
        &self,
        remote: &str,
        names: &[String],
    ) -> Result<HashMap<String, Option<String>>> {
        let mut leases = HashMap::new();
        let mut unseen = Vec::new();
        for n in names {
            let b = &self.state.branches[self.find(n).expect("tracked")];
            match (&b.pushed, b.pr) {
                (Some(p), _) => {
                    leases.insert(n.clone(), Some(p.clone()));
                }
                (None, Some(_)) => {
                    let tracking = self.repo.rev(&format!("refs/remotes/{remote}/{n}"));
                    leases.insert(n.clone(), tracking);
                }
                (None, None) => unseen.push(n.clone()),
            }
        }
        if unseen.is_empty() {
            return Ok(leases);
        }
        let mut args = vec!["ls-remote".to_string(), remote.to_string()];
        args.extend(unseen.iter().map(|n| format!("refs/heads/{n}")));
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self
            .repo
            .git(&a, None)
            .map_err(|e| crate::github::push_error(e, remote, &unseen.join(" ")))?;
        let theirs: HashMap<&str, &str> = out
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .filter_map(|(sha, r)| Some((r.strip_prefix("refs/heads/")?, sha)))
            .collect();
        let mut taken = Vec::new();
        for n in unseen {
            let tip = self.branch_tip(&n).expect("checked");
            match theirs.get(n.as_str()) {
                None => {
                    leases.insert(n, None);
                }
                Some(&r) if r == tip || self.repo.is_ancestor(r, &tip) => {
                    leases.insert(n, Some(r.to_string()));
                }
                Some(_) => taken.push(n),
            }
        }
        if !taken.is_empty() {
            return Err(stack_err_with(
                "remote_branch_exists",
                format!(
                    "{remote} already has {} with commits this window never pushed (another checkout using the same branch name?); nothing was pushed. Rename yours (`agentvc stack rename`), take theirs over (`agentvc stack get`), or overwrite it with `agentvc stack submit --force`",
                    taken.join(", ")
                ),
                json!({ "branches": taken }),
            ));
        }
        Ok(leases)
    }

    /// PRs go through the GitHub API: refuse a remote on another host
    /// before pushing anything (or asking for a token), rather than push and
    /// then fail on the API. `--repo` names the GitHub repository outright.
    fn require_github_remote(&self, opts: &SubmitOptions) -> Result<()> {
        if opts.no_pr || opts.repo.is_some() || self.state.remote.is_empty() {
            return Ok(());
        }
        let remote = &self.state.remote;
        let url = self
            .repo
            .git(&["remote", "get-url", remote], None)
            .unwrap_or_default();
        let url = url.trim();
        if url.is_empty() || crate::github::remote_is_github(url) {
            return Ok(());
        }
        let host = crate::github::remote_host(url).unwrap_or_default();
        Err(stack_err_with(
            "not_github",
            format!(
                "{remote} is on {host}, not GitHub; stack submit opens GitHub PRs, so nothing was pushed. `agentvc stack submit --no-pr` only pushes the branches"
            ),
            json!({ "remote": remote, "url": url, "host": host }),
        ))
    }

    pub fn submit(&mut self, opts: &SubmitOptions) -> Result<SubmitResult> {
        self.require_github_remote(opts)?;
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
        if api.is_some() {
            self.require_github_remote(opts)?;
        }
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
        // Push what changed, all or nothing, with a lease on what this window
        // last pushed, so nobody else's push is overwritten: not the shared
        // tracking ref, which any fetch moves to another checkout's push of
        // the same branch name (#201).
        //
        // A branch deleted on the remote (its PR merged, #184) would fail
        // that lease with "stale info": ask the remote what it has, and
        // lease such a branch against "absent" (dropping its stale tracking
        // ref). A branch that still exists keeps its lease, so a push by
        // someone else is still refused.
        let mut leases = if opts.force {
            HashMap::new()
        } else {
            self.submit_leases(&remote, &names)?
        };
        let heads = if opts.force {
            None
        } else {
            self.remote_heads(&names)
        };
        let mut pushed = Vec::new();
        let mut args: Vec<String> = vec!["push".into(), "--quiet".into(), "--atomic".into()];
        let mut specs = Vec::new();
        for n in &names {
            let tip = self.branch_tip(n).expect("checked");
            let mut tracking = self.repo.rev(&format!("refs/remotes/{remote}/{n}"));
            if let Some(h) = &heads {
                match h.get(n) {
                    None => {
                        if let Some(t) = tracking.take() {
                            self.drop_tracking_ref(n, &t);
                        }
                        leases.insert(n.clone(), None);
                    }
                    Some(r) if *r == tip => {
                        leases.insert(n.clone(), Some(r.clone()));
                    }
                    Some(_) => {}
                }
            }
            let lease = leases.get(n).cloned().flatten();
            let on_remote = if opts.force { tracking } else { lease.clone() };
            if on_remote.as_deref() == Some(tip.as_str()) {
                let i = self.find(n).expect("tracked");
                self.state.branches[i].pushed = Some(tip);
                continue;
            }
            if opts.force {
                specs.push(format!("+refs/heads/{n}:refs/heads/{n}"));
            } else {
                args.push(format!(
                    "--force-with-lease=refs/heads/{n}:{}",
                    lease.unwrap_or_default()
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
            for n in &pushed {
                let i = self.find(n).expect("tracked");
                self.state.branches[i].pushed = self.branch_tip(n);
            }
        }
        self.save()?;
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
            // An open PR found by head name is this window's: the leases
            // above refused any branch another checkout pushed (#201).
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
        // Another clone of this space on this machine: its branches are
        // its own, so they are copied over first (#205).
        let other_clone = their_common != self.common_dir;
        if other_clone {
            let (ours, _) = crate::checkouts::space_of(self);
            let theirs = Window::open(to)
                .ok()
                .and_then(|t| crate::checkouts::space_of(&t).0);
            if ours.is_none() || ours != theirs {
                return Err(stack_err(
                    "other_repository",
                    format!(
                        "{} is a checkout of another space; `stack give` hands branches to worktrees and clones of this space on this machine (push the branch and `stack get` it there instead)",
                        to.display()
                    ),
                ));
            }
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
        if other_clone {
            self.copy_branches(&probe, &moving)?;
        }
        let mut theirs = read_state(&their_git)?.unwrap_or_else(|| State {
            version: STATE_VERSION,
            trunk: self.state.trunk.clone(),
            remote: self.state.remote.clone(),
            branches: Vec::new(),
            pending: None,
            agents: Default::default(),
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

    /// Copy `moving` (and the bases they sit on, when `to` lacks them)
    /// from this clone into the clone `to`, with their upstreams. Nothing
    /// is copied when one of `moving` already exists there at another
    /// commit.
    fn copy_branches(&self, to: &HostRepo, moving: &[String]) -> Result<()> {
        let mut specs = Vec::new();
        for n in moving {
            let tip = self.branch_tip(n).ok_or_else(|| {
                stack_err_with(
                    "no_branch",
                    format!("branch {n} is gone"),
                    json!({ "branch": n }),
                )
            })?;
            match to.rev(&format!("refs/heads/{n}")) {
                Some(t) if t == tip => {}
                Some(_) => {
                    return Err(stack_err_with(
                        "branch_exists",
                        format!(
                            "{} already has a branch {n} at another commit; rename or delete one of them first",
                            to.workdir.display()
                        ),
                        json!({ "branch": n }),
                    ))
                }
                None => specs.push(format!("refs/heads/{n}:refs/heads/{n}")),
            }
        }
        for n in moving {
            let p = &self.state.branches[self.find(n).expect("tracked")].parent;
            if *p != self.state.trunk
                && !moving.contains(p)
                && self.branch_tip(p).is_some()
                && to.rev(&format!("refs/heads/{p}")).is_none()
                && !specs
                    .iter()
                    .any(|s| s.starts_with(&format!("refs/heads/{p}:")))
            {
                // A read-only base there, as it is for a worktree.
                specs.push(format!("refs/heads/{p}:refs/heads/{p}"));
            }
        }
        if !specs.is_empty() {
            let src = self.repo.workdir.to_string_lossy().into_owned();
            let mut args = vec!["fetch", "--quiet", "--no-tags", src.as_str()];
            args.extend(specs.iter().map(String::as_str));
            to.git(&args, None)?;
        }
        // Same remote: bring the remote-tracking refs and upstreams, so
        // `submit` there knows what is already pushed.
        let remote = &self.state.remote;
        let url = |r: &HostRepo| r.git(&["remote", "get-url", remote], None).ok();
        if remote.is_empty() || url(&self.repo).is_none() || url(&self.repo) != url(to) {
            return Ok(());
        }
        for n in moving {
            let tracking = format!("refs/remotes/{remote}/{n}");
            if self.repo.rev(&tracking).is_some() && to.rev(&tracking).is_none() {
                let src = self.repo.workdir.to_string_lossy().into_owned();
                let spec = format!("{tracking}:{tracking}");
                let _ = to.git(&["fetch", "--quiet", "--no-tags", &src, &spec], None);
            }
            for key in ["remote", "merge"] {
                let k = format!("branch.{n}.{key}");
                if let Ok(v) = self.repo.git(&["config", "--get", &k], None) {
                    let _ = to.git(&["config", &k, v.trim()], None);
                }
            }
        }
        Ok(())
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
                pr_state: b.pr_state.clone(),
                pr_status: b.pr_status.clone(),
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

    /// #204: every save keeps the previous good state in `stack.json.bak`;
    /// a truncated `stack.json` falls back to it, and doctor's repair puts
    /// it back.
    #[test]
    fn truncated_state_recovers_from_backup() {
        let f = setup();
        let mut w = Window::open(&f.w2).unwrap();
        fs::write(f.w2.join("p.txt"), "p\n").unwrap();
        w.create(Some("p"), Some("p"), true).unwrap();
        let good = fs::read_to_string(state_path(&w.git_dir)).unwrap();
        fs::write(f.w2.join("q.txt"), "q\n").unwrap();
        w.create(Some("q"), Some("q"), true).unwrap();
        assert_eq!(
            fs::read_to_string(backup_path(&w.git_dir)).unwrap(),
            good,
            "the state before the last save is the backup"
        );
        // A crash or full disk cut the file short.
        let full = fs::read_to_string(state_path(&w.git_dir)).unwrap();
        fs::write(state_path(&w.git_dir), &full[..full.len() / 2]).unwrap();
        assert!(matches!(
            check_state(&w.git_dir),
            StateHealth::Bad { backup: true, .. }
        ));
        let again = Window::open(&f.w2).unwrap();
        let names: Vec<&str> = again
            .state
            .branches
            .iter()
            .map(|b| b.name.as_str())
            .collect();
        assert_eq!(names, ["p"]);
        // Saving over a bad file must not replace the good backup with it.
        again.save().unwrap();
        assert_eq!(fs::read_to_string(backup_path(&w.git_dir)).unwrap(), good);

        fs::write(state_path(&w.git_dir), "").unwrap();
        assert!(restore_backup(&w.git_dir).unwrap());
        assert_eq!(check_state(&w.git_dir), StateHealth::Ok);
        assert!(state_path(&w.git_dir).with_extension("json.bad").exists());
        assert!(!restore_backup(&w.git_dir).unwrap(), "nothing left to do");

        // No usable backup: a clear error.
        fs::write(state_path(&w.git_dir), "{").unwrap();
        fs::write(backup_path(&w.git_dir), "").unwrap();
        let e = match Window::open(&f.w2) {
            Ok(_) => panic!("opened a bad state"),
            Err(e) => e,
        };
        assert_eq!(e.code(), "bad_stack_state");
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
    fn create_with_message_refuses_when_only_unstaged_changes() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("a.txt"), "changed\n").unwrap();
        let e = w.create(Some("feat"), Some("msg"), false).unwrap_err();
        assert_eq!(e.code(), "nothing_staged");
        assert!(e.to_string().contains("add -a"), "{e}");
        // Nothing was created, so the `-am` retry works.
        let mut w = Window::open(&f.w1).unwrap();
        assert!(w.state.branches.is_empty());
        let (_, commit) = w.create(Some("feat"), Some("msg"), true).unwrap();
        assert!(commit.is_some());
        // A clean tree still makes an empty branch to commit on later.
        let (_, commit) = w.create(Some("next"), Some("msg2"), false).unwrap();
        assert!(commit.is_none());
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
        assert_eq!(rev(&f.w1, "refs/agentvc/trash/s1"), s1, "old tip kept");
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
        fake_github_on(None)
    }

    /// With `origin`: PR heads are read from it, check runs report each
    /// PR's `x_checks` (default `success`), and merging a PR squashes its
    /// branch into origin's main.
    fn fake_github_on(
        origin: Option<PathBuf>,
    ) -> (
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
                let head_of = |pr: &serde_json::Value| -> Option<String> {
                    let o = origin.as_ref()?;
                    let r = pr["head"]["ref"].as_str()?;
                    let out = Command::new("git")
                        .arg("--git-dir")
                        .arg(o)
                        .args([
                            "rev-parse",
                            "--verify",
                            "--quiet",
                            &format!("refs/heads/{r}"),
                        ])
                        .output()
                        .ok()?;
                    out.status
                        .success()
                        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
                };
                if origin.is_some() {
                    for pr in prs.iter_mut() {
                        if pr["merged"] != json!(true) {
                            if let Some(h) = head_of(pr) {
                                pr["head"]["sha"] = json!(h);
                            }
                        }
                    }
                }
                let out = if path.contains("/check-runs") {
                    let sha = path.split('/').nth(5).unwrap_or_default();
                    let c = prs
                        .iter()
                        .find(|p| p["head"]["sha"] == sha)
                        .and_then(|p| p["x_checks"].as_str().map(str::to_string))
                        .unwrap_or_else(|| "success".into());
                    if c == "in_progress" {
                        json!({ "check_runs": [{ "name": "ci", "status": "in_progress" }] })
                    } else {
                        json!({ "check_runs": [{ "name": "ci", "status": "completed", "conclusion": c }] })
                    }
                } else if path.ends_with("/status") {
                    json!({ "statuses": [] })
                } else if path.contains("/reviews")
                    || path.contains("/comments")
                    || path == "/graphql"
                {
                    json!([])
                } else if method == "PUT" && path.ends_with("/merge") {
                    let i = num(&path).unwrap() - 1;
                    let o = origin.clone().expect("merging needs an origin");
                    let tmp = o.parent().unwrap().join(format!("merge-{}", i + 1));
                    let _ = fs::remove_dir_all(&tmp);
                    git(
                        o.parent().unwrap(),
                        &["clone", "-q", o.to_str().unwrap(), tmp.to_str().unwrap()],
                    );
                    let head = prs[i]["head"]["ref"].as_str().unwrap().to_string();
                    git(
                        &tmp,
                        &["merge", "-q", "--squash", &format!("origin/{head}")],
                    );
                    git(&tmp, &["commit", "-q", "-m", &format!("Squash #{}", i + 1)]);
                    git(&tmp, &["push", "-q", "origin", "main"]);
                    prs[i]["merged"] = json!(true);
                    prs[i]["state"] = json!("closed");
                    prs[i]["merged_at"] = json!("2026-11-27T00:00:00Z");
                    json!({ "merged": true, "sha": git(&tmp, &["rev-parse", "HEAD"]) })
                } else if method == "POST" && path == "/repos/o/r/pulls" {
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
    fn submit_refuses_a_non_github_remote_before_pushing() {
        if std::env::var(crate::github::API_ENV).is_ok() {
            return;
        }
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("s.txt"), "x\n").unwrap();
        w.create(Some("s"), Some("S"), true).unwrap();
        git(
            &f.w1,
            &["remote", "set-url", "origin", "https://gitlab.com/o/r.git"],
        );
        let api = Api::new("t".into());
        let e = w
            .submit_with(&SubmitOptions::default(), Some(&api))
            .unwrap_err();
        assert_eq!(e.code(), "not_github", "{e}");
        assert!(e.to_string().contains("--no-pr"), "{e}");
        git(
            &f.w1,
            &["remote", "set-url", "origin", f.origin.to_str().unwrap()],
        );
        assert!(git(&f.w1, &["ls-remote", "--heads", "origin", "s"]).is_empty());
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

    /// Another clone of `origin`, ready to commit.
    fn clone_origin(f: &Fixture, name: &str) -> PathBuf {
        let root = f.origin.parent().unwrap();
        let dir = root.join(name);
        git(
            root,
            &[
                "clone",
                "-q",
                f.origin.to_str().unwrap(),
                dir.to_str().unwrap(),
            ],
        );
        git(&dir, &["config", "user.name", "t"]);
        git(&dir, &["config", "user.email", "t@t"]);
        dir
    }

    #[test]
    fn submit_never_overwrites_another_checkouts_branch_of_the_same_name() {
        // #201: two clones each create `fix-tests`; a submits first.
        let f = setup();
        let opts = SubmitOptions {
            no_pr: true,
            ..Default::default()
        };
        let mut a = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("t1.txt"), "a\n").unwrap();
        a.create(Some("fix-tests"), Some("Fix tests"), true)
            .unwrap();
        commit_file(&f.w1, "t2.txt", "a\n", "More fixes");
        assert_eq!(
            a.submit_with(&opts, None).unwrap().pushed,
            vec!["fix-tests"]
        );
        let a_tip = rev(&f.w1, "fix-tests");
        assert_eq!(a.state.branches[0].pushed.as_deref(), Some(a_tip.as_str()));

        let bdir = clone_origin(&f, "b");
        // b cloned after a's push: its tracking ref has a's commits too.
        git(
            &bdir,
            &["update-ref", "-d", "refs/remotes/origin/fix-tests"],
        );
        let mut b = Window::open(&bdir).unwrap();
        fs::write(bdir.join("t1.txt"), "b\n").unwrap();
        b.create(Some("fix-tests"), Some("Fix tests"), true)
            .unwrap();
        let e = b.submit_with(&opts, None).unwrap_err();
        assert_eq!(e.code(), "remote_branch_exists");
        // An editor's auto-fetch must not make a's push b's to overwrite.
        git(&bdir, &["fetch", "-q", "origin"]);
        let e = b.submit_with(&opts, None).unwrap_err();
        assert_eq!(e.code(), "remote_branch_exists");
        assert_eq!(git(&f.origin, &["rev-parse", "fix-tests"]), a_tip);

        // a amends and resubmits as usual, even with a stale tracking ref.
        git(
            &f.w1,
            &["update-ref", "-d", "refs/remotes/origin/fix-tests"],
        );
        fs::write(f.w1.join("t2.txt"), "a2\n").unwrap();
        a.modify(None, true, false, false).unwrap();
        assert_eq!(
            a.submit_with(&opts, None).unwrap().pushed,
            vec!["fix-tests"]
        );
        assert_eq!(
            git(&f.origin, &["rev-parse", "fix-tests"]),
            rev(&f.w1, "fix-tests")
        );
        // Still refused for b; `--force` is the explicit overwrite.
        git(&bdir, &["fetch", "-q", "origin"]);
        assert_eq!(
            b.submit_with(&opts, None).unwrap_err().code(),
            "remote_branch_exists"
        );
        let force = SubmitOptions {
            force: true,
            ..opts.clone()
        };
        b.submit_with(&force, None).unwrap();
        assert_eq!(
            git(&f.origin, &["rev-parse", "fix-tests"]),
            rev(&bdir, "fix-tests")
        );
        // Now a is the one that has not seen the remote: its lease refuses.
        fs::write(f.w1.join("t2.txt"), "a3\n").unwrap();
        a.modify(None, true, false, false).unwrap();
        git(&f.w1, &["fetch", "-q", "origin"]);
        assert_eq!(
            a.submit_with(&opts, None).unwrap_err().code(),
            "push_rejected"
        );
    }

    #[test]
    fn submit_leases_for_stacks_without_a_recorded_push() {
        let f = setup();
        let opts = SubmitOptions {
            no_pr: true,
            ..Default::default()
        };
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("m.txt"), "1\n").unwrap();
        w.create(Some("m1"), Some("m1"), true).unwrap();
        w.submit_with(&opts, None).unwrap();
        // A stack.json from before `pushed`, with a PR recorded: the
        // tracking ref is the lease once, then the push is recorded.
        w.state.branches[0].pushed = None;
        w.state.branches[0].pr = Some(7);
        w.save().unwrap();
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("m.txt"), "2\n").unwrap();
        w.modify(None, true, false, false).unwrap();
        assert_eq!(w.submit_with(&opts, None).unwrap().pushed, vec!["m1"]);
        let tip = rev(&f.w1, "m1");
        assert_eq!(w.state.branches[0].pushed.as_deref(), Some(tip.as_str()));
        // No PR and no record, but the remote branch is one this branch
        // contains (pushed with plain git): a fast-forward loses nothing.
        w.state.branches[0].pushed = None;
        w.state.branches[0].pr = None;
        commit_file(&f.w1, "m2.txt", "x\n", "more");
        assert_eq!(w.submit_with(&opts, None).unwrap().pushed, vec!["m1"]);
        // `stack track` of a branch pushed with plain git records its
        // remote copy, so amending it later still submits.
        git(&f.w1, &["switch", "-q", "-c", "plain", "main"]);
        commit_file(&f.w1, "p.txt", "p\n", "plain");
        git(&f.w1, &["push", "-q", "origin", "plain"]);
        let t = w.adopt("plain", None, false).unwrap();
        assert_eq!(t[0].pushed, Some(rev(&f.w1, "plain")));
        fs::write(f.w1.join("p.txt"), "p2\n").unwrap();
        w.modify(None, true, false, false).unwrap();
        let only = SubmitOptions {
            branch: Some("plain".into()),
            ..opts.clone()
        };
        assert_eq!(w.submit_with(&only, None).unwrap().pushed, vec!["plain"]);
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
    #[test]
    fn insert_puts_a_branch_between_current_and_its_children() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("x.txt"), "x\n").unwrap();
        w.create(Some("x"), Some("x"), true).unwrap();
        fs::write(f.w1.join("y.txt"), "y\n").unwrap();
        w.create(Some("y"), Some("y"), true).unwrap();
        w.down().unwrap();
        fs::write(f.w1.join("m.txt"), "m\n").unwrap();
        let (t, commit, r) = w.insert(Some("m"), Some("m"), true, false).unwrap();
        assert_eq!(t.parent, "x");
        assert!(commit.is_some());
        assert_eq!(r.updated.len(), 1);
        let w = Window::open(&f.w1).unwrap();
        let y = &w.state.branches[w.find("y").unwrap()];
        assert_eq!(y.parent, "m");
        assert_eq!(y.parent_rev, rev(&f.w1, "m"));
        assert_eq!(git(&f.w1, &["show", "y:m.txt"]), "m");
        assert_eq!(git(&f.w1, &["rev-list", "--count", "m..y"]), "1");
    }

    #[test]
    fn fold_merges_a_branch_into_its_parent() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        for n in ["p", "c", "top"] {
            fs::write(f.w1.join(format!("{n}.txt")), "1\n").unwrap();
            w.create(Some(n), Some(n), true).unwrap();
        }
        w.down().unwrap();
        let c_tip = rev(&f.w1, "c");
        let r = w.fold(false).unwrap();
        assert_eq!((r.folded.as_str(), r.into.as_str()), ("c", "p"));
        assert_eq!(rev(&f.w1, "p"), c_tip);
        assert_eq!(w.current_branch().as_deref(), Some("p"));
        assert!(w.branch_tip("c").is_none());
        let top = &w.state.branches[w.find("top").unwrap()];
        assert_eq!(top.parent, "p");
        // The bottom branch has nothing to fold into.
        assert_eq!(w.fold(false).unwrap_err().code(), "at_bottom");
    }

    #[test]
    fn split_by_commit_gives_each_commit_a_branch() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("one.txt"), "1\n").unwrap();
        w.create(Some("big"), Some("one"), true).unwrap();
        commit_file(&f.w1, "two.txt", "2\n", "two");
        commit_file(&f.w1, "three.txt", "3\n", "three");
        let tip = rev(&f.w1, "big");
        assert_eq!(
            w.split_by_commit(&["only".into()]).unwrap_err().code(),
            "bad_names"
        );
        let added = w.split_by_commit(&[]).unwrap();
        assert_eq!(added.len(), 2);
        assert_eq!(rev(&f.w1, "big"), tip, "no commit changes");
        assert_eq!(git(&f.w1, &["log", "-1", "--format=%s", "big-1"]), "one");
        assert_eq!(git(&f.w1, &["log", "-1", "--format=%s", "big-2"]), "two");
        let l = w.log(false).unwrap();
        let names: Vec<_> = l
            .branches
            .iter()
            .map(|b| (b.name.as_str(), b.commits))
            .collect();
        assert_eq!(names, vec![("big-1", 1), ("big-2", 1), ("big", 1)]);
        assert!(l.branches.iter().all(|b| !b.needs_restack));
        assert_eq!(
            w.split_by_commit(&[]).unwrap_err().code(),
            "nothing_to_split"
        );
    }

    #[test]
    fn absorb_amends_files_into_the_branch_that_changed_them() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("low.txt"), "low\n").unwrap();
        w.create(Some("low"), Some("low"), true).unwrap();
        fs::write(f.w1.join("high.txt"), "high\n").unwrap();
        w.create(Some("high"), Some("high"), true).unwrap();
        fs::write(f.w1.join("low.txt"), "low fixed\n").unwrap();
        fs::write(f.w1.join("high.txt"), "high fixed\n").unwrap();
        fs::write(f.w1.join("new.txt"), "new\n").unwrap();
        let r = w.absorb(true, false).unwrap();
        let mut got: Vec<_> = r
            .absorbed
            .iter()
            .map(|a| (a.branch.clone(), a.paths.clone()))
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("high".to_string(), vec!["high.txt".to_string()]),
                ("low".to_string(), vec!["low.txt".to_string()])
            ]
        );
        assert_eq!(r.left, vec!["new.txt".to_string()]);
        assert_eq!(git(&f.w1, &["show", "low:low.txt"]), "low fixed");
        assert_eq!(git(&f.w1, &["show", "high:high.txt"]), "high fixed");
        assert_eq!(git(&f.w1, &["rev-list", "--count", "low..high"]), "1");
        assert_eq!(
            git(&f.w1, &["rev-list", "--count", "origin/main..low"]),
            "1"
        );
        // Only the unabsorbed file is left, still staged.
        assert_eq!(git(&f.w1, &["status", "--porcelain"]), "A  new.txt");
        git(&f.w1, &["reset", "-q"]);
        fs::remove_file(f.w1.join("new.txt")).unwrap();
        assert_eq!(w.absorb(true, false).unwrap_err().code(), "nothing_staged");
    }

    #[test]
    fn undo_walks_back_and_redoes() {
        let f = setup();
        let cwd = f.w1.clone();
        let mk = |n: &str| {
            crate::stackundo::recorded(&cwd, &format!("create {n}"), || {
                let mut w = Window::open(&cwd)?;
                fs::write(cwd.join(format!("{n}.txt")), "1\n").unwrap();
                w.create(Some(n), Some(n), true).map(|_| ())
            })
            .unwrap()
        };
        mk("u1");
        mk("u2");
        let u1 = rev(&f.w1, "u1");
        crate::stackundo::recorded(&cwd, "delete u1", || {
            Window::open(&cwd)?.delete("u1", false).map(|_| ())
        })
        .unwrap();
        assert!(Window::open(&cwd).unwrap().branch_tip("u1").is_none());

        let mut w = Window::open(&cwd).unwrap();
        let u = crate::stackundo::undo(&mut w, None).unwrap();
        assert_eq!(u.command, "delete u1");
        let w = Window::open(&cwd).unwrap();
        assert_eq!(w.branch_tip("u1").as_deref(), Some(u1.as_str()));
        assert_eq!(w.state.branches.len(), 2);
        assert_eq!(w.current_branch().as_deref(), Some("u2"));

        // Again: undoes `create u2` (the branch goes, u1 is checked out).
        let mut w = Window::open(&cwd).unwrap();
        crate::stackundo::undo(&mut w, None).unwrap();
        let w = Window::open(&cwd).unwrap();
        assert!(w.branch_tip("u2").is_none());
        assert_eq!(w.current_branch().as_deref(), Some("u1"));

        // Undoing that undo redoes the create.
        let mut w = Window::open(&cwd).unwrap();
        let rows = crate::stackundo::list(&w);
        assert_eq!(rows[0].undoes, Some(2));
        crate::stackundo::undo(&mut w, Some(rows[0].id)).unwrap();
        let w = Window::open(&cwd).unwrap();
        assert!(w.branch_tip("u2").is_some());
        assert_eq!(w.current_branch().as_deref(), Some("u2"));

        // A branch that moved since blocks the undo, and changes nothing.
        commit_file(&f.w1, "u2.txt", "2\n", "more");
        let mut w = Window::open(&cwd).unwrap();
        assert_eq!(
            crate::stackundo::undo(&mut w, None).unwrap_err().code(),
            "undo_conflict"
        );
        assert!(w.branch_tip("u2").is_some());
        // Undoing an entry twice is refused.
        assert_eq!(
            crate::stackundo::undo(&mut w, Some(3)).unwrap_err().code(),
            "already_undone"
        );
    }

    #[test]
    fn checkout_by_name_and_trunk() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("k.txt"), "k\n").unwrap();
        w.create(Some("k"), Some("k"), true).unwrap();
        assert_eq!(w.checkout("main").unwrap(), "origin/main");
        assert_eq!(w.current_branch(), None);
        assert_eq!(w.checkout("k").unwrap(), "k");
        assert_eq!(w.checkout("nope").unwrap_err().code(), "not_in_stack");
    }
    #[test]
    fn merge_lands_the_stack_bottom_up_and_stops_on_pending_checks() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        for i in 1..=3 {
            fs::write(f.w1.join(format!("m{i}.txt")), format!("{i}\n")).unwrap();
            w.create(
                Some(&format!("m{i}")),
                Some(&format!("Merge step {i}")),
                true,
            )
            .unwrap();
        }
        let (api, prs) = fake_github_on(Some(f.origin.clone()));
        let sub = SubmitOptions {
            repo: Some("o/r".into()),
            ..Default::default()
        };
        w.submit_with(&sub, Some(&api)).unwrap();
        let st = w.refresh_prs_with(&api, "o/r").unwrap();
        assert_eq!(st.len(), 3);
        assert!(st.iter().all(|(_, b)| b.checks == "pass"));
        assert_eq!(
            w.log(false).unwrap().branches[0]
                .pr_status
                .as_ref()
                .unwrap()
                .checks,
            "pass"
        );

        // Checks still run on m2: m1 merges, the rest is re-submitted onto
        // main, and it stops at m2.
        prs.lock().unwrap()[1]["x_checks"] = json!("in_progress");
        let opts = MergeOptions {
            branch: Some("m3".into()),
            repo: Some("o/r".into()),
            ..Default::default()
        };
        let r = w.merge_stack_with(&opts, &api).unwrap();
        assert_eq!(
            r.merged
                .iter()
                .map(|m| m.branch.as_str())
                .collect::<Vec<_>>(),
            vec!["m1"]
        );
        let stop = r.stopped.unwrap();
        assert_eq!(
            (stop.branch.as_str(), stop.code.as_str()),
            ("m2", "checks_pending")
        );
        assert!(!w.tracks("m1"));
        assert_eq!(prs.lock().unwrap()[1]["base"]["ref"], "main");
        assert!(
            git(&f.origin, &["branch", "--list", "m1"]).is_empty(),
            "merged branch left the remote"
        );

        // Nothing ready: an error with the code, nothing merged.
        let e = w.merge_stack_with(&opts, &api).unwrap_err();
        assert_eq!(e.code(), "checks_pending");

        // Checks pass: m2 and m3 merge in one go.
        prs.lock().unwrap()[1]["x_checks"] = json!("success");
        let dry = MergeOptions {
            dry_run: true,
            ..opts.clone()
        };
        assert!(w.merge_stack_with(&dry, &api).unwrap().ready);
        let r = w.merge_stack_with(&opts, &api).unwrap();
        assert_eq!(r.merged.len(), 2);
        assert!(r.stopped.is_none());
        assert!(w.state.branches.is_empty());
        let main = git(&f.origin, &["ls-tree", "--name-only", "main"]);
        for i in 1..=3 {
            assert!(main.contains(&format!("m{i}.txt")), "{main}");
        }

        // A failing check stops it before merging anything.
        fs::write(f.w1.join("m4.txt"), "4\n").unwrap();
        git(&f.w1, &["switch", "-q", "--detach", "origin/main"]);
        w.create(Some("m4"), Some("m4"), true).unwrap();
        w.submit_with(&sub, Some(&api)).unwrap();
        prs.lock().unwrap()[3]["x_checks"] = json!("failure");
        let o4 = MergeOptions {
            branch: None,
            ..opts
        };
        assert_eq!(
            w.merge_stack_with(&o4, &api).unwrap_err().code(),
            "checks_failed"
        );
    }

    #[test]
    fn merge_finds_a_pr_opened_outside_agentvc() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("x.txt"), "x\n").unwrap();
        w.create(Some("x"), Some("Outside PR"), true).unwrap();
        let (api, _prs) = fake_github_on(Some(f.origin.clone()));
        let sub = SubmitOptions {
            repo: Some("o/r".into()),
            ..Default::default()
        };
        w.submit_with(&sub, Some(&api)).unwrap();
        // As if the PR was opened on GitHub, not by `stack submit`.
        let i = w.find("x").unwrap();
        w.state.branches[i].pr = None;
        w.save().unwrap();
        let opts = MergeOptions {
            repo: Some("o/r".into()),
            ..Default::default()
        };
        // `stack log --prs` (and the daemon's poller) pick it up too.
        w.refresh_prs_with(&api, "o/r").unwrap();
        assert!(w.state.branches[i].pr.is_some());
        w.state.branches[i].pr = None;
        let r = w.merge_stack_with(&opts, &api).unwrap();
        assert_eq!(r.merged.len(), 1, "{:?}", r.stopped);
        assert!(r.stopped.is_none());
        assert!(!w.tracks("x"));
    }

    fn push_only() -> SubmitOptions {
        SubmitOptions {
            no_pr: true,
            ..Default::default()
        }
    }

    /// #184: the remote branch was deleted (merged PR) but the clone's
    /// `origin/<branch>` remains: submit leases against the remote's actual
    /// state and pushes, while a push by someone else is still refused.
    #[test]
    fn submit_after_the_remote_branch_was_deleted() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("d.txt"), "1\n").unwrap();
        w.create(Some("gone"), Some("gone"), true).unwrap();
        assert_eq!(
            w.submit_with(&push_only(), None).unwrap().pushed,
            vec!["gone"]
        );
        git(&f.origin, &["branch", "-D", "gone"]);
        assert!(
            w.repo.rev("refs/remotes/origin/gone").is_some(),
            "stale tracking ref"
        );

        // Unchanged since the last push: pushed again, not skipped.
        assert_eq!(
            w.submit_with(&push_only(), None).unwrap().pushed,
            vec!["gone"]
        );
        assert_eq!(rev(&f.origin, "gone"), rev(&f.w1, "gone"));

        // Amended after the remote deletion: no `stale info`.
        git(&f.origin, &["branch", "-D", "gone"]);
        fs::write(f.w1.join("d.txt"), "2\n").unwrap();
        w.modify(None, true, false, false).unwrap();
        assert_eq!(
            w.submit_with(&push_only(), None).unwrap().pushed,
            vec!["gone"]
        );
        assert_eq!(rev(&f.origin, "gone"), rev(&f.w1, "gone"));
        assert_eq!(rev(&f.w1, "origin/gone"), rev(&f.w1, "gone"));

        // Someone else recreated it on the remote: still refused.
        git(&f.origin, &["branch", "-f", "gone", "main"]);
        fs::write(f.w1.join("d.txt"), "3\n").unwrap();
        w.modify(None, true, false, false).unwrap();
        let e = w.submit_with(&push_only(), None).unwrap_err();
        assert_eq!(e.code(), "push_rejected");
        assert_eq!(rev(&f.origin, "gone"), rev(&f.origin, "main"));
    }

    #[test]
    fn sync_prunes_tracking_refs_of_deleted_remote_branches() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("p.txt"), "1\n").unwrap();
        w.create(Some("keep"), Some("keep"), true).unwrap();
        w.submit_with(&push_only(), None).unwrap();
        git(&f.origin, &["branch", "old", "main"]);
        git(&f.w1, &["fetch", "-q", "origin"]);
        git(&f.origin, &["branch", "-D", "old"]);
        let s = w.sync(true, true, false).unwrap();
        assert_eq!(s.pruned, vec!["old"]);
        assert!(w.repo.rev("refs/remotes/origin/old").is_none());
        assert!(w.repo.rev("refs/remotes/origin/keep").is_some());
        assert!(w.repo.rev("refs/remotes/origin/main").is_some());
        assert!(w.sync(false, true, false).unwrap().pruned.is_empty());
    }

    /// #187: a branch with no commits of its own, adopted while
    /// `origin/main` was stale, got the stale trunk as its base; sync then
    /// deleted it as merged.
    #[test]
    fn adopting_an_empty_branch_keeps_it_through_sync() {
        let f = setup();
        advance_origin(&f, "up.txt", "u\n");
        // The thread branch is the remote's latest main; origin/main is not.
        let url = f.origin.to_str().unwrap();
        git(&f.w1, &["fetch", "-q", url, "main:refs/heads/claude/x"]);
        git(&f.w1, &["switch", "-q", "claude/x"]);
        let mut w = Window::open(&f.w1).unwrap();
        assert_ne!(w.base().unwrap(), rev(&f.w1, "claude/x"), "stale trunk");
        assert!(w.fetch_trunk().unwrap());
        w.adopt("claude/x", None, true).unwrap();
        let l = w.log(false).unwrap();
        assert_eq!(l.base, Some(rev(&f.w1, "claude/x")));
        assert_eq!(
            (l.branches[0].parent.as_str(), l.branches[0].commits),
            ("main", 0)
        );
        let s = w.sync(true, true, false).unwrap();
        assert!(s.merged.is_empty(), "{:?}", s.merged);
        assert!(w.tracks("claude/x"));
        assert_eq!(w.current_branch().as_deref(), Some("claude/x"));
    }

    #[test]
    fn a_restarted_thread_branch_replaces_its_merged_remote_copy() {
        // A thread can push only its own branch: after its PR is
        // squash-merged (the remote branch stays), the branch restarts on
        // the trunk and the next submit replaces the merged head.
        let f = setup();
        let opts = SubmitOptions {
            no_pr: true,
            ..Default::default()
        };
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("t.txt"), "1\n").unwrap();
        w.create(Some("thread"), Some("work"), true).unwrap();
        assert_eq!(w.submit_with(&opts, None).unwrap().pushed, vec!["thread"]);
        advance_origin(&f, "t.txt", "1\n");
        w.fetch_trunk().unwrap();
        fs::write(f.w1.join("n.txt"), "next\n").unwrap();
        w.create_reset(Some("thread"), Some("next"), true).unwrap();
        assert_eq!(w.submit_with(&opts, None).unwrap().pushed, vec!["thread"]);
        let remote = git(
            &f.w1,
            &["ls-remote", f.origin.to_str().unwrap(), "refs/heads/thread"],
        );
        assert!(remote.starts_with(&rev(&f.w1, "thread")), "{remote}");

        // Forgotten by the window (a fresh clone): the remote copy is the
        // merged tip it had, so it is still ours to replace.
        advance_origin(&f, "n.txt", "next\n");
        w.fetch_trunk().unwrap();
        w.untrack("thread").unwrap();
        fs::write(f.w1.join("m.txt"), "more\n").unwrap();
        w.create_reset(Some("thread"), Some("more"), true).unwrap();
        assert_eq!(w.submit_with(&opts, None).unwrap().pushed, vec!["thread"]);
    }

    #[test]
    fn create_reset_restarts_a_merged_branch_on_the_trunk() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("t.txt"), "1\n").unwrap();
        w.create(Some("thread"), Some("work"), true).unwrap();
        assert_eq!(
            w.create(Some("thread"), None, false).unwrap_err().code(),
            "branch_exists"
        );
        // Unmerged work is never thrown away.
        let e = w.create_reset(Some("thread"), None, false).unwrap_err();
        assert_eq!(e.code(), "unmerged_commits");
        assert!(w.tracks("thread"));

        // Squash-merged upstream, then restarted from the latest trunk with
        // a local edit kept and committed.
        advance_origin(&f, "t.txt", "1\n");
        w.fetch_trunk().unwrap();
        fs::write(f.w1.join("n.txt"), "next\n").unwrap();
        let (t, c) = w.create_reset(Some("thread"), Some("next"), true).unwrap();
        assert_eq!(t.parent, "main");
        assert_eq!(t.parent_rev, w.base().unwrap());
        assert!(c.is_some());
        assert_eq!(rev(&f.w1, "thread~1"), w.base().unwrap());
        assert_eq!(w.current_branch().as_deref(), Some("thread"));
        assert_eq!(w.log(false).unwrap().branches[0].commits, 1);
        assert_eq!(w.state.branches.len(), 1);

        // From the trunk (detached), an untracked merged branch: reset there.
        w.checkout("main").unwrap();
        git(&f.w1, &["branch", "-f", "other", "origin/main~1"]);
        let (t, c) = w.create_reset(Some("other"), None, false).unwrap();
        assert!(c.is_none());
        assert_eq!(rev(&f.w1, "other"), w.base().unwrap());
        assert!(w.tracks(&t.name));
        assert_eq!(
            w.create_reset(Some("main"), None, false)
                .unwrap_err()
                .code(),
            "bad_branch"
        );
    }

    /// main has n.py pushed; `ws` re-indents `bar()`; main then edits the
    /// same line (#200).
    fn whitespace_only_branch(f: &Fixture) -> (Window, String) {
        commit_file(
            &f.w1,
            "n.py",
            "def g():\n    if y:\n        bar()\n",
            "add n.py",
        );
        git(&f.w1, &["push", "-q", "origin", "main"]);
        git(&f.w1, &["fetch", "-q", "origin"]);
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("n.py"), "def g():\n    if y:\n    bar()\n").unwrap();
        w.create(Some("ws"), Some("ws only"), true).unwrap();
        let tip = rev(&f.w1, "ws");
        advance_origin(f, "n.py", "def g():\n    if y:\n        bar(1)\n");
        (w, tip)
    }

    #[test]
    fn sync_keeps_a_whitespace_only_commit() {
        let f = setup();
        let (mut w, tip) = whitespace_only_branch(&f);
        let e = w.sync(true, true, false).unwrap_err();
        assert_eq!(e.code(), "conflict", "{e}");
        assert!(w.tracks("ws"));
        assert_eq!(rev(&f.w1, "ws"), tip, "nothing changed");
        assert!(git(&f.w1, &["branch", "--list", "ws"]).contains("ws"));
        assert!(
            git(&f.w1, &["for-each-ref", "refs/agentvc/trash"]).is_empty(),
            "nothing was deleted"
        );
        // Restacking by hand raises the same conflict; with --keep-conflicts
        // it is left in the worktree rather than dropped.
        let e = w.restack(None, false).unwrap_err();
        assert_eq!(e.code(), "conflict");
        let r = w.restack(None, true).unwrap();
        let c = r.conflict.expect("conflict");
        assert_eq!(c.branch, "ws");
        assert_eq!(c.paths, vec!["n.py".to_string()]);
        assert!(r.updated.is_empty());
    }

    #[test]
    fn restack_keeps_a_whitespace_only_commit_mid_stack() {
        let f = setup();
        let (mut w, tip) = whitespace_only_branch(&f);
        fs::write(f.w1.join("top.txt"), "top\n").unwrap();
        w.create(Some("top"), Some("top"), true).unwrap();
        git(&f.w1, &["fetch", "-q", "origin"]);
        let e = w.restack(None, false).unwrap_err();
        assert_eq!(e.code(), "conflict", "{e}");
        assert_eq!(rev(&f.w1, "ws"), tip);
        assert_eq!(rev(&f.w1, "top~1"), tip);
    }

    #[test]
    fn sync_keeps_an_empty_branch_fast_forwarded_to_trunk() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        w.create(Some("wip"), None, false).unwrap();
        advance_origin(&f, "up.txt", "up\n");
        git(&f.w1, &["fetch", "-q", "origin"]);
        git(&f.w1, &["merge", "-q", "--ff-only", "origin/main"]);
        let s = w.sync(true, true, false).unwrap();
        assert!(s.merged.is_empty(), "{:?}", s.merged);
        assert!(s.deleted.is_empty());
        assert!(w.tracks("wip"));
        assert_eq!(
            git(&f.w1, &["symbolic-ref", "--short", "HEAD"]),
            "wip",
            "HEAD stays on the branch"
        );
        assert_eq!(rev(&f.w1, "wip"), rev(&f.w1, "origin/main"));
    }

    #[test]
    fn sync_trashes_a_fast_forward_merged_branch_before_deleting_it() {
        let f = setup();
        let mut w = Window::open(&f.w1).unwrap();
        fs::write(f.w1.join("ff.txt"), "ff\n").unwrap();
        w.create(Some("ff"), Some("ff work"), true).unwrap();
        let tip = rev(&f.w1, "ff");
        // Merged upstream as a fast-forward: the tip is in main as is.
        git(&f.w1, &["push", "-q", "origin", "ff:main"]);
        git(&f.w1, &["switch", "-q", "--detach", "HEAD"]);
        let s = w.sync(true, true, false).unwrap();
        assert_eq!(s.merged, vec!["ff".to_string()]);
        assert_eq!(s.deleted, vec!["ff".to_string()]);
        assert!(git(&f.w1, &["branch", "--list", "ff"]).is_empty());
        assert_eq!(rev(&f.w1, "refs/agentvc/trash/ff"), tip);
        assert_eq!(s.trashed.len(), 1);
        assert_eq!(s.trashed[0].tip, tip);
        assert_eq!(s.trashed[0].restore, "git branch ff refs/agentvc/trash/ff");
        git(&f.w1, &["branch", "ff", "refs/agentvc/trash/ff"]);
        assert_eq!(rev(&f.w1, "ff"), tip);
    }
}
