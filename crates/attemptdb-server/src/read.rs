//! The read API: an organisation's work graph, served from the tenant's
//! database (RFC 0006 §10.8, `docs/server-api.md`).
//!
//! Reader and admin keys only — a device key uploads and reads back its own
//! inferences, nothing else. Every handler resolves the tenant's
//! [`TenantView`] through the registry slot's cache (see [`crate::engine`]),
//! shapes the projection the way the local UI's `/api/` does, and applies
//! the inference merge rule (see [`crate::merge`]) wherever an attempt,
//! handoff, work unit or decision is returned.
//!
//! Filters: `project` selects one project's entities out of the tenant's
//! projection (it does not re-project the subset, so ids and counts are the
//! same as in the unfiltered view); `since`/`until` keep entities whose
//! activity window overlaps the range. Lists are newest first and paged by
//! an opaque keyset `cursor`, so a page is stable under ingest.

use crate::AppState;
use crate::auth::Principal;
use crate::engine::TenantView;
use crate::merge::DeviceInferences;
use crate::shape as sh;
use crate::tenants::TenantId;
use attemptdb_core::{DeviceId, Event, EventKind, ProjectId, Timestamp};
use attemptdb_project::{
    ALGORITHM_VERSION, Attempt, Decision, Handoff, Projection, Session, WorkUnit,
};
use attemptdb_query::{QueryError, QueryResult, ResultKind, TimeExpr, format_parse_error};
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

pub const DEFAULT_LIMIT: usize = 200;
pub const MAX_LIMIT: usize = 2000;
pub const DEFAULT_ATTENTION_LIMIT: usize = 20;
/// Wall-clock budget of one `/v1/query` statement. Cooperative: DataFusion
/// yields between batches; the AttemptQL evaluators (`WHY`, `TRACE`,
/// `STATE`) run to completion, which is milliseconds.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

type Params = HashMap<String, String>;

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// An early exit from a helper, boxed so the `Err` arm stays small.
fn refuse(status: StatusCode, message: impl Into<String>) -> Box<Response> {
    Box::new(error(status, message))
}

/// The read gate for other modules (`/v1/live`).
pub(crate) fn reader_principal(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Principal, Box<Response>> {
    reader(state, headers)
}

pub(crate) fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    error(status, message)
}

/// The tenant's view for other modules (`/v1/live` seeding).
pub(crate) async fn load_view(
    state: &Arc<AppState>,
    principal: &Principal,
) -> Result<Arc<TenantView>, Box<Response>> {
    load(state, principal).await.map(|l| l.view)
}

/// The header an operator read names its tenant with (see [`operator`]).
pub const TENANT_HEADER: &str = "x-attemptdb-tenant";

/// The operator's read: the admin token as the bearer plus
/// `X-AttemptDB-Tenant: <tenant>`. The product's backend reads every
/// tenant this way (its devices page, its console) without a reader key
/// provisioned and stored per tenant — the admin token can already mint
/// such a key for any tenant, so this grants nothing new. Without the
/// header the admin token is not a read credential.
fn operator(state: &AppState, headers: &HeaderMap) -> Option<Result<Principal, Box<Response>>> {
    let tenant = headers.get(TENANT_HEADER)?.to_str().ok()?.trim();
    crate::admin::gate(state, headers).ok()?;
    Some(match TenantId::parse(tenant) {
        Ok(tenant) => Ok(Principal {
            device_id: DeviceId::derive(&["attemptdb-server", tenant.as_str()]),
            tenant,
            scope: crate::auth::Scope::Admin,
            user_id: None,
        }),
        Err(e) => Err(refuse(StatusCode::BAD_REQUEST, e.to_string())),
    })
}

/// The read gate: a known key with reader or admin scope, or the operator.
fn reader(state: &AppState, headers: &HeaderMap) -> Result<Principal, Box<Response>> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let principal = match state.authenticate(authorization) {
        Some(p) => p,
        None => match operator(state, headers) {
            Some(r) => return r,
            None => {
                return Err(refuse(
                    StatusCode::UNAUTHORIZED,
                    "missing or unknown bearer key",
                ));
            }
        },
    };
    if !principal.can_read_tenant() {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            format!(
                "a {} key cannot read the tenant; reads need a reader or admin key",
                principal.scope.as_str()
            ),
        ));
    }
    Ok(principal)
}

/// Everything one read needs, resolved once per request.
pub struct Loaded {
    pub tenant: TenantId,
    pub view: Arc<TenantView>,
    pub inferences: Arc<DeviceInferences>,
    /// Device → the product's user id, from the tenant's device keys: how
    /// "Claude Code #18" gets its "Kevin". Read per request so a key issued
    /// or relabelled a moment ago is reflected without a new view.
    pub people: Arc<People>,
}

/// The tenant's device-to-user mapping.
#[derive(Debug, Default)]
pub struct People {
    by_device: HashMap<DeviceId, String>,
}

impl People {
    pub fn of(state: &AppState, tenant: &TenantId) -> Self {
        let mut by_device = HashMap::new();
        if let Ok(keys) = state.keys.read() {
            for e in keys.entries() {
                if e.tenant == tenant.as_str()
                    && e.scope == crate::auth::Scope::Device
                    && let Some(u) = e.user_id
                {
                    by_device.entry(e.device_id).or_insert(u);
                }
            }
        }
        Self { by_device }
    }

    pub fn user_of(&self, device: &DeviceId) -> Option<&str> {
        self.by_device.get(device).map(String::as_str)
    }

    /// The distinct users behind `devices`, first seen first.
    pub fn users_of<'a>(&self, devices: impl IntoIterator<Item = &'a DeviceId>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for d in devices {
            if let Some(u) = self.user_of(d)
                && !out.iter().any(|x| x == u)
            {
                out.push(u.to_string());
            }
        }
        out
    }
}

