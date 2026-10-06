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
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
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

/// Most buckets one generation holds before the generations rotate.
pub const GENERATION_CAP: usize = 25_000;

#[derive(Debug, Default)]
struct Buckets {
    /// Keys seen since the last rotation.
    fresh: HashMap<String, Bucket>,
    /// Keys seen before it. A key used again moves to `fresh`; one that is
    /// not is gone at the next rotation.
    stale: HashMap<String, Bucket>,
}

/// Buckets by key; a key is a credential digest or a client address. Bounded
/// at twice [`GENERATION_CAP`] entries by two generations that rotate when
/// the fresh one fills: every operation is O(1) amortised (the rotation drops
/// a map in one move; nothing ever scans the table), so a flood of distinct
/// keys costs memory up to a fixed ceiling and no more CPU than any other
/// request. A key forgotten by a rotation starts again with a full bucket —
/// the price of the bound.
#[derive(Debug, Default)]
pub struct Limiter {
    inner: Mutex<Buckets>,
}

impl Limiter {
    /// Take one token for `key` at `rate`; `Err(retry_after_secs)` when
    /// the bucket is empty.
    pub fn take(&self, key: &str, rate: Rate, now: Instant) -> Result<(), u64> {
        let mut m = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if !m.fresh.contains_key(key) {
            let moved = m.stale.remove(key).unwrap_or(Bucket {
                tokens: rate.burst,
                at: now,
            });
            if m.fresh.len() >= GENERATION_CAP {
                m.stale = std::mem::take(&mut m.fresh);
            }
            m.fresh.insert(key.to_string(), moved);
        }
        let b = m.fresh.get_mut(key).expect("just inserted");
        let elapsed = now.saturating_duration_since(b.at).as_secs_f64();
        b.tokens = (b.tokens + elapsed * rate.per_second).min(rate.burst);
        b.at = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - b.tokens) / rate.per_second).ceil().max(1.0) as u64)
        }
    }

    /// Buckets held (both generations).
    pub fn len(&self) -> usize {
        let m = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        m.fresh.len() + m.stale.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The client address a request is limited under, resolved once per request
/// by [`middleware`] and stored in its extensions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientAddr(pub String);

/// The address a request came from.
///
/// Only the header named in [`crate::ServerConfig::client_ip_header`] is
/// believed, because only the operator knows which header their proxy
/// overwrites: `fly-client-ip` is set by Fly's edge and cannot be supplied by
/// a client, while `X-Forwarded-For` and `X-Real-IP` are whatever the client
/// sent unless the proxy replaces them. With the header unset or absent, the
/// socket's peer address is used. A header with a list (`a, b, c`) takes its
/// last entry — the one the trusted proxy appended — never the first, which a
/// client can write.
pub(crate) fn client_address(
    header_name: Option<&str>,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> String {
    if let Some(name) = header_name
        && let Some(v) = headers.get(name).and_then(|v| v.to_str().ok())
    {
        let last = v.rsplit(',').next().unwrap_or("").trim();
        if !last.is_empty() && last.len() <= 64 {
            return last.to_string();
        }
    }
    peer.map(|p| p.ip().to_string())
        .unwrap_or_else(|| "anon".to_string())
}

fn bearer_digest(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, k)| crate::auth::digest_hex(k.trim()))
}

