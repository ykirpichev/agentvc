//! `agentvcd`: the local agentvc daemon.
//!
//! Serves a JSON API over one [`Space`] (reads, plus the review actions
//! promote, abandon, mark, rebase and take), a WebSocket stream of live
//! events and the review UI. It only ever binds to the loopback interface and
//! rejects requests whose `Host` or `Origin` is not local, so other websites
//! open in the same browser cannot read the space (DNS rebinding, cross-site
//! WebSockets).
//!
//! Every error response is `{"error": {"code": "...", "message": "..."}}`,
//! using the same stable codes as the CLI.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentvc_core::Space;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use tokio::sync::{broadcast, Notify};

mod api;
mod assets;
mod conflicts;
mod events;
mod graph;
mod history;
mod live;
mod rebase;
mod review;
mod take;
mod workspaces;

pub use api::ApiError;
pub use events::Event;

/// Default port for `agentvcd`.
pub const DEFAULT_PORT: u16 = 7420;

/// Daemon settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// Any directory inside the space to serve.
    pub dir: PathBuf,
    /// How often the event poller checks the database for changes.
    pub poll_interval: Duration,
    /// Who review actions from the UI are recorded as, e.g. `human:ana`.
    pub actor: String,
    /// How often to refresh open PRs from GitHub; `None` never does.
    pub pr_poll: Option<Duration>,
}

impl Config {
    pub fn new(dir: impl Into<PathBuf>) -> Config {
        Config {
            dir: dir.into(),
            poll_interval: Duration::from_millis(500),
            actor: "human".into(),
            pr_poll: None,
        }
    }
}

/// Shared state behind every handler.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    space: Mutex<Space>,
    workdir: PathBuf,
    actor: String,
    events: broadcast::Sender<Event>,
    /// Wakes the event poller early, after the daemon itself wrote.
    poke: Notify,
    /// Same for the workspace poller.
    workspace_poke: Notify,
}

impl AppState {
    /// Open the space containing `config.dir` and start the event poller.
    /// Must be called inside a Tokio runtime.
    pub fn start(config: &Config) -> agentvc_core::Result<AppState> {
        let space = Space::discover(&config.dir)?;
        let workdir = space.workdir().to_path_buf();
        let (tx, _) = broadcast::channel(1024);
        let watcher = events::Watcher::new(&space)?;
        let state = AppState {
            inner: Arc::new(Inner {
                space: Mutex::new(space),
                workdir,
                actor: config.actor.clone(),
                events: tx,
                poke: Notify::new(),
                workspace_poke: Notify::new(),
            }),
        };
        events::spawn_poller(state.clone(), watcher, config.poll_interval);
        workspaces::spawn_poller(
            state.clone(),
            state.workdir().to_path_buf(),
            config.poll_interval * 2,
        );
        if let Some(every) = config.pr_poll {
            spawn_pr_poller(state.clone(), every);
        }
        Ok(state)
    }

    pub fn workdir(&self) -> &Path {
        &self.inner.workdir
    }

    /// The actor review actions are recorded as.
    pub fn actor(&self) -> &str {
        &self.inner.actor
    }

    /// Subscribe to live events.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Ask the event poller to look for changes now rather than at its next
    /// tick, so a write from the UI shows up immediately.
    pub(crate) fn poke(&self) {
        self.inner.poke.notify_one();
    }

    /// Look at the workspaces now (after an edit from the API).
    pub(crate) fn poke_workspaces(&self) {
        self.inner.workspace_poke.notify_one();
    }

    pub(crate) async fn workspaces_poked(&self) {
        self.inner.workspace_poke.notified().await;
    }

    /// Broadcast an event to every connected client.
    pub(crate) fn send(&self, e: Event) {
        // No subscribers is fine.
        let _ = self.inner.events.send(e);
    }

    /// Wait until [`AppState::poke`] is called.
    pub(crate) async fn poked(&self) {
        self.inner.poke.notified().await;
    }

    /// Run `f` against the space on the blocking pool.
    pub(crate) async fn with_space<T, F>(&self, f: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&Space) -> agentvc_core::Result<T> + Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let space = inner.space.lock().unwrap_or_else(|p| p.into_inner());
            f(&space)
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::from)
    }
}

/// Refresh open PRs every `every` (#76), on a space of its own so network
/// calls never hold the one the API uses. Waits out rate limits; quiet when
/// there is no token or no open PR.
fn spawn_pr_poller(state: AppState, every: Duration) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            let workdir = state.workdir().to_path_buf();
            let res = tokio::task::spawn_blocking(move || {
                let space = Space::discover(&workdir)?;
                if space.open_prs()?.is_empty() || agentvc_core::github::token().is_err() {
                    return Ok::<_, agentvc_core::Error>((0, None));
                }
                let (done, err) = agentvc_core::feedback::refresh_open(&space);
                Ok((done.len(), err))
            })
            .await;
            if let Ok(Ok((n, err))) = res {
                if n > 0 {
                    state.poke();
                }
                if let Some(agentvc_core::Error::RateLimited { reset_at }) = err {
                    let wait = reset_at
                        .map(|t| (t - agentvc_core::space::now()).max(0) as u64)
                        .unwrap_or(900);
                    tokio::time::sleep(Duration::from_secs(wait.min(3600))).await;
                }
            }
        }
    });
}

/// The full application: API, events and UI.
pub fn router(state: AppState) -> Router {
    Router::new()
        .nest("/api", api::routes())
        .fallback(assets::serve)
        .layer(middleware::from_fn(local_only))
        .with_state(state)
}

pub use agentvc_core::config::human_actor;

/// True for `localhost`, `127.0.0.1`, `[::1]`, with or without a port.
fn is_local_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(h, _)| h)
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1")
}

fn is_local_origin(origin: &str) -> bool {
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .is_some_and(is_local_host)
}

async fn local_only(req: Request, next: Next) -> Response {
    let h = req.headers();
    let host_ok = h
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(is_local_host);
    // Browsers send Origin on cross-site requests and on every WebSocket
    // handshake; same-origin page loads may omit it.
    let origin_ok = h
        .get(header::ORIGIN)
        .map(|v| v.to_str().ok().is_some_and(is_local_origin))
        .unwrap_or(true);
    if host_ok && origin_ok {
        next.run(req).await
    } else {
        ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden_host",
            "agentvcd only answers requests addressed to localhost".into(),
        )
        .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_actor_names() {
        assert_eq!(human_actor(Some("Ana Lee")), "human:ana-lee");
        assert_eq!(human_actor(Some("  ")), "human");
        assert_eq!(human_actor(None), "human");
    }

    #[test]
    fn local_hosts() {
        for ok in ["localhost", "localhost:7420", "127.0.0.1:1", "[::1]:7420"] {
            assert!(is_local_host(ok), "{ok}");
        }
        for bad in [
            "evil.com",
            "localhost.evil.com",
            "127.0.0.2",
            "evil.com:7420",
        ] {
            assert!(!is_local_host(bad), "{bad}");
        }
        assert!(is_local_origin("http://localhost:5173"));
        assert!(!is_local_origin("https://evil.com"));
        assert!(!is_local_origin("null"));
    }
}
