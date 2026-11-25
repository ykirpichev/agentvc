//! The review UI from `ui/dist`: read from disk in debug builds, embedded in
//! release builds. Unknown paths fall back to `index.html` so client-side
//! routes such as `/tasks/3` work on reload.

use axum::http::{header, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use rust_embed::RustEmbed;

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

fn file(path: &str) -> Option<Response> {
    let f = Ui::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
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

pub(crate) async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty() {
        if let Some(r) = file(path) {
            return r;
        }
        // A missing asset is a real 404, not a client-side route.
        if path.starts_with("assets/") || path.rsplit('/').next().is_some_and(|p| p.contains('.')) {
            return (StatusCode::NOT_FOUND, "not found").into_response();
        }
    }
    file("index.html").unwrap_or_else(|| Html(NOT_BUILT).into_response())
}
