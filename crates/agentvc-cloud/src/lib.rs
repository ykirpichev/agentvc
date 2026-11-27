//! `agentvc-cloud`: the hosted agentvc review UI.
//!
//! People sign in with GitHub and see the same review UI the local daemon
//! serves, for every machine they have connected. A machine connects by
//! running `agentvcd --cloud <url>` with a machine token made here; the
//! daemon dials out over a WebSocket, so nothing on it listens publicly.
//! API calls from the hosted UI are relayed to that daemon and answered
//! there. The service stores only people, sessions, token hashes and the
//! list of connected spaces; code, diffs and events pass through and are
//! never written down.
//!
//! Routes:
//! - `/` home: sign in, your spaces, machine tokens
//! - `/auth/github`, `/auth/github/callback`, `/auth/dev`, `/logout`
//! - `/connect` daemons (WebSocket, `Authorization: Bearer <token>`)
//! - `/s/<slug>/...` the review UI for one space, `/s/<slug>/api/...` its API
//! - `/api/me`, `/api/spaces` for scripts
//! - `/healthz`

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Request;
use axum::http::{header, HeaderValue};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{any, get, post};
use axum::Router;

mod assets;
pub mod auth;
pub mod db;
mod pages;
mod relay;

pub use relay::Hub;

/// GitHub OAuth app credentials.
#[derive(Clone, Debug)]
pub struct GithubOAuth {
    pub client_id: String,
    pub client_secret: String,
    /// `https://github.com`; tests point it at a fake.
    pub web_url: String,
    /// `https://api.github.com`; tests point it at a fake.
    pub api_url: String,
}

/// Service settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where people reach the service, e.g. `https://agentvc.example.com`.
    /// Used for the OAuth callback, cookie security and the Origin check.
    pub public_url: String,
    /// Holds `agentvc-cloud.db` and `backups/`.
    pub data_dir: PathBuf,
    pub github: Option<GithubOAuth>,
    /// GitHub logins allowed to sign in (case-insensitive).
    pub allowed_users: Vec<String>,
    /// Sign in as this login at `/auth/dev`, without GitHub. Local testing
    /// only; `main` refuses it unless the service listens on loopback.
    pub dev_user: Option<String>,
    /// How long to wait for a daemon to answer one API call.
    pub request_timeout: Duration,
    /// Write a copy of the database to `backups/` this often.
    pub backup_every: Option<Duration>,
}

impl Config {
    pub fn new(public_url: &str, data_dir: impl Into<PathBuf>) -> Config {
        Config {
            public_url: public_url.trim_end_matches('/').to_string(),
            data_dir: data_dir.into(),
            github: None,
            allowed_users: Vec::new(),
            dev_user: None,
            request_timeout: Duration::from_secs(120),
            backup_every: None,
        }
    }

    /// Cookies get `Secure` when people reach the service over HTTPS.
    pub fn secure(&self) -> bool {
        self.public_url.starts_with("https://")
    }

    /// `scheme://host[:port]` of [`Config::public_url`].
    pub fn origin(&self) -> String {
        let url = &self.public_url;
        let after_scheme = url.find("://").map(|i| i + 3).unwrap_or(0);
        match url[after_scheme..].find('/') {
            Some(i) => url[..after_scheme + i].to_string(),
            None => url.clone(),
        }
    }

    pub fn allows(&self, login: &str) -> bool {
        self.allowed_users
            .iter()
            .any(|u| u.eq_ignore_ascii_case(login))
    }
}

/// Shared state behind every handler.
#[derive(Clone)]
pub struct AppState {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) config: Config,
    pub(crate) db: Mutex<db::Db>,
    pub(crate) hub: Hub,
}

impl AppState {
    /// Open (or create) the database in `config.data_dir`.
    pub fn open(config: Config) -> anyhow::Result<AppState> {
        std::fs::create_dir_all(&config.data_dir)?;
        let db = db::Db::open(&config.data_dir.join("agentvc-cloud.db"))?;
        Ok(Self::with_db(config, db))
    }

    pub fn with_db(config: Config, db: db::Db) -> AppState {
        AppState {
            inner: Arc::new(Inner {
                config,
                db: Mutex::new(db),
                hub: Hub::default(),
            }),
        }
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Run `f` with the database. Queries are small and local, so they run
    /// inline rather than on the blocking pool.
    pub(crate) fn db<T>(&self, f: impl FnOnce(&db::Db) -> db::Result<T>) -> db::Result<T> {
        let db = self.inner.db.lock().unwrap_or_else(|p| p.into_inner());
        f(&db)
    }

    pub(crate) fn hub(&self) -> &Hub {
        &self.inner.hub
    }

    /// Copy the database to `backups/` every `every`, keeping the last 7.
    pub fn spawn_backups(&self, every: Duration) {
        let st = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let dir = st.config().data_dir.join("backups");
                if let Err(e) = backup_once(&st, &dir) {
                    eprintln!("agentvc-cloud: backup failed: {e}");
                }
            }
        });
    }
}

fn backup_once(st: &AppState, dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("agentvc-cloud-{}.db", db::now()));
    st.db(|d| d.backup_to(&path))?;
    let mut old: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "db"))
        .collect();
    old.sort();
    while old.len() > 7 {
        let _ = std::fs::remove_file(old.remove(0));
    }
    Ok(())
}

/// The whole service.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(pages::home))
        .route("/healthz", get(|| async { "ok" }))
        .route("/auth/github", get(auth::github_start))
        .route("/auth/github/callback", get(auth::github_callback))
        .route("/auth/dev", get(auth::dev_login))
        .route("/logout", post(auth::logout))
        .route("/tokens", post(pages::create_token))
        .route("/tokens/{id}/revoke", post(pages::revoke_token))
        .route("/spaces/{slug}/forget", post(pages::forget_space))
        .route("/api/me", get(pages::me))
        .route("/api/spaces", get(pages::spaces_json))
        .route("/connect", get(relay::connect))
        .route("/s/{slug}/api/events", get(relay::events))
        .route("/s/{slug}/api/{*rest}", any(relay::proxy))
        .route("/s/{slug}", get(assets::space_page))
        .route("/s/{slug}/", get(assets::space_page))
        .route("/s/{slug}/{*rest}", get(assets::space_page))
        .fallback(assets::serve)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn security_headers(req: Request, next: Next) -> Response {
    let https = req
        .headers()
        .get("x-forwarded-proto")
        .is_some_and(|v| v == "https");
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    if https {
        h.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        let c = Config::new("https://avc.example.com/", "/tmp/x");
        assert_eq!(c.public_url, "https://avc.example.com");
        assert_eq!(c.origin(), "https://avc.example.com");
        assert!(c.secure());
        let c = Config::new("http://127.0.0.1:8080/base", "/tmp/x");
        assert_eq!(c.origin(), "http://127.0.0.1:8080");
        assert!(!c.secure());
    }
}
