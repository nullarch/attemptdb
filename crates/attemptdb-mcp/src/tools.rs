//! The MCP tool catalogue and the implementations behind `tools/call`.
//!
//! Every tool returns compact text that cites ids; `attempt_timeline` and
//! `attempt_status` add a JSON mirror as a second content block. Tool
//! failures (bad arguments, unknown ids, query errors) come back as
//! `isError: true` results with a message the caller can act on.

use crate::args::{opt_bool, opt_string, opt_usize, req_string};
use crate::brief;
use crate::protocol::{json_block, text_block, tool_error, tool_ok};
use crate::store::{Ready, ScopeArgs, Store, parse_time};
use crate::text::{
    cell_text, clip, conf, duration, id, id_opt, id_vec, ids, plural, result_text, span, ts,
};
use anyhow::{Result, anyhow, bail};
use attemptdb_capture::daemon::{self, Probe};
use attemptdb_core::{CaptureMode, OutcomeStatus, SpanId, Timestamp};
use attemptdb_project::{
    Attempt, AttemptOutcome, Handoff, Projection, Session, ToolCall, Turn, TurnStatus,
};
use attemptdb_query::{QueryError, QueryResult, format_parse_error};
use serde_json::{Map, Value, json};
use std::fmt::Write as _;

/// Every tool this server exposes, in `tools/list` order.
pub const TOOL_NAMES: &[&str] = &[
    "attempt_status",
    "attempt_timeline",
    "attempt_failures",
    "attempt_why",
    "attempt_trace",
    "attempt_state_at",
    "attempt_evidence",
    "attempt_query",
    "attempt_handoff_brief",
];

const DEFAULT_TIMELINE_SESSIONS: usize = 10;
const DEFAULT_FAILURES: usize = 50;

// ---------------------------------------------------------------------------
// Catalogue
// ---------------------------------------------------------------------------

fn prop_string(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}

fn prop_bool(desc: &str) -> Value {
    json!({ "type": "boolean", "description": desc })
}

fn prop_int(desc: &str, min: u64) -> Value {
    json!({ "type": "integer", "minimum": min, "description": desc })
}

fn prop_enum(desc: &str, values: &[&str]) -> Value {
    json!({ "type": "string", "enum": values, "description": desc })
}

fn schema(props: Vec<(&str, Value)>, required: &[&str]) -> Value {
    let mut map = Map::new();
    for (k, v) in props {
        map.insert(k.to_string(), v);
    }
    let mut s = json!({
        "type": "object",
        "properties": Value::Object(map),
        "additionalProperties": false
    });
    if !required.is_empty() {
        s["required"] = json!(required);
    }
    s
}

