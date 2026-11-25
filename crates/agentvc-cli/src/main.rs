//! `agentvc`: command-line interface for agent-first version control.
//!
//! Every command prints human-readable text by default and a single JSON
//! object with `--json`. Errors in JSON mode look like
//! `{"ok": false, "error": {"code": "not_found", "message": "..."}}` and exit
//! with status 1. Nothing ever prompts for input.

use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};

use agentvc_core::snapshot::ChangeKind;
use agentvc_core::view::BackendChoice;
use agentvc_core::{Attempt, Error, ExportOptions, ForkRequest, PrOptions, Ref, Space, Status};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(
    name = "agentvc",
    version,
    about = "Version control built for AI agents"
)]
struct Cli {
    /// Print one JSON object instead of text.
    #[arg(long, global = true)]
    json: bool,
    /// Run as if started in this directory.
    #[arg(short = 'C', long = "dir", global = true, value_name = "DIR")]
    dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a space here and snapshot the working directory as attempt a1.
    ///
    /// Where forks will use overlayfs, this also starts `agentvc prepare a1`
    /// in the background, so the first fork does not pay for copying the
    /// working directory into a base layer.
    Init {
        /// Do not prepare a1 in the background.
        #[arg(long)]
        no_prepare: bool,
        /// Rebuild a space that `agentvc sync` pushed to this remote (default
        /// origin) instead of starting an empty one: same tasks, attempts
        /// and ids, losers included. Picks the space synced from the branch
        /// checked out here, else the only one; see --space.
        #[arg(long, value_name = "REMOTE", num_args = 0..=1, default_missing_value = "origin")]
        from_remote: Option<String>,
        /// With --from-remote: the space id (or a prefix), from `agentvc sync --list`.
        #[arg(long, requires = "from_remote")]
        space: Option<String>,
        /// With --from-remote: only a space synced from the branch checked out
        /// here; fail with not_found otherwise.
        #[arg(long, requires = "from_remote")]
        this_branch: bool,
    },
    /// Push this space's history (tasks, attempts, snapshots, reasons, runs)
    /// to a git remote as refs/agentvc/spaces/<id>, so it outlives this
    /// machine. Restore it elsewhere with `agentvc init --from-remote`.
    Sync {
        #[arg(long, default_value = "origin")]
        remote: String,
        /// List the spaces synced to the remote instead.
        #[arg(long)]
        list: bool,
        /// Also sync after every `run`, `promote`, `abandon` and `pr` from now
        /// on (best effort: failures are warnings).
        #[arg(long, conflicts_with = "no_auto")]
        auto: bool,
        /// Stop syncing automatically (and don't sync now).
        #[arg(long)]
        no_auto: bool,
        /// Overwrite the remote even if another machine synced this space
        /// since this one did.
        #[arg(long)]
        force: bool,
    },
    /// Create and list tasks.
    #[command(subcommand)]
    Task(TaskCmd),
    /// Start a new attempt in its own view, forked from an attempt or snapshot.
    Fork {
        /// Task this attempt works on (tN or N).
        #[arg(long)]
        task: Option<String>,
        /// What to fork from: aN (latest state of an attempt) or sN. Defaults
        /// to the attempt of the current directory.
        #[arg(long)]
        from: Option<String>,
        /// Who works in this attempt, for example agent:claude-code or human.
        #[arg(long, default_value = "human")]
        actor: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        /// View backend: auto (reflink, else overlay, else copy), reflink,
        /// overlay or copy.
        #[arg(long, default_value = "auto", value_name = "BACKEND")]
        backend: BackendChoice,
    },
    /// Record the current state of an attempt's view.
    Snapshot {
        /// Attempt (aN). Defaults to the attempt of the current directory.
        attempt: Option<String>,
        #[arg(long, default_value = "manual")]
        trigger: String,
        /// Metadata to attach, as key=value. Repeatable.
        #[arg(long = "meta", value_name = "KEY=VALUE")]
        meta: Vec<String>,
    },
    /// Mark an attempt as passed or failed (it stays open).
    Mark {
        attempt: String,
        #[arg(value_parser = ["passed", "failed", "exploring"])]
        status: String,
    },
    /// Close an attempt, keeping it and its reason in history.
    Abandon {
        attempt: String,
        #[arg(long)]
        reason: String,
    },
    /// Pick an attempt as the winner; its open siblings are abandoned.
    Promote { attempt: String },
    /// Where am I and what is going on. Inside an attempt's view (or with
    /// aN): its task, status, head snapshot, files changed since it, active
    /// runs, PR, and whether main has moved since its base (`behind`). In the
    /// host checkout: open tasks with their attempts, what needs review and
    /// what is running. Takes no snapshot and fetches nothing.
    Status { attempt: Option<String> },
    /// Show attempts as a tree.
    Log {
        #[arg(long)]
        task: Option<String>,
    },
    /// Show one attempt and its snapshots.
    Show { attempt: String },
    /// Compare two refs (aN or sN). With one attempt, compares it with the
    /// snapshot it was forked from (or the commit it was rebased onto). With
    /// no refs, inside a view, compares the view's working state with that
    /// base, without taking a snapshot first.
    Diff {
        a: Option<String>,
        b: Option<String>,
        /// Print a unified diff; `--json` adds per-file hunks.
        #[arg(long, short = 'p')]
        patch: bool,
        /// Print changed lines per file, like `git diff --stat`.
        #[arg(long)]
        stat: bool,
        /// Lines of context around changes in --patch.
        #[arg(long, short = 'U', default_value_t = agentvc_core::patch::DEFAULT_CONTEXT, value_name = "N")]
        unified: usize,
    },
    /// Print the view directory of an attempt, mounting an overlay view first
    /// if it is not mounted (for example after a reboot).
    Path { attempt: String },
    /// Freeze an attempt's view into an overlay layer now, so the next fork
    /// from it is fast. The first prepare copies the view; later ones write
    /// only what changed. Does nothing where forks do not use overlayfs.
    Prepare {
        /// Attempt (aN). Defaults to the attempt of the current directory.
        attempt: Option<String>,
    },
    /// Write an attempt's latest state as a commit on a branch of the host git
    /// repo, without touching the working tree, index or checked-out branch.
    Export {
        attempt: String,
        /// Branch to create.
        #[arg(long)]
        branch: String,
        /// Commit message. Defaults to the task goal.
        #[arg(long, short = 'm')]
        message: Option<String>,
        /// Commit to parent on. Defaults to the HEAD recorded at init.
        #[arg(long)]
        base: Option<String>,
        /// Move the branch if it already exists and the new commit does not
        /// build on it.
        #[arg(long)]
        force: bool,
        /// Extra commit trailer, as "Key: value". Repeatable.
        #[arg(long = "trailer", value_name = "KEY: VALUE")]
        trailers: Vec<String>,
    },
    /// Rebase an attempt onto a newer commit of the host repo, so `export`
    /// and `pr` carry only its own changes. Three-way merges the attempt's
    /// tree with the new commit, writes the result into its view and records
    /// it as a snapshot. On conflicts nothing changes and the error (code
    /// `conflict`) lists the paths.
    Rebase {
        attempt: String,
        /// Git revision to rebase onto. Defaults to the remote's default
        /// branch, fetched first.
        #[arg(long)]
        onto: Option<String>,
        /// Remote whose default branch is the default target.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Use the remote-tracking branch as it is, without fetching.
        #[arg(long)]
        no_fetch: bool,
        /// On conflicts, write files with conflict markers into the view and
        /// mark the attempt exploring; fix them, then `snapshot`.
        #[arg(long)]
        keep_conflicts: bool,
    },
    /// Export an attempt, push its branch and open (or update) a GitHub PR.
    Pr {
        attempt: String,
        /// Branch the PR merges into. Defaults to the repo's default branch.
        #[arg(long)]
        base: Option<String>,
        /// Branch to push. Defaults to `agentvc config pr.branch`,
        /// $AGENTVC_PR_BRANCH, then agentvc/aN. If it is the branch checked
        /// out here, the checkout moves along with it.
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        draft: bool,
        /// PR title and commit subject. Defaults to the task goal.
        #[arg(long)]
        title: Option<String>,
        #[arg(long, default_value = "origin")]
        remote: String,
        /// GitHub repository as owner/name. Defaults to the remote's.
        #[arg(long)]
        repo: Option<String>,
        /// Move the branch even if another attempt exported to it.
        #[arg(long)]
        force: bool,
        /// File (or - for stdin) whose text goes above the generated body,
        /// for example attribution lines. Kept on later updates; defaults to
        /// the file in `agentvc config pr.body-prefix`.
        #[arg(long, value_name = "FILE")]
        body_prefix: Option<PathBuf>,
        /// Extra trailer for the exported commit, as "Key: value".
        /// Repeatable; added to those in `agentvc config pr.trailer`.
        #[arg(long = "trailer", value_name = "KEY: VALUE")]
        trailers: Vec<String>,
        /// Do not sync the space to the remote afterwards.
        #[arg(long)]
        no_sync: bool,
    },
    /// Read or change this space's settings. With no key, lists them all.
    ///
    /// Keys: pr.branch (also $AGENTVC_PR_BRANCH), pr.base ($AGENTVC_PR_BASE),
    /// pr.trailer (a list: use --add), pr.body-prefix (a file, relative to
    /// the space; $AGENTVC_PR_BODY_PREFIX).
    Config {
        key: Option<String>,
        value: Option<String>,
        /// Append the value to a list setting (pr.trailer).
        #[arg(long)]
        add: bool,
        /// Remove the setting.
        #[arg(long, conflicts_with = "value")]
        unset: bool,
    },
    /// Fork a new attempt and run a command (usually an agent) inside its view.
    ///
    /// The view is snapshotted when the command exits. With --check, the
    /// attempt is marked passed or failed by the check's exit code; without
    /// it, a non-zero exit marks it failed. In text mode `agentvc run` exits
    /// with the command's exit code.
    Run {
        /// Task this attempt works on (tN or N).
        #[arg(long)]
        task: Option<String>,
        /// What to fork from: aN or sN. Defaults to the attempt of the current directory.
        #[arg(long)]
        from: Option<String>,
        #[arg(long, default_value = "agent")]
        actor: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        /// View backend: auto, reflink, overlay or copy (see `fork`).
        #[arg(long, default_value = "auto", value_name = "BACKEND")]
        backend: BackendChoice,
        /// Shell command run in the view afterwards to judge the attempt.
        #[arg(long)]
        check: Option<String>,
        /// Start in the background and return at once; see `agentvc ps`.
        #[arg(long)]
        detach: bool,
        /// Do not add `--settings <space>/claude-hooks.json` to a `claude`
        /// command. By default it is added unless the command passes its own
        /// --settings or --plugin-dir, or (outside a Claude Code cloud
        /// session) the view's .claude settings already install the hooks.
        #[arg(long)]
        no_hooks: bool,
        /// The command to run, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Summarize the whole space as one JSON document: tasks, attempt trees,
    /// statuses and reasons, latest runs and PRs (schema agentvc.overview/1).
    ///
    /// Used by dashboards outside the daemon, such as the Claude project
    /// artifact; see docs/overview.md.
    Overview {
        /// Write the document to this file (pretty JSON) instead of printing it.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
    /// List runs that have not finished (or all with --all).
    Ps {
        #[arg(long)]
        all: bool,
    },
    #[command(hide = true)]
    RunWorker { run: i64 },
    /// Install or remove Claude Code hooks that snapshot attempts per tool call.
    Hooks {
        /// Write ~/.claude/settings.json instead of this repo's
        /// .claude/settings.local.json.
        #[arg(long)]
        user: bool,
        /// Remove agentvc's hooks instead of installing them.
        #[arg(long)]
        uninstall: bool,
    },
    /// Serve the agentvc tools to coding agents over MCP (stdio).
    ///
    /// Each tool runs the matching command with `--json` and returns its
    /// JSON. Commands run in --space (or -C, or the current directory), which
    /// picks the space and the current attempt.
    Mcp {
        /// Directory to run commands in. Defaults to -C or the current directory.
        #[arg(long, value_name = "DIR")]
        space: Option<PathBuf>,
    },
    /// Hook handler called by Claude Code; reads the hook JSON on stdin.
    /// Always exits 0 so a hook can never break an agent session.
    Hook {
        #[arg(value_parser = [
            "post-tool-use",
            "stop",
            "subagent-stop",
            "cursor-after-file-edit",
            "cursor-stop",
        ])]
        event: String,
    },
    /// Set up an editor to work in this space: for Cursor, the MCP server
    /// (.cursor/mcp.json), hooks that snapshot the agent's edits
    /// (.cursor/hooks.json) and a rule describing the attempt workflow
    /// (.cursor/rules/agentvc.mdc).
    Integrate {
        #[arg(value_parser = ["cursor"])]
        editor: String,
        /// Revert every file `integrate` wrote, keeping your own settings.
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Open a task.
    New {
        goal: String,
        #[arg(long)]
        criteria: Option<String>,
    },
    /// List tasks.
    List,
}

