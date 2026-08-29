//! Execution of AttemptQL statements against a [`QueryEngine`].
//!
//! `SHOW` compiles to SQL over the registered tables. `WHY`, `TRACE`,
//! `STATE`, `DIFF` and `WHAT IS` evaluate the projection directly and build
//! their rows with explicit schemas. Every explanation-style result carries
//! `evidence` (event ids), `confidence` and `uncertainty` columns.

use crate::attemptql::{
    DiffStatement, Filter, ShowStatement, ShowTarget, StateStatement, Statement, Subject, TimeExpr,
    TraceStatement, WhatIsStatement, WhyStatement,
};
use crate::error::{QueryError, Result};
use crate::graph::{endpoint_id, endpoint_type};
use crate::ids::{looks_like_id, readable, readable_list, resolve, split_prefix};
use crate::result::{QueryResult, ResultKind};
use crate::tables::{Kind, TableBuilder, Val, session_state, turn_evidence};
use crate::{QueryEngine, TableInfo};
use attemptdb_core::event::Provider;
use attemptdb_core::{
    AgentId, AttemptId, EventId, ProjectId, SessionId, SpanId, Timestamp, TurnId,
};
use attemptdb_project::{
    ALGORITHM_VERSION, Attempt, AttemptOutcome, CoverageGrade, EdgeEndpoint, Session, SessionState,
    ToolCall, Turn,
};
use std::collections::{BTreeMap, HashMap};

const DEFAULT_LIMIT: usize = 100;
const DEFAULT_TRACE_DEPTH: usize = 10;
const RECENT_WINDOW_MICROS: i64 = 15 * 60 * 1_000_000;

/// SQL string literal.
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// SQL literal for a UTC microsecond timestamp, typed to match the
/// `Timestamp(µs, UTC)` columns.
fn ts_lit(t: Timestamp) -> String {
    format!(
        "arrow_cast({}, 'Timestamp(Microsecond, Some(\"UTC\"))')",
        lit(&t.to_rfc3339())
    )
}

fn normalize_provider(v: &str) -> String {
    v.parse::<Provider>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|_| v.to_string())
}

fn coverage_confidence(c: CoverageGrade) -> f32 {
    match c {
        CoverageGrade::Full => 0.9,
        CoverageGrade::Partial => 0.7,
        CoverageGrade::Minimal => 0.5,
        CoverageGrade::Unknown => 0.3,
    }
}

fn coverage_text(s: &Session) -> String {
    if s.coverage == CoverageGrade::Full {
        return "Coverage is full (start, end, prompts and tool calls observed); only hook events are visible, so work outside the hook surface is not captured.".to_string();
    }
    let mut missing: Vec<&str> = Vec::new();
    if s.start_event_id.is_none() {
        missing.push("no session start");
    }
    if s.end_event_id.is_none() {
        missing.push("no session end");
    }
    if s.prompt_count == 0 {
        missing.push("no prompts");
    }
    if s.tool_call_count == 0 {
        missing.push("no tool events");
    }
    format!(
        "Coverage is {} ({}); events may be missing, so the session may have moved on unobserved.",
        s.coverage.as_str(),
        missing.join(", ")
    )
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// What a `SHOW` statement compiled to.
struct Compiled {
    sql: String,
    limit: usize,
    notes: Vec<String>,
}

/// Predicate selecting the session states a `STATE` subject covers.
type KeepSession = Box<dyn Fn(&SessionState) -> bool>;

struct SessionSubject<'a> {
    label: String,
    sessions: Vec<&'a Session>,
}

impl QueryEngine {
    pub(crate) async fn execute(&self, stmt: Statement) -> Result<QueryResult> {
        match stmt {
            Statement::Explain(inner) => self.explain_statement(*inner).await,
            Statement::Show(s) => self.exec_show(&s).await,
            Statement::Why(w) => self.exec_why(&w),
            Statement::Trace(t) => self.exec_trace(&t),
            Statement::State(s) => self.exec_state(&s),
            Statement::Diff(d) => self.exec_diff(&d),
            Statement::WhatIs(w) => self.exec_what_is(&w),
        }
    }

    // --- lookups ---------------------------------------------------------------

    fn session_ids(&self) -> Vec<SessionId> {
        self.projection
            .sessions
            .iter()
            .map(|s| s.session_id)
            .collect()
    }

    fn find_session(&self, text: &str) -> Result<&Session> {
        let id = resolve::<SessionId>(text, &self.session_ids(), "session", true)?;
        self.projection.session(id).ok_or_else(|| {
            QueryError::not_found(format!("session {} is not loaded", readable(&id)))
        })
    }

    fn find_attempt(&self, text: &str) -> Result<&Attempt> {
        let ids: Vec<AttemptId> = self
            .projection
            .attempts
            .iter()
            .map(|a| a.attempt_id)
            .collect();
        let id = resolve::<AttemptId>(text, &ids, "attempt", true)?;
        self.projection
            .attempts
            .iter()
            .find(|a| a.attempt_id == id)
            .ok_or_else(|| {
                QueryError::not_found(format!("attempt {} is not loaded", readable(&id)))
            })
    }

    fn find_turn(&self, text: &str) -> Result<&Turn> {
        let ids: Vec<TurnId> = self.projection.turns.iter().map(|t| t.turn_id).collect();
        let id = resolve::<TurnId>(text, &ids, "turn", true)?;
        self.projection
            .turns
            .iter()
            .find(|t| t.turn_id == id)
            .ok_or_else(|| QueryError::not_found(format!("turn {} is not loaded", readable(&id))))
    }

    fn find_tool_call(&self, text: &str) -> Result<&ToolCall> {
        let ids: Vec<SpanId> = self
            .projection
            .tool_calls
            .iter()
            .map(|c| c.tool_call_id)
            .collect();
        let id = resolve::<SpanId>(text, &ids, "tool call", true)?;
        self.projection
            .tool_calls
            .iter()
            .find(|c| c.tool_call_id == id)
            .ok_or_else(|| {
                QueryError::not_found(format!("tool call {} is not loaded", readable(&id)))
            })
    }

    fn resolve_event(&self, text: &str) -> Result<EventId> {
        resolve::<EventId>(text, self.event_ids(), "event", true)
    }