fn spec(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

fn with_scope(mut props: Vec<(&'static str, Value)>) -> Vec<(&'static str, Value)> {
    props.extend([
        ("project", prop_string("Project name (owner/repo), prj_ id, or repository path. Default: the project of the repository the server was started in.")),
        ("all_projects", prop_bool("Include every project in the database instead of the current one.")),
        ("session", prop_string("Restrict to one session: a ses_ id (a short prefix of >= 4 hex characters is accepted) or the provider's own session id.")),
        ("since", prop_string("Only events observed at or after this time: RFC 3339, YYYY-MM-DD, today, yesterday, or relative like -2h, -30m, -1d, -1w.")),
        ("until", prop_string("Only events observed at or before this time (same formats as since).")),
        ("captured_only", prop_bool("Ignore events reconstructed from transcripts; use only hook-captured facts.")),
    ]);
    props
}

/// The `tools/list` payload.
pub fn catalogue() -> Vec<Value> {
    vec![
        spec(
            "attempt_status",
            "Health and shape of the local AttemptDB: database path, capture mode (whether prompt/command text is stored at all), event and session counts, per-provider last activity, whether the capture daemon is running, and the default project scope. Call it first when other tools return nothing.",
            schema(vec![], &[]),
        ),
        spec(
            "attempt_timeline",
            "Sessions → turns → attempts for the current project, newest session first, with outcomes, failure classes, touched paths and evidence ids. Returns a dense text rendering plus a JSON mirror (second content block). Use limit for the number of sessions and session to focus on one.",
            schema(
                with_scope(vec![
                    (
                        "limit",
                        prop_int("Maximum sessions to show, newest first (default 10).", 1),
                    ),
                    (
                        "tools",
                        prop_bool("Also list the individual tool calls under each attempt."),
                    ),
                    (
                        "all",
                        prop_bool(
                            "Include sessions with no prompts and no tool calls (capture tests, stray events).",
                        ),
                    ),
                ]),
                &[],
            ),
        ),
        spec(
            "attempt_failures",
            "Attempts that failed or were superseded by a retry, newest first, with approach, failure class, paths, what retried them and evidence ids. Check this before retrying something to see what was already tried and how it failed.",
            schema(
                with_scope(vec![(
                    "limit",
                    prop_int("Maximum attempts (default 50).", 1),
                )]),
                &[],
            ),
        ),
        spec(
            "attempt_why",
            "Evidence-backed explanation. subject = 'project' (default: why is the project blocked?), a ses_ session id (why is this session blocked?), or an att_ attempt id (why did this attempt fail?). Each answer carries a claim, a confidence, an uncertainty statement and evidence event ids; an empty answer means nothing looks blocked.",
            schema(
                with_scope(vec![(
                    "subject",
                    prop_string("'project' (default), a ses_ session id, or an att_ attempt id."),
                )]),
                &[],
            ),
        ),
        spec(
            "attempt_trace",
            "Walk causal edges (default: backwards, i.e. what caused this) from an att_, trn_, ses_, spn_ or ev_ id: the events that caused, blocked, superseded or resolved it, with confidence and evidence per edge.",
            schema(
                with_scope(vec![
                    (
                        "id",
                        prop_string(
                            "An att_, trn_, ses_, spn_ or ev_ id (full, or a prefix of >= 4 hex characters).",
                        ),
                    ),
                    ("depth", prop_int("Maximum edge depth (default 10).", 1)),
                    (
                        "direction",
                        prop_enum(
                            "up = causes (default), down = effects, both.",
                            &["up", "down", "both"],
                        ),
                    ),
                ]),
                &["id"],
            ),
        ),
        spec(
            "attempt_state_at",
            "Time travel: the state of every active session (or one session) at a timestamp — current turn and its status, in-flight tool calls, last attempt and its outcome, failure class, whether it looked blocked and why.",
            schema(
                with_scope(vec![
                    (
                        "at",
                        prop_string(
                            "RFC 3339 timestamp, YYYY-MM-DD, now, today, yesterday, or relative like -2h / -30m / -1d.",
                        ),
                    ),
                    (
                        "subject",
                        prop_string("'project' (default) or a ses_ session id."),
                    ),
                ]),
                &["at"],
            ),
        ),
        spec(
            "attempt_evidence",
            "The raw events behind a projected entity (att_, trn_, ses_, spn_) or one event (ev_): observed_at, kind, tool, path, outcome and event id per row. Use it to verify any claim made by the other tools.",
            schema(
                with_scope(vec![(
                    "id",
                    prop_string("An att_, trn_, ses_, spn_ or ev_ id (full or short prefix)."),
                )]),
                &["id"],
            ),
        ),
        spec(
            "attempt_query",
            "Run one AttemptQL statement (SHOW ATTEMPTS FOR path = 'src/*.rs', SHOW FAILED ATTEMPTS, SHOW HANDOFFS, SHOW EVIDENCE FOR <id>, WHY <ses_id> STATUS BLOCKED, TRACE <id> CAUSES, STATE project AT '<ts>', DIFF STATE '<t1>' '<t2>', WHAT IS project DOING NOW, EXPLAIN <statement>) or read-only SQL (DataFusion dialect) over the tables events, events_raw, sessions, turns, tool_calls, attempts, handoffs, edges, signals. The engine is read-only: only SELECT/WITH/EXPLAIN/DESCRIBE and the AttemptQL verbs are accepted. Rows are capped by the server's max_rows.",
            schema(
                with_scope(vec![
                    ("statement", prop_string("The AttemptQL or SQL statement.")),
                    (
                        "format",
                        prop_enum("Result format (default table).", &["table", "json", "csv"]),
                    ),
                    (
                        "limit",
                        prop_int(
                            "Row cap for this call (default and maximum: the server's max_rows).",
                            1,
                        ),
                    ),
                ]),
                &["statement"],
            ),
        ),
        spec(
            "attempt_handoff_brief",
            "Continuation brief for the next agent: the latest session(s) and provider, what the last turns tried (objectives when prompt text was captured, otherwise prompt sizes), which attempts failed and how, which files were touched, open in-flight tool calls and pending permission signals — every claim with evidence ids — and an explicit uncertainty section (coverage grade, hook-captured vs reconstructed counts, confidence). Call this at the start of a session to continue without asking the human to re-explain.",
            schema(
                with_scope(vec![(
                    "turns",
                    prop_int("How many of the latest turns to describe (default 5).", 1),
                )]),
                &[],
            ),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Run one tool. Never returns a JSON-RPC error: problems become
/// `isError` results.
pub fn call(store: &mut Store, name: &str, args: &Map<String, Value>) -> Value {
    let outcome = match name {
        "attempt_status" => status(store, args),
        "attempt_timeline" => timeline(store, args),
        "attempt_failures" => failures(store, args),
        "attempt_why" => why(store, args),
        "attempt_trace" => trace(store, args),
        "attempt_state_at" => state_at(store, args),
        "attempt_evidence" => evidence(store, args),
        "attempt_query" => query(store, args),
        "attempt_handoff_brief" => handoff_brief(store, args),
        _ => {
            return tool_error(format!(
                "unknown tool {name:?}; available tools: {}",
                TOOL_NAMES.join(", ")
            ));
        }
    };
    match outcome {
        Ok(blocks) => tool_ok(blocks),
        Err(e) => tool_error(format!("{e:#}")),
    }
}

fn bad(e: String) -> anyhow::Error {
    anyhow!("invalid arguments: {e}")
}

fn scope_of(args: &Map<String, Value>) -> Result<ScopeArgs> {
    ScopeArgs::from_json(args).map_err(bad)
}

/// Validate an id argument before it is spliced into a statement.
fn id_token(raw: &str, what: &str) -> Result<String> {
    let s = raw.trim();
    if s.is_empty() {
        bail!("{what} is required");
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!(
            "{what} {s:?} is not an id: expected a prefixed id such as att_0191… or ses_0191… (a prefix of >= 4 hex characters is accepted)"
        );
    }
    Ok(s.to_string())
}

fn quote(s: &str) -> String {
    s.replace('\'', "''")
}

fn run_statement(ready: &Ready<'_>, statement: &str) -> Result<QueryResult> {
    match ready.block_on(ready.view.engine.query(statement)) {
        Ok(r) => Ok(r),
        Err(e @ QueryError::Parse { .. }) => bail!("{}", format_parse_error(statement, &e)),
        Err(e) => bail!("{statement}: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Shared renderers (also used by the brief)
// ---------------------------------------------------------------------------

pub(crate) fn outcome_glyph(o: AttemptOutcome) -> &'static str {
    match o {
        AttemptOutcome::Succeeded => "✓ succeeded",
        AttemptOutcome::Failed => "✗ failed",
        AttemptOutcome::Superseded => "↻ superseded",
        AttemptOutcome::Abandoned => "… abandoned",
        AttemptOutcome::InProgress => "▶ in progress",
        AttemptOutcome::Unknown => "? unknown",
    }
}

pub(crate) fn turn_status_text(s: TurnStatus) -> &'static str {
    match s {
        TurnStatus::Completed => "completed",
        TurnStatus::Failed => "failed",
        TurnStatus::InProgress => "in progress",
        TurnStatus::Unknown => "no stop seen",
    }
}

pub(crate) fn elapsed_ms(start: Timestamp, end: Timestamp) -> u64 {
    (end.as_millis() - start.as_millis()).max(0) as u64
}

pub(crate) fn path_list(paths: &[String], max: usize) -> String {
    let mut s = paths
        .iter()
        .take(max)
        .map(|p| clip(p, 60))
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > max {
        let _ = write!(s, " (+{} more)", paths.len() - max);
    }
    s
}

/// What a turn was about, as far as the capture mode lets us say.
pub(crate) fn turn_objective(t: &Turn, max: usize) -> String {
    match (&t.objective, t.prompt_chars) {
        (Some(o), _) => format!("\"{}\"", clip(o, max)),
        (None, Some(n)) => format!("(prompt of {n} chars; text not captured)"),
        (None, None) if t.index == 0 => "(activity before the first prompt)".to_string(),
        (None, None) => "(prompt; text not captured)".to_string(),
    }
}

/// One dense line per attempt: id, outcome, class, approach, paths,
/// duration, confidence, retry links, evidence.
pub(crate) fn attempt_line(a: &Attempt, evidence_max: usize) -> String {
    let mut s = format!("{} {}", id(&a.attempt_id), outcome_glyph(a.outcome));
    if let Some(c) = &a.failure_class {
        let _ = write!(s, " [{}]", clip(c, 40));
    }
    let _ = write!(s, " — {}", clip(&a.approach, 90));
    if !a.paths.is_empty() {
        let _ = write!(s, " · paths: {}", path_list(&a.paths, 4));
    }
    if let Some(e) = a.ended_at {
        let _ = write!(s, " · {}", duration(elapsed_ms(a.started_at, e)));
    }
    let _ = write!(s, " · conf {}", a.confidence);
    if let Some(n) = a.superseded_by {
        let _ = write!(s, " · retried by {}", id(&n));
    }
    if let Some(prev) = a.supersedes {
        let _ = write!(s, " · retries {}", id(&prev));
    }
    let _ = write!(s, " · evidence: {}", ids(&a.evidence, evidence_max));
    s
}

pub(crate) fn tool_call_line(tc: &ToolCall) -> String {
    let status = tc
        .outcome
        .as_ref()
        .map(|o| {
            let mut t = o.status.as_str().to_string();
            if let Some(c) = &o.class {
                let _ = write!(t, ":{}", clip(c, 30));
            }
            if let Some(code) = o.exit_code {
                let _ = write!(t, " exit {code}");
            }
            t
        })
        .unwrap_or_else(|| "in flight".to_string());
    let mut line = format!(
        "{} {} {} {}",
        id(&tc.tool_call_id),
        clip(&tc.tool.name, 20),
        status,
        tc.duration_ms
            .map(duration)
            .unwrap_or_else(|| "—".to_string())
    );
    if let Some(p) = tc.paths.first() {
        let _ = write!(line, " {}", clip(p.display(), 60));
    }
    let _ = write!(
        line,
        " [{}]",
        ids(
            &tc.start_event_id
                .into_iter()
                .chain(tc.end_event_id)
                .collect::<Vec<_>>(),
            2
        )
    );
    line
}

pub(crate) fn session_header(s: &Session) -> String {
    let mut h = format!(
        "▌ {} {} · {} · {} · coverage {} · {} · {} · {}",
        id(&s.session_id),
        s.provider.display_name(),
        clip(&s.project_name, 40),
        span(s.started_at, s.ended_at),
        s.coverage.as_str(),
        plural(s.turn_count as usize, "turn"),
        plural(s.tool_call_count as usize, "tool call"),
        plural(s.failure_count as usize, "failure")
    );
    if let Some(r) = &s.end_reason {
        let _ = write!(h, " · ended: {}", clip(r, 30));
    }
    h
}

pub(crate) fn turns_of<'a>(p: &'a Projection, s: &Session) -> Vec<&'a Turn> {
    let mut turns: Vec<&Turn> = p.turns_of(s.session_id).collect();
    turns.sort_by_key(|t| t.index);
    turns
}

pub(crate) fn attempts_of_turn<'a>(p: &'a Projection, t: &Turn) -> Vec<&'a Attempt> {
    let mut attempts: Vec<&Attempt> = p
        .attempts
        .iter()
        .filter(|a| a.turn_id == t.turn_id)
        .collect();
    attempts.sort_by_key(|a| a.index);
    attempts
}

pub(crate) fn tool_call<'a>(p: &'a Projection, id: &SpanId) -> Option<&'a ToolCall> {
    p.tool_calls.iter().find(|c| c.tool_call_id == *id)
}