pub async fn middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let addr = client_address(
        state.config.client_ip_header.as_deref(),
        req.headers(),
        peer,
    );
    req.extensions_mut().insert(ClientAddr(addr.clone()));
    let path = req.uri().path();
    let (key, rate) = if path.starts_with("/v1/pair") {
        (format!("pair:{addr}"), state.config.pair_rate)
    } else if crate::admin::gate(&state, req.headers()).is_ok() {
        // The operator (the product's backend) answers to its own users'
        // limits; a bucket here would throttle everyone at once.
        return next.run(req).await;
    } else if let Some(d) = bearer_digest(req.headers()) {
        // A bucket per *known* key. A bearer string the server has never
        // issued must not allocate one — that would let a stranger grow the
        // table with every guess — so it is limited by where it came from.
        let known = state
            .authenticate(
                req.headers()
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok()),
            )
            .is_some();
        if known {
            (d, state.config.key_rate)
        } else {
            (format!("unknown:{addr}"), state.config.key_rate)
        }
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

    #[test]
    fn a_flood_of_distinct_keys_is_bounded_and_never_scans_the_table() {
        let l = Limiter::default();
        let rate = Rate::new(1.0, 1.0);
        let t0 = Instant::now();
        let start = std::time::Instant::now();
        // Far more keys than two generations hold, each a new bucket.
        for i in 0..(GENERATION_CAP * 6) {
            assert!(l.take(&format!("flood-{i}"), rate, t0).is_ok());
        }
        assert!(
            l.len() <= 2 * GENERATION_CAP,
            "{} buckets held; the ceiling is {}",
            l.len(),
            2 * GENERATION_CAP
        );
        // 150k insertions with no O(n) retain on the way: well under a second
        // even unoptimised (the old retain-per-request was quadratic).
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_key_in_use_survives_a_rotation_and_an_idle_one_does_not() {
        let l = Limiter::default();
        let rate = Rate::new(0.001, 2.0);
        let t0 = Instant::now();
        assert!(l.take("busy", rate, t0).is_ok());
        assert!(l.take("idle", rate, t0).is_ok());
        // Fill one generation: "busy" and "idle" are now the stale one.
        for i in 0..GENERATION_CAP {
            let _ = l.take(&format!("filler-{i}"), rate, t0);
        }
        // "busy" is used again (moved to the fresh generation) …
        assert!(l.take("busy", rate, t0).is_ok());
        assert!(
            l.take("busy", rate, t0).is_err(),
            "busy kept its spent bucket across the rotation"
        );
        // … then another full generation passes: "idle" is forgotten and
        // starts again with a full bucket.
        for i in 0..GENERATION_CAP {
            let _ = l.take(&format!("filler2-{i}"), rate, t0);
        }
        assert!(l.take("idle", rate, t0).is_ok());
        assert!(l.take("idle", rate, t0).is_ok());
    }

    #[test]
    fn only_the_named_header_is_believed_and_a_list_gives_its_last_entry() {
        let mut h = HeaderMap::new();
        h.insert("fly-client-ip", "203.0.113.7".parse().unwrap());
        h.insert("x-forwarded-for", "1.2.3.4, 198.51.100.9".parse().unwrap());
        h.insert("x-real-ip", "9.9.9.9".parse().unwrap());
        let peer: SocketAddr = "10.0.0.5:4000".parse().unwrap();
        // Default: Fly's header, and nothing a client can also write.
        assert_eq!(
            client_address(Some("fly-client-ip"), &h, Some(peer)),
            "203.0.113.7"
        );
        // XFF is believed only when named, and then by its last entry (the one
        // the trusted proxy appended), not the first (the client's).
        assert_eq!(
            client_address(Some("x-forwarded-for"), &h, Some(peer)),
            "198.51.100.9"
        );
        // Not named: ignored. A client that sends them gets no say.
        let mut spoof = HeaderMap::new();
        spoof.insert("x-forwarded-for", "6.6.6.6".parse().unwrap());
        spoof.insert("x-real-ip", "6.6.6.6".parse().unwrap());
        assert_eq!(
            client_address(Some("fly-client-ip"), &spoof, Some(peer)),
            "10.0.0.5"
        );
        // No trusted header configured: the socket.
        assert_eq!(client_address(None, &h, Some(peer)), "10.0.0.5");
        assert_eq!(client_address(None, &h, None), "anon");
        // An over-long value is not an address.
        let mut long = HeaderMap::new();
        long.insert("fly-client-ip", "a".repeat(200).parse().unwrap());
        assert_eq!(
            client_address(Some("fly-client-ip"), &long, Some(peer)),
            "10.0.0.5"
        );
    }
}
