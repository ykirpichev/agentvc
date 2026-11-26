//! `agentvc stack`: stacked branches per worktree (#107). See
//! `agentvc_core::stack`.

use std::collections::HashSet;
use std::path::Path;

use agentvc_core::stack::{BranchInfo, LogResult, ParentKind, RestackResult, Window};
use clap::Subcommand;
use serde_json::{json, Value};

#[derive(Subcommand)]
pub enum StackCmd {
    /// Set this window's trunk branch and remote (detected from
    /// `origin/HEAD` otherwise).
    Init {
        #[arg(long)]
        trunk: Option<String>,
        #[arg(long)]
        remote: Option<String>,
    },
    /// Show this window's stack: branches, commits, PRs and what needs a
    /// restack.
    Log {
        /// Also list other windows' stacks (read-only).
        #[arg(long)]
        all: bool,
    },
    /// Create a branch on top of the current one (or on trunk) and commit
    /// the staged changes to it.
    Create {
        /// Branch name; defaults to one made from the message.
        name: Option<String>,
        /// Commit message for the staged changes.
        #[arg(short, long)]
        message: Option<String>,
        /// Stage all changes first (`git add -A`).
        #[arg(short, long)]
        all: bool,
    },
    /// Amend the current branch's last commit with the staged changes and
    /// restack the branches above it.
    Modify {
        #[arg(short, long)]
        message: Option<String>,
        /// Stage all changes first.
        #[arg(short, long)]
        all: bool,
        /// Add a new commit instead of amending.
        #[arg(short, long)]
        commit: bool,
        /// On conflict, rebase the conflicting branch in this worktree.
        #[arg(long)]
        keep_conflicts: bool,
    },
    /// Replay every branch of this window onto its parent's current tip.
    /// Nothing changes on conflict unless --keep-conflicts.
    Restack {
        /// Only this branch and the ones above it.
        #[arg(long)]
        branch: Option<String>,
        /// On conflict, restack what can be and rebase the conflicting
        /// branch in this worktree (resolve, `git rebase --continue`, then
        /// run restack again).
        #[arg(long)]
        keep_conflicts: bool,
    },
    /// Fetch the trunk, remove branches merged into it and restack the rest.
    /// Never moves the local trunk branch or other windows' branches.
    Sync {
        /// Use the trunk as already fetched.
        #[arg(long)]
        no_fetch: bool,
        /// Keep merged branches in git (they still leave the stack).
        #[arg(long)]
        keep_merged: bool,
        #[arg(long)]
        keep_conflicts: bool,
    },
    /// Track existing branches in this window: the branch and the untracked
    /// branches below it (parents from Graphite metadata or git history).
    Adopt {
        branch: String,
        /// The parent to use instead of the inferred one (only this branch).
        #[arg(long)]
        parent: Option<String>,
        /// Only this branch, not the ones below it.
        #[arg(long)]
        no_chain: bool,
    },
    /// Stop tracking a branch; branches above it keep its commits. The git
    /// branch stays.
    Untrack { branch: String },
    /// Check out the branch stacked on the current one.
    Up,
    /// Check out the parent branch.
    Down,
    /// Check out the top of the current stack.
    Top,
    /// Check out the bottom of the current stack.
    Bottom,
}

fn short(s: &str) -> &str {
    &s[..10.min(s.len())]
}

fn restack_text(r: &RestackResult) -> String {
    let mut t = String::new();
    for u in &r.updated {
        t += &format!(
            "restacked {} onto {} ({} commit{}{})\n",
            u.branch,
            short(&u.onto),
            u.commits,
            if u.commits == 1 { "" } else { "s" },
            if u.dropped > 0 {
                format!(", {} already upstream", u.dropped)
            } else {
                String::new()
            }
        );
    }
    if let Some(c) = &r.conflict {
        t += &format!(
            "{} conflicts in {} at \"{}\": resolve the files, `git add` them, `git rebase --continue`, then `agentvc stack restack`\n",
            c.branch,
            c.paths.join(", "),
            c.subject
        );
    }
    if r.updated.is_empty() && r.conflict.is_none() {
        t += "everything is up to date\n";
    }
    t
}

fn branch_line(b: &BranchInfo) -> String {
    let mut s = format!(
        "{}{} {}",
        "  ".repeat(b.depth + 1),
        if b.current { "*" } else { "o" },
        b.name
    );
    s += &format!(
        "  {} commit{}",
        b.commits,
        if b.commits == 1 { "" } else { "s" }
    );
    if let Some(pr) = b.pr {
        s += &format!("  #{pr}");
    }
    if b.depth == 0 {
        match b.parent_kind {
            ParentKind::Trunk => {}
            ParentKind::OtherWindow => {
                s += &format!(
                    "  on {} (window {})",
                    b.parent,
                    b.parent_window
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                )
            }
            _ => s += &format!("  on {}", b.parent),
        }
    }
    if b.needs_restack {
        s += "  needs restack";
    }
    if b.tip.is_none() {
        s += "  (branch deleted)";
    }
    if let Some(p) = &b.checked_out_elsewhere {
        s += &format!("  (checked out in {})", p.display());
    }
    s
}

