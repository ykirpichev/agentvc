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
use agentvc_core::{Attempt, Error, ForkRequest, Ref, Space, Status};
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
    Init,
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
    /// Show attempts as a tree.
    Log {
        #[arg(long)]
        task: Option<String>,
    },
    /// Show one attempt and its snapshots.
    Show { attempt: String },
    /// List files that differ between two refs (default: from the parent snapshot).
    Diff { a: String, b: Option<String> },
    /// Print the view directory of an attempt.
    Path { attempt: String },
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
        /// Shell command run in the view afterwards to judge the attempt.
        #[arg(long)]
        check: Option<String>,
        /// Start in the background and return at once; see `agentvc ps`.
        #[arg(long)]
        detach: bool,
        /// The command to run, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// List runs that have not finished (or all with --all).
    Ps {
        #[arg(long)]
        all: bool,
    },
    #[command(hide = true)]
    RunWorker { run: i64 },
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
            let (code, message) = match e.downcast_ref::<Error>() {
                Some(ce) => (ce.code(), ce.to_string()),
                None => ("error", format!("{e:#}")),
            };
            if as_json {
                println!(
                    "{}",
                    json!({ "ok": false, "error": { "code": code, "message": message } })
                );
            } else {
                eprintln!("error[{code}]: {message}");
            }
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
    if let Cmd::Init = cli.cmd {
        let space = Space::init(&cwd)?;
        let snap = space.head_snapshot(agentvc_core::space::ROOT_ATTEMPT)?;
        return Ok(Out {
            text: format!(
                "Initialized agentvc space in {}\nRoot attempt a1 at snapshot s{} (tree {})",
                space.workdir().display(),
                snap.id,
                &snap.tree[..12]
            ),
            json: json!({ "workdir": space.workdir(), "attempt": 1, "snapshot": snap }),
        });
    }
    let space = Space::discover(&cwd)?;
    let current = || -> anyhow::Result<i64> { Ok(space.attempt_for_path(&cwd)?.id) };

    Ok(match cli.cmd {
        Cmd::Init => unreachable!(),
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
            };
            let r = space.fork(&req)?;
            let timing = r
                .stats
                .as_ref()
                .map(|s| format!(", {} files in {} ms", s.files, s.millis))
                .unwrap_or_default();
            Out {
                text: format!(
                    "a{} forked from {} (s{}) via {}{}\n{}",
                    r.attempt.id,
                    from,
                    r.parent_snapshot.id,
                    r.backend.as_str(),
                    timing,
                    r.attempt.view_path.display()
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
            status_out(space.set_status(attempt_id(&attempt)?, Status::Abandoned, Some(&reason))?)
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
            out
        }
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
        Cmd::Diff { a, b } => {
            let ra = parse_ref(&a)?;
            let (from, to) = match b {
                Some(b) => (ra, parse_ref(&b)?),
                None => {
                    let Ref::Attempt(id) = ra else {
                        anyhow::bail!("diff with one argument needs an attempt (aN)");
                    };
                    let parent = space
                        .attempt(id)?
                        .parent_snapshot
                        .ok_or_else(|| anyhow::anyhow!("a{id} has no parent; pass two refs"))?;
                    (Ref::Snapshot(parent), ra)
                }
            };
            let changes = space.diff(from, to)?;
            let mut text = String::new();
            for c in &changes {
                let mark = match c.kind {
                    ChangeKind::Added => 'A',
                    ChangeKind::Modified => 'M',
                    ChangeKind::Removed => 'D',
                };
                text += &format!("{mark}  {}\n", c.path);
            }
            if changes.is_empty() {
                text = "no differences".into();
            }
            Out {
                text,
                json: json!({ "from": from.to_string(), "to": to.to_string(), "changes": changes }),
            }
        }
        Cmd::Run {
            task,
            from,
            actor,
            model,
            prompt,
            check,
            detach,
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
            })?;
            let a = fork.attempt.id;
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
                        "a{a} started in background (run r{}, pid {})\nview {}\nlog  {}",
                        run.id,
                        child.id(),
                        fork.attempt.view_path.display(),
                        log.display()
                    ),
                    json: json!({ "attempt": fork.attempt, "run": run, "pid": child.id() }),
                }
            } else {
                let out = agentvc_core::run::execute(&space, run.id)?;
                let code = out.run.exit_code.unwrap_or(1);
                EXIT_OVERRIDE.store(code, Ordering::Relaxed);
                Out {
                    text: format!(
                        "a{a} {}  exit {}{}  {} ms  snapshot s{}",
                        out.attempt.status,
                        code,
                        out.run
                            .check_exit_code
                            .map(|c| format!(", check {c}"))
                            .unwrap_or_default(),
                        out.run.duration_ms.unwrap_or(0),
                        out.snapshot.id
                    ),
                    json: json!({ "attempt": out.attempt, "run": out.run, "snapshot": out.snapshot }),
                }
            }
        }
        Cmd::RunWorker { run } => {
            let out = agentvc_core::run::execute(&space, run)?;
            Out {
                text: format!(
                    "[agentvc] r{run} finished: exit {}, a{} {}",
                    out.run.exit_code.unwrap_or(-1),
                    out.attempt.id,
                    out.attempt.status
                ),
                json: json!({ "run": out.run }),
            }
        }
        Cmd::Ps { all } => {
            let runs = space.runs(!all)?;
            let mut text = String::new();
            for r in &runs {
                let state = match (r.exit_code, r.alive) {
                    (Some(c), _) => format!("exit {c}"),
                    (None, Some(true)) => "running".to_string(),
                    _ => "lost".to_string(),
                };
                text += &format!(
                    "r{}  a{}  {:<8}  pid {}  {}\n",
                    r.id,
                    r.attempt_id,
                    state,
                    r.pid.map(|p| p.to_string()).unwrap_or("-".into()),
                    r.command.join(" ")
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
        Cmd::Path { attempt } => {
            let a = space.attempt(attempt_id(&attempt)?)?;
            Out {
                text: a.view_path.display().to_string(),
                json: json!({ "attempt": a.id, "path": a.view_path }),
            }
        }
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
