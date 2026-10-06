//! `POST /v1/sync` — one upload batch in, one acknowledgement out.
//!
//! The batch is RFC 0006 §10.3 with `events` as RFC 0001 canonical envelopes
//! — the same JSON `attempt hook` spools locally, so a client has nothing to
//! translate. Idempotency is the engine's: `event_id` is minted by the
//! client, ingest deduplicates by it, and a re-sent batch acknowledges the
//! same events as duplicates instead of storing them twice.

use crate::AppState;
use crate::auth::Principal;
use attemptdb_core::{
    AttemptId, CaptureMode, DeviceId, Event, EventId, EventKind, SessionId, TurnId,
};
use attemptdb_project::is_meta_kind;
use attemptdb_storage::segment::{Cols, col};
use attemptdb_storage::{Database, ScanFilter};
use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

pub const SYNC_VERSION: u32 = 1;

/// Most events one batch may carry. A client with more splits them; the
/// order across batches is preserved by sending one batch at a time.
pub const MAX_BATCH_EVENTS: usize = 5_000;

#[derive(Debug, Deserialize)]
pub struct SyncBatch {
    pub sync_version: u32,
    pub device_id: DeviceId,
    /// Client-chosen; echoed back so an ack can be matched to its batch.
    pub batch_id: String,
    /// What the client believes it is allowed to persist. Informational: the
    /// server's mode is the ceiling regardless.
    #[serde(default)]
    pub capture_mode: Option<CaptureMode>,
    pub events: Vec<Event>,
}

#[derive(Debug, Serialize)]
pub struct SyncAck {
    pub sync_version: u32,
    pub batch_id: String,
    /// Stored for the first time.
    pub accepted: usize,
    /// Already stored (a re-sent batch, or overlapping batches).
    pub duplicates: usize,
    /// Not stored, with the reason; the client should not retry these.
    pub rejected: Vec<Rejected>,
    /// Attrs dropped by the engine's contract check across the batch.
    pub redactions: usize,
    /// Events whose `content`/`raw` were removed by the server's capture
    /// mode ceiling before storage.
    pub stripped_content: usize,
}