/// Output of a command: JSON for `--json`, text otherwise.
/// Exit code to use in text mode instead of 0 (set by `run`).
static EXIT_OVERRIDE: AtomicI32 = AtomicI32::new(0);

struct Out {
    json: Value,
    text: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Cmd::Mcp { space } = &cli.cmd {
        return mcp(space.as_ref().or(cli.dir.as_ref()));
    }
    let as_json = cli.json;
    match run(cli) {
        Ok(out) => {
            if as_json {
                let mut v = out.json;
                if let Value::Object(m) = &mut v {
                    m.insert("ok".into(), Value::Bool(true));
                }
                println!("{v}");
            } else if !out.text.is_empty() {
                println!("{}", out.text.trim_end());
            }
            match EXIT_OVERRIDE.load(Ordering::Relaxed) {
                c if c > 0 && !as_json => ExitCode::from(c.min(255) as u8),
                _ => ExitCode::SUCCESS,
            }
        }
        Err(e) => {
            let (code, message, details) = match e.downcast_ref::<Error>() {
                Some(ce) => (ce.code(), ce.to_string(), ce.details()),
                None => ("error", format!("{e:#}"), None),
            };
            if as_json {
                let mut err = json!({ "code": code, "message": message });
                if let (Value::Object(m), Some(Value::Object(d))) = (&mut err, details) {
                    m.extend(d);
                }
                println!("{}", json!({ "ok": false, "error": err }));
            } else {
                eprintln!("error[{code}]: {message}");
            }
            ExitCode::FAILURE
        }
    }
}

