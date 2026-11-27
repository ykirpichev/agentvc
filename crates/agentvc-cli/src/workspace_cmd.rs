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
    /// Show or set how agents started in a workspace run: runner, model,
    /// check command, permissions and how many may work at once. With no
    /// setting flags it shows them. Agents started there inherit these.
    Agents {
        /// Workspace id, name or directory. Defaults to this one.
        #[arg(long)]
        workspace: Option<String>,
        /// claude (Claude Code), codex, cursor or command.
        #[arg(long)]
        runner: Option<String>,
        /// Shell command for the `command` runner (prompt in $AGENTVC_AGENT_PROMPT).
        #[arg(long)]
        command: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// Check command run when an agent stops.
        #[arg(long)]
        check: Option<String>,
        /// Claude Code: edits (default: edit files, ask before commands),
        /// ask (ask before edits too) or auto (never ask).
        #[arg(long)]
        permissions: Option<String>,
        /// Agents working at once in this workspace.
        #[arg(long)]
        max: Option<usize>,
        /// Clear every setting first (then apply any given here).
        #[arg(long)]
        clear: bool,
    },
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
    /// Make a new workspace in this space: a worktree of this checkout, or
    /// with --clone a fresh clone (own folder, own build) that joins the
    /// same space. Says which space it joined.
    New {
        /// Folder for it; must not exist or be empty.
        folder: std::path::PathBuf,
        /// Its name (unique in the space). Defaults to the folder's name.
        #[arg(long)]
        name: Option<String>,
        /// Clone the remote into the folder instead of adding a worktree.
        #[arg(long)]
        clone: bool,
    },
    /// Same as `agentvc space join`: show or set the space this checkout
    /// belongs to.
    #[command(hide = true)]
    Space {
        /// The space's name; `none` joins no space.
        name: Option<String>,
        /// Go back to the default (the remote's URL).
        #[arg(long, conflicts_with = "name")]
        reset: bool,
    },
}

