//! The relay: daemons connect at `/connect`, and the hosted UI's API calls
//! and live events for `/s/<slug>/api/...` travel over that connection.
//!
//! Nothing that passes through is stored. A space whose daemon is not
//! connected answers `503 machine_offline`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentvc_core::relay::{CloudMsg, DaemonMsg, MAX_BODY_BYTES, PROTOCOL_VERSION};
use axum::body::{to_bytes, Body};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::auth::{require_user, same_origin, Fail};
use crate::db::{SpaceRow, User};
use crate::AppState;

struct Reply {
    status: u16,
    content_type: Option<String>,
    body: String,
}

/// One connected daemon.
pub(crate) struct Conn {
    /// Frames for the daemon.
    tx: mpsc::Sender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    next_id: AtomicU64,
    /// Its events, as JSON text, for every open UI.
    events: broadcast::Sender<String>,
    /// Flips to false when the daemon goes away.
    alive: watch::Sender<bool>,
    workdir: String,
}

/// Connected daemons by space slug.
#[derive(Default)]
pub struct Hub {
    conns: Mutex<HashMap<String, Arc<Conn>>>,
}

impl Hub {
    fn get(&self, slug: &str) -> Option<Arc<Conn>> {
        self.conns.lock().unwrap().get(slug).cloned()
    }

    /// Is a daemon serving this space right now?
    pub fn online(&self, slug: &str) -> bool {
        self.get(slug).is_some()
    }

    fn insert(&self, slug: &str, c: Arc<Conn>) {
        if let Some(old) = self.conns.lock().unwrap().insert(slug.into(), c) {
            // The same space connected again (a restart); the old socket is
            // dead or about to be.
            old.alive.send_replace(false);
        }
    }

    /// Drop the daemon serving `slug`, e.g. after its token was revoked.
    pub fn disconnect(&self, slug: &str) {
        if let Some(c) = self.conns.lock().unwrap().remove(slug) {
            c.alive.send_replace(false);
        }
    }

    fn remove(&self, slug: &str, c: &Arc<Conn>) {
        let mut m = self.conns.lock().unwrap();
        if m.get(slug).is_some_and(|cur| Arc::ptr_eq(cur, c)) {
            m.remove(slug);
        }
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

/// `GET /connect`: a daemon dials in with its machine token.
pub(crate) async fn connect(
    State(st): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, Fail> {
    let token = bearer(&headers).ok_or_else(|| {
        Fail::new(
            StatusCode::UNAUTHORIZED,
            "no_token",
            "send Authorization: Bearer <machine token>",
        )
    })?;
    let token = token.to_string();
    let (token_id, user) = st.db(|d| d.use_token(&token))?.ok_or_else(|| {
        Fail::new(
            StatusCode::UNAUTHORIZED,
            "bad_token",
            "unknown or revoked machine token; make a new one on the home page",
        )
    })?;
    if !st.config().allows(&user.login) {
        return Err(Fail::new(
            StatusCode::FORBIDDEN,
            "not_allowed",
            format!("{} may no longer use this service", user.login),
        ));
    }
    Ok(ws
        .max_message_size(MAX_BODY_BYTES * 2)
        .max_frame_size(MAX_BODY_BYTES * 2)
        .on_upgrade(move |socket| daemon_session(st, socket, token_id, user)))
}

async fn send_cloud(socket: &mut WebSocket, m: &CloudMsg) -> bool {
    let text = serde_json::to_string(m).unwrap_or_default();
    socket.send(Message::Text(text.into())).await.is_ok()
}

async fn daemon_session(st: AppState, mut socket: WebSocket, token_id: i64, user: User) {
    // The first frame must be a hello.
    let hello = tokio::time::timeout(Duration::from_secs(15), socket.recv()).await;
    let Ok(Some(Ok(Message::Text(text)))) = hello else {
        return;
    };
    let Ok(DaemonMsg::Hello {
        protocol,
        space_id,
        name,
        workdir,
        version,
        read_only,
    }) = serde_json::from_str::<DaemonMsg>(&text)
    else {
        return;
    };
    if protocol != PROTOCOL_VERSION {
        let _ = send_cloud(
            &mut socket,
            &CloudMsg::Error {
                code: "protocol".into(),
                message: format!(
                    "this service speaks relay protocol {PROTOCOL_VERSION}, the daemon {protocol}; update agentvcd"
                ),
            },
        )
        .await;
        return;
    }
    let slug = match st.db(|d| {
        d.upsert_space(
            user.id, token_id, &space_id, &name, &workdir, &version, read_only,
        )
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("agentvc-cloud: cannot record space: {e}");
            return;
        }
    };
    let url = format!("{}/s/{slug}/", st.config().public_url);
    if !send_cloud(
        &mut socket,
        &CloudMsg::Welcome {
            slug: slug.clone(),
            url,
        },
    )
    .await
    {
        return;
    }

    let (tx, mut rx) = mpsc::channel::<String>(256);
    let (events, _) = broadcast::channel(1024);
    let (alive, mut alive_rx) = watch::channel(true);
    let conn = Arc::new(Conn {
        tx,
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        events,
        alive,
        workdir,
    });
    st.hub().insert(&slug, conn.clone());
    eprintln!("agentvc-cloud: {} connected {slug} ({name})", user.login);

    let mut ping = tokio::time::interval(Duration::from_secs(25));
    let mut touch = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(text) => if socket.send(Message::Text(text.into())).await.is_err() { break },
                None => break,
            },
            frame = socket.recv() => {
                let text = match frame {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => continue,
                };
                match serde_json::from_str::<DaemonMsg>(&text) {
                    Ok(DaemonMsg::Response { id, status, content_type, body }) => {
                        let waiter = conn.pending.lock().unwrap().remove(&id);
                        if let Some(w) = waiter {
                            let _ = w.send(Reply { status, content_type, body });
                        }
                    }
                    Ok(DaemonMsg::Event { event }) => {
                        let _ = conn.events.send(event.to_string());
                    }
                    _ => {}
                }
            }
            _ = ping.tick() => if socket.send(Message::Ping(Vec::new().into())).await.is_err() { break },
            _ = touch.tick() => { let _ = st.db(|d| d.touch_space(&slug)); }
            _ = alive_rx.changed() => if !*alive_rx.borrow() { break },
        }
    }
    st.hub().remove(&slug, &conn);
    conn.alive.send_replace(false);
    // Waiting callers get "offline" when their senders drop.
    conn.pending.lock().unwrap().clear();
    let _ = st.db(|d| d.touch_space(&slug));
    eprintln!("agentvc-cloud: {slug} disconnected");
}

