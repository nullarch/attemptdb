//! The JSON API under `/api/`. Every handler refreshes the engine through
//! the store, so a long-running page sees new events without a restart.

use crate::json as j;
use crate::readonly::check_read_only;
use crate::scope::ScopeQuery;
use crate::store::{View, parse_time};
use crate::{AppState, html};
use anyhow::Result;
use attemptdb_core::{AttemptId, EventId, SessionId, Timestamp};
use attemptdb_project::{Attempt, AttentionKind, Projection, Session, WorkUnit};
use attemptdb_query::{QueryError, QueryResult, ResultKind, format_parse_error};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// Rows a single query result may carry back to the browser.
pub const MAX_ROWS: usize = 2000;
pub const DEFAULT_SESSIONS: usize = 10;
pub const DEFAULT_FAILURES: usize = 50;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn bad(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let message = format!("{e:#}");
        let status = if message.starts_with("unknown ") || message.starts_with("cannot parse") {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        Self { status, message }
    }
}

impl From<QueryError> for ApiError {
    fn from(e: QueryError) -> Self {
        let status = match &e {
            QueryError::Parse { .. } => StatusCode::BAD_REQUEST,
            _ if e.to_string().contains("not found") || e.to_string().contains("unknown") => {
                StatusCode::NOT_FOUND
            }
            _ => StatusCode::BAD_REQUEST,
        };
        Self {
            status,
            message: format!("{e}"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

pub type ApiResult = Result<Json<Value>, ApiError>;

pub type Params = HashMap<String, String>;

pub async fn view(state: &AppState, scope: &ScopeQuery) -> Result<Arc<View>, ApiError> {
    Ok(state.store.view(&scope.args()).await?)
}

pub fn param_usize(q: &Params, key: &str, default: usize) -> usize {
    q.get(key)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

pub fn param_flag(q: &Params, key: &str) -> bool {
    crate::scope::flag(&q.get(key).cloned())
}

/// An id argument that will be interpolated into a statement: prefix plus
/// hex/hyphens only.
pub fn id_token(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("an id is required".to_string());
    }
    if s.len() > 64
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "{s:?} is not an id (expected att_…, ses_…, trn_…, spn_… or ev_…)"
        ));
    }
    Ok(s.to_string())
}

/// Resolve a session id: full `ses_` id, bare uuid, or a hex prefix of at
/// least four characters that matches exactly one session.
pub fn find_session<'a>(p: &'a Projection, raw: &str) -> Result<&'a Session, ApiError> {
    let text = raw.trim();
    if let Ok(id) = text.parse::<SessionId>()
        && let Some(s) = p.session(id)
    {
        return Ok(s);
    }
    let needle: String = text
        .trim_start_matches("ses_")
        .chars()
        .filter(|c| *c != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    if needle.len() < 4 || !needle.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::bad(format!("{text:?} is not a session id")));
    }
    let matches: Vec<&Session> = p
        .sessions
        .iter()
        .filter(|s| s.session_id.0.simple().to_string().starts_with(&needle))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(ApiError::not_found(format!(
            "no session {text} among the {} loaded",
            p.sessions.len()
        ))),
        _ => Err(ApiError::bad(format!(
            "{text} is ambiguous ({} sessions)",
            matches.len()
        ))),
    }
}

/// A work unit by full id or by a unique short prefix (`wu_1a2b3c4d`).
pub fn find_work_unit<'a>(p: &'a Projection, raw: &str) -> Result<&'a WorkUnit, ApiError> {
    let text = raw.trim();
    if let Ok(id) = text.parse::<attemptdb_core::WorkUnitId>()
        && let Some(w) = p.work_units.iter().find(|w| w.work_unit_id == id)
    {
        return Ok(w);
    }
    let needle: String = text
        .trim_start_matches("wu_")
        .chars()
        .filter(|c| *c != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    if needle.len() < 4 || !needle.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::bad(format!("{text:?} is not a work unit id")));
    }
    let matches: Vec<&WorkUnit> = p
        .work_units
        .iter()
        .filter(|w| w.work_unit_id.0.simple().to_string().starts_with(&needle))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(ApiError::not_found(format!(
            "no work unit {text} among the {} loaded",
            p.work_units.len()
        ))),
        many => Err(ApiError::bad(format!(
            "{text:?} matches {} work units; use more characters",
            many.len()
        ))),
    }
}