/// What `diff aN` compares an attempt with: the commit it was last rebased
/// onto (so main's newer changes are not listed), else the snapshot it was
/// forked from. `None` for the root attempt. Returns the label and the tree.
fn attempt_base(space: &Space, id: i64) -> anyhow::Result<Option<(String, String)>> {
    if let Some(base) = space.own_git_base(id)? {
        return Ok(Some((format!("git:{}", base.commit), base.tree)));
    }
    Ok(match space.attempt(id)?.parent_snapshot {
        Some(p) => Some((format!("s{p}"), space.snapshot_by_id(p)?.tree)),
        None => None,
    })
}

fn diff_out(from: String, to: String, changes: &[agentvc_core::snapshot::FileChange]) -> Out {
    let mut text = String::new();
    for c in changes {
        text += &format!("{}  {}\n", change_mark(c.kind), c.path);
    }
    if changes.is_empty() {
        text = "no differences".into();
    }
    Out {
        text,
        json: json!({ "from": from, "to": to, "changes": changes }),
    }
}

/// `agentvc mcp`: serve until stdin closes. A missing space is not an error
/// here; tools report `no_space` when called.
fn mcp(dir: Option<&PathBuf>) -> ExitCode {
    let started = (|| -> std::io::Result<()> {
        let cwd = std::env::current_dir()?;
        let dir = match dir {
            Some(d) => cwd.join(d),
            None => cwd,
        };
        agentvc_mcp::Server::new(std::env::current_exe()?, dir).serve_stdio()
    })();
    match started {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agentvc mcp: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_id(s: &str, prefix: char, kind: &'static str) -> anyhow::Result<i64> {
    s.strip_prefix(prefix).unwrap_or(s).parse().map_err(|_| {
        Error::NotFound {
            kind,
            id: s.to_string(),
        }
        .into()
    })
}

fn attempt_id(s: &str) -> anyhow::Result<i64> {
    parse_id(s, 'a', "attempt")
}

fn parse_ref(s: &str) -> anyhow::Result<Ref> {
    if s.chars().all(|c| c.is_ascii_digit()) {
        return Ok(Ref::Attempt(s.parse()?));
    }
    Ok(s.parse()?)
}

fn run(cli: Cli) -> anyhow::Result<Out> {
    let cwd = match &cli.dir {
        Some(d) => d.clone(),
        None => std::env::current_dir()?,
    };
    if let Cmd::Init {
        no_prepare,
        from_remote: Some(remote),
        space,
        this_branch,
    } = &cli.cmd
    {
        let opts = agentvc_core::CloneOptions {
            remote: Some(remote.clone()),
            space: space.clone(),
            this_branch: *this_branch,
        };
        let (space, r) = agentvc_core::sync::clone_from_remote(&cwd, &opts)?;
        let prepare = if *no_prepare {
            json!({ "skipped": "--no-prepare" })
        } else {
            prepare_in_background(&space)
        };
        let views: Vec<_> = r.views.iter().map(|v| format!("a{v}")).collect();
        return Ok(Out {
            text: format!(
                "Rebuilt space {} from {} ({}): {} tasks, {} attempts, {} snapshots\n\
                 Views written for open attempts: {}",
                r.space_id,
                r.remote,
                &r.commit[..12],
                r.tasks,
                r.attempts,
                r.snapshots,
                if views.is_empty() {
                    "none".into()
                } else {
                    views.join(", ")
                }
            ),
            json: json!({ "workdir": space.workdir(), "clone": r, "prepare": prepare }),
        });
    }
    if let Cmd::Init { no_prepare, .. } = cli.cmd {
        let space = Space::init(&cwd)?;
        let snap = space.head_snapshot(agentvc_core::space::ROOT_ATTEMPT)?;
        let prepare = if no_prepare {
            json!({ "skipped": "--no-prepare" })
        } else {
            prepare_in_background(&space)
        };
        let note = match prepare["pid"].as_u64() {
            Some(pid) => {
                format!("\nPreparing a1 for fast overlay forks in the background (pid {pid})")
            }
            None => String::new(),
        };
        return Ok(Out {
            text: format!(
                "Initialized agentvc space in {}\nRoot attempt a1 at snapshot s{} (tree {}){note}",
                space.workdir().display(),
                snap.id,
                &snap.tree[..12]
            ),
            json: json!({
                "workdir": space.workdir(),
                "attempt": 1,
                "snapshot": snap,
                "prepare": prepare,
            }),
        });
    }
    match &cli.cmd {
        Cmd::Hook { event } => return Ok(run_hook(event, &cwd, cli.json)),
        Cmd::Hooks { user, uninstall } => return hooks_cmd(&cwd, *user, *uninstall),
        Cmd::Integrate { remove, .. } => return integrate_cmd(&cwd, *remove),
        _ => {}
    }
    let space = Space::discover(&cwd)?;
    let current = || -> anyhow::Result<i64> { Ok(space.attempt_for_path(&cwd)?.id) };

    Ok(match cli.cmd {
        Cmd::Sync {
            remote,
            list,
            auto,
            no_auto,
            force,
        } => {
            if list {
                let spaces = agentvc_core::sync::remote_spaces(space.workdir(), Some(&remote))?;
                let mine = agentvc_core::sync::space_id(&space)?;
                let mut text = String::new();
                for s in &spaces {
                    text += &format!(
                        "{}{}  {}  {} attempts  {}\n",
                        s.space_id,
                        if s.space_id == mine {
                            " (this space)"
                        } else {
                            ""
                        },
                        s.branch.as_deref().unwrap_or("-"),
                        s.attempts,
                        s.tasks.join("; ")
                    );
                }
                if spaces.is_empty() {
                    text = format!("no spaces synced to {remote}");
                }
                return Ok(Out {
                    text,
                    json: json!({ "remote": remote, "spaces": spaces, "this_space": mine }),
                });
            }
            if auto {
                space.set_meta(agentvc_core::sync::AUTO_SYNC_KEY, &remote)?;
            }
            if no_auto {
                space.delete_meta(agentvc_core::sync::AUTO_SYNC_KEY)?;
                return Ok(Out {
                    text: "auto-sync is off".into(),
                    json: json!({ "auto": null }),
                });
            }
            let r = agentvc_core::sync::sync(
                &space,
                &agentvc_core::SyncOptions {
                    remote: Some(remote),
                    force,
                },
            )?;
            let auto_note = match space.meta(agentvc_core::sync::AUTO_SYNC_KEY)? {
                Some(r) => format!("\nauto-sync to {r} is on"),
                None => String::new(),
            };
            Out {
                text: format!(
                    "{} space {} to {} {} at {}: {} tasks, {} attempts, {} snapshots{auto_note}",
                    if r.unchanged {
                        "Already synced"
                    } else {
                        "Synced"
                    },
                    r.space_id,
                    r.remote,
                    r.refname,
                    &r.commit[..12],
                    r.tasks,
                    r.attempts,
                    r.snapshots
                ),
                json: json!({ "sync": r, "auto": space.meta(agentvc_core::sync::AUTO_SYNC_KEY)? }),
            }
        }
        Cmd::Init { .. }
        | Cmd::Hook { .. }
        | Cmd::Hooks { .. }
        | Cmd::Integrate { .. }
        | Cmd::Mcp { .. } => unreachable!(),
        Cmd::Task(TaskCmd::New { goal, criteria }) => {
            let t = space.create_task(&goal, criteria.as_deref())?;
            Out {
                text: format!("t{}  {}", t.id, t.goal),
                json: json!({ "task": t }),
            }
        }
        Cmd::Task(TaskCmd::List) => {
            let tasks = space.tasks()?;
            let mut text = String::new();
            for t in &tasks {
                let n = space.attempts(Some(t.id))?.len();
                text += &format!("t{}  {}  ({} attempts)\n", t.id, t.goal, n);
            }
            Out {
                text,
                json: json!({ "tasks": tasks }),
            }
        }
        Cmd::Fork {
            task,
            from,
            actor,
            model,
            prompt,
            backend,
        } => {
            let from = match from {
                Some(f) => parse_ref(&f)?,
                None => Ref::Attempt(current()?),
            };
            let req = ForkRequest {
                task_id: task.map(|t| parse_id(&t, 't', "task")).transpose()?,
                from: Some(from),
                actor,
                model,
                prompt,
                backend,
            };
            let r = space.fork(&req)?;
            #[allow(unused_mut)]
            let mut timing = r
                .stats
                .as_ref()
                .map(|s| format!(", {} files in {} ms", s.files, s.millis))
                .unwrap_or_default();
            #[cfg(target_os = "linux")]
            if let (Some(o), Some(s)) = (&r.overlay, &r.stats) {
                timing = if o.reused {
                    format!(
                        ", parent unchanged, layer L{} shared, {} ms",
                        o.layer, s.millis
                    )
                } else {
                    format!(
                        ", {} changed files frozen into L{} ({} layers), {} ms",
                        s.files, o.layer, o.layers, s.millis
                    )
                };
            }
            let mut notes = String::new();
            if let Some(why) = &r.backend_reason {
                notes += &format!("\n({why})");
            }
            #[cfg(target_os = "linux")]
            if r.overlay.as_ref().is_some_and(|o| !o.global) {
                notes += "\nThe view is mounted in a user namespace: only agentvc commands \
                          (agentvc run) see its contents.";
            }
            Out {
                text: format!(
                    "a{} forked from {} (s{}) via {}{}\n{}{}",
                    r.attempt.id,
                    from,
                    r.parent_snapshot.id,
                    r.backend.as_str(),
                    timing,
                    r.attempt.view_path.display(),
                    notes
                ),
                json: json!({ "fork": r }),
            }
        }
        Cmd::Snapshot {
            attempt,
            trigger,
            meta,
        } => {
            let id = match attempt {
                Some(a) => attempt_id(&a)?,
                None => current()?,
            };
            let meta = if meta.is_empty() {
                None
            } else {
                let mut m = serde_json::Map::new();
                for kv in meta {
                    let (k, v) = kv
                        .split_once('=')
                        .ok_or_else(|| anyhow::anyhow!("--meta expects KEY=VALUE, got {kv:?}"))?;
                    m.insert(k.to_string(), Value::String(v.to_string()));
                }
                Some(Value::Object(m))
            };
            let r = space.snapshot(id, &trigger, meta)?;
            Out {
                text: if r.unchanged {
                    format!("a{id} unchanged since s{}", r.snapshot.id)
                } else {
                    format!("s{}  a{id}  tree {}", r.snapshot.id, &r.snapshot.tree[..12])
                },
                json: json!({ "snapshot": r.snapshot, "unchanged": r.unchanged }),
            }
        }
        Cmd::Mark { attempt, status } => {
            let s = Status::parse(&status).expect("clap validated");
            status_out(space.set_status(attempt_id(&attempt)?, s, None)?)
        }
        Cmd::Abandon { attempt, reason } => {
            let mut out = status_out(space.set_status(
                attempt_id(&attempt)?,
                Status::Abandoned,
                Some(&reason),
            )?);
            add_sync(&mut out, agentvc_core::sync::auto_sync(&space));
            out
        }
        Cmd::Promote { attempt } => {
            let a = space.set_status(attempt_id(&attempt)?, Status::Promoted, None)?;
            let closed: Vec<_> = match a.task_id {
                Some(t) => space
                    .attempts(Some(t))?
                    .into_iter()
                    .filter(|s| s.reason.as_deref() == Some(&*format!("lost to a{}", a.id)))
                    .map(|s| s.id)
                    .collect(),
                None => vec![],
            };
            let mut out = status_out(a);
            if !closed.is_empty() {
                let list: Vec<_> = closed.iter().map(|i| format!("a{i}")).collect();
                out.text += &format!("\nabandoned as lost: {}", list.join(", "));
            }
            out.json["closed_siblings"] = json!(closed);
            add_sync(&mut out, agentvc_core::sync::auto_sync(&space));
            out
        }
        Cmd::Status { attempt } => {
            let id = match attempt {
                Some(a) => Some(attempt_id(&a)?),
                None => match space.attempt_for_path(&cwd) {
                    Ok(a) if a.id != agentvc_core::space::ROOT_ATTEMPT => Some(a.id),
                    _ => None,
                },
            };
            match id {
                Some(id) => attempt_status_out(space.attempt_status(id)?),
                None => space_status_out(space.space_status()?),
            }
        }
        Cmd::Overview { out } => overview_out(&space, out.as_deref(), cli.json)?,
        Cmd::Log { task } => {
            let task = task.map(|t| parse_id(&t, 't', "task")).transpose()?;
            let attempts = space.attempts(task)?;
            Out {
                text: render_tree(&attempts),
                json: json!({ "attempts": attempts }),
            }
        }
        Cmd::Show { attempt } => {
            let a = space.attempt(attempt_id(&attempt)?)?;
            let snaps = space.snapshots(a.id)?;
            let mut text = format!(
                "a{}  {}{}\nactor   {}\nmodel   {}\ntask    {}\nparent  {}\nview    {} ({})\n",
                a.id,
                a.status,
                a.reason
                    .as_ref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default(),
                a.actor,
                a.model.as_deref().unwrap_or("-"),
                a.task_id.map(|t| format!("t{t}")).unwrap_or("-".into()),
                match (a.parent_attempt, a.parent_snapshot) {
                    (Some(p), Some(s)) => format!("a{p} at s{s}"),
                    _ => "-".into(),
                },
                a.view_path.display(),
                a.backend,
            );
            for s in &snaps {
                text += &format!(
                    "  s{}  {}  {}{}\n",
                    s.id,
                    &s.tree[..12],
                    s.trigger,
                    s.meta
                        .as_ref()
                        .map(|m| format!("  {m}"))
                        .unwrap_or_default()
                );
            }
            Out {
                text,
                json: json!({ "attempt": a, "snapshots": snaps }),
            }
        }
        Cmd::Diff {
            a,
            b,
            patch,
            stat,
            unified,
        } => {
            let (from, from_tree, to, to_tree) = match (a, b) {
                (Some(a), Some(b)) => {
                    let (ra, rb) = (parse_ref(&a)?, parse_ref(&b)?);
                    let ta = space.resolve(ra)?.tree;
                    let tb = space.resolve(rb)?.tree;
                    (ra.to_string(), ta, rb.to_string(), tb)
                }
                (Some(a), None) => {
                    let Ref::Attempt(id) = parse_ref(&a)? else {
                        anyhow::bail!("diff with one argument needs an attempt (aN)");
                    };
                    let (from, from_tree) = attempt_base(&space, id)?
                        .ok_or_else(|| anyhow::anyhow!("a{id} has no parent; pass two refs"))?;
                    let to_tree = space.head_snapshot(id)?.tree;
                    (from, from_tree, format!("a{id}"), to_tree)
                }
                (None, _) => {
                    // The working state of the current view, like `git diff`.
                    let a = space.attempt(current()?)?;
                    if !a.status.is_open() {
                        anyhow::bail!(
                            "a{} is {}; pass refs to compare its history",
                            a.id,
                            a.status
                        );
                    }
                    let to_tree = space.capture_view(&a)?.to_hex();
                    let (from, from_tree) = match attempt_base(&space, a.id)? {
                        Some(b) => b,
                        // The root attempt: changes since its last snapshot.
                        None => {
                            let h = space.head_snapshot(a.id)?;
                            (format!("s{}", h.id), h.tree)
                        }
                    };
                    (from, from_tree, format!("a{} (working)", a.id), to_tree)
                }
            };
            let (ta, tb) = (
                agentvc_core::objects::Oid::from_hex(&from_tree)?,
                agentvc_core::objects::Oid::from_hex(&to_tree)?,
            );
            if patch || stat {
                let mut files =
                    agentvc_core::patch::diff_trees(space.objects(), &ta, &tb, unified)?;
                let mut text = String::new();
                if stat {
                    text += &agentvc_core::patch::render_stat(&files);
                }
                if patch {
                    if stat {
                        text.push('\n');
                    }
                    text += &agentvc_core::patch::render(&files);
                } else {
                    for f in &mut files {
                        f.hunks.clear();
                    }
                }
                if files.is_empty() {
                    text = "no differences".into();
                }
                Out {
                    text,
                    json: json!({ "from": from, "to": to, "changes": files }),
                }
            } else {
                let changes = agentvc_core::snapshot::diff(space.objects(), &ta, &tb)?;
                diff_out(from, to, &changes)
            }
        }
        Cmd::Run {
            task,
            from,
            actor,
            model,
            prompt,
            backend,
            check,
            detach,
            no_hooks,
            command,
        } => {
            let from = match from {
                Some(f) => parse_ref(&f)?,
                None => Ref::Attempt(current()?),
            };
            let fork = space.fork(&ForkRequest {
                task_id: task.map(|t| parse_id(&t, 't', "task")).transpose()?,
                from: Some(from),
                actor,
                model,
                prompt,
                backend,
            })?;
            let a = fork.attempt.id;
            let hooks = if no_hooks {
                None
            } else {
                Some(agentvc_core::run::with_claude_hooks(
                    &space,
                    &fork.attempt.view_path,
                    &command,
                    &std::env::current_exe()?,
                    agentvc_core::run::in_remote_claude_session(),
                )?)
            };
            let command = hooks.as_ref().map_or(command, |h| h.command.clone());
            let hooks_note = match hooks.as_ref().and_then(|h| h.settings.as_ref()) {
                Some(p) => format!("\nhooks: added --settings {}", p.display()),
                None => String::new(),
            };
            // Child output would corrupt --json, and a detached run has no terminal.
            let log_to_file = detach || cli.json;
            let run = space.create_run(a, &command, check.as_deref(), log_to_file)?;
            if detach {
                let log = run.log_path.clone().expect("detached runs log to a file");
                let f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log)?;
                let mut worker = std::process::Command::new(std::env::current_exe()?);
                worker
                    .arg("-C")
                    .arg(space.workdir())
                    .arg("run-worker")
                    .arg(run.id.to_string())
                    .stdin(Stdio::null())
                    .stdout(f.try_clone()?)
                    .stderr(f);
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    // Own process group, so closing the terminal doesn't stop it.
                    worker.process_group(0);
                }
                let child = worker.spawn()?;
                Out {
                    text: format!(
                        "a{a} started in background (run r{}, pid {})\nview {}\nlog  {}{hooks_note}",
                        run.id,
                        child.id(),
                        fork.attempt.view_path.display(),
                        log.display()
                    ),
                    json: json!({
                        "attempt": fork.attempt,
                        "run": run,
                        "pid": child.id(),
                        "hooks": hooks,
                        "fork": fork_json(&fork),
                    }),
                }
            } else {
                let out = agentvc_core::run::execute(&space, run.id)?;
                let code = out.run.exit_code.unwrap_or(1);
                EXIT_OVERRIDE.store(code, Ordering::Relaxed);
                let warnings: String = out
                    .warnings
                    .iter()
                    .map(|w| format!("\nwarning: {w}"))
                    .collect();
                let mut res = Out {
                    text: format!(
                        "a{a} {}  exit {}{}  {} ms  snapshot s{}{hooks_note}{warnings}",
                        out.attempt.status,
                        code,
                        out.run
                            .check_exit_code
                            .map(|c| format!(", check {c}"))
                            .unwrap_or_default(),
                        out.run.duration_ms.unwrap_or(0),
                        out.snapshot.id
                    ),
                    json: json!({
                        "attempt": out.attempt,
                        "run": out.run,
                        "snapshot": out.snapshot,
                        "hooks": hooks,
                        "hook_snapshots": out.hook_snapshots,
                        "warnings": out.warnings,
                        "fork": fork_json(&fork),
                    }),
                };
                add_sync(&mut res, agentvc_core::sync::auto_sync(&space));
                res
            }
        }
        Cmd::RunWorker { run } => {
            let out = agentvc_core::run::execute(&space, run)?;
            let mut res = Out {
                text: format!(
                    "[agentvc] r{run} finished: exit {}, a{} {}",
                    out.run.exit_code.unwrap_or(-1),
                    out.attempt.id,
                    out.attempt.status
                ),
                json: json!({
                    "run": out.run,
                    "hook_snapshots": out.hook_snapshots,
                    "warnings": out.warnings,
                }),
            };
            add_sync(&mut res, agentvc_core::sync::auto_sync(&space));
            res
        }
        Cmd::Ps { all } => {
            let runs = space.runs(!all)?;
            let mut text = String::new();
            for r in &runs {
                let state = match (r.exit_code, r.alive, r.pid) {
                    (Some(c), _, _) => format!("exit {c}"),
                    (None, Some(true), _) => "running".to_string(),
                    // The worker records the pid just after it spawns the
                    // command; until then the run is starting, not lost.
                    (None, _, None) => "starting".to_string(),
                    _ => "lost".to_string(),
                };
                text += &format!(
                    "r{}  a{}  {:<8}  pid {}  {}\n",
                    r.id,
                    r.attempt_id,
                    state,
                    r.pid.map(|p| p.to_string()).unwrap_or("-".into()),
                    short_command(&r.command)
                );
            }
            if runs.is_empty() {
                text = if all {
                    "no runs"
                } else {
                    "no running attempts"
                }
                .into();
            }
            Out {
                text,
                json: json!({ "runs": runs }),
            }
        }
        Cmd::Prepare { attempt } => {
            let id = match attempt {
                Some(a) => attempt_id(&a)?,
                None => current()?,
            };
            prepare_cmd(&space, id)?
        }
        Cmd::Path { attempt } => {
            let a = space.attempt(attempt_id(&attempt)?)?;
            // Mount an overlay view that is not mounted, e.g. after a reboot.
            space.ensure_view(&a)?;
            Out {
                text: a.view_path.display().to_string(),
                json: json!({ "attempt": a.id, "path": a.view_path }),
            }
        }
        Cmd::Export {
            attempt,
            branch,
            message,
            base,
            force,
            trailers,
        } => {
            let opts = ExportOptions {
                branch,
                base,
                message,
                force,
                trailers,
            };
            let r = agentvc_core::export::export(&space, attempt_id(&attempt)?, &opts)?;
            Out {
                text: format!(
                    "a{} (s{}) -> {} at {}{}",
                    r.attempt,
                    r.snapshot.id,
                    r.branch,
                    &r.commit[..12],
                    r.parent
                        .as_ref()
                        .map(|p| format!(", parent {}", &p[..12]))
                        .unwrap_or_else(|| ", root commit".into())
                ),
                json: json!({ "export": r }),
            }
        }
        Cmd::Rebase {
            attempt,
            onto,
            remote,
            no_fetch,
            keep_conflicts,
        } => {
            let opts = agentvc_core::RebaseOptions {
                onto,
                remote: Some(remote),
                no_fetch,
                keep_conflicts,
                actor: None,
            };
            let r = agentvc_core::rebase::rebase(&space, attempt_id(&attempt)?, &opts)?;
            let short = |c: &str| c[..12.min(c.len())].to_string();
            let text = if !r.rebased {
                format!(
                    "a{} is already based on {} ({})",
                    r.attempt,
                    r.onto_ref,
                    short(&r.onto)
                )
            } else {
                let mut t = format!(
                    "a{} rebased onto {} ({}): {} files changed, snapshot s{}",
                    r.attempt,
                    r.onto_ref,
                    short(&r.onto),
                    r.changes.len(),
                    r.snapshot.as_ref().map(|s| s.id).unwrap_or_default()
                );
                if !r.view_updated {
                    t += "\n(the view is gone, so only history was updated)";
                }
                if !r.conflicts.is_empty() {
                    t += &format!(
                        "\n{} files have conflict markers; fix them, then `agentvc snapshot a{}`:",
                        r.conflicts.len(),
                        r.attempt
                    );
                    for p in &r.conflicts {
                        t += &format!("\n  C  {p}");
                    }
                }
                t
            };
            Out {
                text,
                json: json!({ "rebase": r }),
            }
        }
        Cmd::Config {
            key,
            value,
            add,
            unset,
        } => config_cmd(&space, key, value, add, unset)?,
        Cmd::Pr {
            attempt,
            base,
            branch,
            draft,
            title,
            remote,
            repo,
            force,
            body_prefix,
            trailers,
            no_sync,
        } => {
            let body_prefix = match body_prefix {
                Some(p) if p.as_os_str() == "-" => {
                    let mut s = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                    Some(s)
                }
                Some(p) => Some(
                    std::fs::read_to_string(cwd.join(&p))
                        .map_err(|e| anyhow::anyhow!("--body-prefix {}: {e}", p.display()))?,
                ),
                None => None,
            };
            let opts = PrOptions {
                branch,
                base,
                draft,
                remote: Some(remote.clone()),
                repo,
                title,
                force,
                body_prefix,
                trailers,
            };
            let r = agentvc_core::github::open_pr(&space, attempt_id(&attempt)?, &opts)?;
            // The PR shows only the winner; the attempts behind it go to the
            // remote too, so they outlive this machine (#56).
            let sync = (!no_sync).then(|| {
                agentvc_core::sync::sync(
                    &space,
                    &agentvc_core::SyncOptions {
                        remote: Some(remote),
                        force: false,
                    },
                )
            });
            let mut out = Out {
                text: format!(
                    "{} PR #{}: {}\n{} -> {} ({}){}",
                    if r.updated { "Updated" } else { "Opened" },
                    r.number,
                    r.url,
                    r.export.branch,
                    r.base,
                    &r.export.commit[..12],
                    if r.export.checkout_updated {
                        "\n(the branch is checked out here; the checkout moved to the new commit)"
                    } else {
                        ""
                    }
                ),
                json: json!({ "pr": r }),
            };
            add_sync(&mut out, sync);
            out
        }
    })
}