fn log_text(l: &LogResult) -> String {
    let mut t = format!(
        "{} on {} {}\n",
        l.window.display(),
        l.base_ref,
        l.base.as_deref().map(short).unwrap_or("(missing)")
    );
    if l.branches.is_empty() {
        t += "  no branches yet: `agentvc stack create <name> -m <message>` or `agentvc stack adopt <branch>`\n";
    }
    for b in &l.branches {
        t += &branch_line(b);
        t.push('\n');
    }
    if let Some(p) = &l.pending {
        t += &format!(
            "rebase of {} in progress: finish it, then `agentvc stack restack`\n",
            p.branch
        );
    }
    for o in &l.others {
        t += &format!("\nwindow {} (read-only)\n", o.path.display());
        for b in &o.branches {
            t += &format!("  o {} (on {})\n", b.name, b.parent);
        }
    }
    t
}

/// Run `agentvc stack ...`; returns (json, text).
pub fn run(cwd: &Path, cmd: StackCmd) -> anyhow::Result<(Value, String)> {
    let mut w = Window::open(cwd)?;
    Ok(match cmd {
        StackCmd::Init { trunk, remote } => {
            w.set_trunk(trunk.as_deref(), remote.as_deref())?;
            (
                json!({ "trunk": w.state.trunk, "remote": w.state.remote, "base_ref": w.base_ref() }),
                format!("this window stacks on {}", w.base_ref()),
            )
        }
        StackCmd::Log { all } => {
            let l = w.log(all)?;
            let text = log_text(&l);
            (json!(l), text)
        }
        StackCmd::Create { name, message, all } => {
            let (t, commit) = w.create(name.as_deref(), message.as_deref(), all)?;
            let text = format!(
                "created {} on {}{}",
                t.name,
                if t.parent == w.state.trunk {
                    w.base_ref()
                } else {
                    t.parent.clone()
                },
                match &commit {
                    Some(c) => format!(" with commit {}", short(c)),
                    None => " (nothing staged, no commit)".into(),
                }
            );
            (json!({ "branch": t, "commit": commit }), text)
        }
        StackCmd::Modify {
            message,
            all,
            commit,
            keep_conflicts,
        } => {
            let (c, r) = w.modify(message.as_deref(), all, commit, keep_conflicts)?;
            let text = format!("committed {}\n{}", short(&c), restack_text(&r));
            (json!({ "commit": c, "restack": r }), text)
        }
        StackCmd::Restack {
            branch,
            keep_conflicts,
        } => {
            let scope: Option<HashSet<String>> = match &branch {
                Some(b) => {
                    if !w.tracks(b) {
                        return Err(agentvc_core::Error::Stack {
                            code: "not_in_stack",
                            message: format!("branch {b} is not in this window's stack"),
                            details: Some(json!({ "branch": b })),
                        }
                        .into());
                    }
                    Some(w.upstack(b))
                }
                None => None,
            };
            let r = w.restack(scope.as_ref(), keep_conflicts)?;
            let text = restack_text(&r);
            (json!(r), text)
        }
        StackCmd::Sync {
            no_fetch,
            keep_merged,
            keep_conflicts,
        } => {
            let s = w.sync(!no_fetch, !keep_merged, keep_conflicts)?;
            let mut text = String::new();
            if s.fetched {
                text += &format!("fetched {} {}\n", w.base_ref(), short(&s.base));
            }
            for m in &s.merged {
                text += &format!(
                    "{m} is merged: removed from the stack{}\n",
                    if s.deleted.contains(m) {
                        " and deleted"
                    } else {
                        ""
                    }
                );
            }
            if let Some(t) = &s.switched_to {
                text += &format!("switched to {t}\n");
            }
            text += &restack_text(&s.restack);
            (json!(s), text)
        }
        StackCmd::Adopt {
            branch,
            parent,
            no_chain,
        } => {
            let added = w.adopt(&branch, parent.as_deref(), !no_chain)?;
            let mut text = String::new();
            for t in &added {
                text += &format!("tracking {} on {}\n", t.name, t.parent);
            }
            if added.is_empty() {
                text = format!("{branch} is already tracked");
            }
            (json!({ "adopted": added }), text)
        }
        StackCmd::Untrack { branch } => {
            let t = w.untrack(&branch)?;
            (
                json!({ "untracked": t }),
                format!("stopped tracking {}", t.name),
            )
        }
        StackCmd::Up | StackCmd::Down | StackCmd::Top | StackCmd::Bottom => {
            let b = match cmd {
                StackCmd::Up => w.up()?,
                StackCmd::Down => w.down()?,
                StackCmd::Top => w.top()?,
                _ => w.bottom()?,
            };
            (json!({ "branch": b }), format!("switched to {b}"))
        }
    })
}