pub fn find_attempt<'a>(p: &'a Projection, raw: &str) -> Result<&'a Attempt, ApiError> {
    let text = raw.trim();
    if let Ok(id) = text.parse::<AttemptId>()
        && let Some(a) = p.attempts.iter().find(|a| a.attempt_id == id)
    {
        return Ok(a);
    }
    let needle: String = text
        .trim_start_matches("att_")
        .chars()
        .filter(|c| *c != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    if needle.len() < 4 || !needle.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::bad(format!("{text:?} is not an attempt id")));
    }
    let matches: Vec<&Attempt> = p
        .attempts
        .iter()
        .filter(|a| a.attempt_id.0.simple().to_string().starts_with(&needle))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(ApiError::not_found(format!(
            "no attempt {text} among the {} loaded",
            p.attempts.len()
        ))),
        _ => Err(ApiError::bad(format!(
            "{text} is ambiguous ({} attempts)",
            matches.len()
        ))),
    }
}

/// `WHY …` statement for a subject: `project` (default), `ses_` → blocked,
/// `att_` → failed, anything else → a project name.
pub fn why_statement(subject: &str) -> Result<String, String> {
    let s = subject.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("project") {
        return Ok("WHY project STATUS BLOCKED".to_string());
    }
    if s.starts_with("att_") {
        return Ok(format!("WHY {} FAILED", id_token(s)?));
    }
    if s.starts_with("ses_") {
        return Ok(format!("WHY session '{}' STATUS BLOCKED", id_token(s)?));
    }
    let hexish = s
        .chars()
        .filter(|c| *c != '-')
        .all(|c| c.is_ascii_hexdigit());
    if hexish && s.len() >= 4 {
        return Ok(format!("WHY session '{s}' STATUS BLOCKED"));
    }
    Ok(format!(
        "WHY project '{}' STATUS BLOCKED",
        s.replace('\'', "''")
    ))
}

pub fn trace_statement(id: &str, depth: Option<usize>, direction: &str) -> Result<String, String> {
    let id = id_token(id)?;
    let mut s = format!("TRACE {id} CAUSES");
    if let Some(d) = depth {
        s.push_str(&format!(" DEPTH {}", d.clamp(1, 50)));
    }
    match direction.trim().to_ascii_lowercase().as_str() {
        "" | "up" | "causes" => {}
        "down" | "effects" => s.push_str(" DIRECTION DOWN"),
        "both" => s.push_str(" DIRECTION BOTH"),
        other => return Err(format!("direction {other:?}: use causes, effects or both")),
    }
    Ok(s)
}

pub fn state_statement(at: Timestamp) -> String {
    format!("STATE project AT '{}'", html::rfc3339(at))
}

/// Result rows plus notes as one JSON object.
pub fn result_json(statement: &str, r: &QueryResult) -> Value {
    let rows = r.to_json();
    let total = rows.as_array().map(Vec::len).unwrap_or(0);
    let rows = match rows {
        Value::Array(mut a) if a.len() > MAX_ROWS => {
            a.truncate(MAX_ROWS);
            Value::Array(a)
        }
        other => other,
    };
    let mut notes = r.notes.clone();
    if total > MAX_ROWS {
        notes.push(format!("{total} rows; first {MAX_ROWS} returned"));
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
        "rows": rows,
        "notes": notes,
    })
}

