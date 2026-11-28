//! `agentvcd`: serve the review UI and JSON API for one agentvc space.
//!
//! Binds to 127.0.0.1 only. Prints one JSON line with `--json` once it is
//! listening, so scripts and agents can read the URL. Changes through the API
//! need the token in `$AGENTVC_HOME/agentvcd.token` (#199); the review UI's
//! pages carry it.
//!
//! Plain `agentvcd` runs in the foreground. `agentvcd start` runs the same
//! daemon in the background, `stop` and `status` find it by its pidfile
//! (#221, see [`agentvcd::daemon`]).

use std::ffi::OsString;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use agentvcd::daemon::{self, Paths, PidInfo};
use agentvcd::{AppState, Config, DEFAULT_PORT};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(
    name = "agentvcd",
    version,
    about = "Local daemon for the agentvc review UI",
    long_about = "Local daemon for the agentvc review UI.\n\nWithout a command it serves \
                  the space found from the current folder in the foreground (Ctrl+C \
                  stops it). `agentvcd start` runs it in the background instead.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    serve: Serve,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the daemon in the background for the space found from the
    /// current folder (same options as plain `agentvcd`); returns once it
    /// listens. Its output goes to a log file (see `agentvcd status`).
    Start(StartArgs),
    /// Stop the background daemon of this space.
    Stop(StopArgs),
    /// Whether this space's background daemon runs: pid, URL, log file.
    Status(StatusArgs),
}

#[derive(Args)]
struct StartArgs {
    #[command(flatten)]
    serve: Serve,
    /// How long to wait for the daemon to listen, in seconds.
    #[arg(long, default_value_t = 60, value_name = "SECS")]
    wait_secs: u64,
}

#[derive(Args)]
struct StopArgs {
    /// Stop the daemon of the space containing this directory.
    #[arg(short = 'C', long = "dir", value_name = "DIR", default_value = ".")]
    dir: PathBuf,
    /// Seconds to wait after asking it to stop (SIGTERM) before killing it.
    #[arg(long, default_value_t = 10, value_name = "SECS")]
    timeout_secs: u64,
    /// Print machine-readable JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct StatusArgs {
    /// Look for the daemon of the space containing this directory.
    #[arg(short = 'C', long = "dir", value_name = "DIR", default_value = ".")]
    dir: PathBuf,
    /// Print machine-readable JSON instead of text.
    #[arg(long)]
    json: bool,
}

/// How to serve: the options of plain `agentvcd` and `agentvcd start`.
#[derive(Args, Clone)]
struct Serve {
    /// Port on 127.0.0.1. Use 0 to pick a free one.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    /// Serve the space containing this directory.
    #[arg(short = 'C', long = "dir", value_name = "DIR", default_value = ".")]
    dir: PathBuf,
    /// How often to check for new snapshots and status changes, in ms.
    #[arg(long, default_value_t = 500, value_name = "MS")]
    poll_ms: u64,
    /// How often to refresh open PRs' checks, reviews and comments from
    /// GitHub (`agentvc pr --refresh`), in seconds; 0 turns it off. Needs a
    /// GitHub token.
    #[arg(long, default_value_t = 300, value_name = "SECS")]
    pr_poll_secs: u64,
    /// Print machine-readable JSON instead of text.
    #[arg(long)]
    json: bool,
    /// Fail in a git repository that has no agentvc space yet, instead of
    /// creating one (`agentvc init --no-prepare`; under a second, git-ignored
    /// files are skipped).
    #[arg(long)]
    no_init: bool,
    /// Reviewer name recorded on actions taken in the UI, as `human:<name>`.
    /// Defaults to `git config user.name`, then `$USER`.
    #[arg(long, value_name = "NAME")]
    user: Option<String>,
    // The hosted service is paused, so the --cloud options are hidden from
    // --help; they still work for a self-hosted agentvc-cloud (deploy/).
    /// Also serve this space through the hosted review UI at URL
    /// (agentvc-cloud). The daemon dials out; nothing listens publicly. The
    /// machine token comes from $AGENTVC_CLOUD_TOKEN or --cloud-token-file.
    #[arg(long, value_name = "URL", env = "AGENTVC_CLOUD_URL", hide = true)]
    cloud: Option<String>,
    /// File holding the machine token for --cloud.
    #[arg(long, value_name = "PATH", requires = "cloud", hide = true)]
    cloud_token_file: Option<PathBuf>,
    /// Only answer reads from the hosted UI; review actions stay local.
    #[arg(long, requires = "cloud", hide = true)]
    cloud_read_only: bool,
    /// Let the hosted page start, answer and land agents on this machine
    /// (they run code here). Off by default: relayed calls can only read
    /// agents.
    #[arg(long, requires = "cloud", hide = true)]
    allow_remote_agents: bool,
    /// Write a pidfile here once listening, removed on shutdown (set by
    /// `agentvcd start`).
    #[arg(long, value_name = "PATH", hide = true)]
    pidfile: Option<PathBuf>,
}