/// The failed or denied tool call inside an attempt, if any.
pub(crate) fn failing_call<'a>(a: &Attempt, p: &'a Projection) -> Option<&'a ToolCall> {
    a.tool_call_ids
        .iter()
        .filter_map(|tid| tool_call(p, tid))
        .find(|tc| {
            tc.outcome
                .as_ref()
                .is_some_and(|o| matches!(o.status, OutcomeStatus::Failure | OutcomeStatus::Denied))
        })
}

pub(crate) fn handoff_line(h: &Handoff) -> String {
    format!(
        "⇄ handoff {} {} → {} {} at {} after {} gap · shared paths: {} · conf {} · evidence: {}",
        h.from_provider.display_name(),
        id(&h.from_session),
        h.to_provider.display_name(),
        id(&h.to_session),
        ts(h.at),
        duration(h.gap_ms),
        if h.shared_paths.is_empty() {
            "none".to_string()
        } else {
            path_list(&h.shared_paths, 4)
        },
        h.confidence,
        ids(&h.evidence, 4)
    )
}

fn capture_mode_text(m: CaptureMode) -> &'static str {
    match m {
        CaptureMode::MetadataOnly => {
            "metadata_only — no prompt, command or tool-output text is stored; objectives appear as prompt sizes only"
        }
        CaptureMode::LocalSemantic => {
            "local_semantic — prompt, command and tool-output text is stored locally and never synced"
        }
        CaptureMode::FullSync => {
            "full_sync — content is stored locally and may be synced to a hosted service"
        }
    }
}

// ---------------------------------------------------------------------------
// attempt_status
// ---------------------------------------------------------------------------

fn status(store: &mut Store, _args: &Map<String, Value>) -> Result<Vec<Value>> {
    let ready = store.view(&ScopeArgs::default())?;
    let text = status_text(&ready);
    let mirror = status_json(&ready);
    Ok(vec![text_block(text), json_block(&mirror)])
}