/// Run a statement and map parse errors to a caret rendering.
pub async fn run(view: &View, statement: &str) -> Result<QueryResult, ApiError> {
    match view.engine.query(statement).await {
        Ok(r) => Ok(r),
        Err(e @ QueryError::Parse { .. }) => Err(ApiError::bad(format_parse_error(statement, &e))),
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn status(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    Ok(Json(j::status(&v)))
}

pub async fn projects(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let projects: Vec<Value> = v
        .status
        .projects
        .iter()
        .map(|p| {
            json!({
                "project_id": p.project_id.as_ref().map(j::id),
                "name": p.name,
                "events": p.events,
                "sessions": p.sessions,
                "current": v.scope.project_id.is_some() && v.scope.project_id == p.project_id,
            })
        })
        .collect();
    Ok(Json(
        json!({ "projects": projects, "scope": v.scope.label }),
    ))
}

pub async fn sessions(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let limit = param_usize(&q, "limit", 100);
    let include_empty = param_flag(&q, "all_sessions");
    let sessions: Vec<Value> = j::sessions_sorted(p, include_empty)
        .into_iter()
        .take(limit)
        .map(|s| {
            j::session(
                s,
                v.session_capture
                    .get(&s.session_id)
                    .copied()
                    .unwrap_or_default(),
                None,
            )
        })
        .collect();
    Ok(Json(json!({
        "scope": v.scope.label,
        "total": p.sessions.len(),
        "sessions": sessions,
    })))
}

pub async fn timeline(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let limit = param_usize(&q, "limit", DEFAULT_SESSIONS);
    let page = param_usize(&q, "page", 1);
    let include_empty = param_flag(&q, "all_sessions");
    let with_tools = param_flag(&q, "tools");
    let all = j::sessions_sorted(p, include_empty);
    let total = all.len();
    let sessions: Vec<Value> = all
        .into_iter()
        .skip((page - 1) * limit)
        .take(limit)
        .map(|s| {
            let turns: Vec<Value> = j::turns_of(p, s)
                .into_iter()
                .map(|t| {
                    let attempts = j::attempts_of_turn(p, t)
                        .into_iter()
                        .map(|a| j::attempt(a, p, with_tools))
                        .collect();
                    j::turn(t, attempts)
                })
                .collect();
            j::session(
                s,
                v.session_capture
                    .get(&s.session_id)
                    .copied()
                    .unwrap_or_default(),
                Some(turns),
            )
        })
        .collect();
    Ok(Json(json!({
        "scope": v.scope.label,
        "events": v.engine.event_count(),
        "page": page,
        "limit": limit,
        "total_sessions": total,
        "sessions": sessions,
        "handoffs": p.handoffs.iter().map(j::handoff).collect::<Vec<_>>(),
        "work_units": j::work_units_sorted(p).iter().map(|w| j::work_unit(w)).collect::<Vec<_>>(),
        "decisions": p.decisions.iter().map(j::decision).collect::<Vec<_>>(),
        "inference_version": crate::INFERENCE_VERSION,
        "note": crate::TAGLINE,
    })))
}

pub async fn session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let s = find_session(p, &id)?;
    let turns: Vec<Value> = j::turns_of(p, s)
        .into_iter()
        .map(|t| {
            let attempts = j::attempts_of_turn(p, t)
                .into_iter()
                .map(|a| j::attempt(a, p, false))
                .collect();
            j::turn(t, attempts)
        })
        .collect();
    let tool_calls: Vec<Value> = p.tool_calls_of(s.session_id).map(j::tool_call).collect();
    let signals: Vec<Value> = p.signals_of(s.session_id).map(j::signal).collect();
    let blocked = p.why_blocked(s.session_id).map(|e| {
        json!({
            "claim": e.claim,
            "evidence": j::ids(&e.evidence),
            "confidence": j::conf(e.confidence),
            "uncertainty": e.uncertainty,
        })
    });
    let mut out = j::session(
        s,
        v.session_capture
            .get(&s.session_id)
            .copied()
            .unwrap_or_default(),
        Some(turns),
    );
    out["tool_calls"] = Value::Array(tool_calls);
    out["signals"] = Value::Array(signals);
    out["blocked"] = blocked.unwrap_or(Value::Null);
    out["handoffs"] = Value::Array(
        p.handoffs
            .iter()
            .filter(|h| h.from_session == s.session_id || h.to_session == s.session_id)
            .map(j::handoff)
            .collect(),
    );
    Ok(Json(out))
}

pub async fn attempt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let a = find_attempt(p, &id)?;
    let readable = j::id(&a.attempt_id);
    let readable = readable.as_str().unwrap_or_default().to_string();
    let evidence = run(&v, &format!("SHOW EVIDENCE FOR {readable}")).await?;
    let why = run(&v, &format!("WHY {readable} FAILED")).await?;
    let trace = run(&v, &format!("TRACE {readable} CAUSES")).await?;
    let mut out = j::attempt(a, p, true);
    out["evidence_events"] = result_json(&format!("SHOW EVIDENCE FOR {readable}"), &evidence);
    out["why"] = result_json(&format!("WHY {readable} FAILED"), &why);
    out["trace"] = result_json(&format!("TRACE {readable} CAUSES"), &trace);
    Ok(Json(out))
}

