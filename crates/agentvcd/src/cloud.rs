//! `agentvcd --cloud <url>`: connect this daemon to the hosted service
//! (`agentvc-cloud`) so its review UI is reachable from a browser anywhere.
//!
//! The daemon dials out over one WebSocket and keeps it open, reconnecting
//! with backoff. The service sends API calls from the hosted UI down the
//! socket; each runs in-process against the same router the local UI uses,
//! and the answer goes back up. Live events stream up as they happen. See
//! [`agentvc_core::relay`] for the frames.
//!
//! Only `/api/` paths are served this way, and with `--cloud-read-only` only
//! reads. The service stores none of it.

use std::time::Duration;

use agentvc_core::relay::{CloudMsg, DaemonMsg, CONNECT_PATH, MAX_BODY_BYTES, PROTOCOL_VERSION};
use axum::body::Body;
use axum::http::{header, HeaderValue, Method, Request, StatusCode};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::{broadcast::error::RecvError, mpsc};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};
use tower::ServiceExt;

use crate::AppState;

/// How to reach the hosted service.
#[derive(Clone, Debug)]
pub struct CloudConfig {
    /// The service's base URL, e.g. `https://agentvc.example.com`.
    pub url: String,
    /// A machine token made on the service's home page.
    pub token: String,
    /// Where the token came from; read again after the service refuses
    /// it, so replacing the file is enough to rotate a token.
    pub token_file: Option<std::path::PathBuf>,
    /// Refuse every relayed request that is not a GET.
    pub read_only: bool,
    /// Let the hosted page start, answer and land agents on this machine.
    pub allow_agents: bool,
}

/// Keep a connection to the service open until the process exits.
pub fn spawn(state: AppState, mut cfg: CloudConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let app = crate::router(state.clone());
        let mut backoff = Duration::from_secs(1);
        loop {
            match session(&state, &app, &cfg).await {
                Ok(()) => {
                    eprintln!("agentvcd: cloud connection closed; reconnecting");
                    backoff = Duration::from_secs(1);
                }
                Err(Failure::Rejected(msg)) => {
                    // A wrong or revoked token will not fix itself quickly,
                    // but a new one may be written to the token file.
                    eprintln!("agentvcd: cloud refused this daemon: {msg}");
                    if wait_for_new_token(&mut cfg, Duration::from_secs(300)).await {
                        eprintln!("agentvcd: the token file changed; reconnecting");
                        backoff = Duration::from_secs(1);
                        continue;
                    }
                    backoff = Duration::from_secs(1);
                    continue;
                }
                Err(Failure::Network(msg)) => {
                    eprintln!(
                        "agentvcd: cannot reach {}: {msg}; retrying in {}s",
                        cfg.url,
                        backoff.as_secs()
                    );
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    })
}

/// Wait up to `max` for the token file to hold a different token; true if
/// it did (and `cfg` now has it).
async fn wait_for_new_token(cfg: &mut CloudConfig, max: Duration) -> bool {
    let step = Duration::from_secs(5);
    let mut waited = Duration::ZERO;
    while waited < max {
        tokio::time::sleep(step).await;
        waited += step;
        let Some(p) = &cfg.token_file else { continue };
        let Ok(t) = std::fs::read_to_string(p) else {
            continue;
        };
        let t = t.trim();
        if !t.is_empty() && t != cfg.token {
            cfg.token = t.to_string();
            return true;
        }
    }
    false
}

enum Failure {
    /// The service said no (bad token, old protocol).
    Rejected(String),
    /// Anything else; worth retrying soon.
    Network(String),
}

/// `https://x` -> `wss://x/connect`.
pub(crate) fn connect_url(base: &str) -> Result<String, String> {
    let base = base.trim_end_matches('/');
    let rest = if let Some(r) = base.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = base.strip_prefix("http://") {
        format!("ws://{r}")
    } else {
        return Err(format!(
            "cloud URL must start with https:// or http://: {base}"
        ));
    };
    Ok(format!("{rest}{CONNECT_PATH}"))
}

async fn session(state: &AppState, app: &Router, cfg: &CloudConfig) -> Result<(), Failure> {
    let url = connect_url(&cfg.url).map_err(Failure::Rejected)?;
    let mut req = url
        .into_client_request()
        .map_err(|e| Failure::Rejected(e.to_string()))?;
    let auth = HeaderValue::from_str(&format!("Bearer {}", cfg.token))
        .map_err(|_| Failure::Rejected("the token has characters a header cannot carry".into()))?;
    req.headers_mut().insert(header::AUTHORIZATION, auth);
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_BODY_BYTES * 2))
        .max_frame_size(Some(MAX_BODY_BYTES * 2));
    let (ws, _) = tokio_tungstenite::connect_async_with_config(req, Some(ws_config), false)
        .await
        .map_err(|e| match e {
            tungstenite::Error::Http(resp) => {
                let body = resp
                    .body()
                    .as_deref()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                Failure::Rejected(format!("HTTP {}: {}", resp.status(), body.trim()))
            }
            other => Failure::Network(other.to_string()),
        })?;
    let (mut sink, mut stream) = ws.split();

    let (space_id, name) = state
        .with_space(|s| {
            let id = agentvc_core::sync::space_id(s)?;
            let name = s
                .workdir()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "space".into());
            Ok((id, name))
        })
        .await
        .map_err(|e| Failure::Rejected(format!("cannot read the space: {e:?}")))?;
    let hello = DaemonMsg::Hello {
        protocol: PROTOCOL_VERSION,
        space_id,
        name,
        workdir: state.workdir().to_string_lossy().into_owned(),
        version: env!("CARGO_PKG_VERSION").into(),
        read_only: cfg.read_only,
    };
    sink.send(Message::Text(to_json(&hello).into()))
        .await
        .map_err(|e| Failure::Network(e.to_string()))?;

    // One writer owns the sink; requests and events queue up for it.
    let (tx, mut rx) = mpsc::channel::<String>(256);
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(25));
        ping.tick().await;
        loop {
            tokio::select! {
                m = rx.recv() => match m {
                    Some(text) => if sink.send(Message::Text(text.into())).await.is_err() { break },
                    None => break,
                },
                // Proxies such as Cloudflare drop sockets idle for ~100 s.
                _ = ping.tick() => if sink.send(Message::Ping(Vec::new().into())).await.is_err() { break },
            }
        }
        let _ = sink.close().await;
    });

    let events = {
        let tx = tx.clone();
        let mut sub = state.subscribe();
        tokio::spawn(async move {
            loop {
                let event = match sub.recv().await {
                    Ok(ev) => serde_json::to_value(&ev).unwrap_or(json!({"type": "resync"})),
                    Err(RecvError::Lagged(_)) => json!({"type": "resync"}),
                    Err(RecvError::Closed) => break,
                };
                if tx.send(to_json(&DaemonMsg::Event { event })).await.is_err() {
                    break;
                }
            }
        })
    };

    let result = loop {
        let msg = match stream.next().await {
            Some(Ok(m)) => m,
            Some(Err(e)) => break Err(Failure::Network(e.to_string())),
            None => break Ok(()),
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break Ok(()),
            _ => continue,
        };
        match serde_json::from_str::<CloudMsg>(&text) {
            Ok(CloudMsg::Welcome { url, .. }) => {
                eprintln!("agentvcd: connected to the cloud; review at {url}");
            }
            Ok(CloudMsg::Request {
                id,
                method,
                path,
                content_type,
                body,
            }) => {
                let app = app.clone();
                let tx = tx.clone();
                let read_only = cfg.read_only;
                let allow_agents = cfg.allow_agents;
                tokio::spawn(async move {
                    let res = relay(
                        app,
                        read_only,
                        allow_agents,
                        &method,
                        &path,
                        content_type,
                        body,
                    )
                    .await;
                    let (status, content_type, body) = res;
                    let _ = tx
                        .send(to_json(&DaemonMsg::Response {
                            id,
                            status,
                            content_type,
                            body,
                        }))
                        .await;
                });
            }
            Ok(CloudMsg::Error { code, message }) => {
                break Err(Failure::Rejected(format!("{code}: {message}")));
            }
            // A newer service may send frames this daemon does not know.
            Err(_) => {}
        }
    };
    events.abort();
    drop(tx);
    let _ = writer.await;
    result
}