async fn load(state: &Arc<AppState>, principal: &Principal) -> Result<Loaded, Box<Response>> {
    let tenant = principal.tenant.clone();
    let st = Arc::clone(state);
    let handle = tokio::runtime::Handle::current();
    let loaded = tokio::task::spawn_blocking(move || -> anyhow::Result<Loaded> {
        let t = st.tenants.open_tenant(&tenant)?;
        let dir = st.tenants.dir(&tenant);
        let mut cache = t
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("tenant {tenant}: cache poisoned"))?;
        let view =
            cache.view_windowed(&t.db, tenant.as_str(), &handle, st.config.view_window_days)?;
        let inferences = cache.inferences(&dir)?;
        let people = Arc::new(People::of(&st, &tenant));
        Ok(Loaded {
            tenant,
            view,
            inferences,
            people,
        })
    })
    .await;
    match loaded {
        Ok(Ok(l)) => Ok(l),
        Ok(Err(e)) => Err(refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("cannot load the tenant: {e:#}"),
        )),
        Err(e) => Err(refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("task failed: {e}"),
        )),
    }
}

/// Authenticate and load, in one step.
async fn begin(state: &Arc<AppState>, headers: &HeaderMap) -> Result<Loaded, Box<Response>> {
    let principal = reader(state, headers)?;
    load(state, &principal).await
}

/// `{"tenant", "algorithm_version", "generated_at", …data}`.
fn respond(tenant: &TenantId, data: Map<String, Value>) -> Response {
    let mut out = Map::with_capacity(data.len() + 3);
    out.insert("tenant".into(), Value::String(tenant.as_str().to_string()));
    out.insert("algorithm_version".into(), json!(ALGORITHM_VERSION));
    out.insert("generated_at".into(), sh::ts(Timestamp::now()));
    out.extend(data);
    Json(Value::Object(out)).into_response()
}

fn object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// Parse a time argument: RFC 3339, `YYYY-MM-DD`, epoch, `now`, `today`,
/// `yesterday`, or `-<n>(s|m|h|d|w)`.
pub fn parse_time(spec: &str) -> Option<Timestamp> {
    TimeExpr::parse_literal(spec).map(|e| e.resolve(Timestamp::now()))
}

fn time_param(q: &Params, key: &str) -> Result<Option<Timestamp>, Box<Response>> {
    match q.get(key).map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(spec) => parse_time(spec).map(Some).ok_or_else(|| {
            refuse(
                StatusCode::BAD_REQUEST,
                format!(
                    "cannot parse {key}={spec:?}: use RFC 3339, YYYY-MM-DD, now, today, yesterday or -<n>(s|m|h|d|w)"
                ),
            )
        }),
    }
}

/// `limit`: `default` when absent, clamped to `1..=MAX_LIMIT`; 400 when
/// it is not a positive integer.
pub fn limit_param(q: &Params, default: usize) -> Result<usize, Box<Response>> {
    match q.get("limit").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => Ok(default),
        Some(s) => match s.parse::<usize>() {
            Ok(n) if n > 0 => Ok(n.min(MAX_LIMIT)),
            _ => Err(refuse(
                StatusCode::BAD_REQUEST,
                format!("limit={s:?} is not a positive integer (max {MAX_LIMIT})"),
            )),
        },
    }
}

fn flag(q: &Params, key: &str) -> bool {
    matches!(
        q.get(key).map(|s| s.trim()),
        Some("1") | Some("true") | Some("on") | Some("yes")
    )
}

/// Keyset position in a list sorted `(at desc, id asc)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub at: Timestamp,
    pub id: Uuid,
}

/// Opaque on the wire; a consumer passes it back untouched.
pub fn encode_cursor(c: &Cursor) -> String {
    hex::encode(format!("{}:{}", c.at.as_micros(), c.id.simple()))
}

pub fn decode_cursor(s: &str) -> Option<Cursor> {
    let raw = hex::decode(s.trim()).ok()?;
    let text = String::from_utf8(raw).ok()?;
    let (at, id) = text.split_once(':')?;
    Some(Cursor {
        at: Timestamp::from_micros(at.parse().ok()?),
        id: Uuid::parse_str(id).ok()?,
    })
}

fn cursor_param(q: &Params) -> Result<Option<Cursor>, Box<Response>> {
    match q.get("cursor").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => decode_cursor(s).map(Some).ok_or_else(|| {
            refuse(
                StatusCode::BAD_REQUEST,
                "cursor is not one this server issued",
            )
        }),
    }
}

/// The filters every list accepts.
pub struct Scope {
    pub project: Option<ProjectId>,
    pub since: Option<Timestamp>,
    pub until: Option<Timestamp>,
}

impl Scope {
    fn from(q: &Params, view: &TenantView) -> Result<Self, Box<Response>> {
        let project = match q.get("project").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            None => None,
            Some(spec) => Some(
                view.resolve_project(spec)
                    .map_err(|e| refuse(StatusCode::BAD_REQUEST, e))?,
            ),
        };
        Ok(Self {
            project,
            since: time_param(q, "since")?,
            until: time_param(q, "until")?,
        })
    }

    fn project_ok(&self, pid: ProjectId) -> bool {
        self.project.is_none_or(|p| p == pid)
    }

    /// Whether `[start, end]` overlaps the requested window.
    fn window_ok(&self, start: Timestamp, end: Timestamp) -> bool {
        overlaps(start, end, self.since, self.until)
    }

    fn session_ok(&self, s: &Session) -> bool {
        self.project_ok(s.project_id) && self.window_ok(s.started_at, s.last_event_at)
    }

    fn event_ok(&self, ev: &Event) -> bool {
        self.project_ok(ev.project.project_id)
            && self.since.is_none_or(|t| ev.observed_at >= t)
            && self.until.is_none_or(|t| ev.observed_at <= t)
    }

    fn label(&self) -> Value {
        json!({
            "project_id": self.project.as_ref().map(sh::id),
            "since": sh::ts_opt(self.since),
            "until": sh::ts_opt(self.until),
        })
    }
}

