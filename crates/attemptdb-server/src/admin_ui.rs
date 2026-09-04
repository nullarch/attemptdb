//! `/admin` — the operator's console, served by the server itself.
//!
//! One page (`assets/admin.html`, embedded) that talks to the same `/v1`
//! API a curl would: tenants and their keys, devices and last syncs, the
//! live state, sessions, timeline, work, attention, raw events, SQL, and
//! the webhook's cursors. Nothing is computed twice: the console is a
//! client of the read API.
//!
//! Sign-in is the admin token, once, in a form; the browser then holds a
//! random session id in an `HttpOnly; SameSite=Strict` cookie (12 h,
//! process-local). Cookie-authenticated calls must also carry
//! `X-Requested-With: attemptdb-admin` — a header a cross-site page cannot
//! add without a preflight the server never grants — so the cookie alone
//! cannot be replayed by another origin. A bearer works exactly as before;
//! the cookie is a second door to the same gate, never a wider one.

use crate::AppState;
use crate::auth::eq_ct;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::{Form, Router, routing::get};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const ADMIN_HTML: &str = include_str!("../assets/admin.html");
/// The product's mark, from the one master the icon is generated from
/// (`assets/icon/render.py`). Served for the browser tab; public, because a
/// logo is not a secret and the sign-in page needs it too.
pub const FAVICON_SVG: &str = include_str!("../../../assets/icon/attemptdb.svg");
const COOKIE: &str = "atdb_admin";
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const MAX_SESSIONS: usize = 100;
/// The header a cookie-authenticated API call must carry (CSRF guard).
pub const REQUESTED_WITH: &str = "attemptdb-admin";

/// Signed-in consoles: session id → expiry.
#[derive(Default)]
pub struct Sessions {
    inner: Mutex<HashMap<String, Instant>>,
}

impl Sessions {
    fn issue(&self) -> String {
        let id =
            uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string();
        if let Ok(mut m) = self.inner.lock() {
            let now = Instant::now();
            m.retain(|_, exp| *exp > now);
            if m.len() >= MAX_SESSIONS {
                // Oldest out: a console left open for days should not
                // block a new sign-in.
                if let Some(oldest) = m.iter().min_by_key(|(_, e)| **e).map(|(k, _)| k.clone()) {
                    m.remove(&oldest);
                }
            }
            m.insert(id.clone(), now + SESSION_TTL);
        }
        id
    }

    fn valid(&self, id: &str) -> bool {
        self.inner
            .lock()
            .map(|m| m.get(id).is_some_and(|exp| *exp > Instant::now()))
            .unwrap_or(false)
    }

    fn revoke(&self, id: &str) {
        if let Ok(mut m) = self.inner.lock() {
            m.remove(id);
        }
    }
}

fn cookie_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.split(';').map(str::trim).find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                (k == COOKIE).then(|| v.trim().to_string())
            })
        })
}

/// A signed-in console: the session cookie is valid. Used by the API gate
/// together with the `X-Requested-With` check.
pub fn session_ok(state: &AppState, headers: &HeaderMap) -> bool {
    match cookie_value(headers) {
        Some(id) if !id.is_empty() => state.admin_sessions.valid(&id),
        _ => false,
    }
}

/// The API gate's cookie door: a valid session AND the custom header.
pub fn cookie_admin(state: &AppState, headers: &HeaderMap) -> bool {
    let marked = headers
        .get("x-requested-with")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == REQUESTED_WITH);
    marked && session_ok(state, headers)
}

fn set_cookie(state: &AppState, value: &str, max_age: u64) -> String {
    // On a loopback bind (tests, a laptop) there is no TLS; everywhere else
    // the cookie is Secure.
    let secure = if state.config.bind.is_loopback() {
        ""
    } else {
        "; Secure"
    };
    format!("{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}")
}

