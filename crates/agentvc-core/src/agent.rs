//! Agents: a coding agent started from one point on one workspace's stack.
//!
//! The flow is workspace, then a branch of its stack (or main), then a
//! prompt. Each agent works in its own git worktree on its own branch, so
//! several agents can start from the same branch and none of them touches
//! the workspace's checkout. When it stops, its work is committed, the
//! repo's check command runs, and the result waits for review. Landing puts
//! it on the workspace's stack:
//!
//! - [`Mode::New`]: the agent's branch joins the stack as a child of the
//!   start branch (like `stack create`).
//! - [`Mode::Revise`]: the start branch moves to the agent's commit and the
//!   branches above it restack (like `stack modify`).
//!
//! Everything lives in files under `<git-common-dir>/agentvc/agents/agN/`,
//! so the CLI, the daemon, the MCP server inside the agent and the worker
//! process that supervises it all read and write the same state:
//!
//! - `agent.json`: the [`Agent`] record (written under `agent.lock`).
//! - `steps.jsonl`: what the agent did, one [`Step`] per line.
//! - `output.log`: the runner's raw output; `worker.log`: the worker's.
//! - `answers/<question>.json`: answers to [`ask`] and [`approve`].
//!
//! The agent's worktree lives outside `.git` (Claude Code refuses to edit
//! files under a `.git` directory): in `git config agentvc.agent-worktrees`,
//! else `~/.agentvc/worktrees/<repo>-<hash>/agN`. Its git directory carries
//! an `agentvc-agent` marker that hides it from the workspace list.
//!
//! Runners: Claude Code headless (`claude -p --output-format stream-json`)
//! with agentvc's MCP server attached, so `ask_human` and the permission
//! prompt tool `approve` reach the person in the UI; or any shell command
//! (`AGENTVC_AGENT_PROMPT` holds the prompt, and `"$AGENTVC_BIN" agent
//! ask` asks the person a question).

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, IoContext, Result};
use crate::export::HostRepo;
use crate::run::pid_alive;
use crate::space::now;
use crate::stack::{canon, slug, Window};
use crate::workspace::{self, EditOutcome, Op, RequestStatus};

/// Agents that may work at once on one machine, unless
/// `git config agentvc.agents-max` says otherwise. A safety cap across all
/// workspaces; each workspace can set a lower `max` of its own.
pub const DEFAULT_LIMIT: usize = 8;

/// Runners an agent can use.
pub const RUNNERS: &[&str] = &["claude", "codex", "cursor", "command"];
/// Permission levels for Claude Code agents, the default first.
pub const PERMISSIONS: &[&str] = &["auto", "edits", "ask", "bypass"];
/// The permission level when nothing sets one.
pub const DEFAULT_PERMISSIONS: &str = "auto";

/// Claude Code's `--permission-mode` for an agentvc permission level.
pub fn claude_permission_mode(level: Option<&str>) -> &'static str {
    match level.unwrap_or(DEFAULT_PERMISSIONS) {
        "edits" => "acceptEdits",
        "ask" => "default",
        "bypass" => "bypassPermissions",
        _ => "auto",
    }
}

/// How agents started in one workspace run. Kept in the workspace's
/// `stack.json` (so it is published with the workspace); every field is
/// optional and falls back to repo `git config agentvc.agent-*`, then to
/// the defaults.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSettings {
    /// `claude` (Claude Code, default), `codex`, `cursor` or `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    /// The shell command of the `command` runner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Check command run when an agent stops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    /// Claude Code: `auto` (Claude Code's auto mode: a safety check
    /// approves routine steps, risky ones come to you; default), `edits`
    /// (edit files, ask before commands), `ask` (ask before edits too) or
    /// `bypass` (never ask, no checks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
    /// Claude Code: run commands in its sandbox, which lets them write only
    /// inside the agent's worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<bool>,
    /// Agents working at once in this workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<usize>,
}

impl AgentSettings {
    pub fn is_empty(&self) -> bool {
        *self == AgentSettings::default()
    }
}
/// Environment variable that points the MCP tools at their agent.
pub const ENV_DIR: &str = "AGENTVC_AGENT_DIR";
/// Environment variable naming the Claude Code program (default `claude`).
pub const ENV_CLAUDE: &str = "AGENTVC_CLAUDE";
const RECORD: &str = "agent.json";
/// File in an agent worktree's git directory that marks it as one.
pub const AGENT_MARKER: &str = "agentvc-agent";
const STEPS: &str = "steps.jsonl";
const OUTPUT: &str = "output.log";
const LOCK_WAIT: Duration = Duration::from_secs(10);
const LOCK_STALE: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(300);
const TAIL_LINES: usize = 20;

const SYSTEM_PROMPT: &str = "You are one of several agents a person runs from the agentvc control center. \
You work in your own git worktree; do not push, switch branches or commit (agentvc commits your work when you stop). \
When you need a decision from the person, call the ask_human tool with one short question instead of guessing; \
they answer in the control center. Finish with a short summary of what you changed.";

fn agent_err(code: &'static str, message: impl Into<String>) -> Error {
    Error::Stack {
        code,
        message: message.into(),
        details: None,
    }
}

fn agent_err_with(code: &'static str, message: impl Into<String>, details: Value) -> Error {
    Error::Stack {
        code,
        message: message.into(),
        details: Some(details),
    }
}

// ---- the record --------------------------------------------------------------

/// Where an agent's result goes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// A new branch on top of the start branch.
    New,
    /// A new version of the start branch.
    Revise,
}

/// One state per agent; the UI shows it as one dot.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Waiting for a free slot on its machine.
    Queued,
    Running,
    /// Waiting for an answer to `ask_human`.
    Asking,
    /// Waiting for Approve or Deny on a command.
    NeedsApproval,
    /// Stopped and its checks passed (or there were none): review it.
    Ready,
    TestsFailing,
    /// The runner failed or its worker died.
    Failed,
    /// Stopped by a person.
    Stopped,
    Landed,
    Discarded,
}

impl AgentState {
    /// Queued or working: has (or waits for) a worker process.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            AgentState::Queued
                | AgentState::Running
                | AgentState::Asking
                | AgentState::NeedsApproval
        )
    }

    /// Holds one of the machine's slots.
    pub fn is_working(self) -> bool {
        matches!(
            self,
            AgentState::Running | AgentState::Asking | AgentState::NeedsApproval
        )
    }

    /// Landed or discarded: nothing more happens to it.
    pub fn is_closed(self) -> bool {
        matches!(self, AgentState::Landed | AgentState::Discarded)
    }

    /// Stopped with a result a person can review, reply to or land.
    pub fn is_done(self) -> bool {
        matches!(
            self,
            AgentState::Ready | AgentState::TestsFailing | AgentState::Failed | AgentState::Stopped
        )
    }
}

/// What an agent started from.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartPoint {
    /// A branch of the workspace's stack, or its trunk.
    pub branch: String,
    /// The commit the agent's branch was created at.
    pub commit: String,
    /// `branch` is the trunk (main).
    #[serde(default)]
    pub trunk: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// A question from `ask_human`, answered with text.
    Ask,
    /// A tool call waiting for Approve or Deny.
    Approve,
}

/// What an agent is waiting on a person for.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub kind: QuestionKind,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    pub asked_at: i64,
}

/// The answer file written for a [`Question`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Answer {
    #[serde(default)]
    pub text: Option<String>,
    /// For approvals.
    #[serde(default)]
    pub allow: Option<bool>,
}

/// The repo's check command, run when the agent stops.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckResult {
    pub command: String,
    pub exit_code: Option<i32>,
    pub passed: bool,
    /// The last lines of its output.
    pub tail: Vec<String>,
    pub duration_ms: i64,
}

/// What the agent changed, from its start commit to its branch tip.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentResult {
    pub commit: Option<String>,
    pub files: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub paths: Vec<String>,
    /// Each changed path with `added`, `modified` or `removed`.
    #[serde(default)]
    pub changes: Vec<FileKind>,
}

/// One changed path of an agent's result.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileKind {
    pub path: String,
    pub kind: String,
}

/// How the result went onto the stack.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Landing {
    /// `applied`, or `queued` while an agent works in the workspace itself.
    pub outcome: RequestStatus,
    pub request_id: String,
    /// The stack branch that now holds the work.
    pub branch: String,
    pub at: i64,
}

/// One message of the conversation with the agent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    /// `you` or `agent`.
    pub from: String,
    pub text: String,
    pub at: i64,
}

/// One coding agent started from a stack.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Agent {
    /// `agN`.
    pub id: String,
    /// A few words, from the prompt's first line.
    pub title: String,
    pub prompt: String,
    /// Workspace id and name.
    pub workspace: String,
    pub workspace_name: String,
    pub start: StartPoint,
    pub mode: Mode,
    /// The agent's own branch, checked out in `worktree`.
    pub branch: String,
    pub worktree: PathBuf,
    /// `claude`, `codex`, `cursor` or `command`.
    pub runner: String,
    /// Claude Code permission level (`auto`, `edits`, `ask` or `bypass`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
    /// Claude Code commands run in its sandbox.
    #[serde(default)]
    pub sandbox: bool,
    /// The workspace checkout it was started from, whose Claude Code setup
    /// (local settings, local MCP servers) it inherits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_from: Option<PathBuf>,
    /// The permission mode Claude Code reported when it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Check command run when it stops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    pub machine: String,
    pub state: AgentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_pid: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_pid: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub finished_at: Option<i64>,
    /// Seconds spent running, summed over turns.
    #[serde(default)]
    pub seconds: i64,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub turns: u64,
    /// What it is doing now (or did last).
    #[serde(default)]
    pub last_step: Option<String>,
    #[serde(default)]
    pub question: Option<Question>,
    /// A reply waiting for the next turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_reply: Option<String>,
    #[serde(default)]
    pub stop_requested: bool,
    #[serde(default)]
    pub tests: Option<CheckResult>,
    #[serde(default)]
    pub result: Option<AgentResult>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub landed: Option<Landing>,
    #[serde(default)]
    pub messages: Vec<Message>,
    /// Computed on read: the start branch moved since the agent started.
    #[serde(default)]
    pub stale: bool,
    /// Computed on read: rank in the "Needs you" list (1 is most urgent),
    /// `None` when it does not need a person.
    #[serde(default)]
    pub urgency: Option<u8>,
    /// Computed on read: since when it has waited on a person.
    #[serde(default)]
    pub waiting_since: Option<i64>,
}

