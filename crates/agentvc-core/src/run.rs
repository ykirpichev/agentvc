//! Running an agent (or any command) inside an attempt's view and recording
//! what happened: exit code, duration, an optional check command, and a
//! snapshot of the view when it finishes.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::error::{Error, IoContext, Result};
use crate::space::{now, Attempt, Snapshot, Space, Status, SPACE_DIR};

/// One execution of a command in an attempt's view.
#[derive(Clone, Debug, Serialize)]
pub struct Run {
    pub id: i64,
    pub attempt_id: i64,
    pub command: Vec<String>,
    pub check_command: Option<String>,
    pub pid: Option<i64>,
    pub log_path: Option<PathBuf>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub check_exit_code: Option<i32>,
    pub duration_ms: Option<i64>,
    /// For unfinished runs: whether the recorded process is still alive.
    pub alive: Option<bool>,
}

/// The result of [`execute`].
#[derive(Clone, Debug, Serialize)]
pub struct RunOutcome {
    pub run: Run,
    pub attempt: Attempt,
    pub snapshot: Snapshot,
    /// For Claude Code commands: how many snapshots its hooks recorded.
    pub hook_snapshots: Option<i64>,
    /// Things that went quietly wrong, such as hooks that never fired.
    pub warnings: Vec<String>,
}

/// Settings file, inside the space, that `agentvc run` passes to Claude Code
/// with `--settings` so its hooks fire where `settings.local.json` is ignored.
pub const CLAUDE_HOOKS_FILE: &str = "claude-hooks.json";

/// What [`with_claude_hooks`] decided for a command.
#[derive(Clone, Debug, Serialize)]
pub struct HookInjection {
    /// The command to run, with `--settings <file>` added when `settings` is set.
    pub command: Vec<String>,
    /// The settings file that was added, if any.
    pub settings: Option<PathBuf>,
    /// Why nothing was added, for Claude Code commands that were left alone.
    pub skipped: Option<String>,
}

/// Whether `program` starts Claude Code.
pub fn is_claude_code(program: &str) -> bool {
    Path::new(program)
        .file_name()
        .is_some_and(|n| n == "claude" || n == "claude.exe")
}

/// Whether this process runs inside a Claude Code cloud session, which only
/// reads `.claude/settings.json` (not `settings.local.json`).
pub fn in_remote_claude_session() -> bool {
    std::env::var_os("CLAUDE_CODE_REMOTE").is_some_and(|v| !v.is_empty() && v != "false")
}

/// Make sure a Claude Code command loads agentvc's hooks.
///
/// Claude Code sessions in the cloud ignore `.claude/settings.local.json`,
/// where `agentvc hooks` installs them, so the attempt would only get its
/// `run-exit` snapshot. For a `claude` command this writes the hooks into
/// `.agentvc/claude-hooks.json` and adds `--settings <that file>`, unless
/// the command already passes `--settings` or `--plugin-dir`, or (outside a
/// cloud session) the view's own settings already install the hooks.
/// Other commands are returned unchanged.
pub fn with_claude_hooks(
    space: &Space,
    view: &Path,
    command: &[String],
    exe: &Path,
    remote: bool,
) -> Result<HookInjection> {
    let unchanged = |why: Option<String>| HookInjection {
        command: command.to_vec(),
        settings: None,
        skipped: why,
    };
    let Some((program, args)) = command.split_first() else {
        return Ok(unchanged(None));
    };
    if !is_claude_code(program) {
        return Ok(unchanged(None));
    }
    for flag in ["--settings", "--plugin-dir"] {
        if args
            .iter()
            .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
        {
            return Ok(unchanged(Some(format!("the command passes {flag}"))));
        }
    }
    if !remote {
        for f in [".claude/settings.local.json", ".claude/settings.json"] {
            if crate::hooks::installed_in(&view.join(f)) {
                return Ok(unchanged(Some(format!("hooks are installed in {f}"))));
            }
        }
    }
    let path = space.workdir().join(SPACE_DIR).join(CLAUDE_HOOKS_FILE);
    crate::hooks::write_standalone(&path, exe)?;
    let mut cmd = vec![program.clone(), "--settings".into()];
    cmd.push(path.to_string_lossy().into_owned());
    cmd.extend(args.iter().cloned());
    Ok(HookInjection {
        command: cmd,
        settings: Some(path),
        skipped: None,
    })
}