/// The machine token for `--cloud`: the file, else `$AGENTVC_CLOUD_TOKEN`.
fn cloud_token(file: Option<&std::path::Path>) -> Result<String, String> {
    let token = match file {
        Some(p) => {
            std::fs::read_to_string(p).map_err(|e| format!("cannot read {}: {e}", p.display()))?
        }
        None => std::env::var("AGENTVC_CLOUD_TOKEN").map_err(|_| {
            "--cloud needs a machine token: set AGENTVC_CLOUD_TOKEN or pass --cloud-token-file \
             (make one on the hosted service's home page)"
                .to_string()
        })?,
    };
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("the cloud machine token is empty".into());
    }
    Ok(token)
}

/// Who UI actions are recorded as: `--user`, else the space's identity
/// (`agentvc config user.name`, then `git config user.name`), else `$USER`.
fn reviewer(dir: &std::path::Path, flag: Option<String>) -> String {
    if let Some(name) = flag {
        return agentvcd::human_actor(Some(&name));
    }
    let actor = agentvc_core::Space::discover(dir)
        .map(|s| s.human())
        .unwrap_or_else(|_| "human".into());
    if actor == "human" {
        return agentvcd::human_actor(std::env::var("USER").ok().as_deref());
    }
    actor
}

fn fail(json: bool, code: &str, message: String) -> ExitCode {
    fail_with(json, code, message, json!({}))
}

/// [`fail`], with `extra` fields merged into the JSON error object.
fn fail_with(json: bool, code: &str, message: String, extra: Value) -> ExitCode {
    if json {
        let mut err = json!({ "code": code, "message": message });
        if let (Value::Object(m), Value::Object(e)) = (&mut err, extra) {
            m.extend(e);
        }
        println!("{}", json!({ "ok": false, "error": err }));
    } else {
        eprintln!("agentvcd: {message}");
    }
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        None => match tokio::runtime::Runtime::new() {
            Ok(rt) => rt.block_on(serve(cli.serve)),
            Err(e) => fail(cli.serve.json, "io", e.to_string()),
        },
        Some(Cmd::Start(a)) => start(&a),
        Some(Cmd::Stop(a)) => stop(&a),
        Some(Cmd::Status(a)) => status(&a),
    }
}

/// Plain `agentvcd`: serve in the foreground until Ctrl+C (or SIGTERM).
async fn serve(cli: Serve) -> ExitCode {
    let mut config = Config::new(&cli.dir);
    config.poll_interval = Duration::from_millis(cli.poll_ms.max(50));
    config.actor = reviewer(&cli.dir, cli.user);
    config.init_if_missing = !cli.no_init;
    config.pr_poll = (cli.pr_poll_secs > 0).then(|| Duration::from_secs(cli.pr_poll_secs.max(30)));

    let cloud = match &cli.cloud {
        Some(url) => match cloud_token(cli.cloud_token_file.as_deref()) {
            Ok(token) => Some(agentvcd::cloud::CloudConfig {
                url: url.clone(),
                token,
                token_file: cli.cloud_token_file.clone(),
                read_only: cli.cloud_read_only,
                allow_agents: cli.allow_remote_agents,
            }),
            Err(e) => return fail(cli.json, "cloud_token", e),
        },
        None => None,
    };

    // The token every change must carry, shared by this user's daemons and
    // kept across restarts so an open review UI keeps working.
    if let Some(path) = agentvcd::auth::token_file() {
        match agentvcd::auth::load_or_create_token(&path) {
            Ok(t) => {
                config.token = Some(t);
                config.token_file = Some(path);
            }
            Err(e) => {
                return fail(
                    cli.json,
                    "token_file",
                    format!("cannot keep the daemon token in {}: {e}", path.display()),
                )
            }
        }
    }

    let state = match AppState::start(&config) {
        Ok(s) => s,
        Err(e) => return fail(cli.json, e.code(), e.to_string()),
    };
    // Join this checkout's space (#160), so the other checkouts of it on this
    // machine show up here and this one in theirs.
    let space_name = agentvc_core::stack::Window::open(state.workdir())
        .ok()
        .and_then(|w| agentvc_core::checkouts::register(&w));
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, cli.port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => return fail(cli.json, "bind", format!("cannot listen on {addr}: {e}")),
    };
    let addr = listener.local_addr().unwrap_or(addr);
    let url = format!("http://localhost:{}", addr.port());
    if cli.json {
        println!(
            "{}",
            json!({ "ok": true, "url": url, "workdir": state.workdir(), "actor": state.actor(), "space": space_name, "cloud": cli.cloud, "token_file": state.token_file() })
        );
    } else {
        println!(
            "agentvcd serving {} at {url} (actions recorded as {})",
            state.workdir().display(),
            state.actor()
        );
    }

    if let Some(c) = cloud {
        agentvcd::cloud::spawn(state.clone(), c);
    }
    // Written only once listening, so `agentvcd start` and `status` can
    // trust that the port named in it answers.
    let pid = std::process::id();
    if let Some(p) = &cli.pidfile {
        let info = PidInfo {
            pid,
            port: addr.port(),
            url: url.clone(),
            workdir: state.workdir().to_path_buf(),
            started_at: agentvc_core::space::now(),
        };
        if let Err(e) = daemon::write_pidfile(p, &info) {
            return fail(
                cli.json,
                "pidfile",
                format!("cannot write {}: {e}", p.display()),
            );
        }
    }
    let app = agentvcd::router(state);
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    if let Some(p) = &cli.pidfile {
        daemon::remove_pidfile_if(p, pid);
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(cli.json, "io", e.to_string()),
    }
}