impl Agent {
    /// The rank in "Needs you": 1 blocked on a question or approval, 2
    /// failed, 3 tests failing, 4 ready for review, 5 ready but stale.
    pub fn compute_urgency(&self) -> Option<u8> {
        match self.state {
            AgentState::Asking | AgentState::NeedsApproval => Some(1),
            AgentState::Failed => Some(2),
            AgentState::TestsFailing => Some(3),
            AgentState::Ready | AgentState::Stopped if self.stale => Some(5),
            AgentState::Ready | AgentState::Stopped => Some(4),
            _ => None,
        }
    }
}

/// One thing the agent did.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    pub at: i64,
    /// `text`, `tool`, `you`, `question`, `result`, `check` or `line`.
    pub kind: String,
    pub text: String,
}

// ---- files -------------------------------------------------------------------

/// `<git-common-dir>/agentvc/agents` of the repo `w` belongs to.
pub fn agents_root(common_dir: &Path) -> PathBuf {
    common_dir.join("agentvc").join("agents")
}

/// Where this repo's agent worktrees go.
fn worktrees_home(w: &Window) -> Result<PathBuf> {
    if let Some(d) = config(w, "agentvc.agent-worktrees") {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| {
            agent_err(
                "no_home",
                "no home directory for agent worktrees; set git config agentvc.agent-worktrees",
            )
        })?;
    let repo = w
        .common_dir
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    use sha1::Digest;
    let hash = hex::encode(sha1::Sha1::digest(
        w.common_dir.to_string_lossy().as_bytes(),
    ));
    Ok(home
        .join(".agentvc")
        .join("worktrees")
        .join(format!("{repo}-{}", &hash[..8])))
}

fn root_of(cwd: &Path) -> Result<PathBuf> {
    Ok(agents_root(&Window::open(cwd)?.common_dir))
}

/// The repo agent `a` works in: its workspace's, which can be another
/// checkout of the space than `cwd`'s (#160).
fn agent_window(cwd: &Path, a: &Agent) -> Result<Window> {
    let here = Window::open(cwd)?;
    match workspace::find_local(&here, &a.workspace) {
        Ok(l) if l.other_checkout => Window::open(&l.path),
        _ => Ok(here),
    }
}

/// The agent stores of every checkout in the space of `w` on this machine
/// (#160): its own first.
fn space_roots(w: &Window) -> Vec<PathBuf> {
    let mut out = vec![agents_root(&w.common_dir)];
    let (_, sib) = crate::checkouts::space_and_siblings(w);
    for p in sib {
        if let Ok(sw) = Window::open(&p) {
            let r = agents_root(&sw.common_dir);
            if !out.contains(&r) {
                out.push(r);
            }
        }
    }
    out
}

fn read_agent(dir: &Path) -> Result<Agent> {
    let path = dir.join(RECORD);
    let text = fs::read_to_string(&path).at(&path)?;
    serde_json::from_str(&text)
        .map_err(|e| agent_err("bad_agent", format!("{}: {e}", path.display())))
}

fn write_agent(dir: &Path, a: &Agent) -> Result<()> {
    let tmp = dir.join(format!("{RECORD}.{}.tmp", std::process::id()));
    let mut stored = a.clone();
    stored.stale = false;
    stored.urgency = None;
    stored.waiting_since = None;
    let text = serde_json::to_string_pretty(&stored).expect("agent serializes");
    fs::write(&tmp, text).at(&tmp)?;
    fs::rename(&tmp, dir.join(RECORD)).at(dir.join(RECORD))
}

struct RecordLock(PathBuf);

impl Drop for RecordLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn lock_record(dir: &Path) -> Result<RecordLock> {
    let path = dir.join("agent.lock");
    let start = Instant::now();
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(RecordLock(path)),
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
                    return Err(agent_err(
                        "agent_locked",
                        format!("{} is locked by another command; try again", path.display()),
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e).at(&path),
        }
    }
}

/// Change the record in `dir` under its lock.
fn update(dir: &Path, f: impl FnOnce(&mut Agent) -> Result<()>) -> Result<Agent> {
    let _lock = lock_record(dir)?;
    let mut a = read_agent(dir)?;
    f(&mut a)?;
    a.updated_at = now();
    write_agent(dir, &a)?;
    Ok(a)
}

fn add_step(dir: &Path, kind: &str, text: &str) {
    let step = Step {
        at: now(),
        kind: kind.into(),
        text: text.into(),
    };
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(STEPS))
    {
        let _ = writeln!(
            f,
            "{}",
            serde_json::to_string(&step).expect("step serializes")
        );
    }
}

/// The last `n` steps of an agent (all with `n == 0`).
pub fn steps(dir: &Path, n: usize) -> Vec<Step> {
    let text = fs::read_to_string(dir.join(STEPS)).unwrap_or_default();
    let all: Vec<Step> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let from = if n == 0 {
        0
    } else {
        all.len().saturating_sub(n)
    };
    all[from..].to_vec()
}

fn parse_agent_id(id: &str) -> Result<u64> {
    id.trim()
        .strip_prefix("ag")
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| agent_err("bad_agent_id", format!("{id:?} is not an agent id (agN)")))
}

/// The directory of agent `id` in the repo containing `cwd`.
/// The directory of agent `id` in the repo containing `cwd`, or in another
/// checkout of its space on this machine (#160).
pub fn agent_dir(cwd: &Path, id: &str) -> Result<PathBuf> {
    let n = parse_agent_id(id)?;
    let own = root_of(cwd)?.join(format!("ag{n}"));
    if own.join(RECORD).exists() {
        return Ok(own);
    }
    let w = Window::open(cwd)?;
    space_roots(&w)
        .into_iter()
        .map(|r| r.join(format!("ag{n}")))
        .find(|d| d.join(RECORD).exists())
        .ok_or_else(|| Error::NotFound {
            kind: "agent",
            id: format!("ag{n}"),
        })
}

/// A new agent directory in `root`, numbered past every agent in `roots`
/// too, so ids stay unique across the checkouts of a space.
fn new_agent_dir(root: &Path, roots: &[PathBuf]) -> Result<(String, PathBuf)> {
    fs::create_dir_all(root).at(root)?;
    let highest = |r: &Path| -> u64 {
        fs::read_dir(r)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        e.file_name()
                            .to_str()?
                            .strip_prefix("ag")?
                            .parse::<u64>()
                            .ok()
                    })
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    };
    let mut n = roots
        .iter()
        .map(|r| highest(r))
        .chain([highest(root)])
        .max()
        .unwrap_or(0);
    loop {
        n += 1;
        let dir = root.join(format!("ag{n}"));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok((format!("ag{n}"), dir)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).at(&dir),
        }
    }
}

fn title_of(prompt: &str) -> String {
    let line = prompt
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut t: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        t = t.trim_end().to_string();
        t.push('…');
    }
    t
}