/// Environment variables set for the command, so agents and hooks can find
/// their attempt without parsing paths.
pub const ENV_ATTEMPT: &str = "AGENTVC_ATTEMPT";
pub const ENV_VIEW: &str = "AGENTVC_VIEW";
pub const ENV_SPACE: &str = "AGENTVC_SPACE";
pub const ENV_RUN: &str = "AGENTVC_RUN";

const RUN_COLS: &str = "id, attempt_id, command, check_command, pid, log_path, started_at, \
     finished_at, exit_code, check_exit_code, duration_ms";

fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    let command: String = r.get(2)?;
    let log: Option<String> = r.get(5)?;
    Ok(Run {
        id: r.get(0)?,
        attempt_id: r.get(1)?,
        command: serde_json::from_str(&command).unwrap_or_default(),
        check_command: r.get(3)?,
        pid: r.get(4)?,
        log_path: log.map(PathBuf::from),
        started_at: r.get(6)?,
        finished_at: r.get(7)?,
        exit_code: r.get(8)?,
        check_exit_code: r.get(9)?,
        duration_ms: r.get(10)?,
        alive: None,
    })
}

/// Whether a process with this pid exists.
#[cfg(unix)]
pub fn pid_alive(pid: i64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // Signal 0 checks for existence without sending anything.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub fn pid_alive(_pid: i64) -> bool {
    false
}

impl Space {
    /// Register a run for an attempt. Output goes to
    /// `.agentvc/runs/r<id>.log` when `log_to_file` is set.
    pub fn create_run(
        &self,
        attempt_id: i64,
        command: &[String],
        check: Option<&str>,
        log_to_file: bool,
    ) -> Result<Run> {
        self.attempt(attempt_id)?;
        self.db.execute(
            "INSERT INTO runs (attempt_id, command, check_command, started_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                attempt_id,
                serde_json::to_string(command).expect("strings serialize"),
                check,
                now()
            ],
        )?;
        let id = self.db.last_insert_rowid();
        if log_to_file {
            let dir = self.workdir().join(SPACE_DIR).join("runs");
            fs::create_dir_all(&dir).at(&dir)?;
            let log = dir.join(format!("r{id}.log"));
            self.db.execute(
                "UPDATE runs SET log_path = ?1 WHERE id = ?2",
                params![log.to_string_lossy(), id],
            )?;
        }
        self.run(id)
    }

    pub fn run(&self, id: i64) -> Result<Run> {
        self.db
            .query_row(
                &format!("SELECT {RUN_COLS} FROM runs WHERE id = ?1"),
                params![id],
                row_to_run,
            )
            .optional()?
            .ok_or(Error::NotFound {
                kind: "run",
                id: format!("r{id}"),
            })
    }

    /// Runs in creation order; `active_only` keeps unfinished ones and
    /// reports whether their process is still alive.
    pub fn runs(&self, active_only: bool) -> Result<Vec<Run>> {
        let filter = if active_only {
            "WHERE finished_at IS NULL"
        } else {
            ""
        };
        let mut stmt = self
            .db
            .prepare(&format!("SELECT {RUN_COLS} FROM runs {filter} ORDER BY id"))?;
        let rows = stmt.query_map([], row_to_run)?;
        let mut runs: Vec<Run> = rows.collect::<std::result::Result<_, _>>()?;
        for r in &mut runs {
            if r.finished_at.is_none() {
                r.alive = Some(r.pid.is_some_and(pid_alive));
            }
        }
        Ok(runs)
    }

    pub(crate) fn set_run_pid(&self, id: i64, pid: u32) -> Result<()> {
        self.db.execute(
            "UPDATE runs SET pid = ?1 WHERE id = ?2",
            params![pid as i64, id],
        )?;
        Ok(())
    }

    fn finish_run(
        &self,
        id: i64,
        exit_code: i32,
        check_exit_code: Option<i32>,
        duration_ms: i64,
    ) -> Result<()> {
        self.db.execute(
            "UPDATE runs SET finished_at = ?1, exit_code = ?2, check_exit_code = ?3,
                             duration_ms = ?4 WHERE id = ?5",
            params![now(), exit_code, check_exit_code, duration_ms, id],
        )?;
        Ok(())
    }
}

fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    if let Some(c) = status.code() {
        return c;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    -1
}

/// Run a registered run to completion in the foreground.
///
/// Output goes to the run's log file if it has one, otherwise it is
/// inherited from this process. When the command finishes, the view is
/// snapshotted with trigger `run-exit` and the outcome:
///
/// - with a check command: `passed` if the check exits 0, else `failed`;
/// - without one: `failed` if the command exits non-zero, else the attempt
///   stays `exploring` for a human or a later check to judge.
///
/// A command that cannot be started is recorded as exit code 127 and the
/// attempt is marked failed.
pub fn execute(space: &Space, run_id: i64) -> Result<RunOutcome> {
    let run = space.run(run_id)?;
    let attempt = space.attempt(run.attempt_id)?;
    let view = attempt.view_path.clone();
    // A detached worker is a new process: mount an overlay view again here.
    space.ensure_view(&attempt)?;
    let stdio = || -> Result<(Stdio, Stdio)> {
        match &run.log_path {
            Some(p) => {
                let f = OpenOptions::new().create(true).append(true).open(p).at(p)?;
                let g = f.try_clone().at(p)?;
                Ok((Stdio::from(f), Stdio::from(g)))
            }
            None => Ok((Stdio::inherit(), Stdio::inherit())),
        }
    };

    let start = Instant::now();
    let (out, err) = stdio()?;
    let (program, args) = run
        .command
        .split_first()
        .ok_or_else(|| Error::BadRef("empty command".into()))?;
    let spawned = Command::new(program)
        .args(args)
        .current_dir(&view)
        .env(ENV_ATTEMPT, format!("a{}", attempt.id))
        .env(ENV_VIEW, &view)
        .env(ENV_SPACE, space.workdir())
        .env(ENV_RUN, format!("r{run_id}"))
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn();
    let (exit_code, spawn_error) = match spawned {
        Ok(mut child) => {
            space.set_run_pid(run_id, child.id())?;
            let status = child.wait().at(&view)?;
            (exit_code_of(status), None)
        }
        Err(e) => (127, Some(e)),
    };

    let check_exit_code = match (&run.check_command, &spawn_error) {
        (Some(check), None) => {
            let (out, err) = stdio()?;
            let status = Command::new("sh")
                .arg("-c")
                .arg(check)
                .current_dir(&view)
                .env(ENV_ATTEMPT, format!("a{}", attempt.id))
                .env(ENV_VIEW, &view)
                .stdin(Stdio::null())
                .stdout(out)
                .stderr(err)
                .status()
                .at(&view)?;
            Some(exit_code_of(status))
        }
        _ => None,
    };
    let duration_ms = start.elapsed().as_millis() as i64;
    let meta = serde_json::json!({
        "run": run_id,
        "command": run.command,
        "exit_code": exit_code,
        "check_exit_code": check_exit_code,
        "duration_ms": duration_ms,
    });
    let snapshot = space
        .snapshot(attempt.id, "run-exit", Some(meta.clone()))?
        .snapshot;
    // Finish only after the final snapshot, so a run that `ps` no longer
    // lists always has its run-exit snapshot.
    space.finish_run(run_id, exit_code, check_exit_code, duration_ms)?;
    space.record(&attempt.actor, "run.finish", meta)?;

    let verdict = match (check_exit_code, exit_code) {
        (Some(0), _) => Some((Status::Passed, None)),
        (Some(c), _) => Some((Status::Failed, Some(format!("check exited {c}")))),
        (None, 0) => None,
        (None, c) => Some((Status::Failed, Some(format!("command exited {c}")))),
    };
    let attempt = match verdict {
        Some((status, reason)) => space.set_status(attempt.id, status, reason.as_deref())?,
        None => space.attempt(attempt.id)?,
    };

    let mut warnings = Vec::new();
    let hook_snapshots = if is_claude_code(program) && spawn_error.is_none() {
        let n: i64 = space.db.query_row(
            "SELECT COUNT(*) FROM snapshots
             WHERE attempt_id = ?1 AND json_extract(meta, '$.hook') IS NOT NULL",
            params![attempt.id],
            |r| r.get(0),
        )?;
        if n == 0 {
            warnings.push(format!(
                "Claude Code ran in a{} but its hooks recorded no snapshots, so the \
                 attempt has no per-tool-call history. Let `agentvc run` add them (drop \
                 --no-hooks), or pass claude `--settings` with a file that has agentvc's \
                 hooks (see `agentvc hooks`)",
                attempt.id,
            ));
        }
        Some(n)
    } else {
        None
    };
    if let Some(p) = &run.log_path {
        if let Ok(mut f) = OpenOptions::new().append(true).open(p) {
            for w in &warnings {
                let _ = writeln!(f, "[agentvc] warning: {w}");
            }
        }
    }

    if let Some(e) = spawn_error {
        return Err(Error::Spawn {
            program: program.clone(),
            source: e,
        });
    }
    Ok(RunOutcome {
        run: space.run(run_id)?,
        attempt,
        snapshot,
        hook_snapshots,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::ForkRequest;

    fn space_with_attempt() -> (crate::testutil::TestDir, Space, i64) {
        let tmp = crate::testutil::tempdir();
        fs::write(tmp.path().join("a.txt"), "one\n").unwrap();
        let space = Space::init(tmp.path()).unwrap();
        let a = space
            .fork(&ForkRequest {
                actor: "agent:test".into(),
                ..Default::default()
            })
            .unwrap()
            .attempt
            .id;
        (tmp, space, a)
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn success_snapshots_and_stays_exploring() {
        let (_tmp, space, a) = space_with_attempt();
        let run = space
            .create_run(
                a,
                &sh("echo two > a.txt; echo $AGENTVC_ATTEMPT > who"),
                None,
                true,
            )
            .unwrap();
        let out = execute(&space, run.id).unwrap();
        assert_eq!(out.run.exit_code, Some(0));
        assert!(out.run.finished_at.is_some());
        assert_eq!(out.attempt.status, Status::Exploring);
        assert_eq!(out.snapshot.trigger, "run-exit");
        let view = &out.attempt.view_path;
        assert_eq!(fs::read_to_string(view.join("a.txt")).unwrap(), "two\n");
        assert_eq!(
            fs::read_to_string(view.join("who")).unwrap(),
            format!("a{a}\n")
        );
        assert_eq!(out.snapshot.meta.unwrap()["exit_code"], 0);
    }

    #[test]
    fn check_decides_status() {
        let (_tmp, space, a) = space_with_attempt();
        let run = space
            .create_run(a, &sh("echo ok > a.txt"), Some("grep -q ok a.txt"), true)
            .unwrap();
        assert_eq!(
            execute(&space, run.id).unwrap().attempt.status,
            Status::Passed
        );

        let (_tmp, space, b) = space_with_attempt();
        let run = space
            .create_run(b, &sh("true"), Some("grep -q ok a.txt"), true)
            .unwrap();
        let out = execute(&space, run.id).unwrap();
        assert_eq!(out.attempt.status, Status::Failed);
        assert_eq!(out.attempt.reason.as_deref(), Some("check exited 1"));
    }

    #[test]
    fn failing_command_marks_failed_and_logs_output() {
        let (_tmp, space, a) = space_with_attempt();
        let run = space
            .create_run(a, &sh("echo boom >&2; exit 3"), None, true)
            .unwrap();
        let out = execute(&space, run.id).unwrap();
        assert_eq!(out.run.exit_code, Some(3));
        assert_eq!(out.attempt.status, Status::Failed);
        let log = fs::read_to_string(out.run.log_path.unwrap()).unwrap();
        assert!(log.contains("boom"));
    }

    #[test]
    fn missing_program_is_recorded_as_failed() {
        let (_tmp, space, a) = space_with_attempt();
        let run = space
            .create_run(a, &["definitely-not-a-program-xyz".to_string()], None, true)
            .unwrap();
        let err = execute(&space, run.id).unwrap_err();
        assert_eq!(err.code(), "spawn_failed");
        assert_eq!(space.run(run.id).unwrap().exit_code, Some(127));
        assert_eq!(space.attempt(a).unwrap().status, Status::Failed);
    }

    /// A fake `claude` that records its arguments in `$AGENTVC_VIEW/args`.
    fn fake_claude(dir: &Path) -> PathBuf {
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let path = bin.join("claude");
        fs::write(
            &path,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$AGENTVC_VIEW/args\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn claude_commands_get_the_hooks_settings() {
        let (tmp, space, a) = space_with_attempt();
        let view = space.attempt(a).unwrap().view_path;
        let claude = fake_claude(tmp.path()).to_string_lossy().into_owned();
        let exe = Path::new("/opt/agentvc");
        let cmd = vec![claude.clone(), "-p".into(), "hi".into()];

        let inj = with_claude_hooks(&space, &view, &cmd, exe, true).unwrap();
        let settings = inj.settings.clone().expect("settings added");
        assert_eq!(inj.command[1], "--settings");
        assert_eq!(inj.command[2], settings.to_string_lossy());
        assert_eq!(&inj.command[3..], &cmd[1..]);
        assert!(crate::hooks::installed_in(&settings));

        // Other commands, explicit --settings and --plugin-dir are left alone.
        let other = with_claude_hooks(&space, &view, &sh("true"), exe, true).unwrap();
        assert_eq!(other.command, sh("true"));
        assert!(other.skipped.is_none());
        let own = vec![claude.clone(), "--settings=x.json".into()];
        let r = with_claude_hooks(&space, &view, &own, exe, true).unwrap();
        assert_eq!(r.command, own);
        assert!(r.skipped.unwrap().contains("--settings"));

        // Locally, hooks installed in the view's settings are enough...
        crate::hooks::install(&view.join(".claude/settings.local.json"), exe).unwrap();
        let r = with_claude_hooks(&space, &view, &cmd, exe, false).unwrap();
        assert_eq!(r.command, cmd);
        assert!(r.skipped.unwrap().contains("settings.local.json"));
        // ...but a cloud session ignores that file.
        let r = with_claude_hooks(&space, &view, &cmd, exe, true).unwrap();
        assert!(r.settings.is_some());

        // The fake claude saw the flag.
        let run = space.create_run(a, &inj.command, None, true).unwrap();
        let out = execute(&space, run.id).unwrap();
        let args = fs::read_to_string(view.join("args")).unwrap();
        assert!(args.starts_with("--settings\n"), "{args}");
        // It never ran the hook, so the run warns.
        assert_eq!(out.hook_snapshots, Some(0));
        assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
        let log = fs::read_to_string(out.run.log_path.unwrap()).unwrap();
        assert!(log.contains("warning: Claude Code ran"), "{log}");
    }

    #[test]
    fn no_warning_when_hooks_snapshot_or_command_is_not_claude() {
        let (_tmp, space, a) = space_with_attempt();
        let run = space.create_run(a, &sh("true"), None, true).unwrap();
        let out = execute(&space, run.id).unwrap();
        assert_eq!(out.hook_snapshots, None);
        assert!(out.warnings.is_empty());

        // A hook snapshot during the run silences the warning.
        let (tmp, space, a) = space_with_attempt();
        let claude = fake_claude(tmp.path()).to_string_lossy().into_owned();
        space
            .snapshot_with(a, "stop", Some(serde_json::json!({ "hook": "stop" })), true)
            .unwrap();
        let run = space.create_run(a, &[claude], None, true).unwrap();
        let out = execute(&space, run.id).unwrap();
        assert_eq!(out.hook_snapshots, Some(1));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn runs_lists_active() {
        let (_tmp, space, a) = space_with_attempt();
        let r = space.create_run(a, &sh("true"), None, false).unwrap();
        let active = space.runs(true).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].alive, Some(false));
        execute(&space, r.id).unwrap();
        assert!(space.runs(true).unwrap().is_empty());
        assert_eq!(space.runs(false).unwrap().len(), 1);
    }
}