fn login_page(error: Option<&str>) -> Html<String> {
    let err = error
        .map(|e| format!("<p class=\"err\">{}</p>", html_escape(e)))
        .unwrap_or_default();
    Html(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="referrer" content="no-referrer"><link rel="icon" type="image/svg+xml" href="/favicon.svg"><title>AttemptDB · admin console</title>
<style>
:root {{
  color-scheme: light;
  --bg:#f7f7f4; --panel:#fff; --raised:#f1f1ec; --line:#e3e3dc; --ink:#17191c; --muted:#676d76; --faint:#9198a1;
  --accent:#6d28d9; --accent-ink:#fff; --fail:#cf222e;
  --shadow: 0 1px 2px rgba(16,18,22,.06), 0 20px 48px -32px rgba(16,18,22,.5);
  --mono: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace;
  --sans: system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", sans-serif;
}}
@media (prefers-color-scheme: dark) {{
  :root {{
    color-scheme: dark;
    --bg:#0e1014; --panel:#14171c; --raised:#1a1e25; --line:#232830; --ink:#e7eaef; --muted:#8b93a0; --faint:#69717e;
    --accent:#a78bfa; --accent-ink:#17111f; --fail:#f85149;
    --shadow: 0 1px 2px rgba(0,0,0,.5), 0 22px 56px -34px rgba(0,0,0,.95);
  }}
}}
* {{ box-sizing: border-box; }}
body {{
  margin:0; background:var(--bg); color:var(--ink); font:13.5px/1.5 var(--sans);
  -webkit-font-smoothing:antialiased; display:grid; place-items:center; min-height:100vh; padding:24px;
}}
form {{ background:var(--panel); border:1px solid var(--line); border-radius:14px; padding:26px 26px 24px; width:min(400px, 100%); box-shadow:var(--shadow); }}
.brand {{ display:flex; align-items:baseline; gap:7px; font-weight:650; font-size:15px; letter-spacing:-.01em; }}
.brand .mark {{ color:var(--accent); font-family:var(--mono); font-size:16px; }}
.brand .sub {{ color:var(--faint); font-weight:400; font-size:10.5px; text-transform:uppercase; letter-spacing:.09em; }}
p.lede {{ color:var(--muted); margin:10px 0 18px; font-size:12.5px; line-height:1.6; }}
label {{ display:block; font-size:10px; text-transform:uppercase; letter-spacing:.08em; color:var(--faint); margin-bottom:6px; }}
input {{
  width:100%; font:13px var(--mono); padding:10px 11px; border:1px solid var(--line); border-radius:9px;
  background:var(--bg); color:var(--ink);
}}
input:focus {{ outline:none; border-color:var(--accent); box-shadow:0 0 0 3px color-mix(in srgb, var(--accent) 18%, transparent); }}
button {{
  margin-top:14px; width:100%; padding:10px; border:0; border-radius:9px; background:var(--accent);
  color:var(--accent-ink); font:600 13.5px var(--sans); cursor:pointer;
}}
button:hover {{ filter:brightness(1.08); }}
.err {{
  color:var(--fail); font-size:12.5px; margin:0 0 14px; padding:8px 11px; border-radius:8px;
  border:1px solid color-mix(in srgb, var(--fail) 30%, var(--line)); border-left:3px solid var(--fail);
  background:color-mix(in srgb, var(--fail) 8%, transparent);
}}
.foot {{ margin:16px 0 0; font-size:11.5px; color:var(--faint); line-height:1.6; }}
.foot code {{ font-family:var(--mono); background:var(--raised); padding:1px 5px; border-radius:4px; }}
</style></head><body>
<form method="post" action="/admin/login" autocomplete="off">
<div class="brand"><span class="mark">&#9612;</span>AttemptDB<span class="sub">admin console</span></div>
<p class="lede">Every tenant on this server — devices, sessions, events, the webhook's cursors — behind one operator token.</p>
{err}
<label for="token">Operator token</label>
<input id="token" type="password" name="token" placeholder="ATTEMPTDB_ADMIN_TOKEN" autofocus required>
<button type="submit">Sign in</button>
<p class="foot">The token stays on the server; the browser keeps a session for 12 hours. It is the same token <code>ATTEMPTDB_ADMIN_TOKEN</code> the API takes as a bearer.</p>
</form></body></html>"#
    ))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn index(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if state.config.admin_token.is_none() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    if !session_ok(&state, &headers) {
        return Redirect::to("/admin/login").into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        ADMIN_HTML,
    )
        .into_response()
}

async fn login_get(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if state.config.admin_token.is_none() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    if session_ok(&state, &headers) {
        return Redirect::to("/admin").into_response();
    }
    login_page(None).into_response()
}

#[derive(Deserialize)]
struct LoginForm {
    token: String,
}

async fn login_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let Some(expected) = state.config.admin_token.as_deref() else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    // Guesses are as rate limited as pairing attempts, per client address.
    let who = crate::limiter::client_address(&headers).unwrap_or_else(|| "anon".into());
    if let Err(retry) = state.limiter.take(
        &format!("admin-login:{who}"),
        state.config.pair_rate,
        Instant::now(),
    ) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry.to_string())],
            login_page(Some("too many attempts; try again in a minute")),
        )
            .into_response();
    }
    let given = form.token.trim();
    if given.is_empty() || !eq_ct(given.as_bytes(), expected.as_bytes()) {
        return (
            StatusCode::UNAUTHORIZED,
            login_page(Some("that is not the admin token")),
        )
            .into_response();
    }
    let id = state.admin_sessions.issue();
    (
        StatusCode::SEE_OTHER,
        [
            (
                header::SET_COOKIE,
                set_cookie(&state, &id, SESSION_TTL.as_secs()),
            ),
            (header::LOCATION, "/admin".to_string()),
        ],
    )
        .into_response()
}

async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(id) = cookie_value(&headers) {
        state.admin_sessions.revoke(&id);
    }
    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, set_cookie(&state, "", 0)),
            (header::LOCATION, "/admin/login".to_string()),
        ],
    )
        .into_response()
}

async fn favicon() -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        FAVICON_SVG,
    )
        .into_response()
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/favicon.svg", get(favicon))
        .route("/admin", get(index))
        .route("/admin/login", get(login_get).post(login_post))
        .route("/admin/logout", axum::routing::post(logout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_expire_and_are_capped() {
        let s = Sessions::default();
        let a = s.issue();
        assert!(s.valid(&a));
        s.revoke(&a);
        assert!(!s.valid(&a));
        let ids: Vec<String> = (0..MAX_SESSIONS + 5).map(|_| s.issue()).collect();
        assert!(s.inner.lock().unwrap().len() <= MAX_SESSIONS);
        assert!(s.valid(ids.last().unwrap()));
    }

    #[test]
    fn cookie_parsing_finds_ours_among_others() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            "theme=dark; atdb_admin=abc123; other=1".parse().unwrap(),
        );
        assert_eq!(cookie_value(&h).as_deref(), Some("abc123"));
        let mut h2 = HeaderMap::new();
        h2.insert(header::COOKIE, "theme=dark".parse().unwrap());
        assert_eq!(cookie_value(&h2), None);
    }
}