/// Ctrl+C, or SIGTERM on Unix (what `agentvcd stop` sends).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// The JSON describing a running daemon.
fn running_json(info: &PidInfo, paths: &Paths) -> Value {
    json!({
        "running": true,
        "pid": info.pid,
        "url": info.url,
        "port": info.port,
        "workdir": info.workdir,
        "started_at": info.started_at,
        "log": paths.log,
        "pidfile": paths.pidfile,
    })
}

fn print_ok(json: bool, body: Value, text: String) -> ExitCode {
    if json {
        let mut out = json!({ "ok": true });
        if let (Value::Object(m), Value::Object(b)) = (&mut out, body) {
            m.extend(b);
        }
        println!("{out}");
    } else {
        println!("{text}");
    }
    ExitCode::SUCCESS
}

/// The arguments the background daemon runs with.
fn child_args(s: &Serve, root: &Path, pidfile: &Path) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec![
        "--json".into(),
        "-C".into(),
        root.into(),
        "--port".into(),
        s.port.to_string().into(),
        "--poll-ms".into(),
        s.poll_ms.to_string().into(),
        "--pr-poll-secs".into(),
        s.pr_poll_secs.to_string().into(),
        "--pidfile".into(),
        pidfile.into(),
    ];
    if s.no_init {
        a.push("--no-init".into());
    }
    if let Some(u) = &s.user {
        a.extend(["--user".into(), u.into()]);
    }
    if let Some(c) = &s.cloud {
        a.extend(["--cloud".into(), c.into()]);
        if let Some(f) = &s.cloud_token_file {
            a.extend(["--cloud-token-file".into(), absolute(f).into()]);
        }
        if s.cloud_read_only {
            a.push("--cloud-read-only".into());
        }
        if s.allow_remote_agents {
            a.push("--allow-remote-agents".into());
        }
    }
    a
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Whether the port is taken by something else (0 never is).
fn port_in_use(port: u16) -> bool {
    if port == 0 {
        return false;
    }
    matches!(
        std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse
    )
}

