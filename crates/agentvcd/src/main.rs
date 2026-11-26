//! `agentvcd`: serve the review UI and JSON API for one agentvc space.
//!
//! Binds to 127.0.0.1 only. Prints one JSON line with `--json` once it is
//! listening, so scripts and agents can read the URL.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use agentvcd::{AppState, Config, DEFAULT_PORT};
use clap::Parser;
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "agentvcd",
    version,
    about = "Local daemon for the agentvc review UI"
)]
struct Cli {
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
    /// Reviewer name recorded on actions taken in the UI, as `human:<name>`.
    /// Defaults to `git config user.name`, then `$USER`.
    #[arg(long, value_name = "NAME")]
    user: Option<String>,
}

/// The reviewer's name: `--user`, else git's `user.name`, else `$USER`.
fn reviewer_name(dir: &std::path::Path, flag: Option<String>) -> Option<String> {
    flag.or_else(|| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["config", "user.name"])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (out.status.success() && !name.is_empty()).then_some(name)
    })
    .or_else(|| std::env::var("USER").ok())
}

fn fail(json: bool, code: &str, message: String) -> ExitCode {
    if json {
        println!(
            "{}",
            json!({ "ok": false, "error": { "code": code, "message": message } })
        );
    } else {
        eprintln!("agentvcd: {message}");
    }
    ExitCode::FAILURE
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut config = Config::new(&cli.dir);
    config.poll_interval = Duration::from_millis(cli.poll_ms.max(50));
    config.pr_poll = (cli.pr_poll_secs > 0).then(|| Duration::from_secs(cli.pr_poll_secs.max(30)));
    config.actor = agentvcd::human_actor(reviewer_name(&cli.dir, cli.user).as_deref());

    let state = match AppState::start(&config) {
        Ok(s) => s,
        Err(e) => return fail(cli.json, e.code(), e.to_string()),
    };
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
            json!({ "ok": true, "url": url, "workdir": state.workdir(), "actor": state.actor() })
        );
    } else {
        println!(
            "agentvcd serving {} at {url} (actions recorded as {})",
            state.workdir().display(),
            state.actor()
        );
    }

    let app = agentvcd::router(state);
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    match axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(cli.json, "io", e.to_string()),
    }
}