    /// Project ids matching a name or id; errors when nothing matches.
    fn project_ids_for(&self, value: &str) -> Result<Vec<ProjectId>> {
        let v = value.trim();
        let (prefix, _) = split_prefix(v);
        if prefix == Some("prj_") || (prefix.is_none() && looks_like_id(v) && !v.contains('/')) {
            let ids: Vec<ProjectId> = {
                let mut ids: Vec<ProjectId> = self
                    .projection
                    .sessions
                    .iter()
                    .map(|s| s.project_id)
                    .collect();
                ids.sort();
                ids.dedup();
                ids
            };
            if let Ok(id) = resolve::<ProjectId>(v, &ids, "project", false) {
                return Ok(vec![id]);
            }
        }
        let mut ids: Vec<ProjectId> = self
            .projection
            .sessions
            .iter()
            .filter(|s| s.project_name.eq_ignore_ascii_case(v))
            .map(|s| s.project_id)
            .collect();
        ids.sort();
        ids.dedup();
        if ids.is_empty() {
            let mut known: Vec<String> = self
                .projection
                .sessions
                .iter()
                .map(|s| s.project_name.clone())
                .collect();
            known.sort();
            known.dedup();
            return Err(QueryError::not_found(format!(
                "unknown project '{v}'; loaded projects: {} (SHOW SESSIONS lists them)",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            )));
        }
        Ok(ids)
    }