fn config_cmd(
    space: &Space,
    key: Option<String>,
    value: Option<String>,
    add: bool,
    unset: bool,
) -> anyhow::Result<Out> {
    use agentvc_core::config;
    let show = |k: &config::Key| -> anyhow::Result<Value> {
        let stored = space.config_values(k.name)?;
        let env = k
            .env
            .and_then(|v| std::env::var(v).ok().filter(|s| !s.trim().is_empty()));
        Ok(json!({
            "key": k.name,
            "values": stored,
            "env": k.env,
            "env_value": env,
            "about": k.about,
        }))
    };
    let line = |v: &Value| -> String {
        let vals: Vec<String> = v["values"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let mut l = format!(
            "{} = {}",
            v["key"].as_str().unwrap_or(""),
            if vals.is_empty() {
                "(unset)".to_string()
            } else {
                vals.join(" | ")
            }
        );
        if let (Some(e), Some(val)) = (v["env"].as_str(), v["env_value"].as_str()) {
            l += &format!("  (overridden by ${e}={val})");
        }
        l
    };
    let Some(name) = key else {
        let all: Vec<Value> = config::KEYS
            .iter()
            .map(show)
            .collect::<anyhow::Result<_>>()?;
        let text = all.iter().map(line).collect::<Vec<_>>().join("\n");
        return Ok(Out {
            text,
            json: json!({ "config": all }),
        });
    };
    let k = config::key(&name)?;
    if unset {
        space.set_config(k.name, &[])?;
    } else if let Some(v) = value {
        if add && !k.multi {
            anyhow::bail!("{} holds one value; set it without --add", k.name);
        }
        let mut vals = if add {
            space.config_values(k.name)?
        } else {
            Vec::new()
        };
        vals.push(v);
        space.set_config(k.name, &vals)?;
    }
    let v = show(k)?;
    Ok(Out {
        text: line(&v),
        json: json!({ "config": v }),
    })
}

/// Fork stats for `run --json`, the same numbers `fork --json` reports.
fn fork_json(f: &agentvc_core::ForkResult) -> Value {
    let mut v = json!({
        "backend": f.backend.as_str(),
        "backend_reason": f.backend_reason,
        "parent_snapshot": f.parent_snapshot.id,
        "millis": f.stats.as_ref().map(|s| s.millis),
        "bytes_copied": f.stats.as_ref().map(|s| s.bytes_copied),
        "files": f.stats.as_ref().map(|s| s.files),
        "stats": f.stats,
    });
    #[cfg(target_os = "linux")]
    {
        v["overlay"] = json!(f.overlay);
    }
    v
}

fn ago(t: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(t);
    match (now - t).max(0) {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86400 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86400),
    }
}

