//! `agentvc agent`: start coding agents on a workspace's stack and drive
//! them. See `agentvc_core::agent`.

use std::path::{Path, PathBuf};

use agentvc_core::agent::{self, Agent, AgentState, Mode, StartRequest};
use clap::Subcommand;
use serde_json::{json, Value};

#[derive(Subcommand)]
pub enum AgentCmd {
    /// Start an agent on top of a branch of a workspace's stack. It works in
    /// its own worktree and branch, so the workspace's checkout is never
    /// touched; `land` puts its work on the stack after review.
    Start {
        /// Workspace id, name or directory. Defaults to this one.
        #[arg(long)]
        workspace: Option<String>,
        /// Branch to start from (or the trunk). Defaults to the workspace's
        /// checked-out branch.
        #[arg(long)]
        from: Option<String>,
        /// An older commit of --from to start at.
        #[arg(long)]
        at: Option<String>,
        /// Land as a new version of --from instead of a new branch on top.
        #[arg(long)]
        revise: bool,
        #[arg(long)]
        model: Option<String>,
        /// Check command run when the agent stops. Defaults to
        /// `git config agentvc.agent-check`.
        #[arg(long)]
        check: Option<String>,
        /// Run this shell command instead of Claude Code (the prompt is in
        /// $AGENTVC_AGENT_PROMPT).
        #[arg(long)]
        command: Option<String>,
        /// What the agent should do.
        #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
        prompt: Vec<String>,
    },
    /// List agents, newest first: what each works on, its state, and what
    /// needs you.
    Ls {
        /// Include landed and discarded agents.
        #[arg(long)]
        all: bool,
    },
    /// One agent with its conversation and recent steps.
    Show { agent: String },
    /// Reply to an agent: answers its question, or starts another turn
    /// after it stopped.
    Reply {
        agent: String,
        #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// Approve the command an agent is waiting on.
    Approve { agent: String },
    /// Deny the command an agent is waiting on.
    Deny {
        agent: String,
        /// Why, passed to the agent.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Stop an agent; what it did so far is kept.
    Stop { agent: String },
    /// Put the agent's work on its workspace's stack.
    Land { agent: String },
    /// Throw the agent's work away (its worktree and branch).
    Discard { agent: String },
    /// Inside an agent: ask the person one question and wait for the answer
    /// (the MCP tool `ask_human`).
    Ask {
        #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
        question: Vec<String>,
    },
    /// Inside an agent: ask the person to approve a tool call and wait (the
    /// MCP permission prompt tool `approve`).
    #[command(hide = true)]
    Permission {
        #[arg(long)]
        tool: String,
        /// The tool's input, as JSON.
        #[arg(long)]
        input: String,
    },
}

pub fn state_word(s: AgentState) -> &'static str {
    match s {
        AgentState::Queued => "waiting for a slot",
        AgentState::Running => "working",
        AgentState::Asking => "has a question",
        AgentState::NeedsApproval => "needs your OK",
        AgentState::Ready => "ready to review",
        AgentState::TestsFailing => "tests failing",
        AgentState::Failed => "failed",
        AgentState::Stopped => "stopped",
        AgentState::Landed => "landed",
        AgentState::Discarded => "discarded",
    }
}

fn line(a: &Agent) -> String {
    let mut t = format!(
        "{:<5} {:<16} {}  [{} > {}{}]",
        a.id,
        state_word(a.state),
        a.title,
        a.workspace_name,
        a.start.branch,
        match a.mode {
            Mode::New => "",
            Mode::Revise => ", revise",
        }
    );
    if let Some(q) = &a.question {
        t.push_str(&format!("\n      ? {}", q.text));
    } else if let Some(s) = &a.last_step {
        t.push_str(&format!("\n      {s}"));
    }
    if let Some(e) = &a.error {
        t.push_str(&format!("\n      error: {e}"));
    }
    t.push('\n');
    t
}

fn exe() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_exe()?)
}

pub fn run(cwd: &Path, cmd: AgentCmd) -> anyhow::Result<(Value, String)> {
    Ok(match cmd {
        AgentCmd::Start {
            workspace,
            from,
            at,
            revise,
            model,
            check,
            command,
            prompt,
        } => {
            let a = agent::start(
                cwd,
                &StartRequest {
                    workspace,
                    from,
                    at,
                    prompt: prompt.join(" "),
                    mode: if revise { Mode::Revise } else { Mode::New },
                    model,
                    check,
                    command,
                    exe: exe()?,
                },
            )?;
            let text = format!(
                "{} started on {} > {} (branch {}, worktree {})",
                a.id,
                a.workspace_name,
                a.start.branch,
                a.branch,
                a.worktree.display()
            );
            (json!({ "agent": a }), text)
        }
        AgentCmd::Ls { all } => {
            let l = agent::list(cwd, all)?;
            let mut text = format!(
                "{} need you · {} of {} working on {}\n",
                l.needs_you, l.working, l.limit, l.machine
            );
            for a in &l.agents {
                text.push_str(&line(a));
            }
            (json!(l), text)
        }
        AgentCmd::Show { agent: id } => {
            let d = agent::show(cwd, &id)?;
            let mut text = line(&d.agent);
            for m in &d.agent.messages {
                text.push_str(&format!("  {}: {}\n", m.from, m.text));
            }
            if let Some(r) = &d.agent.result {
                text.push_str(&format!(
                    "  changes: {} files, +{} -{}\n",
                    r.files, r.insertions, r.deletions
                ));
            }
            if let Some(t) = &d.agent.tests {
                text.push_str(&format!(
                    "  checks: {} ({})\n",
                    if t.passed { "passed" } else { "failed" },
                    t.command
                ));
            }
            (json!(d), text)
        }
        AgentCmd::Reply { agent: id, text } => {
            let a = agent::reply(cwd, &id, &text.join(" "), &exe()?)?;
            let t = format!("{}: {}", a.id, state_word(a.state));
            (json!({ "agent": a }), t)
        }
        AgentCmd::Approve { agent: id } => {
            let a = agent::answer(cwd, &id, None, Some(true))?;
            (json!({ "agent": a }), format!("{id}: approved"))
        }
        AgentCmd::Deny { agent: id, reason } => {
            let a = agent::answer(cwd, &id, reason.as_deref(), Some(false))?;
            (json!({ "agent": a }), format!("{id}: denied"))
        }
        AgentCmd::Stop { agent: id } => {
            let a = agent::stop(cwd, &id)?;
            (json!({ "agent": a }), format!("{id}: stopping"))
        }
        AgentCmd::Land { agent: id } => {
            let (a, out) = agent::land(cwd, &id)?;
            let on = a
                .landed
                .as_ref()
                .map(|l| l.branch.clone())
                .unwrap_or_default();
            let text = match out.reason.as_deref() {
                Some(why) => format!("{id}: queued to land on {on} ({why})"),
                None => format!("{id}: landed on {on} in {}", a.workspace_name),
            };
            (json!({ "agent": a, "edit": out }), text)
        }
        AgentCmd::Discard { agent: id } => {
            let a = agent::discard(cwd, &id)?;
            (json!({ "agent": a }), format!("{id}: discarded"))
        }
        AgentCmd::Ask { question } => {
            let answer = agent::ask(&question.join(" "))?;
            (json!({ "answer": answer }), answer)
        }
        AgentCmd::Permission { tool, input } => {
            let input: Value = serde_json::from_str(&input)
                .map_err(|e| anyhow::anyhow!("--input is not JSON: {e}"))?;
            let decision = agent::approve(&tool, &input)?;
            (json!({ "decision": decision }), decision.to_string())
        }
    })
}