    /// Sessions a session-or-project subject refers to, most recent first.
    fn subject_sessions(&self, subject: &Subject) -> Result<SessionSubject<'_>> {
        let mut sessions: Vec<&Session> = match subject {
            Subject::Project(None) => self.projection.sessions.iter().collect(),
            Subject::Project(Some(name)) => {
                let ids = self.project_ids_for(name)?;
                self.projection
                    .sessions
                    .iter()
                    .filter(|s| ids.contains(&s.project_id))
                    .collect()
            }
            Subject::Session(id) => vec![self.find_session(id)?],
            Subject::Id(id) => match split_prefix(id.trim()).0 {
                Some("ses_") | None => vec![self.find_session(id)?],
                Some(other) => {
                    return Err(QueryError::plan(format!(
                        "expected a session (ses_…) or project subject, got a {other}… id"
                    )));
                }
            },
            other => {
                return Err(QueryError::plan(format!(
                    "expected a session or project subject, got {}",
                    subject_label(other)
                )));
            }
        };
        sessions.sort_by(|a, b| {
            b.last_event_at
                .cmp(&a.last_event_at)
                .then(a.session_id.cmp(&b.session_id))
        });
        let label = match subject {
            Subject::Project(None) => "project (all loaded sessions)".to_string(),
            Subject::Project(Some(name)) => format!("project '{name}'"),
            _ => sessions
                .first()
                .map(|s| format!("session {}", readable(&s.session_id)))
                .unwrap_or_else(|| "session".to_string()),
        };
        Ok(SessionSubject { label, sessions })
    }

    fn subject_attempt(&self, subject: &Subject) -> Result<&Attempt> {
        match subject {
            Subject::Attempt(id) => self.find_attempt(id),
            Subject::Id(id) => match split_prefix(id.trim()).0 {
                Some("att_") | None => self.find_attempt(id),
                Some(other) => Err(QueryError::plan(format!(
                    "expected an attempt (att_…) id, got a {other}… id"
                ))),
            },
            other => Err(QueryError::plan(format!(
                "expected an attempt id, got {}",
                subject_label(other)
            ))),
        }
    }

    /// Resolve a subject to a node of the causal graph.
    fn subject_endpoint(&self, subject: &Subject) -> Result<EdgeEndpoint> {
        let by_prefix = |prefix: Option<&str>, id: &str| -> Result<EdgeEndpoint> {
            match prefix {
                Some("att_") => Ok(EdgeEndpoint::Attempt(self.find_attempt(id)?.attempt_id)),
                Some("ses_") => Ok(EdgeEndpoint::Session(self.find_session(id)?.session_id)),
                Some("trn_") => Ok(EdgeEndpoint::Turn(self.find_turn(id)?.turn_id)),
                Some("spn_") => Ok(EdgeEndpoint::Span(self.find_tool_call(id)?.tool_call_id)),
                Some("ev_") => Ok(EdgeEndpoint::Event(self.resolve_event(id)?)),
                Some(other) => Err(QueryError::plan(format!(
                    "TRACE needs an attempt, session, turn, tool call or event id; got a {other}… id"
                ))),
                None => {
                    // Bare id: try each entity type in turn.
                    if let Ok(a) = self.find_attempt(id) {
                        return Ok(EdgeEndpoint::Attempt(a.attempt_id));
                    }
                    if let Ok(s) = self.find_session(id) {
                        return Ok(EdgeEndpoint::Session(s.session_id));
                    }
                    if let Ok(t) = self.find_turn(id) {
                        return Ok(EdgeEndpoint::Turn(t.turn_id));
                    }
                    if let Ok(c) = self.find_tool_call(id) {
                        return Ok(EdgeEndpoint::Span(c.tool_call_id));
                    }
                    if let Ok(e) = self.resolve_event(id) {
                        return Ok(EdgeEndpoint::Event(e));
                    }
                    Err(QueryError::not_found(format!(
                        "'{id}' does not match any loaded attempt, session, turn, tool call or event"
                    )))
                }
            }
        };
        match subject {
            Subject::Attempt(id) => Ok(EdgeEndpoint::Attempt(self.find_attempt(id)?.attempt_id)),
            Subject::Session(id) => Ok(EdgeEndpoint::Session(self.find_session(id)?.session_id)),
            Subject::Turn(id) => Ok(EdgeEndpoint::Turn(self.find_turn(id)?.turn_id)),
            Subject::Span(id) => Ok(EdgeEndpoint::Span(self.find_tool_call(id)?.tool_call_id)),
            Subject::Event(id) => Ok(EdgeEndpoint::Event(self.resolve_event(id)?)),
            Subject::Id(id) => by_prefix(split_prefix(id.trim()).0, id),
            other => Err(QueryError::plan(format!(
                "TRACE needs an attempt, session, turn, tool call or event id; got {}",
                subject_label(other)
            ))),
        }
    }

    /// `(label, evidence event ids)` for `SHOW EVIDENCE FOR`.
    fn evidence_ids(&self, subject: &Subject) -> Result<(String, Vec<EventId>)> {
        let calls: HashMap<SpanId, &ToolCall> = self
            .projection
            .tool_calls
            .iter()
            .map(|c| (c.tool_call_id, c))
            .collect();
        let endpoint = self.subject_endpoint(subject).map_err(|e| match e {
            QueryError::Plan(m) => {
                QueryError::Plan(m.replace("TRACE needs", "SHOW EVIDENCE FOR needs"))
            }
            other => other,
        })?;
        Ok(match endpoint {
            EdgeEndpoint::Attempt(id) => {
                let a = self.projection.attempts.iter().find(|a| a.attempt_id == id);
                (
                    format!("attempt {}", readable(&id)),
                    a.map(|a| a.evidence.clone()).unwrap_or_default(),
                )
            }
            EdgeEndpoint::Turn(id) => {
                let t = self.projection.turns.iter().find(|t| t.turn_id == id);
                (
                    format!("turn {}", readable(&id)),
                    t.map(|t| turn_evidence(t, &calls)).unwrap_or_default(),
                )
            }
            EdgeEndpoint::Session(id) => (
                format!("session {}", readable(&id)),
                self.session_event_ids(id).to_vec(),
            ),
            EdgeEndpoint::Span(id) => {
                let c = calls.get(&id);
                let ids = c
                    .map(|c| c.start_event_id.into_iter().chain(c.end_event_id).collect())
                    .unwrap_or_default();
                (format!("tool call {}", readable(&id)), ids)
            }
            EdgeEndpoint::Event(id) => (format!("event {}", readable(&id)), vec![id]),
        })
    }

    // --- EXPLAIN ---------------------------------------------------------------

    async fn explain_statement(&self, stmt: Statement) -> Result<QueryResult> {
        match &stmt {
            Statement::Show(s) if !matches!(s.target, ShowTarget::Decisions | ShowTarget::Evidence(_)) => {
                let compiled = self.compile_show(s)?;
                let mut r = self.explain(&compiled.sql).await?;
                r.notes.push(format!("compiled SQL: {}", compiled.sql));
                r.notes.extend(compiled.notes);
                Ok(r)
            }
            Statement::Show(s) => {
                if let ShowTarget::Evidence(subject) = &s.target {
                    let (label, ids) = self.evidence_ids(subject)?;
                    let sql = self.evidence_sql(&ids);
                    let mut r = self.explain(&sql).await?;
                    r.notes.push(format!("evidence for {label}: {} event id(s)", ids.len()));
                    r.notes.push(format!("compiled SQL: {sql}"));
                    return Ok(r);
                }
                self.plan_rows(vec![
                    ("statement", "SHOW DECISIONS".to_string()),
                    ("plan", "decisions are not projected yet (planned, RFC 0003); always empty".to_string()),
                ])
            }
            Statement::Why(w) => self.plan_rows(vec![
                ("statement", format!("WHY {} STATUS {}", subject_label(&w.subject), w.state)),
                ("subject", self.describe_subject(&w.subject)),
                (
                    "algorithm",
                    format!(
                        "{ALGORITHM_VERSION}: blocked = uncleared pending-input signal, or the last two attempts failed with the same class; failed = attempt outcome in (failed, superseded)"
                    ),
                ),
                ("tables", "projection (sessions, attempts, signals); no SQL plan".to_string()),
            ]),
            Statement::Trace(t) => self.plan_rows(vec![
                ("statement", format!("TRACE {} CAUSES", subject_label(&t.subject))),
                ("subject", self.describe_subject(&t.subject)),
                (
                    "plan",
                    format!(
                        "breadth-first walk over the edges table ({} edges: projection + derived) direction {} depth <= {}; evidence_for edges skipped",
                        self.graph().edge_count(),
                        t.direction.as_str(),
                        t.depth.unwrap_or(DEFAULT_TRACE_DEPTH)
                    ),
                ),
            ]),
            Statement::State(s) => self.plan_rows(vec![
                ("statement", format!("STATE {} AT {}", subject_label(&s.subject), s.at.describe())),
                ("at", s.at.resolve(Timestamp::now()).to_rfc3339()),
                ("plan", format!("Projection::state_at over {} session(s) ({ALGORITHM_VERSION})", self.projection.sessions.len())),
            ]),
            Statement::Diff(d) => self.plan_rows(vec![
                ("statement", format!("DIFF STATE {} {}", d.from.describe(), d.to.describe())),
                ("plan", "two Projection::state_at snapshots compared per session and field".to_string()),
            ]),
            Statement::WhatIs(w) => self.plan_rows(vec![
                ("statement", format!("WHAT IS {} DOING NOW", subject_label(&w.subject))),
                ("plan", "STATE <subject> AT now plus sessions with events in the last 15 minutes".to_string()),
            ]),
            Statement::Explain(inner) => Box::pin(self.explain_statement((**inner).clone())).await,
        }
    }

    fn plan_rows(&self, rows: Vec<(&str, String)>) -> Result<QueryResult> {
        let mut b = TableBuilder::new(&[
            ("plan_type", Kind::Utf8, false),
            ("plan", Kind::Utf8, false),
        ]);
        for (k, v) in rows {
            b.push(vec![k.into(), v.into()])?;
        }
        let batch = b.finish()?;
        Ok(QueryResult::new(
            batch.schema(),
            vec![batch],
            ResultKind::Explanation,
            Vec::new(),
        ))
    }

    fn describe_subject(&self, subject: &Subject) -> String {
        match subject {
            Subject::Project(None) => {
                format!("all {} loaded session(s)", self.projection.sessions.len())
            }
            other => subject_label(other),
        }
    }

    // --- SHOW ------------------------------------------------------------------

    async fn exec_show(&self, s: &ShowStatement) -> Result<QueryResult> {
        match &s.target {
            ShowTarget::Decisions => {
                let b = TableBuilder::new(&[
                    ("decision_id", Kind::Utf8, false),
                    ("selected", Kind::Utf8, true),
                    ("outcome", Kind::Utf8, true),
                    ("made_by", Kind::Utf8, true),
                    ("decided_at", Kind::Ts, true),
                    ("evidence", Kind::ListUtf8, false),
                    ("confidence", Kind::Float32, false),
                ]);
                Ok(QueryResult::empty(
                    b.schema(),
                    "decisions are not projected yet (planned, RFC 0003)",
                ))
            }
            ShowTarget::Evidence(subject) => self.exec_evidence(subject).await,
            _ => {
                let compiled = self.compile_show(s)?;
                let mut r = self.sql(&compiled.sql).await?;
                r.notes.extend(compiled.notes);
                let n = r.row_count();
                if n == 0 {
                    r.kind = ResultKind::Empty;
                    r.notes.push(format!("no {} matched", s.target.name()));
                } else if n >= compiled.limit {
                    r.notes.push(format!(
                        "showing the first {n} rows (LIMIT {}); add LIMIT n for more",
                        compiled.limit
                    ));
                }
                Ok(r)
            }
        }
    }

    fn compile_show(&self, s: &ShowStatement) -> Result<Compiled> {
        let (table, mut conds): (&str, Vec<String>) = match &s.target {
            ShowTarget::Attempts => ("attempts", Vec::new()),
            ShowTarget::FailedAttempts => (
                "attempts",
                vec!["outcome IN ('failed', 'superseded')".into()],
            ),
            ShowTarget::SupersededAttempts => {
                ("attempts", vec!["superseded_by IS NOT NULL".into()])
            }
            ShowTarget::Sessions => ("sessions", Vec::new()),
            ShowTarget::Turns => ("turns", Vec::new()),
            ShowTarget::ToolCalls => ("tool_calls", Vec::new()),
            ShowTarget::Handoffs { between } => {
                let mut conds = Vec::new();
                if let Some((a, b)) = between {
                    let (a, b) = (normalize_provider(a), normalize_provider(b));
                    if a == b {
                        return Err(QueryError::plan(format!(
                            "BETWEEN needs two different agents (both are '{a}')"
                        )));
                    }
                    conds.push(format!(
                        "((from_provider = {a} AND to_provider = {b}) OR (from_provider = {b} AND to_provider = {a}))",
                        a = lit(&a),
                        b = lit(&b)
                    ));
                }
                ("handoffs", conds)
            }
            ShowTarget::Edges => ("edges", Vec::new()),
            ShowTarget::Signals => ("signals", Vec::new()),
            ShowTarget::Decisions | ShowTarget::Evidence(_) => {
                return Err(QueryError::plan("internal: target does not compile to SQL"));
            }
        };
        let mut notes = Vec::new();
        for f in &s.filters {
            conds.push(self.filter_sql(table, f)?);
        }
        if let Some(p) = &s.predicate {
            conds.push(format!("({p})"));
        }
        let now = Timestamp::now();
        if let Some(t) = &s.since {
            conds.push(self.time_cond(table, ">=", t.resolve(now))?);
        }
        if let Some(t) = &s.until {
            conds.push(self.time_cond(table, "<=", t.resolve(now))?);
        }
        let order = match &s.order_by {
            Some(o) => format!("{} {}", o.column, if o.descending { "DESC" } else { "ASC" }),
            None => default_order(table).to_string(),
        };
        let limit = s.limit.unwrap_or(DEFAULT_LIMIT);
        let where_clause = if conds.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conds.join(" AND "))
        };
        let sql = format!("SELECT * FROM {table}{where_clause} ORDER BY {order} LIMIT {limit}");
        notes.push(format!(
            "{table} are Tier 1 projections ({ALGORITHM_VERSION}) over {} event(s); confidence and evidence columns carry the uncertainty",
            self.event_count
        ));
        if table == "attempts"
            && !self.projection.attempts.is_empty()
            && self
                .projection
                .attempts
                .iter()
                .all(|a| a.objective.is_none())
        {
            notes.push(
                "objective text is unavailable (no prompt content captured; metadata-only mode?)"
                    .into(),
            );
        }
        Ok(Compiled { sql, limit, notes })
    }

    fn time_cond(&self, table: &str, op: &str, t: Timestamp) -> Result<String> {
        let col = time_column(table).ok_or_else(|| {
            QueryError::plan(format!(
                "{table} has no time column; SINCE/UNTIL cannot be applied"
            ))
        })?;
        Ok(format!("{col} {op} {}", ts_lit(t)))
    }

    fn filter_sql(&self, table: &str, f: &Filter) -> Result<String> {
        let unsupported =
            |what: &str| QueryError::plan(format!("filter '{what}' is not supported for {table}"));
        Ok(match f {
            Filter::Project(v) => {
                let ids = self.project_ids_for(v)?;
                format!(
                    "project_id IN ({})",
                    ids.iter()
                        .map(|id| lit(&readable(id)))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Filter::Session(v) => {
                let id = lit(&readable(&resolve::<SessionId>(
                    v,
                    &self.session_ids(),
                    "session",
                    false,
                )?));
                match table {
                    "handoffs" => format!("(from_session = {id} OR to_session = {id})"),
                    "edges" => format!("(from_id = {id} OR to_id = {id})"),
                    _ => format!("session_id = {id}"),
                }
            }
            Filter::Provider(v) => self.provider_sql(table, v)?,
            Filter::Agent(v) => {
                let (prefix, _) = split_prefix(v.trim());
                if prefix == Some("agt_") {
                    let mut ids: Vec<AgentId> = self
                        .projection
                        .sessions
                        .iter()
                        .flat_map(|s| s.agents.iter().copied())
                        .collect();
                    ids.sort();
                    ids.dedup();
                    let id = lit(&readable(&resolve::<AgentId>(v, &ids, "agent", false)?));
                    match table {
                        "sessions" => format!("array_has(agents, {id})"),
                        "tool_calls" => format!("agent_id = {id}"),
                        _ => {
                            return Err(QueryError::plan(format!(
                                "agent ids (agt_…) can filter sessions and tool_calls only; use a provider id for {table}"
                            )));
                        }
                    }
                } else {
                    self.provider_sql(table, v)?
                }
            }
            Filter::Turn(v) => {
                let ids: Vec<TurnId> = self.projection.turns.iter().map(|t| t.turn_id).collect();
                let id = lit(&readable(&resolve::<TurnId>(v, &ids, "turn", false)?));
                match table {
                    "turns" | "tool_calls" | "attempts" => format!("turn_id = {id}"),
                    "edges" => format!("(from_id = {id} OR to_id = {id})"),
                    _ => return Err(unsupported("turn")),
                }
            }
            Filter::Path(v) => {
                let col = match table {
                    "attempts" | "tool_calls" => "paths",
                    "handoffs" => "shared_paths",
                    _ => return Err(unsupported("path")),
                };
                if v.contains(['*', '%']) {
                    format!(
                        "array_to_string({col}, '\n') LIKE {}",
                        lit(&v.replace('*', "%"))
                    )
                } else {
                    format!("array_has({col}, {})", lit(v))
                }
            }
            Filter::Outcome(v) | Filter::Status(v) => {
                let col = match table {
                    "attempts" => "outcome",
                    "tool_calls" => "outcome_status",
                    "turns" => "status",
                    "sessions" => "state",
                    _ => return Err(unsupported(f.key())),
                };
                format!("{col} = {}", lit(&v.to_ascii_lowercase()))
            }
            Filter::Tool(v) => match table {
                "tool_calls" => format!("tool_name = {}", lit(v)),
                _ => return Err(unsupported("tool")),
            },
            Filter::Since(t) => self.time_cond(table, ">=", t.resolve(Timestamp::now()))?,
            Filter::Until(t) => self.time_cond(table, "<=", t.resolve(Timestamp::now()))?,
        })
    }

    fn provider_sql(&self, table: &str, v: &str) -> Result<String> {
        let p = lit(&normalize_provider(v));
        Ok(match table {
            "handoffs" => format!("(from_provider = {p} OR to_provider = {p})"),
            "edges" | "signals" => {
                return Err(QueryError::plan(format!(
                    "filter 'provider' is not supported for {table}"
                )));
            }
            _ => format!("provider = {p}"),
        })
    }

    fn evidence_sql(&self, ids: &[EventId]) -> String {
        let list = ids
            .iter()
            .map(|id| lit(&readable(id)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "SELECT observed_at, kind, tool_name, path_relative, outcome_status, outcome_class, event_id, session_id FROM events WHERE event_id IN ({list}) ORDER BY observed_at, source_seq"
        )
    }

    async fn exec_evidence(&self, subject: &Subject) -> Result<QueryResult> {
        let (label, ids) = self.evidence_ids(subject)?;
        if ids.is_empty() {
            let b = TableBuilder::new(&[
                ("observed_at", Kind::Ts, false),
                ("kind", Kind::Utf8, false),
                ("tool_name", Kind::Utf8, true),
                ("path_relative", Kind::Utf8, true),
                ("outcome_status", Kind::Utf8, true),
                ("outcome_class", Kind::Utf8, true),
                ("event_id", Kind::Utf8, false),
                ("session_id", Kind::Utf8, false),
            ]);
            return Ok(QueryResult::empty(
                b.schema(),
                format!("no evidence recorded for {label}"),
            ));
        }
        let mut r = self.sql(&self.evidence_sql(&ids)).await?;
        let loaded = r.row_count();
        r.kind = if loaded == 0 {
            ResultKind::Empty
        } else {
            ResultKind::Rows
        };
        r.notes.push(format!(
            "{} for {label}; {loaded} loaded",
            plural(ids.len(), "evidence event")
        ));
        let missing = ids.iter().filter(|id| !self.has_event(id)).count();
        if missing > 0 {
            r.notes.push(format!(
                "{} not among the loaded events (filtered scan?)",
                plural(missing, "evidence id")
            ));
        }
        Ok(r)
    }

    // --- WHY -------------------------------------------------------------------

    fn exec_why(&self, w: &WhyStatement) -> Result<QueryResult> {
        match w.state.as_str() {
            "BLOCKED" => self.why_blocked(&w.subject),
            "FAILED" => self.why_failed(&w.subject),
            other => Err(QueryError::plan(format!(
                "unsupported state '{other}'; supported: BLOCKED (for a session or project) and FAILED (for an attempt)"
            ))),
        }
    }

    fn why_blocked(&self, subject: &Subject) -> Result<QueryResult> {
        let SessionSubject { label, sessions } = self.subject_sessions(subject)?;
        let mut b = TableBuilder::new(&[
            ("session_id", Kind::Utf8, false),
            ("provider", Kind::Utf8, false),
            ("project_name", Kind::Utf8, false),
            ("claim", Kind::Utf8, false),
            ("confidence", Kind::Float32, false),
            ("uncertainty", Kind::Utf8, false),
            ("evidence", Kind::ListUtf8, false),
            ("last_event_at", Kind::Ts, false),
        ]);
        for s in &sessions {
            if let Some(e) = self.projection.why_blocked(s.session_id) {
                b.push(vec![
                    readable(&s.session_id).into(),
                    s.provider.as_str().into(),
                    s.project_name.clone().into(),
                    e.claim.into(),
                    e.confidence.into(),
                    e.uncertainty.into(),
                    readable_list(&e.evidence).into(),
                    s.last_event_at.into(),
                ])?;
            }
        }
        let examined = sessions.len();
        if b.is_empty() {
            let mut r = QueryResult::empty(
                b.schema(),
                format!(
                    "no blocked session found (evidence: {} examined)",
                    plural(examined, "session")
                ),
            );
            if let [s] = sessions.as_slice() {
                let last = self.projection.attempts_of(s.session_id).last();
                r.notes.push(format!(
                    "session {} is {}; last event {} at {}; last attempt outcome: {}",
                    readable(&s.session_id),
                    session_state(s),
                    readable(&s.last_event_id),
                    s.last_event_at.to_rfc3339(),
                    last.map(|a| a.outcome.as_str()).unwrap_or("none")
                ));
            }
            return Ok(r);
        }
        let found = b.len();
        let batch = b.finish()?;
        Ok(QueryResult::new(
            batch.schema(),
            vec![batch],
            ResultKind::Explanation,
            vec![format!(
                "{} of {} examined for {label}; blocked = an uncleared pending-input signal, or the last two attempts failed the same way ({ALGORITHM_VERSION})",
                plural(found, "blocked session"),
                plural(examined, "session")
            )],
        ))
    }

    fn why_failed(&self, subject: &Subject) -> Result<QueryResult> {
        let a = self.subject_attempt(subject)?;
        let mut b = TableBuilder::new(&[
            ("attempt_id", Kind::Utf8, false),
            ("session_id", Kind::Utf8, false),
            ("provider", Kind::Utf8, false),
            ("turn_index", Kind::Int64, false),
            ("attempt_index", Kind::Int64, false),
            ("outcome", Kind::Utf8, false),
            ("failure_class", Kind::Utf8, true),
            ("approach", Kind::Utf8, false),
            ("paths", Kind::ListUtf8, false),
            ("superseded_by", Kind::Utf8, true),
            ("claim", Kind::Utf8, false),
            ("confidence", Kind::Float32, false),
            ("uncertainty", Kind::Utf8, false),
            ("evidence", Kind::ListUtf8, false),
        ]);
        let session = self.projection.session(a.session_id);
        let provider = session
            .map(|s| s.provider.as_str().to_string())
            .unwrap_or_else(|| "unknown".into());
        if !a.outcome.is_failure() {
            return Ok(QueryResult::empty(
                b.schema(),
                format!(
                    "attempt {} did not fail (outcome: {}; evidence: {})",
                    readable(&a.attempt_id),
                    a.outcome.as_str(),
                    plural(a.evidence.len(), "event")
                ),
            ));
        }
        let failing = self
            .graph()
            .edges
            .iter()
            .find(|e| {
                e.derived
                    && e.kind == attemptdb_project::EdgeKind::Caused
                    && e.to == EdgeEndpoint::Attempt(a.attempt_id)
            })
            .and_then(|e| match e.from {
                EdgeEndpoint::Event(id) => Some(id),
                _ => None,
            });
        let class_text = a
            .failure_class
            .as_deref()
            .map(|c| format!("failed with `{c}`"))
            .unwrap_or_else(|| "failed without a failure class".to_string());
        let after = match a.superseded_by {
            Some(next) => format!("; it was superseded by {}", readable(&next)),
            None => "; no later attempt retried the same paths".to_string(),
        };
        let failing_text = failing
            .map(|f| format!(" The failing event is {}.", readable(&f)))
            .unwrap_or_default();
        let claim = format!(
            "Attempt {} (turn {} #{}: {}) {}{}.{}",
            readable(&a.attempt_id),
            a.turn_index,
            a.index,
            a.approach,
            class_text,
            after,
            failing_text
        );
        let uncertainty = format!(
            "Attempt boundaries are Tier 1 heuristics ({ALGORITHM_VERSION}, confidence {}); the failure class is the provider's coarse classification and the error text was not inspected. {}",
            a.confidence,
            session.map(coverage_text).unwrap_or_default()
        );
        b.push(vec![
            readable(&a.attempt_id).into(),
            readable(&a.session_id).into(),
            provider.into(),
            a.turn_index.into(),
            a.index.into(),
            a.outcome.as_str().into(),
            a.failure_class.clone().into(),
            a.approach.clone().into(),
            a.paths.clone().into(),
            crate::ids::readable_opt(&a.superseded_by).into(),
            claim.into(),
            a.confidence.into(),
            uncertainty.into(),
            readable_list(&a.evidence).into(),
        ])?;
        let batch = b.finish()?;
        Ok(QueryResult::new(
            batch.schema(),
            vec![batch],
            ResultKind::Explanation,
            vec![format!(
                "{} for attempt {}",
                plural(a.evidence.len(), "evidence event"),
                readable(&a.attempt_id)
            )],
        ))
    }

    // --- TRACE -----------------------------------------------------------------

    fn exec_trace(&self, t: &TraceStatement) -> Result<QueryResult> {
        let start = self.subject_endpoint(&t.subject)?;
        let depth = t.depth.unwrap_or(DEFAULT_TRACE_DEPTH);
        let (steps, truncated) = self.graph().trace(start, depth, t.direction);
        let mut b = TableBuilder::new(&[
            ("depth", Kind::Int64, false),
            ("edge_kind", Kind::Utf8, false),
            ("from_type", Kind::Utf8, false),
            ("from_id", Kind::Utf8, false),
            ("to_type", Kind::Utf8, false),
            ("to_id", Kind::Utf8, false),
            ("evidence", Kind::ListUtf8, false),
            ("confidence", Kind::Float32, false),
            ("uncertainty", Kind::Utf8, false),
            ("edge_source", Kind::Utf8, false),
        ]);
        for step in &steps {
            let e = &self.graph().edges[step.edge];
            let mut uncertainty = if e.confidence >= 1.0 {
                "deterministic".to_string()
            } else {
                format!(
                    "heuristic ({ALGORITHM_VERSION}, confidence {})",
                    e.confidence
                )
            };
            if e.derived {
                uncertainty.push_str("; derived by the query layer from projection evidence");
            }
            b.push(vec![
                (step.depth as u64).into(),
                e.kind.as_str().into(),
                endpoint_type(&e.from).into(),
                endpoint_id(&e.from).into(),
                endpoint_type(&e.to).into(),
                endpoint_id(&e.to).into(),
                readable_list(&e.evidence).into(),
                e.confidence.into(),
                uncertainty.into(),
                if e.derived { "derived" } else { "projection" }.into(),
            ])?;
        }
        let subject_text = format!("{} {}", endpoint_type(&start), endpoint_id(&start));
        let mut notes = vec![format!(
            "trace from {subject_text} (direction {}, depth <= {depth}): {} reached of {} in the graph",
            t.direction.as_str(),
            plural(steps.len(), "edge"),
            self.graph().edge_count()
        )];
        if truncated {
            notes.push(format!(
                "depth limit {depth} reached; add DEPTH n to go further"
            ));
        }
        if steps.is_empty() {
            let mut r = QueryResult::empty(
                b.schema(),
                format!(
                    "no causal edges reach {subject_text} (evidence: {} examined)",
                    plural(self.graph().edge_count(), "edge")
                ),
            );
            r.notes.extend(notes);
            return Ok(r);
        }
        let batch = b.finish()?;
        Ok(QueryResult::new(
            batch.schema(),
            vec![batch],
            ResultKind::Explanation,
            notes,
        ))
    }

    // --- STATE / DIFF / WHAT IS -------------------------------------------------

    /// Snapshot rows for the subject at `at`, most recently active first.
    fn snapshot(&self, subject: &Subject, at: Timestamp) -> Result<(String, Vec<SessionState>)> {
        let snap = self.projection.state_at(at);
        let (label, keep): (String, KeepSession) = match subject {
            Subject::Project(None) => (
                "project (all loaded sessions)".to_string(),
                Box::new(|_| true),
            ),
            Subject::Project(Some(name)) => {
                let ids = self.project_ids_for(name)?;
                (
                    format!("project '{name}'"),
                    Box::new(move |s| ids.contains(&s.project_id)),
                )
            }
            Subject::Session(id) => {
                let sid = self.find_session(id)?.session_id;
                (
                    format!("session {}", readable(&sid)),
                    Box::new(move |s| s.session_id == sid),
                )
            }
            Subject::Id(id) => match split_prefix(id.trim()).0 {
                Some("ses_") | None => {
                    let sid = self.find_session(id)?.session_id;
                    (
                        format!("session {}", readable(&sid)),
                        Box::new(move |s| s.session_id == sid),
                    )
                }
                Some(other) => {
                    return Err(QueryError::plan(format!(
                        "STATE needs a project or session subject, got a {other}… id"
                    )));
                }
            },
            other => {
                return Err(QueryError::plan(format!(
                    "STATE needs a project or session subject, got {}",
                    subject_label(other)
                )));
            }
        };
        let mut states: Vec<SessionState> = snap.sessions.into_iter().filter(|s| keep(s)).collect();
        states.sort_by(|a, b| {
            b.last_activity_at
                .cmp(&a.last_activity_at)
                .then(a.session_id.cmp(&b.session_id))
        });
        Ok((label, states))
    }

    fn state_row_confidence(&self, st: &SessionState) -> (f32, String) {
        if let Some(b) = &st.block {
            return (b.confidence, b.uncertainty.clone());
        }
        let text = self
            .projection
            .session(st.session_id)
            .map(coverage_text)
            .unwrap_or_else(|| "session not found in projection".to_string());
        (coverage_confidence(st.coverage), text)
    }

    fn state_result(&self, subject: &Subject, at_expr: &TimeExpr) -> Result<QueryResult> {
        let at = at_expr.resolve(Timestamp::now());
        let (label, states) = self.snapshot(subject, at)?;
        let mut b = TableBuilder::new(&[
            ("session_id", Kind::Utf8, false),
            ("provider", Kind::Utf8, false),
            ("project_id", Kind::Utf8, false),
            ("project_name", Kind::Utf8, false),
            ("is_open", Kind::Bool, false),
            ("coverage", Kind::Utf8, false),
            ("current_turn", Kind::Utf8, true),
            ("turn_index", Kind::Int64, true),
            ("turn_status", Kind::Utf8, true),
            ("in_flight_tool_calls", Kind::Int64, false),
            ("in_flight_tool_call_ids", Kind::ListUtf8, false),
            ("last_attempt", Kind::Utf8, true),
            ("last_attempt_outcome", Kind::Utf8, true),
            ("last_failure_class", Kind::Utf8, true),
            ("last_activity_at", Kind::Ts, false),
            ("blocked", Kind::Bool, false),
            ("block_claim", Kind::Utf8, true),
            ("confidence", Kind::Float32, false),
            ("uncertainty", Kind::Utf8, false),
            ("evidence", Kind::ListUtf8, false),
        ]);
        for st in &states {
            let (confidence, uncertainty) = self.state_row_confidence(st);
            let project_name = self
                .projection
                .session(st.session_id)
                .map(|s| s.project_name.clone())
                .unwrap_or_default();
            b.push(vec![
                readable(&st.session_id).into(),
                st.provider.as_str().into(),
                readable(&st.project_id).into(),
                project_name.into(),
                st.open.into(),
                st.coverage.as_str().into(),
                crate::ids::readable_opt(&st.current_turn).into(),
                st.turn_index.into(),
                st.turn_status.map(|s| s.as_str().to_string()).into(),
                (st.in_flight_tool_calls.len() as u64).into(),
                readable_list(&st.in_flight_tool_calls).into(),
                crate::ids::readable_opt(&st.last_attempt).into(),
                st.last_attempt_outcome
                    .map(|o| o.as_str().to_string())
                    .into(),
                st.last_failure_class.clone().into(),
                st.last_activity_at.into(),
                st.blocked.into(),
                st.block.as_ref().map(|b| b.claim.clone()).into(),
                confidence.into(),
                uncertainty.into(),
                readable_list(&st.evidence).into(),
            ])?;
        }
        let total = self.projection.sessions.len();
        if b.is_empty() {
            let mut r = QueryResult::empty(
                b.schema(),
                format!(
                    "no session active at {} (evidence: {} examined)",
                    at.to_rfc3339(),
                    plural(total, "session")
                ),
            );
            if let Subject::Session(id) | Subject::Id(id) = subject
                && let Ok(s) = self.find_session(id)
            {
                r.notes.push(format!(
                    "session {} started at {} and {}",
                    readable(&s.session_id),
                    s.started_at.to_rfc3339(),
                    s.ended_at
                        .map(|e| format!("ended at {}", e.to_rfc3339()))
                        .unwrap_or_else(|| "has not ended".into())
                ));
            }
            return Ok(r);
        }
        let n = b.len();
        let batch = b.finish()?;
        Ok(QueryResult::new(
            batch.schema(),
            vec![batch],
            ResultKind::Explanation,
            vec![format!(
                "state of {label} as of {} ({} active of {}; {ALGORITHM_VERSION})",
                at.to_rfc3339(),
                plural(n, "session"),
                plural(total, "session")
            )],
        ))
    }

    fn exec_state(&self, s: &StateStatement) -> Result<QueryResult> {
        self.state_result(&s.subject, &s.at)
    }

    fn exec_diff(&self, d: &DiffStatement) -> Result<QueryResult> {
        let now = Timestamp::now();
        let (from, to) = (d.from.resolve(now), d.to.resolve(now));
        if from >= to {
            return Err(QueryError::plan(format!(
                "DIFF STATE needs the earlier timestamp first (got {} then {})",
                from.to_rfc3339(),
                to.to_rfc3339()
            )));
        }
        let subject = d.subject.clone().unwrap_or(Subject::Project(None));
        let (label, before) = self.snapshot(&subject, from)?;
        let (_, after) = self.snapshot(&subject, to)?;
        let before: BTreeMap<SessionId, SessionState> =
            before.into_iter().map(|s| (s.session_id, s)).collect();
        let after: BTreeMap<SessionId, SessionState> =
            after.into_iter().map(|s| (s.session_id, s)).collect();
        let mut ids: Vec<SessionId> = before.keys().chain(after.keys()).copied().collect();
        ids.sort();
        ids.dedup();
        // Order by session start so the diff reads chronologically.
        ids.sort_by_key(|id| {
            self.projection
                .session(*id)
                .map(|s| s.started_at)
                .unwrap_or_default()
        });

        let mut b = TableBuilder::new(&[
            ("session_id", Kind::Utf8, false),
            ("provider", Kind::Utf8, false),
            ("change", Kind::Utf8, false),
            ("field", Kind::Utf8, false),
            ("before", Kind::Utf8, true),
            ("after", Kind::Utf8, true),
            ("confidence", Kind::Float32, false),
            ("uncertainty", Kind::Utf8, false),
            ("evidence", Kind::ListUtf8, false),
        ]);
        let mut sessions_changed = 0usize;
        for id in ids {
            let sid = readable(&id);
            let provider = self
                .projection
                .session(id)
                .map(|s| s.provider.as_str().to_string())
                .unwrap_or_else(|| "unknown".into());
            match (before.get(&id), after.get(&id)) {
                (None, Some(s)) => {
                    let (c, u) = self.state_row_confidence(s);
                    sessions_changed += 1;
                    b.push(vec![
                        sid.into(),
                        provider.into(),
                        "added".into(),
                        "presence".into(),
                        Val::Null,
                        summarize_state(s).into(),
                        c.into(),
                        u.into(),
                        readable_list(&s.evidence).into(),
                    ])?;
                }
                (Some(s), None) => {
                    let (c, u) = self.state_row_confidence(s);
                    sessions_changed += 1;
                    b.push(vec![
                        sid.into(),
                        provider.into(),
                        "removed".into(),
                        "presence".into(),
                        summarize_state(s).into(),
                        Val::Null,
                        c.into(),
                        u.into(),
                        readable_list(&s.evidence).into(),
                    ])?;
                }
                (Some(x), Some(y)) => {
                    let (cx, ux) = self.state_row_confidence(x);
                    let (cy, uy) = self.state_row_confidence(y);
                    let confidence = cx.min(cy);
                    let uncertainty = if ux == uy {
                        uy
                    } else {
                        format!("{ux} After: {uy}")
                    };
                    let mut evidence: Vec<EventId> = x.evidence.clone();
                    for e in &y.evidence {
                        if !evidence.contains(e) {
                            evidence.push(*e);
                        }
                    }
                    let mut any = false;
                    for (field, bv, av) in state_fields(x, y) {
                        if bv != av {
                            any = true;
                            b.push(vec![
                                sid.clone().into(),
                                provider.clone().into(),
                                "changed".into(),
                                field.into(),
                                bv.into(),
                                av.into(),
                                confidence.into(),
                                uncertainty.clone().into(),
                                readable_list(&evidence).into(),
                            ])?;
                        }
                    }
                    if any {
                        sessions_changed += 1;
                    }
                }
                (None, None) => {}
            }
        }
        let n = b.len();
        if n == 0 {
            return Ok(QueryResult::empty(
                b.schema(),
                format!(
                    "no state change for {label} between {} and {} (evidence: {} examined)",
                    from.to_rfc3339(),
                    to.to_rfc3339(),
                    plural(before.len().max(after.len()), "session")
                ),
            ));
        }
        let batch = b.finish()?;
        Ok(QueryResult::new(
            batch.schema(),
            vec![batch],
            ResultKind::Explanation,
            vec![format!(
                "diff of {label} between {} and {}: {} across {} ({ALGORITHM_VERSION})",
                from.to_rfc3339(),
                to.to_rfc3339(),
                plural(n, "change"),
                plural(sessions_changed, "session")
            )],
        ))
    }

    fn exec_what_is(&self, w: &WhatIsStatement) -> Result<QueryResult> {
        let now = Timestamp::now();
        let mut r = self.state_result(&w.subject, &TimeExpr::Now)?;
        let cutoff = Timestamp::from_micros(now.as_micros().saturating_sub(RECENT_WINDOW_MICROS));
        let mut recent: Vec<&Session> = self
            .projection
            .sessions
            .iter()
            .filter(|s| s.last_event_at >= cutoff)
            .collect();
        recent.sort_by(|a, b| b.last_event_at.cmp(&a.last_event_at));
        if recent.is_empty() {
            match self.projection.sessions.iter().max_by_key(|s| s.last_event_at) {
                Some(s) => r.notes.push(format!(
                    "no session had events in the last 15 minutes; most recent event {} at {} (session {}, {})",
                    readable(&s.last_event_id),
                    s.last_event_at.to_rfc3339(),
                    readable(&s.session_id),
                    s.provider.as_str()
                )),
                None => r.notes.push("no events loaded".to_string()),
            }
        } else {
            let list = recent
                .iter()
                .map(|s| {
                    format!(
                        "{} ({}, last event at {})",
                        readable(&s.session_id),
                        s.provider.as_str(),
                        s.last_event_at.to_rfc3339()
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            r.notes.push(format!(
                "sessions with events in the last 15 minutes: {list}"
            ));
        }
        Ok(r)
    }
}

fn subject_label(s: &Subject) -> String {
    match s {
        Subject::Project(None) => "project".to_string(),
        Subject::Project(Some(n)) => format!("project '{n}'"),
        Subject::Session(id) => format!("session '{id}'"),
        Subject::Attempt(id) => format!("attempt '{id}'"),
        Subject::Turn(id) => format!("turn '{id}'"),
        Subject::Span(id) => format!("tool call '{id}'"),
        Subject::Event(id) => format!("event '{id}'"),
        Subject::Agent(id) => format!("agent '{id}'"),
        Subject::Id(id) => id.clone(),
    }
}

fn summarize_state(s: &SessionState) -> String {
    format!(
        "{}; turn {}; last attempt {}{}",
        if s.open { "open" } else { "closed" },
        s.turn_index
            .map(|i| i.to_string())
            .unwrap_or_else(|| "none".into()),
        s.last_attempt_outcome
            .map(|o| o.as_str().to_string())
            .unwrap_or_else(|| "none".into()),
        if s.blocked { "; blocked" } else { "" }
    )
}

/// `(field, before, after)` for every compared field of a session state.
fn state_fields(
    x: &SessionState,
    y: &SessionState,
) -> Vec<(&'static str, Option<String>, Option<String>)> {
    let opt_id = |t: &Option<TurnId>| t.as_ref().map(readable);
    let opt_att = |a: &Option<AttemptId>| a.as_ref().map(readable);
    let outcome = |o: &Option<AttemptOutcome>| o.map(|o| o.as_str().to_string());
    vec![
        (
            "is_open",
            Some(x.open.to_string()),
            Some(y.open.to_string()),
        ),
        (
            "current_turn",
            opt_id(&x.current_turn),
            opt_id(&y.current_turn),
        ),
        (
            "turn_status",
            x.turn_status.map(|s| s.as_str().to_string()),
            y.turn_status.map(|s| s.as_str().to_string()),
        ),
        (
            "in_flight_tool_calls",
            Some(x.in_flight_tool_calls.len().to_string()),
            Some(y.in_flight_tool_calls.len().to_string()),
        ),
        (
            "last_attempt",
            opt_att(&x.last_attempt),
            opt_att(&y.last_attempt),
        ),
        (
            "last_attempt_outcome",
            outcome(&x.last_attempt_outcome),
            outcome(&y.last_attempt_outcome),
        ),
        (
            "last_failure_class",
            x.last_failure_class.clone(),
            y.last_failure_class.clone(),
        ),
        (
            "blocked",
            Some(x.blocked.to_string()),
            Some(y.blocked.to_string()),
        ),
    ]
}

fn time_column(table: &str) -> Option<&'static str> {
    match table {
        "attempts" | "sessions" | "turns" | "tool_calls" => Some("started_at"),
        "handoffs" => Some("handoff_at"),
        "signals" => Some("raised_at"),
        "events" => Some("observed_at"),
        _ => None,
    }
}

fn default_order(table: &str) -> &'static str {
    match table {
        "attempts" => "started_at DESC, turn_index DESC, attempt_index DESC",
        "sessions" => "started_at DESC, session_id",
        "turns" => "started_at DESC, session_id, turn_index DESC",
        "tool_calls" => "started_at DESC NULLS LAST, tool_call_id",
        "handoffs" => "handoff_at DESC, to_session",
        "signals" => "raised_at DESC, event_id",
        "edges" => "ordinal",
        _ => "1",
    }
}

impl TableInfo {
    /// Whether this table has the named column.
    pub fn has_column(&self, name: &str) -> bool {
        self.columns.iter().any(|(c, _)| c == name)
    }
}