fn tail(text: &str, n: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

fn git_in(dir: &Path) -> HostRepo {
    HostRepo {
        workdir: dir.to_path_buf(),
    }
}

/// `git config <key>` in the repo of `w`, if set.
fn config(w: &Window, key: &str) -> Option<String> {
    w.repo
        .git(&["config", "--get", key], None)
        .ok()
        .filter(|s| !s.is_empty())
}

// ---- starting ----------------------------------------------------------------

/// What [`start`] needs.
#[derive(Clone, Debug)]
pub struct StartRequest {
    /// Workspace id, name or directory; defaults to the one containing `cwd`.
    pub workspace: Option<String>,
    /// Branch of its stack (or the trunk); defaults to its checked-out branch.
    pub from: Option<String>,
    /// An older commit of `from` to start at (a previous version).
    pub at: Option<String>,
    pub prompt: String,
    pub mode: Mode,
    pub model: Option<String>,
    /// Check command; defaults to `git config agentvc.agent-check`.
    pub check: Option<String>,
    /// A shell command to run instead of Claude Code.
    pub command: Option<String>,
    /// `claude`, `codex`, `cursor` or `command`; defaults to the workspace's.
    pub runner: Option<String>,
    /// Claude Code permission level; defaults to the workspace's.
    pub permissions: Option<String>,
    /// The `agentvc` program, for the worker and the MCP server.
    pub exe: PathBuf,
}

fn check_choice(what: &str, v: &str, allowed: &[&str]) -> Result<()> {
    if allowed.contains(&v) {
        Ok(())
    } else {
        Err(agent_err_with(
            "bad_setting",
            format!("{what} must be one of {}, not {v}", allowed.join(", ")),
            json!({ "setting": what, "allowed": allowed }),
        ))
    }
}

impl AgentSettings {
    /// Refuse values no agent could run with.
    pub fn validate(&self) -> Result<()> {
        if let Some(r) = &self.runner {
            check_choice("runner", r, RUNNERS)?;
        }
        if let Some(p) = &self.permissions {
            check_choice("permissions", p, PERMISSIONS)?;
        }
        if self.max == Some(0) {
            return Err(agent_err("bad_setting", "max must be at least 1"));
        }
        Ok(())
    }

    /// Fields set here win over `under`'s.
    pub fn over(&self, under: &AgentSettings) -> AgentSettings {
        AgentSettings {
            runner: self.runner.clone().or_else(|| under.runner.clone()),
            command: self.command.clone().or_else(|| under.command.clone()),
            model: self.model.clone().or_else(|| under.model.clone()),
            check: self.check.clone().or_else(|| under.check.clone()),
            permissions: self
                .permissions
                .clone()
                .or_else(|| under.permissions.clone()),
            sandbox: self.sandbox.or(under.sandbox),
            max: self.max.or(under.max),
        }
    }
}

/// The repo-wide defaults from `git config agentvc.agent-*`.
fn repo_defaults(w: &Window) -> AgentSettings {
    AgentSettings {
        runner: config(w, "agentvc.agent-runner"),
        command: config(w, "agentvc.agent-command"),
        model: config(w, "agentvc.agent-model"),
        check: config(w, "agentvc.agent-check"),
        permissions: config(w, "agentvc.agent-permissions"),
        sandbox: config(w, "agentvc.agent-sandbox").map(|v| v == "true"),
        max: None,
    }
}

/// One workspace's agent settings, what applies when they are not set, and
/// the machine's cap.
#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceAgentSettings {
    pub workspace: String,
    pub workspace_name: String,
    /// What the workspace sets.
    pub settings: AgentSettings,
    /// Repo defaults (`git config agentvc.agent-*`) under them.
    pub defaults: AgentSettings,
    /// Agents working at once on this machine, across all workspaces.
    pub machine_limit: usize,
}

/// The agent settings of `workspace` (id, name or directory; default: the
/// one containing `cwd`).
pub fn settings(cwd: &Path, workspace: Option<&str>) -> Result<WorkspaceAgentSettings> {
    let here = Window::open(cwd)?;
    let target = workspace
        .map(str::to_string)
        .unwrap_or_else(|| here.path().to_string_lossy().into_owned());
    let ws = workspace::find_local(&here, &target)?;
    let w = Window::open(&ws.path)?;
    Ok(WorkspaceAgentSettings {
        workspace: ws.record.id.clone(),
        workspace_name: ws.record.name.clone(),
        settings: w.state.agents.clone(),
        defaults: repo_defaults(&w),
        machine_limit: limit(&w),
    })
}

/// Replace the agent settings of `workspace`. Empty strings clear a field.
pub fn set_settings(
    cwd: &Path,
    workspace: Option<&str>,
    new: AgentSettings,
) -> Result<WorkspaceAgentSettings> {
    let blank = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let new = AgentSettings {
        runner: blank(new.runner),
        command: blank(new.command),
        model: blank(new.model),
        check: blank(new.check),
        permissions: blank(new.permissions),
        sandbox: new.sandbox,
        max: new.max,
    };
    new.validate()?;
    if new.runner.as_deref() == Some("command") && new.command.is_none() {
        return Err(agent_err(
            "command_required",
            "the command runner needs a command",
        ));
    }
    let here = Window::open(cwd)?;
    let target = workspace
        .map(str::to_string)
        .unwrap_or_else(|| here.path().to_string_lossy().into_owned());
    let ws = workspace::find_local(&here, &target)?;
    {
        let mut w = Window::open(&ws.path)?;
        let _lock = workspace::lock(&w.git_dir)?;
        // Re-read under the lock so a stack edit in between is kept.
        w = Window::open(&ws.path)?;
        w.state.agents = new;
        w.save()?;
    }
    settings(cwd, Some(&ws.record.id))
}

/// What agents started in one workspace may do.
#[derive(Clone, Debug, Serialize)]
pub struct WorkspacePermissions {
    pub workspace: String,
    pub workspace_name: String,
    pub path: PathBuf,
    pub runner: String,
    /// The permission level (`auto`, `edits`, `ask` or `bypass`) and where it
    /// comes from: `workspace`, `repo` (git config) or `default`.
    pub permissions: String,
    pub permissions_from: String,
    /// Claude Code's `--permission-mode` for it.
    pub claude_mode: String,
    pub sandbox: bool,
    /// The person's Claude Code setup agents inherit (Claude Code runner).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup: Option<crate::agent_setup::ClaudeSetup>,
}

/// One open agent's permissions.
#[derive(Clone, Debug, Serialize)]
pub struct AgentPermissions {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub workspace_name: String,
    pub state: AgentState,
    pub runner: String,
    pub permissions: Option<String>,
    /// What Claude Code reported when it started.
    pub claude_mode: Option<String>,
    pub sandbox: bool,
    pub worktree: PathBuf,
}

/// `agentvc agent permissions`: what agents may do, per workspace and per
/// open agent, and the guard rails agentvc adds.
#[derive(Clone, Debug, Serialize)]
pub struct PermissionsReport {
    pub guard_rails: Vec<String>,
    pub workspaces: Vec<WorkspacePermissions>,
    pub agents: Vec<AgentPermissions>,
}

/// What agents of every workspace of the space containing `cwd` may do.
pub fn permissions(cwd: &Path) -> Result<PermissionsReport> {
    let here = Window::open(cwd)?;
    let mut workspaces = Vec::new();
    for l in workspace::space_locals(&here)? {
        let Ok(w) = Window::open(&l.path) else {
            continue;
        };
        let own = &w.state.agents;
        let repo = repo_defaults(&w);
        let eff = own.over(&repo);
        let runner = eff.runner.clone().unwrap_or_else(|| "claude".into());
        let (level, from) = match (&own.permissions, &repo.permissions) {
            (Some(p), _) => (p.clone(), "workspace"),
            (None, Some(p)) => (p.clone(), "repo"),
            (None, None) => (DEFAULT_PERMISSIONS.to_string(), "default"),
        };
        workspaces.push(WorkspacePermissions {
            workspace: l.record.id.clone(),
            workspace_name: l.record.name.clone(),
            path: l.path.clone(),
            claude_mode: claude_permission_mode(Some(&level)).into(),
            permissions: level,
            permissions_from: from.into(),
            sandbox: eff.sandbox.unwrap_or(false),
            setup: (runner == "claude").then(|| crate::agent_setup::read(&l.path)),
            runner,
        });
    }
    let agents = list(cwd, false)?
        .agents
        .into_iter()
        .map(|a| AgentPermissions {
            id: a.id,
            title: a.title,
            workspace: a.workspace,
            workspace_name: a.workspace_name,
            state: a.state,
            runner: a.runner,
            permissions: a.permissions,
            claude_mode: a.claude_mode,
            sandbox: a.sandbox,
            worktree: a.worktree,
        })
        .collect();
    Ok(PermissionsReport {
        guard_rails: crate::agent_setup::GUARD_RAILS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        workspaces,
        agents,
    })
}

/// The settings of the workspace `id` of the repo containing `w`, if it is
/// on this machine.
fn workspace_settings(w: &Window, id: &str) -> AgentSettings {
    workspace::find_local(w, id)
        .ok()
        .and_then(|ws| Window::open(&ws.path).ok())
        .map(|ws| ws.state.agents)
        .unwrap_or_default()
}

