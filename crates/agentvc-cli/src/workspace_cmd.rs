//! `agentvc workspace`: every worktree of the repo with its stack, and
//! edits to any of them. See `agentvc_core::workspace`.

use std::path::Path;

use agentvc_core::checkouts;
use agentvc_core::stack::Window;
use agentvc_core::workspace::{self, Activity, Kind, RequestStatus, WorkspaceInfo};
use agentvc_core::Space;
use clap::{Subcommand, ValueEnum};
use serde_json::{json, Value};

#[derive(Subcommand)]
pub enum WorkspaceCmd {
    /// List every workspace of this repo (each worktree) with its stack,
    /// activity and pending edits.
    Ls {
        /// Also fetch and list workspaces published from other machines
        /// (read-only).
        #[arg(long)]
        remote: bool,
    },
    /// Name this workspace (names are unique in the repo and work wherever
    /// a workspace id does).
    Name { name: String },
    /// Say whether an agent is working here. Edits from other workspaces
    /// wait while one is; going idle applies them. The agent hooks do this
    /// for Claude Code and Cursor.
    State {
        state: StateArg,
        /// Who is working, e.g. agent:claude.
        #[arg(long)]
        by: Option<String>,
    },
    /// Edits asked of this repo's workspaces, queued and recent.
    Requests {
        /// Only this workspace's (id, name or directory).
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Cancel a queued edit.
    Cancel { request: String },
    /// Apply the edits queued for this workspace now.
    Drain,
    /// Push this workspace's record and stack as refs/agentvc/workspaces/<id>
    /// so other machines list it (`ls --remote`).
    Publish,
    /// Show or set the space this checkout belongs to. Checkouts (clones)
    /// of one space on this machine list and edit each other's workspaces.
    /// By default the space is the remote's URL, so clones of one repo join
    /// it on their own.
    Space {
        /// The space's name; `none` joins no space.
        name: Option<String>,
        /// Go back to the default (the remote's URL).
        #[arg(long, conflicts_with = "name")]
        reset: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum StateArg {
    Idle,
    AgentRunning,
}

fn short(s: &str) -> &str {
    &s[..10.min(s.len())]
}

fn ws_text(w: &WorkspaceInfo) -> String {
    let mut t = format!(
        "{}{} {} ({}){}  {}\n",
        if w.here { "* " } else { "  " },
        w.name,
        w.id,
        match w.kind {
            Kind::Main => "main worktree".to_string(),
            Kind::Worktree => "worktree".to_string(),
            Kind::Remote => format!("on {}", w.machine),
        },
        match w.state {
            Activity::AgentRunning => format!(
                ", agent running{}",
                w.busy
                    .as_ref()
                    .and_then(|b| b.by.as_ref())
                    .map(|b| format!(" ({b})"))
                    .unwrap_or_default()
            ),
            Activity::Idle => String::new(),
        },
        w.path.display(),
    );
    if w.branches.is_empty() {
        t += "    (no stack)\n";
    }
    for b in &w.branches {
        let mut flags = Vec::new();
        if b.commits != 1 {
            flags.push(format!("{} commits", b.commits));
        }
        if b.needs_restack {
            flags.push("needs restack".into());
        }
        if b.behind > 0 {
            flags.push(format!("{} behind {}", b.behind, w.base_ref));
        }
        if b.conflicts > 0 {
            flags.push(format!("{} conflicted files", b.conflicts));
        }
        if let Some(pr) = &b.pr {
            flags.push(match &pr.state {
                Some(s) => format!("#{} {s}", pr.number),
                None => format!("#{}", pr.number),
            });
        }
        if let (Some(a), Some(s)) = (b.attempt, &b.status) {
            flags.push(format!("a{a} {s}"));
        }
        t += &format!(
            "    {}{} {}{}{}\n",
            "  ".repeat(b.depth),
            if b.current { "◉" } else { "○" },
            b.name,
            b.tip
                .as_deref()
                .map(|t| format!(" {}", short(t)))
                .unwrap_or_default(),
            if flags.is_empty() {
                String::new()
            } else {
                format!("  ({})", flags.join(", "))
            }
        );
    }
    let queued: Vec<_> = w
        .requests
        .iter()
        .filter(|r| r.status == RequestStatus::Queued)
        .collect();
    for r in queued {
        t += &format!(
            "    queued {} {}{}\n",
            r.id,
            serde_json::to_value(&r.op)
                .ok()
                .and_then(|v| v["op"].as_str().map(str::to_string))
                .unwrap_or_default(),
            r.op.branch().map(|b| format!(" {b}")).unwrap_or_default()
        );
    }
    t
}

/// `agentvc workspace space`: the checkout's space and the checkouts in it.
fn space_cmd(cwd: &Path, name: Option<String>, reset: bool) -> anyhow::Result<(Value, String)> {
    let w = Window::open(cwd)?;
    if reset {
        checkouts::set_space(&w, None)?;
    } else if let Some(n) = &name {
        checkouts::set_space(&w, Some(n))?;
    }
    let (space, source) = checkouts::space_of(&w);
    let (_, siblings) = checkouts::space_and_siblings(&w);
    let me = checkouts::main_path(&w);
    let text = match &space {
        Some(s) => {
            let mut t = format!(
                "space {s} ({})\n  * {}\n",
                match source {
                    checkouts::Source::Config => "set here",
                    _ => "from the remote's URL",
                },
                me.display()
            );
            for p in &siblings {
                t += &format!("    {}\n", p.display());
            }
            t
        }
        None => "this checkout joins no space; `agentvc workspace space NAME` joins one\n".into(),
    };
    Ok((
        json!({ "space": space, "source": source, "checkout": me, "checkouts": siblings }),
        text,
    ))
}

pub fn run(cwd: &Path, cmd: WorkspaceCmd) -> anyhow::Result<(Value, String)> {
    Ok(match cmd {
        WorkspaceCmd::Ls { remote } => {
            let space = Space::discover(cwd).ok();
            let l = workspace::list(cwd, space.as_ref(), remote)?;
            let mut text = String::new();
            let mut last: Option<&Path> = None;
            let several = l.workspaces.iter().any(|w| w.other_checkout);
            for w in &l.workspaces {
                if several && w.kind != Kind::Remote && last != Some(w.checkout.as_path()) {
                    text += &format!("checkout {}\n", w.checkout.display());
                    last = Some(w.checkout.as_path());
                }
                text += &ws_text(w);
            }
            (json!(l), text)
        }
        WorkspaceCmd::Space { name, reset } => space_cmd(cwd, name, reset)?,
        WorkspaceCmd::Name { name } => {
            let r = workspace::set_name(cwd, &name)?;
            let text = format!("this workspace is {} ({})", r.name, r.id);
            (json!({ "workspace": r }), text)
        }
        WorkspaceCmd::State { state, by } => {
            let a = match state {
                StateArg::Idle => Activity::Idle,
                StateArg::AgentRunning => Activity::AgentRunning,
            };
            let applied = workspace::set_activity(cwd, a, by.as_deref())?;
            let text = match a {
                Activity::Idle if applied.is_empty() => "idle".to_string(),
                Activity::Idle => format!("idle; applied {} queued edits", applied.len()),
                Activity::AgentRunning => {
                    "agent running: edits from other workspaces wait".to_string()
                }
            };
            (json!({ "state": a, "applied": applied }), text)
        }
        WorkspaceCmd::Requests { workspace: target } => {
            let w = Window::open(cwd)?;
            let mut out = Vec::new();
            let locals = match &target {
                Some(t) => vec![workspace::find_local(&w, t)?],
                None => workspace::locals(&w)?,
            };
            for l in locals {
                out.extend(workspace::requests(&l.git_dir));
            }
            out.sort_by_key(|r| r.created_at);
            let text = out
                .iter()
                .map(|r| {
                    format!(
                        "{} {} {:?} {}{}\n",
                        r.id,
                        r.workspace,
                        r.status,
                        serde_json::to_value(&r.op)
                            .ok()
                            .and_then(|v| v["op"].as_str().map(str::to_string))
                            .unwrap_or_default(),
                        r.reason
                            .as_deref()
                            .map(|x| format!(": {x}"))
                            .unwrap_or_default()
                    )
                    .to_lowercase()
                })
                .collect::<String>();
            (json!({ "requests": out }), text)
        }
        WorkspaceCmd::Cancel { request } => {
            let r = workspace::cancel(cwd, &request)?;
            (json!({ "request": r }), format!("cancelled {request}"))
        }
        WorkspaceCmd::Drain => {
            let done = workspace::drain(cwd)?;
            let text = format!("applied {} queued edits", done.len());
            (json!({ "applied": done }), text)
        }
        WorkspaceCmd::Publish => {
            let p = workspace::publish(cwd)?;
            let text = format!("published {} as {} on {}", p.workspace, p.r#ref, p.remote);
            (json!(p), text)
        }
    })
}