pub async fn failures(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let limit = param_usize(&q, "limit", DEFAULT_FAILURES);
    let mut failed: Vec<&Attempt> = p
        .attempts
        .iter()
        .filter(|a| a.outcome.is_failure())
        .collect();
    failed.sort_by_key(|a| std::cmp::Reverse(a.started_at));
    Ok(Json(json!({
        "scope": v.scope.label,
        "total": failed.len(),
        "attempts": failed.iter().take(limit).map(|a| j::attempt(a, p, true)).collect::<Vec<_>>(),
        "note": "attempts are Tier 1 inferences; /api/trace/<att_id> and /api/why?subject=<att_id> carry the evidence",
    })))
}

pub async fn handoffs(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let limit = param_usize(&q, "limit", DEFAULT_FAILURES);
    let mut list: Vec<&attemptdb_project::Handoff> = p.handoffs.iter().collect();
    list.sort_by_key(|a| std::cmp::Reverse(a.at));
    Ok(Json(json!({
        "scope": v.scope.label,
        "total": list.len(),
        "handoffs": list.iter().take(limit).map(|h| j::handoff(h)).collect::<Vec<_>>(),
        "note": "a handoff is a session of another agent starting shortly after one went idle in the same project (tier1-v1 heuristic)",
    })))
}

pub async fn work_units(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let limit = param_usize(&q, "limit", DEFAULT_FAILURES);
    let status = q.get("status").map(|s| s.trim().to_ascii_lowercase());
    let list: Vec<Value> = j::work_units_sorted(p)
        .into_iter()
        .filter(|w| {
            status
                .as_deref()
                .is_none_or(|s| s.is_empty() || w.status.as_str() == s)
        })
        .take(limit)
        .map(j::work_unit)
        .collect();
    Ok(Json(json!({
        "scope": v.scope.label,
        "total": p.work_units.len(),
        "work_units": list,
        "note": "a work unit is a connected component of turns (shared paths, consecutive turns, handoffs); phase and status are tier1-v1 heuristics with evidence ids",
    })))
}

pub async fn decisions(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let limit = param_usize(&q, "limit", DEFAULT_FAILURES);
    let mut list: Vec<&attemptdb_project::Decision> = p.decisions.iter().collect();
    list.sort_by_key(|a| std::cmp::Reverse(a.decided_at));
    Ok(Json(json!({
        "scope": v.scope.label,
        "total": list.len(),
        "decisions": list.iter().take(limit).map(|d| j::decision(d)).collect::<Vec<_>>(),
        "note": "decisions are derived from the attempt structure (a superseded failure, a denial followed by another tool); nothing here was stated by a human",
    })))
}

pub async fn why(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let subject = q.get("subject").cloned().unwrap_or_default();
    let statement = why_statement(&subject).map_err(ApiError::bad)?;
    let r = run(&v, &statement).await?;
    Ok(Json(result_json(&statement, &r)))
}

pub async fn trace(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let depth = q.get("depth").and_then(|d| d.trim().parse::<usize>().ok());
    let direction = q.get("direction").cloned().unwrap_or_default();
    let statement = trace_statement(&id, depth, &direction).map_err(ApiError::bad)?;
    let r = run(&v, &statement).await?;
    Ok(Json(result_json(&statement, &r)))
}