fn change_mark(k: ChangeKind) -> char {
    match k {
        ChangeKind::Added => 'A',
        ChangeKind::Modified => 'M',
        ChangeKind::Removed => 'D',
    }
}

fn attempt_status_out(st: agentvc_core::status::AttemptStatus) -> Out {
    let a = &st.attempt;
    let mut t = format!(
        "a{}  {}{}  {}{}\n",
        a.id,
        a.status,
        a.reason
            .as_ref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default(),
        a.actor,
        a.model
            .as_ref()
            .map(|m| format!(" ({m})"))
            .unwrap_or_default(),
    );
    t += &format!(
        "task     {}\n",
        st.task
            .as_ref()
            .map(|t| format!("t{}  {}", t.id, t.goal))
            .unwrap_or_else(|| "-".into())
    );
    t += &format!(
        "head     s{}  {}  {}\n",
        st.head.id,
        st.head.trigger,
        ago(st.head.created_at)
    );
    t += &format!("view     {}\n", a.view_path.display());
    match &st.changes {
        Some(c) if c.is_empty() => t += &format!("changes  none since s{}\n", st.head.id),
        Some(c) => {
            t += &format!(
                "changes  {} file{} since s{} (`agentvc snapshot a{}` to record):\n",
                c.len(),
                if c.len() == 1 { "" } else { "s" },
                st.head.id,
                a.id
            );
            for f in c {
                t += &format!("  {}  {}\n", change_mark(f.kind), f.path);
            }
        }
        None => {}
    }
    for r in &st.runs {
        t += &format!(
            "run      r{}  {}  pid {}\n",
            r.id,
            if r.alive == Some(true) {
                "running"
            } else {
                "lost"
            },
            r.pid.map(|p| p.to_string()).unwrap_or("-".into())
        );
    }
    if let Some(u) = &st.upstream {
        let base = &u.base[..12.min(u.base.len())];
        t += &if u.behind == 0 {
            format!("base     {base}, up to date with {}\n", u.upstream)
        } else {
            format!(
                "base     {base}, behind: {} commit{} of {} (run `agentvc rebase a{}` before `pr`)\n",
                u.behind,
                if u.behind == 1 { "" } else { "s" },
                u.upstream,
                a.id
            )
        };
    }
    if let Some(u) = &st.pr_url {
        t += &format!("pr       {u}\n");
    }
    Out {
        text: t,
        json: json!({ "status": st }),
    }
}

