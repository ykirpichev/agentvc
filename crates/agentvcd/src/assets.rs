//! The review UI from `ui/dist`: read from disk in debug builds, embedded in
//! release builds. Unknown paths fall back to `index.html` so client-side
//! routes such as `/tasks/3` work on reload.
//!
//! Every HTML page carries the daemon token in
//! `<meta name="agentvc-token">` (#199), so the UI can make changes in any
//! tab or frame. Only pages the guard let through get it: the `Host` is a
//! loopback name (no DNS rebinding) and no `Access-Control-Allow-Origin` is
//! ever sent, so other sites cannot read it.

use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::AppState;

#[derive(RustEmbed)]
#[folder = "../../ui/dist"]
#[allow_missing = true]
struct Ui;

const NOT_BUILT: &str = r#"<!doctype html>
<meta charset="utf-8"><title>agentvc</title>
<style>body{font:15px/1.5 system-ui,sans-serif;max-width:36rem;margin:15vh auto;padding:0 1rem;color:#334}
code{background:#eef;padding:.1em .3em;border-radius:4px}</style>
<h1>agentvcd is running</h1>
<p>The review UI has not been built. Run <code>npm ci &amp;&amp; npm run build</code> in
<code>ui/</code> and reload, or use <code>npm run dev</code> for live reload.</p>
<p>The JSON API is at <a href="/api/tasks"><code>/api/tasks</code></a>.</p>"#;

/// `html` with the token's meta tag at the top of its `<head>`.
fn with_token(html: &str, token: &str) -> String {
    // The token is hex; escape anyway so no token can break out.
    let t: String = token
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let meta = format!(r#"<meta name="agentvc-token" content="{t}">"#);
    let at = html
        .find("<head>")
        .map(|i| i + "<head>".len())
        .or_else(|| html.find("<meta charset=\"utf-8\">").map(|i| i + 22))
        .unwrap_or(0);
    format!("{}{meta}{}", &html[..at], &html[at..])
}

/// An HTML page with the token; never stored by caches.
fn page(html: &str, token: &str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        with_token(html, token),
    )
        .into_response()
}

fn file(path: &str, token: &str) -> Option<Response> {
    let f = Ui::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    if mime.essence_str() == "text/html" {
        return Some(page(&String::from_utf8_lossy(&f.data), token));
    }
    // Vite fingerprints everything under assets/, so it can be cached forever.
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Some(
        (
            [
                (header::CONTENT_TYPE, mime.as_ref().to_string()),
                (header::CACHE_CONTROL, cache.to_string()),
            ],
            f.data,
        )
            .into_response(),
    )
}

pub(crate) async fn serve(State(st): State<AppState>, uri: Uri) -> Response {
    let token = st.token();
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty() && path != "index.html" {
        if let Some(r) = file(path, token) {
            return r;
        }
        // A missing asset is a real 404, not a client-side route. Pages
        // under `review/` can end in a file name (the Code tab, #172).
        let page = path.starts_with("review/");
        if path.starts_with("assets/")
            || (!page && path.rsplit('/').next().is_some_and(|p| p.contains('.')))
        {
            return (StatusCode::NOT_FOUND, "not found").into_response();
        }
    }
    file("index.html", token).unwrap_or_else(|| page(NOT_BUILT, token))
}
