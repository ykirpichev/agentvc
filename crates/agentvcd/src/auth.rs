//! Who may talk to the daemon (#199).
//!
//! Every request must be addressed to the loopback name it arrived on: the
//! `Host` header is `localhost`, `127.0.0.1` or `[::1]` with an optional
//! numeric port and nothing else (no `user@`), which defeats DNS rebinding.
//! A request that carries an `Origin` (every cross-site request and every
//! WebSocket handshake a browser makes) must come from exactly that
//! `http://<host>`: another localhost port is another site.
//!
//! Requests that change anything (any method but GET, HEAD and OPTIONS) must
//! also carry the daemon's token in the `x-agentvc-token` header. The token
//! lives in `$AGENTVC_HOME/agentvcd.token` (default `~/.agentvc`), readable by
//! this user alone; editors, the Claude panel and scripts read it from there,
//! and the review UI finds it in the pages the daemon serves (see `assets`):
//! the strict `Host` check and the absence of any CORS header keep those
//! pages unreadable to other sites. Note that a local process that can reach
//! the port can read a page too; the token stops web pages, not local code.

use std::io::Write;
use std::path::{Path, PathBuf};

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::api::ApiError;
use crate::AppState;

/// The header that carries the daemon token.
pub const TOKEN_HEADER: &str = "x-agentvc-token";

/// Where the daemon keeps its token: `$AGENTVC_HOME/agentvcd.token`.
pub fn token_file() -> Option<PathBuf> {
    agentvc_core::checkouts::home().map(|h| h.join("agentvcd.token"))
}