/// The status text for the `attemptdb://status` resource.
pub fn status_text_for(store: &mut Store) -> Result<String> {
    let ready = store.view(&ScopeArgs::default())?;
    Ok(status_text(&ready))
}

fn daemon_state(ready: &Ready<'_>) -> (String, Value) {
    if ready.view.status.snapshot {
        return (
            "not applicable (serving a snapshot)".to_string(),
            json!({ "state": "n/a" }),
        );
    }
    match daemon::probe(ready.locator) {
        Probe::Running(s) => (
            format!(
                "running (pid {}, {}; {} events ingested, generation {})",
                s.pid, s.endpoint, s.events_ingested, s.generation
            ),
            json!({ "state": "running", "pid": s.pid, "endpoint": s.endpoint, "events_ingested": s.events_ingested }),
        ),
        Probe::NotRunning => (
            "not running (hooks spool to disk; this server imports the spool whenever it refreshes)"
                .to_string(),
            json!({ "state": "not_running" }),
        ),
        Probe::Unresponsive(e) => (
            format!("not answering ({e})"),
            json!({ "state": "unresponsive", "error": e.to_string() }),
        ),
    }
}

fn status_text(ready: &Ready<'_>) -> String {
    let st = &ready.view.status;
    let scope = &ready.view.scope;
    let (daemon_text, _) = daemon_state(ready);
    let mut out = String::new();
    let _ = writeln!(out, "AttemptDB status (read at {})", ts(st.loaded_at));
    let _ = writeln!(
        out,
        "database      {}{}",
        st.source,
        if st.read_only && !st.snapshot {
            " (read-only: another writer — normally the daemon — holds the lock; its WAL is still visible)"
        } else {
            ""
        }
    );
    let _ = writeln!(out, "capture mode  {}", capture_mode_text(st.capture_mode));
    let _ = writeln!(
        out,
        "events        {} ({} in {} segment(s), {} in WAL) · {} · {} · {} hook-captured, {} reconstructed from transcripts",
        st.events,
        st.segment_rows,
        st.segments,
        st.memtable_rows,
        plural(st.sessions, "session"),
        plural(st.projects.len(), "project"),
        st.captured_events,
        st.reconstructed_events
    );
    let _ = writeln!(
        out,
        "last event    {}",
        st.last_event_at
            .map(ts)
            .unwrap_or_else(|| "none".to_string())
    );
    let _ = writeln!(out, "generation    {}", st.generation);
    let _ = writeln!(out, "daemon        {daemon_text}");
    let _ = writeln!(
        out,
        "scope         {}{}",
        scope.label,
        scope
            .default_reason
            .as_deref()
            .map(|r| format!(" — {r}"))
            .unwrap_or_default()
    );
    if let Some(r) = st
        .import
        .as_ref()
        .filter(|r| r.accepted > 0 || r.spool_files > 0)
    {
        let _ = writeln!(
            out,
            "imported      {} new event(s) from {} spool file(s) on this refresh{}",
            r.accepted,
            r.spool_files,
            if r.duplicates > 0 {
                format!(", {} duplicate(s) skipped", r.duplicates)
            } else {
                String::new()
            }
        );
    }
    if st.spool_pending {
        let _ = writeln!(
            out,
            "spool         pending files could not be imported (read-only); the daemon imports them"
        );
    }
    if !st.providers.is_empty() {
        let _ = writeln!(out, "providers:");
        for p in &st.providers {
            let _ = writeln!(
                out,
                "  {:<13} {:>7} events   last {}",
                p.provider,
                p.events,
                p.last_event_at
                    .map(ts)
                    .unwrap_or_else(|| "capture test only".to_string())
            );
        }
    }
    if !st.projects.is_empty() {
        let _ = writeln!(out, "projects:");
        for p in st.projects.iter().take(20) {
            let _ = writeln!(
                out,
                "  {} ({})  {} events · {} session(s)",
                clip(&p.name, 50),
                p.project_id.map(|x| id(&x)).unwrap_or_default(),
                p.events,
                p.sessions
            );
        }
        if st.projects.len() > 20 {
            let _ = writeln!(out, "  (+{} more projects)", st.projects.len() - 20);
        }
    }
    for w in &st.warnings {
        let _ = writeln!(out, "warning: {}", clip(w, 200));
    }
    if st.events == 0 {
        let _ = writeln!(
            out,
            "no events yet: install hooks with `attempt hook install`, work with a coding agent, then ask again"
        );
    }
    out.trim_end().to_string()
}

fn status_json(ready: &Ready<'_>) -> Value {
    let st = &ready.view.status;
    let scope = &ready.view.scope;
    let (_, daemon) = daemon_state(ready);
    json!({
        "database": st.source,
        "read_only": st.read_only,
        "snapshot": st.snapshot,
        "capture_mode": st.capture_mode.as_str(),
        "content_captured": st.capture_mode.persists_content_locally(),
        "generation": st.generation,
        "segments": st.segments,
        "segment_rows": st.segment_rows,
        "memtable_rows": st.memtable_rows,
        "wal_bytes": st.wal_bytes,
        "spool_pending": st.spool_pending,
        "events": st.events,
        "captured_events": st.captured_events,
        "reconstructed_events": st.reconstructed_events,
        "sessions": st.sessions,
        "last_event_at": st.last_event_at.map(ts),
        "daemon": daemon,
        "scope": {
            "label": scope.label,
            "project_id": id_opt(&scope.project_id),
            "project_name": scope.project_name,
            "default_reason": scope.default_reason,
        },
        "providers": st.providers.iter().map(|p| json!({
            "provider": p.provider, "events": p.events, "last_event_at": p.last_event_at.map(ts)
        })).collect::<Vec<_>>(),
        "projects": st.projects.iter().map(|p| json!({
            "project_id": id_opt(&p.project_id), "name": p.name, "events": p.events, "sessions": p.sessions
        })).collect::<Vec<_>>(),
        "import": st.import,
        "warnings": st.warnings,
        "read_at": ts(st.loaded_at),
    })
}

// ---------------------------------------------------------------------------
// attempt_timeline
// ---------------------------------------------------------------------------