/// Start an agent: make its worktree and branch, record it and launch its
/// worker in the background. Returns at once; the agent is `queued` until
/// its machine has a free slot.
pub fn start(cwd: &Path, req: &StartRequest) -> Result<Agent> {
    let prompt = req.prompt.trim();
    if prompt.is_empty() {
        return Err(agent_err("prompt_required", "tell the agent what to do"));
    }
    let here = Window::open(cwd)?;
    let target = match &req.workspace {
        Some(t) => t.clone(),
        None => here.path().to_string_lossy().into_owned(),
    };
    let ws = workspace::find_local(&here, &target)?;
    let w = Window::open(&ws.path)?;
    let asked = AgentSettings {
        runner: req
            .runner
            .clone()
            .or_else(|| req.command.as_ref().map(|_| "command".to_string())),
        command: req.command.clone(),
        model: req.model.clone(),
        check: req.check.clone(),
        permissions: req.permissions.clone(),
        sandbox: None,
        max: None,
    };
    asked.validate()?;
    let eff = asked.over(&w.state.agents).over(&repo_defaults(&w));
    let runner = eff.runner.clone().unwrap_or_else(|| "claude".into());
    if runner == "command" && eff.command.is_none() {
        return Err(agent_err(
            "command_required",
            "the command runner needs a command (--command, or the workspace's settings)",
        ));
    }
    let trunk = w.state.trunk.clone();
    let from = req
        .from
        .clone()
        .or_else(|| w.current_branch())
        .unwrap_or_else(|| trunk.clone());
    let is_trunk = from == trunk;
    let tip = if is_trunk {
        w.base()
    } else {
        w.branch_tip(&from)
    }
    .ok_or_else(|| {
        agent_err_with(
            "no_branch",
            format!("no branch {from} in workspace {}", ws.record.name),
            json!({ "branch": from, "workspace": ws.record.id }),
        )
    })?;
    let commit = match &req.at {
        Some(at) => {
            let c = w
                .repo
                .rev(at)
                .ok_or_else(|| agent_err("bad_ref", format!("no commit {at}")))?;
            if !w.repo.is_ancestor(&c, &tip) {
                return Err(agent_err(
                    "bad_ref",
                    format!("{at} is not a version of {from}"),
                ));
            }
            c
        }
        None => tip,
    };
    if req.mode == Mode::Revise && is_trunk {
        return Err(agent_err(
            "bad_mode",
            format!("{trunk} cannot be revised; start a new branch on it instead"),
        ));
    }
    if req.mode == Mode::Revise && !w.tracks(&from) {
        return Err(agent_err_with(
            "not_in_stack",
            format!(
                "branch {from} is not in workspace {}'s stack",
                ws.record.name
            ),
            json!({ "branch": from }),
        ));
    }

    let root = agents_root(&w.common_dir);
    let (id, dir) = new_agent_dir(&root, &space_roots(&here))?;
    let title = title_of(prompt);
    let branch = match req.mode {
        Mode::Revise => format!("agentvc/{id}"),
        Mode::New => {
            let base = match slug(&title) {
                s if s.is_empty() => id.clone(),
                s => s,
            };
            let mut name = base.clone();
            let mut k = 2;
            while w.branch_tip(&name).is_some() {
                name = format!("{base}-{k}");
                k += 1;
            }
            name
        }
    };
    let wt = worktrees_home(&w)?.join(&id);
    if wt.exists() {
        // Left over from an agent of an older record store; never reuse it.
        return Err(agent_err(
            "worktree_exists",
            format!(
                "{} already exists; remove it or set agentvc.agent-worktrees",
                wt.display()
            ),
        ));
    }
    let wt_s = wt.to_string_lossy().into_owned();
    if let Err(e) = w.repo.git(
        &["worktree", "add", "--quiet", "-b", &branch, &wt_s, &commit],
        None,
    ) {
        let _ = fs::remove_dir_all(&dir);
        return Err(e);
    }
    if let Ok(g) = git_in(&wt).git(&["rev-parse", "--absolute-git-dir"], None) {
        let _ = fs::write(Path::new(&g).join(AGENT_MARKER), &id);
    }
    let created = now();
    let a = Agent {
        id: id.clone(),
        title,
        prompt: prompt.to_string(),
        workspace: ws.record.id.clone(),
        workspace_name: ws.record.name.clone(),
        start: StartPoint {
            branch: from,
            commit,
            trunk: is_trunk,
        },
        mode: req.mode,
        branch,
        worktree: canon(&wt),
        command: if runner == "command" {
            eff.command.clone()
        } else {
            None
        },
        runner,
        permissions: Some(
            eff.permissions
                .clone()
                .unwrap_or_else(|| DEFAULT_PERMISSIONS.into()),
        ),
        sandbox: eff.sandbox.unwrap_or(false),
        setup_from: Some(canon(&ws.path)),
        claude_mode: None,
        model: eff.model.clone(),
        check: eff.check.clone(),
        machine: workspace::machine(),
        state: AgentState::Queued,
        worker_pid: None,
        child_pid: None,
        session_id: None,
        created_at: created,
        updated_at: created,
        started_at: None,
        finished_at: None,
        seconds: 0,
        cost_usd: 0.0,
        turns: 0,
        last_step: None,
        question: None,
        pending_reply: None,
        stop_requested: false,
        tests: None,
        result: None,
        error: None,
        landed: None,
        messages: vec![Message {
            from: "you".into(),
            text: prompt.to_string(),
            at: created,
        }],
        stale: false,
        urgency: None,
        waiting_since: None,
    };
    write_agent(&dir, &a)?;
    add_step(&dir, "you", prompt);
    launch(&req.exe, &dir)
}

/// Spawn the worker; a failure to spawn fails the agent rather than losing it.
fn launch(exe: &Path, dir: &Path) -> Result<Agent> {
    match spawn_worker(exe, dir) {
        Ok(()) => read_agent(dir),
        Err(e) => update(dir, |a| {
            a.state = AgentState::Failed;
            a.error = Some(format!("could not start the agent's worker: {e}"));
            Ok(())
        }),
    }
}

/// Launch `agentvc agent-worker <dir>` detached from the caller.
fn spawn_worker(exe: &Path, dir: &Path) -> Result<()> {
    let log = dir.join("worker.log");
    let f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .at(&log)?;
    let mut cmd = Command::new(exe);
    cmd.arg("agent-worker")
        .arg(dir)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(f.try_clone().at(&log)?)
        .stderr(f);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().at(exe)?;
    Ok(())
}

// ---- reading -----------------------------------------------------------------

/// Fill the computed fields, and mark an agent whose worker died as failed.
fn refresh(
    dir: &Path,
    mut a: Agent,
    here: &str,
    tips: &mut dyn FnMut(&Agent) -> Option<String>,
) -> Agent {
    if a.state.is_active() && a.machine == here {
        let dead = match a.worker_pid {
            Some(pid) => !pid_alive(pid),
            // A worker records its pid within a second of starting.
            None => now() - a.updated_at > 30,
        };
        if dead {
            if let Ok(b) = update(dir, |b| {
                if b.state.is_active() && b.worker_pid == a.worker_pid {
                    b.state = AgentState::Failed;
                    b.error = Some("the agent's worker stopped unexpectedly".into());
                    b.question = None;
                    b.worker_pid = None;
                    b.child_pid = None;
                }
                Ok(())
            }) {
                a = b;
            }
        }
    }
    if a.state.is_done() {
        if let Some(t) = tips(&a) {
            a.stale = t != a.start.commit;
        }
    }
    a.urgency = a.compute_urgency();
    a.waiting_since = a.urgency.map(|_| match &a.question {
        Some(q) => q.asked_at,
        None => a.finished_at.unwrap_or(a.updated_at),
    });
    a
}

/// What [`list`] returns.
#[derive(Clone, Debug, Serialize)]
pub struct AgentList {
    /// Newest first, landed and discarded ones only with `all`.
    pub agents: Vec<Agent>,
    /// How many need a person.
    pub needs_you: usize,
    /// Agents working on this machine, and how many may.
    pub working: usize,
    pub limit: usize,
    pub machine: String,
    /// Whoever asked may start, answer, stop and land agents here. False for
    /// calls relayed by the hosted service to a daemon without
    /// `--allow-remote-agents`.
    pub can_steer: bool,
}

/// Every agent of the repo containing `cwd`.
pub fn list(cwd: &Path, all: bool) -> Result<AgentList> {
    let w = Window::open(cwd)?;
    let roots = space_roots(&w);
    let here = workspace::machine();
    let mut windows: std::collections::HashMap<String, Option<Window>> = Default::default();
    let locals = workspace::space_locals(&w).unwrap_or_default();
    let mut tips = |a: &Agent| -> Option<String> {
        let win = windows.entry(a.workspace.clone()).or_insert_with(|| {
            locals
                .iter()
                .find(|l| l.record.id == a.workspace)
                .and_then(|l| Window::open(&l.path).ok())
        });
        let win = win.as_ref()?;
        if a.start.trunk {
            win.base()
        } else {
            win.branch_tip(&a.start.branch)
        }
    };
    let mut agents: Vec<Agent> = Vec::new();
    for root in &roots {
        let Ok(rd) = fs::read_dir(root) else { continue };
        for e in rd.flatten() {
            let dir = e.path();
            let Ok(a) = read_agent(&dir) else { continue };
            if !all && a.state.is_closed() {
                continue;
            }
            // Agents made before checkouts shared a space can share an id;
            // this checkout's own win.
            if agents.iter().any(|x| x.id == a.id) {
                continue;
            }
            agents.push(refresh(&dir, a, &here, &mut tips));
        }
    }
    agents.sort_by_key(|a| std::cmp::Reverse(parse_agent_id(&a.id).unwrap_or(0)));
    let needs_you = agents.iter().filter(|a| a.urgency.is_some()).count();
    let working = agents
        .iter()
        .filter(|a| a.machine == here && a.state.is_working())
        .count();
    Ok(AgentList {
        agents,
        needs_you,
        working,
        limit: limit(&w),
        machine: here,
        can_steer: true,
    })
}

fn limit(w: &Window) -> usize {
    config(w, "agentvc.agents-max")
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_LIMIT)
}

/// One agent with its recent steps.
#[derive(Clone, Debug, Serialize)]
pub struct AgentDetail {
    pub agent: Agent,
    pub steps: Vec<Step>,
}

pub fn show(cwd: &Path, id: &str) -> Result<AgentDetail> {
    let dir = agent_dir(cwd, id)?;
    let agent = list(cwd, true)?
        .agents
        .into_iter()
        .find(|a| a.id == id.trim())
        .ok_or_else(|| Error::NotFound {
            kind: "agent",
            id: id.to_string(),
        })?;
    Ok(AgentDetail {
        agent,
        steps: steps(&dir, 200),
    })
}

// ---- talking to it -----------------------------------------------------------