/// `[start, end]` overlaps `[since, until]` (either bound optional).
pub fn overlaps(
    start: Timestamp,
    end: Timestamp,
    since: Option<Timestamp>,
    until: Option<Timestamp>,
) -> bool {
    until.is_none_or(|u| start <= u) && since.is_none_or(|s| end >= s)
}

/// One page of a list sorted `(at desc, id asc)`: the items strictly after
/// `cursor`, at most `limit`, and the cursor of the next page when more
/// remain.
pub fn page<'a, T>(
    items: &[&'a T],
    key: impl Fn(&T) -> (Timestamp, Uuid),
    cursor: Option<Cursor>,
    limit: usize,
) -> (Vec<&'a T>, Option<Cursor>) {
    let after = |t: &T| match cursor {
        None => true,
        Some(c) => {
            let (at, id) = key(t);
            at < c.at || (at == c.at && id > c.id)
        }
    };
    let mut out: Vec<&'a T> = Vec::with_capacity(limit);
    let mut more = false;
    for item in items.iter().copied().filter(|t| after(t)) {
        if out.len() == limit {
            more = true;
            break;
        }
        out.push(item);
    }
    let next = if more {
        out.last().map(|t| {
            let (at, id) = key(t);
            Cursor { at, id }
        })
    } else {
        None
    };
    (out, next)
}

fn next_cursor(c: Option<Cursor>) -> Value {
    c.map(|c| Value::String(encode_cursor(&c)))
        .unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Merge: which computation an inference object comes from
// ---------------------------------------------------------------------------

fn pick(inf: &DeviceInferences, kind: &str, id: &str) -> Option<Value> {
    inf.winner(kind, id).map(sh::device_item)
}

fn attempt_json(a: &Attempt, p: &Projection, inf: &DeviceInferences, with_tools: bool) -> Value {
    pick(inf, "attempt", &a.attempt_id.to_string()).unwrap_or_else(|| sh::attempt(a, p, with_tools))
}

fn handoff_json(h: &Handoff, inf: &DeviceInferences) -> Value {
    pick(
        inf,
        "handoff",
        &format!("{}:{}", h.from_session, h.to_session),
    )
    .unwrap_or_else(|| sh::handoff(h))
}

fn work_unit_json(w: &WorkUnit, inf: &DeviceInferences) -> Value {
    pick(inf, "work_unit", &w.work_unit_id.to_string()).unwrap_or_else(|| sh::work_unit(w))
}

fn decision_json(d: &Decision, inf: &DeviceInferences) -> Value {
    pick(inf, "decision", &d.decision_id.to_string()).unwrap_or_else(|| sh::decision(d))
}

fn session_json(l: &Loaded, s: &Session, turns: Option<Vec<Value>>) -> Value {
    let view = &l.view;
    let p = view.engine.projection();
    let facts = sh::session_facts(view, s);
    let mut v = sh::session(s, facts, p.attempts_of(s.session_id).count(), turns);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("user_id".into(), json!(l.people.user_of(&facts.device_id)));
    }
    v
}

/// A work unit with the people behind its sessions (the devices that wrote
/// them, mapped through the tenant's keys).
fn work_unit_json_for(l: &Loaded, w: &WorkUnit) -> Value {
    let mut v = work_unit_json(w, &l.inferences);
    if let Some(obj) = v.as_object_mut() {
        let devices: Vec<DeviceId> = w
            .sessions
            .iter()
            .filter_map(|sid| l.view.sessions.get(sid).map(|f| f.device_id))
            .collect();
        obj.insert("devices".into(), sh::ids(&devices));
        obj.insert("users".into(), json!(l.people.users_of(devices.iter())));
        obj.insert("signal".into(), work_unit_signal(l, w));
    }
    v
}

/// The countable signals of a work unit: the newest test run and build its
/// sessions reported since the unit started. `null` fields mean "nothing
/// to count" — a console shows a phase badge then, never a made-up number.
fn work_unit_signal(l: &Loaded, w: &WorkUnit) -> Value {
    let facts = l.view.engine.facts();
    let mut tests: Option<attemptdb_query::TestSignal> = None;
    let mut build: Option<attemptdb_query::BuildSignal> = None;
    for sid in &w.sessions {
        let Some(f) = facts.session(sid) else {
            continue;
        };
        if let Some(t) = f.last_tests
            && t.at >= w.started_at
            && tests.is_none_or(|m| t.at >= m.at)
        {
            tests = Some(t);
        }
        if let Some(b) = f.last_build
            && b.at >= w.started_at
            && build.is_none_or(|m| b.at >= m.at)
        {
            build = Some(b);
        }
    }
    json!({
        "tests": tests.map(|t| json!({
            "passed": t.passed, "failed": t.failed, "skipped": t.skipped,
            "total": t.passed + t.failed + t.skipped, "at": sh::ts(t.at),
        })),
        "build": build.map(|b| json!({ "ok": b.ok, "at": sh::ts(b.at) })),
    })
}