#[derive(Debug, Serialize)]
pub struct Rejected {
    pub event_id: EventId,
    pub reason: &'static str,
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn rank(mode: CaptureMode) -> u8 {
    match mode {
        CaptureMode::MetadataOnly => 0,
        CaptureMode::LocalSemantic => 1,
        CaptureMode::FullSync => 2,
    }
}

/// The more restrictive of the two.
pub fn clamp(client: CaptureMode, ceiling: CaptureMode) -> CaptureMode {
    if rank(client) <= rank(ceiling) {
        client
    } else {
        ceiling
    }
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<SyncBatch>, JsonRejection>,
) -> Response {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let Some(principal) = state.authenticate(authorization) else {
        return error(StatusCode::UNAUTHORIZED, "missing or unknown bearer key");
    };
    if !principal.can_write() {
        return error(
            StatusCode::FORBIDDEN,
            format!(
                "a {} key cannot upload; uploads need a device key",
                principal.scope.as_str()
            ),
        );
    }
    let Json(batch) = match body {
        Ok(b) => b,
        Err(e) => return error(e.status(), e.body_text()),
    };
    if batch.sync_version != SYNC_VERSION {
        return error(
            StatusCode::BAD_REQUEST,
            format!(
                "sync_version {} not supported (server speaks {SYNC_VERSION})",
                batch.sync_version
            ),
        );
    }
    if batch.device_id != principal.device_id {
        return error(
            StatusCode::FORBIDDEN,
            "batch device_id does not match the key's device",
        );
    }
    if batch.events.len() > MAX_BATCH_EVENTS {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "{} events in one batch; the limit is {MAX_BATCH_EVENTS}",
                batch.events.len()
            ),
        );
    }

    // Key and device agree: the device is here, whatever the batch holds.
    if let Ok(mut seen) = state.seen.lock() {
        seen.insert(
            (principal.tenant.clone(), principal.device_id),
            attemptdb_core::Timestamp::now(),
        );
    }

    let (events, mut rejected, stripped_content) =
        prepare(batch.events, &principal, state.config.capture_mode);
    let batch_id = batch.batch_id;

    if events.is_empty() {
        return Json(SyncAck {
            sync_version: SYNC_VERSION,
            batch_id,
            accepted: 0,
            duplicates: 0,
            rejected,
            redactions: 0,
            stripped_content,
        })
        .into_response();
    }

    let tenant = principal.tenant.clone();
    let st = Arc::clone(&state);
    let principal = principal.clone();
    let ingest = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let db = st.tenants.open(&tenant)?;
        let mut db = db
            .lock()
            .map_err(|_| anyhow::anyhow!("tenant {tenant}: database poisoned"))?;
        // Facts first, then corrections and retractions: a retraction in the
        // same batch as the session it retracts is checked against a database
        // that already holds that session. A device may retract or correct
        // only what it uploaded itself.
        let (mut facts, mut metas): (Vec<Event>, Vec<Event>) =
            events.into_iter().partition(|e| !is_meta_kind(e.kind));
        let mut report = attemptdb_storage::IngestReport::default();
        let mut refused: Vec<Rejected> = Vec::new();
        for stage in 0..2 {
            let events = if stage == 0 {
                std::mem::take(&mut facts)
            } else {
                let (allowed, refused_now) =
                    enforce_meta_ownership(&db, &principal, std::mem::take(&mut metas))?;
                refused.extend(refused_now);
                allowed
            };
            if events.is_empty() {
                continue;
            }
            // The live facts of the stage, taken before the ingest consumes
            // it and merged once it is durable. A duplicate the engine
            // rejects cannot move them backwards.
            let delta = crate::live::LiveState::from_events(&events);
            let r = db.ingest(events)?;
            if r.accepted > 0 {
                st.live.merge(&tenant, &delta);
                st.ingested(&tenant);
            }
            report.accepted += r.accepted;
            report.duplicates += r.duplicates;
            report.redactions += r.redactions;
        }
        Ok((report, refused))
    })
    .await;
    match ingest {
        Ok(Ok((report, refused))) => {
            rejected.extend(refused);
            Json(SyncAck {
                sync_version: SYNC_VERSION,
                batch_id,
                accepted: report.accepted,
                duplicates: report.duplicates,
                rejected: std::mem::take(&mut rejected),
                redactions: report.redactions,
                stripped_content,
            })
            .into_response()
        }
        // Storage trouble is the server's, not the client's: say so with a
        // status that tells the client to keep the batch and retry.
        Ok(Err(e)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("ingest failed: {e:#}"),
        ),
        Err(e) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("ingest task failed: {e}"),
        ),
    }
}

/// Per-event checks and the capture-mode ceiling. Returns the events to
/// ingest, the rejections, and how many events lost content to the ceiling.
fn prepare(
    events: Vec<Event>,
    principal: &Principal,
    ceiling: CaptureMode,
) -> (Vec<Event>, Vec<Rejected>, usize) {
    let mut keep = Vec::with_capacity(events.len());
    let mut rejected = Vec::new();
    let mut stripped = 0;
    for mut ev in events {
        if ev.device_id != principal.device_id {
            rejected.push(Rejected {
                event_id: ev.event_id,
                reason: "event device_id does not match the batch",
            });
            continue;
        }
        // The same retention rule the local receiver applies, for clients
        // that predate it: a span without a session is the exporter's own
        // execution trace, and one busy device sends hundreds of thousands
        // a day. Rejected, not stored — the client counts it and moves on.
        if !attemptdb_adapters::otel::retained(&ev) {
            rejected.push(Rejected {
                event_id: ev.event_id,
                reason: "telemetry span without a session is not retained",
            });
            continue;
        }
        // The client's own sequence number survives as metadata; the server
        // assigns this database's `source_seq` at ingest.
        if ev.source_seq != 0 {
            ev.attrs
                .insert("device_seq".to_string(), json!(ev.source_seq));
        }
        let had_content = ev.content.is_some() || ev.raw.is_some();
        ev.capture_mode = clamp(ev.capture_mode, ceiling);
        ev.apply_capture_mode();
        if had_content && !ev.capture_mode.persists_content_locally() {
            stripped += 1;
        }
        keep.push(ev);
    }
    (keep, rejected, stripped)
}

/// What a retraction or correction points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaTarget {
    Session(SessionId),
    Event(EventId),
    Attempt(AttemptId),
    Turn(TurnId),
}