/// Send the agent a follow-up. An agent waiting on a question gets it as the
/// answer; a stopped one starts another turn with it.
pub fn reply(cwd: &Path, id: &str, text: &str, exe: &Path) -> Result<Agent> {
    let text = text.trim();
    if text.is_empty() {
        return Err(agent_err("reply_required", "the reply is empty"));
    }
    let dir = agent_dir(cwd, id)?;
    let a = read_agent(&dir)?;
    match a.state {
        AgentState::Asking => return answer(cwd, id, Some(text), None),
        AgentState::NeedsApproval => {
            return Err(agent_err(
                "needs_approval",
                format!("{id} is waiting for Approve or Deny"),
            ))
        }
        AgentState::Queued | AgentState::Running => {
            return Err(agent_err(
                "agent_busy",
                format!("{id} is working; reply when it stops or asks"),
            ))
        }
        AgentState::Landed | AgentState::Discarded => {
            return Err(agent_err(
                "agent_closed",
                format!("{id} is {:?}", a.state).to_lowercase(),
            ))
        }
        _ => {}
    }
    let a = update(&dir, |a| {
        a.state = AgentState::Queued;
        a.pending_reply = Some(text.to_string());
        a.error = None;
        a.stop_requested = false;
        a.worker_pid = None;
        a.messages.push(Message {
            from: "you".into(),
            text: text.into(),
            at: now(),
        });
        Ok(())
    })?;
    add_step(&dir, "you", text);
    let _ = a;
    launch(exe, &dir)
}

/// Answer the agent's open question: `text` for `ask_human`, `allow` (and
/// an optional reason in `text`) for an approval.
pub fn answer(cwd: &Path, id: &str, text: Option<&str>, allow: Option<bool>) -> Result<Agent> {
    let dir = agent_dir(cwd, id)?;
    let a = update(&dir, |a| {
        let q = a.question.clone().ok_or_else(|| {
            agent_err(
                "no_question",
                format!("{} is not waiting on a question", a.id),
            )
        })?;
        let ans = match q.kind {
            QuestionKind::Ask => Answer {
                text: Some(
                    text.filter(|t| !t.trim().is_empty())
                        .ok_or_else(|| agent_err("reply_required", "the answer is empty"))?
                        .to_string(),
                ),
                allow: None,
            },
            QuestionKind::Approve => Answer {
                text: text.map(str::to_string),
                allow: Some(allow.ok_or_else(|| {
                    agent_err("allow_required", "approve or deny (--allow / --deny)")
                })?),
            },
        };
        let answers = dir.join("answers");
        fs::create_dir_all(&answers).at(&answers)?;
        let path = answers.join(format!("{}.json", q.id));
        fs::write(
            &path,
            serde_json::to_string(&ans).expect("answer serializes"),
        )
        .at(&path)?;
        let said = match (q.kind, ans.allow) {
            (QuestionKind::Approve, Some(true)) => "Approved".to_string(),
            (QuestionKind::Approve, _) => {
                format!(
                    "Denied{}",
                    text.map(|t| format!(": {t}")).unwrap_or_default()
                )
            }
            _ => ans.text.clone().unwrap_or_default(),
        };
        a.messages.push(Message {
            from: "you".into(),
            text: said,
            at: now(),
        });
        a.question = None;
        a.state = AgentState::Running;
        Ok(())
    })?;
    Ok(a)
}

/// The directory of the agent this process runs inside (`AGENTVC_AGENT_DIR`).
fn own_dir() -> Result<PathBuf> {
    std::env::var_os(ENV_DIR)
        .map(PathBuf::from)
        .filter(|d| d.join(RECORD).exists())
        .ok_or_else(|| {
            agent_err(
                "not_an_agent",
                "only an agent started by `agentvc agent start` can ask the person",
            )
        })
}

fn new_question_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("q{nanos:x}")
}

/// Put `q` to the person and wait for the answer file.
fn wait_for_answer(q: Question) -> Result<Answer> {
    let dir = own_dir()?;
    let state = match q.kind {
        QuestionKind::Ask => AgentState::Asking,
        QuestionKind::Approve => AgentState::NeedsApproval,
    };
    let id = q.id.clone();
    let text = q.text.clone();
    let kind = q.kind;
    update(&dir, |a| {
        a.state = state;
        a.question = Some(q);
        if kind == QuestionKind::Ask {
            a.messages.push(Message {
                from: "agent".into(),
                text: text.clone(),
                at: now(),
            });
        }
        Ok(())
    })?;
    add_step(&dir, "question", &text);
    let path = dir.join("answers").join(format!("{id}.json"));
    loop {
        if let Ok(t) = fs::read_to_string(&path) {
            if let Ok(ans) = serde_json::from_str::<Answer>(&t) {
                return Ok(ans);
            }
        }
        if read_agent(&dir).is_ok_and(|a| a.stop_requested) {
            return Err(agent_err("agent_stopped", "the person stopped this agent"));
        }
        std::thread::sleep(POLL);
    }
}

/// `ask_human`: ask the person one question and wait for the answer.
pub fn ask(question: &str) -> Result<String> {
    let q = Question {
        id: new_question_id(),
        kind: QuestionKind::Ask,
        text: question.trim().to_string(),
        tool: None,
        input: None,
        asked_at: now(),
    };
    Ok(wait_for_answer(q)?.text.unwrap_or_default())
}

/// The permission prompt tool: ask the person to approve `tool` with
/// `input`. Returns Claude Code's permission decision.
pub fn approve(tool: &str, input: &Value) -> Result<Value> {
    if tool.starts_with("mcp__agentvc__") {
        return Ok(json!({ "behavior": "allow", "updatedInput": input }));
    }
    let summary = step_of_tool(tool, input);
    let q = Question {
        id: new_question_id(),
        kind: QuestionKind::Approve,
        text: summary,
        tool: Some(tool.to_string()),
        input: Some(input.clone()),
        asked_at: now(),
    };
    let ans = wait_for_answer(q)?;
    Ok(if ans.allow == Some(true) {
        json!({ "behavior": "allow", "updatedInput": input })
    } else {
        json!({
            "behavior": "deny",
            "message": ans.text.unwrap_or_else(|| "The person denied this.".into()),
        })
    })
}

// ---- stopping, landing, discarding -------------------------------------------

#[cfg(unix)]
fn terminate(pid: i64) {
    if let Ok(p) = libc::pid_t::try_from(pid) {
        // The child leads its own process group: stop its children too.
        unsafe {
            libc::kill(-p, libc::SIGTERM);
            libc::kill(p, libc::SIGTERM);
        }
    }
}

#[cfg(not(unix))]
fn terminate(_pid: i64) {}

/// Stop a queued or working agent. It ends up `stopped`, with whatever it
/// did so far committed.
pub fn stop(cwd: &Path, id: &str) -> Result<Agent> {
    let dir = agent_dir(cwd, id)?;
    let a = update(&dir, |a| {
        if !a.state.is_active() {
            return Err(agent_err("not_running", format!("{} is not running", a.id)));
        }
        a.stop_requested = true;
        Ok(())
    })?;
    if let Some(pid) = a.child_pid.filter(|p| pid_alive(*p)) {
        terminate(pid);
    }
    Ok(a)
}

fn remove_worktree(w: &Window, a: &Agent) {
    if a.worktree.exists() {
        let _ = w.repo.git(
            &[
                "worktree",
                "remove",
                "--force",
                &a.worktree.to_string_lossy(),
            ],
            None,
        );
    }
    let _ = w.repo.git(&["worktree", "prune"], None);
}

/// Put the agent's work on its workspace's stack: its branch as a child of
/// the start branch ([`Mode::New`]) or as the start branch's new version
/// ([`Mode::Revise`]). Goes through the workspace's edit queue, so it waits
/// while an agent works in that workspace's own checkout.
pub fn land(cwd: &Path, id: &str) -> Result<(Agent, EditOutcome)> {
    let dir = agent_dir(cwd, id)?;
    let a = read_agent(&dir)?;
    if !a.state.is_done() {
        return Err(agent_err_with(
            "not_ready",
            format!("{id} is {}; land it after it stops", state_word(a.state)),
            json!({ "state": a.state }),
        ));
    }
    let w = agent_window(cwd, &a)?;
    let tip = w.branch_tip(&a.branch).ok_or_else(|| {
        agent_err(
            "no_branch",
            format!("the agent's branch {} is gone", a.branch),
        )
    })?;
    if tip == a.start.commit {
        return Err(agent_err(
            "nothing_to_land",
            format!("{id} changed nothing"),
        ));
    }
    let from = format!("agent:{id}");
    let (op, landed_on) = match a.mode {
        Mode::New => (
            Op::Adopt {
                branch: a.branch.clone(),
                parent: a.start.branch.clone(),
            },
            a.branch.clone(),
        ),
        Mode::Revise => (
            Op::Revise {
                branch: a.start.branch.clone(),
                commit: tip.clone(),
                base: a.start.commit.clone(),
            },
            a.start.branch.clone(),
        ),
    };
    // The branch must not stay checked out in the agent's worktree, or the
    // workspace could not check it out.
    remove_worktree(&w, &a);
    let out = workspace::edit(cwd, &a.workspace, op, Some(&from), false)?;
    if a.mode == Mode::Revise && out.outcome == RequestStatus::Applied {
        let _ = w.repo.git(&["branch", "-D", &a.branch], None);
    }
    let landing = Landing {
        outcome: out.outcome,
        request_id: out.request_id.clone(),
        branch: landed_on,
        at: now(),
    };
    let a = update(&dir, |a| {
        a.state = AgentState::Landed;
        a.landed = Some(landing);
        Ok(())
    })?;
    add_step(
        &dir,
        "result",
        &format!(
            "Landed on {}",
            a.landed.as_ref().map(|l| l.branch.as_str()).unwrap_or("")
        ),
    );
    Ok((a, out))
}