fn tool_call_json(tc: &ToolCall) -> Value {
    json!({
        "tool_call_id": id(&tc.tool_call_id),
        "tool": tc.tool.name,
        "category": tc.tool.category.as_str(),
        "call_id": tc.tool.call_id,
        "status": tc.outcome.as_ref().map(|o| o.status.as_str()),
        "class": tc.outcome.as_ref().and_then(|o| o.class.clone()),
        "exit_code": tc.outcome.as_ref().and_then(|o| o.exit_code),
        "paths": tc.paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "started_at": tc.started_at.map(ts),
        "finished_at": tc.finished_at.map(ts),
        "duration_ms": tc.duration_ms,
        "start_event_id": id_opt(&tc.start_event_id),
        "end_event_id": id_opt(&tc.end_event_id),
    })
}

fn attempt_json(a: &Attempt, p: &Projection, with_tools: bool) -> Value {
    let mut v = json!({
        "attempt_id": id(&a.attempt_id),
        "session_id": id(&a.session_id),
        "turn_id": id(&a.turn_id),
        "turn_index": a.turn_index,
        "index": a.index,
        "outcome": a.outcome.as_str(),
        "failure_class": a.failure_class,
        "approach": a.approach,
        "objective": a.objective.as_deref().map(|o| clip(o, 500)),
        "paths": a.paths,
        "started_at": ts(a.started_at),
        "ended_at": a.ended_at.map(ts),
        "superseded_by": id_opt(&a.superseded_by),
        "supersedes": id_opt(&a.supersedes),
        "confidence": conf(a.confidence),
        "tool_call_count": a.tool_call_ids.len(),
        "evidence": id_vec(&a.evidence),
    });
    if with_tools {
        v["tool_calls"] = Value::Array(
            a.tool_call_ids
                .iter()
                .filter_map(|tid| tool_call(p, tid))
                .map(tool_call_json)
                .collect(),
        );
    }
    v
}

fn turn_json(t: &Turn, attempts: Vec<Value>) -> Value {
    json!({
        "turn_id": id(&t.turn_id),
        "index": t.index,
        "status": t.status.as_str(),
        "started_at": ts(t.started_at),
        "ended_at": t.ended_at.map(ts),
        "objective": t.objective.as_deref().map(|o| clip(o, 500)),
        "prompt_chars": t.prompt_chars,
        "prompt_event_id": id_opt(&t.prompt_event_id),
        "stop_event_id": id_opt(&t.stop_event_id),
        "attempts": attempts,
    })
}

fn session_json(s: &Session, turns: Vec<Value>) -> Value {
    json!({
        "session_id": id(&s.session_id),
        "provider": s.provider.as_str(),
        "provider_session_id": s.provider_session_id,
        "project_id": id(&s.project_id),
        "project_name": s.project_name,
        "started_at": ts(s.started_at),
        "ended_at": s.ended_at.map(ts),
        "end_reason": s.end_reason,
        "start_source": s.start_source,
        "coverage": s.coverage.as_str(),
        "event_count": s.event_count,
        "turn_count": s.turn_count,
        "prompt_count": s.prompt_count,
        "tool_call_count": s.tool_call_count,
        "failure_count": s.failure_count,
        "first_event_id": id(&s.first_event_id),
        "last_event_id": id(&s.last_event_id),
        "last_event_at": ts(s.last_event_at),
        "turns": turns,
    })
}

fn handoff_json(h: &Handoff) -> Value {
    json!({
        "from_session": id(&h.from_session),
        "to_session": id(&h.to_session),
        "from_provider": h.from_provider.as_str(),
        "to_provider": h.to_provider.as_str(),
        "at": ts(h.at),
        "gap_ms": h.gap_ms,
        "shared_paths": h.shared_paths,
        "confidence": conf(h.confidence),
        "evidence": id_vec(&h.evidence),
    })
}

fn timeline(store: &mut Store, args: &Map<String, Value>) -> Result<Vec<Value>> {
    let scope = scope_of(args)?;
    let limit = opt_usize(args, "limit")
        .map_err(bad)?
        .unwrap_or(DEFAULT_TIMELINE_SESSIONS)
        .max(1);
    let with_tools = opt_bool(args, "tools").map_err(bad)?.unwrap_or(false);
    let show_all = opt_bool(args, "all").map_err(bad)?.unwrap_or(false);
    let ready = store.view(&scope)?;
    let view = ready.view;
    let p = view.engine.projection();
    let max_rows = ready.config.max_rows;

    let mut sessions: Vec<&Session> = p
        .sessions
        .iter()
        .filter(|s| show_all || s.prompt_count > 0 || s.tool_call_count > 0)
        .collect();
    sessions.sort_by_key(|a| std::cmp::Reverse(a.started_at));
    let total = sessions.len();
    let shown: Vec<&Session> = sessions.into_iter().take(limit).collect();

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} · {} · {} · {} from {} (inference {}) · scope: {}",
        plural(p.sessions.len(), "session"),
        plural(p.turns.len(), "turn"),
        plural(p.attempts.len(), "attempt"),
        plural(p.handoffs.len(), "handoff"),
        plural(view.engine.event_count(), "event"),
        p.algorithm_version,
        view.scope.label
    );
    if shown.is_empty() {
        if p.sessions.is_empty() {
            let _ = writeln!(
                out,
                "no sessions in scope. Check attempt_status (hooks installed? right project? try all_projects=true)."
            );
        } else {
            let _ = writeln!(
                out,
                "{} session(s) carry no prompts or tool calls (capture tests, stray events); pass all=true to list them.",
                p.sessions.len()
            );
        }
    }

    let mut attempt_lines = 0usize;
    let mut truncated = false;
    let mut sessions_json = Vec::new();
    for s in &shown {
        let _ = writeln!(out);
        let _ = writeln!(out, "{}", session_header(s));
        let mut turns_json = Vec::new();
        for t in turns_of(p, s) {
            let _ = writeln!(
                out,
                "  turn {} {} {} {} — {}",
                t.index,
                id(&t.turn_id),
                turn_status_text(t.status),
                span(t.started_at, t.ended_at),
                turn_objective(t, 100)
            );
            let mut attempts_json = Vec::new();
            for a in attempts_of_turn(p, t) {
                if attempt_lines >= max_rows {
                    truncated = true;
                    break;
                }
                attempt_lines += 1;
                let _ = writeln!(out, "    {}", attempt_line(a, 3));
                if with_tools {
                    for tid in &a.tool_call_ids {
                        if let Some(tc) = tool_call(p, tid) {
                            let _ = writeln!(out, "      {}", tool_call_line(tc));
                        }
                    }
                }
                attempts_json.push(attempt_json(a, p, with_tools));
            }
            turns_json.push(turn_json(t, attempts_json));
        }
        sessions_json.push(session_json(s, turns_json));
    }
    if total > shown.len() {
        let _ = writeln!(
            out,
            "\n({} more session(s) in scope; raise limit or pass session=<ses_id>)",
            total - shown.len()
        );
    }
    if truncated {
        let _ = writeln!(
            out,
            "\n(attempt list cut at max_rows={max_rows}; narrow with session/since or raise --max-rows)"
        );
    }
    if !p.handoffs.is_empty() {
        let _ = writeln!(out);
        for h in &p.handoffs {
            let _ = writeln!(out, "{}", handoff_line(h));
        }
    }
    let mirror = json!({
        "scope": view.scope.label,
        "inference": p.algorithm_version.as_str(),
        "event_count": view.engine.event_count(),
        "totals": {
            "sessions": p.sessions.len(),
            "turns": p.turns.len(),
            "attempts": p.attempts.len(),
            "handoffs": p.handoffs.len(),
        },
        "sessions_matching": total,
        "sessions_shown": shown.len(),
        "attempts_truncated": truncated,
        "sessions": sessions_json,
        "handoffs": p.handoffs.iter().map(handoff_json).collect::<Vec<_>>(),
    });
    Ok(vec![text_block(out.trim_end()), json_block(&mirror)])
}