/// A fresh random token: 32 bytes, hex.
pub fn new_token() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("the OS random number generator failed");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The token in `path`, made (mode 600) when it is missing or empty. Every
/// daemon of this user shares it, and it survives restarts so the review UI
/// in a browser keeps working.
pub fn load_or_create_token(path: &Path) -> std::io::Result<String> {
    if let Some(t) = read_token(path)? {
        return Ok(t);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write a private file under a unique name and link it into place: the
    // link fails when another daemon got there first, and then theirs wins.
    let token = new_token();
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    {
        let mut f = private_create(&tmp)?;
        f.write_all(token.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(token),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match read_token(path)? {
            Some(t) => Ok(t),
            // An empty file someone left: replace it.
            None => {
                let mut f = private_create_truncate(path)?;
                f.write_all(token.as_bytes())?;
                f.write_all(b"\n")?;
                Ok(token)
            }
        },
        Err(e) => Err(e),
    }
}

fn read_token(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => {
            let t = t.trim();
            if t.is_empty() {
                return Ok(None);
            }
            keep_private(path)?;
            Ok(Some(t.to_string()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn keep_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn keep_private(_: &Path) -> std::io::Result<()> {
    Ok(())
}

fn private_create(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

fn private_create_truncate(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let f = o.open(path)?;
    keep_private(path)?;
    Ok(f)
}

/// True for `localhost`, `127.0.0.1` or `[::1]`, alone or with a numeric
/// port: the whole value, so `localhost:1@evil.com` and `localhost.evil.com`
/// are refused.
pub(crate) fn is_local_host(host: &str) -> bool {
    let (name, port) = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((name, after)) if name == "::1" => match after {
                "" => (name, None),
                a => match a.strip_prefix(':') {
                    Some(p) => (name, Some(p)),
                    None => return false,
                },
            },
            _ => return false,
        }
    } else {
        match host.split_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (host, None),
        }
    };
    let port_ok =
        port.is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()));
    port_ok && (name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "::1")
}

/// The `host[:port]` of an `http://` (or `https://`) origin, if it is one.
fn origin_authority(origin: &str) -> Option<&str> {
    let lower = origin.get(..8).map(str::to_ascii_lowercase);
    let rest = if lower.as_deref().is_some_and(|l| l.starts_with("http://")) {
        &origin[7..]
    } else if lower.as_deref() == Some("https://") {
        &origin[8..]
    } else {
        return None;
    };
    is_local_host(rest).then_some(rest)
}

/// Same-origin: the page's `Origin` names the very `host:port` the request
/// was sent to. A proxy in front (the UI's dev server) keeps its own `Host`,
/// so it matches too.
pub(crate) fn same_origin(origin: &str, host: &str) -> bool {
    origin_authority(origin).is_some_and(|a| a.eq_ignore_ascii_case(host))
}

fn changes_state(m: &Method) -> bool {
    !matches!(*m, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Compare without stopping at the first difference.
fn token_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn check(st: &AppState, method: &Method, h: &HeaderMap) -> Result<(), ApiError> {
    let host = h
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|h| is_local_host(h))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden_host",
                "agentvcd only answers requests addressed to localhost".into(),
            )
        })?;
    if let Some(o) = h.get(header::ORIGIN) {
        if !o.to_str().is_ok_and(|o| same_origin(o, host)) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden_origin",
                format!("agentvcd only answers pages it serves itself (http://{host})"),
            ));
        }
    }
    if changes_state(method) {
        let where_ = st
            .token_file()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "$AGENTVC_HOME/agentvcd.token".into());
        match h.get(TOKEN_HEADER).map(|v| v.to_str().unwrap_or("")) {
            None => {
                return Err(ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "token_required",
                    format!(
                        "changes need agentvcd's token: reload the review UI, or send the \
                         token in {where_} as the {TOKEN_HEADER} header"
                    ),
                ))
            }
            Some(t) if !token_eq(t.trim(), st.token()) => {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "bad_token",
                    format!(
                        "wrong agentvcd token: reload the review UI, or use the token in \
                         {where_}"
                    ),
                ))
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// The middleware in front of every route.
pub(crate) async fn guard(State(st): State<AppState>, req: Request, next: Next) -> Response {
    match check(&st, req.method(), req.headers()) {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_hosts() {
        for ok in [
            "localhost",
            "LocalHost:7420",
            "localhost:7420",
            "127.0.0.1:1",
            "[::1]:7420",
            "[::1]",
        ] {
            assert!(is_local_host(ok), "{ok}");
        }
        for bad in [
            "evil.com",
            "localhost.evil.com",
            "127.0.0.2",
            "evil.com:7420",
            "localhost:1@evil.com",
            "localhost:7420@evil.com:80",
            "user@localhost:7420",
            "localhost:",
            "localhost:7420:1",
            "localhost:abc",
            "[::1]x",
            "[::2]:7420",
            "",
        ] {
            assert!(!is_local_host(bad), "{bad}");
        }
    }

    #[test]
    fn origins() {
        assert!(same_origin("http://localhost:7420", "localhost:7420"));
        assert!(same_origin("http://127.0.0.1:5173", "127.0.0.1:5173"));
        assert!(!same_origin("http://localhost:5173", "localhost:7420"));
        assert!(!same_origin("http://127.0.0.1:7420", "localhost:7420"));
        assert!(!same_origin("https://evil.com", "localhost:7420"));
        assert!(!same_origin(
            "http://localhost:7420@evil.com",
            "localhost:7420"
        ));
        assert!(!same_origin("http://localhost:7420/", "localhost:7420"));
        assert!(!same_origin("null", "localhost:7420"));
        assert!(!same_origin("vscode-webview://abc", "localhost:7420"));
    }

    #[test]
    fn token_file_is_private_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("home").join("agentvcd.token");
        let t = load_or_create_token(&p).unwrap();
        assert_eq!(t.len(), 64);
        assert_eq!(load_or_create_token(&p).unwrap(), t);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(load_or_create_token(&p).unwrap(), t);
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(&p, "").unwrap();
        let t2 = load_or_create_token(&p).unwrap();
        assert_ne!(t2, t);
        assert_eq!(std::fs::read_to_string(&p).unwrap().trim(), t2);
    }
}
