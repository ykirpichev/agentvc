//! End to end: a local daemon connects to the service with a machine token,
//! and the hosted UI's API calls and live events reach it.

use std::time::Duration;

use agentvc_cloud::{AppState, Config, GithubOAuth};
use agentvc_core::Space;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

struct Svc {
    base: String,
    _data: tempfile::TempDir,
}

async fn listen(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

/// The service, signed-in-able as `ana` at /auth/dev, with GitHub sign-in
/// pointed at `github` when given.
async fn service(github: Option<String>) -> Svc {
    let data = tempfile::tempdir().unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let mut cfg = Config::new(&base, data.path());
    cfg.allowed_users = vec!["ana".into()];
    cfg.dev_user = Some("ana".into());
    cfg.request_timeout = Duration::from_secs(10);
    cfg.github = github.map(|g| GithubOAuth {
        client_id: "cid".into(),
        client_secret: "secret".into(),
        web_url: g.clone(),
        api_url: g,
    });
    let st = AppState::open(cfg).unwrap();
    let app = agentvc_cloud::router(st);
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Svc { base, _data: data }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(None)
        .build()
        .new_agent()
}

struct Res {
    status: u16,
    cookies: Vec<String>,
    location: Option<String>,
    body: String,
}

/// One blocking HTTP call on the blocking pool.
async fn http(
    method: &'static str,
    url: String,
    headers: Vec<(&'static str, String)>,
    body: Option<String>,
) -> Res {
    tokio::task::spawn_blocking(move || {
        let a = agent();
        let mut res = match method {
            "GET" => {
                let mut r = a.get(&url);
                for (k, v) in &headers {
                    r = r.header(*k, v);
                }
                r.call().unwrap()
            }
            _ => {
                let mut r = a.post(&url);
                for (k, v) in &headers {
                    r = r.header(*k, v);
                }
                r.send(body.unwrap_or_default()).unwrap()
            }
        };
        let cookies = res
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap().to_string())
            .collect();
        let location = res
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        Res {
            status: res.status().as_u16(),
            cookies,
            location,
            body: res.body_mut().read_to_string().unwrap_or_default(),
        }
    })
    .await
    .unwrap()
}

async fn dev_session(svc: &Svc) -> String {
    let r = http("GET", format!("{}/auth/dev", svc.base), vec![], None).await;
    assert_eq!(r.status, 303);
    r.cookies
        .into_iter()
        .find(|c| c.starts_with("avc_session=") && c.len() > 12)
        .expect("a session cookie")
}

async fn new_token(svc: &Svc, cookie: &str) -> String {
    let r = http(
        "POST",
        format!("{}/tokens", svc.base),
        vec![
            ("cookie", cookie.into()),
            ("origin", svc.base.clone()),
            ("accept", "application/json".into()),
            ("content-type", "application/x-www-form-urlencoded".into()),
        ],
        Some("name=laptop".into()),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let v: Value = serde_json::from_str(&r.body).unwrap();
    v["token"].as_str().unwrap().to_string()
}

async fn spaces(svc: &Svc, cookie: &str) -> Vec<Value> {
    let r = http(
        "GET",
        format!("{}/api/spaces", svc.base),
        vec![("cookie", cookie.into())],
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    serde_json::from_str::<Vec<Value>>(&r.body).unwrap()
}

async fn wait_online(svc: &Svc, cookie: &str) -> String {
    for _ in 0..100 {
        if let Some(s) = spaces(svc, cookie)
            .await
            .into_iter()
            .find(|s| s["online"] == true)
        {
            return s["slug"].as_str().unwrap().to_string();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the daemon never came online");
}

fn space_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "a\n").unwrap();
    let s = Space::init(tmp.path()).unwrap();
    s.create_task("first task", None).unwrap();
    tmp
}

fn daemon(dir: &std::path::Path, url: &str, token: &str, read_only: bool) -> agentvcd::AppState {
    let mut cfg = agentvcd::Config::new(dir);
    cfg.poll_interval = Duration::from_millis(50);
    let st = agentvcd::AppState::start(&cfg).unwrap();
    agentvcd::cloud::spawn(
        st.clone(),
        agentvcd::cloud::CloudConfig {
            url: url.into(),
            token: token.into(),
            token_file: None,
            read_only,
            allow_agents: false,
        },
    );
    st
}

#[tokio::test(flavor = "multi_thread")]
async fn relays_api_and_events() {
    let svc = service(None).await;
    let cookie = dev_session(&svc).await;
    let token = new_token(&svc, &cookie).await;
    let dir = space_dir();
    let _d = daemon(dir.path(), &svc.base, &token, false);
    let slug = wait_online(&svc, &cookie).await;

    // A read goes through and matches what the daemon says locally.
    let r = http(
        "GET",
        format!("{}/s/{slug}/api/tasks", svc.base),
        vec![("cookie", cookie.clone())],
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let tasks: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(tasks[0]["goal"], "first task");

    // Query strings survive, and daemon errors keep their codes.
    let r = http(
        "GET",
        format!("{}/s/{slug}/api/tasks/99?x=1", svc.base),
        vec![("cookie", cookie.clone())],
        None,
    )
    .await;
    assert_eq!(r.status, 404);
    let err: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(err["error"]["code"], "not_found", "{err}");

    // The UI shell is served under the space's path.
    let r = http(
        "GET",
        format!("{}/s/{slug}/review/t/1", svc.base),
        vec![("cookie", cookie.clone())],
        None,
    )
    .await;
    assert_eq!(r.status, 200);

    // Signed out: nothing.
    let r = http(
        "GET",
        format!("{}/s/{slug}/api/tasks", svc.base),
        vec![],
        None,
    )
    .await;
    assert_eq!(r.status, 401);
    let r = http("GET", format!("{}/s/{slug}/", svc.base), vec![], None).await;
    assert_eq!(r.status, 303);

    // Writes need the service's own Origin.
    let r = http(
        "POST",
        format!("{}/s/{slug}/api/undo", svc.base),
        vec![
            ("cookie", cookie.clone()),
            ("origin", "https://evil.example".into()),
        ],
        Some("{}".into()),
    )
    .await;
    assert_eq!(r.status, 403);
    let err: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(err["error"]["code"], "cross_origin");

    // Live events: hello first, then a new task made locally streams up.
    let ws_url = format!(
        "{}/s/{slug}/api/events",
        svc.base.replace("http://", "ws://")
    );
    let mut req = ws_url.clone().into_client_request().unwrap();
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    req.headers_mut()
        .insert("origin", svc.base.parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let first = next_json(&mut ws).await;
    assert_eq!(first["type"], "hello");
    Space::discover(dir.path())
        .unwrap()
        .create_task("second task", None)
        .unwrap();
    loop {
        let ev = next_json(&mut ws).await;
        if ev["type"] == "task_created" {
            assert_eq!(ev["task"]["goal"], "second task");
            break;
        }
    }

    // A WebSocket from another site is refused even with the cookie.
    let mut req = ws_url.into_client_request().unwrap();
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    req.headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_err());
}

async fn next_json<S>(ws: &mut S) -> Value
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let m = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("an event in time")
            .unwrap()
            .unwrap();
        if let Message::Text(t) = m {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_daemons_refuse_writes() {
    let svc = service(None).await;
    let cookie = dev_session(&svc).await;
    let token = new_token(&svc, &cookie).await;
    let dir = space_dir();
    let _d = daemon(dir.path(), &svc.base, &token, true);
    let slug = wait_online(&svc, &cookie).await;
    let r = http(
        "POST",
        format!("{}/s/{slug}/api/undo", svc.base),
        vec![
            ("cookie", cookie.clone()),
            ("origin", svc.base.clone()),
            ("content-type", "application/json".into()),
        ],
        Some("{}".into()),
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.body);
    let err: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(err["error"]["code"], "read_only");
    assert_eq!(spaces(&svc, &cookie).await[0]["read_only"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_and_revoked_tokens_are_refused() {
    let svc = service(None).await;
    let connect = |token: &str| {
        let mut req = format!("{}/connect", svc.base.replace("http://", "ws://"))
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        tokio_tungstenite::connect_async(req)
    };
    assert!(connect("avc_nope").await.is_err());

    let cookie = dev_session(&svc).await;
    let token = new_token(&svc, &cookie).await;
    let dir = space_dir();
    let _d = daemon(dir.path(), &svc.base, &token, false);
    wait_online(&svc, &cookie).await;

    // Revoking from the home page drops the machine and its spaces.
    let r = http(
        "POST",
        format!("{}/tokens/1/revoke", svc.base),
        vec![("cookie", cookie.clone()), ("origin", svc.base.clone())],
        None,
    )
    .await;
    assert_eq!(r.status, 303, "{}", r.body);
    assert!(spaces(&svc, &cookie).await.is_empty());
    assert!(connect(&token).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn github_sign_in() {
    // A fake GitHub: any code is good, the user is Ana (or Bob, who is not
    // allowed).
    let fake = Router::new()
        .route(
            "/login/oauth/access_token",
            post(|body: String| async move {
                let who = if body.contains("code=bob") {
                    "bob"
                } else {
                    "ana"
                };
                Json(json!({ "access_token": who }))
            }),
        )
        .route(
            "/user",
            get(|h: axum::http::HeaderMap| async move {
                let auth = h["authorization"].to_str().unwrap().to_string();
                let login = if auth.ends_with("bob") { "bob" } else { "Ana" };
                Json(json!({ "login": login, "id": 5 }))
            }),
        );
    let gh = listen(fake).await;
    let svc = service(Some(gh.clone())).await;

    let r = http("GET", format!("{}/auth/github", svc.base), vec![], None).await;
    assert_eq!(r.status, 303);
    let loc = r.location.unwrap();
    assert!(
        loc.starts_with(&format!("{gh}/login/oauth/authorize?client_id=cid")),
        "{loc}"
    );
    let state_cookie = r
        .cookies
        .iter()
        .find(|c| c.starts_with("avc_oauth="))
        .unwrap()
        .clone();
    let state = state_cookie.trim_start_matches("avc_oauth=").to_string();

    // Wrong state: refused.
    let r = http(
        "GET",
        format!("{}/auth/github/callback?code=x&state=other", svc.base),
        vec![("cookie", state_cookie.clone())],
        None,
    )
    .await;
    assert_eq!(r.status, 400);

    // Bob is not on the allow list.
    let r = http(
        "GET",
        format!("{}/auth/github/callback?code=bob&state={state}", svc.base),
        vec![("cookie", state_cookie.clone())],
        None,
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.body);

    // Ana gets a session (logins match case-insensitively).
    let r = http(
        "GET",
        format!("{}/auth/github/callback?code=ana&state={state}", svc.base),
        vec![("cookie", state_cookie)],
        None,
    )
    .await;
    assert_eq!(r.status, 303, "{}", r.body);
    let session = r
        .cookies
        .into_iter()
        .find(|c| c.starts_with("avc_session=") && c.len() > 12)
        .unwrap();
    let r = http(
        "GET",
        format!("{}/api/me", svc.base),
        vec![("cookie", session.clone())],
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("\"login\":\"Ana\""), "{}", r.body);

    // The home page lists nothing yet and offers a token form.
    let r = http(
        "GET",
        format!("{}/", svc.base),
        vec![("cookie", session)],
        None,
    )
    .await;
    assert!(r.body.contains("New machine token"));
}
