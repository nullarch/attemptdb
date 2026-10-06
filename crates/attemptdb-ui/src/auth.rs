//! Access control for the local server.
//!
//! - A 32-byte random token is generated per run. The first visit passes it
//!   as `?token=`; the server answers with a `Path=/; HttpOnly;
//!   SameSite=Strict` cookie whose name carries the listening port (two UI
//!   instances on one machine do not evict each other's cookie) and
//!   redirects to the same URL without the token. Every later request must
//!   carry the cookie. Anything else is `401`.
//! - When bound to loopback, the `Host` header must be exactly `localhost`,
//!   `127.0.0.1` or `[::1]`, with the listening port if it has one, so a
//!   DNS-rebinding page (`127.evil.example`) cannot reach the server through
//!   a browser.
//! - An API or state-changing request that carries an `Origin` header must
//!   carry the UI's own origin: a page on another site cannot drive the UI
//!   with the visitor's cookie, with or without CORS.
//! - Every response carries a strict Content-Security-Policy and the other
//!   hardening headers.

use crate::{AppState, html};
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

fn cookie_value(req: &Request, name: &str) -> Option<String> {
    for raw in req.headers().get_all(header::COOKIE) {
        let Ok(text) = raw.to_str() else { continue };
        for part in text.split(';') {
            let part = part.trim();
            if let Some(v) = part.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Split a `Host` header (or the authority of an `Origin`) into its host
/// name and optional port. `[::1]:8080` gives `("::1", Some(8080))`.
fn split_authority(authority: &str) -> Option<(String, Option<u16>)> {
    let a = authority.trim();
    if a.is_empty() || a.contains(['/', '@', ' ', '\\']) {
        return None;
    }
    let (host, port) = if let Some(rest) = a.strip_prefix('[') {
        let (h, after) = rest.split_once(']')?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':')?),
        };
        (h, port)
    } else {
        match a.rsplit_once(':') {
            // More than one colon without brackets is not a host:port.
            Some((h, _)) if h.contains(':') => return None,
            Some((h, p)) => (h, Some(p)),
            None => (a, None),
        }
    };
    let port = match port {
        None => None,
        Some(p) => Some(p.parse::<u16>().ok()?),
    };
    Some((host.to_ascii_lowercase(), port))
}

/// Whether `authority` names this machine's loopback exactly: `localhost`,
/// `127.0.0.1` or `[::1]`, with no port or the listening one. Anything else,
/// including `127.evil.example` and `127.0.0.2`, is not.
pub(crate) fn authority_is_loopback(authority: &str, listening_port: u16) -> bool {
    match split_authority(authority) {
        Some((host, port)) => {
            matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1")
                && port.is_none_or(|p| p == listening_port)
        }
        None => false,
    }
}

fn host_header(req: &Request) -> Option<&str> {
    req.headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
}

fn host_is_loopback(req: &Request, listening_port: u16) -> bool {
    host_header(req).is_some_and(|h| authority_is_loopback(h, listening_port))
}

/// Whether the `Origin` header, when there is one, is this server's own:
/// `http(s)://` plus exactly the `Host` the request was addressed to.
/// Requests without an `Origin` (curl, navigation, same-origin GETs) pass.
fn origin_is_own(req: &Request) -> bool {
    let Some(origin) = req.headers().get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some(host) = host_header(req) else {
        return false;
    };
    let Some(authority) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    authority.eq_ignore_ascii_case(host.trim())
}

/// Requests whose `Origin` must be our own: everything under `/api/` and
/// anything that is not a plain read.
fn needs_origin_check(req: &Request) -> bool {
    req.uri().path().starts_with("/api/")
        || !matches!(
            *req.method(),
            axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
        )
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
    if state.loopback_only && !host_is_loopback(&req, state.port) {
        return (
            StatusCode::FORBIDDEN,
            "403: Host header does not name the loopback interface",
        )
            .into_response();
    }
    if needs_origin_check(&req) && !origin_is_own(&req) {
        return (
            StatusCode::FORBIDDEN,
            "403: Origin header is not this server's own origin",
        )
            .into_response();
    }
    let expected = state.token.as_bytes();
    if let Some(t) = token_param(req.uri()) {
        if eq_ct(t.as_bytes(), expected) {
            let cookie = format!(
                "{}={}; Path=/; HttpOnly; SameSite=Strict",
                state.cookie_name, state.token
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
    match cookie_value(&req, &state.cookie_name) {
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

    #[test]
    fn only_the_loopback_names_pass_the_host_check() {
        for ok in [
            "127.0.0.1",
            "127.0.0.1:8080",
            "localhost",
            "localhost:8080",
            "LOCALHOST:8080",
            "[::1]",
            "[::1]:8080",
        ] {
            assert!(authority_is_loopback(ok, 8080), "{ok}");
        }
        for bad in [
            "127.evil.example",
            "127.evil.example:8080",
            "127.0.0.1.evil.example",
            "127.0.0.2",
            "127.1",
            "0.0.0.0",
            "localhost.evil.example",
            "evil.example",
            "evil.example:8080",
            "127.0.0.1:9",
            "localhost:80",
            "[::1]:9",
            "::1",
            "[::1",
            "127.0.0.1:notaport",
            "127.0.0.1:8080:8080",
            "127.0.0.1/",
            "user@127.0.0.1",
            "",
        ] {
            assert!(!authority_is_loopback(bad, 8080), "{bad:?}");
        }
    }
}