fn space_status_out(st: agentvc_core::status::SpaceStatus) -> Out {
    let mut t = format!("space {}\n", st.workdir.display());
    if st.open_tasks.is_empty() {
        t += "no open tasks";
        if st.done_tasks > 0 {
            t += &format!(" ({} done)", st.done_tasks);
        }
        t += "\n";
    }
    for ts in &st.open_tasks {
        t += &format!("\nt{}  {}\n", ts.task.id, ts.task.goal);
        if ts.attempts.is_empty() {
            t += "  no attempts yet\n";
        }
        for a in &ts.attempts {
            t += &format!(
                "  a{}  {}{}  {}{}{}\n",
                a.id,
                a.status,
                a.reason
                    .as_ref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default(),
                a.actor,
                a.model
                    .as_ref()
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default(),
                if a.needs_review {
                    "  <- needs review"
                } else {
                    ""
                },
            );
        }
    }
    if !st.needs_review.is_empty() {
        let list: Vec<_> = st.needs_review.iter().map(|i| format!("a{i}")).collect();
        t += &format!(
            "\nneeds review: {} (passed, not promoted; `agentvc diff --stat` them, then `agentvc promote`)\n",
            list.join(", ")
        );
    }
    for r in &st.runs {
        t += &format!(
            "running: r{}  a{}  pid {}\n",
            r.id,
            r.attempt_id,
            r.pid.map(|p| p.to_string()).unwrap_or("-".into())
        );
    }
    Out {
        text: t,
        json: json!({ "status": st }),
    }
}

