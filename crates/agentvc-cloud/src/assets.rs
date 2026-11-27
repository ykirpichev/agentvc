//! The review UI build (`ui/dist`), the same one the local daemon serves.
//! `/s/<slug>/...` gets `index.html` (the UI reads its base path from the
//! URL); fingerprinted files live at `/assets/...`.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Redirect, Response};
use rust_embed::RustEmbed;

use crate::auth::current_user;
use crate::AppState;

#[derive(RustEmbed)]
#[folder = "../../ui/dist"]
#[allow_missing = true]
struct Ui;

const NOT_BUILT: &str = r#"<!doctype html><meta charset="utf-8"><title>agentvc</title>
<p>The review UI was not built into this binary. Run <code>npm ci && npm run build</code> in <code>ui/</code> and rebuild.</p>"#;

fn file(path: &str) -> Option<Response> {
    let f = Ui::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
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

/// `/s/<slug>[/...]`: the UI shell, for a signed-in person.
pub(crate) async fn space_page(
    State(st): State<AppState>,
    Path(params): Path<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    let slug = params
        .iter()
        .find(|(k, _)| k == "slug")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    match current_user(&st, &headers) {
        Ok(Some(u)) => {
            let mine = st
                .db(|d| d.space(slug))
                .ok()
                .flatten()
                .is_some_and(|s| s.user_id == u.id);
            if !mine {
                return (StatusCode::NOT_FOUND, "no such space").into_response();
            }
        }
        _ => return Redirect::to("/").into_response(),
    }
    file("index.html").unwrap_or_else(|| Html(NOT_BUILT).into_response())
}

/// Anything else: a file of the build, or 404.
pub(crate) async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty() && path != "index.html" {
        if let Some(r) = file(path) {
            return r;
        }
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}
