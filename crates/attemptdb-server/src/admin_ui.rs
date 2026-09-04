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
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>AttemptDB · admin</title>
<style>
:root {{ --bg:#f7f7f5; --card:#fff; --ink:#1b1b1b; --muted:#6b6b6b; --line:#e3e3de; --accent:#2f6fed; --fail:#c62828; --mono: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg:#131416; --card:#1c1e21; --ink:#e8e8e6; --muted:#9a9a96; --line:#2c2f33; --accent:#7aa2ff; --fail:#ff6b6b; }} }}
body {{ margin:0; background:var(--bg); color:var(--ink); font:14px/1.45 system-ui,-apple-system,"Segoe UI",Roboto,sans-serif; display:grid; place-items:center; min-height:100vh; }}
form {{ background:var(--card); border:1px solid var(--line); border-radius:10px; padding:22px 24px; width:min(420px, 92vw); }}
h1 {{ font-size:17px; margin:0 0 4px; }} p {{ color:var(--muted); margin:0 0 14px; font-size:13px; }}
input {{ width:100%; font:13px var(--mono); padding:9px 10px; border:1px solid var(--line); border-radius:6px; background:var(--bg); color:var(--ink); }}
button {{ margin-top:12px; width:100%; padding:9px; border:0; border-radius:6px; background:var(--accent); color:#fff; font-weight:600; cursor:pointer; }}
.err {{ color:var(--fail); }}
</style></head><body>
<form method="post" action="/admin/login" autocomplete="off">
<h1>AttemptDB · admin</h1><p>The operator token (ATTEMPTDB_ADMIN_TOKEN). It stays on the server; the browser keeps a session for 12 hours.</p>
{err}
<input type="password" name="token" placeholder="admin token" autofocus required>
<button type="submit">Sign in</button>
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

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
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