/// Throw the agent's work away: stop it, remove its worktree and branch.
pub fn discard(cwd: &Path, id: &str) -> Result<Agent> {
    let dir = agent_dir(cwd, id)?;
    let a = read_agent(&dir)?;
    if a.state == AgentState::Landed {
        return Err(agent_err("agent_closed", format!("{id} already landed")));
    }
    if a.state.is_active() {
        let _ = stop(cwd, id);
    }
    let w = agent_window(cwd, &a)?;
    remove_worktree(&w, &a);
    let _ = w.repo.git(&["branch", "-D", &a.branch], None);
    update(&dir, |a| {
        a.state = AgentState::Discarded;
        a.stop_requested = true;
        a.question = None;
        Ok(())
    })
}

fn state_word(s: AgentState) -> &'static str {
    match s {
        AgentState::Queued => "queued",
        AgentState::Running => "running",
        AgentState::Asking => "asking",
        AgentState::NeedsApproval => "waiting for approval",
        AgentState::Ready => "ready",
        AgentState::TestsFailing => "failing tests",
        AgentState::Failed => "failed",
        AgentState::Stopped => "stopped",
        AgentState::Landed => "landed",
        AgentState::Discarded => "discarded",
    }
}

// ---- the worker --------------------------------------------------------------

/// One line describing a tool call, e.g. `Edit src/lib.rs`.
fn step_of_tool(name: &str, input: &Value) -> String {
    let field = [
        "file_path",
        "command",
        "pattern",
        "path",
        "url",
        "description",
        "query",
    ]
    .iter()
    .find_map(|k| input.get(*k).and_then(Value::as_str));
    let mut s = match field {
        Some(f) => format!("{name} {}", f.lines().next().unwrap_or("")),
        None => name.to_string(),
    };
    if s.chars().count() > 160 {
        s = s.chars().take(160).collect::<String>() + "…";
    }
    s
}

fn first_line(s: &str) -> String {
    let l = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if l.chars().count() > 160 {
        l.chars().take(160).collect::<String>() + "…"
    } else {
        l.to_string()
    }
}

/// Apply one line of Claude Code's `stream-json` output to the record.
fn on_stream_line(dir: &Path, line: &str) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("system") => {
            if let Some(sid) = v.get("session_id").and_then(Value::as_str) {
                let sid = sid.to_string();
                let mode = v
                    .get("permissionMode")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let _ = update(dir, |a| {
                    a.session_id = Some(sid);
                    if mode.is_some() {
                        a.claude_mode = mode;
                    }
                    Ok(())
                });
            }
        }
        Some("assistant") => {
            let content = v
                .pointer("/message/content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for c in content {
                let (kind, text) = match c.get("type").and_then(Value::as_str) {
                    Some("text") => (
                        "text",
                        c.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ),
                    Some("tool_use") => (
                        "tool",
                        step_of_tool(
                            c.get("name").and_then(Value::as_str).unwrap_or("tool"),
                            c.get("input").unwrap_or(&Value::Null),
                        ),
                    ),
                    _ => continue,
                };
                if text.trim().is_empty() {
                    continue;
                }
                add_step(dir, kind, &text);
                let step = first_line(&text);
                let _ = update(dir, |a| {
                    a.last_step = Some(step);
                    Ok(())
                });
            }
        }
        Some("result") => {
            let cost = v
                .get("total_cost_usd")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let turns = v.get("num_turns").and_then(Value::as_u64).unwrap_or(0);
            let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let text = v
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !text.is_empty() {
                add_step(dir, "result", &text);
            }
            let _ = update(dir, |a| {
                a.cost_usd += cost;
                a.turns += turns;
                if !text.is_empty() {
                    a.messages.push(Message {
                        from: "agent".into(),
                        text: text.clone(),
                        at: now(),
                    });
                }
                if is_error {
                    a.error = Some(first_line(&text));
                }
                Ok(())
            });
        }
        _ => {}
    }
}

/// The Codex CLI, non-interactive. `AGENTVC_CODEX` names another program.
const CODEX: &str = r#"exec "${AGENTVC_CODEX:-codex}" exec --full-auto ${AGENTVC_AGENT_MODEL:+-m "$AGENTVC_AGENT_MODEL"} "$AGENTVC_AGENT_PROMPT""#;
/// The Cursor CLI in print mode. `AGENTVC_CURSOR` names another program.
const CURSOR: &str = r#"exec "${AGENTVC_CURSOR:-cursor-agent}" -p --force --output-format text ${AGENTVC_AGENT_MODEL:+--model "$AGENTVC_AGENT_MODEL"} "$AGENTVC_AGENT_PROMPT""#;