pub async fn state(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let at = match q.get("at").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => Timestamp::now(),
        Some(spec) => parse_time(spec).ok_or_else(|| {
            ApiError::bad(format!(
                "cannot parse at={spec:?}: use RFC 3339, YYYY-MM-DD, now, today, yesterday or -<n>(s|m|h|d|w)"
            ))
        })?,
    };
    let statement = state_statement(at);
    let r = run(&v, &statement).await?;
    let mut out = result_json(&statement, &r);
    out["at"] = j::ts(at);
    let snap = v.engine.projection().state_at(at);
    out["sessions"] = Value::Array(snap.sessions.iter().map(j::session_state).collect());
    Ok(Json(out))
}

/// `SELECT * FROM events WHERE event_id …` for a full or prefixed id.
pub fn evidence_sql(raw: &str) -> Result<String, ApiError> {
    let text = raw.trim();
    if let Ok(id) = text.parse::<EventId>() {
        return Ok(format!(
            "SELECT * FROM events WHERE event_id = 'ev_{id}' LIMIT 1"
        ));
    }
    let hex: String = text
        .trim_start_matches("ev_")
        .chars()
        .filter(|c| *c != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    if hex.len() < 4 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::bad(format!("{text:?} is not an event id")));
    }
    // Hyphenated form: the first eight hex digits are contiguous.
    let prefix: String = hex.chars().take(8).collect();
    Ok(format!(
        "SELECT * FROM events WHERE event_id LIKE 'ev_{prefix}%' ORDER BY observed_at LIMIT 2"
    ))
}

pub async fn evidence_row(view: &View, raw: &str) -> Result<Value, ApiError> {
    let sql = evidence_sql(raw)?;
    let r = view.engine.sql(&sql).await?;
    let rows = r.to_json();
    let rows = rows.as_array().cloned().unwrap_or_default();
    match rows.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(ApiError::not_found(format!(
            "no event {} among the {} loaded",
            raw.trim(),
            view.engine.event_count()
        ))),
        _ => Err(ApiError::bad(format!(
            "{} is ambiguous; give more digits",
            raw.trim()
        ))),
    }
}

pub async fn evidence(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let row = evidence_row(&v, &id).await?;
    Ok(Json(
        json!({ "event": row, "note": "events are facts: this row is exactly what the hook reported" }),
    ))
}

#[derive(Deserialize)]
pub struct QueryBody {
    pub statement: String,
    #[serde(default)]
    pub format: Option<String>,
}

pub async fn query(
    State(state): State<Arc<AppState>>,
    Query(q): Query<Params>,
    Json(body): Json<QueryBody>,
) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    check_read_only(&body.statement).map_err(ApiError::bad)?;
    let statement = body
        .statement
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_string();
    let r = run(&v, &statement).await?;
    let format = body
        .format
        .as_deref()
        .unwrap_or("json")
        .trim()
        .to_ascii_lowercase();
    let mut out = result_json(&statement, &r);
    match format.as_str() {
        "json" => {}
        "table" => {
            let (capped, _) = cap(&r, MAX_ROWS);
            out["text"] = Value::String(capped.render_table(None));
        }
        "csv" => {
            let (capped, _) = cap(&r, MAX_ROWS);
            out["text"] = Value::String(capped.render_csv());
        }
        other => {
            return Err(ApiError::bad(format!(
                "format {other:?}: use json, table or csv"
            )));
        }
    }
    Ok(Json(out))
}