/// Report a sync that ran after a command: a line of text and `sync` in the
/// JSON. A failed sync is a warning, never the command's failure.
fn add_sync(out: &mut Out, sync: Option<agentvc_core::Result<agentvc_core::SyncResult>>) {
    match sync {
        None => {}
        Some(Ok(r)) => {
            if !r.unchanged {
                out.text += &format!(
                    "\nsynced space {} to {} ({})",
                    r.space_id,
                    r.remote,
                    &r.commit[..12]
                );
            }
            out.json["sync"] = json!(r);
        }
        Some(Err(e)) => {
            out.text += &format!(
                "\nwarning: could not sync the space ({}): {e}; run `agentvc sync`",
                e.code()
            );
            out.json["sync"] = json!({ "error": { "code": e.code(), "message": e.to_string() } });
        }
    }
}

fn overview_out(
    space: &Space,
    out: Option<&std::path::Path>,
    as_json: bool,
) -> anyhow::Result<Out> {
    let o = space.overview()?;
    let mut text = String::new();
    for t in &o.tasks {
        let c = &t.counts;
        text += &format!(
            "{}  {}  {}  ({} attempts{}{})\n",
            t.id,
            serde_json::to_value(t.state)?.as_str().unwrap_or_default(),
            t.goal,
            c.total,
            if c.running > 0 {
                format!(", {} running", c.running)
            } else {
                String::new()
            },
            t.winner
                .as_ref()
                .map(|w| format!(", winner {w}"))
                .unwrap_or_default(),
        );
    }
    if o.tasks.is_empty() {
        text = "no tasks".into();
    }
    let Some(path) = out else {
        // Plain `agentvc overview --json` prints the document itself under
        // "overview"; text mode prints one line per task.
        return Ok(Out {
            text: if as_json { String::new() } else { text },
            json: json!({ "overview": o }),
        });
    };
    let body = serde_json::to_string_pretty(&o)?;
    std::fs::write(path, &body).map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
    Ok(Out {
        text: format!("{text}\nWrote {} ({} bytes)", path.display(), body.len()),
        json: json!({
            "written": path,
            "bytes": body.len(),
            "tasks": o.tasks.len(),
            "attempts": o.space.counts.total,
        }),
    })
}

fn status_out(a: Attempt) -> Out {
    Out {
        text: format!(
            "a{}  {}{}",
            a.id,
            a.status,
            a.reason
                .as_ref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        ),
        json: json!({ "attempt": a }),
    }
}

fn describe(a: &Attempt) -> String {
    let mut s = format!("a{}  {}", a.id, a.status);
    if let Some(r) = &a.reason {
        s += &format!(" ({r})");
    }
    s += &format!("  {}", a.actor);
    if let Some(m) = &a.model {
        s += &format!(" [{m}]");
    }
    if let Some(t) = a.task_id {
        s += &format!("  t{t}");
    }
    s
}

/// Render attempts as a tree by parent attempt. Attempts whose parent is not
/// in the list are shown as roots.
fn render_tree(attempts: &[Attempt]) -> String {
    let ids: std::collections::HashSet<i64> = attempts.iter().map(|a| a.id).collect();
    let mut out = String::new();
    for root in attempts
        .iter()
        .filter(|a| a.parent_attempt.is_none_or(|p| !ids.contains(&p)))
    {
        out += &describe(root);
        out.push('\n');
        render_children(attempts, root.id, "", &mut out);
    }
    if out.is_empty() {
        out = "no attempts".into();
    }
    out
}