/// The signed-in person's space `slug`. Someone else's space is "not found",
/// not "forbidden", so slugs cannot be probed.
fn owned_space(st: &AppState, headers: &HeaderMap, slug: &str) -> Result<SpaceRow, Fail> {
    let user = require_user(st, headers)?;
    st.db(|d| d.space(slug))?
        .filter(|s| s.user_id == user.id)
        .ok_or_else(|| Fail::new(StatusCode::NOT_FOUND, "no_space", "no such space"))
}

fn offline(space: &SpaceRow) -> Fail {
    Fail::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "machine_offline",
        format!(
            "{} on {} is not connected; run `agentvcd --cloud <this service's URL>` there",
            space.name, space.machine
        ),
    )
}

/// `/s/<slug>/api/<rest>`: relay one API call to the space's daemon.
pub(crate) async fn proxy(
    State(st): State<AppState>,
    Path((slug, rest)): Path<(String, String)>,
    req: Request,
) -> Result<Response, Fail> {
    let (parts, body) = req.into_parts();
    let space = owned_space(&st, &parts.headers, &slug)?;
    if parts.method != axum::http::Method::GET && parts.method != axum::http::Method::HEAD {
        same_origin(&st, &parts.headers)?;
    }
    let conn = st.hub().get(&slug).ok_or_else(|| offline(&space))?;
    let body = to_bytes(body, MAX_BODY_BYTES).await.map_err(|_| {
        Fail::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "request too large",
        )
    })?;
    let body = if body.is_empty() {
        None
    } else {
        Some(String::from_utf8(body.to_vec()).map_err(|_| {
            Fail::new(
                StatusCode::BAD_REQUEST,
                "not_text",
                "request body must be text",
            )
        })?)
    };
    let path = match parts.uri.query() {
        Some(q) => format!("/api/{rest}?{q}"),
        None => format!("/api/{rest}"),
    };
    let id = conn.next_id.fetch_add(1, Ordering::Relaxed);
    let (wtx, wrx) = oneshot::channel();
    conn.pending.lock().unwrap().insert(id, wtx);
    let msg = CloudMsg::Request {
        id,
        method: parts.method.to_string(),
        path,
        content_type: parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        body,
    };
    if conn
        .tx
        .send(serde_json::to_string(&msg).unwrap_or_default())
        .await
        .is_err()
    {
        conn.pending.lock().unwrap().remove(&id);
        return Err(offline(&space));
    }
    let reply = match tokio::time::timeout(st.config().request_timeout, wrx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => return Err(offline(&space)),
        Err(_) => {
            conn.pending.lock().unwrap().remove(&id);
            return Err(Fail::new(
                StatusCode::GATEWAY_TIMEOUT,
                "machine_timeout",
                format!("{} did not answer in time", space.name),
            ));
        }
    };
    let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut res = Response::new(Body::from(reply.body));
    *res.status_mut() = status;
    let ct = reply
        .content_type
        .unwrap_or_else(|| "application/json".into());
    if let Ok(v) = ct.parse() {
        res.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    res.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok(res)
}

/// `/s/<slug>/api/events`: the daemon's live events for one browser tab.
pub(crate) async fn events(
    State(st): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, Fail> {
    let space = owned_space(&st, &headers, &slug)?;
    // Browsers send cookies on cross-site WebSocket handshakes too.
    same_origin(&st, &headers)?;
    let conn = st.hub().get(&slug).ok_or_else(|| offline(&space))?;
    Ok(ws
        .on_upgrade(move |socket| browser_events(conn, socket))
        .into_response())
}

async fn browser_events(conn: Arc<Conn>, mut socket: WebSocket) {
    let mut rx = conn.events.subscribe();
    let mut alive = conn.alive.subscribe();
    let hello = serde_json::json!({ "type": "hello", "workdir": conn.workdir }).to_string();
    if socket.send(Message::Text(hello.into())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let text = match ev {
                    Ok(t) => t,
                    Err(broadcast::error::RecvError::Lagged(_)) => r#"{"type":"resync"}"#.to_string(),
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if socket.send(Message::Text(text.into())).await.is_err() { break }
            }
            _ = alive.changed() => if !*alive.borrow() { break },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            }
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}