/// `agentvcd start`.
fn start(a: &StartArgs) -> ExitCode {
    let s = &a.serve;
    let json = s.json;
    let paths = Paths::for_dir(&s.dir);
    let where_ = json!({ "log": paths.log, "pidfile": paths.pidfile, "workdir": paths.root });
    if let Err(e) = std::fs::create_dir_all(&paths.dir) {
        return fail(
            json,
            "start_failed",
            format!("cannot make {}: {e}", paths.dir.display()),
        );
    }
    // One `start` at a time per checkout.
    let _lock = match agentvc_core::filelock::acquire(
        &paths.dir.join("start.lock"),
        Duration::from_secs(a.wait_secs.max(1)),
        Duration::from_millis(100),
    ) {
        Ok(Ok(l)) => l,
        Ok(Err(_)) => {
            return fail_with(
                json,
                "start_failed",
                "another `agentvcd start` for this space is still running".into(),
                where_,
            )
        }
        Err(e) => return fail_with(json, "start_failed", e.to_string(), where_),
    };
    if let Some(info) = daemon::running(&paths) {
        return fail_with(
            json,
            "already_running",
            format!(
                "agentvcd already serves {} at {} (pid {}); `agentvcd stop` stops it",
                info.workdir.display(),
                info.url,
                info.pid
            ),
            running_json(&info, &paths),
        );
    }
    if port_in_use(s.port) {
        return fail_with(
            json,
            "port_in_use",
            format!(
                "port {} on 127.0.0.1 is in use by another program (another agentvcd?); \
                 pass --port N, or --port 0 for any free one",
                s.port
            ),
            json!({ "port": s.port, "log": paths.log, "pidfile": paths.pidfile }),
        );
    }

    let log = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log)
    {
        Ok(f) => f,
        Err(e) => {
            return fail(
                json,
                "start_failed",
                format!("cannot open {}: {e}", paths.log.display()),
            )
        }
    };
    let from = log.metadata().map(|m| m.len()).unwrap_or(0);
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return fail(json, "start_failed", format!("cannot find agentvcd: {e}")),
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(child_args(s, &paths.root, &paths.pidfile))
        .current_dir(&paths.root)
        .stdin(Stdio::null());
    match log.try_clone() {
        Ok(out) => cmd.stdout(out).stderr(log),
        Err(e) => return fail(json, "start_failed", e.to_string()),
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group, so closing the terminal does not stop it.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail(json, "start_failed", format!("cannot start agentvcd: {e}")),
    };
    let pid = child.id();

    let deadline = Instant::now() + Duration::from_secs(a.wait_secs);
    loop {
        if let Some(info) = daemon::read_pidfile(&paths.pidfile).filter(|i| i.pid == pid) {
            if daemon::port_answers(info.port) {
                return print_ok(
                    json,
                    running_json(&info, &paths),
                    format!(
                        "agentvcd serving {} at {} (pid {pid}, log {})",
                        info.workdir.display(),
                        info.url,
                        paths.log.display()
                    ),
                );
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            let (cause, message) = daemon::error_in_log(&paths.log, from).unwrap_or_else(|| {
                (
                    "exited".into(),
                    format!(
                        "agentvcd exited ({status}) before listening: {}",
                        daemon::log_tail(&paths.log, from, 2000)
                    ),
                )
            });
            let code = if cause == "bind" && message.contains("in use") {
                "port_in_use"
            } else {
                "start_failed"
            };
            return fail_with(
                json,
                code,
                format!("{message} (log: {})", paths.log.display()),
                json!({ "cause": cause, "log": paths.log, "pidfile": paths.pidfile }),
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            daemon::remove_pidfile_if(&paths.pidfile, pid);
            return fail_with(
                json,
                "start_failed",
                format!(
                    "agentvcd did not listen within {}s; stopped it (log: {})",
                    a.wait_secs,
                    paths.log.display()
                ),
                json!({ "cause": "timeout", "log": paths.log, "pidfile": paths.pidfile }),
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn not_running(json: bool, paths: &Paths) -> ExitCode {
    fail_with(
        json,
        "not_running",
        format!(
            "no background agentvcd serves {} (`agentvcd start` starts one)",
            paths.root.display()
        ),
        json!({ "running": false, "workdir": paths.root, "log": paths.log, "pidfile": paths.pidfile }),
    )
}

/// `agentvcd stop`.
fn stop(a: &StopArgs) -> ExitCode {
    let paths = Paths::for_dir(&a.dir);
    let Some(info) = daemon::running(&paths) else {
        return not_running(a.json, &paths);
    };
    match daemon::terminate(info.pid, Duration::from_secs(a.timeout_secs)) {
        Ok(forced) => {
            daemon::remove_pidfile_if(&paths.pidfile, info.pid);
            let mut body = running_json(&info, &paths);
            body["running"] = json!(false);
            body["stopped"] = json!(true);
            body["forced"] = json!(forced);
            print_ok(
                a.json,
                body,
                format!(
                    "stopped agentvcd (pid {}) at {}{}",
                    info.pid,
                    info.url,
                    if forced { ", killed" } else { "" }
                ),
            )
        }
        Err(e) => fail_with(
            a.json,
            "stop_failed",
            format!("cannot stop agentvcd (pid {}): {e}", info.pid),
            running_json(&info, &paths),
        ),
    }
}

/// `agentvcd status`.
fn status(a: &StatusArgs) -> ExitCode {
    let paths = Paths::for_dir(&a.dir);
    match daemon::running(&paths) {
        Some(info) => print_ok(
            a.json,
            running_json(&info, &paths),
            format!(
                "agentvcd running: pid {}, {} serving {}\nlog {}",
                info.pid,
                info.url,
                info.workdir.display(),
                paths.log.display()
            ),
        ),
        None => not_running(a.json, &paths),
    }
}