/// The first `limit` rows of a result, with the original row count.
pub fn cap(r: &QueryResult, limit: usize) -> (QueryResult, usize) {
    let total = r.row_count();
    if total <= limit {
        return (r.clone(), total);
    }
    let mut remaining = limit;
    let mut batches = Vec::new();
    for b in &r.batches {
        if remaining == 0 {
            break;
        }
        let take = b.num_rows().min(remaining);
        batches.push(b.slice(0, take));
        remaining -= take;
    }
    (
        QueryResult::new(r.schema.clone(), batches, r.kind, r.notes.clone()),
        total,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements() {
        assert_eq!(why_statement("").unwrap(), "WHY project STATUS BLOCKED");
        assert_eq!(why_statement("att_abcd").unwrap(), "WHY att_abcd FAILED");
        assert_eq!(
            why_statement("ses_abcd").unwrap(),
            "WHY session 'ses_abcd' STATUS BLOCKED"
        );
        assert_eq!(
            why_statement("acme's repo").unwrap(),
            "WHY project 'acme''s repo' STATUS BLOCKED"
        );
        assert!(why_statement("att_x' OR 1=1").is_err());
        assert_eq!(
            trace_statement("att_abcd", Some(3), "both").unwrap(),
            "TRACE att_abcd CAUSES DEPTH 3 DIRECTION BOTH"
        );
        assert!(trace_statement("att_abcd", None, "sideways").is_err());
        assert!(evidence_sql("ev_zz").is_err());
        assert!(
            evidence_sql("ev_abcdef01")
                .unwrap()
                .contains("LIKE 'ev_abcdef01%'")
        );
    }
}

/// `GET /api/overview` — everything the Overview refetches when the
/// database changes: the live sessions, the current work unit, the queue
/// size. Bounded by construction: no history, no all-project scan.
pub async fn overview(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let now = attemptdb_core::Timestamp::now();
    let snap = p.state_at(now);
    let items = p.attention_at(now, attemptdb_project::DEFAULT_MIN_CONFIDENCE);
    let open = snap.sessions.iter().filter(|s| s.open).count();
    let sessions: Vec<Value> = snap
        .sessions
        .iter()
        .filter(|s| {
            s.open
                && crate::html::elapsed_ms(s.last_activity_at, now) <= crate::LIVE_WINDOW_MS
        })
        .map(|s| {
            let mut v = j::session_state(s);
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "in_flight_tools".into(),
                    json!(
                        s.in_flight_tool_calls
                            .iter()
                            .filter_map(|id| p.tool_calls.iter().find(|c| &c.tool_call_id == id))
                            .map(|c| c.tool.name.clone())
                            .collect::<Vec<_>>()
                    ),
                );
                obj.insert(
                    "provider_name".into(),
                    json!(p.session(s.session_id).map(|x| x.provider.as_str())),
                );
                obj.insert(
                    "project_name".into(),
                    json!(p.session(s.session_id).map(|x| x.project_name.clone())),
                );
            }
            v
        })
        .collect();
    let current = j::work_units_sorted(p).into_iter().next().map(j::work_unit);
    Ok(Json(json!({
        "scope": v.scope.label,
        "at": j::ts(now),
        "active_sessions": sessions,
        "open_sessions": open,
        "live_window_ms": crate::LIVE_WINDOW_MS,
        "current_work_unit": current,
        "attention_total": items.len(),
        "attention": items.iter().take(3).map(j::attention_item).collect::<Vec<_>>(),
        "events": v.engine.event_count(),
        "note": "live execution is observed fact; the work unit, its phase and the queue are inferences with evidence ids",
    })))
}

/// `GET /api/attention` — the Needs You queue.
pub async fn attention(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let limit = param_usize(&q, "limit", 50);
    let min = q
        .get("min_confidence")
        .and_then(|s| s.trim().parse::<f32>().ok())
        .unwrap_or(attemptdb_project::DEFAULT_MIN_CONFIDENCE);
    let kind = q.get("kind").and_then(|s| AttentionKind::parse(s));
    let now = attemptdb_core::Timestamp::now();
    let items: Vec<attemptdb_project::AttentionItem> = p
        .attention_at(now, min)
        .into_iter()
        .filter(|i| kind.is_none_or(|k| i.kind == k))
        .collect();
    Ok(Json(json!({
        "scope": v.scope.label,
        "at": j::ts(now),
        "open_sessions": p.sessions.iter().filter(|s| s.ended_at.is_none()).count(),
        "total": items.len(),
        "min_confidence": min,
        "items": items.iter().take(limit).map(|i| j::attention_item(i)).collect::<Vec<_>>(),
        "note": "only an unanswered permission request, an agent waiting for input, the same failure twice with nothing superseding it, or two open work units editing the same paths reach this queue",
    })))
}