/// The command line for one turn of the agent.
fn turn_command(a: &Agent, dir: &Path, exe: &Path, prompt: &str) -> Command {
    let mut cmd = match a.runner.as_str() {
        "command" | "codex" | "cursor" => {
            let script = match a.runner.as_str() {
                "codex" => CODEX,
                "cursor" => CURSOR,
                _ => a.command.as_deref().unwrap_or("true"),
            };
            let mut c = Command::new("sh");
            c.arg("-c").arg(script);
            c.env("AGENTVC_AGENT_PROMPT", prompt)
                .env("AGENTVC_AGENT_MODEL", a.model.as_deref().unwrap_or(""))
                .env("AGENTVC_BIN", exe);
            c
        }
        _ => {
            let program = std::env::var(ENV_CLAUDE).unwrap_or_else(|_| "claude".into());
            // The person's own setup for the workspace's checkout (#162):
            // its local settings and MCP servers, plus agentvc's guard rails.
            let setup = a
                .setup_from
                .as_deref()
                .map(crate::agent_setup::read)
                .unwrap_or_default();
            let mut servers = setup.mcp_servers_arg().clone();
            servers.insert(
                "agentvc".into(),
                json!({
                    "type": "stdio",
                    "command": exe,
                    "args": ["mcp"],
                    "env": { ENV_DIR: dir },
                }),
            );
            let mcp = json!({ "mcpServers": servers });
            let mut c = Command::new(program);
            c.arg("-p")
                .arg(prompt)
                .args(["--output-format", "stream-json", "--verbose"])
                .args([
                    "--permission-mode",
                    claude_permission_mode(a.permissions.as_deref()),
                ])
                .arg("--settings")
                .arg(setup.settings_arg(exe, a.sandbox).to_string())
                .arg("--mcp-config")
                .arg(mcp.to_string())
                .args(["--permission-prompt-tool", "mcp__agentvc__approve"])
                .args(["--allowedTools", "mcp__agentvc__ask_human"])
                .arg("--append-system-prompt")
                .arg(SYSTEM_PROMPT);
            if let Some(s) = &a.session_id {
                c.arg("--resume").arg(s);
            }
            if let Some(m) = &a.model {
                c.arg("--model").arg(m);
            }
            c
        }
    };
    cmd.current_dir(&a.worktree)
        .env(ENV_DIR, dir)
        .env("AGENTVC_AGENT", &a.id)
        .env(crate::agent_setup::ENV_WORKTREE, &a.worktree)
        // `ask_human` waits for a person, which can take hours.
        .env("MCP_TOOL_TIMEOUT", "86400000")
        .env_remove("CLAUDECODE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd
}

/// Commit what the agent left in its worktree.
fn commit_work(a: &Agent) -> Result<Option<String>> {
    let g = git_in(&a.worktree);
    g.git(&["add", "-A"], None)?;
    if g.git(&["diff", "--cached", "--quiet"], None).is_ok() {
        return Ok(None);
    }
    let msg = format!("{}\n\n{}\n\nAgent: {}", a.title, a.prompt, a.id);
    let have_identity = g
        .git(&["config", "user.email"], None)
        .is_ok_and(|e| !e.is_empty());
    if have_identity {
        g.git(&["commit", "--quiet", "--no-verify", "-F", "-"], Some(&msg))?;
    } else {
        g.git_env(
            &["commit", "--quiet", "--no-verify", "-F", "-"],
            Some(&msg),
            &[
                ("GIT_AUTHOR_NAME", "agentvc agent"),
                ("GIT_AUTHOR_EMAIL", "agent@agentvc.invalid"),
                ("GIT_COMMITTER_NAME", "agentvc agent"),
                ("GIT_COMMITTER_EMAIL", "agent@agentvc.invalid"),
            ],
        )?;
    }
    Ok(g.rev("HEAD"))
}

fn result_of(a: &Agent) -> AgentResult {
    let g = git_in(&a.worktree);
    let head = g.rev("HEAD");
    let mut r = AgentResult {
        commit: head.clone().filter(|h| *h != a.start.commit),
        ..Default::default()
    };
    if let Ok(out) = g.git(&["diff", "--numstat", &a.start.commit, "HEAD"], None) {
        for l in out.lines() {
            let mut it = l.splitn(3, '\t');
            let ins = it.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
            let del = it.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
            if let Some(p) = it.next() {
                r.files += 1;
                r.insertions += ins;
                r.deletions += del;
                r.paths.push(p.to_string());
            }
        }
    }
    if let Ok(out) = g.git(
        &[
            "diff",
            "--name-status",
            "--no-renames",
            &a.start.commit,
            "HEAD",
        ],
        None,
    ) {
        for l in out.lines() {
            if let Some((st, p)) = l.split_once('\t') {
                let kind = match st {
                    "A" => "added",
                    "D" => "removed",
                    _ => "modified",
                };
                r.changes.push(FileKind {
                    path: p.to_string(),
                    kind: kind.into(),
                });
            }
        }
    }
    r
}

fn run_check(a: &Agent, command: &str) -> CheckResult {
    let t = Instant::now();
    let out = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&a.worktree)
        .stdin(Stdio::null())
        .output();
    let (code, text) = match out {
        Ok(o) => (
            o.status.code(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        ),
        Err(e) => (None, format!("could not run {command}: {e}")),
    };
    CheckResult {
        command: command.to_string(),
        exit_code: code,
        passed: code == Some(0),
        tail: tail(&text, TAIL_LINES),
        duration_ms: t.elapsed().as_millis() as i64,
    }
}

/// The background process behind one agent turn: wait for a slot, run the
/// runner, then commit, check and record the outcome. `agentvc agent-worker`.
pub fn work(dir: &Path, exe: &Path) -> Result<Agent> {
    let me = std::process::id() as i64;
    let a = update(dir, |a| {
        a.worker_pid = Some(me);
        Ok(())
    })?;
    // Wait for a free slot on this machine. Counting and taking the slot
    // happen under one lock on the agents directory, so two workers that
    // start together can't both take the last slot.
    let common = dir.parent().map(Path::to_path_buf).unwrap_or_default();
    let wt = Window::open(&a.worktree).ok();
    let max = wt.as_ref().map(limit).unwrap_or(DEFAULT_LIMIT);
    let (a, began) = loop {
        let cur = read_agent(dir)?;
        if cur.stop_requested {
            return update(dir, |a| {
                a.state = AgentState::Stopped;
                a.stop_requested = false;
                a.worker_pid = None;
                Ok(())
            });
        }
        let slots = lock_record(&common)?;
        let here = workspace::machine();
        let busy: Vec<Agent> = fs::read_dir(&common)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| read_agent(&e.path()).ok())
                    .filter(|o| {
                        o.id != cur.id
                            && o.machine == here
                            && o.state.is_working()
                            && o.worker_pid.is_some_and(pid_alive)
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The workspace's own cap, read each time so a change applies to
        // agents already waiting.
        let ws_max = wt
            .as_ref()
            .and_then(|w| workspace_settings(w, &cur.workspace).max);
        let in_ws = busy.iter().filter(|o| o.workspace == cur.workspace).count();
        if busy.len() < max && ws_max.is_none_or(|m| in_ws < m) {
            let began = now();
            let a = update(dir, |a| {
                a.state = AgentState::Running;
                a.started_at.get_or_insert(began);
                a.finished_at = None;
                a.error = None;
                Ok(())
            })?;
            drop(slots);
            break (a, began);
        }
        drop(slots);
        std::thread::sleep(Duration::from_secs(1));
    };
    let prompt = match (&a.pending_reply, &a.session_id, a.runner.as_str()) {
        (Some(r), Some(_), _) | (Some(r), _, "command") => r.clone(),
        // No session to resume: give the whole conversation again.
        (Some(r), None, _) => format!("{}\n\nFollow-up from the person: {r}", a.prompt),
        (None, _, _) => a.prompt.clone(),
    };
    let out_path = dir.join(OUTPUT);
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out_path)
        .at(&out_path)?;
    let mut cmd = turn_command(&a, dir, exe, &prompt);
    cmd.stderr(log.try_clone().at(&out_path)?);
    let spawned = cmd.spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let why = format!("could not start the {} runner: {e}", a.runner);
            add_step(dir, "line", &why);
            return update(dir, |a| {
                a.state = AgentState::Failed;
                a.error = Some(why);
                a.pending_reply = None;
                a.worker_pid = None;
                a.finished_at = Some(now());
                Ok(())
            });
        }
    };
    let pid = child.id() as i64;
    update(dir, |a| {
        a.child_pid = Some(pid);
        a.pending_reply = None;
        Ok(())
    })?;
    let stream = a.runner != "command";
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            let _ = writeln!(log, "{line}");
            if stream {
                on_stream_line(dir, &line);
            } else if !line.trim().is_empty() {
                add_step(dir, "line", &line);
                let step = first_line(&line);
                let _ = update(dir, |a| {
                    a.last_step = Some(step);
                    Ok(())
                });
            }
        }
    }
    let status = child.wait().at("runner")?;
    let cur = read_agent(dir)?;
    let committed = commit_work(&cur);
    let result = result_of(&cur);
    let stopped = cur.stop_requested;
    let failed = !status.success() && !stopped;
    let check = match (&cur.check, failed || stopped) {
        (Some(c), false) => {
            update(dir, |a| {
                a.last_step = Some(format!("Checking: {c}"));
                Ok(())
            })?;
            let r = run_check(&cur, c);
            add_step(
                dir,
                "check",
                &format!(
                    "{} {}",
                    if r.passed { "Passed:" } else { "Failed:" },
                    r.command
                ),
            );
            Some(r)
        }
        _ => None,
    };
    let output_tail = tail(&fs::read_to_string(&out_path).unwrap_or_default(), 5);
    update(dir, |a| {
        a.seconds += now() - began;
        a.finished_at = Some(now());
        a.worker_pid = None;
        a.child_pid = None;
        a.question = None;
        a.result = Some(result);
        if let Err(e) = &committed {
            a.error = Some(format!("could not commit the agent's work: {e}"));
        }
        a.state = if stopped {
            AgentState::Stopped
        } else if failed {
            if a.error.is_none() {
                a.error = Some(match status.code() {
                    Some(c) => format!(
                        "the runner exited with {c}{}",
                        output_tail
                            .last()
                            .map(|l| format!(": {}", first_line(l)))
                            .unwrap_or_default()
                    ),
                    None => "the runner was killed".into(),
                });
            }
            AgentState::Failed
        } else if check.as_ref().is_some_and(|c| !c.passed) {
            AgentState::TestsFailing
        } else {
            AgentState::Ready
        };
        a.stop_requested = false;
        if let Some(c) = &check {
            a.last_step = Some(
                if c.passed {
                    "Checks passed"
                } else {
                    "Checks failed"
                }
                .into(),
            );
        }
        if check.is_some() {
            a.tests = check;
        }
        Ok(())
    })
}

