//! Signing people in: GitHub OAuth (or `/auth/dev` for local testing), a
//! session cookie, and the checks every handler uses.
//!
//! The GitHub access token is used once to read the person's login and then
//! dropped; it is never stored. Only logins on the allow list get a session.

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::{self, User};
use crate::AppState;

pub const SESSION_COOKIE: &str = "avc_session";
const STATE_COOKIE: &str = "avc_oauth";

/// An error page or JSON error with a stable code.
#[derive(Debug)]
pub struct Fail {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl Fail {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Fail {
        Fail {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn internal(e: impl std::fmt::Display) -> Fail {
        Fail::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
    }

    pub fn unauthorized() -> Fail {
        Fail::new(
            StatusCode::UNAUTHORIZED,
            "signed_out",
            "sign in on the home page first",
        )
    }
}

impl From<rusqlite::Error> for Fail {
    fn from(e: rusqlite::Error) -> Fail {
        Fail::internal(e)
    }
}

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "code": self.code, "message": self.message } });
        (self.status, axum::Json(body)).into_response()
    }
}

/// The value of cookie `name`.
pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

fn set_cookie(st: &AppState, name: &str, value: &str, path: &str, max_age: i64) -> HeaderValue {
    let secure = if st.config().secure() { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{name}={value}; Path={path}; Max-Age={max_age}; HttpOnly; SameSite=Lax{secure}"
    ))
    .expect("cookie values are hex")
}

/// The signed-in person, if any.
pub fn current_user(st: &AppState, headers: &HeaderMap) -> Result<Option<User>, Fail> {
    let Some(secret) = cookie(headers, SESSION_COOKIE) else {
        return Ok(None);
    };
    let user = st.db(|d| d.session_user(secret))?;
    // Someone removed from the allow list loses access at once.
    Ok(user.filter(|u| st.config().allows(&u.login)))
}

pub fn require_user(st: &AppState, headers: &HeaderMap) -> Result<User, Fail> {
    current_user(st, headers)?.ok_or_else(Fail::unauthorized)
}

/// Writes from a browser must come from the service's own pages: the
/// session cookie alone would let any site make them (CSRF).
pub fn same_origin(st: &AppState, headers: &HeaderMap) -> Result<(), Fail> {
    let want = st.config().origin();
    let from = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            // Some older browsers send only a Referer on form posts.
            let r = headers.get(header::REFERER)?.to_str().ok()?;
            let after = r.find("://")? + 3;
            let end = r[after..].find('/').map_or(r.len(), |i| after + i);
            Some(r[..end].to_string())
        });
    match from {
        Some(o) if o == want => Ok(()),
        _ => Err(Fail::new(
            StatusCode::FORBIDDEN,
            "cross_origin",
            "this request did not come from the agentvc service's own pages",
        )),
    }
}

fn sign_in(st: &AppState, login: &str, github_id: Option<i64>) -> Result<Response, Fail> {
    if !st.config().allows(login) {
        return Err(Fail::new(
            StatusCode::FORBIDDEN,
            "not_allowed",
            format!("{login} is not allowed to use this agentvc service"),
        ));
    }
    let secret = st.db(|d| {
        let user = d.upsert_user(login, github_id)?;
        d.create_session(user.id)
    })?;
    let mut res = Redirect::to("/").into_response();
    res.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(st, SESSION_COOKIE, &secret, "/", db::SESSION_SECS),
    );
    res.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(st, STATE_COOKIE, "", "/auth", 0),
    );
    Ok(res)
}

pub(crate) async fn dev_login(State(st): State<AppState>) -> Result<Response, Fail> {
    let Some(login) = st.config().dev_user.clone() else {
        return Err(Fail::new(
            StatusCode::NOT_FOUND,
            "no_dev_login",
            "dev sign-in is off",
        ));
    };
    sign_in(&st, &login, None)
}

pub(crate) async fn logout(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, Fail> {
    same_origin(&st, &headers)?;
    if let Some(secret) = cookie(&headers, SESSION_COOKIE) {
        let secret = secret.to_string();
        st.db(|d| d.delete_session(&secret))?;
    }
    let mut res = Redirect::to("/").into_response();
    res.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(&st, SESSION_COOKIE, "", "/", 0),
    );
    Ok(res)
}

