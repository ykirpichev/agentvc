//! `agentvc comment`: review comments on a branch, an agent or a try,
//! pinned to lines of a file (#166). See `agentvc_core::comments`.

use std::path::Path;

use agentvc_core::comments::{self, AddRequest, Deliver, Filter, LineRange, Side, Target, Thread};
use agentvc_core::stack::Window;
use clap::Subcommand;
use serde_json::{json, Value};

#[derive(Subcommand)]
pub enum CommentCmd {
    /// Comment on a branch (of this workspace, or --workspace), an agent
    /// (agN) or a try (aN), on lines with --path and --line.
    Add {
        /// Branch name, agN, aN, or a key like branch:WS/NAME.
        target: String,
        /// Workspace id or name of the branch. Defaults to this one.
        #[arg(long)]
        workspace: Option<String>,
        /// File the comment is on.
        #[arg(long, requires = "line")]
        path: Option<String>,
        /// Line N or range N-M (new side; --old for removed lines).
        #[arg(long, requires = "path")]
        line: Option<String>,
        /// The line numbers are of the base (removed or old lines).
        #[arg(long)]
        old: bool,
        /// Keep it as a draft of a review (`comment send` sends it).
        #[arg(long)]
        draft: bool,
        /// Who writes it. Defaults to human:<your user name>.
        #[arg(long)]
        author: Option<String>,
        #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// List threads, oldest first: where each is now, and whether its lines
    /// are gone (outdated).
    Ls {
        /// Only this branch, agent or try.
        target: Option<String>,
        /// What the agent (or person) working here works on: this agent and
        /// the checked-out branch. Hands over reviews sent its way.
        #[arg(long, conflicts_with = "target")]
        here: bool,
        #[arg(long)]
        workspace: Option<String>,
        /// Only open and addressed threads.
        #[arg(long)]
        open: bool,
        /// Drafts too.
        #[arg(long)]
        drafts: bool,
    },
    /// One thread with all its comments.
    Show { id: String },
    /// Reply in a thread.
    Reply {
        id: String,
        #[arg(long)]
        author: Option<String>,
        /// Write as the agent working here (`agent:agN`, from
        /// $AGENTVC_AGENT or this worktree).
        #[arg(long, conflicts_with = "author")]
        as_agent: bool,
        #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// Close a thread (an agent's --author marks it addressed instead).
    Resolve {
        id: String,
        #[arg(long)]
        author: Option<String>,
        /// Write as the agent working here (`agent:agN`, from
        /// $AGENTVC_AGENT or this worktree).
        #[arg(long, conflicts_with = "author")]
        as_agent: bool,
    },
    /// Open a thread again.
    Reopen {
        id: String,
        #[arg(long)]
        author: Option<String>,
        /// Write as the agent working here (`agent:agN`, from
        /// $AGENTVC_AGENT or this worktree).
        #[arg(long, conflicts_with = "author")]
        as_agent: bool,
    },
    /// Delete one of your drafts, or with --all every draft on a target.
    Discard {
        /// Thread id (cN), or a target with --all.
        id: String,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        author: Option<String>,
    },
    /// Send your drafts on a target as one review; --agent hands them to the
    /// agent working on it.
    Send {
        target: String,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        agent: bool,
        /// Also post them as a review of the branch's GitHub PR.
        #[arg(long)]
        github: bool,
        #[arg(long)]
        author: Option<String>,
    },
    /// Sync a branch's threads with its GitHub PR: bring in review threads,
    /// replies and resolutions from GitHub, push replies and states from
    /// here. Without a target, the checked-out branch.
    Sync {
        target: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Inside an agent: the Claude Code `PostToolUse` hook that hands it the
    /// review comments sent while it works. Reads the hook input on stdin.
    #[command(hide = true)]
    Inbox,
}

/// The person here, as the review UI records them: `agentvc config
/// user.name`, else git's `user.name`, else the OS user.
pub fn me(cwd: &Path) -> String {
    if let Ok(s) = agentvc_core::Space::discover(cwd) {
        let h = s.human();
        if h != "human" {
            return h;
        }
    }
    let git = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["config", "user.name"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(agentvc_core::workspace::user);
    agentvc_core::config::human_actor(Some(&git))
}

/// A target from the command line: agN, aN, a key, or a branch name of
/// this workspace (or `workspace`).
pub fn parse_target(cwd: &Path, s: &str, workspace: Option<&str>) -> anyhow::Result<Target> {
    if s.contains(':') {
        return Ok(Target::parse_key(s)?);
    }
    if let Some(n) = s.strip_prefix("ag") {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return Ok(Target::agent(s));
        }
    }
    if let Some(n) = s.strip_prefix('a') {
        if let Ok(n) = n.parse::<i64>() {
            return Ok(Target::attempt(n));
        }
    }
    let w = Window::open(cwd)?;
    let ws = match workspace {
        Some(id) => agentvc_core::workspace::find_local(&w, id)?,
        None => agentvc_core::workspace::find_local(&w, &w.path().to_string_lossy())?,
    };
    Ok(Target::branch(&ws.record.id, s))
}

fn parse_lines(s: &str) -> anyhow::Result<(usize, usize)> {
    let bad = || agentvc_core::Error::Stack {
        code: "bad_line",
        message: format!("{s:?} is not a line or range (N or N-M)"),
        details: None,
    };
    let (a, b) = s.split_once('-').unwrap_or((s, s));
    let a: usize = a.trim().parse().map_err(|_| bad())?;
    let b: usize = b.trim().parse().map_err(|_| bad())?;
    Ok((a, b))
}

fn where_(t: &Thread) -> String {
    match &t.anchor {
        None => "(whole change)".into(),
        Some(a) => {
            let side = if a.side == Side::Old { " (old)" } else { "" };
            match t.current {
                Some(p) if p.start == p.end => format!("{}:{}{side}", a.path, p.start),
                Some(p) => format!("{}:{}-{}{side}", a.path, p.start, p.end),
                None => format!("{}:{} (outdated){side}", a.path, a.start),
            }
        }
    }
}

fn line(t: &Thread) -> String {
    let first = t
        .comments
        .first()
        .map(|c| c.body.lines().next().unwrap_or("").to_string())
        .unwrap_or_default();
    let replies = t.comments.len().saturating_sub(1);
    format!(
        "{:<5} {:<9} {:<28} {}  {}{}",
        t.id,
        t.state.as_str(),
        where_(t),
        t.author(),
        first,
        if replies > 0 {
            format!("  (+{replies})")
        } else {
            String::new()
        }
    )
}

fn full(t: &Thread) -> String {
    let mut out = format!(
        "{}  {}  {}  {}\n",
        t.id,
        t.state.as_str(),
        t.target.key(),
        where_(t)
    );
    if let Some(a) = &t.anchor {
        for l in &a.lines {
            out.push_str(&format!("    > {l}\n"));
        }
    }
    for c in &t.comments {
        out.push_str(&format!("  {}: {}\n", c.author, c.body));
    }
    out.trim_end().to_string()
}

pub fn run(cwd: &Path, cmd: CommentCmd) -> anyhow::Result<(Value, String)> {
    let author = |a: Option<String>| {
        a.filter(|a| !a.trim().is_empty())
            .unwrap_or_else(|| me(cwd))
    };
    Ok(match cmd {
        CommentCmd::Add {
            target,
            workspace,
            path,
            line: lines,
            old,
            draft,
            author: who,
            text,
        } => {
            let target = parse_target(cwd, &target, workspace.as_deref())?;
            let lines = match (path, lines) {
                (Some(path), Some(l)) => {
                    let (start, end) = parse_lines(&l)?;
                    Some(LineRange {
                        path,
                        side: if old { Side::Old } else { Side::New },
                        start,
                        end: Some(end),
                    })
                }
                _ => None,
            };
            let t = comments::add(
                cwd,
                &AddRequest {
                    target,
                    lines,
                    body: text.join(" "),
                    author: author(who),
                    pending: draft,
                },
            )?;
            let text = format!("{} {}", t.id, where_(&t));
            (json!({ "thread": t }), text)
        }
        CommentCmd::Ls {
            target,
            here,
            workspace,
            open,
            drafts,
        } => {
            let target = target
                .map(|t| parse_target(cwd, &t, workspace.as_deref()))
                .transpose()?;
            let threads = if here {
                comments::here(cwd, open)?
            } else {
                comments::list(
                    cwd,
                    &Filter {
                        target,
                        active: open,
                        pending: drafts,
                    },
                )?
            };
            let text = if threads.is_empty() {
                "no comments".to_string()
            } else {
                threads.iter().map(line).collect::<Vec<_>>().join("\n")
            };
            (json!({ "threads": threads }), text)
        }
        CommentCmd::Show { id } => {
            let t = comments::get(cwd, &id)?;
            let text = full(&t);
            (json!({ "thread": t }), text)
        }
        CommentCmd::Reply {
            id,
            author: who,
            as_agent,
            text,
        } => {
            let who = if as_agent {
                Some(comments::agent_here(cwd))
            } else {
                who
            };
            let t = comments::reply(cwd, &id, &text.join(" "), &author(who))?;
            let text = format!(
                "{}: {} comments, {}",
                t.id,
                t.comments.len(),
                t.state.as_str()
            );
            (json!({ "thread": t }), text)
        }
        CommentCmd::Resolve {
            id,
            author: who,
            as_agent,
        } => {
            let who = if as_agent {
                Some(comments::agent_here(cwd))
            } else {
                who
            };
            let t = comments::resolve(cwd, &id, &author(who))?;
            let text = format!("{} {}", t.id, t.state.as_str());
            (json!({ "thread": t }), text)
        }
        CommentCmd::Reopen {
            id,
            author: who,
            as_agent,
        } => {
            let who = if as_agent {
                Some(comments::agent_here(cwd))
            } else {
                who
            };
            let t = comments::reopen(cwd, &id, &author(who))?;
            let text = format!("{} {}", t.id, t.state.as_str());
            (json!({ "thread": t }), text)
        }
        CommentCmd::Discard {
            id,
            all,
            workspace,
            author: who,
        } => {
            if all {
                let target = parse_target(cwd, &id, workspace.as_deref())?;
                let gone = comments::discard_all(cwd, &target, &author(who))?;
                let text = format!("discarded {} drafts", gone.len());
                (json!({ "discarded": gone }), text)
            } else {
                let t = comments::discard(cwd, &id, &author(who))?;
                let text = format!("discarded {}", t.id);
                (json!({ "discarded": [t.id] }), text)
            }
        }
        CommentCmd::Send {
            target,
            workspace,
            agent,
            github,
            author: who,
        } => {
            let target = parse_target(cwd, &target, workspace.as_deref())?;
            let exe = std::env::current_exe()?;
            let s = comments::submit(
                cwd,
                &target,
                Deliver { agent, github },
                &author(who),
                Some(&exe),
            )?;
            let n = s.threads.len();
            let text = match (s.delivery, &s.agent) {
                (comments::Delivery::Prompt, Some(a)) => {
                    format!(
                        "{}: {n} comments sent to {} (next turn started)",
                        s.review.id, a.id
                    )
                }
                (comments::Delivery::Queued, Some(a)) => format!(
                    "{}: {n} comments queued for {} (it gets them at its next step)",
                    s.review.id, a.id
                ),
                (comments::Delivery::Waiting, _) => format!(
                    "{}: {n} comments sent; no agent works on {} yet, the next one gets them",
                    s.review.id,
                    target.key()
                ),
                _ => format!("{}: {n} comments sent", s.review.id),
            };
            let text = match s.threads.iter().find_map(|t| t.github.as_ref()) {
                Some(l) if s.review.github_review.is_some() => {
                    format!("{text}; posted as a review of PR #{}", l.pr)
                }
                _ => text,
            };
            (serde_json::to_value(&s)?, text)
        }
        CommentCmd::Sync { target, workspace } => {
            let target = match target {
                Some(t) => parse_target(cwd, &t, workspace.as_deref())?,
                None => {
                    let w = Window::open(cwd)?;
                    let b = w
                        .current_branch()
                        .ok_or_else(|| agentvc_core::Error::Stack {
                            code: "no_branch",
                            message: "not on a branch; name one".into(),
                            details: None,
                        })?;
                    parse_target(cwd, &b, workspace.as_deref())?
                }
            };
            let s = comments::github::sync(cwd, &target)?;
            let mut text = format!(
                "{}#{}: {} new, {} updated, {} pushed",
                s.repo,
                s.pr,
                s.imported.len(),
                s.updated.len(),
                s.pushed.len()
            );
            for e in &s.errors {
                text.push_str(&format!("\nnot pushed: {e}"));
            }
            (serde_json::to_value(&s)?, text)
        }
        CommentCmd::Inbox => {
            use std::io::Read;
            let mut raw = String::new();
            let _ = std::io::stdin().read_to_string(&mut raw);
            let input: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
            let dir = input
                .get("cwd")
                .and_then(Value::as_str)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| cwd.to_path_buf());
            // Never fail the agent's tool call.
            let note = comments::inbox(&dir, "hook").ok().flatten();
            let text = match &note {
                Some(n) => hook_context(n),
                None => String::new(),
            };
            (json!({ "notice": note }), text)
        }
    })
}

/// What a `PostToolUse` hook prints to hand `note` to the agent.
pub fn hook_context(note: &str) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": note,
        }
    })
    .to_string()
}