// The tests drive agents with `sh` commands.
#[cfg(all(test, unix))]
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

    /// A repo with origin/main and one stack branch `feat`.
    fn repo() -> (tempfile::TempDir, PathBuf) {
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
        let wts = t.path().join("wts");
        git(
            &work,
            &["config", "agentvc.agent-worktrees", wts.to_str().unwrap()],
        );
        fs::write(work.join("a.txt"), "one\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", "init"]);
        git(
            &work,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&work, &["push", "--quiet", "origin", "main"]);
        git(&work, &["fetch", "--quiet", "origin"]);
        let mut w = Window::open(&work).unwrap();
        fs::write(work.join("b.txt"), "feat\n").unwrap();
        git(&work, &["add", "."]);
        w.create(Some("feat"), Some("feat"), false).unwrap();
        (t, canon(&work))
    }

    /// Run the worker inline (the test binary is not `agentvc`).
    fn start_inline(work: &Path, req: StartRequest) -> Agent {
        let a = start(
            work,
            &StartRequest {
                exe: PathBuf::from("true"),
                ..req
            },
        )
        .unwrap();
        let dir = agent_dir(work, &a.id).unwrap();
        work_inline(&dir)
    }

    fn work_inline(dir: &Path) -> Agent {
        // The spawn of the real worker failed or is racing us; take over.
        let _ = update(dir, |a| {
            a.worker_pid = None;
            Ok(())
        });
        work(dir, Path::new("true")).unwrap()
    }

    fn req(prompt: &str, command: &str) -> StartRequest {
        StartRequest {
            workspace: None,
            from: Some("feat".into()),
            at: None,
            prompt: prompt.into(),
            mode: Mode::New,
            model: None,
            check: None,
            command: Some(command.into()),
            runner: None,
            permissions: None,
            exe: PathBuf::new(),
        }
    }

    #[test]
    fn agents_inherit_their_workspaces_settings() {
        let (_t, work) = repo();
        // Nothing set: Claude Code, repo check from git config.
        git(&work, &["config", "agentvc.agent-check", "true"]);
        let s = settings(&work, None).unwrap();
        assert!(s.settings.is_empty());
        assert_eq!(s.defaults.check.as_deref(), Some("true"));
        assert_eq!(s.machine_limit, DEFAULT_LIMIT);

        // Bad values are refused.
        let bad = |x: AgentSettings| set_settings(&work, None, x).unwrap_err().code();
        assert_eq!(
            bad(AgentSettings {
                runner: Some("vim".into()),
                ..Default::default()
            }),
            "bad_setting"
        );
        assert_eq!(
            bad(AgentSettings {
                max: Some(0),
                ..Default::default()
            }),
            "bad_setting"
        );
        assert_eq!(
            bad(AgentSettings {
                runner: Some("command".into()),
                ..Default::default()
            }),
            "command_required"
        );

        let s = set_settings(
            &work,
            None,
            AgentSettings {
                runner: Some("command".into()),
                command: Some("printf 'ws\\n' > ws.txt".into()),
                check: Some("grep -q ws ws.txt".into()),
                model: Some("  ".into()),
                permissions: Some("edits".into()),
                sandbox: Some(true),
                max: Some(2),
            },
        )
        .unwrap();
        assert_eq!(s.settings.model, None, "blank clears");
        // It survives stack edits and is part of the workspace's info.
        let mut w = Window::open(&work).unwrap();
        fs::write(work.join("d.txt"), "d\n").unwrap();
        git(&work, &["add", "."]);
        w.create(Some("more"), Some("more"), false).unwrap();
        let w = Window::open(&work).unwrap();
        assert_eq!(w.state.agents.max, Some(2));

        // An agent started without its own settings uses the workspace's.
        let a = start_inline(
            &work,
            StartRequest {
                command: None,
                ..req("Write ws", "unused")
            },
        );
        assert_eq!(a.runner, "command");
        assert_eq!(a.permissions.as_deref(), Some("edits"));
        assert!(a.sandbox);
        let p = permissions(&work).unwrap();
        assert_eq!(p.workspaces[0].permissions, "edits");
        assert_eq!(p.workspaces[0].permissions_from, "workspace");
        assert!(p.workspaces[0].sandbox);
        assert_eq!(p.workspaces[0].claude_mode, "acceptEdits");
        assert_eq!(a.check.as_deref(), Some("grep -q ws ws.txt"));
        assert_eq!(a.state, AgentState::Ready, "{a:?}");

        // The request wins over the workspace.
        let a = start_inline(
            &work,
            StartRequest {
                check: Some("false".into()),
                ..req("Own check", "true")
            },
        );
        assert_eq!(a.state, AgentState::TestsFailing, "{a:?}");
    }

    #[test]
    fn agent_runs_in_its_own_worktree_and_lands_on_top() {
        let (_t, work) = repo();
        let a = start_inline(
            &work,
            StartRequest {
                check: Some("grep -q hello c.txt".into()),
                ..req(
                    "Add a greeting file",
                    "echo working; printf 'hello\\n' > c.txt",
                )
            },
        );
        assert_eq!(a.state, AgentState::Ready, "{a:?}");
        assert_eq!(a.branch, "add-a-greeting-file");
        assert_eq!(a.result.as_ref().unwrap().paths, vec!["c.txt"]);
        assert_eq!(a.result.as_ref().unwrap().changes[0].kind, "added");
        assert!(a.tests.as_ref().unwrap().passed);
        assert_eq!(a.last_step.as_deref(), Some("Checks passed"));
        let dir = agent_dir(&work, &a.id).unwrap();
        assert!(steps(&dir, 0)
            .iter()
            .any(|s| s.kind == "line" && s.text == "working"));
        // The workspace's checkout is untouched.
        assert!(!work.join("c.txt").exists());
        // Agent worktrees are not workspaces.
        let w = Window::open(&work).unwrap();
        assert_eq!(workspace::locals(&w).unwrap().len(), 1);

        let l = list(&work, false).unwrap();
        assert_eq!(l.agents.len(), 1);
        assert_eq!(l.needs_you, 1);
        assert_eq!(l.agents[0].urgency, Some(4));

        let (landed, out) = land(&work, &a.id).unwrap();
        assert_eq!(out.outcome, RequestStatus::Applied);
        assert_eq!(landed.state, AgentState::Landed);
        let w = Window::open(&work).unwrap();
        let t = w
            .state
            .branches
            .iter()
            .find(|b| b.name == a.branch)
            .unwrap();
        assert_eq!(t.parent, "feat");
        assert!(!a.worktree.exists());
        assert!(list(&work, false).unwrap().agents.is_empty());
    }

    #[test]
    fn revise_moves_the_start_branch_and_failing_checks_need_you() {
        let (_t, work) = repo();
        let old_tip = Window::open(&work).unwrap().branch_tip("feat").unwrap();
        let a = start_inline(
            &work,
            StartRequest {
                mode: Mode::Revise,
                check: Some("false".into()),
                ..req("Tweak b", "printf 'feat 2\\n' > b.txt")
            },
        );
        assert_eq!(a.state, AgentState::TestsFailing);
        assert_eq!(a.compute_urgency(), Some(3));
        assert_eq!(a.branch, format!("agentvc/{}", a.id));
        land(&work, &a.id).unwrap();
        let w = Window::open(&work).unwrap();
        let tip = w.branch_tip("feat").unwrap();
        assert_ne!(tip, old_tip);
        assert_eq!(fs::read_to_string(work.join("b.txt")).unwrap(), "feat 2\n");
        assert!(w.branch_tip(&a.branch).is_none());
    }

    #[test]
    fn failed_runner_reply_and_discard() {
        let (_t, work) = repo();
        let a = start_inline(&work, req("Break", "echo boom; exit 3"));
        assert_eq!(a.state, AgentState::Failed);
        assert!(
            a.error.as_deref().unwrap().contains("exited with 3"),
            "{:?}",
            a.error
        );
        assert!(matches!(
            land(&work, &a.id),
            Err(Error::Stack {
                code: "nothing_to_land",
                ..
            })
        ));
        // A reply starts another turn with the reply as the prompt.
        let dir = agent_dir(&work, &a.id).unwrap();
        update(&dir, |a| {
            a.command = Some("printf '%s\\n' \"$AGENTVC_AGENT_PROMPT\" > reply.txt".into());
            Ok(())
        })
        .unwrap();
        let r = reply(&work, &a.id, "try again", Path::new("true")).unwrap();
        assert_eq!(r.state, AgentState::Queued);
        let b = work_inline(&dir);
        assert_eq!(b.state, AgentState::Ready, "{b:?}");
        assert_eq!(
            fs::read_to_string(b.worktree.join("reply.txt")).unwrap(),
            "try again\n"
        );
        let d = discard(&work, &a.id).unwrap();
        assert_eq!(d.state, AgentState::Discarded);
        assert!(Window::open(&work).unwrap().branch_tip(&a.branch).is_none());
    }

    #[test]
    fn questions_wait_for_answers() {
        let (_t, work) = repo();
        let a = start(
            &work,
            &StartRequest {
                runner: None,
                permissions: None,
                exe: PathBuf::from("true"),
                ..req("Ask", "true")
            },
        );
        assert_eq!(a.unwrap().state, AgentState::Queued);
        let dir = agent_dir(&work, "ag1").unwrap();
        std::env::set_var(ENV_DIR, &dir);
        let d2 = dir.clone();
        let asker = std::thread::spawn(move || {
            let _ = d2;
            ask("Tabs or commas?").unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while read_agent(&dir).unwrap().question.is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        let q = read_agent(&dir).unwrap();
        assert_eq!(q.state, AgentState::Asking);
        assert_eq!(q.compute_urgency(), Some(1));
        assert!(matches!(
            answer(&work, "ag1", None, Some(true)),
            Err(Error::Stack {
                code: "reply_required",
                ..
            })
        ));
        answer(&work, "ag1", Some("Both"), None).unwrap();
        assert_eq!(asker.join().unwrap(), "Both");

        let d3 = dir.clone();
        let approver = std::thread::spawn(move || {
            let _ = d3;
            approve("Bash", &json!({"command": "rm -rf build"})).unwrap()
        });
        while read_agent(&dir).unwrap().question.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let q = read_agent(&dir).unwrap();
        assert_eq!(q.state, AgentState::NeedsApproval);
        assert_eq!(q.question.unwrap().text, "Bash rm -rf build");
        answer(&work, "ag1", Some("not now"), Some(false)).unwrap();
        let v = approver.join().unwrap();
        assert_eq!(v["behavior"], "deny");
        assert_eq!(v["message"], "not now");
        std::env::remove_var(ENV_DIR);
    }

    #[test]
    fn stream_json_updates_the_record() {
        let (_t, work) = repo();
        let script = r#"printf '%s\n' "$@" > "$AGENTVC_AGENT_DIR/args.txt"
echo '{"type":"system","subtype":"init","session_id":"s-1","permissionMode":"auto"}'
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/x.rs"}}]}}'
echo '{"type":"result","total_cost_usd":0.25,"num_turns":3,"is_error":false,"result":"Done: added x"}'"#;
        let a = start(
            &work,
            &StartRequest {
                runner: None,
                permissions: None,
                exe: PathBuf::from("true"),
                ..req("Stream", "true")
            },
        );
        assert!(a.is_ok());
        let dir = agent_dir(&work, "ag1").unwrap();
        // Pretend to be Claude Code: a fake `claude` that prints stream-json.
        let fake = dir.join("fake-claude");
        fs::write(&fake, format!("#!/bin/sh\n{script}\n")).unwrap();
        Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
        update(&dir, |a| {
            a.runner = "claude".into();
            a.command = None;
            Ok(())
        })
        .unwrap();
        std::env::set_var(ENV_CLAUDE, &fake);
        let b = work_inline(&dir);
        std::env::remove_var(ENV_CLAUDE);
        assert_eq!(b.state, AgentState::Ready, "{b:?}");
        assert_eq!(b.session_id.as_deref(), Some("s-1"));
        assert_eq!(
            b.permissions.as_deref(),
            Some("auto"),
            "auto mode by default"
        );
        assert_eq!(b.claude_mode.as_deref(), Some("auto"));
        let args = fs::read_to_string(dir.join("args.txt")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        let after = |flag: &str| args[args.iter().position(|a| *a == flag).unwrap() + 1];
        assert_eq!(after("--permission-mode"), "auto");
        let settings: Value = serde_json::from_str(after("--settings")).unwrap();
        assert!(settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with("agent guard"));
        assert!(settings["permissions"]["deny"]
            .as_array()
            .unwrap()
            .contains(&json!("Bash(git push:*)")));
        assert_eq!(b.cost_usd, 0.25);
        assert_eq!(b.turns, 3);
        assert_eq!(b.last_step.as_deref(), Some("Edit src/x.rs"));
        assert_eq!(b.messages.last().unwrap().text, "Done: added x");
        let s = steps(&dir, 0);
        assert!(s.iter().any(|s| s.kind == "tool"));
    }
}