fn to_json(m: &DaemonMsg) -> String {
    serde_json::to_string(m).unwrap_or_default()
}

fn error_body(code: &str, message: &str) -> String {
    json!({ "error": { "code": code, "message": message } }).to_string()
}

/// Run one relayed API call against the local router.
pub(crate) async fn relay(
    app: Router,
    read_only: bool,
    allow_agents: bool,
    method: &str,
    path: &str,
    content_type: Option<String>,
    body: Option<String>,
) -> (u16, Option<String>, String) {
    let json_ct = Some("application/json".to_string());
    let route = path.split('?').next().unwrap_or("");
    if !route.starts_with("/api/") || route == "/api/events" || path.contains("..") {
        return (
            404,
            json_ct,
            error_body("no_route", "only API calls are relayed"),
        );
    }
    let Ok(method) = Method::from_bytes(method.as_bytes()) else {
        return (
            400,
            json_ct,
            error_body("bad_method", "unknown HTTP method"),
        );
    };
    if read_only && method != Method::GET {
        return (
            403,
            json_ct,
            error_body(
                "read_only",
                "this daemon was started with --cloud-read-only; make changes locally",
            ),
        );
    }
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "localhost")
        .header(
            crate::agents::RELAY_HEADER,
            if allow_agents { "agents" } else { "1" },
        );
    if let Some(ct) = content_type {
        req = req.header(header::CONTENT_TYPE, ct);
    }
    let req = match req.body(Body::from(body.unwrap_or_default())) {
        Ok(r) => r,
        Err(e) => return (400, json_ct, error_body("bad_request", &e.to_string())),
    };
    let res = match app.oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    let status = res.status();
    let ct = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match axum::body::to_bytes(res.into_body(), MAX_BODY_BYTES).await {
        Ok(bytes) => (
            status.as_u16(),
            ct,
            String::from_utf8_lossy(&bytes).into_owned(),
        ),
        Err(_) => (
            StatusCode::BAD_GATEWAY.as_u16(),
            json_ct,
            error_body("too_large", "the response is too large to relay"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_urls() {
        assert_eq!(
            connect_url("https://avc.example.com/").unwrap(),
            "wss://avc.example.com/connect"
        );
        assert_eq!(
            connect_url("http://127.0.0.1:8080").unwrap(),
            "ws://127.0.0.1:8080/connect"
        );
        assert!(connect_url("avc.example.com").is_err());
    }
}