/// `agentvc space`: the project that humans and agents work on together,
/// and its workspaces.
#[derive(Subcommand)]
pub enum SpaceCmd {
    /// This checkout's space and every workspace in it, by machine and
    /// folder (the default).
    Show,
    /// Every space with a checkout on this machine.
    Ls,
    /// Show or set the space's name (what people call it; its id stays).
    Name {
        name: Option<String>,
        /// Back to the default name (the last part of its key).
        #[arg(long, conflicts_with = "name")]
        reset: bool,
    },
    /// Show or set which space this checkout belongs to. Checkouts (clones)
    /// of one space list and edit each other's workspaces. By default it is
    /// the remote's URL, so clones of one repo join it on their own.
    Join {
        /// The space's key; `none` joins no space.
        key: Option<String>,
        /// Go back to the default (the remote's URL).
        #[arg(long, conflicts_with = "key")]
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

/// `~/src/mono-2 on mac`: where a workspace lives.
fn where_text(w: &WorkspaceInfo) -> String {
    format!("{} on {}", w.folder, w.machine)
}

fn space_line(s: &checkouts::SpaceRef) -> String {
    format!("space {} ({}, {})", s.name, s.id, s.key)
}

fn ws_text(w: &WorkspaceInfo) -> String {
    let mut t = format!(
        "{}{} {} ({}){}  {}\n",
        if w.here { "* " } else { "  " },
        w.name,
        w.id,
        match w.kind {
            Kind::Main => "main worktree",
            Kind::Worktree => "worktree",
            Kind::Remote => "published",
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
        where_text(w),
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

/// `agentvc space join`: the checkout's space and the checkouts in it.
fn join_cmd(cwd: &Path, name: Option<String>, reset: bool) -> anyhow::Result<(Value, String)> {
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
                "space key {s} ({})\n  * {}\n",
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
        None => "this checkout joins no space; `agentvc space join KEY` joins one\n".into(),
    };
    Ok((
        json!({
            "space": space,
            "space_info": checkouts::describe(&w),
            "source": source,
            "checkout": me,
            "checkouts": siblings,
        }),
        text,
    ))
}

pub fn run(cwd: &Path, cmd: WorkspaceCmd) -> anyhow::Result<(Value, String)> {
    Ok(match cmd {
        WorkspaceCmd::Ls { remote } => {
            let space = Space::discover(cwd).ok();
            let l = workspace::list(cwd, space.as_ref(), remote)?;
            let mut text = match &l.space_info {
                Some(s) => format!("{}\n", space_line(s)),
                None => String::new(),
            };
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
        WorkspaceCmd::Space { name, reset } => join_cmd(cwd, name, reset)?,
        WorkspaceCmd::New {
            folder,
            name,
            clone,
        } => {
            let kind = if clone {
                workspace::NewKind::Clone
            } else {
                workspace::NewKind::Worktree
            };
            let c = workspace::create(cwd, &folder, name.as_deref(), kind)?;
            let text = format!(
                "workspace {} ({}) at {} on {}\n{}\n",
                c.workspace.name,
                c.workspace.id,
                c.folder,
                c.workspace.machine,
                match &c.space {
                    Some(s) => format!("joined {}", space_line(s)),
                    None =>
                        "joined no space (no remote); `agentvc space join KEY` joins one".into(),
                }
            );
            (json!(c), text)
        }
        WorkspaceCmd::Name { name } => {
            let r = workspace::set_name(cwd, &name)?;
            let text = format!("this workspace is {} ({})", r.name, r.id);
            (json!({ "workspace": r }), text)
        }
        WorkspaceCmd::Agents {
            workspace,
            runner,
            command,
            model,
            check,
            permissions,
            max,
            clear,
        } => {
            use agentvc_core::agent::{self, AgentSettings};
            let given = AgentSettings {
                runner,
                command,
                model,
                check,
                permissions,
                max,
            };
            let r = if clear || !given.is_empty() {
                let base = if clear {
                    AgentSettings::default()
                } else {
                    agent::settings(cwd, workspace.as_deref())?.settings
                };
                agent::set_settings(cwd, workspace.as_deref(), given.over(&base))?
            } else {
                agent::settings(cwd, workspace.as_deref())?
            };
            let eff = r.settings.over(&r.defaults);
            let or = |v: &Option<String>, d: &str| v.clone().unwrap_or_else(|| d.to_string());
            let mut text = format!(
                "agents in {}:\n  runner       {}\n",
                r.workspace_name,
                or(&eff.runner, "claude")
            );
            if let Some(c) = &eff.command {
                text.push_str(&format!("  command      {c}\n"));
            }
            text.push_str(&format!(
                "  model        {}\n  check        {}\n  permissions  {}\n  at once      {} (machine cap {})",
                or(&eff.model, "default"),
                or(&eff.check, "none"),
                or(&eff.permissions, "edits"),
                eff.max.map(|m| m.to_string()).unwrap_or_else(|| "no limit".into()),
                r.machine_limit
            ));
            (json!(r), text)
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

/// `agentvc space ...`.
pub fn run_space(cwd: &Path, cmd: Option<SpaceCmd>) -> anyhow::Result<(Value, String)> {
    Ok(match cmd.unwrap_or(SpaceCmd::Show) {
        SpaceCmd::Show => show_space(cwd)?,
        SpaceCmd::Ls => {
            let here = Window::open(cwd).ok().and_then(|w| {
                checkouts::register(&w);
                checkouts::describe(&w)
            });
            let all = checkouts::known_spaces();
            let mut text = String::new();
            for k in &all {
                let mine = here.as_ref().map(|h| h.id == k.space.id).unwrap_or(false);
                text += &format!(
                    "{}{} {}  {}\n",
                    if mine { "* " } else { "  " },
                    k.space.name,
                    k.space.id,
                    k.space.key
                );
                for c in &k.checkouts {
                    text += &format!("    {}\n", workspace::tilde(c));
                }
            }
            if all.is_empty() {
                text = "no spaces on this machine yet\n".into();
            }
            (
                json!({ "here": here.map(|h| h.id), "machine": workspace::machine(), "spaces": all }),
                text,
            )
        }
        SpaceCmd::Name { name, reset } => {
            let w = Window::open(cwd)?;
            let s = if reset || name.is_some() {
                checkouts::set_name(&w, name.as_deref())?
            } else {
                checkouts::describe(&w).ok_or_else(|| agentvc_core::Error::Stack {
                    code: "no_space",
                    message: "this checkout joins no space; `agentvc space join KEY` joins one"
                        .into(),
                    details: None,
                })?
            };
            (json!({ "space": s }), format!("{}\n", space_line(&s)))
        }
        SpaceCmd::Join { key, reset } => join_cmd(cwd, key, reset)?,
    })
}

/// `agentvc space show`: the space and its workspaces by machine and folder.
fn show_space(cwd: &Path) -> anyhow::Result<(Value, String)> {
    let space = Space::discover(cwd).ok();
    let l = workspace::list(cwd, space.as_ref(), false)?;
    let mut text = match &l.space_info {
        Some(s) => format!("{}\n", space_line(s)),
        None => "this checkout joins no space; its workspaces:\n".into(),
    };
    // machine -> checkout -> workspaces, in list order.
    let mut groups: Vec<(String, std::path::PathBuf, Vec<&WorkspaceInfo>)> = Vec::new();
    for w in &l.workspaces {
        match groups
            .iter_mut()
            .find(|(m, c, _)| *m == w.machine && *c == w.checkout)
        {
            Some(g) => g.2.push(w),
            None => groups.push((w.machine.clone(), w.checkout.clone(), vec![w])),
        }
    }
    let mut last_machine = None;
    let mut checkouts_json = Vec::new();
    for (m, c, ws) in &groups {
        if last_machine != Some(m) {
            text += &format!(
                "  machine {m}{}\n",
                if *m == l.machine {
                    " (this machine)"
                } else {
                    ""
                }
            );
            last_machine = Some(m);
        }
        let this = ws.first().map(|w| w.this_machine).unwrap_or(false);
        text += &format!(
            "    checkout {}\n",
            if this {
                workspace::tilde(c)
            } else {
                c.display().to_string()
            }
        );
        for w in ws {
            text += &format!(
                "      {}{} {}  {}{}{}\n",
                if w.here { "* " } else { "  " },
                w.name,
                w.id,
                w.folder,
                if w.user.is_empty() {
                    String::new()
                } else {
                    format!("  ({})", w.user)
                },
                match w.state {
                    Activity::AgentRunning => "  agent running",
                    Activity::Idle => "",
                }
            );
        }
        checkouts_json.push(json!({
            "machine": m,
            "checkout": c,
            "workspaces": ws.iter().map(|w| json!({
                "id": w.id,
                "name": w.name,
                "kind": w.kind,
                "path": w.path,
                "folder": w.folder,
                "user": w.user,
                "state": w.state,
                "here": w.here,
            })).collect::<Vec<_>>(),
        }));
    }
    Ok((
        json!({
            "space": l.space_info,
            "machine": l.machine,
            "here": l.here,
            "checkouts": checkouts_json,
        }),
        text,
    ))
}
