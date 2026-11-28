//! The tool table: one MCP tool per CLI command.
//!
//! Each tool has a JSON schema for its arguments and turns those arguments
//! into the argv of the matching `agentvc` command. Options are always passed
//! as `--name=value` and positionals after `--`, so a value that starts with
//! `-` can never be read as a flag.

use serde_json::{json, Map, Value};

/// The tool lists a server can offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// The workspace's stack of branches and PRs.
    Stack,
    /// Review comments on a branch or an agent.
    Review,
    /// Only for agents started from the control center.
    Agent,
    /// The attempt tree (tasks, forks, snapshots...), behind `--attempts`.
    Attempt,
}

/// Which tools a server offers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    /// Inside an agent started from the control center
    /// (`AGENTVC_AGENT_DIR` is set): its own tools, review comments and a
    /// read-only stack log; agentvc commits, lands and submits for it.
    pub agent: bool,
    /// Also the attempt tools (`agentvc mcp --attempts`).
    pub attempts: bool,
}

impl Profile {
    /// Whether `t` is offered.
    pub fn offers(&self, t: &Tool) -> bool {
        match t.group {
            Group::Review => true,
            Group::Agent => self.agent,
            Group::Stack => !self.agent || t.read_only,
            Group::Attempt => self.attempts,
        }
    }

    /// The tools offered, in `tools/list` order.
    pub fn tools(&self) -> impl Iterator<Item = &'static Tool> + '_ {
        TOOLS.iter().filter(move |t| self.offers(t))
    }
}

/// Arguments did not match the tool's schema.
#[derive(Debug, PartialEq)]
pub struct BadArgs(pub String);

/// One MCP tool.
pub struct Tool {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// Tool only reads state (MCP `readOnlyHint`).
    pub read_only: bool,
    /// Tool may close or rewrite something a human cares about
    /// (MCP `destructiveHint`).
    pub destructive: bool,
    /// Tool reaches outside the machine (MCP `openWorldHint`).
    pub open_world: bool,
    /// Which tool list it belongs to (see [`Profile`]).
    pub group: Group,
    /// Properties of the input schema, as (name, schema, required).
    pub params: fn() -> Vec<(&'static str, Value, bool)>,
    /// Build the argv after `agentvc --json -C <dir>`.
    pub argv: fn(&Args) -> Result<Vec<String>, BadArgs>,
}

impl Tool {
    /// The entry for `tools/list`.
    pub fn describe(&self) -> Value {
        let mut props = Map::new();
        let mut required = vec![];
        for (name, schema, req) in (self.params)() {
            props.insert(name.into(), schema);
            if req {
                required.push(name);
            }
        }
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": {
                "type": "object",
                "properties": props,
                "required": required,
                "additionalProperties": false,
            },
            "annotations": {
                "title": self.title,
                "readOnlyHint": self.read_only,
                "destructiveHint": self.destructive,
                "idempotentHint": self.read_only,
                "openWorldHint": self.open_world,
            },
        })
    }

    /// Check `args` against the schema and build the argv.
    pub fn build(&self, args: &Value) -> Result<Vec<String>, BadArgs> {
        let obj = match args {
            Value::Null => Map::new(),
            Value::Object(m) => m.clone(),
            _ => return Err(BadArgs("arguments must be an object".into())),
        };
        let params = (self.params)();
        for key in obj.keys() {
            if !params.iter().any(|(n, _, _)| n == key) {
                return Err(BadArgs(format!("unknown argument {key:?}")));
            }
        }
        for (name, _, req) in &params {
            if *req && obj.get(*name).is_none_or(Value::is_null) {
                return Err(BadArgs(format!("missing required argument {name:?}")));
            }
        }
        (self.argv)(&Args(obj))
    }
}

/// Tool arguments with typed accessors.
pub struct Args(Map<String, Value>);

impl Args {
    fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key).filter(|v| !v.is_null())
    }

    /// A string argument. Numbers are accepted too, so `3` works for `a3`.
    fn str(&self, key: &str) -> Result<Option<String>, BadArgs> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(Value::Number(n)) => Ok(Some(n.to_string())),
            Some(_) => Err(BadArgs(format!("{key} must be a string"))),
        }
    }

    fn req(&self, key: &str) -> Result<String, BadArgs> {
        self.str(key)?
            .ok_or_else(|| BadArgs(format!("missing required argument {key:?}")))
    }

    fn bool(&self, key: &str) -> Result<Option<bool>, BadArgs> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(BadArgs(format!("{key} must be a boolean"))),
        }
    }

    fn strings(&self, key: &str) -> Result<Vec<String>, BadArgs> {
        match self.get(key) {
            None => Ok(vec![]),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s.clone()),
                    _ => Err(BadArgs(format!("{key} must be an array of strings"))),
                })
                .collect(),
            Some(_) => Err(BadArgs(format!("{key} must be an array of strings"))),
        }
    }
}

/// Collects argv: options first, then `--` and positionals.
#[derive(Default)]
struct Argv {
    cmd: Vec<String>,
    opts: Vec<String>,
    pos: Vec<String>,
}

