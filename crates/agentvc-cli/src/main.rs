//! `agentvc`: command-line interface for agent-first version control.
//!
//! Every command prints human-readable text by default and a single JSON
//! object with `--json`. Errors in JSON mode look like
//! `{"ok": false, "error": {"code": "not_found", "message": "..."}}` and exit
//! with status 1. Nothing ever prompts for input.

use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

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
        /// Rebuild the space that `agentvc pr` (or `sync`) saved in a comment
        /// on this PR; without a number, the PR of the branch checked out
        /// here. For machines that can't push refs/agentvc/* (cloud sessions).
        #[arg(
            long,
            value_name = "PR",
            num_args = 0..=1,
            default_missing_value = "branch",
            conflicts_with = "from_remote"
        )]
        from_pr: Option<String>,
        /// With --from-pr: GitHub repository as owner/name. Defaults to origin's.
        #[arg(long, requires = "from_pr")]
        repo: Option<String>,
    },
    /// Save this space's history (tasks, attempts, snapshots, reasons, runs)
    /// so it outlives this machine: push it to a git remote as
    /// refs/agentvc/spaces/<id> (restore with `agentvc init --from-remote`),
    /// and write it into a comment on the space's latest PR (restore with
    /// `agentvc init --from-pr`). Either one is enough.
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
        /// PR (number) to keep the space in. Defaults to the space's latest PR.
        #[arg(long)]
        pr: Option<u64>,
        /// With --pr: GitHub repository as owner/name. Defaults to the remote's.
        #[arg(long, requires = "pr")]
        repo: Option<String>,
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
        #[arg(long, conflicts_with = "from_worktree")]
        from: Option<String>,
        /// Start from the host checkout's uncommitted work (tracked edits,
        /// deletions, untracked files; not ignored ones), based on HEAD.
        #[arg(long)]
        from_worktree: bool,
        /// With --from-worktree: then reset those files in the checkout to
        /// HEAD, like `git stash` (the index is not touched). Bring them back
        /// with `agentvc apply aN`.
        #[arg(long = "move", requires = "from_worktree")]
        move_changes: bool,
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
    /// Write an attempt's changes (against its base) into the host checkout
    /// as uncommitted changes, like `git stash pop` or trying out a branch.
    /// Refuses with `dirty_worktree` when that would overwrite local edits.
    /// Never touches the index or the checked-out branch.
    Apply {
        attempt: String,
        /// Only these files or directories. Repeatable.
        #[arg(long = "paths", value_name = "PATH", num_args = 1..)]
        paths: Vec<String>,
        /// Overwrite local changes.
        #[arg(long)]
        force: bool,
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
    /// The operation log: every change to the space, newest first, with the
    /// ids `agentvc undo` takes.
    #[command(subcommand)]
    Op(OpCmd),
    /// Reverse an operation: a status change, a promote (its siblings and
    /// ancestors reopen), a task creation, or a fork (the attempt is closed
    /// as `fork undone` and its view put aside, not deleted). Without an id,
    /// undoes the newest undoable operation that still stands, so repeated
    /// `undo` walks back. Undo is itself an operation: `agentvc undo <id of
    /// the undo>` redoes. Refused with `undo_conflict`, changing nothing,
    /// when later operations changed the same attempts.
    Undo {
        /// Operation id from `agentvc op log` (N or opN).
        op: Option<String>,
        #[arg(long, default_value = "human")]
        actor: String,
    },
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
    /// Show one attempt and its snapshots (aN, or a tag: its attempt).
    Show { attempt: String },
    /// Name a snapshot (a checkpoint): `agentvc tag good a3` names a3's
    /// current state, `agentvc tag demo s12` one snapshot. Names work
    /// wherever a ref does (fork --from, diff, show), pin the snapshot
    /// against gc, and travel with sync. `--list` lists, `-d` deletes.
    Tag {
        /// The name (letters, digits, '.', '_', '-', '/'; not aN/sN/tN).
        #[arg(required_unless_present = "list")]
        name: Option<String>,
        /// What to name: aN (its latest state) or sN. Defaults to the
        /// attempt of the current directory.
        #[arg(conflicts_with_all = ["list", "delete"])]
        target: Option<String>,
        #[arg(long, conflicts_with_all = ["list", "delete"])]
        note: Option<String>,
        #[arg(long, conflicts_with = "delete")]
        list: bool,
        #[arg(short = 'd', long)]
        delete: bool,
        /// Who names it.
        #[arg(long, default_value = "human")]
        actor: String,
    },
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
    /// Check that the database, objects, views, overlay layers, runs and sync
    /// state agree (the `git fsck` of agentvc). Each finding has a stable
    /// code and severity; exits non-zero (also with --json) while errors
    /// remain.
    Doctor {
        /// Fix what is safe to fix: remount views, rewrite missing views from
        /// their latest snapshot, copy missing objects back from git, mark
        /// dead runs lost, delete directories nothing refers to.
        #[arg(long)]
        repair: bool,
        /// Skip the check against the sync remote.
        #[arg(long)]
        offline: bool,
    },
    /// Keep the space small: pack live objects into a git packfile, prune the
    /// trees of attempts abandoned more than gc.keepAbandonedDays ago (their
    /// history stays), delete views of attempts closed more than
    /// gc.keepViewsDays ago, and overlay layers nothing uses. Never prunes
    /// open, promoted, exported or named work, or what the last sync did
    /// not carry.
    Gc {
        /// Report what would be freed; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Only pack loose objects, and only past gc.auto of them (what
        /// snapshots start in the background).
        #[arg(long)]
        auto: bool,
        /// Override gc.keepAbandonedDays (default 30).
        #[arg(long, value_name = "DAYS")]
        keep_abandoned_days: Option<i64>,
        /// Override gc.keepViewsDays (default 7).
        #[arg(long, value_name = "DAYS")]
        keep_views_days: Option<i64>,
        /// Also drop unreferenced loose objects written in the last hour.
        #[arg(long)]
        prune_now: bool,
        /// Do not write packfiles.
        #[arg(long)]
        no_pack: bool,
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
    /// Take files or hunks of another attempt's changes (what `agentvc diff
    /// aY` shows) into an attempt's view, three-way merged with its own
    /// work, and record a `take` snapshot crediting the source. The source
    /// is not changed.
    Take {
        /// The attempt to take from (aY).
        source: String,
        /// The attempt that receives the changes. Defaults to the attempt of
        /// the current directory.
        #[arg(long)]
        into: Option<String>,
        /// Changed files to take whole: paths, directories or globs such as
        /// `tests/**`. Repeatable.
        #[arg(long = "paths", value_name = "PATH|GLOB", num_args = 1..)]
        paths: Vec<String>,
        /// Single hunks, as FILE:N or FILE:N,M, numbered from 1 as in
        /// `agentvc diff aY --patch`. Repeatable.
        #[arg(long = "hunks", value_name = "FILE:N", num_args = 1..)]
        hunks: Vec<String>,
        /// On conflicts, write conflict markers into the view and mark the
        /// attempt exploring; fix them, then `snapshot`.
        #[arg(long)]
        keep_conflicts: bool,
    },
    /// Three-way merge all of another attempt's changes into an attempt
    /// (`take` with everything), recorded as a `merge` snapshot.
    Merge {
        source: String,
        #[arg(long)]
        into: Option<String>,
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
    /// Git hook handler for attempt views: `post-commit` records the commit
    /// just made in the view as a snapshot (trigger `git-commit`).
    #[command(hide = true)]
    GitHook {
        #[arg(value_parser = ["post-commit"])]
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
enum OpCmd {
    /// List operations, newest first.
    Log {
        /// How many to show.
        #[arg(long, default_value_t = 20, conflicts_with = "all")]
        limit: usize,
        /// Show the whole log.
        #[arg(long)]
        all: bool,
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
/// Exit with failure even with `--json` (set by `doctor` when errors remain).
static EXIT_FAILURE: AtomicBool = AtomicBool::new(false);

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
            if EXIT_FAILURE.load(Ordering::Relaxed) {
                return ExitCode::FAILURE;
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
        Some(p) => Some((
            format!("s{p}"),
            space.snapshot_by_id(p)?.tree_oid()?.to_hex(),
        )),
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

fn run(cli: Cli) -> anyhow::Result<Out> {
    let cwd = match &cli.dir {
        Some(d) => d.clone(),
        None => std::env::current_dir()?,
    };
    if let Cmd::Init {
        no_prepare,
        from_pr: Some(pr),
        repo,
        ..
    } = &cli.cmd
    {
        let number = match pr.as_str() {
            "branch" => None,
            n => Some(
                n.trim_start_matches('#')
                    .parse::<u64>()
                    .ok()
                    .or_else(|| agentvc_core::github::parse_pr_url(n).map(|(_, n)| n))
                    .ok_or_else(|| anyhow::anyhow!("--from-pr expects a PR number or URL"))?,
            ),
        };
        let repo = match agentvc_core::github::parse_pr_url(pr) {
            Some((r, _)) if repo.is_none() => r,
            _ => agentvc_core::github::repo_for(&cwd, "origin", repo.as_deref())?,
        };
        let (n, payload) = agentvc_core::github::load_space_comment(&cwd, &repo, number)?;
        let (space, r) = agentvc_core::bundle::restore(&cwd, &payload, &format!("{repo}#{n}"))?;
        let prepare = if *no_prepare {
            json!({ "skipped": "--no-prepare" })
        } else {
            prepare_in_background(&space)
        };
        let list = |v: &[i64]| {
            if v.is_empty() {
                "none".to_string()
            } else {
                v.iter()
                    .map(|i| format!("a{i}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        let mut text = format!(
            "Rebuilt space {} from {repo}#{n}: {} tasks, {} attempts, {} snapshots\n\
             Files rebuilt for {}; views written for open attempts: {}",
            r.space_id,
            r.tasks,
            r.attempts,
            r.snapshots,
            list(&r.rebuilt),
            list(&r.views)
        );
        if !r.incomplete.is_empty() {
            text += &format!(
                "\nHistory only (files left out of the comment or base commit missing): {}",
                list(&r.incomplete)
            );
        }
        return Ok(Out {
            text,
            json: json!({ "workdir": space.workdir(), "restore": r, "prepare": prepare }),
        });
    }
    if let Cmd::Init {
        no_prepare,
        from_remote: Some(remote),
        space,
        this_branch,
        ..
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
            pr,
            repo,
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
            let pr = match pr {
                Some(n) => Some((
                    agentvc_core::github::repo_for(space.workdir(), &remote, repo.as_deref())?,
                    n,
                )),
                None => None,
            };
            let r = agentvc_core::github::save_space(
                &space,
                &agentvc_core::SyncOptions {
                    remote: Some(remote),
                    force,
                },
                pr,
                false,
            );
            if !r.saved() {
                let why = |e: &Option<Value>| {
                    e.as_ref()
                        .and_then(|e| e["message"].as_str().map(str::to_string))
                };
                return Err(Error::NotSaved {
                    git: why(&r.git_error).unwrap_or_else(|| "not tried".into()),
                    pr: why(&r.pr_error)
                        .unwrap_or_else(|| "no PR yet (pass --pr, or run agentvc pr)".into()),
                }
                .into());
            }
            let mut out = Out {
                text: String::new(),
                json: json!({ "auto": space.meta(agentvc_core::sync::AUTO_SYNC_KEY)? }),
            };
            add_sync(&mut out, Some(r));
            out.text = out.text.trim_start().to_string();
            if let Some(a) = space.meta(agentvc_core::sync::AUTO_SYNC_KEY)? {
                out.text += &format!("\nauto-sync to {a} is on");
            }
            out
        }
        Cmd::Init { .. }
        | Cmd::Hook { .. }
        | Cmd::Hooks { .. }
        | Cmd::Integrate { .. }
        | Cmd::Mcp { .. } => unreachable!(),
        Cmd::GitHook { event: _ } => {
            let r = space.record_git_commit(&cwd)?;
            Out {
                text: format!(
                    "a{} s{} recorded git commit",
                    r.snapshot.attempt_id, r.snapshot.id
                ),
                json: json!({ "snapshot": r }),
            }
        }
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
            from_worktree: true,
            move_changes,
            actor,
            model,
            prompt,
            backend,
            ..
        } => {
            let req = ForkRequest {
                task_id: task.map(|t| parse_id(&t, 't', "task")).transpose()?,
                from: None,
                actor,
                model,
                prompt,
                backend,
            };
            let r = agentvc_core::bridge::fork_from_worktree(&space, &req, move_changes)?;
            let mut text = format!(
                "a{} holds the checkout's work: {} files on top of HEAD {} (s{})\n{}",
                r.fork.attempt.id,
                r.paths.len(),
                &r.base[..12.min(r.base.len())],
                r.snapshot,
                r.fork.attempt.view_path.display()
            );
            if move_changes {
                text += &format!(
                    "\nmoved {} files out of the checkout; `agentvc apply a{}` brings them back",
                    r.moved.len(),
                    r.fork.attempt.id
                );
            }
            if !r.kept.is_empty() {
                text += &format!(
                    "\nleft in the checkout (agentvc does not capture them): {}",
                    r.kept.join(", ")
                );
            }
            if !r.staged.is_empty() {
                text += &format!(
                    "\nstill staged in the index (untouched): {}",
                    r.staged.join(", ")
                );
            }
            Out {
                text,
                json: json!({ "fork": r.fork, "worktree": {
                    "base": r.base, "snapshot": r.snapshot, "paths": r.paths,
                    "moved": r.moved, "kept": r.kept, "staged": r.staged,
                } }),
            }
        }
        Cmd::Apply {
            attempt,
            paths,
            force,
        } => {
            let id = attempt_id(&attempt)?;
            let r = agentvc_core::bridge::apply(
                &space,
                id,
                &agentvc_core::bridge::ApplyOptions { paths, force },
            )?;
            let mut text = String::new();
            for p in &r.written {
                text += &format!("W  {p}\n");
            }
            for p in &r.deleted {
                text += &format!("D  {p}\n");
            }
            text += &format!(
                "applied a{id} (s{}, against {}) to the checkout: {} written, {} deleted, {} already matched",
                r.snapshot,
                r.base,
                r.written.len(),
                r.deleted.len(),
                r.unchanged.len()
            );
            Out {
                text,
                json: json!({ "apply": r }),
            }
        }
        Cmd::Fork {
            task,
            from,
            actor,
            model,
            prompt,
            backend,
            ..
        } => {
            let from = match from {
                Some(f) => space.parse_ref(&f)?,
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
            auto_gc_in_background(&space);
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
            add_sync(&mut out, agentvc_core::github::auto_save(&space));
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
            add_sync(&mut out, agentvc_core::github::auto_save(&space));
            out
        }
        Cmd::Op(OpCmd::Log { limit, all }) => {
            let ops = space.operations((!all).then_some(limit))?;
            let mut text = String::new();
            for o in &ops {
                let mark = match (o.undone_by, o.undoable) {
                    (Some(by), _) => format!("  [undone by {by}]"),
                    (None, true) => String::new(),
                    (None, false) => "  [not undoable]".into(),
                };
                text += &format!(
                    "{:>4}  {:>8}  {:<18}  {}{}\n",
                    o.id,
                    ago(o.created_at),
                    o.actor,
                    o.summary,
                    mark
                );
            }
            Out {
                text: text.trim_end().to_string(),
                json: json!({ "operations": ops }),
            }
        }
        Cmd::Undo { op, actor } => {
            let op = op
                .map(|s| {
                    s.strip_prefix("op")
                        .unwrap_or(&s)
                        .parse::<i64>()
                        .map_err(|_| {
                            anyhow::Error::from(Error::NotFound {
                                kind: "operation",
                                id: s.clone(),
                            })
                        })
                })
                .transpose()?;
            let r = space.undo(&actor, op)?;
            let mut text = format!(
                "undid op {} ({}) as op {}",
                r.undid.id, r.undid.summary, r.op
            );
            for line in agentvc_core::undo::describe(&r.changes) {
                text += &format!("\n  {line}");
            }
            for w in &r.warnings {
                text += &format!("\nwarning: {w}");
            }
            text += &format!("\nredo with: agentvc undo {}", r.op);
            let mut out = Out {
                text,
                json: json!({ "op": r.op, "undid": r.undid, "changes": r.changes, "warnings": r.warnings }),
            };
            add_sync(&mut out, agentvc_core::github::auto_save(&space));
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
        Cmd::Tag {
            name,
            target,
            note,
            list,
            delete,
            actor,
        } => {
            if list {
                let tags = space.tags()?;
                let mut text = String::new();
                for t in &tags {
                    text += &format!(
                        "{}  s{}  a{}{}\n",
                        t.name,
                        t.snapshot_id,
                        t.attempt_id,
                        t.note
                            .as_ref()
                            .map(|n| format!("  {n}"))
                            .unwrap_or_default()
                    );
                }
                if tags.is_empty() {
                    text = "no tags".into();
                }
                Out {
                    text,
                    json: json!({ "tags": tags }),
                }
            } else if delete {
                let t = space.delete_tag(name.as_deref().expect("clap"), &actor)?;
                Out {
                    text: format!("deleted tag {} (was s{})", t.name, t.snapshot_id),
                    json: json!({ "deleted": t }),
                }
            } else {
                let target = match target {
                    Some(t) => space.parse_ref(&t)?,
                    None => Ref::Attempt(current()?),
                };
                let t = space.create_tag(
                    name.as_deref().expect("clap"),
                    target,
                    note.as_deref(),
                    &actor,
                )?;
                Out {
                    text: format!("{} -> s{} (a{})", t.name, t.snapshot_id, t.attempt_id),
                    json: json!({ "tag": t }),
                }
            }
        }
        Cmd::Show { attempt } => {
            let id = match space.parse_ref(&attempt)? {
                Ref::Attempt(id) => id,
                Ref::Snapshot(s) => space.snapshot_by_id(s)?.attempt_id,
            };
            let a = space.attempt(id)?;
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
                    "  s{}  {}{}  {}{}{}\n",
                    s.id,
                    &s.tree[..12],
                    if s.pruned_at.is_some() {
                        " (tree pruned)"
                    } else {
                        ""
                    },
                    s.trigger,
                    s.tags.iter().map(|t| format!("  #{t}")).collect::<String>(),
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
                    let (ra, rb) = (space.parse_ref(&a)?, space.parse_ref(&b)?);
                    let ta = space.resolve(ra)?.tree_oid()?.to_hex();
                    let tb = space.resolve(rb)?.tree_oid()?.to_hex();
                    (ra.to_string(), ta, rb.to_string(), tb)
                }
                (Some(a), None) => {
                    let Ref::Attempt(id) = space.parse_ref(&a)? else {
                        anyhow::bail!("diff with one argument needs an attempt (aN)");
                    };
                    let (from, from_tree) = attempt_base(&space, id)?
                        .ok_or_else(|| anyhow::anyhow!("a{id} has no parent; pass two refs"))?;
                    let to_tree = space.head_snapshot(id)?.tree_oid()?.to_hex();
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
                Some(f) => space.parse_ref(&f)?,
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
                add_sync(&mut res, agentvc_core::github::auto_save(&space));
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
            add_sync(&mut res, agentvc_core::github::auto_save(&space));
            res
        }
        Cmd::Ps { all } => {
            let runs = space.runs(!all)?;
            let mut text = String::new();
            for r in &runs {
                let state = match (r.exit_code, r.alive, r.pid) {
                    (Some(c), _, _) => format!("exit {c}"),
                    (None, Some(true), _) => "running".to_string(),
                    // Finished without an exit code: `doctor --repair`
                    // found its process gone.
                    (None, _, _) if r.finished_at.is_some() => "lost".to_string(),
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
        Cmd::Doctor { repair, offline } => doctor_cmd(&space, repair, offline)?,
        Cmd::Gc {
            dry_run,
            auto,
            keep_abandoned_days,
            keep_views_days,
            prune_now,
            no_pack,
        } => gc_cmd(
            &space,
            &agentvc_core::gc::GcOptions {
                dry_run,
                auto,
                keep_abandoned_days,
                keep_views_days,
                prune_expire: prune_now.then_some(std::time::Duration::ZERO),
                no_pack,
            },
        )?,
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
        Cmd::Take {
            source,
            into,
            paths,
            hunks,
            keep_conflicts,
        } => {
            let into = match into {
                Some(a) => attempt_id(&a)?,
                None => current()?,
            };
            let opts = agentvc_core::combine::TakeOptions {
                paths,
                hunks,
                keep_conflicts,
                actor: None,
            };
            take_out(agentvc_core::combine::take(
                &space,
                into,
                attempt_id(&source)?,
                &opts,
            )?)
        }
        Cmd::Merge {
            source,
            into,
            keep_conflicts,
        } => {
            let into = match into {
                Some(a) => attempt_id(&a)?,
                None => current()?,
            };
            let opts = agentvc_core::combine::TakeOptions {
                keep_conflicts,
                ..Default::default()
            };
            take_out(agentvc_core::combine::take(
                &space,
                into,
                attempt_id(&source)?,
                &opts,
            )?)
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
                agentvc_core::github::save_space(
                    &space,
                    &agentvc_core::SyncOptions {
                        remote: Some(remote),
                        force: false,
                    },
                    Some((r.repo.clone(), r.number)),
                    false,
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

fn take_out(r: agentvc_core::combine::TakeResult) -> Out {
    let mut t = match &r.snapshot {
        None => format!(
            "a{} already has {} from a{}; nothing changed",
            r.into,
            if r.mode == "merge" {
                "everything"
            } else {
                "that"
            },
            r.source
        ),
        Some(s) => format!(
            "{} a{} (s{}) into a{}: {} files taken, {} changed in the view, snapshot s{}",
            if r.mode == "merge" {
                "merged"
            } else {
                "took from"
            },
            r.source,
            r.source_snapshot,
            r.into,
            r.taken.len(),
            r.changes.len(),
            s.id
        ),
    };
    for c in &r.taken {
        let hunks = r
            .hunks
            .get(&c.path)
            .map(|n| {
                let n: Vec<String> = n.iter().map(usize::to_string).collect();
                format!("  (hunks {})", n.join(","))
            })
            .unwrap_or_default();
        t += &format!("\n  {}  {}{hunks}", change_mark(c.kind), c.path);
    }
    if !r.conflicts.is_empty() {
        t += &format!(
            "\n{} files have conflict markers; fix them, then `agentvc snapshot a{}`:",
            r.conflicts.len(),
            r.into
        );
        for p in &r.conflicts {
            t += &format!("\n  C  {p}");
        }
    }
    Out {
        text: t,
        json: json!({ "take": r }),
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

/// Report where a command saved the space: lines of text and `sync` in the
/// JSON. A failed save is a warning, never the command's failure.
fn add_sync(out: &mut Out, sync: Option<agentvc_core::github::SaveResult>) {
    let Some(r) = sync else { return };
    if let Some(g) = &r.git {
        out.text += &format!(
            "\n{} space {} to {} {} ({})",
            if g.unchanged {
                "already synced"
            } else {
                "synced"
            },
            g.space_id,
            g.remote,
            g.refname,
            &g.commit[..12]
        );
    }
    if let Some(c) = &r.pr {
        out.text += &format!(
            "\nsaved space {} in {}#{} ({} attempts, {} with their files{})",
            c.bundle.space_id,
            c.repo,
            c.pr,
            c.bundle.attempts,
            c.bundle.patches,
            if c.bundle.omitted.is_empty() {
                String::new()
            } else {
                format!("; files left out for size: {}", c.bundle.omitted.join(", "))
            }
        );
    }
    if !r.saved() {
        let msg = |e: &Option<Value>| {
            e.as_ref()
                .and_then(|e| e["message"].as_str().map(str::to_string))
        };
        if let Some(m) = msg(&r.git_error).or_else(|| msg(&r.pr_error)) {
            out.text += &format!("\nwarning: could not save the space: {m}; run `agentvc sync`");
        }
    } else if let Some(m) = r.pr_error.as_ref().and_then(|e| e["message"].as_str()) {
        out.text += &format!("\nwarning: could not update the PR's space comment: {m}");
    }
    out.json["sync"] = json!(r);
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
    for t in &a.tags {
        s += &format!("  #{t}");
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
    } else if let Ok(space) = Space::discover(cwd) {
        auto_gc_in_background(&space);
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

/// Start `agentvc gc --auto` in the background when there are enough loose
/// objects (one directory listing when there are not). Off with
/// `agentvc config gc.auto 0`.
fn auto_gc_in_background(space: &Space) {
    if !agentvc_core::gc::auto_needed(space) {
        return;
    }
    let start = || -> std::io::Result<()> {
        let log = space
            .workdir()
            .join(agentvc_core::space::SPACE_DIR)
            .join("gc.log");
        let f = std::fs::File::create(&log)?;
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.arg("-C")
            .arg(space.workdir())
            .args(["gc", "--auto"])
            .stdin(Stdio::null())
            .stdout(f.try_clone()?)
            .stderr(f);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd.spawn()?;
        Ok(())
    };
    let _ = start();
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn doctor_cmd(space: &Space, repair: bool, offline: bool) -> anyhow::Result<Out> {
    let r = agentvc_core::doctor::doctor(
        space,
        &agentvc_core::doctor::DoctorOptions { repair, offline },
    )?;
    let mut text = String::new();
    for f in &r.findings {
        let sev = match f.severity {
            agentvc_core::doctor::Severity::Error => "error",
            agentvc_core::doctor::Severity::Warning => "warning",
            agentvc_core::doctor::Severity::Info => "info",
        };
        let tag = if f.repaired {
            "  [repaired]"
        } else if f.repairable && !repair {
            "  [--repair fixes]"
        } else {
            ""
        };
        text += &format!("{sev:<7}  {}  {}{tag}\n", f.code, f.message);
    }
    text += &format!(
        "checked {} attempts, {} snapshots, {} objects, {} active runs: ",
        r.checked.attempts, r.checked.snapshots, r.checked.objects, r.checked.runs
    );
    text += &match (r.errors, r.warnings, r.repaired) {
        (0, 0, 0) => "no problems".to_string(),
        (e, w, fixed) => {
            let mut parts = vec![format!("{e} errors"), format!("{w} warnings")];
            if fixed > 0 {
                parts.push(format!("{fixed} repaired"));
            }
            parts.join(", ")
        }
    };
    if r.errors > 0 {
        EXIT_FAILURE.store(true, Ordering::Relaxed);
    }
    Ok(Out {
        text,
        json: json!({ "doctor": r }),
    })
}

fn gc_cmd(space: &Space, opts: &agentvc_core::gc::GcOptions) -> anyhow::Result<Out> {
    let r = agentvc_core::gc::gc(space, opts)?;
    let mut text = String::new();
    if let Some(why) = &r.skipped {
        text = format!("gc: nothing to do ({why})");
        return Ok(Out {
            text,
            json: json!({ "gc": r }),
        });
    }
    let would = if r.dry_run { "would " } else { "" };
    let stats = |s: &agentvc_core::gc::ObjectStats| {
        format!(
            "{} loose ({}), {} packed in {} packs ({})",
            s.loose,
            human_bytes(s.loose_bytes),
            s.packed,
            s.packs,
            human_bytes(s.pack_bytes)
        )
    };
    text += &format!("objects before: {}\n", stats(&r.before));
    if !r.dry_run {
        text += &format!("objects after:  {}\n", stats(&r.after));
    }
    if !r.pruned_snapshots.is_empty() {
        let mut list: Vec<String> = r
            .pruned_snapshots
            .iter()
            .take(10)
            .map(|s| format!("s{s}"))
            .collect();
        if r.pruned_snapshots.len() > 10 {
            list.push(format!("and {} more", r.pruned_snapshots.len() - 10));
        }
        text += &format!(
            "{would}prune trees of {} snapshots: {}\n",
            r.pruned_snapshots.len(),
            list.join(" ")
        );
    }
    if r.kept_unsynced > 0 {
        text += &format!(
            "kept {} expired snapshots the last sync did not carry (run `agentvc sync`)\n",
            r.kept_unsynced
        );
    }
    for v in &r.views {
        text += &format!(
            "{would}remove view of a{} ({})\n",
            v.attempt,
            human_bytes(v.bytes)
        );
    }
    for id in &r.views_kept {
        text += &format!("kept view of a{id} (busy or running)\n");
    }
    #[cfg(target_os = "linux")]
    for l in &r.layers {
        text += &format!(
            "{would}remove layer L{} of a{} ({})\n",
            l.layer,
            l.source_attempt,
            human_bytes(l.bytes)
        );
    }
    text += &format!(
        "{would}drop {} unreferenced objects, {}pack {}; {} {}freed in {} ms\n",
        r.objects_removed,
        would,
        r.objects_packed,
        human_bytes(r.bytes_freed),
        if r.dry_run { "would be " } else { "" },
        r.millis
    );
    for n in &r.notes {
        text += &format!("note: {n}\n");
    }
    Ok(Out {
        text,
        json: json!({ "gc": r }),
    })
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