fn callback_url(st: &AppState) -> String {
    format!("{}/auth/github/callback", st.config().public_url)
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub(crate) async fn github_start(State(st): State<AppState>) -> Result<Response, Fail> {
    let Some(gh) = st.config().github.clone() else {
        return Err(Fail::new(
            StatusCode::NOT_FOUND,
            "no_github",
            "GitHub sign-in is not configured (GITHUB_CLIENT_ID / GITHUB_CLIENT_SECRET)",
        ));
    };
    let state = db::random_hex(16);
    // No scope: the login is public, and the service needs nothing else.
    let url = format!(
        "{}/login/oauth/authorize?client_id={}&redirect_uri={}&state={}&allow_signup=false",
        gh.web_url,
        encode(&gh.client_id),
        encode(&callback_url(&st)),
        state
    );
    let mut res = Redirect::to(&url).into_response();
    res.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(&st, STATE_COOKIE, &state, "/auth", 600),
    );
    Ok(res)
}

#[derive(Deserialize)]
pub(crate) struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

pub(crate) async fn github_callback(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Result<Response, Fail> {
    let Some(gh) = st.config().github.clone() else {
        return Err(Fail::new(
            StatusCode::NOT_FOUND,
            "no_github",
            "GitHub sign-in is not configured",
        ));
    };
    if let Some(e) = q.error {
        return Err(Fail::new(StatusCode::FORBIDDEN, "github_denied", e));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return Err(Fail::new(
            StatusCode::BAD_REQUEST,
            "bad_callback",
            "missing code or state",
        ));
    };
    if cookie(&headers, STATE_COOKIE) != Some(state.as_str()) {
        return Err(Fail::new(
            StatusCode::BAD_REQUEST,
            "bad_state",
            "sign-in expired or came from elsewhere; start again from the home page",
        ));
    }
    let redirect = callback_url(&st);
    let (login, id) = tokio::task::spawn_blocking(move || github_login(&gh, &code, &redirect))
        .await
        .map_err(Fail::internal)??;
    sign_in(&st, &login, Some(id))
}

/// Trade the OAuth code for a token, read the login, drop the token.
fn github_login(
    gh: &crate::GithubOAuth,
    code: &str,
    redirect: &str,
) -> Result<(String, i64), Fail> {
    let bad = |e: String| Fail::new(StatusCode::BAD_GATEWAY, "github", e);
    let agent = ureq::Agent::new_with_defaults();
    let tok: Value = agent
        .post(&format!("{}/login/oauth/access_token", gh.web_url))
        .header("accept", "application/json")
        .send_form([
            ("client_id", gh.client_id.as_str()),
            ("client_secret", gh.client_secret.as_str()),
            ("code", code),
            ("redirect_uri", redirect),
        ])
        .map_err(|e| bad(e.to_string()))?
        .body_mut()
        .read_json()
        .map_err(|e| bad(e.to_string()))?;
    let Some(access) = tok["access_token"].as_str() else {
        let why = tok["error_description"]
            .as_str()
            .or(tok["error"].as_str())
            .unwrap_or("no access token");
        return Err(bad(why.to_string()));
    };
    let user: Value = agent
        .get(&format!("{}/user", gh.api_url))
        .header("accept", "application/vnd.github+json")
        .header("authorization", &format!("Bearer {access}"))
        .header("user-agent", "agentvc-cloud")
        .call()
        .map_err(|e| bad(e.to_string()))?
        .body_mut()
        .read_json()
        .map_err(|e| bad(e.to_string()))?;
    match (user["login"].as_str(), user["id"].as_i64()) {
        (Some(login), Some(id)) => Ok((login.to_string(), id)),
        _ => Err(bad("GitHub did not return a login".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "a=1; avc_session=abc; b=2".parse().unwrap());
        assert_eq!(cookie(&h, SESSION_COOKIE), Some("abc"));
        assert_eq!(cookie(&h, "c"), None);
    }

    #[test]
    fn origin_check() {
        let mut c = crate::Config::new("https://avc.example.com", "/tmp/x");
        c.allowed_users = vec!["Ana".into()];
        let st = AppState::with_db(c, db::Db::memory().unwrap());
        let mut h = HeaderMap::new();
        assert!(same_origin(&st, &h).is_err());
        h.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(same_origin(&st, &h).is_err());
        h.insert(header::ORIGIN, "https://avc.example.com".parse().unwrap());
        assert!(same_origin(&st, &h).is_ok());
        let mut r = HeaderMap::new();
        r.insert(
            header::REFERER,
            "https://avc.example.com/x".parse().unwrap(),
        );
        assert!(same_origin(&st, &r).is_ok());
        assert!(st.config().allows("ana"));
    }
}