fn turns_json(
    view: &TenantView,
    s: &Session,
    inf: &DeviceInferences,
    with_tools: bool,
) -> Vec<Value> {
    let p = view.engine.projection();
    sh::turns_of(p, s)
        .into_iter()
        .map(|t| {
            let attempts = sh::attempts_of_turn(p, t)
                .into_iter()
                .map(|a| attempt_json(a, p, inf, with_tools))
                .collect();
            sh::turn(t, attempts)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /v1/sessions` — the projection's sessions, newest first.
pub async fn sessions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<Params>,
) -> Response {
    let l = match begin(&state, &headers).await {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let view = &l.view;
    let p = view.engine.projection();
    let (scope, limit, cursor) = match (
        Scope::from(&q, view),
        limit_param(&q, DEFAULT_LIMIT),
        cursor_param(&q),
    ) {
        (Ok(s), Ok(l), Ok(c)) => (s, l, c),
        (Err(r), _, _) | (_, Err(r), _) | (_, _, Err(r)) => return *r,
    };
    let all: Vec<&Session> = sh::sessions_sorted(p)
        .into_iter()
        .filter(|s| scope.session_ok(s))
        .collect();
    let (items, next) = page(&all, |s| (s.started_at, s.session_id.0), cursor, limit);
    let open = all.iter().filter(|s| s.ended_at.is_none()).count();
    respond(
        &l.tenant,
        object(json!({
            "scope": scope.label(),
            "total": all.len(),
            "open": open,
            "sessions": items.iter().map(|s| session_json(&l, s, None)).collect::<Vec<_>>(),
            "next_cursor": next_cursor(next),
        })),
    )
}

/// `GET /v1/timeline` — sessions → turns → attempts, plus the tenant's
/// handoffs, work units and decisions, as the local UI's `/api/timeline`.
pub async fn timeline(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<Params>,
) -> Response {
    let l = match begin(&state, &headers).await {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let view = &l.view;
    let inf = &l.inferences;
    let p = view.engine.projection();
    let (scope, limit, cursor) = match (
        Scope::from(&q, view),
        limit_param(&q, DEFAULT_LIMIT),
        cursor_param(&q),
    ) {
        (Ok(s), Ok(l), Ok(c)) => (s, l, c),
        (Err(r), _, _) | (_, Err(r), _) | (_, _, Err(r)) => return *r,
    };
    let with_tools = flag(&q, "tools");
    let all: Vec<&Session> = sh::sessions_sorted(p)
        .into_iter()
        .filter(|s| scope.session_ok(s))
        .collect();
    let (items, next) = page(&all, |s| (s.started_at, s.session_id.0), cursor, limit);
    let sessions: Vec<Value> = items
        .iter()
        .map(|s| session_json(&l, s, Some(turns_json(view, s, inf, with_tools))))
        .collect();
    let attempts_total: usize = all
        .iter()
        .map(|s| p.attempts_of(s.session_id).count())
        .sum();

    let mut handoffs: Vec<&Handoff> = p
        .handoffs
        .iter()
        .filter(|h| scope.project_ok(h.project_id) && scope.window_ok(h.at, h.at))
        .collect();
    handoffs.sort_by(|a, b| b.at.cmp(&a.at).then(a.to_session.cmp(&b.to_session)));
    let work_units: Vec<&WorkUnit> = sh::work_units_sorted(p)
        .into_iter()
        .filter(|w| scope.project_ok(w.project_id) && scope.window_ok(w.started_at, w.updated_at))
        .collect();
    let mut decisions: Vec<&Decision> = p
        .decisions
        .iter()
        .filter(|d| {
            p.session(d.session_id)
                .is_some_and(|s| scope.project_ok(s.project_id))
                && scope.window_ok(d.decided_at, d.decided_at)
        })
        .collect();
    decisions.sort_by(|a, b| {
        b.decided_at
            .cmp(&a.decided_at)
            .then(a.decision_id.cmp(&b.decision_id))
    });

    respond(
        &l.tenant,
        object(json!({
            "scope": scope.label(),
            "events": view.event_count(),
            "total_sessions": all.len(),
            "total_attempts": attempts_total,
            "sessions": sessions,
            "next_cursor": next_cursor(next),
            "handoffs": handoffs.iter().take(limit).map(|h| handoff_json(h, inf)).collect::<Vec<_>>(),
            "handoffs_total": handoffs.len(),
            "work_units": work_units.iter().take(limit).map(|w| work_unit_json_for(&l, w)).collect::<Vec<_>>(),
            "work_units_total": work_units.len(),
            "decisions": decisions.iter().take(limit).map(|d| decision_json(d, inf)).collect::<Vec<_>>(),
            "decisions_total": decisions.len(),
            "note": "attempts, blockers and handoffs are inferences with evidence; events are facts",
        })),
    )
}

/// `GET /v1/work` — work units with their member attempts and blocker.
pub async fn work(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<Params>,
) -> Response {
    let l = match begin(&state, &headers).await {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let view = &l.view;
    let inf = &l.inferences;
    let p = view.engine.projection();
    let (scope, limit, cursor) = match (
        Scope::from(&q, view),
        limit_param(&q, DEFAULT_LIMIT),
        cursor_param(&q),
    ) {
        (Ok(s), Ok(l), Ok(c)) => (s, l, c),
        (Err(r), _, _) | (_, Err(r), _) | (_, _, Err(r)) => return *r,
    };
    let status = q.get("status").map(|s| s.trim().to_ascii_lowercase());
    let phase = q.get("phase").map(|s| s.trim().to_ascii_lowercase());
    let all: Vec<&WorkUnit> = sh::work_units_sorted(p)
        .into_iter()
        .filter(|w| scope.project_ok(w.project_id) && scope.window_ok(w.started_at, w.updated_at))
        .filter(|w| {
            status
                .as_deref()
                .is_none_or(|s| s.is_empty() || w.status.as_str() == s)
        })
        .filter(|w| {
            phase
                .as_deref()
                .is_none_or(|s| s.is_empty() || w.phase.as_str() == s)
        })
        .collect();
    let (items, next) = page(&all, |w| (w.updated_at, w.work_unit_id.0), cursor, limit);
    let units: Vec<Value> = items
        .iter()
        .map(|w| {
            let mut v = work_unit_json_for(&l, w);
            let members: Vec<Value> = w
                .attempts
                .iter()
                .filter_map(|id| p.attempts.iter().find(|a| a.attempt_id == *id))
                .map(|a| attempt_json(a, p, inf, false))
                .collect();
            v["member_attempts"] = Value::Array(members);
            v["blocked"] = p
                .why_blocked_unit(w.work_unit_id)
                .map(|e| sh::explanation(&e))
                .unwrap_or(Value::Null);
            v
        })
        .collect();
    // Conflicts touching a listed unit, with the people on each side.
    let listed: std::collections::HashSet<_> = items.iter().map(|w| w.work_unit_id).collect();
    let conflicts: Vec<Value> = p
        .conflicts
        .iter()
        .filter(|c| listed.contains(&c.first) || listed.contains(&c.second))
        .map(|c| conflict_json(&l, c))
        .collect();
    respond(
        &l.tenant,
        object(json!({
            "scope": scope.label(),
            "total": all.len(),
            "work_units": units,
            "conflicts": conflicts,
            "next_cursor": next_cursor(next),
            "note": "a work unit is a connected component of turns (shared paths, consecutive turns, handoffs); phase and status are heuristics with evidence ids",
        })),
    )
}

/// A conflict with the people and devices behind each side.
fn conflict_json(l: &Loaded, c: &attemptdb_project::Conflict) -> Value {
    let p = l.view.engine.projection();
    let side = |id: attemptdb_core::WorkUnitId| -> Value {
        let Some(w) = p.work_unit(id) else {
            return json!({ "work_unit_id": sh::id(&id) });
        };
        let devices: Vec<DeviceId> = w
            .sessions
            .iter()
            .filter_map(|sid| l.view.sessions.get(sid).map(|f| f.device_id))
            .collect();
        json!({
            "work_unit_id": sh::id(&id),
            "objective": w.objective,
            "phase": w.phase.as_str(),
            "actors": w.actors.iter().map(|a| a.as_str().to_string()).collect::<Vec<_>>(),
            "devices": sh::ids(&devices),
            "users": l.people.users_of(devices.iter()),
        })
    };
    let mut v = sh::conflict(c);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("first".into(), side(c.first));
        obj.insert("second".into(), side(c.second));
    }
    v
}

/// One "Needs You" item: an open session that looks blocked.
fn attention_item(l: &Loaded, s: &Session) -> Option<Value> {
    let view = &l.view;
    let p = view.engine.projection();
    let e = p.why_blocked(s.session_id)?;
    let signal = p
        .signals
        .iter()
        .find(|g| g.session_id == s.session_id && e.evidence.contains(&g.event_id));
    let (reason, signal_type, failure_class, since) = match signal {
        Some(g) => (
            "pending_input",
            Some(match g.kind {
                EventKind::PermissionRequested => "permission_request".to_string(),
                _ => g
                    .signal_type
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            }),
            None,
            g.at,
        ),
        None => {
            let last = p.attempts_of(s.session_id).last();
            (
                "repeated_failure",
                None,
                last.and_then(|a| a.failure_class.clone()),
                last.and_then(|a| a.ended_at).unwrap_or(s.last_event_at),
            )
        }
    };
    let mut v = sh::explanation(&e);
    let obj = v.as_object_mut()?;
    obj.insert("session".into(), session_json(l, s, None));
    obj.insert("session_id".into(), sh::id(&s.session_id));
    obj.insert("project_id".into(), sh::id(&s.project_id));
    obj.insert("reason".into(), json!(reason));
    obj.insert("signal_type".into(), json!(signal_type));
    obj.insert("failure_class".into(), json!(failure_class));
    obj.insert("since".into(), sh::ts(since));
    Some(v)
}

/// `GET /v1/attention` — "Needs You": every open session that looks
/// blocked, highest confidence first.
pub async fn attention(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<Params>,
) -> Response {
    let l = match begin(&state, &headers).await {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let view = &l.view;
    let p = view.engine.projection();
    let (scope, limit) = match (
        Scope::from(&q, view),
        limit_param(&q, DEFAULT_ATTENTION_LIMIT),
    ) {
        (Ok(s), Ok(l)) => (s, l),
        (Err(r), _) | (_, Err(r)) => return *r,
    };
    let open: Vec<&Session> = p
        .sessions
        .iter()
        .filter(|s| s.ended_at.is_none() && scope.session_ok(s))
        .collect();
    let mut items: Vec<Value> = open.iter().filter_map(|s| attention_item(&l, s)).collect();
    // Work conflicts: the third kind of "needs you", one item per pair.
    for c in &p.conflicts {
        if !scope.project_ok(c.project_id) || !scope.window_ok(c.started_at, c.updated_at) {
            continue;
        }
        let mut v = conflict_json(&l, c);
        if let Some(obj) = v.as_object_mut() {
            obj.insert("reason".into(), json!("work_conflict"));
            obj.insert("since".into(), sh::ts(c.started_at));
            obj.insert(
                "claim".into(),
                json!(format!(
                    "two work units are editing {} shared path(s) at the same time{}",
                    c.paths.len(),
                    if c.paths
                        .iter()
                        .all(|x| !x.first_committed && !x.second_committed)
                    {
                        ", neither committed since"
                    } else {
                        ""
                    }
                )),
            );
            obj.insert("session_id".into(), Value::Null);
        }
        items.push(v);
    }
    items.sort_by(|a, b| {
        let conf = |v: &Value| v["confidence"].as_f64().unwrap_or(0.0);
        conf(b)
            .partial_cmp(&conf(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b["since"].as_str().cmp(&a["since"].as_str()))
            .then_with(|| a["session_id"].as_str().cmp(&b["session_id"].as_str()))
    });
    let total = items.len();
    items.truncate(limit);
    respond(
        &l.tenant,
        object(json!({
            "scope": scope.label(),
            "open_sessions": open.len(),
            "total": total,
            "items": items,
            "note": "blocked is an inference: a pending-input signal with no later event, or two consecutive failures of the same class; a response given outside the hook surface is invisible",
        })),
    )
}

/// `GET /v1/state?at=` — every session active at `at`, as it stood then.
pub async fn state_at(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<Params>,
) -> Response {
    let l = match begin(&state, &headers).await {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let view = &l.view;
    let p = view.engine.projection();
    let scope = match Scope::from(&q, view) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let at = match time_param(&q, "at") {
        Ok(t) => t.unwrap_or_else(Timestamp::now),
        Err(r) => return *r,
    };
    let snap = p.state_at(at);
    let sessions: Vec<Value> = snap
        .sessions
        .iter()
        .filter(|st| scope.project_ok(st.project_id))
        .map(sh::session_state)
        .collect();
    let blocked = sessions.iter().filter(|s| s["blocked"] == true).count();
    respond(
        &l.tenant,
        object(json!({
            "scope": scope.label(),
            "at": sh::ts(at),
            "total": sessions.len(),
            "blocked": blocked,
            "sessions": sessions,
        })),
    )
}

/// `GET /v1/events?after=&limit=` — the tenant's events in `source_seq`
/// order, strictly after the cursor, as stored.
/// Every stored event with `source_seq > after`, unordered: the manifest's
/// segments decoded only past the cursor (each one's range is known), then
/// the WAL. Shared by `/v1/events` and the webhook worker.
pub(crate) fn scan_events_after(
    db: &attemptdb_storage::Database,
    after: u64,
) -> anyhow::Result<Vec<Event>> {
    let mut out = Vec::new();
    let reader = attemptdb_storage::blobs::BlobReader::new(
        db.blob_store(),
        db.key_provider().map(|k| k.as_ref()),
    );
    for seg in &db.manifest().segments {
        if seg.max_source_seq <= after {
            continue;
        }
        let path = attemptdb_storage::segment::segments_dir(db.root()).join(&seg.file);
        for b in attemptdb_storage::segment::read_segment_batches(&path)? {
            out.extend(
                attemptdb_storage::segment::batch_to_events_with(&b, Some(&reader))?
                    .into_iter()
                    .filter(|e| e.source_seq > after),
            );
        }
    }
    out.extend(
        db.memtable_events()
            .iter()
            .filter(|e| e.source_seq > after)
            .cloned(),
    );
    Ok(out)
}

pub async fn events(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<Params>,
) -> Response {
    let l = match begin(&state, &headers).await {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let view = &l.view;
    let (scope, limit) = match (Scope::from(&q, view), limit_param(&q, DEFAULT_LIMIT)) {
        (Ok(s), Ok(l)) => (s, l),
        (Err(r), _) | (_, Err(r)) => return *r,
    };
    let after: u64 = match q.get("after").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => 0,
        Some(s) => match s.parse() {
            Ok(n) => n,
            Err(_) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    format!("after={s:?} is not a source_seq"),
                );
            }
        },
    };
    // A backfill by sequence reads the whole history, whatever window the
    // resident view keeps: the manifest's segments, decoded only past the
    // cursor (each one's `source_seq` range is known), then the WAL.
    let tenant = l.tenant.clone();
    let st = Arc::clone(&state);
    let scanned = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Event>> {
        let db = st.tenants.open(&tenant)?;
        let db = db
            .lock()
            .map_err(|_| anyhow::anyhow!("tenant {tenant}: database poisoned"))?;
        scan_events_after(&db, after)
    })
    .await;
    let mut selected: Vec<Event> = match scanned {
        Ok(Ok(v)) => v.into_iter().filter(|e| scope.event_ok(e)).collect(),
        Ok(Err(e)) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("cannot read the tenant: {e:#}"),
            );
        }
        Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, format!("task failed: {e}")),
    };
    selected.sort_by_key(|e| e.source_seq);
    let has_more = selected.len() > limit;
    selected.truncate(limit);
    let next = selected.last().map_or(after, |e| e.source_seq);
    let events: Vec<Value> = selected
        .iter()
        .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
        .collect();
    respond(
        &l.tenant,
        object(json!({
            "scope": scope.label(),
            "after": after,
            "count": events.len(),
            "next": next,
            "has_more": has_more,
            "last_source_seq": view.stats.last_source_seq,
            "events": events,
        })),
    )
}

#[derive(Debug, Deserialize)]
pub struct QueryBody {
    pub statement: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Result rows plus notes as one JSON object, capped at `limit` rows.
pub fn result_json(statement: &str, r: &QueryResult, limit: usize) -> Value {
    let rows = r.to_json();
    let total = rows.as_array().map(Vec::len).unwrap_or(0);
    let rows = match rows {
        Value::Array(mut a) if a.len() > limit => {
            a.truncate(limit);
            Value::Array(a)
        }
        other => other,
    };
    let mut notes = r.notes.clone();
    if total > limit {
        notes.push(format!("{total} rows; first {limit} returned"));
    }
    json!({
        "statement": statement,
        "kind": match r.kind {
            ResultKind::Rows => "rows",
            ResultKind::Explanation => "explanation",
            ResultKind::Empty => "empty",
        },
        "columns": r.column_names(),
        "row_count": total,
        "truncated": total > limit,
        "rows": rows,
        "notes": notes,
    })
}

/// `POST /v1/query` — AttemptQL or SQL over the tenant's engine.
pub async fn query(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<Params>,
    body: Result<Json<QueryBody>, JsonRejection>,
) -> Response {
    let l = match begin(&state, &headers).await {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return error(e.status(), e.body_text()),
    };
    let limit = match limit_param(&q, body.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)) {
        Ok(n) => n,
        Err(r) => return *r,
    };
    let statement = body
        .statement
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_string();
    if statement.is_empty() {
        return error(StatusCode::BAD_REQUEST, "statement is empty");
    }
    let run = tokio::time::timeout(QUERY_TIMEOUT, l.view.engine.query(&statement)).await;
    match run {
        Ok(Ok(r)) => respond(&l.tenant, object(result_json(&statement, &r, limit))),
        Ok(Err(e @ QueryError::Parse { .. })) => {
            error(StatusCode::BAD_REQUEST, format_parse_error(&statement, &e))
        }
        Ok(Err(e)) => error(StatusCode::BAD_REQUEST, e.to_string()),
        Err(_) => error(
            StatusCode::REQUEST_TIMEOUT,
            format!(
                "statement exceeded the {} s budget",
                QUERY_TIMEOUT.as_secs()
            ),
        ),
    }
}

/// `GET /v1/status` — the tenant's counts and the cache behind them.
pub async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let l = match begin(&state, &headers).await {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let view = &l.view;
    let p = view.engine.projection();
    let (cache, reprojected) = state.tenants.cache_stats(&l.tenant).unwrap_or_default();
    respond(
        &l.tenant,
        object(json!({
            "capture_mode": state.config.capture_mode.as_str(),
            "events": view.event_count(),
            "sessions": p.sessions.len(),
            "open_sessions": p.sessions.iter().filter(|s| s.ended_at.is_none()).count(),
            "turns": p.turns.len(),
            "tool_calls": p.tool_calls.len(),
            "attempts": p.attempts.len(),
            "handoffs": p.handoffs.len(),
            "work_units": p.work_units.len(),
            "decisions": p.decisions.len(),
            "retracted_sessions": p.retracted_ids.sessions.len(),
            "last_event_at": sh::ts_opt(view.last_event_at),
            "projects": view.projects.iter().map(|pr| json!({
                "project_id": sh::id(&pr.project_id),
                "name": pr.name,
                "repo_remote": pr.repo_remote,
                "events": pr.events,
                "sessions": pr.sessions,
            })).collect::<Vec<_>>(),
            "providers": view.providers.iter().map(|pr| json!({
                "provider": pr.provider,
                "events": pr.events,
                "last_event_at": sh::ts_opt(pr.last_event_at),
            })).collect::<Vec<_>>(),
            "view_window": view.window_since.map(|t| json!({
                "days": state.config.view_window_days,
                "since": sh::ts(t),
                "note": "counts above are the resident window; /v1/events reads the whole history",
            })),
            "storage": {
                "generation": view.stats.generation,
                "segments": view.stats.segments,
                "segment_rows": view.stats.segment_rows,
                "memtable_rows": view.stats.memtable_rows,
                "wal_bytes": view.stats.wal_bytes,
                "last_source_seq": view.stats.last_source_seq,
            },
            "cache": {
                "view_built_at": sh::ts(view.built_at),
                "decodes": cache.decodes,
                "refreshes": cache.refreshes,
                "segments": cache.segments,
                "projected_events": cache.events,
                "sessions_reprojected": reprojected,
            },
            "device_inferences": {
                "documents": l.inferences.documents,
                "items": l.inferences.len(),
            },
            "projection_stats": {
                "events_seen": p.stats.events_seen,
                "out_of_order_events": p.stats.out_of_order_events,
                "unpaired_tool_starts": p.stats.unpaired_tool_starts,
                "unpaired_tool_finishes": p.stats.unpaired_tool_finishes,
                "fifo_pairings": p.stats.fifo_pairings,
                "unknown_events": p.stats.unknown_events,
                "retracted_events": p.stats.retracted_events,
            },
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Item {
        at: i64,
        id: u128,
    }

    fn key(i: &Item) -> (Timestamp, Uuid) {
        (Timestamp::from_micros(i.at), Uuid::from_u128(i.id))
    }

    #[test]
    fn cursors_round_trip_and_reject_garbage() {
        let c = Cursor {
            at: Timestamp::from_micros(1_756_368_000_123_456),
            id: Uuid::from_u128(0xabc),
        };
        let text = encode_cursor(&c);
        assert!(text.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(decode_cursor(&text), Some(c));
        assert_eq!(decode_cursor("zz"), None);
        assert_eq!(decode_cursor(&hex::encode("nocolon")), None);
        assert_eq!(decode_cursor(&hex::encode("x:y")), None);
    }

    #[test]
    fn pages_are_keyset_stable_newest_first() {
        // Sorted (at desc, id asc), with a tie at `at = 5`.
        let items = [
            Item { at: 9, id: 1 },
            Item { at: 5, id: 2 },
            Item { at: 5, id: 3 },
            Item { at: 1, id: 4 },
        ];
        let refs: Vec<&Item> = items.iter().collect();
        let (p1, next) = page(&refs, key, None, 2);
        assert_eq!(p1.iter().map(|i| i.id).collect::<Vec<_>>(), [1, 2]);
        let next = next.expect("more");
        assert_eq!(next.id, Uuid::from_u128(2));
        let (p2, next) = page(&refs, key, Some(next), 2);
        assert_eq!(p2.iter().map(|i| i.id).collect::<Vec<_>>(), [3, 4]);
        assert!(next.is_none(), "last page");
        // A newer item arriving at the front does not shift the second page.
        let mut grown = vec![&Item { at: 12, id: 0 }];
        grown.extend(refs.iter().copied());
        let c = Cursor {
            at: Timestamp::from_micros(5),
            id: Uuid::from_u128(2),
        };
        let (p2b, _) = page(&grown, key, Some(c), 2);
        assert_eq!(p2b.iter().map(|i| i.id).collect::<Vec<_>>(), [3, 4]);
        let (exact, next) = page(&refs, key, None, 4);
        assert_eq!(exact.len(), 4);
        assert!(next.is_none(), "a full last page has no next cursor");
    }

    #[test]
    fn windows_overlap_inclusively() {
        let t = Timestamp::from_micros;
        assert!(overlaps(t(1), t(5), None, None));
        assert!(overlaps(t(1), t(5), Some(t(5)), None));
        assert!(!overlaps(t(1), t(5), Some(t(6)), None));
        assert!(overlaps(t(1), t(5), None, Some(t(1))));
        assert!(!overlaps(t(1), t(5), None, Some(t(0))));
        assert!(overlaps(t(3), t(3), Some(t(1)), Some(t(5))));
    }

    #[test]
    fn limits_default_clamp_and_reject() {
        let q = |v: &str| {
            let mut m = Params::new();
            m.insert("limit".into(), v.into());
            m
        };
        assert_eq!(limit_param(&Params::new(), 7).unwrap(), 7);
        assert_eq!(limit_param(&q("50"), 7).unwrap(), 50);
        assert_eq!(limit_param(&q("99999"), 7).unwrap(), MAX_LIMIT);
        assert!(limit_param(&q("0"), 7).is_err());
        assert!(limit_param(&q("ten"), 7).is_err());
        assert!(parse_time("-1h").is_some());
        assert!(parse_time("2026-08-30T00:00:00Z").is_some());
        assert!(parse_time("later").is_none());
    }
}

/// One device as `/v1/devices` reports it: its keys (from the key table)
/// and what it has uploaded (from the tenant's own events).
#[derive(Default)]
struct DeviceRow {
    keys: Vec<Value>,
    events: usize,
    sessions: BTreeSet<String>,
    providers: BTreeSet<String>,
    first_observed_at: Option<Timestamp>,
    last_observed_at: Option<Timestamp>,
    /// Server receipt time of the newest event: "last sync".
    last_ingested_at: Option<Timestamp>,
}

/// `GET /v1/devices` — every device the tenant knows, newest upload first.
/// This is what a "Connected · last sync 3 s ago" row is made of: the key
/// binding (label, user, scope, revoked = no key left) and the newest
/// `ingested_at` among the device's events. Facts only; no projection.
pub async fn devices(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let l = match begin(&state, &headers).await {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let view = &l.view;
    let mut rows: BTreeMap<DeviceId, DeviceRow> = BTreeMap::new();
    let entries = state.keys.read().map(|k| k.entries()).unwrap_or_default();
    for e in entries.iter().filter(|e| e.tenant == l.tenant.as_str()) {
        rows.entry(e.device_id).or_default().keys.push(json!({
            "sha256": e.sha256,
            "label": e.label,
            "scope": e.scope.as_str(),
            "user_id": e.user_id,
        }));
    }
    // The tenant database's own writer identity (see `tenants::Registry::open`).
    let server_device = DeviceId::derive(&["attemptdb-server", l.tenant.as_str()]);
    for ((device_id, is_meta), d) in &view.engine.facts().devices {
        if *device_id == server_device && *is_meta {
            continue; // the server's own retractions are not a device
        }
        let r = rows.entry(*device_id).or_default();
        r.events += d.events as usize;
        r.sessions
            .extend(d.sessions.iter().map(ToString::to_string));
        r.providers.extend(d.providers.iter().cloned());
        r.first_observed_at = match (r.first_observed_at, d.first_observed_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        r.last_observed_at = match (r.last_observed_at, d.last_observed_at) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        r.last_ingested_at = match (r.last_ingested_at, d.last_ingested_at) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
    let p = view.engine.projection();
    let seen = state.seen.lock().map(|m| m.clone()).unwrap_or_default();
    let mut devices: Vec<(Option<Timestamp>, Value)> = rows
        .into_iter()
        .map(|(id, r)| {
            let last_seen = seen.get(&(l.tenant.clone(), id)).copied();
            let retracted_sessions = r
                .sessions
                .iter()
                .filter(|s| {
                    s.parse::<attemptdb_core::SessionId>()
                        .map(|sid| p.retracted_ids.contains_session(&sid))
                        .unwrap_or(false)
                })
                .count();
            let device_keys = r.keys.iter().filter(|k| k["scope"] == "device").count();
            (
                r.last_ingested_at,
                json!({
                    "device_id": id,
                    "keys": r.keys,
                    "connected": device_keys > 0,
                    "last_seen_at": sh::ts_opt(last_seen),
                    "events": r.events,
                    "sessions": r.sessions.len(),
                    "retracted_sessions": retracted_sessions,
                    "providers": r.providers,
                    "first_observed_at": sh::ts_opt(r.first_observed_at),
                    "last_observed_at": sh::ts_opt(r.last_observed_at),
                    "last_sync_at": sh::ts_opt(r.last_ingested_at),
                }),
            )
        })
        .collect();
    devices.sort_by_key(|a| std::cmp::Reverse(a.0));
    let devices: Vec<Value> = devices.into_iter().map(|(_, v)| v).collect();
    respond(
        &l.tenant,
        object(json!({
            "count": devices.len(),
            "devices": devices,
        })),
    )
}
