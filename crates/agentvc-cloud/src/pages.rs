//! The service's own pages: sign in, your spaces, machine tokens. Plain
//! server-rendered HTML with forms, so they work without the UI bundle.

use axum::extract::{Form, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::{current_user, require_user, same_origin, Fail};
use crate::db::{now, SpaceRow, Token, User};
use crate::AppState;

pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn ago(t: i64) -> String {
    let d = (now() - t).max(0);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86399 => format!("{} h ago", d / 3600),
        _ => format!("{} days ago", d / 86400),
    }
}

const STYLE: &str = r#"
:root{--bg:#f7f7f8;--card:#fff;--fg:#1d1f24;--muted:#6b7080;--line:#e3e4e8;--accent:#3b5bdb;--ok:#2b8a3e;--warn:#c92a2a;color-scheme:light}
@media (prefers-color-scheme:dark){:root{--bg:#15161a;--card:#1d1f24;--fg:#e6e7ea;--muted:#9a9fae;--line:#2c2f36;--accent:#7c93f5;--ok:#51cf66;--warn:#ff8787;color-scheme:dark}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",system-ui,sans-serif}
main{max-width:760px;margin:0 auto;padding:32px 16px 64px}
header{display:flex;align-items:center;justify-content:space-between;margin-bottom:24px}
.brand{display:flex;gap:10px;align-items:center;font-weight:600;font-size:16px}
.mark{width:24px;height:24px;border-radius:6px;background:var(--accent);display:inline-block}
h2{font-size:13px;text-transform:uppercase;letter-spacing:.04em;color:var(--muted);margin:28px 0 8px}
.card{background:var(--card);border:1px solid var(--line);border-radius:10px}
.row{display:flex;gap:12px;align-items:center;padding:12px 14px;border-top:1px solid var(--line)}
.row:first-child{border-top:0}.grow{flex:1;min-width:0}
.name{font-weight:600}.sub{color:var(--muted);font-size:12px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.dot{width:8px;height:8px;border-radius:50%;background:var(--muted);flex:none}.dot.on{background:var(--ok)}
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}
.btn{font:inherit;border:1px solid var(--line);background:var(--card);color:var(--fg);border-radius:7px;padding:6px 12px;cursor:pointer}
.btn.primary{background:var(--accent);border-color:var(--accent);color:#fff}.btn.danger{color:var(--warn)}
input[type=text]{font:inherit;padding:6px 10px;border:1px solid var(--line);border-radius:7px;background:var(--bg);color:var(--fg);min-width:0}
form.inline{display:inline;margin:0}
pre{background:var(--bg);border:1px solid var(--line);border-radius:8px;padding:12px;overflow:auto;white-space:pre-wrap;word-break:break-all;font:12px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace}
.empty{padding:16px 14px;color:var(--muted)}
.hero{text-align:center;padding:12vh 0}.hero p{color:var(--muted);max-width:440px;margin:12px auto 24px}
"#;

fn page(title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <link rel=\"icon\" type=\"image/svg+xml\" href=\"/favicon.svg\">\
         <title>{}</title><style>{STYLE}</style></head><body><main>{body}</main></body></html>",
        esc(title)
    );
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; form-action 'self'; frame-ancestors 'none'",
            ),
        ],
        Html(html),
    )
        .into_response()
}

fn header_bar(user: Option<&User>) -> String {
    let right = match user {
        Some(u) => format!(
            "<span class=\"sub\">{} &nbsp;</span><form class=\"inline\" method=\"post\" action=\"/logout\">\
             <button class=\"btn\">Sign out</button></form>",
            esc(&u.login)
        ),
        None => String::new(),
    };
    format!(
        "<header><div class=\"brand\"><span class=\"mark\"></span>agentvc</div><div>{right}</div></header>"
    )
}

