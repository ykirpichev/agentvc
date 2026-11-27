//! `agentvc-cloud`: run the hosted agentvc review UI.
//!
//! Configuration comes from flags or the environment, so the same container
//! image runs on a VM, Cloud Run, Fly.io or a laptop.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use agentvc_cloud::{AppState, Config, GithubOAuth};
use clap::Parser;

#[derive(Parser)]
#[command(name = "agentvc-cloud", version, about = "Hosted agentvc review UI")]
struct Cli {
    /// Address to listen on.
    #[arg(long, env = "AGENTVC_CLOUD_LISTEN", default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// Where people reach the service, e.g. https://agentvc.example.com.
    #[arg(long, env = "AGENTVC_CLOUD_PUBLIC_URL")]
    public_url: String,
    /// Directory for the database and its backups.
    #[arg(long, env = "AGENTVC_CLOUD_DATA", default_value = "./data")]
    data: PathBuf,
    /// GitHub logins allowed to sign in, comma-separated.
    #[arg(long, env = "AGENTVC_CLOUD_ALLOW_USERS", value_delimiter = ',')]
    allow_users: Vec<String>,
    /// GitHub OAuth app client id.
    #[arg(long, env = "GITHUB_CLIENT_ID")]
    github_client_id: Option<String>,
    /// GitHub OAuth app client secret.
    #[arg(long, env = "GITHUB_CLIENT_SECRET", hide_env_values = true)]
    github_client_secret: Option<String>,
    #[arg(
        long,
        env = "GITHUB_WEB_URL",
        default_value = "https://github.com",
        hide = true
    )]
    github_web_url: String,
    #[arg(
        long,
        env = "GITHUB_API_URL",
        default_value = "https://api.github.com",
        hide = true
    )]
    github_api_url: String,
    /// Sign in as this login at /auth/dev, without GitHub. Only allowed when
    /// listening on loopback; for trying the service locally.
    #[arg(long, env = "AGENTVC_CLOUD_DEV_USER")]
    dev_user: Option<String>,
    /// Hours between database backups into DATA/backups; 0 turns them off.
    #[arg(long, env = "AGENTVC_CLOUD_BACKUP_HOURS", default_value_t = 24)]
    backup_hours: u64,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut config = Config::new(&cli.public_url, &cli.data);
    if !config.public_url.starts_with("https://") && !config.public_url.starts_with("http://") {
        eprintln!("agentvc-cloud: --public-url must start with https:// or http://");
        return ExitCode::FAILURE;
    }
    config.allowed_users = cli
        .allow_users
        .iter()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .collect();
    config.github = match (cli.github_client_id, cli.github_client_secret) {
        (Some(id), Some(secret)) if !id.is_empty() && !secret.is_empty() => Some(GithubOAuth {
            client_id: id,
            client_secret: secret,
            web_url: cli.github_web_url.trim_end_matches('/').into(),
            api_url: cli.github_api_url.trim_end_matches('/').into(),
        }),
        _ => None,
    };
    if let Some(dev) = cli.dev_user.filter(|d| !d.trim().is_empty()) {
        if !cli.listen.ip().is_loopback() {
            eprintln!("agentvc-cloud: --dev-user signs anyone in; it only works when listening on loopback");
            return ExitCode::FAILURE;
        }
        if !config.allows(&dev) {
            config.allowed_users.push(dev.clone());
        }
        config.dev_user = Some(dev);
    }
    if config.allowed_users.is_empty() {
        eprintln!(
            "agentvc-cloud: set AGENTVC_CLOUD_ALLOW_USERS to the GitHub logins that may sign in"
        );
        return ExitCode::FAILURE;
    }
    if config.github.is_none() && config.dev_user.is_none() {
        eprintln!(
            "agentvc-cloud: set GITHUB_CLIENT_ID and GITHUB_CLIENT_SECRET (a GitHub OAuth app)"
        );
        return ExitCode::FAILURE;
    }
    config.backup_every =
        (cli.backup_hours > 0).then(|| Duration::from_secs(cli.backup_hours * 3600));

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("agentvc-cloud: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async move {
        let state = match AppState::open(config) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("agentvc-cloud: cannot open the database: {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Some(every) = state.config().backup_every {
            state.spawn_backups(every);
        }
        let listener = match tokio::net::TcpListener::bind(cli.listen).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("agentvc-cloud: cannot listen on {}: {e}", cli.listen);
                return ExitCode::FAILURE;
            }
        };
        eprintln!(
            "agentvc-cloud listening on {} for {}",
            cli.listen,
            state.config().public_url
        );
        let shutdown = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        match axum::serve(listener, agentvc_cloud::router(state))
            .with_graceful_shutdown(shutdown)
            .await
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("agentvc-cloud: {e}");
                ExitCode::FAILURE
            }
        }
    })
}