fn render_children(attempts: &[Attempt], parent: i64, prefix: &str, out: &mut String) {
    let kids: Vec<_> = attempts
        .iter()
        .filter(|a| a.parent_attempt == Some(parent))
        .collect();
    for (i, k) in kids.iter().enumerate() {
        let last = i + 1 == kids.len();
        out.push_str(prefix);
        out.push_str(if last { "└─ " } else { "├─ " });
        out.push_str(&describe(k));
        out.push('\n');
        let next = format!("{prefix}{}", if last { "   " } else { "│  " });
        render_children(attempts, k.id, &next, out);
    }
}

fn run_hook(event: &str, cwd: &std::path::Path, as_json: bool) -> Out {
    use std::io::Read;
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let mut input: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
    let mut event = event;
    // Cursor's hooks send their own shape; reshape it for the shared handler.
    if let Some((e, reshaped)) = agentvc_core::cursor::hook_input(event, &input) {
        event = e;
        input = reshaped;
    }
    let result = agentvc_core::hooks::handle(event, &input, cwd);
    let json = match &result {
        Ok(o) => json!({ "hook": o }),
        Err(e) => {
            json!({ "hook": { "result": "error", "code": e.code(), "message": e.to_string() } })
        }
    };
    if let Err(e) = &result {
        // Surface hook failures without failing the agent's tool call.
        eprintln!("agentvc hook {event}: {e}");
    }
    Out {
        // Claude Code shows hook stdout in transcripts; stay quiet in text mode.
        text: String::new(),
        json: if as_json { json } else { Value::Null },
    }
}

fn integrate_cmd(cwd: &std::path::Path, remove: bool) -> anyhow::Result<Out> {
    let space = Space::discover(cwd)?;
    let root = space.workdir().to_path_buf();
    let exe = std::env::current_exe()?;
    // A bare `agentvc` keeps committed .cursor files portable; fall back to
    // the absolute path when this binary is not the one on PATH.
    let on_path = std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| {
            d.join("agentvc")
                .canonicalize()
                .is_ok_and(|c| exe.canonicalize().is_ok_and(|e| e == c))
        })
    });
    let cmd = if on_path {
        "agentvc".to_string()
    } else {
        exe.to_string_lossy().into_owned()
    };
    let r = agentvc_core::cursor::integrate(&root, &cmd, remove)?;
    let mut text = String::new();
    for f in &r.files {
        let action = serde_json::to_value(&f.action)?;
        text += &format!(
            "{:<10} {}\n",
            action.as_str().unwrap_or(""),
            f.path.strip_prefix(&root).unwrap_or(&f.path).display()
        );
    }
    if !remove {
        text += "Reload Cursor, then enable the agentvc MCP server in Settings > MCP if asked.";
    }
    Ok(Out {
        text: text.trim_end().to_string(),
        json: json!({ "integrate": r, "command": cmd }),
    })
}

fn hooks_cmd(cwd: &std::path::Path, user: bool, uninstall: bool) -> anyhow::Result<Out> {
    let path = if user {
        let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
        PathBuf::from(home).join(".claude/settings.json")
    } else {
        let root = Space::discover(cwd)
            .map(|s| s.workdir().to_path_buf())
            .unwrap_or_else(|_| cwd.to_path_buf());
        root.join(".claude/settings.local.json")
    };
    let exe = std::env::current_exe()?;
    if uninstall {
        agentvc_core::hooks::uninstall(&path)?;
    } else {
        agentvc_core::hooks::install(&path, &exe)?;
    }
    // Claude Code cloud sessions read .claude/settings.json only, so the hooks
    // in settings.local.json never fire there (#57).
    let remote_note =
        (!uninstall && !user && agentvc_core::run::in_remote_claude_session()).then(|| {
            format!(
                "This is a Claude Code cloud session (CLAUDE_CODE_REMOTE is set), which \
                 ignores .claude/settings.local.json. `agentvc run -- claude ...` loads the \
                 hooks with --settings for you; to start claude yourself, run \
                 `claude --settings {} ...`.",
                path.display()
            )
        });
    let mut text = format!(
        "{} Claude Code hooks in {}",
        if uninstall { "Removed" } else { "Installed" },
        path.display()
    );
    if let Some(n) = &remote_note {
        text += &format!("\n{n}");
    }
    Ok(Out {
        text,
        json: json!({
            "settings": path, "installed": !uninstall, "exe": exe, "remote_note": remote_note,
        }),
    })
}

#[cfg(target_os = "linux")]
fn prepare_cmd(space: &Space, id: i64) -> anyhow::Result<Out> {
    let r = space.prepare(id)?;
    let text = match (&r.skipped, r.layer) {
        (Some(why), _) => format!("a{id}: nothing to prepare ({why})"),
        (None, Some(l)) if r.reused => {
            format!("a{id} unchanged since layer L{l}, {} ms", r.millis)
        }
        (None, Some(l)) => format!(
            "a{id} frozen into L{l} ({} layers): {} files, {} MB, {} ms",
            r.layers,
            r.freeze.files,
            r.freeze.bytes / (1 << 20),
            r.millis
        ),
        (None, None) => format!("a{id} prepared"),
    };
    Ok(Out {
        text,
        json: json!({ "prepare": r }),
    })
}

#[cfg(not(target_os = "linux"))]
fn prepare_cmd(_space: &Space, id: i64) -> anyhow::Result<Out> {
    Ok(Out {
        text: format!("a{id}: nothing to prepare (overlayfs needs Linux)"),
        json: json!({ "prepare": { "attempt": id, "skipped": "overlayfs needs Linux" } }),
    })
}

/// Start `agentvc prepare a1` detached, when forks here will use overlayfs.
/// Returns what happened, for the init output.
fn prepare_in_background(space: &Space) -> Value {
    #[cfg(target_os = "linux")]
    {
        if let Err(why) = space.overlay_mounter() {
            return json!({ "skipped": why });
        }
        let start = || -> std::io::Result<u32> {
            let log = space
                .workdir()
                .join(agentvc_core::space::SPACE_DIR)
                .join("prepare.log");
            let f = std::fs::File::create(&log)?;
            let mut cmd = std::process::Command::new(std::env::current_exe()?);
            cmd.arg("-C")
                .arg(space.workdir())
                .args(["prepare", "a1"])
                .stdin(Stdio::null())
                .stdout(f.try_clone()?)
                .stderr(f);
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
            Ok(cmd.spawn()?.id())
        };
        match start() {
            Ok(pid) => json!({ "pid": pid }),
            Err(e) => json!({ "skipped": format!("could not start prepare: {e}") }),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = space;
        json!({ "skipped": "overlayfs needs Linux" })
    }
}

/// A run's command for one line of `ps`: agents get long prompts, so cut it
/// at 80 characters. `--json` keeps the full command.
fn short_command(cmd: &[String]) -> String {
    let full = cmd.join(" ");
    match full.char_indices().nth(80) {
        Some((i, _)) => format!("{}…", &full[..i]),
        None => full,
    }
}

#[cfg(test)]
mod tests {
    use super::short_command;

    #[test]
    fn short_command_cuts_long_prompts() {
        let cmd = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(short_command(&cmd("claude -p hi")), "claude -p hi");
        let long = short_command(&cmd(&format!("claude -p {}", "é".repeat(200))));
        assert_eq!(long.chars().count(), 81);
        assert!(long.ends_with('…'));
    }
}