fn signed_out(st: &AppState) -> Response {
    let mut buttons = String::new();
    if st.config().github.is_some() {
        buttons.push_str("<a class=\"btn primary\" href=\"/auth/github\">Sign in with GitHub</a> ");
    }
    if st.config().dev_user.is_some() {
        buttons.push_str("<a class=\"btn\" href=\"/auth/dev\">Dev sign-in</a>");
    }
    if buttons.is_empty() {
        buttons.push_str("<span class=\"sub\">No sign-in method is configured.</span>");
    }
    page(
        "agentvc",
        &format!(
            "{}<div class=\"hero\"><h1>Review your agents from anywhere</h1>\
             <p>The agentvc review UI for every machine you connect. Your code stays on your machines; \
             this service only relays the UI to them.</p>{buttons}</div>",
            header_bar(None)
        ),
    )
}

fn connect_help(st: &AppState) -> String {
    format!(
        "<pre>export AGENTVC_CLOUD_TOKEN=&lt;machine token&gt;\nagentvcd --cloud {}</pre>",
        esc(&st.config().public_url)
    )
}

fn spaces_html(st: &AppState, spaces: &[SpaceRow]) -> String {
    if spaces.is_empty() {
        return format!(
            "<div class=\"card\"><div class=\"empty\">No machines yet. Make a machine token below, then on \
             the machine, inside the repo:{}</div></div>",
            connect_help(st)
        );
    }
    let mut rows = String::new();
    for s in spaces {
        let on = st.hub().online(&s.slug);
        let status = if on {
            "online".to_string()
        } else {
            format!("offline, seen {}", ago(s.last_seen))
        };
        let ro = if s.read_only { " · read-only" } else { "" };
        let action = if on {
            format!(
                "<a class=\"btn primary\" href=\"/s/{}/\">Open</a>",
                esc(&s.slug)
            )
        } else {
            format!(
                "<form class=\"inline\" method=\"post\" action=\"/spaces/{}/forget\">\
                 <button class=\"btn\">Forget</button></form>",
                esc(&s.slug)
            )
        };
        rows.push_str(&format!(
            "<div class=\"row\"><span class=\"dot{}\"></span><div class=\"grow\">\
             <div class=\"name\">{}</div><div class=\"sub\">{} · {} · {}{}</div></div>{}</div>",
            if on { " on" } else { "" },
            esc(&s.name),
            esc(&s.machine),
            esc(&s.workdir),
            status,
            ro,
            action
        ));
    }
    format!("<div class=\"card\">{rows}</div>")
}

fn tokens_html(tokens: &[Token]) -> String {
    let mut rows = String::new();
    for t in tokens {
        let used = t
            .last_used_at
            .map(|u| format!("last used {}", ago(u)))
            .unwrap_or_else(|| "never used".into());
        rows.push_str(&format!(
            "<div class=\"row\"><div class=\"grow\"><div class=\"name\">{}</div>\
             <div class=\"sub\">made {} · {}</div></div>\
             <form class=\"inline\" method=\"post\" action=\"/tokens/{}/revoke\">\
             <button class=\"btn danger\">Revoke</button></form></div>",
            esc(&t.name),
            ago(t.created_at),
            used,
            t.id
        ));
    }
    rows.push_str(
        "<div class=\"row\"><form method=\"post\" action=\"/tokens\" style=\"display:flex;gap:8px;flex:1\">\
         <input type=\"text\" name=\"name\" placeholder=\"Machine name, e.g. MacBook\" required maxlength=\"60\" style=\"flex:1\">\
         <button class=\"btn primary\">New machine token</button></form></div>",
    );
    format!("<div class=\"card\">{rows}</div>")
}