// ---------------------------------------------------------------------------
// attempt_failures
// ---------------------------------------------------------------------------

fn failures(store: &mut Store, args: &Map<String, Value>) -> Result<Vec<Value>> {
    let scope = scope_of(args)?;
    let limit = opt_usize(args, "limit")
        .map_err(bad)?
        .unwrap_or(DEFAULT_FAILURES)
        .max(1);
    let ready = store.view(&scope)?;
    let view = ready.view;
    let p = view.engine.projection();
    let limit = limit.min(ready.config.max_rows);
    let mut failed: Vec<&Attempt> = p
        .attempts
        .iter()
        .filter(|a| a.outcome.is_failure())
        .collect();
    failed.sort_by_key(|a| std::cmp::Reverse(a.started_at));
    let mut out = String::new();
    if failed.is_empty() {
        let _ = writeln!(
            out,
            "no failed or superseded attempts among {} (inference {}) · scope: {}",
            plural(p.attempts.len(), "attempt"),
            p.algorithm_version,
            view.scope.label
        );
        return Ok(vec![text_block(out.trim_end())]);
    }
    let _ = writeln!(
        out,
        "{} failed/superseded of {} (inference {}), newest first · scope: {}",
        failed.len(),
        plural(p.attempts.len(), "attempt"),
        p.algorithm_version,
        view.scope.label
    );
    for a in failed.iter().take(limit) {
        let session = p.session(a.session_id);
        let provider = session
            .map(|s| s.provider.display_name().to_string())
            .unwrap_or_else(|| "?".to_string());
        let project = session
            .map(|s| clip(&s.project_name, 40))
            .unwrap_or_default();
        let mut line = format!(
            "{} {}",
            id(&a.attempt_id),
            outcome_glyph(a.outcome).trim_start_matches(|c: char| !c.is_alphabetic())
        );
        if let Some(c) = &a.failure_class {
            let _ = write!(line, " [{}]", clip(c, 40));
        }
        let _ = write!(
            line,
            " · {provider} · {project} · {} · turn {} #{} · session {}",
            ts(a.started_at),
            a.turn_index,
            a.index,
            id(&a.session_id)
        );
        let _ = write!(line, "\n    approach: {}", clip(&a.approach, 120));
        if !a.paths.is_empty() {
            let _ = write!(line, " · paths: {}", path_list(&a.paths, 6));
        }
        if let Some(tc) = failing_call(a, p) {
            let _ = write!(line, "\n    failing call: {}", tool_call_line(tc));
        }
        match a.objective.as_deref() {
            Some(o) => {
                let _ = write!(line, "\n    objective: \"{}\"", clip(o, 160));
            }
            None => {
                let _ = write!(line, "\n    objective: (prompt text not captured)");
            }
        }
        match a.superseded_by {
            Some(n) => {
                let next = p.attempts.iter().find(|x| x.attempt_id == n);
                let _ = write!(
                    line,
                    "\n    retried by {} ({})",
                    id(&n),
                    next.map(|x| x.outcome.as_str()).unwrap_or("unknown")
                );
            }
            None => {
                let _ = write!(line, "\n    not retried on the same paths");
            }
        }
        let _ = write!(
            line,
            " · conf {} · evidence: {}",
            a.confidence,
            ids(&a.evidence, 4)
        );
        let _ = writeln!(out, "{line}");
    }
    if failed.len() > limit {
        let _ = writeln!(
            out,
            "(+{} more; raise limit or narrow with session/since)",
            failed.len() - limit
        );
    }
    let _ = writeln!(
        out,
        "note: attempts are Tier 1 inferences; attempt_why <att_id> explains one, attempt_evidence <att_id> lists its events"
    );
    Ok(vec![text_block(out.trim_end())])
}

// ---------------------------------------------------------------------------
// attempt_why / attempt_trace / attempt_state_at / attempt_evidence
// ---------------------------------------------------------------------------