impl Argv {
    fn new(cmd: &[&str]) -> Self {
        Argv {
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn opt(&mut self, name: &str, value: Option<String>) -> &mut Self {
        if let Some(v) = value {
            self.opts.push(format!("--{name}={v}"));
        }
        self
    }

    fn flag(&mut self, name: &str, on: Option<bool>) -> &mut Self {
        if on == Some(true) {
            self.opts.push(format!("--{name}"));
        }
        self
    }

    fn pos(&mut self, value: Option<String>) -> &mut Self {
        self.pos.extend(value);
        self
    }

    fn done(&mut self) -> Vec<String> {
        let mut out = std::mem::take(&mut self.cmd);
        out.append(&mut self.opts);
        if !self.pos.is_empty() {
            out.push("--".into());
            out.append(&mut self.pos);
        }
        out
    }
}

fn s(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn b(description: &str) -> Value {
    json!({ "type": "boolean", "description": description })
}

const ATTEMPT: &str = "Attempt id, for example \"a3\".";
const TASK: &str = "Task id, for example \"t1\".";
const FROM: &str = "What to fork from: \"aN\" (latest state of an attempt) or \"sN\" (a snapshot). Defaults to the attempt of the server's directory.";
const ACTOR: &str =
    "Who works in the attempt, for example \"agent:claude-code\". Defaults to \"agent\".";

/// Default actor for attempts started through MCP. The CLI defaults `fork`
/// to `human`; a caller of this server is an agent.
const DEFAULT_ACTOR: &str = "agent";

const VERIFY: &str = "true: run the repository's pre-commit and commit-msg hooks in the attempt's view first (a failure returns `hook_failed` with the output and writes nothing); false: skip them. Defaults to `agentvc config export.verify`.";

fn verify_arg(argv: &mut Argv, verify: Option<bool>) {
    match verify {
        Some(true) => argv.opts.push("--verify".into()),
        Some(false) => argv.opts.push("--no-verify".into()),
        None => {}
    }
}

fn fork_params() -> Vec<(&'static str, Value, bool)> {
    vec![
        ("task", s(TASK), false),
        ("from", s(FROM), false),
        ("actor", s(ACTOR), false),
        (
            "model",
            s("Model the agent runs on, for example \"opus\"."),
            false,
        ),
        (
            "prompt",
            s("The prompt or instructions this attempt works from."),
            false,
        ),
        (
            "backend",
            json!({
                "type": "string",
                "enum": ["auto", "reflink", "overlay", "copy"],
                "description": "How the view is made. Defaults to auto. An overlay view in an unprivileged user namespace is visible only to agentvc processes (and what `run` starts); use copy or reflink for a view you edit directly.",
            }),
            false,
        ),
    ]
}

fn fork_opts(argv: &mut Argv, a: &Args) -> Result<(), BadArgs> {
    argv.opt("task", a.str("task")?)
        .opt("from", a.str("from")?)
        .opt(
            "actor",
            Some(a.str("actor")?.unwrap_or_else(|| DEFAULT_ACTOR.into())),
        )
        .opt("model", a.str("model")?)
        .opt("prompt", a.str("prompt")?)
        .opt("backend", a.str("backend")?);
    Ok(())
}

/// Every tool, in the order `tools/list` returns them.
pub static TOOLS: &[Tool] = &[
    Tool {
        name: "stack_log",
        title: "Stack",
        description: "This workspace's stack: its branches bottom to top with their commits, parent, PR (number, state, checks) and what needs a restack, plus which one is checked out here. Returns {base_ref, branches: [{name, parent, commits, current, needs_restack, ...}]}.",
        read_only: true,
        destructive: false,
        open_world: false,
        group: Group::Stack,
        params: || vec![("prs", b("Fetch each PR's checks and review from GitHub first."), false)],
        argv: |a| Ok(Argv::new(&["stack", "log"]).flag("prs", a.bool("prs")?).done()),
    },
    Tool {
        name: "stack_create",
        title: "New branch on the stack",
        description: "Commit your changes as a new branch on top of the current one: one branch is one PR of the stack. Stages every change first unless staged_only. Use it for each separate change instead of git checkout -b and git commit. Returns {branch: {name, parent}, commit}.",
        read_only: false,
        destructive: false,
        open_world: false,
        group: Group::Stack,
        params: || {
            vec![
                ("message", s("The commit message: what this branch does."), true),
                ("name", s("Branch name. Defaults to one made from the message."), false),
                ("insert", b("Put it between the current branch and the branches on top of it (they restack onto it)."), false),
                ("staged_only", b("Commit only what is staged."), false),
            ]
        },
        argv: |a| {
            let all = !a.bool("staged_only")?.unwrap_or(false);
            Ok(Argv::new(&["stack", "create"])
                .flag("all", Some(all))
                .opt("message", Some(a.req("message")?))
                .flag("insert", a.bool("insert")?)
                .pos(a.str("name")?)
                .done())
        },
    },
    Tool {
        name: "stack_modify",
        title: "Change the current branch",
        description: "Put your changes into the current branch (amend its last commit, or add a commit with new_commit) and restack the branches above it. Stages every change first unless staged_only. Use it for review fixes instead of git commit --amend. Returns {commit, restack: {updated, unchanged, conflict}}.",
        read_only: false,
        destructive: false,
        open_world: false,
        group: Group::Stack,
        params: || {
            vec![
                ("message", s("New commit message (or the new commit's, with new_commit)."), false),
                ("new_commit", b("Add a commit instead of amending the last one."), false),
                ("staged_only", b("Use only what is staged."), false),
            ]
        },
        argv: |a| {
            let all = !a.bool("staged_only")?.unwrap_or(false);
            Ok(Argv::new(&["stack", "modify"])
                .flag("all", Some(all))
                .flag("commit", a.bool("new_commit")?)
                .opt("message", a.str("message")?)
                .done())
        },
    },
    Tool {
        name: "stack_submit",
        title: "Push the stack and open PRs",
        description: "Push the stack and open or update one PR per branch, each based on the branch below it. Pushes with a lease, so other people's pushes are never overwritten. Defaults to the stack of the current branch. Returns each branch with its PR.",
        read_only: false,
        destructive: false,
        open_world: true,
        group: Group::Stack,
        params: || {
            vec![
                ("branch", s("Only this branch and the ones below it."), false),
                ("draft", b("Open new PRs as drafts."), false),
            ]
        },
        argv: |a| {
            Ok(Argv::new(&["stack", "submit"])
                .opt("branch", a.str("branch")?)
                .flag("draft", a.bool("draft")?)
                .done())
        },
    },
    Tool {
        name: "stack_sync",
        title: "Sync with the trunk",
        description: "Fetch the trunk, drop branches whose PRs merged and restack the rest onto it. On a conflict nothing changes and the error lists the paths. Returns {merged, deleted, restack}.",
        read_only: false,
        destructive: false,
        open_world: true,
        group: Group::Stack,
        params: Vec::new,
        argv: |_| Ok(Argv::new(&["stack", "sync"]).done()),
    },
    Tool {
        name: "stack_restack",
        title: "Restack",
        description: "Replay each branch onto its parent's current tip, after a branch below it changed. On a conflict nothing changes and the error lists the paths. Returns {updated, unchanged, conflict}.",
        read_only: false,
        destructive: false,
        open_world: false,
        group: Group::Stack,
        params: || vec![("branch", s("Only this branch and the ones above it."), false)],
        argv: |a| {
            Ok(Argv::new(&["stack", "restack"])
                .opt("branch", a.str("branch")?)
                .done())
        },
    },
    Tool {
        name: "ask_human",
        title: "Ask the person",
        description: "Only for agents started from the agentvc control center: ask the person one short question and wait for their answer (it may take a while). Use it instead of guessing when a decision is theirs. Returns {answer}.",
        read_only: true,
        destructive: false,
        open_world: true,
        group: Group::Agent,
        params: || vec![("question", s("One short question, with the options if there are any."), true)],
        argv: |a| Ok(Argv::new(&["agent", "ask"]).pos(Some(a.req("question")?)).done()),
    },
    Tool {
        name: "commit_review",
        title: "Commit a review",
        description: "Only for agents started from the agentvc control center: commit what you changed since your last review as one review (one PR) on a new branch stacked on the previous one, then keep working on top of it. Use it instead of git branches and commits when the person wants several reviews or a stack of PRs. The last review is committed for you when you stop. Returns {branch, commit, stack}.",
        read_only: false,
        destructive: false,
        open_world: false,
        group: Group::Agent,
        params: || {
            vec![
                ("branch", s("A short name for this review's branch, e.g. \"cli-missing-input\"."), true),
                ("message", s("The commit message: what this review does."), true),
            ]
        },
        argv: |a| {
            Ok(Argv::new(&["agent", "commit"])
                .opt("message", Some(a.req("message")?))
                .pos(Some(a.req("branch")?))
                .done())
        },
    },
    Tool {
        name: "approve",
        title: "Approve tool call",
        description: "Claude Code's permission prompt tool for agents started from the agentvc control center: asks the person to approve a tool call and waits. Returns {decision}.",
        read_only: true,
        destructive: false,
        open_world: true,
        group: Group::Agent,
        params: || {
            vec![
                ("tool_name", s("The tool that wants to run."), true),
                ("input", json!({ "type": "object", "description": "The tool's input." }), true),
                ("tool_use_id", s("The tool call's id."), false),
            ]
        },
        argv: |a| {
            let input = a.get("input").cloned().unwrap_or(Value::Null);
            Ok(Argv::new(&["agent", "permission"])
                .opt("tool", Some(a.req("tool_name")?))
                .opt("input", Some(input.to_string()))
                .done())
        },
    },
    Tool {
        name: "task_new",
        title: "New task",
        description: "Open a task: the goal that one or more attempts work on. Returns {task}.",
        read_only: false,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || {
            vec![
                ("goal", s("What the task should achieve."), true),
                ("criteria", s("Acceptance criteria."), false),
            ]
        },
        argv: |a| {
            Ok(Argv::new(&["task", "new"])
                .opt("criteria", a.str("criteria")?)
                .pos(Some(a.req("goal")?))
                .done())
        },
    },
    Tool {
        name: "task_list",
        title: "List tasks",
        description: "List all tasks. Returns {tasks}.",
        read_only: true,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: Vec::new,
        argv: |_| Ok(Argv::new(&["task", "list"]).done()),
    },
    Tool {
        name: "fork",
        title: "Fork attempt",
        description: "Start a new attempt in its own copy-on-write view, forked from an attempt or snapshot. Fork before trying a risky or alternative approach; work only inside the returned fork.attempt.view_path. Returns {fork}.",
        read_only: false,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: fork_params,
        argv: |a| {
            let mut argv = Argv::new(&["fork"]);
            fork_opts(&mut argv, a)?;
            Ok(argv.done())
        },
    },
    Tool {
        name: "snapshot",
        title: "Snapshot attempt",
        description: "Record the current state of an attempt's view. Attach results such as tests or cost as meta. Returns {snapshot, unchanged}.",
        read_only: false,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || {
            vec![
                ("attempt", s("Attempt id, for example \"a3\". Defaults to the attempt of the server's directory."), false),
                ("trigger", s("What caused the snapshot. Defaults to \"mcp\"."), false),
                (
                    "meta",
                    json!({
                        "type": "object",
                        "description": "Metadata to attach, for example {\"tests\": \"pass\"}.",
                        "additionalProperties": { "type": "string" },
                    }),
                    false,
                ),
            ]
        },
        argv: |a| {
            let mut argv = Argv::new(&["snapshot"]);
            argv.opt("trigger", Some(a.str("trigger")?.unwrap_or_else(|| "mcp".into())));
            match a.get("meta") {
                None => {}
                Some(Value::Object(m)) => {
                    for (k, v) in m {
                        let v = match v {
                            Value::String(s) => s.clone(),
                            Value::Number(_) | Value::Bool(_) => v.to_string(),
                            _ => return Err(BadArgs(format!("meta.{k} must be a string"))),
                        };
                        if k.is_empty() || k.contains('=') {
                            return Err(BadArgs(format!("bad meta key {k:?}")));
                        }
                        argv.opt("meta", Some(format!("{k}={v}")));
                    }
                }
                Some(_) => return Err(BadArgs("meta must be an object".into())),
            }
            Ok(argv.pos(a.str("attempt")?).done())
        },
    },
    Tool {
        name: "annotate",
        title: "Annotate attempt",
        description: "Record agent metadata on an attempt: model, cost in dollars, tokens, a note, the transcript's path or URL. Repeatable; cost and tokens add up. Returns {annotation, agent} where agent is the attempt's summed metadata.",
        read_only: false,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || {
            let n = |d: &str| json!({ "type": "number", "description": d });
            vec![
                ("attempt", s("Attempt id, for example \"a3\". Defaults to the attempt of the server's directory."), false),
                ("model", s("Model that did the work."), false),
                ("cost_usd", n("Cost in dollars, for example 0.42."), false),
                ("tokens_in", n("Input tokens."), false),
                ("tokens_out", n("Output tokens."), false),
                ("note", s("A note on the attempt."), false),
                ("transcript", s("Path or URL of the transcript."), false),
            ]
        },
        argv: |a| {
            let mut argv = Argv::new(&["annotate"]);
            argv.opt("model", a.str("model")?)
                .opt("note", a.str("note")?)
                .opt("transcript", a.str("transcript")?);
            for key in ["cost_usd", "tokens_in", "tokens_out"] {
                match a.get(key) {
                    None => {}
                    Some(Value::Number(n)) => {
                        argv.opt(&key.replace('_', "-"), Some(n.to_string()));
                    }
                    Some(_) => return Err(BadArgs(format!("{key} must be a number"))),
                }
            }
            Ok(argv.pos(a.str("attempt")?).done())
        },
    },
    Tool {
        name: "check",
        title: "Check attempt",
        description: "Run a test command in an attempt's view and record the result on its snapshot: pass/fail, duration, test counts and the end of the output. The same tree and command reuse an earlier result unless force is set. Marks an open attempt passed or failed. Returns {check, cached, status}.",
        read_only: false,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || {
            vec![
                ("command", s("Shell command, for example \"cargo test --workspace\"."), true),
                ("attempt", s("Attempt id, for example \"a3\". Defaults to the attempt of the server's directory."), false),
                ("name", s("What the result is called. Defaults to \"tests\"."), false),
                (
                    "junit",
                    json!({ "type": "array", "items": { "type": "string" },
                            "description": "JUnit XML files (globs, relative to the view) to count tests from." }),
                    false,
                ),
                ("force", b("Run even if this tree already has a result."), false),
            ]
        },
        argv: |a| {
            let command = a.req("command")?;
            let mut argv = Argv::new(&["check"]);
            argv.opt("name", a.str("name")?)
                .flag("force", a.bool("force")?);
            for j in a.strings("junit")? {
                argv.opt("junit", Some(j));
            }
            let mut out = argv.done();
            if let Some(at) = a.str("attempt")? {
                if at.starts_with('-') {
                    return Err(BadArgs(format!("bad attempt {at:?}")));
                }
                out.push(at);
            }
            out.extend(["--".to_string(), command]);
            Ok(out)
        },
    },
    Tool {
        name: "mark",
        title: "Mark attempt",
        description: "Mark an attempt passed, failed or exploring. It stays open. Returns {attempt}.",
        read_only: false,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || {
            vec![
                ("attempt", s(ATTEMPT), true),
                (
                    "status",
                    json!({ "type": "string", "enum": ["passed", "failed", "exploring"] }),
                    true,
                ),
            ]
        },
        argv: |a| {
            let status = a.req("status")?;
            if !["passed", "failed", "exploring"].contains(&status.as_str()) {
                return Err(BadArgs(format!(
                    "status must be passed, failed or exploring, got {status:?}"
                )));
            }
            Ok(Argv::new(&["mark"])
                .pos(Some(a.req("attempt")?))
                .pos(Some(status))
                .done())
        },
    },
    Tool {
        name: "abandon",
        title: "Abandon attempt",
        description: "Close an attempt, keeping it and the reason in history. Returns {attempt}.",
        read_only: false,
        destructive: true,
        open_world: false,
        group: Group::Attempt,
        params: || {
            vec![
                ("attempt", s(ATTEMPT), true),
                ("reason", s("Why the attempt is abandoned."), true),
            ]
        },
        argv: |a| {
            Ok(Argv::new(&["abandon"])
                .opt("reason", Some(a.req("reason")?))
                .pos(Some(a.req("attempt")?))
                .done())
        },
    },
    Tool {
        name: "promote",
        title: "Promote attempt",
        description: "Pick an attempt as the task's winner; its open siblings are abandoned as lost. Usually a human's call. Returns {attempt, closed_siblings}.",
        read_only: false,
        destructive: true,
        open_world: false,
        group: Group::Attempt,
        params: || vec![("attempt", s(ATTEMPT), true)],
        argv: |a| Ok(Argv::new(&["promote"]).pos(Some(a.req("attempt")?)).done()),
    },
    Tool {
        name: "op_log",
        title: "Operation log",
        description: "The space's operation log, newest first: every task creation, fork, status change, promote, rebase, export, run and undo, with its id, actor, a one-line summary, whether it can be undone (`undoable`) and the undo that reversed it (`undone_by`). Returns {operations}.",
        read_only: true,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || vec![("limit", s("How many operations, newest first, as a number string. Defaults to 20."), false)],
        argv: |a| Ok(Argv::new(&["op", "log"]).opt("limit", a.str("limit")?).done()),
    },
    Tool {
        name: "undo",
        title: "Undo operation",
        description: "Reverse an operation from op_log: a status change, a promote (its siblings and ancestors reopen), a task creation, or a fork (the attempt closes as `fork undone` and its view is put aside, recoverable). Without `op`, undoes the newest undoable operation that still stands. Undo is an operation too: undo the undo's id to redo. Fails with `undo_conflict` and changes nothing when later operations changed the same attempts. Returns {op, undid, changes}.",
        read_only: false,
        destructive: true,
        open_world: false,
        group: Group::Attempt,
        params: || {
            vec![
                ("op", s("Operation id from op_log, for example \"12\". Defaults to the newest undoable one."), false),
                ("actor", s(ACTOR), false),
            ]
        },
        argv: |a| {
            Ok(Argv::new(&["undo"])
                .opt(
                    "actor",
                    Some(a.str("actor")?.unwrap_or_else(|| DEFAULT_ACTOR.into())),
                )
                .pos(a.str("op")?)
                .done())
        },
    },
    Tool {
        name: "status",
        title: "Status",
        description: "Where am I and what is going on. With `attempt`, or when the server runs inside an attempt's view: the attempt, its task, head snapshot, files changed in the view since it (`changes`), active runs, PR URL and `upstream` ({base, upstream, behind}: behind > 0 means rebase before pr). Otherwise the space: open tasks with their attempts, `needs_review` (passed, not promoted) and active runs. Returns {status}.",
        read_only: true,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || vec![("attempt", s("Attempt id, for example \"a3\". Defaults to the server's view."), false)],
        argv: |a| Ok(Argv::new(&["status"]).pos(a.str("attempt")?).done()),
    },
    Tool {
        name: "log",
        title: "Attempt log",
        description: "List attempts (the attempt tree via parent_attempt), optionally for one task. Returns {attempts}.",
        read_only: true,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || vec![("task", s(TASK), false)],
        argv: |a| Ok(Argv::new(&["log"]).opt("task", a.str("task")?).done()),
    },
    Tool {
        name: "query",
        title: "Query attempts",
        description: "Find attempts by task, status, actor, model, files their own changes touched, creation time, snapshot metadata and text, all combined with AND. For example attempts on t3 that touched x.py and passed tests: {task: \"t3\", touched: [\"x.py\"], meta: [\"tests=pass\"]}. Returns {rows}: flat rows with the task goal, latest metadata and the matching touched files.",
        read_only: true,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || {
            let list = |d: &str| json!({ "type": "array", "items": { "type": "string" }, "description": d });
            vec![
                ("task", s(TASK), false),
                ("status", list("Any of: exploring, passed, failed, abandoned, promoted."), false),
                ("actor", s("Actor, exact or prefix: \"agent:claude\", or \"agent\" for every agent."), false),
                ("model", s("Model, exact or prefix."), false),
                ("touched", list("Paths, directories or globs; the attempt's own changes must touch one."), false),
                ("since", s("Created since: 30m, 2h, 3d, 1w, YYYY-MM-DD or Unix seconds."), false),
                ("meta", list("KEY=VALUE snapshot metadata the attempt must have, e.g. tests=pass."), false),
                ("grep", s("Text in the task goal, prompt, reason or metadata (any case)."), false),
            ]
        },
        argv: |a| {
            let mut argv = Argv::new(&["log"]);
            for (key, flag) in [("status", "status"), ("touched", "touched"), ("meta", "meta")] {
                for v in a.strings(key)? {
                    argv.opt(flag, Some(v));
                }
            }
            Ok(argv
                .flag("flat", Some(true))
                .opt("task", a.str("task")?)
                .opt("actor", a.str("actor")?)
                .opt("model", a.str("model")?)
                .opt("since", a.str("since")?)
                .opt("grep", a.str("grep")?)
                .done())
        },
    },
    Tool {
        name: "blame",
        title: "Blame lines to attempts",
        description: "For each line of a file in an attempt: the snapshot, attempt, actor, model and trigger that last changed it, across the attempts it was forked from; lines older than the attempt come from git blame. Includes the view's unsnapshotted work unless no_working. Returns {blame: {lines: [{line, text, origin}]}}.",
        read_only: true,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || {
            vec![
                ("path", s("File path relative to the view root."), true),
                ("attempt", s(ATTEMPT), true),
                ("no_working", b("Blame the head snapshot only."), false),
            ]
        },
        argv: |a| {
            Ok(Argv::new(&["blame"])
                .flag("no-working", a.bool("no_working")?)
                .pos(Some(a.req("path")?))
                .pos(Some(a.req("attempt")?))
                .done())
        },
    },
    Tool {
        name: "show",
        title: "Show attempt",
        description: "Show one attempt and its snapshots. Returns {attempt, snapshots}.",
        read_only: true,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || vec![("attempt", s(ATTEMPT), true)],
        argv: |a| Ok(Argv::new(&["show"]).pos(Some(a.req("attempt")?)).done()),
    },
    Tool {
        name: "diff",
        title: "Diff",
        description: "Compare two refs (aN or sN). With only `a`, diffs attempt a against the snapshot it was forked from (or the commit it was rebased onto). With neither, diffs the working state of the current view against that base, without taking a snapshot. With `patch`, each change carries `hunks` (unified diff lines) plus `added`/`removed` counts and `binary`; `stat` gives the counts without hunks. Returns {from, to, changes}.",
        read_only: true,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || {
            vec![
                ("a", s("First ref: aN or sN. Omit to diff the current view's working state."), false),
                ("b", s("Second ref: aN or sN."), false),
                ("patch", b("Include unified-diff hunks per file."), false),
                ("stat", b("Include added/removed line counts per file, without hunks."), false),
            ]
        },
        argv: |a| {
            let first = a.str("a")?;
            if first.is_none() && a.str("b")?.is_some() {
                return Err(BadArgs("`b` needs `a`".into()));
            }
            Ok(Argv::new(&["diff"])
                .flag("patch", a.bool("patch")?)
                .flag("stat", a.bool("stat")?)
                .pos(first)
                .pos(a.str("b")?)
                .done())
        },
    },
    Tool {
        name: "path",
        title: "Attempt path",
        description: "The view directory of an attempt. Returns {attempt, path}.",
        read_only: true,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || vec![("attempt", s(ATTEMPT), true)],
        argv: |a| Ok(Argv::new(&["path"]).pos(Some(a.req("attempt")?)).done()),
    },
    Tool {
        name: "run",
        title: "Run in new attempt",
        description: "Fork a new attempt and run a command (usually an agent) inside its view. The view is snapshotted when it exits; `check` decides passed or failed. Runs in the background by default (detach=true) and returns {attempt, run, pid}; follow it with `ps`. With detach=false it waits and returns {attempt, run, snapshot}.",
        read_only: false,
        destructive: false,
        open_world: true,
        group: Group::Attempt,
        params: || {
            let mut p = fork_params();
            p.push((
                "command",
                json!({
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "Program and arguments, for example [\"claude\", \"-p\", \"fix the parser\"].",
                }),
                true,
            ));
            p.push(("check", s("Shell command run in the view afterwards to judge the attempt, for example \"cargo test\"."), false));
            p.push(("detach", b("Return at once and run in the background. Defaults to true."), false));
            p
        },
        argv: |a| {
            let command = a.strings("command")?;
            if command.is_empty() {
                return Err(BadArgs("command must not be empty".into()));
            }
            let mut argv = Argv::new(&["run"]);
            fork_opts(&mut argv, a)?;
            argv.opt("check", a.str("check")?)
                .flag("detach", Some(a.bool("detach")?.unwrap_or(true)));
            // `run` takes the command after `--`, so it goes in as positionals.
            for c in command {
                argv.pos(Some(c));
            }
            Ok(argv.done())
        },
    },
    Tool {
        name: "ps",
        title: "List runs",
        description: "List runs that have not finished, or all runs. Returns {runs}.",
        read_only: true,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || vec![("all", b("Include finished runs."), false)],
        argv: |a| Ok(Argv::new(&["ps"]).flag("all", a.bool("all")?).done()),
    },
    Tool {
        name: "rebase",
        title: "Rebase attempt",
        description: "Move an attempt onto a newer commit of the host repo (default: the remote's default branch, fetched), so `pr` carries only its own changes. Three-way merges the attempt with the new commit, writes the result into its view and records a `rebase` snapshot. On conflicts nothing changes and the error has code `conflict` with `paths`; with keep_conflicts=true the files get conflict markers in the view and the attempt becomes exploring: fix them, then `snapshot`. Returns {rebase}.",
        read_only: false,
        destructive: false,
        open_world: true,
        group: Group::Attempt,
        params: || {
            vec![
                ("attempt", s(ATTEMPT), true),
                ("onto", s("Git revision to rebase onto, for example \"origin/main\". Defaults to the remote's default branch."), false),
                ("remote", s("Remote whose default branch is the default target. Defaults to \"origin\"."), false),
                ("no_fetch", b("Use the remote-tracking branch as it is, without fetching."), false),
                ("keep_conflicts", b("On conflicts, write conflict markers into the view instead of failing."), false),
            ]
        },
        argv: |a| {
            Ok(Argv::new(&["rebase"])
                .opt("onto", a.str("onto")?)
                .opt("remote", a.str("remote")?)
                .flag("no-fetch", a.bool("no_fetch")?)
                .flag("keep-conflicts", a.bool("keep_conflicts")?)
                .pos(Some(a.req("attempt")?))
                .done())
        },
    },
    Tool {
        name: "take",
        title: "Take from another attempt",
        description: "Combine attempts: take files, globs or single hunks of another attempt's changes (what `diff` with one attempt shows) into an attempt's view, three-way merged with its own work, and record a `take` snapshot whose metadata names the source. With no paths and no hunks, takes everything (a merge). The source is not changed, and the PR body credits it. On conflicts nothing changes and the error has code `conflict` with `paths`; with keep_conflicts=true the files get conflict markers and the attempt becomes exploring. Returns {take}.",
        read_only: false,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || {
            vec![
                ("source", s("Attempt to take from, as aN."), true),
                ("into", s("Attempt that receives the changes, as aN."), true),
                ("paths", json!({ "type": "array", "items": { "type": "string" }, "description": "Changed files to take whole: paths, directories or globs such as \"tests/**\"." }), false),
                ("hunks", json!({ "type": "array", "items": { "type": "string" }, "description": "Single hunks as \"FILE:N\" or \"FILE:N,M\", numbered from 1 as in the source's diff with patch=true." }), false),
                ("keep_conflicts", b("On conflicts, write conflict markers into the view instead of failing."), false),
            ]
        },
        argv: |a| {
            let mut argv = Argv::new(&["take"]);
            for p in a.strings("paths")? {
                argv.opt("paths", Some(p));
            }
            for h in a.strings("hunks")? {
                argv.opt("hunks", Some(h));
            }
            let source = a.req("source")?;
            let into = a.req("into")?;
            Ok(argv
                .opt("into", Some(into))
                .flag("keep-conflicts", a.bool("keep_conflicts")?)
                .pos(Some(source))
                .done())
        },
    },
    Tool {
        name: "pr",
        title: "Open pull request",
        description: "Export an attempt as a commit, push its branch and open (or update) a GitHub pull request that lists the rejected attempts. Returns {pr}.",
        read_only: false,
        destructive: false,
        open_world: true,
        group: Group::Attempt,
        params: || {
            vec![
                ("attempt", s(ATTEMPT), true),
                ("base", s("Branch the PR merges into. Defaults to the repository's default branch."), false),
                ("branch", s("Branch to push. Defaults to `agentvc config pr.branch`, $AGENTVC_PR_BRANCH, then agentvc/aN. If it is checked out in the host repo, the checkout moves along."), false),
                ("draft", b("Open as a draft."), false),
                ("title", s("PR title and commit subject. Defaults to the attempt's task goal; on updates a title edited on GitHub (or set by an earlier title) is kept unless this is passed."), false),
                ("remote", s("Git remote to push to. Defaults to \"origin\"."), false),
                ("repo", s("GitHub repository as owner/name. Defaults to the remote's."), false),
                ("force", b("Move the branch even if another attempt exported to it."), false),
                ("body_prefix", s("File whose text goes above the generated PR body (for example attribution lines). Kept on later updates."), false),
                ("body_suffix", s("File whose text ends the PR body, below the generated part (for example a \"Generated with\" footer). Replaces an earlier footer; kept on later updates."), false),
                ("trailers", json!({ "type": "array", "items": { "type": "string" }, "description": "Extra \"Key: value\" trailers for the exported commit." }), false),
                ("verify", b(VERIFY), false),
            ]
        },
        argv: |a| {
            let mut argv = Argv::new(&["pr"]);
            verify_arg(&mut argv, a.bool("verify")?);
            for t in a.strings("trailers")? {
                argv.opt("trailer", Some(t));
            }
            Ok(argv
                .opt("base", a.str("base")?)
                .opt("branch", a.str("branch")?)
                .flag("draft", a.bool("draft")?)
                .opt("title", a.str("title")?)
                .opt("remote", a.str("remote")?)
                .opt("repo", a.str("repo")?)
                .flag("force", a.bool("force")?)
                .opt("body-prefix", a.str("body_prefix")?)
                .opt("body-suffix", a.str("body_suffix")?)
                .pos(Some(a.req("attempt")?))
                .done())
        },
    },
    Tool {
        name: "comments",
        title: "PR review comments",
        description: "The review on an attempt's pull request, to address it: change requests and open comment threads with file and line, plus PR state and failing checks. Finds the PR through the attempts this one was forked from, so call it right after forking from a PR'd attempt. refresh=true fetches from GitHub first. Returns {pr: {state, checks, reviews, decision, threads, unresolved}}.",
        read_only: true,
        destructive: false,
        open_world: true,
        group: Group::Attempt,
        params: || {
            vec![
                ("attempt", s("Attempt id, for example \"a3\". Defaults to the server's view."), false),
                ("all", b("Also resolved and outdated threads."), false),
                ("refresh", b("Fetch from GitHub first."), false),
            ]
        },
        argv: |a| {
            Ok(Argv::new(&["comments"])
                .flag("all", a.bool("all")?)
                .flag("refresh", a.bool("refresh")?)
                .pos(a.str("attempt")?)
                .done())
        },
    },
    Tool {
        name: "review_comments",
        title: "Review comments sent to you",
        description: "The review comments on what you work on: your own agent (agN) and the branch checked out here, or another target (branch:WS/NAME, agent:agN, attempt:aN). Each thread has its id (cN), state (open, addressed, resolved), file, side and line range as they are now (current; null when outdated), the quoted lines it was written on, and its comments. Call it before you finish and address every open thread: fix the code, reply_comment to say what you did, resolve_comment to mark it done. Returns {threads}.",
        read_only: true,
        destructive: false,
        open_world: false,
        group: Group::Review,
        params: || {
            vec![
                ("target", s("Another target to list instead: branch:WS/NAME, agent:agN or attempt:aN."), false),
                ("all", b("Also resolved threads."), false),
            ]
        },
        argv: |a| {
            let all = a.bool("all")?.unwrap_or(false);
            let target = a.str("target")?;
            let mut argv = Argv::new(&["comment", "ls"]);
            if target.is_none() {
                argv.flag("here", Some(true));
            }
            // Agents never see a reviewer's unsent drafts.
            Ok(argv
                .flag("open", Some(!all))
                .flag("no-drafts", Some(all && target.is_some()))
                .pos(target)
                .done())
        },
    },
    Tool {
        name: "reply_comment",
        title: "Reply to a review comment",
        description: "Reply in a review thread (id cN from review_comments), as the agent working here: say what you changed, or ask what the reviewer meant. Returns {thread}.",
        read_only: false,
        destructive: false,
        open_world: false,
        group: Group::Review,
        params: || {
            vec![
                ("id", s("Thread id, for example \"c3\"."), true),
                ("text", s("The reply."), true),
            ]
        },
        argv: |a| {
            Ok(Argv::new(&["comment", "reply"])
                .flag("as-agent", Some(true))
                .pos(Some(a.req("id")?))
                .pos(Some(a.req("text")?))
                .done())
        },
    },
    Tool {
        name: "resolve_comment",
        title: "Mark a review comment done",
        description: "Mark a review thread (id cN) as addressed once you dealt with it; the person who wrote it resolves it after looking. Returns {thread}.",
        read_only: false,
        destructive: false,
        open_world: false,
        group: Group::Review,
        params: || vec![("id", s("Thread id, for example \"c3\"."), true)],
        argv: |a| {
            Ok(Argv::new(&["comment", "resolve"])
                .flag("as-agent", Some(true))
                .pos(Some(a.req("id")?))
                .done())
        },
    },
    Tool {
        name: "land",
        title: "Land attempt on a local branch",
        description: "Put an attempt on a local git branch without GitHub (then `git push` works with any remote): rebase it onto the branch when the branch moved (a conflict fails with `conflict` and changes nothing), then fast-forward the branch to one clean commit, or write a merge commit with merge=true. Defaults to the checked-out branch, whose working tree moves along; local edits the move would overwrite fail with `checkout_dirty`. Returns {land: {branch, commit, previous, rebased, checkout_updated, hooks}}.",
        read_only: false,
        destructive: false,
        open_world: false,
        group: Group::Attempt,
        params: || {
            vec![
                ("attempt", s(ATTEMPT), true),
                ("onto", s("Local branch to land on. Defaults to the checked-out branch."), false),
                ("merge", b("Write a merge commit instead of one commit on top."), false),
                ("message", s("Commit message. Defaults to the task goal."), false),
                ("verify", b(VERIFY), false),
            ]
        },
        argv: |a| {
            let mut argv = Argv::new(&["land"]);
            verify_arg(&mut argv, a.bool("verify")?);
            Ok(argv
                .opt("onto", a.str("onto")?)
                .flag("merge", a.bool("merge")?)
                .opt("message", a.str("message")?)
                .pos(Some(a.req("attempt")?))
                .done())
        },
    },
    Tool {
        name: "sync",
        title: "Sync space to remote",
        description: "Save the whole space (tasks, attempts including abandoned ones, snapshots, reasons, run logs) so the history outlives this machine: push it to a git remote as refs/agentvc/spaces/<id>, and write it into a comment on the space's latest PR (for machines that may push only one branch). `agentvc init --from-remote` or `--from-pr` restores it. `pr` saves on its own. Returns {sync: {git, pr}}.",
        read_only: false,
        destructive: false,
        open_world: true,
        group: Group::Attempt,
        params: || {
            vec![
                ("remote", s("Git remote. Defaults to \"origin\"."), false),
                ("auto", b("Also sync after every run, promote, abandon and pr from now on."), false),
            ]
        },
        argv: |a| {
            Ok(Argv::new(&["sync"])
                .opt("remote", a.str("remote")?)
                .flag("auto", a.bool("auto")?)
                .done())
        },
    },
];

/// Look up a tool by name.
pub fn find(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(tool: &str, args: Value) -> Result<Vec<String>, BadArgs> {
        find(tool).unwrap().build(&args)
    }

    #[test]
    fn stack_tools_by_default_attempt_tools_behind_a_flag() {
        let names = |p: Profile| p.tools().map(|t| t.name).collect::<Vec<_>>();
        let person = names(Profile::default());
        assert_eq!(
            person,
            [
                "stack_log",
                "stack_create",
                "stack_modify",
                "stack_submit",
                "stack_sync",
                "stack_restack",
                "review_comments",
                "reply_comment",
                "resolve_comment"
            ]
        );
        // A control-center agent: its own tools, review, and only reads the stack.
        let agent = names(Profile {
            agent: true,
            attempts: false,
        });
        assert_eq!(
            agent,
            [
                "stack_log",
                "ask_human",
                "commit_review",
                "approve",
                "review_comments",
                "reply_comment",
                "resolve_comment"
            ]
        );
        let all = names(Profile {
            agent: false,
            attempts: true,
        });
        assert!(all.contains(&"fork") && all.contains(&"stack_create"));
        assert!(!all.contains(&"ask_human"));
        assert_eq!(all.len(), TOOLS.len() - 3);
    }

    #[test]
    fn stack_argv() {
        assert_eq!(
            argv("stack_create", json!({ "message": "Add x", "name": "-x" })).unwrap(),
            ["stack", "create", "--all", "--message=Add x", "--", "-x"]
        );
        assert_eq!(
            argv(
                "stack_create",
                json!({ "message": "m", "staged_only": true, "insert": true })
            )
            .unwrap(),
            ["stack", "create", "--message=m", "--insert"]
        );
        assert_eq!(
            argv("stack_modify", json!({ "new_commit": true })).unwrap(),
            ["stack", "modify", "--all", "--commit"]
        );
        assert_eq!(
            argv("stack_submit", json!({ "branch": "b", "draft": true })).unwrap(),
            ["stack", "submit", "--branch=b", "--draft"]
        );
        assert_eq!(argv("stack_sync", json!({})).unwrap(), ["stack", "sync"]);
        assert!(
            argv("stack_create", json!({})).is_err(),
            "message is required"
        );
    }

    #[test]
    fn land_argv() {
        assert_eq!(
            argv(
                "land",
                json!({ "attempt": "a3", "merge": true, "verify": false })
            )
            .unwrap(),
            ["land", "--no-verify", "--merge", "--", "a3"]
        );
        assert_eq!(
            argv(
                "land",
                json!({ "attempt": "a3", "onto": "main", "verify": true })
            )
            .unwrap(),
            ["land", "--verify", "--onto=main", "--", "a3"]
        );
    }

    #[test]
    fn values_never_become_flags() {
        assert_eq!(
            argv("task_new", json!({ "goal": "--help", "criteria": "-x" })).unwrap(),
            ["task", "new", "--criteria=-x", "--", "--help"]
        );
        assert_eq!(
            argv("abandon", json!({ "attempt": "a3", "reason": "--force" })).unwrap(),
            ["abandon", "--reason=--force", "--", "a3"]
        );
    }

    #[test]
    fn fork_defaults_to_agent_actor() {
        assert_eq!(
            argv("fork", json!({ "task": "t1", "from": 1 })).unwrap(),
            ["fork", "--task=t1", "--from=1", "--actor=agent"]
        );
        assert_eq!(
            argv("fork", json!({ "backend": "copy" })).unwrap(),
            ["fork", "--actor=agent", "--backend=copy"]
        );
    }

    #[test]
    fn snapshot_meta_and_default_trigger() {
        assert_eq!(
            argv(
                "snapshot",
                json!({ "attempt": "a2", "meta": { "tests": "pass", "cost": 0.5 } })
            )
            .unwrap(),
            [
                "snapshot",
                "--trigger=mcp",
                "--meta=cost=0.5",
                "--meta=tests=pass",
                "--",
                "a2"
            ]
        );
        assert_eq!(
            argv("snapshot", json!({})).unwrap(),
            ["snapshot", "--trigger=mcp"]
        );
    }

    #[test]
    fn annotate_and_check_argv() {
        assert_eq!(
            argv(
                "annotate",
                json!({ "attempt": "a2", "cost_usd": 0.42, "tokens_in": 10, "note": "n" })
            )
            .unwrap(),
            [
                "annotate",
                "--note=n",
                "--cost-usd=0.42",
                "--tokens-in=10",
                "--",
                "a2"
            ]
        );
        assert_eq!(
            argv("check", json!({ "attempt": "a2", "command": "cargo test" })).unwrap(),
            ["check", "a2", "--", "cargo test"]
        );
        assert!(argv("check", json!({ "attempt": "-x", "command": "true" })).is_err());
    }

    #[test]
    fn run_detaches_by_default() {
        assert_eq!(
            argv(
                "run",
                json!({ "task": "t1", "command": ["sh", "-c", "true"], "check": "true" })
            )
            .unwrap(),
            [
                "run",
                "--task=t1",
                "--actor=agent",
                "--check=true",
                "--detach",
                "--",
                "sh",
                "-c",
                "true"
            ]
        );
        let attached = argv("run", json!({ "command": ["true"], "detach": false })).unwrap();
        assert!(!attached.contains(&"--detach".to_string()));
    }

    #[test]
    fn rejects_bad_arguments() {
        assert!(argv("show", json!({})).is_err());
        assert!(argv("show", json!({ "attempt": "a1", "extra": 1 })).is_err());
        assert!(argv("mark", json!({ "attempt": "a1", "status": "won" })).is_err());
        assert!(argv("run", json!({ "command": [] })).is_err());
        assert!(argv("ps", json!({ "all": "yes" })).is_err());
        assert!(argv("log", json!([1])).is_err());
    }

    #[test]
    fn every_tool_has_an_object_schema() {
        for t in TOOLS {
            let d = t.describe();
            assert_eq!(d["inputSchema"]["type"], "object", "{}", t.name);
            for r in d["inputSchema"]["required"].as_array().unwrap() {
                assert!(d["inputSchema"]["properties"]
                    .get(r.as_str().unwrap())
                    .is_some());
            }
        }
    }
}