pub(crate) async fn home(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, Fail> {
    let Some(user) = current_user(&st, &headers)? else {
        return Ok(signed_out(&st));
    };
    let (spaces, tokens) = st.db(|d| Ok((d.spaces(user.id)?, d.tokens(user.id)?)))?;
    Ok(page(
        "agentvc",
        &format!(
            "{}<h2>Spaces</h2>{}<h2>Machine tokens</h2>{}\
             <p class=\"sub\">A token lets one machine's agentvcd connect here. Revoking it disconnects that machine.</p>",
            header_bar(Some(&user)),
            spaces_html(&st, &spaces),
            tokens_html(&tokens)
        ),
    ))
}

#[derive(Deserialize)]
pub(crate) struct NewToken {
    name: String,
}

fn wants_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"))
}

pub(crate) async fn create_token(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(f): Form<NewToken>,
) -> Result<Response, Fail> {
    let user = require_user(&st, &headers)?;
    same_origin(&st, &headers)?;
    let name: String = f.name.trim().chars().take(60).collect();
    if name.is_empty() {
        return Err(Fail::new(
            StatusCode::BAD_REQUEST,
            "no_name",
            "name the machine",
        ));
    }
    let (tok, secret) = st.db(|d| d.create_token(user.id, &name))?;
    if wants_json(&headers) {
        return Ok(
            Json(json!({ "id": tok.id, "name": tok.name, "token": secret })).into_response(),
        );
    }
    Ok(page(
        "New machine token",
        &format!(
            "{}<h2>Machine token for {}</h2><div class=\"card\"><div class=\"empty\">\
             Copy it now; it is not shown again.<pre>{}</pre>\
             On that machine, inside the repo, start the daemon with it:\
             <pre>export AGENTVC_CLOUD_TOKEN={}\nagentvcd --cloud {}</pre>\
             Add <code>--cloud-read-only</code> to allow only looking, not review actions.</div></div>\
             <p><a href=\"/\">Back to your spaces</a></p>",
            header_bar(Some(&user)),
            esc(&name),
            esc(&secret),
            esc(&secret),
            esc(&st.config().public_url)
        ),
    ))
}

pub(crate) async fn revoke_token(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, Fail> {
    let user = require_user(&st, &headers)?;
    same_origin(&st, &headers)?;
    let slugs: Vec<String> = st.db(|d| {
        Ok(d.spaces(user.id)?
            .into_iter()
            .filter(|s| s.token_id == id)
            .map(|s| s.slug)
            .collect())
    })?;
    if !st.db(|d| d.revoke_token(user.id, id))? {
        return Err(Fail::new(
            StatusCode::NOT_FOUND,
            "no_token",
            "no such token",
        ));
    }
    for s in slugs {
        st.hub().disconnect(&s);
    }
    Ok(Redirect::to("/").into_response())
}

pub(crate) async fn forget_space(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Result<Response, Fail> {
    let user = require_user(&st, &headers)?;
    same_origin(&st, &headers)?;
    st.db(|d| d.forget_space(user.id, &slug))?;
    st.hub().disconnect(&slug);
    Ok(Redirect::to("/").into_response())
}

pub(crate) async fn me(State(st): State<AppState>, headers: HeaderMap) -> Result<Json<User>, Fail> {
    Ok(Json(require_user(&st, &headers)?))
}

#[derive(Serialize)]
pub(crate) struct SpaceJson {
    slug: String,
    name: String,
    machine: String,
    workdir: String,
    version: String,
    read_only: bool,
    online: bool,
    last_seen: i64,
    url: String,
}

pub(crate) async fn spaces_json(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<SpaceJson>>, Fail> {
    let user = require_user(&st, &headers)?;
    let rows = st.db(|d| d.spaces(user.id))?;
    Ok(Json(
        rows.into_iter()
            .map(|s| SpaceJson {
                online: st.hub().online(&s.slug),
                url: format!("{}/s/{}/", st.config().public_url, s.slug),
                slug: s.slug,
                name: s.name,
                machine: s.machine,
                workdir: s.workdir,
                version: s.version,
                read_only: s.read_only,
                last_seen: s.last_seen,
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html() {
        assert_eq!(
            esc("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }
}
