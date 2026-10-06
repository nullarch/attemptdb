//! A request body over the limit is answered with a readable 413.
//!
//! The limit itself is enforced where the body is read, but a server that
//! answers 413 and closes while the client is still writing makes the client's
//! write fail with a broken pipe or a reset *before* it can read the answer:
//! to it the server looks down, and it retries the same oversized event for
//! ever. This layer reads the body itself. A body within the limit is
//! buffered and handed on (the handlers buffer it anyway); one over it is
//! *drained* — read and discarded, up to a cap and a time budget — and only
//! then refused, so the client has finished writing when the 413 arrives. A
//! body past the cap is not read to its end: the answer carries
//! `Connection: close` and the connection is dropped, which a client with a
//! request that large to resend has no use for anyway.

use crate::AppState;
use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::StreamExt;

/// How long a refused body may keep arriving while it is being discarded.
pub const DRAIN_TIME: Duration = Duration::from_secs(20);

/// The most a refused body is read for: sixteen times the limit, at least
/// 32 MiB.
pub fn drain_cap(limit: usize) -> usize {
    limit.saturating_mul(16).max(32 << 20)
}

fn too_large(limit: usize, close: bool) -> Response {
    let body = Json(json!({
        "error": format!("request body is larger than the {limit}-byte limit"),
        "limit_bytes": limit,
    }));
    if close {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            [(header::CONNECTION, "close")],
            body,
        )
            .into_response()
    } else {
        (StatusCode::PAYLOAD_TOO_LARGE, body).into_response()
    }
}

pub async fn middleware(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let limit = state.config.body_limit;
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<usize>().ok());
    let chunked = req.headers().contains_key(header::TRANSFER_ENCODING);
    // Nothing to read: every GET passes untouched.
    if declared == Some(0) || (declared.is_none() && !chunked) {
        return next.run(req).await;
    }
    let (parts, body) = req.into_parts();
    let mut stream = body.into_data_stream();
    let mut total = 0usize;
    let mut buf: Vec<u8> = Vec::new();
    // A body that says up front it is too large is never buffered.
    let mut over = declared.is_some_and(|n| n > limit);
    if !over {
        buf.reserve(declared.unwrap_or(0).min(limit));
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "the request body could not be read" })),
                )
                    .into_response();
            };
            total += chunk.len();
            if total > limit {
                over = true;
                buf = Vec::new();
                break;
            }
            buf.extend_from_slice(&chunk);
        }
    }
    if !over {
        return next.run(Request::from_parts(parts, Body::from(buf))).await;
    }
    // Over the limit: let the client finish writing before it is told.
    let cap = drain_cap(limit);
    let drained = tokio::time::timeout(DRAIN_TIME, async {
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    total += chunk.len();
                    if total > cap {
                        return false;
                    }
                }
                Err(_) => return false,
            }
        }
        true
    })
    .await
    .unwrap_or(false);
    too_large(limit, !drained)
}