fn why_statement(subject: &str) -> Result<String> {
    let s = subject.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("project") {
        return Ok("WHY project STATUS BLOCKED".to_string());
    }
    if s.starts_with("att_") {
        return Ok(format!("WHY {} FAILED", id_token(s, "subject")?));
    }
    if s.starts_with("ses_") {
        return Ok(format!(
            "WHY session '{}' STATUS BLOCKED",
            id_token(s, "subject")?
        ));
    }
    let hexish = s
        .chars()
        .filter(|c| *c != '-')
        .all(|c| c.is_ascii_hexdigit());
    if hexish && s.len() >= 4 {
        return Ok(format!("WHY session '{s}' STATUS BLOCKED"));
    }
    Ok(format!("WHY project '{}' STATUS BLOCKED", quote(s)))
}

fn why(store: &mut Store, args: &Map<String, Value>) -> Result<Vec<Value>> {
    let scope = scope_of(args)?;
    let subject = opt_string(args, "subject").map_err(bad)?;
    let statement = why_statement(subject.as_deref().unwrap_or("project"))?;
    let ready = store.view(&scope)?;
    let r = run_statement(&ready, &statement)?;
    let mut text = format!("{statement}\n{}", result_text(&r, ready.config.max_rows));
    if r.row_count() == 0 {
        text.push_str("\nnothing looks blocked/failed for this subject; attempt_state_at or attempt_timeline show what it is doing");
    }
    Ok(vec![text_block(text)])
}

fn trace(store: &mut Store, args: &Map<String, Value>) -> Result<Vec<Value>> {
    let scope = scope_of(args)?;
    let subject = id_token(&req_string(args, "id").map_err(bad)?, "id")?;
    let depth = opt_usize(args, "depth").map_err(bad)?;
    let direction = opt_string(args, "direction").map_err(bad)?;
    let mut statement = format!("TRACE {subject} CAUSES");
    if let Some(d) = depth {
        let _ = write!(statement, " DEPTH {}", d.max(1));
    }
    if let Some(dir) = direction {
        match dir.to_ascii_lowercase().as_str() {
            "up" => {}
            "down" => statement.push_str(" DIRECTION DOWN"),
            "both" => statement.push_str(" DIRECTION BOTH"),
            other => bail!("invalid arguments: direction must be up, down or both (got {other:?})"),
        }
    }
    let ready = store.view(&scope)?;
    let r = run_statement(&ready, &statement)?;
    let max_rows = ready.config.max_rows;
    let mut out = format!("{statement}\n");
    let rows = r.to_json();
    let rows = rows.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        out.push_str(&result_text(&r, max_rows));
        return Ok(vec![text_block(out)]);
    }
    for row in rows.iter().take(max_rows) {
        let _ = writeln!(
            out,
            "d{} {:<11} {} {} → {} {} · conf {} ({}) · evidence: {}",
            cell_text(&row["depth"]),
            cell_text(&row["edge_kind"]),
            cell_text(&row["from_type"]),
            cell_text(&row["from_id"]),
            cell_text(&row["to_type"]),
            cell_text(&row["to_id"]),
            cell_text(&row["confidence"]),
            cell_text(&row["edge_source"]),
            clip(&cell_text(&row["evidence"]), 200)
        );
    }
    let _ = write!(out, "({}", plural(rows.len(), "edge"));
    if rows.len() > max_rows {
        let _ = write!(out, ", first {max_rows} shown");
    }
    out.push(')');
    for n in &r.notes {
        let _ = write!(out, "\nnote: {}", clip(n, 400));
    }
    Ok(vec![text_block(out)])
}

fn state_at(store: &mut Store, args: &Map<String, Value>) -> Result<Vec<Value>> {
    let scope = scope_of(args)?;
    let at_raw = req_string(args, "at").map_err(bad)?;
    let at = parse_time(&at_raw).ok_or_else(|| {
        bad(format!(
            "cannot parse at {at_raw:?}: use RFC 3339, YYYY-MM-DD, now, today, yesterday or -<n>(s|m|h|d|w)"
        ))
    })?;
    let subject = opt_string(args, "subject").map_err(bad)?;
    let subject_sql = match subject.as_deref().map(str::trim) {
        None | Some("") => "project".to_string(),
        Some(s) if s.eq_ignore_ascii_case("project") => "project".to_string(),
        Some(s) if s.starts_with("ses_") => format!("session '{}'", id_token(s, "subject")?),
        Some(s) if s.starts_with("att_") || s.starts_with("trn_") => {
            bail!("invalid arguments: subject must be 'project' or a ses_ session id (got {s:?})")
        }
        Some(s) => format!("project '{}'", quote(s)),
    };
    let statement = format!("STATE {subject_sql} AT '{}'", at.to_rfc3339());
    let ready = store.view(&scope)?;
    let r = run_statement(&ready, &statement)?;
    Ok(vec![text_block(format!(
        "{statement}\n{}",
        result_text(&r, ready.config.max_rows)
    ))])
}

fn evidence(store: &mut Store, args: &Map<String, Value>) -> Result<Vec<Value>> {
    let scope = scope_of(args)?;
    let subject = id_token(&req_string(args, "id").map_err(bad)?, "id")?;
    let statement = format!("SHOW EVIDENCE FOR {subject}");
    let ready = store.view(&scope)?;
    let r = run_statement(&ready, &statement)?;
    Ok(vec![text_block(format!(
        "{statement}\n{}",
        result_text(&r, ready.config.max_rows)
    ))])
}

// ---------------------------------------------------------------------------
// attempt_query
// ---------------------------------------------------------------------------

const READ_VERBS: &[&str] = &[
    "SELECT", "WITH", "VALUES", "DESCRIBE", "EXPLAIN", "SHOW", "WHY", "TRACE", "STATE", "DIFF",
    "WHAT",
];
const WRITE_WORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "TRUNCATE", "COPY", "SET", "RESET",
    "GRANT", "REVOKE", "MERGE", "UNLOAD", "INSTALL", "LOAD", "ATTACH", "DETACH",
];