/// The target of a Retraction or Correction event, read from `attrs` the way
/// the projector reads it (`ses_…`, `ev_…`, `att_…`, `trn_…`; a bare id is
/// typed by `target_type` or `correction_type`). `None` for a target nothing
/// can resolve — the projector ignores such an event, so it is harmless.
pub fn meta_target(ev: &Event) -> Option<MetaTarget> {
    let text = ev.attrs.get("target")?.as_str()?.trim();
    if let Some(r) = text.strip_prefix("ses_") {
        return r.parse().ok().map(MetaTarget::Session);
    }
    if let Some(r) = text.strip_prefix("ev_") {
        return r.parse().ok().map(MetaTarget::Event);
    }
    if let Some(r) = text.strip_prefix("att_") {
        return r.parse().ok().map(MetaTarget::Attempt);
    }
    if let Some(r) = text.strip_prefix("trn_") {
        return r.parse().ok().map(MetaTarget::Turn);
    }
    let declared = ev
        .attrs
        .get("target_type")
        .or_else(|| ev.attrs.get("correction_type"))
        .and_then(|v| v.as_str())?;
    match declared {
        "session" => text.parse().ok().map(MetaTarget::Session),
        "event" => text.parse().ok().map(MetaTarget::Event),
        "attempt" | "attempt_outcome" | "attempt_note" => {
            text.parse().ok().map(MetaTarget::Attempt)
        }
        "turn_objective" => text.parse().ok().map(MetaTarget::Turn),
        _ => None,
    }
}

/// Facts about one session in a tenant, read from columns (no event is
/// decoded): which devices wrote it, and how many prompts it holds.
#[derive(Debug, Default)]
struct SessionFacts {
    devices: std::collections::BTreeSet<DeviceId>,
    prompts: usize,
    events: usize,
}

fn session_facts(db: &Database, sid: SessionId) -> anyhow::Result<SessionFacts> {
    let filter = ScanFilter {
        session_id: Some(sid),
        ..Default::default()
    };
    let mut out = SessionFacts::default();
    for batch in db.batches(&filter)? {
        let Some(kept) = filter.filter_batch(&batch)? else {
            continue;
        };
        let cols = Cols::new(kept.clone())?;
        for row in 0..kept.num_rows() {
            let kind = cols.str_ref(col::KIND, row).and_then(EventKind::parse);
            if kind.is_some_and(is_meta_kind) {
                continue;
            }
            if let Some(d) = cols.fsb(col::DEVICE_ID, row) {
                out.devices.insert(DeviceId::from_bytes(d));
            }
            out.events += 1;
            if kind == Some(EventKind::PromptSubmitted) {
                out.prompts += 1;
            }
        }
    }
    Ok(out)
}

/// The device that wrote a stored event, or `None` when it is not stored.
fn event_device(db: &Database, id: EventId) -> anyhow::Result<Option<DeviceId>> {
    for batch in db.batches(&ScanFilter::default())? {
        let cols = Cols::new(batch.clone())?;
        for row in 0..batch.num_rows() {
            if cols.fsb(col::EVENT_ID, row) == Some(*id.as_bytes()) {
                return Ok(cols.fsb(col::DEVICE_ID, row).map(DeviceId::from_bytes));
            }
        }
    }
    Ok(None)
}

/// Most attempts one turn is searched for when an attempt id has to be tied
/// to a session (ids are `derive(session, turn, index)`; nothing stores the
/// reverse).
const MAX_ATTEMPT_INDEX: usize = 256;

fn owns_session(facts: &SessionFacts, device: DeviceId) -> bool {
    facts.events > 0 && facts.devices.iter().all(|d| *d == device)
}

