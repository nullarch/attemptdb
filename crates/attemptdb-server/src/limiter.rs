//! Request rate limits: a token bucket per credential, and a stricter one
//! per client address for the unauthenticated pairing routes.
//!
//! Every route but `/v1/health` needs a bearer key, so a leaked key is the
//! one thing that could hammer the server; `/v1/pair*` needs nothing but a
//! token, so it is limited by address. Both buckets live in memory (this
//! is a one-process server) and refill continuously; a request over the
//! limit gets `429` with `Retry-After`. No dependency: a hash map and a
//! clock.

use crate::AppState;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rate {
    /// Sustained requests per second.
    pub per_second: f64,
    /// How many may arrive at once before the sustained rate applies.
    pub burst: f64,
}

impl Rate {
    pub const fn new(per_second: f64, burst: f64) -> Self {
        Self { per_second, burst }
    }
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

/// Buckets by key; a key is a credential digest or a client address.
#[derive(Debug, Default)]
pub struct Limiter {
    inner: Mutex<HashMap<String, Bucket>>,
}

impl Limiter {
    /// Take one token for `key` at `rate`; `Err(retry_after_secs)` when
    /// the bucket is empty.
    pub fn take(&self, key: &str, rate: Rate, now: Instant) -> Result<(), u64> {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if map.len() > 100_000 {
            // A flood of distinct keys: forget the stale rather than grow.
            map.retain(|_, b| now.duration_since(b.at).as_secs() < 60);
        }
        let b = map.entry(key.to_string()).or_insert(Bucket {
            tokens: rate.burst,
            at: now,
        });
        let elapsed = now.duration_since(b.at).as_secs_f64();
        b.tokens = (b.tokens + elapsed * rate.per_second).min(rate.burst);
        b.at = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - b.tokens) / rate.per_second).ceil().max(1.0) as u64)
        }
    }
}

/// The client address as the proxy reports it (Fly, most reverse proxies),
/// else nothing — the server does not track sockets.
fn client_address(headers: &HeaderMap) -> Option<String> {
    for name in ["fly-client-ip", "x-real-ip", "x-forwarded-for"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let first = v.split(',').next().unwrap_or("").trim();
            if !first.is_empty() {
                return Some(first.to_string());
            }
        }
    }
    None
}

fn bearer_digest(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, k)| crate::auth::digest_hex(k.trim()))
}

pub async fn middleware(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let (key, rate) = if path.starts_with("/v1/pair") {
        (
            client_address(req.headers()).unwrap_or_else(|| "anon".into()),
            state.config.pair_rate,
        )
    } else if let Some(d) = bearer_digest(req.headers()) {
        (d, state.config.key_rate)
    } else {
        return next.run(req).await;
    };
    match state.limiter.take(&key, rate, Instant::now()) {
        Ok(()) => next.run(req).await,
        Err(retry) => (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry.to_string())],
            axum::Json(json!({
                "error": "rate limit exceeded",
                "retry_after_secs": retry,
            })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_bucket_allows_the_burst_then_refills_at_the_rate() {
        let l = Limiter::default();
        let rate = Rate::new(2.0, 3.0);
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(l.take("k", rate, t0).is_ok());
        }
        assert_eq!(l.take("k", rate, t0), Err(1));
        // Half a second later: one token back.
        let t1 = t0 + Duration::from_millis(500);
        assert!(l.take("k", rate, t1).is_ok());
        assert!(l.take("k", rate, t1).is_err());
        // Another key is its own bucket.
        assert!(l.take("other", rate, t1).is_ok());
    }
}