/// Words of a statement outside single-quoted strings, upper-cased.
fn bare_words(statement: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    for c in statement.chars() {
        if c == '\'' {
            in_string = !in_string;
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        if in_string {
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            current.push(c.to_ascii_uppercase());
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Accept only read statements. The engine cannot write to the database,
/// but DataFusion would happily `CREATE` an in-memory table or `COPY` rows
/// to a file, so anything that is not a read verb is refused up front.
pub fn check_read_only(statement: &str) -> std::result::Result<(), String> {
    let trimmed = statement.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err("empty statement".to_string());
    }
    if trimmed.contains(';') {
        return Err("one statement per call (found ';' inside the statement)".to_string());
    }
    let words = bare_words(trimmed);
    let Some(first) = words.first() else {
        return Err("statement has no keyword".to_string());
    };
    if !READ_VERBS.contains(&first.as_str()) {
        return Err(format!(
            "read-only: {first} statements are not accepted; use SELECT/WITH/EXPLAIN/DESCRIBE (SQL) or SHOW/WHY/TRACE/STATE/DIFF/WHAT IS (AttemptQL)"
        ));
    }
    if let Some(w) = words.iter().find(|w| WRITE_WORDS.contains(&w.as_str())) {
        return Err(format!(
            "read-only: {w} is not allowed inside a statement served over MCP"
        ));
    }
    Ok(())
}

/// The first `limit` rows of a result, with the original row count.
fn cap_rows(r: &QueryResult, limit: usize) -> (QueryResult, usize) {
    let total = r.row_count();
    if total <= limit {
        return (r.clone(), total);
    }
    let mut batches = Vec::new();
    let mut remaining = limit;
    for b in &r.batches {
        if remaining == 0 {
            break;
        }
        let n = b.num_rows().min(remaining);
        batches.push(b.slice(0, n));
        remaining -= n;
    }
    (
        QueryResult::new(r.schema.clone(), batches, r.kind, r.notes.clone()),
        total,
    )
}

fn query(store: &mut Store, args: &Map<String, Value>) -> Result<Vec<Value>> {
    let scope = scope_of(args)?;
    let statement = req_string(args, "statement").map_err(bad)?;
    let format = opt_string(args, "format")
        .map_err(bad)?
        .unwrap_or_else(|| "table".to_string())
        .to_ascii_lowercase();
    if !matches!(format.as_str(), "table" | "json" | "csv") {
        bail!("invalid arguments: format must be table, json or csv (got {format:?})");
    }
    check_read_only(&statement).map_err(|e| anyhow!("{e}"))?;
    let ready = store.view(&scope)?;
    let max_rows = ready.config.max_rows;
    let limit = opt_usize(args, "limit")
        .map_err(bad)?
        .map(|l| l.clamp(1, max_rows))
        .unwrap_or(max_rows);
    let r = run_statement(&ready, &statement)?;
    let (capped, total) = cap_rows(&r, limit);
    let truncated = total > limit;
    let text = match format.as_str() {
        "json" => {
            let doc = json!({
                "statement": statement,
                "columns": capped.column_names(),
                "rows": capped.to_json(),
                "row_count": total,
                "returned": capped.row_count(),
                "truncated": truncated,
                "kind": format!("{:?}", r.kind).to_ascii_lowercase(),
                "notes": r.notes,
            });
            serde_json::to_string_pretty(&doc)?
        }
        "csv" => {
            let mut s = capped.render_csv();
            if truncated {
                let _ = writeln!(s, "# {total} rows, first {limit} shown");
            }
            for n in &r.notes {
                let _ = writeln!(s, "# note: {}", clip(n, 400));
            }
            s
        }
        _ => {
            let mut s = format!("{statement}\n{}", result_text(&capped, limit));
            if truncated {
                let _ = write!(
                    s,
                    "\n({total} rows in total; first {limit} shown — add LIMIT or narrow the query)"
                );
            }
            s
        }
    };
    Ok(vec![text_block(text)])
}

// ---------------------------------------------------------------------------
// attempt_handoff_brief
// ---------------------------------------------------------------------------

fn handoff_brief(store: &mut Store, args: &Map<String, Value>) -> Result<Vec<Value>> {
    let scope = scope_of(args)?;
    let turns = opt_usize(args, "turns").map_err(bad)?;
    let text = brief_text(store, &scope, turns)?;
    Ok(vec![text_block(text)])
}

/// The brief text, also served as the `attemptdb://brief` resource.
pub fn brief_text(store: &mut Store, scope: &ScopeArgs, turns: Option<usize>) -> Result<String> {
    let ready = store.view(scope)?;
    Ok(brief::render(
        &ready,
        turns.unwrap_or(brief::DEFAULT_TURNS).max(1),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_gate() {
        assert!(check_read_only("SELECT count(*) FROM events").is_ok());
        assert!(check_read_only("  with x as (select 1) select * from x;").is_ok());
        assert!(check_read_only("SHOW FAILED ATTEMPTS FOR path = 'drop table'").is_ok());
        assert!(check_read_only("WHY project STATUS BLOCKED").is_ok());
        assert!(check_read_only("EXPLAIN SELECT 1").is_ok());
        assert!(check_read_only("INSERT INTO events VALUES (1)").is_err());
        assert!(check_read_only("CREATE TABLE t AS SELECT 1").is_err());
        assert!(check_read_only("SELECT 1; DROP TABLE events").is_err());
        assert!(check_read_only("COPY (SELECT 1) TO '/tmp/x.csv'").is_err());
        assert!(check_read_only("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x").is_err());
        assert!(check_read_only("").is_err());
    }

    #[test]
    fn why_subjects() {
        assert_eq!(
            why_statement("project").unwrap(),
            "WHY project STATUS BLOCKED"
        );
        assert_eq!(
            why_statement("att_0191abcd").unwrap(),
            "WHY att_0191abcd FAILED"
        );
        assert_eq!(
            why_statement("ses_0191abcd").unwrap(),
            "WHY session 'ses_0191abcd' STATUS BLOCKED"
        );
        assert_eq!(
            why_statement("acme/repo").unwrap(),
            "WHY project 'acme/repo' STATUS BLOCKED"
        );
        assert!(why_statement("att_x'; DROP").is_err());
    }

    #[test]
    fn catalogue_is_complete() {
        let names: Vec<String> = catalogue()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, TOOL_NAMES);
        for t in catalogue() {
            assert_eq!(t["inputSchema"]["type"], "object");
            assert!(t["inputSchema"]["properties"].is_object());
            assert!(!t["description"].as_str().unwrap().is_empty());
        }
    }
}