/// May this device's Retraction or Correction stand? Only if what it points
/// at is the device's own: a session whose every fact the device wrote, an
/// event the device wrote, an attempt or turn of such a session. The
/// projector honours a retraction from any device, so without this a member
/// of a tenant could hide or rewrite a teammate's sessions with a key that is
/// meant only to upload its own.
fn meta_allowed(
    db: &Database,
    device: DeviceId,
    ev: &Event,
) -> anyhow::Result<std::result::Result<(), &'static str>> {
    const NOT_OWN: &str = "a retraction or correction may only target this device's own events";
    let Some(target) = meta_target(ev) else {
        return Ok(Ok(()));
    };
    Ok(match target {
        MetaTarget::Session(sid) => {
            if ev.session_id != sid || !owns_session(&session_facts(db, sid)?, device) {
                Err(NOT_OWN)
            } else {
                Ok(())
            }
        }
        MetaTarget::Event(id) => match event_device(db, id)? {
            Some(d) if d == device => Ok(()),
            _ => Err(NOT_OWN),
        },
        MetaTarget::Attempt(_) | MetaTarget::Turn(_) => {
            // The event names its session; the target must be a member of it.
            let sid = ev.session_id;
            let facts = session_facts(db, sid)?;
            if !owns_session(&facts, device) {
                return Ok(Err(NOT_OWN));
            }
            let s = sid.to_string();
            let turns = facts.prompts + 2;
            let found = match target {
                MetaTarget::Turn(t) => {
                    (0..=turns).any(|i| TurnId::derive(&[&s, &i.to_string()]) == t)
                }
                MetaTarget::Attempt(a) => (0..=turns).any(|t| {
                    (0..=MAX_ATTEMPT_INDEX)
                        .any(|i| AttemptId::derive(&[&s, &t.to_string(), &i.to_string()]) == a)
                }),
                _ => false,
            };
            if found { Ok(()) } else { Err(NOT_OWN) }
        }
    })
}

/// Split `events` into those that may be stored and the rejections. Events
/// that are not retractions or corrections pass without a read of the
/// database, so ordinary uploads pay nothing.
fn enforce_meta_ownership(
    db: &Database,
    principal: &Principal,
    events: Vec<Event>,
) -> anyhow::Result<(Vec<Event>, Vec<Rejected>)> {
    if !events.iter().any(|e| is_meta_kind(e.kind)) {
        return Ok((events, Vec::new()));
    }
    let mut keep = Vec::with_capacity(events.len());
    let mut refused = Vec::new();
    for ev in events {
        if is_meta_kind(ev.kind)
            && let Err(reason) = meta_allowed(db, principal.device_id, &ev)?
        {
            refused.push(Rejected {
                event_id: ev.event_id,
                reason,
            });
            continue;
        }
        keep.push(ev);
    }
    Ok((keep, refused))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_is_the_minimum() {
        use CaptureMode::*;
        assert_eq!(clamp(FullSync, MetadataOnly), MetadataOnly);
        assert_eq!(clamp(MetadataOnly, FullSync), MetadataOnly);
        assert_eq!(clamp(LocalSemantic, LocalSemantic), LocalSemantic);
        assert_eq!(clamp(LocalSemantic, FullSync), LocalSemantic);
    }

    #[test]
    fn prepare_refuses_telemetry_spans_without_a_session() {
        use attemptdb_core::event::Provider;
        use attemptdb_core::{EventKind, ProjectRef};
        let device = attemptdb_core::DeviceId::derive(&["sync-test"]);
        let principal = crate::auth::Principal {
            tenant: crate::tenants::TenantId::parse("org_test").unwrap(),
            device_id: device,
            scope: crate::auth::Scope::Device,
            user_id: None,
        };
        let project = ProjectRef::derive("/home/dev/example/project", None, &device);
        let event = |name: &str, kind: EventKind| {
            Event::new(
                device,
                Provider::Codex,
                name,
                kind,
                project.clone(),
                "s1",
                CaptureMode::MetadataOnly,
                "sync-test/0",
            )
        };
        let hook = event("PostToolUse", EventKind::ToolCallFinished);
        let mut span = event("receiving", EventKind::Unknown);
        span.attrs.insert("source".into(), json!("otel"));
        span.attrs.insert("x_otel_signal".into(), json!("traces"));
        span.attrs
            .insert("x_otel_record_type".into(), json!("span"));
        span.attrs
            .insert("x_otel_session_attributed".into(), json!(false));
        let mut attributed = span.clone();
        attributed.event_id = attemptdb_core::EventId::derive(&["sync-test", "attributed"]);
        attributed
            .attrs
            .insert("x_otel_session_attributed".into(), json!(true));
        let refused = span.event_id;
        let (kept, rejected, _) = prepare(
            vec![hook, span, attributed],
            &principal,
            CaptureMode::MetadataOnly,
        );
        assert_eq!(kept.len(), 2, "the hook and the attributed span stay");
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].event_id, refused);
        assert!(rejected[0].reason.contains("not retained"));
    }
}