/// `GET /api/work` — the board: work units grouped into the three columns.
pub async fn work(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> ApiResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let limit = param_usize(&q, "limit", 100);
    let now = attemptdb_core::Timestamp::now();
    let blocked: Vec<attemptdb_core::WorkUnitId> = p
        .attention_at(now, attemptdb_project::DEFAULT_MIN_CONFIDENCE)
        .into_iter()
        .filter_map(|i| i.work_unit_id)
        .collect();
    let mut columns: std::collections::BTreeMap<&str, Vec<Value>> = Default::default();
    for w in j::work_units_sorted(p).into_iter().take(limit) {
        let col = match w.status {
            attemptdb_project::WorkUnitStatus::Completed
            | attemptdb_project::WorkUnitStatus::Abandoned => "finished",
            _ if w.phase == attemptdb_project::Phase::Blocked
                || blocked.contains(&w.work_unit_id) =>
            {
                "blocked"
            }
            _ => "active",
        };
        columns.entry(col).or_default().push(j::work_unit(w));
    }
    Ok(Json(json!({
        "scope": v.scope.label,
        "total": p.work_units.len(),
        "active": columns.remove("active").unwrap_or_default(),
        "blocked": columns.remove("blocked").unwrap_or_default(),
        "finished": columns.remove("finished").unwrap_or_default(),
        "decisions": p.decisions.iter().rev().take(20).map(j::decision).collect::<Vec<_>>(),
        "note": "columns are derived from the inferred status/phase and the Needs You queue; AttemptDB never invents planned work",
    })))
}

/// How often the live stream stats the database files. The budget is
/// "live activity appears within 2 seconds of durable ingestion".
const LIVE_POLL: std::time::Duration = std::time::Duration::from_millis(1000);

/// `GET /api/live` — server-sent invalidation. Each message names the new
/// revision; the client refetches the smallest resource its page needs, and
/// never reloads all history for one event.
///
/// The probe is a `stat` of the WAL, manifest and spool files: it does not
/// open the database, decode a segment or project anything.
pub async fn live(
    State(state): State<Arc<AppState>>,
    Query(q): Query<Params>,
) -> axum::response::Sse<impl futures_core::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>>
{
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    use tokio_stream::StreamExt as _;

    let demo = ScopeQuery::from_map(&q).demo();
    let mut last: Option<String> = None;
    let stream = tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(LIVE_POLL))
        .filter_map(move |_| {
            let revision = state.store.fingerprint_of(demo).revision();
            if last.as_deref() == Some(revision.as_str()) {
                return None;
            }
            let first = last.is_none();
            last = Some(revision.clone());
            let payload = json!({
                "revision": revision,
                "at": j::ts(attemptdb_core::Timestamp::now()),
                "initial": first,
            });
            Some(Ok(SseEvent::default()
                .event("change")
                .data(payload.to_string())))
        });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// `GET /card.svg` — the sanitized summary card for the current scope.
/// Served as an image so it can be dragged into a README or an issue.
pub async fn card(
    State(state): State<Arc<AppState>>,
    Query(q): Query<Params>,
) -> Result<axum::response::Response, ApiError> {
    use axum::http::header;
    use axum::response::IntoResponse;
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let svg = crate::card::render(
        v.engine.projection(),
        &crate::card::CardOptions {
            project: v.scope.project_name.clone(),
            window: Some(v.scope.label.clone()),
            attribution: !param_flag(&q, "no_attribution"),
        },
    );
    Ok((
        [
            (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (
                header::CONTENT_DISPOSITION,
                "inline; filename=\"attemptdb-card.svg\"",
            ),
        ],
        svg,
    )
        .into_response())
}
