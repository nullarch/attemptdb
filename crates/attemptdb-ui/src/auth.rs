//! Access control for the local server.
//!
//! - A 32-byte random token is generated per run. The first visit passes it
//!   as `?token=`; the server answers with a `HttpOnly; SameSite=Strict`
//!   cookie and redirects to the same URL without the token. Every later
//!   request must carry the cookie. Anything else is `401`.
//! - When bound to loopback, the `Host` header must name loopback too, so a
//!   DNS-rebinding page cannot reach the server through a browser.
//! - Every response carries a strict Content-Security-Policy and the other
//!   hardening headers.

use crate::{AppState, COOKIE_NAME, html};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'";

/// Constant-time comparison of two byte strings.
pub fn eq_ct(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 32 random bytes as 64 lowercase hex characters.
pub fn new_token() -> String {
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    let mut s = String::with_capacity(64);
    for byte in a.as_bytes().iter().chain(b.as_bytes().iter()) {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// The `token` query parameter, if present.
fn token_param(uri: &Uri) -> Option<String> {
    let q = uri.query()?;
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == "token" {
            return Some(v.to_string());
        }
    }
    None
}

/// The same URI without the `token` parameter.
fn strip_token(uri: &Uri) -> String {
    let path = uri.path();
    let rest: Vec<&str> = uri
        .query()
        .map(|q| {
            q.split('&')
                .filter(|p| !p.starts_with("token=") && *p != "token")
                .collect()
        })
        .unwrap_or_default();
    if rest.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", rest.join("&"))
    }
}

fn cookie_value(req: &Request) -> Option<String> {
    for raw in req.headers().get_all(header::COOKIE) {
        let Ok(text) = raw.to_str() else { continue };
        for part in text.split(';') {
            let part = part.trim();
            if let Some(v) = part
                .strip_prefix(COOKIE_NAME)
                .and_then(|r| r.strip_prefix('='))
            {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn host_is_loopback(req: &Request) -> bool {
    let Some(host) = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
    else {
        return false;
    };
    let host = host.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    matches!(name, "127.0.0.1" | "localhost" | "::1") || name.starts_with("127.")
}

pub fn unauthorized() -> Response {
    let body = html::bare(
        "Unauthorized",
        "<section class=\"card\"><h1>401 · this page needs the token</h1>\
         <p>Open the URL printed by <code>attempt ui</code> (it carries a one-time <code>?token=</code>); \
         the browser then keeps a session cookie. Restart <code>attempt ui</code> to get a new token.</p></section>",
    );
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Token / cookie gate.
pub async fn guard(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if state.loopback_only && !host_is_loopback(&req) {
        return (
            StatusCode::FORBIDDEN,
            "403: Host header does not name the loopback interface",
        )
            .into_response();
    }
    let expected = state.token.as_bytes();
    if let Some(t) = token_param(req.uri()) {
        if eq_ct(t.as_bytes(), expected) {
            let cookie = format!(
                "{COOKIE_NAME}={}; Path=/; HttpOnly; SameSite=Strict",
                state.token
            );
            let location = strip_token(req.uri());
            return Response::builder()
                .status(StatusCode::SEE_OTHER)
                .header(header::LOCATION, location)
                .header(header::SET_COOKIE, cookie)
                .body(Body::empty())
                .unwrap_or_else(|_| unauthorized());
        }
        return unauthorized();
    }
    match cookie_value(&req) {
        Some(c) if eq_ct(c.as_bytes(), expected) => next.run(req).await,
        _ => unauthorized(),
    }
}

/// Hardening headers on every response.
pub async fn headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_helpers() {
        let t = new_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t, new_token());
        assert!(eq_ct(b"abc", b"abc"));
        assert!(!eq_ct(b"abc", b"abd"));
        assert!(!eq_ct(b"abc", b"ab"));
        let uri: Uri = "/timeline?project=x&token=abc&page=2".parse().unwrap();
        assert_eq!(token_param(&uri).as_deref(), Some("abc"));
        assert_eq!(strip_token(&uri), "/timeline?project=x&page=2");
        let uri: Uri = "/?token=abc".parse().unwrap();
        assert_eq!(strip_token(&uri), "/");
    }
}
