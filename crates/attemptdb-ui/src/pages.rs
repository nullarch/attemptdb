//! Server-rendered pages. Each handler builds the page from the projection
//! (and, where an explanation is needed, from an AttemptQL statement) and
//! wraps it in the shared layout. Every value from the database passes
//! through [`html::esc`] or one of the typed link helpers.

use crate::api::{
    self, ApiError, Params, cap, evidence_row, find_attempt, find_session, param_flag, param_usize,
    run, state_statement, trace_statement, why_statement,
};
use crate::html::{
    self, attempt_link, badge, clip, confidence, coverage_badge, duration, elapsed_ms, esc,
    evidence_link, evidence_links, id_link, key_values, layout, notes, outcome_badge, plural,
    rfc3339, session_link, table, ts, ts_time, turn_badge,
};
use crate::json as j;
use crate::scope::ScopeQuery;
use crate::store::{View, parse_time};
use crate::{AppState, svg};
use attemptdb_core::{CaptureMode, Timestamp};
use attemptdb_project::{
    Attempt, AttentionItem, AttentionKind, Phase, Projection, Session, ToolCall, Turn, WorkUnit,
    WorkUnitStatus,
};
use attemptdb_query::{QueryResult, ResultKind};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde_json::Value;
use std::fmt::Write as _;
use std::sync::Arc;

/// Rows a server-rendered table shows before it points at the API.
const PAGE_ROWS: usize = 500;

pub struct PageError(ApiError);

impl From<ApiError> for PageError {
    fn from(e: ApiError) -> Self {
        Self(e)
    }
}

impl From<anyhow::Error> for PageError {
    fn from(e: anyhow::Error) -> Self {
        Self(e.into())
    }
}

impl From<attemptdb_query::QueryError> for PageError {
    fn from(e: attemptdb_query::QueryError) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for PageError {
    fn into_response(self) -> Response {
        let title = match self.0.status {
            StatusCode::NOT_FOUND => "Not found",
            StatusCode::BAD_REQUEST => "Bad request",
            _ => "Error",
        };
        let body = format!(
            "<section class=\"card\"><h1>{} · {}</h1><pre class=\"error\">{}</pre><p><a href=\"/\">back to the project state</a></p></section>",
            self.0.status.as_u16(),
            esc(title),
            esc(&self.0.message)
        );
        (
            self.0.status,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            html::bare(title, &body),
        )
            .into_response()
    }
}

pub type PageResult = Result<Html<String>, PageError>;

async fn view(state: &AppState, scope: &ScopeQuery) -> Result<Arc<View>, PageError> {
    Ok(api::view(state, scope).await?)
}

fn objective_html(
    objective: &Option<String>,
    prompt_chars: Option<u64>,
    index: u32,
    max: usize,
) -> String {
    match (objective, prompt_chars) {
        (Some(o), _) => format!("<span class=\"objective\">{}</span>", clip(o, max)),
        (None, Some(n)) => {
            format!("<span class=\"muted\">(prompt of {n} chars; text not captured)</span>")
        }
        (None, None) if index == 0 => {
            "<span class=\"muted\">(activity before the first prompt)</span>".to_string()
        }
        (None, None) => "<span class=\"muted\">(prompt; text not captured)</span>".to_string(),
    }
}

fn paths_html(paths: &[String], max: usize) -> String {
    if paths.is_empty() {
        return "<span class=\"muted\">no paths</span>".to_string();
    }
    let mut s: String = paths
        .iter()
        .take(max)
        .map(|p| format!("<code class=\"path\">{}</code>", clip(p, 80)))
        .collect::<Vec<_>>()
        .join(" ");
    if paths.len() > max {
        let _ = write!(
            s,
            " <span class=\"muted\">(+{} more)</span>",
            paths.len() - max
        );
    }
    s
}

fn tool_call_status(tc: &ToolCall) -> String {
    match &tc.outcome {
        Some(o) => {
            let mut t = o.status.as_str().to_string();
            if let Some(c) = &o.class {
                let _ = write!(t, ":{c}");
            }
            if let Some(code) = o.exit_code {
                let _ = write!(t, " exit {code}");
            }
            badge(html::status_class(o.status.as_str()), &t)
        }
        None if tc.started_at.is_some() && tc.finished_at.is_none() => badge("live", "in flight"),
        None => badge("muted", "no end observed"),
    }
}

fn tool_call_duration(tc: &ToolCall) -> String {
    tc.duration_ms
        .or_else(|| match (tc.started_at, tc.finished_at) {
            (Some(s), Some(e)) => Some(elapsed_ms(s, e)),
            _ => None,
        })
        .map(duration)
        .unwrap_or_default()
}

fn tool_calls_table(calls: &[&ToolCall], scope: &ScopeQuery) -> String {
    let rows: Vec<Vec<String>> = calls
        .iter()
        .map(|tc| {
            vec![
                tc.started_at.map(ts_time).unwrap_or_else(|| "—".into()),
                format!(
                    "<code>{}</code> <span class=\"muted small\">{}</span>",
                    esc(&tc.tool.name),
                    esc(tc.tool.category.as_str())
                ),
                tool_call_status(tc),
                tool_call_duration(tc),
                paths_html(
                    &tc.paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>(),
                    3,
                ),
                format!(
                    "{} {}",
                    tc.start_event_id
                        .map(|e| evidence_link(&e, scope))
                        .unwrap_or_default(),
                    tc.end_event_id
                        .map(|e| evidence_link(&e, scope))
                        .unwrap_or_default()
                ),
            ]
        })
        .collect();
    table(
        &[
            "started", "tool", "outcome", "duration", "paths", "evidence",
        ],
        &rows,
    )
}

fn attempt_row(a: &Attempt, p: &Projection, scope: &ScopeQuery) -> String {
    let dur = a
        .ended_at
        .map(|e| duration(elapsed_ms(a.started_at, e)))
        .unwrap_or_default();
    let class = a
        .failure_class
        .as_deref()
        .map(|c| format!(" {}", badge("class", c)))
        .unwrap_or_default();
    let sup = match (a.superseded_by, a.supersedes) {
        (Some(n), _) => format!(
            " <span class=\"muted\">retried by</span> {}",
            attempt_link(&n, scope)
        ),
        (None, Some(prev)) => format!(
            " <span class=\"muted\">retries</span> {}",
            attempt_link(&prev, scope)
        ),
        _ => String::new(),
    };
    let calls = plural(a.tool_call_ids.len(), "tool call");
    let _ = p;
    format!(
        "<li class=\"attempt\">{} {}{} <span class=\"approach\">{}</span> <span class=\"muted small\">{} · {} · {}</span> {}{} <span class=\"evidence\">evidence: {}</span></li>",
        attempt_link(&a.attempt_id, scope),
        outcome_badge(a.outcome),
        class,
        clip(&a.approach, 90),
        calls,
        plural(a.paths.len(), "path"),
        dur,
        confidence(a.confidence),
        sup,
        evidence_links(&a.evidence, 3, scope)
    )
}

fn session_header(s: &Session, v: &View, scope: &ScopeQuery) -> String {
    let caps = v
        .session_capture
        .get(&s.session_id)
        .copied()
        .unwrap_or_default();
    let end = s
        .ended_at
        .map(|t| format!("→ {}", ts_time(t)))
        .unwrap_or_else(|| "→ open".to_string());
    format!(
        "<span class=\"provider\">{}</span> <span class=\"project\">{}</span> <span class=\"when\">{} {}</span> {} <span class=\"muted small\">{} · {} · {} · {} captured / {} reconstructed</span> {}",
        esc(s.provider.display_name()),
        clip(&s.project_name, 40),
        ts(s.started_at),
        end,
        coverage_badge(s.coverage),
        plural(s.turn_count as usize, "turn"),
        plural(s.tool_call_count as usize, "tool call"),
        plural(s.failure_count as usize, "failure"),
        caps.captured,
        caps.reconstructed,
        session_link(&s.session_id, scope)
    )
}

fn turn_block(t: &Turn, p: &Projection, scope: &ScopeQuery) -> String {
    let mut s = format!(
        "<li class=\"turn\"><div class=\"turn-head\"><span class=\"when\">{}</span> <b>turn {}</b> {} {}</div>",
        ts_time(t.started_at),
        t.index,
        turn_badge(t.status),
        objective_html(&t.objective, t.prompt_chars, t.index, 120)
    );
    let attempts = j::attempts_of_turn(p, t);
    if attempts.is_empty() {
        s.push_str("<div class=\"muted small\">no tool calls in this turn</div>");
    } else {
        s.push_str("<ul class=\"attempts\">");
        for a in attempts {
            s.push_str(&attempt_row(a, p, scope));
        }
        s.push_str("</ul>");
    }
    s.push_str("</li>");
    s
}

fn handoffs_table(p: &Projection, scope: &ScopeQuery, limit: usize) -> String {
    if p.handoffs.is_empty() {
        return "<p class=\"muted\">no handoffs detected: a handoff needs two sessions from different agents in the same project within 30 minutes (tier1-v1)</p>".to_string();
    }
    let mut list: Vec<&attemptdb_project::Handoff> = p.handoffs.iter().collect();
    list.sort_by_key(|a| std::cmp::Reverse(a.at));
    let rows: Vec<Vec<String>> = list
        .iter()
        .take(limit)
        .map(|h| {
            vec![
                ts(h.at),
                format!(
                    "{} {}",
                    esc(h.from_provider.display_name()),
                    session_link(&h.from_session, scope)
                ),
                format!(
                    "{} {}",
                    esc(h.to_provider.display_name()),
                    session_link(&h.to_session, scope)
                ),
                duration(h.gap_ms),
                paths_html(&h.shared_paths, 4),
                confidence(h.confidence),
                evidence_links(&h.evidence, 4, scope),
            ]
        })
        .collect();
    table(
        &[
            "at",
            "from",
            "to",
            "gap",
            "shared paths",
            "confidence",
            "evidence",
        ],
        &rows,
    )
}

/// Explanation rows (`WHY`, `STATE`) as key/value records with id links.
fn records_html(r: &QueryResult, scope: &ScopeQuery) -> String {
    let rows = r.to_json();
    let rows = rows.as_array().cloned().unwrap_or_default();
    let mut s = String::new();
    for row in rows.iter().take(PAGE_ROWS) {
        let Some(obj) = row.as_object() else { continue };
        let mut kv: Vec<(&str, String)> = Vec::new();
        for (k, v) in obj {
            let rendered = match v {
                Value::Null => continue,
                Value::String(t) if t.is_empty() => continue,
                Value::Array(a) if a.is_empty() => continue,
                Value::Array(a) => a
                    .iter()
                    .map(|x| match x {
                        Value::String(t) => id_link(t, scope),
                        other => format!("<code>{}</code>", esc(&other.to_string())),
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                Value::String(t)
                    if k.ends_with("_id")
                        || k == "superseded_by"
                        || k == "last_attempt"
                        || k == "current_turn"
                        || k == "from_session"
                        || k == "to_session" =>
                {
                    id_link(t, scope)
                }
                Value::String(t) => esc(t),
                other => esc(&other.to_string()),
            };
            kv.push((k.as_str(), rendered));
        }
        s.push_str(&key_values(&kv));
    }
    if rows.len() > PAGE_ROWS {
        let _ = write!(
            s,
            "<p class=\"muted\">{} rows; first {PAGE_ROWS} shown</p>",
            rows.len()
        );
    }
    s
}

/// Row-style results as a table with id links.
fn rows_html(r: &QueryResult, scope: &ScopeQuery) -> String {
    let names = r.column_names();
    let (capped, total) = cap(r, PAGE_ROWS);
    let json = capped.to_json();
    let rows = json.as_array().cloned().unwrap_or_default();
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            names
                .iter()
                .map(|n| match row.get(n) {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::String(t)) => {
                        if t.len() >= 8
                            && (t.starts_with("ev_")
                                || t.starts_with("att_")
                                || t.starts_with("ses_"))
                        {
                            id_link(t, scope)
                        } else {
                            clip(t, 200)
                        }
                    }
                    Some(Value::Array(a)) => a
                        .iter()
                        .map(|x| match x {
                            Value::String(t) => id_link(t, scope),
                            other => esc(&other.to_string()),
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                    Some(other) => esc(&other.to_string()),
                })
                .collect()
        })
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut s = table(&names, &body);
    let _ = write!(
        s,
        "<p class=\"muted small\">({}{})</p>",
        plural(total, "row"),
        if total > PAGE_ROWS {
            format!(
                ", first {PAGE_ROWS} shown; the API returns up to {}",
                api::MAX_ROWS
            )
        } else {
            String::new()
        }
    );
    s
}

fn result_html(r: &QueryResult, scope: &ScopeQuery) -> String {
    let mut s = if r.row_count() == 0 && matches!(r.kind, ResultKind::Empty) {
        "<p class=\"muted\">(no rows)</p>".to_string()
    } else if matches!(r.kind, ResultKind::Explanation) {
        records_html(r, scope)
    } else {
        rows_html(r, scope)
    };
    s.push_str(&notes(&r.notes));
    s
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

/// Capture coverage, privacy mode, storage and pairing diagnostics — the
/// database facts, below the work they explain.
fn coverage_card(v: &View, p: &Projection, _scope: &ScopeQuery) -> String {
    let st = &v.status;
    let mut body = String::new();
    // Coverage and privacy.
    let scoped = v.scoped_capture();
    let mode_text = match st.capture_mode {
        CaptureMode::MetadataOnly => {
            "metadata_only — no prompt, command or tool-output text is stored; objectives appear as prompt sizes only"
        }
        CaptureMode::LocalSemantic => {
            "local_semantic — prompts, commands and tool output stay on this machine; nothing is synced"
        }
        CaptureMode::FullSync => {
            "full_sync — content may be synced to a hosted companion (explicit opt-in)"
        }
    };
    let mut kv: Vec<(&str, String)> = vec![
        (
            "database",
            format!(
                "<code>{}</code>{}",
                esc(&st.source),
                if st.read_only && !st.snapshot {
                    " (read-only: another writer holds the lock; its WAL is still visible)"
                } else {
                    ""
                }
            ),
        ),
        (
            "privacy mode",
            format!(
                "{} {}",
                badge(
                    match st.capture_mode {
                        CaptureMode::MetadataOnly => "muted",
                        CaptureMode::LocalSemantic => "ok",
                        CaptureMode::FullSync => "warn",
                    },
                    st.capture_mode.as_str()
                ),
                esc(mode_text)
            ),
        ),
        ("daemon", esc(&st.daemon.label())),
        (
            "events in scope",
            format!(
                "{} — {} hook-captured, {} reconstructed from transcripts",
                v.engine.event_count(),
                scoped.captured,
                scoped.reconstructed
            ),
        ),
        (
            "database-wide",
            format!(
                "{} events, {} sessions — {} captured, {} reconstructed; last event {}",
                st.events,
                st.sessions,
                st.captured_events,
                st.reconstructed_events,
                st.last_event_at.map(ts).unwrap_or_else(|| "—".into())
            ),
        ),
        (
            "projection",
            format!(
                "{} · {} · {} · {} · {} · inference <code>{}</code>",
                plural(p.sessions.len(), "session"),
                plural(p.turns.len(), "turn"),
                plural(p.tool_calls.len(), "tool call"),
                plural(p.attempts.len(), "attempt"),
                plural(p.handoffs.len(), "handoff"),
                esc(crate::INFERENCE_VERSION)
            ),
        ),
        (
            "pairing",
            format!(
                "{} tool start(s) never finished, {} finish(es) without a start, {} FIFO pairing(s), {} unknown event(s), {} out-of-order, {} injected prompt(s) skipped",
                p.stats.unpaired_tool_starts,
                p.stats.unpaired_tool_finishes,
                p.stats.fifo_pairings,
                p.stats.unknown_events,
                p.stats.out_of_order_events,
                p.stats.injected_prompts
            ),
        ),
        (
            "storage",
            format!(
                "generation {} · {} segment(s), {} rows · {} memtable rows · {} WAL bytes{}",
                st.generation,
                st.segments,
                st.segment_rows,
                st.memtable_rows,
                st.wal_bytes,
                if st.spool_pending {
                    " · spool pending"
                } else {
                    ""
                }
            ),
        ),
    ];
    if let Some(r) = &st.import {
        kv.push((
            "spool import",
            format!(
                "{} accepted, {} duplicates from {} file(s) on this refresh",
                r.accepted, r.duplicates, r.spool_files
            ),
        ));
    }
    if !st.warnings.is_empty() {
        kv.push((
            "warnings",
            st.warnings
                .iter()
                .map(|w| esc(w))
                .collect::<Vec<_>>()
                .join("<br>"),
        ));
    }
    let providers: Vec<Vec<String>> = st
        .providers
        .iter()
        .map(|pr| {
            vec![
                esc(&pr.provider),
                pr.events.to_string(),
                pr.last_event_at.map(ts).unwrap_or_else(|| "—".into()),
            ]
        })
        .collect();
    let _ = write!(
        body,
        "<section class=\"card\"><h2>Capture coverage and privacy</h2>{}{}</section>",
        key_values(&kv),
        if providers.is_empty() {
            String::new()
        } else {
            table(&["provider", "events", "last event"], &providers)
        }
    );
    body
}

// ---------------------------------------------------------------------------
// Overview (`/`)
// ---------------------------------------------------------------------------

/// How many Needs You items the Overview strip shows before it defers to
/// the full queue (`docs/agent-timeline-ui.md` §8.1: "zero to three").
const OVERVIEW_ATTENTION: usize = 3;

use crate::LIVE_WINDOW_MS;

/// The work unit the Overview is about: the one with the latest activity,
/// preferring open work.
fn current_work_unit(p: &Projection) -> Option<&WorkUnit> {
    p.work_units
        .iter()
        .max_by_key(|w| (w.status == WorkUnitStatus::Open, w.updated_at))
}

fn objective_or_reason(w: &WorkUnit, scope: &ScopeQuery) -> String {
    match (&w.objective, w.objective_event_id) {
        (Some(o), _) => format!("<span class=\"objective\">{}</span>", clip(o, 140)),
        (None, Some(e)) => format!(
            "<span class=\"muted\">prompt text not captured in this mode</span> {}",
            evidence_link(&e, scope)
        ),
        (None, None) => "<span class=\"muted\">no prompt observed</span>".to_string(),
    }
}

/// `12m ago`, or `just now` under a minute.
fn ago(then: Timestamp, now: Timestamp) -> String {
    let ms = elapsed_ms(then, now);
    if ms < 60_000 {
        "just now".to_string()
    } else {
        format!("{} ago", duration(ms))
    }
}

/// One Needs You item as a list entry. `compact` drops the explanation
/// details (the Overview strip); the full queue keeps them.
fn attention_item_html(it: &AttentionItem, scope: &ScopeQuery, compact: bool) -> String {
    let kind_badge = badge(
        match it.kind {
            AttentionKind::PermissionGate => "fail",
            AttentionKind::InputRequest => "warn",
            AttentionKind::RepeatedFailure => "sup",
            AttentionKind::WorkConflict => "live",
        },
        it.kind.as_str(),
    );
    let mut meta = vec![
        kind_badge,
        format!("<span class=\"muted\">{}</span>", esc(&it.project_name)),
    ];
    if let Some(p) = &it.provider {
        meta.push(esc(p.display_name()));
    }
    if let Some(s) = it.session_id {
        meta.push(session_link(&s, scope));
    }
    if let Some(w) = it.work_unit_id {
        meta.push(wu_link(&w, scope));
    }
    meta.push(format!(
        "<span class=\"waiting\">waiting {}</span>",
        duration(it.waiting_ms)
    ));
    meta.push(confidence(it.confidence));
    let mut s = format!(
        "<li class=\"atn rank-{}\" id=\"{}\"><p class=\"atn-action\">{}</p><p class=\"atn-meta\">{}</p>",
        it.rank,
        esc(&it.attention_id),
        esc(&it.action),
        meta.join(" · ")
    );
    if !compact {
        let _ = write!(
            s,
            "<details class=\"why\"><summary>why AttemptDB believes this</summary><p>{}</p><p class=\"muted small\">{}</p><p class=\"evidence\">evidence {} · inference <code>{}</code></p></details>",
            esc(&it.claim),
            esc(&it.uncertainty),
            evidence_links(&it.evidence, 6, scope),
            esc(it.algorithm_version.as_str())
        );
        let mut actions: Vec<String> = Vec::new();
        if let Some(sid) = it.session_id {
            actions.push(format!(
                "<a href=\"/session/{}{}\">Open session</a>",
                html::seg(&sid.to_string()),
                scope.without_session().query_string(&[])
            ));
            actions.push(format!(
                "<a href=\"/why{}\">Show why</a>",
                scope.without_session().query_string(&[("session", &sid.to_string())])
            ));
        }
        actions.push(format!(
            "<button type=\"button\" class=\"copy-brief\" data-brief=\"{}\">Copy continuation brief</button>",
            esc(&continuation_brief(it))
        ));
        let _ = write!(s, "<p class=\"row-actions\">{}</p>", actions.join(" "));
        if let Some(sid) = it.session_id {
            let _ = write!(
                s,
                "<p class=\"muted small\">wrong? the correction is a fact, not an edit: <code>attempt correct session {} --not-blocked --note \"…\"</code></p>",
                esc(&sid.to_string())
            );
        }
    } else {
        let _ = write!(
            s,
            "<p class=\"evidence\">{} · <a href=\"/attention{}#{}\">why</a></p>",
            evidence_links(&it.evidence, 3, scope),
            scope.without_session().query_string(&[]),
            esc(&it.attention_id)
        );
    }
    s.push_str("</li>");
    s
}

/// Plain text a person can paste into the agent that has to continue.
fn continuation_brief(it: &AttentionItem) -> String {
    let mut s = format!("{}\n\n{}\n", it.action, it.claim);
    if let Some(c) = &it.failure_class {
        let _ = write!(s, "failure class: {c}\n");
    }
    if let Some(sid) = it.session_id {
        let _ = write!(s, "session: {sid}\n");
    }
    if let Some(w) = it.work_unit_id {
        let _ = write!(s, "work unit: {w}\n");
    }
    let _ = write!(
        s,
        "waiting since: {}\nconfidence: {:.2} ({})\nuncertainty: {}\nevidence: {}\n",
        it.since,
        it.confidence,
        it.algorithm_version.as_str(),
        it.uncertainty,
        it.evidence
            .iter()
            .take(6)
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    );
    s
}

/// The Needs You strip: absent when nothing needs a person.
fn attention_strip(items: &[AttentionItem], scope: &ScopeQuery) -> String {
    if items.is_empty() {
        return String::new();
    }
    let shown = items.len().min(OVERVIEW_ATTENTION);
    let mut s = format!(
        "<section class=\"card attention\" id=\"needs-you\" data-live=\"attention\"><h2>Needs you {}</h2><ol class=\"atn-list\">",
        badge("fail", &items.len().to_string())
    );
    for it in items.iter().take(shown) {
        s.push_str(&attention_item_html(it, scope, true));
    }
    s.push_str("</ol>");
    let _ = write!(
        s,
        "<p><a href=\"/attention{}\">{}</a></p></section>",
        scope.without_session().query_string(&[]),
        if items.len() > shown {
            format!("all {} items, with the evidence", items.len())
        } else {
            "open the queue".to_string()
        }
    );
    s
}

/// The attempt path of one work unit: `failed → superseded → succeeded`.
fn attempt_chain(w: &WorkUnit, p: &Projection, scope: &ScopeQuery) -> String {
    let mut attempts: Vec<&Attempt> = w
        .attempts
        .iter()
        .filter_map(|id| p.attempts.iter().find(|a| a.attempt_id == *id))
        .collect();
    attempts.sort_by_key(|a| (a.started_at, a.attempt_id));
    if attempts.is_empty() {
        return "<p class=\"muted\">no attempt projected for this work unit yet</p>".to_string();
    }
    let mut s = String::from("<ol class=\"chain\">");
    for (i, a) in attempts.iter().enumerate() {
        if i > 0 {
            let arrow = if attempts[i - 1].superseded_by == Some(a.attempt_id) {
                "<li class=\"arrow supersedes\" title=\"superseded by\">⇒</li>"
            } else {
                "<li class=\"arrow\">→</li>"
            };
            s.push_str(arrow);
        }
        let _ = write!(
            s,
            "<li class=\"chip out-{}\">{} {}{}<span class=\"approach\">{}</span></li>",
            esc(a.outcome.as_str()),
            attempt_link(&a.attempt_id, scope),
            outcome_badge(a.outcome),
            a.failure_class
                .as_deref()
                .map(|c| format!(" {}", badge("class", c)))
                .unwrap_or_default(),
            clip(&a.approach, 60)
        );
    }
    s.push_str("</ol>");
    s
}

/// Active sessions as cards: current turn, in-flight tool, silence.
fn live_execution(
    p: &Projection,
    snap: &attemptdb_project::ProjectStateSnapshot,
    now: Timestamp,
    scope: &ScopeQuery,
) -> String {
    let mut open: Vec<_> = snap.sessions.iter().filter(|s| s.open).collect();
    open.sort_by_key(|s| std::cmp::Reverse(s.last_activity_at));
    // "Open" is not the same as "live": a session whose provider never sent
    // an end event stays open forever. Only recent activity goes in the
    // grid; the rest is counted honestly below it.
    let (active, quiet): (Vec<&attemptdb_project::SessionState>, Vec<&attemptdb_project::SessionState>) = open
        .iter()
        .copied()
        .partition(|s| elapsed_ms(s.last_activity_at, now) <= LIVE_WINDOW_MS);
    let mut s = String::from(
        "<section class=\"card\" id=\"live-execution\" data-live=\"overview\"><h2>Live execution</h2>",
    );
    if active.is_empty() {
        let _ = write!(
            s,
            "<p class=\"muted\">Nothing has run in the last {}. {}</p>",
            duration(LIVE_WINDOW_MS),
            match quiet.first() {
                Some(q) => format!(
                    "{} session(s) are still open — the newest was last active {}. The work below is the most recent state, not a live one.",
                    quiet.len(),
                    ago(q.last_activity_at, now)
                ),
                None => "Every observed session has ended.".to_string(),
            }
        );
        s.push_str("</section>");
        return s;
    }
    s.push_str("<div class=\"live-grid\">");
    for st in &active {
        let session = p.session(st.session_id);
        let in_flight: Vec<String> = st
            .in_flight_tool_calls
            .iter()
            .filter_map(|id| p.tool_calls.iter().find(|c| &c.tool_call_id == id))
            .map(|tc| {
                format!(
                    "<code>{}</code>{}",
                    esc(&tc.tool.name),
                    tc.started_at
                        .map(|t| format!(" <span class=\"muted\">{}</span>", ago(t, now)))
                        .unwrap_or_default()
                )
            })
            .collect();
        let _ = write!(
            s,
            "<article class=\"live-session\"><header><span class=\"provider\">{}</span> {} <span class=\"project\">{}</span></header><p>{} · {}</p><p class=\"muted small\">last event {} · {}</p><p>{}</p></article>",
            esc(session.map(|x| x.provider.display_name()).unwrap_or("?")),
            session_link(&st.session_id, scope),
            esc(&session.map(|x| clip(&x.project_name, 40)).unwrap_or_default()),
            match (st.turn_index, st.turn_status) {
                (Some(i), Some(t)) => format!("turn {i} {}", turn_badge(t)),
                _ => "<span class=\"muted\">no turn yet</span>".to_string(),
            },
            if in_flight.is_empty() {
                "<span class=\"muted\">no tool in flight</span>".to_string()
            } else {
                format!("running {}", in_flight.join(", "))
            },
            ago(st.last_activity_at, now),
            ts_time(st.last_activity_at),
            match (st.last_attempt, st.last_attempt_outcome) {
                (Some(id), Some(o)) => format!(
                    "last attempt {} {}{}",
                    attempt_link(&id, scope),
                    outcome_badge(o),
                    st.last_failure_class
                        .as_deref()
                        .map(|c| format!(" {}", badge("class", c)))
                        .unwrap_or_default()
                ),
                _ => "<span class=\"muted\">no attempt yet</span>".to_string(),
            }
        );
    }
    s.push_str("</div>");
    if !quiet.is_empty() {
        let _ = write!(
            s,
            "<p class=\"muted small\">{} further open session(s) with no activity in the last {} — a session stays open until its provider sends an end event, so this is a gap in what was captured, not proof that an agent is running.</p>",
            quiet.len(),
            duration(LIVE_WINDOW_MS)
        );
    }
    s.push_str("</section>");
    s
}

/// Paths, commits and handoffs the recent work produced.
fn produced_card(p: &Projection, scope: &ScopeQuery) -> String {
    let mut attempts: Vec<&Attempt> = p.attempts.iter().collect();
    attempts.sort_by_key(|a| std::cmp::Reverse(a.started_at));
    let mut paths: Vec<&str> = Vec::new();
    for a in attempts.iter().take(40) {
        for path in &a.paths {
            if !paths.contains(&path.as_str()) && paths.len() < 24 {
                paths.push(path);
            }
        }
    }
    let mut commits: Vec<&attemptdb_project::Commit> = p.commits.iter().collect();
    commits.sort_by_key(|c| std::cmp::Reverse(c.at));
    let mut s = String::from("<section class=\"card\"><h2>What the work produced</h2>");
    if paths.is_empty() && commits.is_empty() {
        s.push_str("<p class=\"muted\">no paths or commits observed in scope</p></section>");
        return s;
    }
    let _ = write!(
        s,
        "<p class=\"paths\"><span class=\"k muted\">changed paths</span> {}</p>",
        if paths.is_empty() {
            "<span class=\"muted\">none</span>".to_string()
        } else {
            paths
                .iter()
                .map(|p| format!("<code class=\"path\">{}</code>", esc(p)))
                .collect::<Vec<_>>()
                .join(" ")
        }
    );
    if !commits.is_empty() {
        let rows: Vec<Vec<String>> = commits
            .iter()
            .take(8)
            .map(|c| {
                vec![
                    ts(c.at),
                    c.sha
                        .as_deref()
                        .map(|s| format!("<code>{}</code>", esc(&s[..s.len().min(10)])))
                        .unwrap_or_else(|| "<span class=\"muted\">unresolved</span>".to_string()),
                    c.branch.as_deref().map(esc).unwrap_or_default(),
                    c.attempt_id
                        .map(|a| attempt_link(&a, scope))
                        .unwrap_or_default(),
                    format!(
                        "<span class=\"muted small\">{}</span> {}",
                        esc(&c.linkage),
                        confidence(c.confidence)
                    ),
                ]
            })
            .collect();
        s.push_str(&table(
            &["committed", "sha", "branch", "attempt", "linkage"],
            &rows,
        ));
    }
    s.push_str("</section>");
    s
}

/// Sharing, sanitized by default: the summary image is content-free by
/// construction, and the full replay says what it would include.
fn share_card(scope: &ScopeQuery) -> String {
    format!(
        "<section class=\"card\"><h2>Share this</h2><p class=\"row-actions\"><a class=\"cta\" href=\"/card.svg{qs}\">Summary card (SVG)</a> <span class=\"muted small\">1200×630 for a README, an issue or a social preview — outcomes, failure classes, counts and repository-relative paths only; no prompt, command or tool output</span></p><p class=\"muted small\">the same card from the command line: <code>attempt ui export card.svg</code> · a full sanitized replay: <code>attempt ui export --sanitized timeline.html</code> (without <code>--sanitized</code> the replay contains prompt text and full paths — review it before sharing)</p></section>",
        qs = scope.without_session().query_string(&[])
    )
}

/// The first-run screen (`docs/agent-timeline-ui.md` §9.1): three steps and
/// a way to see the product without waiting for an event.
fn first_run(v: &View, scope: &ScopeQuery) -> String {
    let st = &v.status;
    let step = |ok: bool, head: &str, detail: String| {
        format!(
            "<li class=\"step {}\">{} <b>{}</b><br><span class=\"muted small\">{}</span></li>",
            if ok { "done" } else { "waiting" },
            badge(if ok { "ok" } else { "muted" }, if ok { "done" } else { "waiting" }),
            esc(head),
            detail
        )
    };
    let providers: String = if st.providers.is_empty() {
        "no provider has sent an event yet — run <code>attempt doctor</code> to see which agents are wired".to_string()
    } else {
        st.providers
            .iter()
            .map(|p| format!("<code>{}</code> {} events", esc(&p.provider), p.events))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    format!(
        "<section class=\"card first-run\"><h1>AttemptDB is running. Nothing has been captured yet.</h1><ol class=\"steps\">{}{}{}</ol><p class=\"row-actions\"><a class=\"cta\" href=\"/{}\">Open the AttemptDB build-history demo</a> <a href=\"/query{}\">Run a query anyway</a></p><p class=\"muted small\">the demo is a synthesized, clearly labelled dataset; it never mixes with your database</p></section>",
        step(
            true,
            "Database created",
            format!("<code>{}</code>", esc(&st.source))
        ),
        step(
            !st.providers.is_empty(),
            "Agents detected and hooks installed",
            providers
        ),
        step(
            false,
            "Waiting for the first real event",
            "work normally with a supported coding agent; this page updates on its own".to_string()
        ),
        scope.without_session().query_string(&[("demo", "1")]),
        scope.query_string(&[]),
    )
}

pub async fn now(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> PageResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let scope = &scope;
    let p = v.engine.projection();
    let now = Timestamp::now();

    if v.engine.event_count() == 0 && v.status.events == 0 {
        let body = first_run(&v, scope);
        return Ok(Html(layout(&v, scope, "Overview", "/", &body)));
    }

    let snap = p.state_at(now);
    let attention = p.attention_at(now, attemptdb_project::DEFAULT_MIN_CONFIDENCE);
    let label = v
        .scope
        .project_name
        .clone()
        .unwrap_or_else(|| "all projects".to_string());
    let current = current_work_unit(p);
    let mut body = String::new();

    // 1. Current project state.
    let active_agents: Vec<String> = snap
        .sessions
        .iter()
        .filter(|s| s.open && elapsed_ms(s.last_activity_at, now) <= LIVE_WINDOW_MS)
        .map(|s| {
            format!(
                "{} {}",
                esc(p.session(s.session_id)
                    .map(|x| x.provider.display_name())
                    .unwrap_or("?")),
                session_link(&s.session_id, scope)
            )
        })
        .collect();
    let last_activity = snap
        .sessions
        .iter()
        .map(|s| s.last_activity_at)
        .max()
        .or_else(|| p.sessions.iter().map(|s| s.last_event_at).max());
    let scoped = v.scoped_capture();
    let cells = [
        (
            "current work",
            match current {
                Some(w) => format!(
                    "{} {} {}<br>{}",
                    wu_link(&w.work_unit_id, scope),
                    phase_badge(w.phase),
                    wu_status_badge(w.status),
                    objective_or_reason(w, scope)
                ),
                None => "<span class=\"muted\">no work unit projected in scope</span>".to_string(),
            },
        ),
        (
            "active agents",
            if active_agents.is_empty() {
                "<span class=\"muted\">none right now</span>".to_string()
            } else {
                active_agents.join("<br>")
            },
        ),
        (
            "last meaningful activity",
            match last_activity {
                Some(t) => format!("{} <span class=\"muted\">{}</span>", ago(t, now), ts(t)),
                None => "<span class=\"muted\">nothing observed</span>".to_string(),
            },
        ),
        (
            "evidence coverage",
            format!(
                "{} hook-captured · {} reconstructed{}",
                scoped.captured,
                scoped.reconstructed,
                current
                    .map(|w| format!(" · work unit {}", confidence(w.confidence)))
                    .unwrap_or_default()
            ),
        ),
    ];
    let _ = write!(
        body,
        "<section class=\"card hero\"><h1>What is <b>{}</b> doing?</h1><p class=\"muted small\">as of {} · <code>WHAT IS project DOING NOW</code> · {}</p><div class=\"hero-grid\">{}</div></section>",
        esc(&label),
        ts(now),
        esc(crate::TAGLINE),
        cells
            .iter()
            .map(|(k, val)| format!(
                "<div class=\"hero-cell\"><span class=\"k\">{}</span><div>{}</div></div>",
                esc(k),
                val
            ))
            .collect::<String>()
    );

    // 2. Needs You.
    body.push_str(&attention_strip(&attention, scope));

    // 3. Live execution.
    body.push_str(&live_execution(p, &snap, now, scope));

    // 4. The attempt path.
    if let Some(w) = current {
        let _ = write!(
            body,
            "<section class=\"card\"><h2>Attempt path</h2><p class=\"muted small\">the current work unit's attempts in order — <span class=\"arrow\">⇒</span> is a supersession; every attempt is an inference with evidence ({})</p>{}<p><a href=\"/work{}\">the whole work board</a> · <a href=\"/timeline{}\">the full timeline</a></p></section>",
            esc(crate::INFERENCE_VERSION),
            attempt_chain(w, p, scope),
            scope.without_session().query_string(&[]),
            scope.query_string(&[]),
        );
    }

    // Below the fold.
    body.push_str(&work_units_card(p, scope, true, 12));
    body.push_str(&decisions_card(p, scope, 8));
    body.push_str(&produced_card(p, scope));
    body.push_str(&handoffs_table(p, scope, 8));
    body.push_str(&share_card(scope));
    body.push_str(&coverage_card(&v, p, scope));
    Ok(Html(layout(&v, scope, "Overview", "/", &body)))
}

pub async fn timeline(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> PageResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let limit = param_usize(&q, "limit", api::DEFAULT_SESSIONS);
    let page = param_usize(&q, "page", 1);
    let include_empty = param_flag(&q, "all_sessions");
    let all = j::sessions_sorted(p, include_empty);
    let total = all.len();
    let mut body = String::new();
    let _ = write!(
        body,
        "<section class=\"card\"><h1>Timeline</h1><p class=\"muted small\">{} · {} · {} · {} from {} events · inference <code>{}</code> · <a href=\"/timeline{}\">{}</a></p>",
        plural(p.sessions.len(), "session"),
        plural(p.turns.len(), "turn"),
        plural(p.attempts.len(), "attempt"),
        plural(p.handoffs.len(), "handoff"),
        v.engine.event_count(),
        esc(crate::INFERENCE_VERSION),
        scope.query_string(&[("all_sessions", if include_empty { "" } else { "1" })]),
        if include_empty {
            "hide empty sessions"
        } else {
            "include sessions without prompts or tool calls"
        }
    );
    if all.is_empty() {
        body.push_str("<p>no sessions yet. Work with a coding agent whose hooks are installed, then reload; check wiring with <code>attempt doctor</code>.</p></section>");
        return Ok(Html(layout(&v, &scope, "Timeline", "/timeline", &body)));
    }
    body.push_str("</section>");
    for s in all.iter().skip((page - 1) * limit).take(limit) {
        let _ = write!(
            body,
            "<details class=\"session card\" open><summary>{}</summary><ol class=\"turns\">",
            session_header(s, &v, &scope)
        );
        let turns = j::turns_of(p, s);
        if turns.is_empty() {
            body.push_str(
                "<li class=\"muted\">no turns projected (no prompts or tool calls observed)</li>",
            );
        }
        for t in turns {
            body.push_str(&turn_block(t, p, &scope));
        }
        body.push_str("</ol></details>");
    }
    let pages = total.div_ceil(limit).max(1);
    let mut pager = String::from("<nav class=\"pager\">");
    if page > 1 {
        let _ = write!(
            pager,
            "<a href=\"/timeline{}\">← newer</a>",
            scope.query_string(&[
                ("page", &(page - 1).to_string()),
                ("limit", &limit.to_string()),
                ("all_sessions", if include_empty { "1" } else { "" })
            ])
        );
    }
    let _ = write!(
        pager,
        " <span>page {page} of {pages} · {} per page</span> ",
        limit
    );
    if page < pages {
        let _ = write!(
            pager,
            "<a href=\"/timeline{}\">older →</a>",
            scope.query_string(&[
                ("page", &(page + 1).to_string()),
                ("limit", &limit.to_string()),
                ("all_sessions", if include_empty { "1" } else { "" })
            ])
        );
    }
    pager.push_str("</nav>");
    body.push_str(&pager);
    body.push_str(&work_units_card(p, &scope, false, 50));
    body.push_str(&decisions_card(p, &scope, 50));
    if !p.handoffs.is_empty() {
        let _ = write!(
            body,
            "<section class=\"card\"><h2>Handoffs</h2>{}</section>",
            handoffs_table(p, &scope, 20)
        );
    }
    Ok(Html(layout(&v, &scope, "Timeline", "/timeline", &body)))
}

fn waterfall(s: &Session, p: &Projection, scope: &ScopeQuery) -> String {
    let t0 = s.started_at;
    let mut t1 = s.last_event_at.max(s.ended_at.unwrap_or(s.last_event_at));
    for c in p.tool_calls_of(s.session_id) {
        if let Some(f) = c.finished_at {
            t1 = t1.max(f);
        }
    }
    let span = (t1.as_micros() - t0.as_micros()).max(1_000) as f64;
    let pct = |t: Timestamp| {
        ((t.as_micros() - t0.as_micros()).max(0) as f64 / span * 100.0).clamp(0.0, 100.0)
    };
    let bar = |start: Timestamp, end: Option<Timestamp>, class: &str, label: &str, title: &str| {
        let left = pct(start);
        let right = end.map(pct).unwrap_or(100.0);
        let width = (right - left).max(0.4);
        format!(
            "<div class=\"bar {}\" style=\"left:{left:.2}%;width:{width:.2}%\" title=\"{}\"><span>{}</span></div>",
            esc(class),
            esc(title),
            label
        )
    };
    let mut out = String::from("<div class=\"waterfall\"><div class=\"axis\">");
    for i in 0..=4 {
        let t = Timestamp::from_micros(t0.as_micros() + (span * i as f64 / 4.0) as i64);
        let _ = write!(
            out,
            "<span style=\"left:{:.1}%\">{}</span>",
            i as f64 * 25.0,
            ts_time(t)
        );
    }
    out.push_str("</div>");
    for t in j::turns_of(p, s) {
        let label = format!(
            "turn {} {}",
            t.index,
            objective_html(&t.objective, t.prompt_chars, t.index, 60)
        );
        let _ = write!(
            out,
            "<div class=\"row turn-row\"><div class=\"label\">{} {}</div><div class=\"track\">{}</div></div>",
            label,
            turn_badge(t.status),
            bar(
                t.started_at,
                t.ended_at,
                &format!("turn-{}", t.status.as_str()),
                &format!(
                    "{} → {}",
                    ts_time(t.started_at),
                    t.ended_at.map(ts_time).unwrap_or_else(|| "open".into())
                ),
                &format!("turn {} {}", t.index, t.status.as_str())
            )
        );
        let calls: Vec<&ToolCall> = t
            .tool_call_ids
            .iter()
            .filter_map(|id| p.tool_calls.iter().find(|c| &c.tool_call_id == id))
            .collect();
        for tc in calls {
            let status = tc.outcome.as_ref().map(|o| o.status.as_str()).unwrap_or(
                if tc.finished_at.is_none() {
                    "in_flight"
                } else {
                    "unknown"
                },
            );
            let path = tc
                .paths
                .first()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let text = format!(
                "<code>{}</code> {} <span class=\"muted\">{}</span>",
                esc(&tc.tool.name),
                clip(&path, 40),
                tool_call_duration(tc)
            );
            let _ = write!(
                out,
                "<div class=\"row call-row\" id=\"{}\"><div class=\"label\">{} {} {}</div><div class=\"track\">{}</div></div>",
                esc(&format!("spn_{}", tc.tool_call_id)),
                text,
                tc.start_event_id
                    .map(|e| evidence_link(&e, scope))
                    .unwrap_or_default(),
                tc.end_event_id
                    .map(|e| evidence_link(&e, scope))
                    .unwrap_or_default(),
                bar(
                    tc.started_at.unwrap_or(t.started_at),
                    tc.finished_at,
                    &format!("call-{status}"),
                    "",
                    &format!("{} {} {}", tc.tool.name, status, tool_call_duration(tc))
                )
            );
        }
    }
    out.push_str("</div>");
    out
}

pub async fn session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> PageResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let s = find_session(p, &id)?;
    let caps = v
        .session_capture
        .get(&s.session_id)
        .copied()
        .unwrap_or_default();
    let mut body = String::new();
    let _ = write!(
        body,
        "<section class=\"card\"><h1>Session {} <span class=\"muted\">{}</span></h1>",
        html::short_id(&s.session_id),
        esc(s.provider.display_name())
    );
    let agents = if s.agents.is_empty() {
        "<span class=\"muted\">none recorded</span>".to_string()
    } else {
        s.agents
            .iter()
            .map(|a| format!("<code>{}</code>", esc(&html::short_id(a))))
            .collect::<Vec<_>>()
            .join(" ")
    };
    body.push_str(&key_values(&[
        (
            "session id",
            format!("<code>{}</code>", esc(&html::id(&s.session_id))),
        ),
        (
            "provider session id",
            format!("<code>{}</code>", esc(&s.provider_session_id)),
        ),
        (
            "project",
            format!(
                "{} <code>{}</code>",
                esc(&s.project_name),
                esc(&html::id(&s.project_id))
            ),
        ),
        (
            "span",
            format!(
                "{} → {}",
                ts(s.started_at),
                s.ended_at
                    .map(ts)
                    .unwrap_or_else(|| "open (no session end observed)".into())
            ),
        ),
        (
            "coverage",
            format!(
                "{} {}",
                coverage_badge(s.coverage),
                esc(&crate::export::coverage_note(s))
            ),
        ),
        (
            "counts",
            format!(
                "{} · {} · {} · {} · {} events ({} hook-captured, {} reconstructed)",
                plural(s.turn_count as usize, "turn"),
                plural(s.prompt_count as usize, "prompt"),
                plural(s.tool_call_count as usize, "tool call"),
                plural(s.failure_count as usize, "failure"),
                s.event_count,
                caps.captured,
                caps.reconstructed
            ),
        ),
        ("agents", agents),
        (
            "start / end",
            format!(
                "{} {} · {}",
                s.start_source.as_deref().map(esc).unwrap_or_default(),
                s.start_event_id
                    .map(|e| evidence_link(&e, &scope))
                    .unwrap_or_else(|| "<span class=\"muted\">no start</span>".into()),
                s.end_event_id
                    .map(|e| format!(
                        "{}{}",
                        s.end_reason
                            .as_deref()
                            .map(|r| format!("{} ", esc(r)))
                            .unwrap_or_default(),
                        evidence_link(&e, &scope)
                    ))
                    .unwrap_or_else(|| "<span class=\"muted\">no end</span>".into())
            ),
        ),
        (
            "first / last event",
            format!(
                "{} · {} at {}",
                evidence_link(&s.first_event_id, &scope),
                evidence_link(&s.last_event_id, &scope),
                ts(s.last_event_at)
            ),
        ),
    ]));
    match p.why_blocked(s.session_id) {
        Some(e) => {
            let _ = write!(
                body,
                "<div class=\"callout fail\"><b>Blocked:</b> {} {} <div class=\"muted small\">{}</div><div class=\"evidence\">evidence: {}</div></div>",
                esc(&e.claim),
                confidence(e.confidence),
                esc(&e.uncertainty),
                evidence_links(&e.evidence, 6, &scope)
            );
        }
        None => body.push_str("<p class=\"muted small\">not blocked: no uncleared input signal, no repeated identical failure</p>"),
    }
    for h in p
        .handoffs
        .iter()
        .filter(|h| h.from_session == s.session_id || h.to_session == s.session_id)
    {
        let _ = write!(
            body,
            "<p>⇄ handoff {} {} → {} {} at {} after {} gap · {} · {}</p>",
            esc(h.from_provider.display_name()),
            session_link(&h.from_session, &scope),
            esc(h.to_provider.display_name()),
            session_link(&h.to_session, &scope),
            ts(h.at),
            duration(h.gap_ms),
            confidence(h.confidence),
            evidence_links(&h.evidence, 4, &scope)
        );
    }
    body.push_str("</section>");
    let _ = write!(
        body,
        "<section class=\"card\"><h2>Waterfall</h2><p class=\"muted small\">turns and tool calls by time; bars are colored by outcome, an open end means no end event was observed</p>{}</section>",
        waterfall(s, p, &scope)
    );
    body.push_str("<section class=\"card\"><h2>Turns and attempts</h2><ol class=\"turns\">");
    let turns = j::turns_of(p, s);
    if turns.is_empty() {
        body.push_str("<li class=\"muted\">no turns projected</li>");
    }
    for t in turns {
        body.push_str(&turn_block(t, p, &scope));
    }
    body.push_str("</ol></section>");
    let signals: Vec<_> = p.signals_of(s.session_id).collect();
    if !signals.is_empty() {
        body.push_str("<section class=\"card\"><h2>Input signals</h2><ul>");
        for g in signals {
            let _ = write!(
                body,
                "<li>{} {}{} at {} · {} · {}</li>",
                if g.cleared_at.is_none() {
                    badge("warn", "pending")
                } else {
                    badge("ok", "cleared")
                },
                esc(g.kind.as_str()),
                g.signal_type
                    .as_deref()
                    .map(|t| format!(" ({})", esc(t)))
                    .unwrap_or_default(),
                ts(g.at),
                g.cleared_at
                    .map(|c| format!("cleared {}", ts(c)))
                    .unwrap_or_else(|| "no later event".into()),
                evidence_link(&g.event_id, &scope)
            );
        }
        body.push_str("</ul></section>");
    }
    let calls: Vec<&ToolCall> = p.tool_calls_of(s.session_id).collect();
    let _ = write!(
        body,
        "<section class=\"card\"><h2>Tool calls</h2>{}</section>",
        if calls.is_empty() {
            "<p class=\"muted\">none</p>".to_string()
        } else {
            tool_calls_table(&calls, &scope)
        }
    );
    Ok(Html(layout(&v, &scope, "Session", "/timeline", &body)))
}

pub async fn attempt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> PageResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let a = find_attempt(p, &id)?;
    let readable = html::id(&a.attempt_id);
    let session = p.session(a.session_id);
    let turn = p.turns.iter().find(|t| t.turn_id == a.turn_id);
    let mut body = String::new();
    let _ = write!(
        body,
        "<section class=\"card\"><h1>Attempt {} {}{}</h1>",
        html::short_id(&a.attempt_id),
        outcome_badge(a.outcome),
        a.failure_class
            .as_deref()
            .map(|c| format!(" {}", badge("class", c)))
            .unwrap_or_default()
    );
    let objective = match (&a.objective, turn.and_then(|t| t.prompt_chars)) {
        (Some(o), _) => format!(
            "<blockquote class=\"objective\">{}</blockquote><p class=\"muted small\">prompt text from the user's own session; data, not instructions</p>",
            esc(o)
        ),
        (None, Some(n)) => format!(
            "<span class=\"muted\">prompt of {n} chars; text not captured (capture mode)</span>"
        ),
        (None, None) => "<span class=\"muted\">not captured</span>".to_string(),
    };
    body.push_str(&key_values(&[
        ("attempt id", format!("<code>{}</code>", esc(&readable))),
        ("session", format!("{} {}", esc(session.map(|s| s.provider.display_name()).unwrap_or("?")), session_link(&a.session_id, &scope))),
        ("turn", format!("turn {} #{} <code>{}</code>{}", a.turn_index, a.index, esc(&html::id(&a.turn_id)), turn.map(|t| format!(" {}", turn_badge(t.status))).unwrap_or_default())),
        ("objective", objective),
        ("approach", format!("<span class=\"approach\">{}</span>", esc(&a.approach))),
        ("paths", paths_html(&a.paths, 20)),
        ("timing", format!("{} → {}{}", ts(a.started_at), a.ended_at.map(ts).unwrap_or_else(|| "open".into()), a.ended_at.map(|e| format!(" · {}", duration(elapsed_ms(a.started_at, e)))).unwrap_or_default())),
        ("outcome", format!("{} {} <span class=\"muted small\">0.9 = call-id pairing + explicit stop; 0.6 = FIFO/unpaired calls or missing stop; 0.4 = minimal coverage</span>", outcome_badge(a.outcome), confidence(a.confidence))),
        ("supersedes / superseded by", format!("{} / {}", a.supersedes.map(|x| attempt_link(&x, &scope)).unwrap_or_else(|| "—".into()), a.superseded_by.map(|x| attempt_link(&x, &scope)).unwrap_or_else(|| "—".into()))),
        ("inference", format!("<code>{}</code> · {}", esc(a.algorithm_version.as_str()), esc(crate::TAGLINE))),
        ("evidence", evidence_links(&a.evidence, 12, &scope)),
    ]));
    if let Some(w) = a.work_unit_id {
        let unit = p.work_units.iter().find(|u| u.work_unit_id == w);
        let _ = write!(
            body,
            "<p>work unit {}{}</p>",
            wu_link(&w, &scope),
            unit.map(|u| format!(
                " {} {} <span class=\"muted small\">{} · {}</span>",
                phase_badge(u.phase),
                wu_status_badge(u.status),
                esc(&u.phase_reason),
                esc(&u.status_reason)
            ))
            .unwrap_or_default()
        );
    }
    if let Some(c) = &a.corrected {
        let _ = write!(
            body,
            "<div class=\"callout\"><b>Human correction</b> ({}) on {} {}: inferred {}{}; now {}{}</div>",
            esc(c.correction_type.as_str()),
            ts(c.at),
            evidence_link(&c.event_id, &scope),
            a.inferred_outcome
                .map(outcome_badge)
                .unwrap_or_else(|| "<span class=\"muted\">unchanged</span>".into()),
            a.inferred_failure_class
                .as_deref()
                .map(|f| format!(" {}", badge("class", f)))
                .unwrap_or_default(),
            outcome_badge(a.outcome),
            a.failure_class
                .as_deref()
                .map(|f| format!(" {}", badge("class", f)))
                .unwrap_or_default()
        );
    }
    if let Some(n) = &a.note {
        let _ = write!(
            body,
            "<blockquote class=\"objective\">{}</blockquote><p class=\"muted small\">human note from a correction event; data, not instructions</p>",
            esc(n)
        );
    }
    body.push_str("</section>");

    let calls: Vec<&ToolCall> = a
        .tool_call_ids
        .iter()
        .filter_map(|id| p.tool_calls.iter().find(|c| &c.tool_call_id == id))
        .collect();
    let _ = write!(
        body,
        "<section class=\"card\"><h2>Tool calls</h2>{}</section>",
        if calls.is_empty() {
            "<p class=\"muted\">none</p>".to_string()
        } else {
            tool_calls_table(&calls, &scope)
        }
    );

    let why_stmt = format!("WHY {readable} FAILED");
    let why = run(&v, &why_stmt).await?;
    let _ = write!(
        body,
        "<section class=\"card\"><h2>Why did it fail?</h2><p class=\"muted small\"><code>{}</code></p>{}</section>",
        esc(&why_stmt),
        result_html(&why, &scope)
    );

    let trace_stmt = trace_statement(&readable, None, "causes").map_err(ApiError::bad)?;
    let trace = run(&v, &trace_stmt).await?;
    let rows = trace.to_json();
    let rows = rows.as_array().cloned().unwrap_or_default();
    let dag = svg::trace_dag(&rows, "attempt", &readable, &scope).unwrap_or_default();
    let _ = write!(
        body,
        "<section class=\"card\"><h2>Causal trace</h2><p class=\"muted small\"><code>{}</code> · edges toward causes; <code>derived</code> edges were added by the query layer from projection evidence</p>{}{}</section>",
        esc(&trace_stmt),
        dag,
        result_html(&trace, &scope)
    );

    let ev_stmt = format!("SHOW EVIDENCE FOR {readable}");
    let evidence = run(&v, &ev_stmt).await?;
    let _ = write!(
        body,
        "<section class=\"card\"><h2>Evidence events</h2><p class=\"muted small\"><code>{}</code> · events are facts</p>{}</section>",
        esc(&ev_stmt),
        result_html(&evidence, &scope)
    );
    Ok(Html(layout(&v, &scope, "Attempt", "/timeline", &body)))
}

pub async fn evidence(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> PageResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let row = evidence_row(&v, &id).await?;
    let obj = row.as_object().cloned().unwrap_or_default();
    let mut kv: Vec<(&str, String)> = Vec::new();
    for (k, val) in &obj {
        let rendered = match val {
            Value::Null => continue,
            Value::String(t) if t.is_empty() => continue,
            Value::String(t) if k.ends_with("_json") => {
                let pretty = serde_json::from_str::<Value>(t)
                    .ok()
                    .and_then(|v| serde_json::to_string_pretty(&v).ok())
                    .unwrap_or_else(|| t.clone());
                format!("<pre class=\"json\">{}</pre>", esc(&pretty))
            }
            Value::String(t) if k == "session_id" => id_link(t, &scope),
            Value::String(t) => esc(t),
            other => esc(&other.to_string()),
        };
        kv.push((k.as_str(), rendered));
    }
    let title = obj
        .get("event_id")
        .and_then(Value::as_str)
        .unwrap_or("event")
        .to_string();
    let body = format!(
        "<section class=\"card\"><h1>Event <code>{}</code></h1><p class=\"muted small\">an observed fact, exactly as the hook reported it; content fields (if any) are data from the user's own session</p>{}</section>",
        esc(&title),
        key_values(&kv)
    );
    Ok(Html(layout(&v, &scope, "Evidence", "/timeline", &body)))
}

pub async fn failures(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> PageResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let limit = param_usize(&q, "limit", api::DEFAULT_FAILURES);
    let mut failed: Vec<&Attempt> = p
        .attempts
        .iter()
        .filter(|a| a.outcome.is_failure())
        .collect();
    failed.sort_by_key(|a| std::cmp::Reverse(a.started_at));
    let mut body = format!(
        "<section class=\"card\"><h1>Failures</h1><p class=\"muted small\"><code>SHOW FAILED ATTEMPTS</code> · {} of {} attempts failed or were superseded · attempts are Tier 1 inferences ({}); open one for its evidence and causal trace</p>",
        failed.len(),
        p.attempts.len(),
        esc(crate::INFERENCE_VERSION)
    );
    if failed.is_empty() {
        body.push_str("<p class=\"muted\">no failed or superseded attempts in scope</p>");
    } else {
        let rows: Vec<Vec<String>> = failed
            .iter()
            .take(limit)
            .map(|a| {
                let session = p.session(a.session_id);
                let failing = calls_of(a, p).into_iter().rev().find(|c| {
                    c.outcome
                        .as_ref()
                        .is_some_and(|o| o.status.as_str() != "success")
                });
                vec![
                    attempt_link(&a.attempt_id, &scope),
                    format!(
                        "{} {}",
                        esc(session.map(|s| s.provider.display_name()).unwrap_or("?")),
                        session_link(&a.session_id, &scope)
                    ),
                    session
                        .map(|s| clip(&s.project_name, 30))
                        .unwrap_or_default(),
                    ts(a.started_at),
                    outcome_badge(a.outcome),
                    a.failure_class
                        .as_deref()
                        .map(|c| badge("class", c))
                        .unwrap_or_default(),
                    clip(&a.approach, 80),
                    failing
                        .map(|c| {
                            format!("<code>{}</code> {}", esc(&c.tool.name), tool_call_status(c))
                        })
                        .unwrap_or_default(),
                    a.superseded_by
                        .map(|x| attempt_link(&x, &scope))
                        .unwrap_or_else(|| "<span class=\"muted\">not retried</span>".into()),
                    confidence(a.confidence),
                    evidence_links(&a.evidence, 3, &scope),
                ]
            })
            .collect();
        body.push_str(&table(
            &[
                "attempt",
                "session",
                "project",
                "started",
                "outcome",
                "failure class",
                "approach",
                "failing call",
                "retried by",
                "confidence",
                "evidence",
            ],
            &rows,
        ));
        if failed.len() > limit {
            let _ = write!(
                body,
                "<p><a href=\"/failures{}\">show all {}</a></p>",
                scope.query_string(&[("limit", &failed.len().to_string())]),
                failed.len()
            );
        }
    }
    body.push_str("</section>");
    Ok(Html(layout(&v, &scope, "Failures", "/failures", &body)))
}

fn calls_of<'a>(a: &Attempt, p: &'a Projection) -> Vec<&'a ToolCall> {
    a.tool_call_ids
        .iter()
        .filter_map(|id| p.tool_calls.iter().find(|c| &c.tool_call_id == id))
        .collect()
}

pub async fn handoffs(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> PageResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let limit = param_usize(&q, "limit", api::DEFAULT_FAILURES);
    let body = format!(
        "<section class=\"card\"><h1>Handoffs</h1><p class=\"muted small\"><code>SHOW HANDOFFS</code> · work moving between agents: a session of another provider starting shortly after one went idle in the same project ({}); 0.8 when they share a path within 30 minutes, 0.5 when the receiving session merely starts within 5 minutes</p>{}</section>",
        esc(crate::INFERENCE_VERSION),
        handoffs_table(p, &scope, limit)
    );
    Ok(Html(layout(&v, &scope, "Handoffs", "/handoffs", &body)))
}

pub async fn why(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> PageResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let subject = q.get("subject").cloned().unwrap_or_default();
    let statement = why_statement(&subject).map_err(ApiError::bad)?;
    let r = run(&v, &statement).await?;
    let mut body = format!(
        "<section class=\"card\"><h1>Why?</h1><form method=\"get\" action=\"/why\" class=\"inline\">{}<label>subject <input name=\"subject\" value=\"{}\" placeholder=\"project, ses_…, att_…\" size=\"44\"></label> <button type=\"submit\">Explain</button></form><p class=\"muted small\"><code>{}</code> · blocked = an uncleared pending-input signal, or the last two attempts failed the same way ({}); an empty answer means nothing looks blocked</p>{}</section>",
        scope
            .pairs()
            .iter()
            .map(|(k, val)| format!(
                "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
                esc(k),
                esc(val)
            ))
            .collect::<String>(),
        esc(&subject),
        esc(&statement),
        esc(crate::INFERENCE_VERSION),
        result_html(&r, &scope)
    );
    let blocked: Vec<(&Session, attemptdb_project::Explanation)> = p
        .sessions
        .iter()
        .filter_map(|s| p.why_blocked(s.session_id).map(|e| (s, e)))
        .collect();
    body.push_str("<section class=\"card\"><h2>Sessions that look blocked now</h2>");
    if blocked.is_empty() {
        body.push_str("<p class=\"muted\">none of the sessions in scope looks blocked as of its latest event</p>");
    } else {
        body.push_str("<ul>");
        for (s, e) in blocked {
            let _ = write!(
                body,
                "<li>{} {} — {} {} <span class=\"evidence\">{}</span> <a href=\"/why{}\">explain</a></li>",
                esc(s.provider.display_name()),
                session_link(&s.session_id, &scope),
                esc(&e.claim),
                confidence(e.confidence),
                evidence_links(&e.evidence, 3, &scope),
                scope.query_string(&[("subject", &html::id(&s.session_id))])
            );
        }
        body.push_str("</ul>");
    }
    body.push_str("</section>");
    Ok(Html(layout(&v, &scope, "Why", "/why", &body)))
}

pub async fn state(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> PageResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let p = v.engine.projection();
    let at_spec = q.get("at").cloned().unwrap_or_default();
    let at = if at_spec.trim().is_empty() {
        Timestamp::now()
    } else {
        parse_time(&at_spec).ok_or_else(|| {
            ApiError::bad(format!(
                "cannot parse at={at_spec:?}: use RFC 3339, YYYY-MM-DD, now, today, yesterday or -<n>(s|m|h|d|w)"
            ))
        })?
    };
    let statement = state_statement(at);
    let r = run(&v, &statement).await?;
    let min = p
        .sessions
        .iter()
        .map(|s| s.started_at)
        .min()
        .unwrap_or(at)
        .min(at);
    let max = v
        .status
        .last_event_at
        .unwrap_or(at)
        .max(at)
        .max(Timestamp::now());
    let body = format!(
        "<section class=\"card\"><h1>State at a point in time</h1>\
         <form method=\"get\" action=\"/state\" class=\"inline\" id=\"state-form\" data-api=\"/api/state{api_qs}\">{hidden}\
         <label>at <input type=\"datetime-local\" step=\"1\" name=\"at\" id=\"state-at\" value=\"{local}\"></label> \
         <input type=\"range\" id=\"state-slider\" min=\"{min}\" max=\"{max}\" value=\"{cur}\" step=\"1000\" data-min=\"{min}\" data-max=\"{max}\"> \
         <button type=\"submit\">Show</button> <span id=\"state-live\" class=\"muted small\"></span></form>\
         <p class=\"muted small\"><code id=\"state-statement\">{stmt}</code> · sessions started at or before the time and not ended before it; every row carries evidence ids and an uncertainty note</p>\
         <div id=\"state-result\">{result}</div></section>",
        api_qs = scope.query_string(&[]),
        hidden = scope
            .pairs()
            .iter()
            .map(|(k, val)| format!(
                "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
                esc(k),
                esc(val)
            ))
            .collect::<String>(),
        local = esc(rfc3339(at).trim_end_matches('Z')),
        min = min.as_millis(),
        max = max.as_millis(),
        cur = at.as_millis(),
        stmt = esc(&statement),
        result = result_html(&r, &scope)
    );
    Ok(Html(layout(&v, &scope, "State", "/state", &body)))
}

pub const EXAMPLES: &[&str] = &[
    "WHAT IS project DOING NOW",
    "SHOW FAILED ATTEMPTS",
    "SHOW FAILED ATTEMPTS FOR project = 'attemptdb'",
    "WHY project STATUS BLOCKED",
    "WHY ses_0191e3a1 STATUS BLOCKED",
    "TRACE att_0191e3b0 CAUSES",
    "STATE project AT '2026-08-28T09:00:00Z'",
    "SHOW HANDOFFS BETWEEN agent = 'claude_code' AND agent = 'codex'",
    "SHOW EVIDENCE FOR att_0191e3a2",
    "SELECT tool_name, outcome_status, count(*) FROM events GROUP BY 1, 2",
    "SELECT kind, count(*) FROM events GROUP BY 1 ORDER BY 2 DESC",
];

pub async fn query(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> PageResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let statement = q.get("statement").cloned().unwrap_or_default();
    let format = q.get("format").cloned().unwrap_or_else(|| "table".into());
    let mut body = format!(
        "<section class=\"card\"><h1>Query console</h1>\
         <form method=\"get\" action=\"/query\" id=\"query-form\" data-api=\"/api/query{api_qs}\">{hidden}\
         <textarea name=\"statement\" id=\"query-statement\" rows=\"4\" spellcheck=\"false\" placeholder=\"AttemptQL (SHOW / WHY / TRACE / STATE / DIFF / WHAT IS) or read-only SQL over events, sessions, turns, tool_calls, attempts, handoffs, edges, signals\">{stmt}</textarea>\
         <div class=\"row-actions\"><label>format <select name=\"format\">{formats}</select></label> <button type=\"submit\">Run</button> <span class=\"muted small\">Ctrl+Enter runs · read-only: only SELECT/WITH/EXPLAIN/DESCRIBE and the AttemptQL verbs are accepted</span></div></form>\
         <details class=\"examples\"><summary>examples</summary><ul>{examples}</ul></details></section>",
        api_qs = scope.query_string(&[]),
        hidden = scope
            .pairs()
            .iter()
            .map(|(k, val)| format!("<input type=\"hidden\" name=\"{}\" value=\"{}\">", esc(k), esc(val)))
            .collect::<String>(),
        stmt = esc(&statement),
        formats = ["table", "json", "csv"]
            .iter()
            .map(|f| format!(
                "<option value=\"{f}\"{}>{f}</option>",
                if *f == format { " selected" } else { "" }
            ))
            .collect::<String>(),
        examples = EXAMPLES
            .iter()
            .map(|e| format!(
                "<li><a href=\"/query{}\" class=\"example\" data-statement=\"{}\"><code>{}</code></a></li>",
                scope.query_string(&[("statement", e)]),
                esc(e),
                esc(e)
            ))
            .collect::<String>()
    );
    if !statement.trim().is_empty() {
        body.push_str("<section class=\"card\" id=\"query-result\">");
        match crate::readonly::check_read_only(&statement) {
            Err(e) => {
                let _ = write!(body, "<pre class=\"error\">{}</pre>", esc(&e));
            }
            Ok(()) => {
                let trimmed = statement.trim().trim_end_matches(';').trim();
                match v.engine.query(trimmed).await {
                    Ok(r) => {
                        let _ = write!(
                            body,
                            "<p class=\"muted small\"><code>{}</code> · {}</p>",
                            esc(trimmed),
                            plural(r.row_count(), "row")
                        );
                        match format.as_str() {
                            "json" => {
                                let (capped, _) = cap(&r, PAGE_ROWS);
                                let _ = write!(
                                    body,
                                    "<pre class=\"json\">{}</pre>{}",
                                    esc(&serde_json::to_string_pretty(&capped.to_json())
                                        .unwrap_or_default()),
                                    notes(&r.notes)
                                );
                            }
                            "csv" => {
                                let (capped, _) = cap(&r, PAGE_ROWS);
                                let _ = write!(
                                    body,
                                    "<pre class=\"json\">{}</pre>{}",
                                    esc(&capped.render_csv()),
                                    notes(&r.notes)
                                );
                            }
                            _ => body.push_str(&result_html(&r, &scope)),
                        }
                    }
                    Err(e @ attemptdb_query::QueryError::Parse { .. }) => {
                        let _ = write!(
                            body,
                            "<pre class=\"error\">{}</pre>",
                            esc(&attemptdb_query::format_parse_error(trimmed, &e))
                        );
                    }
                    Err(e) => {
                        let _ = write!(body, "<pre class=\"error\">{}</pre>", esc(&e.to_string()));
                    }
                }
            }
        }
        body.push_str("</section>");
    }
    Ok(Html(layout(&v, &scope, "Query", "/query", &body)))
}

// ---------------------------------------------------------------------------
// Work units and decisions (tier1-v1 §5.6 / §5.7)
// ---------------------------------------------------------------------------

fn phase_badge(p: Phase) -> String {
    let class = match p {
        Phase::Blocked => "fail",
        Phase::Deliver | Phase::Verify => "ok",
        Phase::Debug => "warn",
        Phase::Implement => "live",
        Phase::Explore | Phase::Plan | Phase::Review => "muted",
    };
    badge(class, p.as_str())
}

fn wu_status_badge(s: WorkUnitStatus) -> String {
    let class = match s {
        WorkUnitStatus::Open => "live",
        WorkUnitStatus::Completed => "ok",
        WorkUnitStatus::Abandoned => "warn",
        WorkUnitStatus::Unknown => "muted",
    };
    badge(class, s.as_str())
}

/// Link to the work unit's row in the timeline's work-unit card.
fn wu_link(id: &attemptdb_core::WorkUnitId, scope: &ScopeQuery) -> String {
    format!(
        "<a class=\"id\" href=\"/timeline{}#wu_{}\">{}</a>",
        scope.without_session().query_string(&[]),
        esc(&id.to_string()),
        html::short_id(id)
    )
}

fn work_units_card(p: &Projection, scope: &ScopeQuery, only_open: bool, limit: usize) -> String {
    let list: Vec<&WorkUnit> = j::work_units_sorted(p)
        .into_iter()
        .filter(|w| !only_open || w.status == WorkUnitStatus::Open || w.phase == Phase::Blocked)
        .collect();
    let mut s = format!(
        "<section class=\"card\" id=\"work-units\"><h2>{}</h2><p class=\"muted small\">a work unit groups turns that touch the same paths, follow each other within ten minutes, or are linked by a handoff ({}); phase and status are heuristics with evidence — hover a badge for the rule</p>",
        if only_open {
            "Open work units"
        } else {
            "Work units"
        },
        esc(crate::INFERENCE_VERSION)
    );
    if list.is_empty() {
        s.push_str(if only_open {
            "<p class=\"muted\">no open work unit in scope</p>"
        } else {
            "<p class=\"muted\">no work units projected in scope</p>"
        });
        s.push_str("</section>");
        return s;
    }
    s.push_str("<div class=\"scroll\"><table><thead><tr><th>work unit</th><th>phase</th><th>status</th><th>objective</th><th>actors</th><th>sessions</th><th>attempts</th><th>paths</th><th>span</th><th>confidence</th><th>evidence</th></tr></thead><tbody>");
    for w in list.iter().take(limit) {
        let objective = match (&w.objective, w.objective_event_id) {
            (Some(o), _) => format!("<span class=\"objective\">{}</span>", clip(o, 100)),
            (None, Some(e)) => format!(
                "<span class=\"muted\">(prompt; text not captured)</span> {}",
                evidence_link(&e, scope)
            ),
            (None, None) => "<span class=\"muted\">(no prompt)</span>".to_string(),
        };
        let _ = write!(
            s,
            "<tr id=\"wu_{}\"><td><code>{}</code></td><td><span title=\"{}\">{}</span></td><td><span title=\"{}\">{}</span>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}{}{}</td><td>{}</td><td>{} → {}</td><td>{}</td><td>{}</td></tr>",
            esc(&w.work_unit_id.to_string()),
            esc(&html::short_id(&w.work_unit_id)),
            esc(&w.phase_reason),
            phase_badge(w.phase),
            esc(&w.status_reason),
            wu_status_badge(w.status),
            w.blocking_signal
                .map(|e| format!(" {}", evidence_link(&e, scope)))
                .unwrap_or_default(),
            objective,
            w.actors
                .iter()
                .map(|a| esc(a.display_name()))
                .collect::<Vec<_>>()
                .join(", "),
            w.sessions
                .iter()
                .take(4)
                .map(|sid| session_link(sid, scope))
                .collect::<Vec<_>>()
                .join(" "),
            w.attempts.len(),
            if w.failure_count > 0 {
                format!(
                    " <span class=\"badge badge-fail\">{} failed</span>",
                    w.failure_count
                )
            } else {
                String::new()
            },
            w.last_attempt
                .map(|a| format!(" last {}", attempt_link(&a, scope)))
                .unwrap_or_default(),
            paths_html(&w.paths, 4),
            ts(w.started_at),
            w.ended_at.map(ts_time).unwrap_or_else(|| "open".into()),
            confidence(w.confidence),
            evidence_links(&w.evidence, 3, scope)
        );
    }
    s.push_str("</tbody></table></div>");
    if list.len() > limit {
        let _ = write!(
            s,
            "<p class=\"muted small\">{} work units; first {limit} shown (<code>/api/work_units</code> has them all)</p>",
            list.len()
        );
    }
    s.push_str("</section>");
    s
}

fn decisions_card(p: &Projection, scope: &ScopeQuery, limit: usize) -> String {
    if p.decisions.is_empty() {
        return String::new();
    }
    let mut list: Vec<&attemptdb_project::Decision> = p.decisions.iter().collect();
    list.sort_by_key(|a| std::cmp::Reverse(a.decided_at));
    let rows: Vec<Vec<String>> = list
        .iter()
        .take(limit)
        .map(|d| {
            vec![
                ts(d.decided_at),
                badge(
                    match d.kind {
                        attemptdb_project::DecisionKind::ApproachChange => "sup",
                        attemptdb_project::DecisionKind::HumanIntervention => "warn",
                    },
                    d.kind.as_str(),
                ),
                attempt_link(&d.selected, scope),
                if d.alternatives.is_empty() {
                    "<span class=\"muted\">same attempt</span>".to_string()
                } else {
                    d.alternatives
                        .iter()
                        .map(|a| attempt_link(a, scope))
                        .collect::<Vec<_>>()
                        .join(" ")
                },
                format!(
                    "{} <span class=\"muted small\">({})</span>",
                    clip(&d.rationale, 160),
                    esc(&d.rationale_source)
                ),
                d.work_unit_id
                    .map(|w| wu_link(&w, scope))
                    .unwrap_or_default(),
                confidence(d.confidence),
                evidence_links(&d.evidence, 3, scope),
            ]
        })
        .collect();
    format!(
        "<section class=\"card\"><h2>Decisions</h2><p class=\"muted small\">derived from the attempt structure — a superseded failure, or a denial followed by another tool ({}); nothing here was stated by a human</p>{}</section>",
        esc(crate::INFERENCE_VERSION),
        table(
            &[
                "decided",
                "kind",
                "continued with",
                "gave up on",
                "rationale",
                "work unit",
                "confidence",
                "evidence"
            ],
            &rows
        )
    )
}

// ---------------------------------------------------------------------------
// Work board (`/work`, `/work/{id}`) and Needs You (`/attention`)
// ---------------------------------------------------------------------------

/// Which board column a work unit belongs in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Column {
    Active,
    Blocked,
    Finished,
}

impl Column {
    fn of(w: &WorkUnit, blocked_units: &[attemptdb_core::WorkUnitId]) -> Self {
        match w.status {
            WorkUnitStatus::Completed | WorkUnitStatus::Abandoned => Column::Finished,
            _ if w.phase == Phase::Blocked || blocked_units.contains(&w.work_unit_id) => {
                Column::Blocked
            }
            _ => Column::Active,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Column::Active => "Active",
            Column::Blocked => "Blocked",
            Column::Finished => "Recently finished",
        }
    }

    fn note(self) -> &'static str {
        match self {
            Column::Active => "open work whose phase is not blocked",
            Column::Blocked => "open work with a blocking phase or a Needs You signal",
            Column::Finished => "completed or abandoned in the selected range",
        }
    }
}

fn work_link(id: &attemptdb_core::WorkUnitId, scope: &ScopeQuery) -> String {
    format!(
        "<a class=\"id\" href=\"/work/{}{}\">{}</a>",
        html::seg(&id.to_string()),
        scope.without_session().query_string(&[]),
        html::short_id(id)
    )
}

/// One board card. Everything on it is inference; the evidence is one click
/// away and the confidence is on the card.
fn work_card(w: &WorkUnit, p: &Projection, scope: &ScopeQuery, blocked: Option<&str>) -> String {
    let mut s = format!(
        "<article class=\"work-card\" id=\"wu_{}\"><header>{} {} <span title=\"{}\">{}</span></header><p class=\"work-objective\">{}</p>",
        esc(&w.work_unit_id.to_string()),
        work_link(&w.work_unit_id, scope),
        wu_status_badge(w.status),
        esc(&w.phase_reason),
        phase_badge(w.phase),
        objective_or_reason(w, scope)
    );
    if let Some(claim) = blocked {
        let _ = write!(s, "<p class=\"callout fail small\">{}</p>", esc(claim));
    }
    let _ = write!(
        s,
        "<p class=\"work-meta\">{} · {} attempt(s){} · {}</p>",
        w.actors
            .iter()
            .map(|a| esc(a.display_name()))
            .collect::<Vec<_>>()
            .join(", "),
        w.attempts.len(),
        if w.failure_count > 0 {
            format!(
                " <span class=\"badge badge-fail\">{} failed</span>",
                w.failure_count
            )
        } else {
            String::new()
        },
        w.sessions
            .iter()
            .take(3)
            .map(|sid| session_link(sid, scope))
            .collect::<Vec<_>>()
            .join(" ")
    );
    if !w.paths.is_empty() {
        let _ = write!(s, "{}", paths_html(&w.paths, 4));
    }
    let _ = write!(
        s,
        "<p class=\"work-foot muted small\">{} → {} · {} · {} · evidence {}</p>",
        ts(w.started_at),
        w.ended_at.map(ts_time).unwrap_or_else(|| "open".into()),
        duration(elapsed_ms(w.started_at, w.updated_at)),
        confidence(w.confidence),
        evidence_links(&w.evidence, 3, scope)
    );
    if !w.commit_shas.is_empty() {
        let _ = write!(
            s,
            "<p class=\"small\">commits {}</p>",
            w.commit_shas
                .iter()
                .take(4)
                .map(|sha| format!("<code>{}</code>", esc(&sha[..sha.len().min(10)])))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    let _ = write!(
        s,
        "<div class=\"chain-wrap\">{}</div></article>",
        attempt_chain(w, p, scope)
    );
    s
}

pub async fn work(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> PageResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let scope = &scope;
    let p = v.engine.projection();
    let now = Timestamp::now();
    let attention = p.attention_at(now, attemptdb_project::DEFAULT_MIN_CONFIDENCE);
    let blocked_units: Vec<attemptdb_core::WorkUnitId> =
        attention.iter().filter_map(|i| i.work_unit_id).collect();

    let mut body = format!(
        "<section class=\"card\"><h1>Work</h1><p class=\"muted small\">an evidence-backed board over inferred work units ({}) — not a task manager: AttemptDB never invents planned work, and every card states what it was derived from</p><p class=\"row-actions\"><a href=\"/timeline{}?view=failures\">failed attempts</a> <a href=\"/handoffs{}\">handoffs</a> <a href=\"/attention{}\">needs you ({})</a></p></section>",
        esc(crate::INFERENCE_VERSION),
        String::new(),
        scope.without_session().query_string(&[]),
        scope.without_session().query_string(&[]),
        attention.len(),
    );

    let units = j::work_units_sorted(p);
    if units.is_empty() {
        body.push_str("<section class=\"card\"><p class=\"muted\">no work unit projected in scope — work units need at least one prompted turn</p></section>");
        return Ok(Html(layout(&v, scope, "Work", "/work", &body)));
    }
    body.push_str("<div class=\"board\">");
    for col in [Column::Active, Column::Blocked, Column::Finished] {
        let list: Vec<&WorkUnit> = units
            .iter()
            .copied()
            .filter(|w| Column::of(w, &blocked_units) == col)
            .collect();
        let _ = write!(
            body,
            "<section class=\"board-col\" id=\"col-{}\"><h2>{} <span class=\"badge badge-muted\">{}</span></h2><p class=\"muted small\">{}</p>",
            col.title().to_ascii_lowercase().replace(' ', "-"),
            col.title(),
            list.len(),
            col.note()
        );
        if list.is_empty() {
            body.push_str("<p class=\"muted small\">nothing here</p>");
        }
        for w in list.iter().take(30) {
            let claim = attention
                .iter()
                .find(|i| i.work_unit_id == Some(w.work_unit_id))
                .map(|i| i.claim.as_str());
            body.push_str(&work_card(w, p, scope, claim));
        }
        if list.len() > 30 {
            let _ = write!(
                body,
                "<p class=\"muted small\">{} more not shown</p>",
                list.len() - 30
            );
        }
        body.push_str("</section>");
    }
    body.push_str("</div>");
    body.push_str(&decisions_card(p, scope, 20));
    Ok(Html(layout(&v, scope, "Work", "/work", &body)))
}

/// `/work/{id}`: one work unit with its attempt chain, decisions, artifacts
/// and handoffs — the inspector the board opens.
pub async fn work_detail(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> PageResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let scope = &scope;
    let p = v.engine.projection();
    let w = api::find_work_unit(p, &id)?;
    let now = Timestamp::now();
    let attention = p.attention_at(now, attemptdb_project::DEFAULT_MIN_CONFIDENCE);

    let mut body = format!(
        "<section class=\"card\"><h1>Work unit <code>{}</code></h1><p class=\"work-objective\">{}</p><p>{} {} <span title=\"{}\">{}</span> · {} · {}</p>{}</section>",
        esc(&html::short_id(&w.work_unit_id)),
        objective_or_reason(w, scope),
        wu_status_badge(w.status),
        badge("muted", &format!("{} attempts", w.attempts.len())),
        esc(&w.phase_reason),
        phase_badge(w.phase),
        confidence(w.confidence),
        esc(&w.status_reason),
        key_values(&[
            ("project", esc(&w.project_name)),
            (
                "actors",
                w.actors
                    .iter()
                    .map(|a| esc(a.display_name()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            (
                "sessions",
                w.sessions
                    .iter()
                    .map(|s| session_link(s, scope))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            (
                "span",
                format!(
                    "{} → {} ({})",
                    ts(w.started_at),
                    w.ended_at.map(ts).unwrap_or_else(|| "open".into()),
                    duration(elapsed_ms(w.started_at, w.updated_at))
                )
            ),
            ("paths", paths_html(&w.paths, 20)),
            (
                "commits",
                if w.commit_shas.is_empty() {
                    "<span class=\"muted\">none</span>".to_string()
                } else {
                    w.commit_shas
                        .iter()
                        .map(|s| format!("<code>{}</code>", esc(&s[..s.len().min(12)])))
                        .collect::<Vec<_>>()
                        .join(" ")
                }
            ),
            ("evidence", evidence_links(&w.evidence, 12, scope)),
            (
                "inference",
                format!(
                    "<code>{}</code> version {}",
                    esc(w.algorithm_version.as_str()),
                    w.version
                )
            ),
        ])
    );

    let mine: Vec<&AttentionItem> = attention
        .iter()
        .filter(|i| i.work_unit_id == Some(w.work_unit_id))
        .collect();
    if !mine.is_empty() {
        body.push_str("<section class=\"card attention\"><h2>Needs you</h2><ol class=\"atn-list\">");
        for it in mine {
            body.push_str(&attention_item_html(it, scope, false));
        }
        body.push_str("</ol></section>");
    }

    let _ = write!(
        body,
        "<section class=\"card\"><h2>Attempt path</h2>{}</section>",
        attempt_chain(w, p, scope)
    );

    // Attempts in full.
    let mut attempts: Vec<&Attempt> = w
        .attempts
        .iter()
        .filter_map(|id| p.attempts.iter().find(|a| a.attempt_id == *id))
        .collect();
    attempts.sort_by_key(|a| (a.started_at, a.attempt_id));
    if !attempts.is_empty() {
        body.push_str("<section class=\"card\"><h2>Attempts</h2><ul class=\"attempts\">");
        for a in &attempts {
            body.push_str(&attempt_row(a, p, scope));
        }
        body.push_str("</ul></section>");
    }

    // Decisions taken inside this unit.
    let decisions: Vec<&attemptdb_project::Decision> = p
        .decisions
        .iter()
        .filter(|d| d.work_unit_id == Some(w.work_unit_id))
        .collect();
    if !decisions.is_empty() {
        let rows: Vec<Vec<String>> = decisions
            .iter()
            .map(|d| {
                vec![
                    ts(d.decided_at),
                    esc(d.kind.as_str()),
                    attempt_link(&d.selected, scope),
                    d.alternatives
                        .iter()
                        .map(|a| attempt_link(a, scope))
                        .collect::<Vec<_>>()
                        .join(" "),
                    clip(&d.rationale, 200),
                    confidence(d.confidence),
                    evidence_links(&d.evidence, 3, scope),
                ]
            })
            .collect();
        let _ = write!(
            body,
            "<section class=\"card\"><h2>Decisions</h2>{}</section>",
            table(
                &[
                    "decided",
                    "kind",
                    "continued with",
                    "gave up on",
                    "rationale",
                    "confidence",
                    "evidence"
                ],
                &rows
            )
        );
    }

    // Handoffs in or out of this unit's sessions.
    let handoffs: Vec<&attemptdb_project::Handoff> = p
        .handoffs
        .iter()
        .filter(|h| w.sessions.contains(&h.from_session) || w.sessions.contains(&h.to_session))
        .collect();
    if !handoffs.is_empty() {
        let rows: Vec<Vec<String>> = handoffs
            .iter()
            .map(|h| {
                vec![
                    ts(h.at),
                    format!(
                        "{} {} → {} {}",
                        esc(h.from_provider.display_name()),
                        session_link(&h.from_session, scope),
                        esc(h.to_provider.display_name()),
                        session_link(&h.to_session, scope)
                    ),
                    duration(h.gap_ms),
                    paths_html(&h.shared_paths, 4),
                    confidence(h.confidence),
                    evidence_links(&h.evidence, 3, scope),
                ]
            })
            .collect();
        let _ = write!(
            body,
            "<section class=\"card\"><h2>Handoffs</h2>{}</section>",
            table(
                &["at", "from → to", "gap", "shared paths", "confidence", "evidence"],
                &rows
            )
        );
    }

    let title = format!("Work unit {}", html::short_id(&w.work_unit_id));
    Ok(Html(layout(&v, scope, &title, "/work", &body)))
}

/// `/attention`: the Needs You queue in full, with the evidence.
pub async fn attention(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> PageResult {
    let scope = ScopeQuery::from_map(&q);
    let v = view(&state, &scope).await?;
    let scope = &scope;
    let p = v.engine.projection();
    let now = Timestamp::now();
    let items = p.attention_at(now, attemptdb_project::DEFAULT_MIN_CONFIDENCE);
    let open_sessions = p.sessions.iter().filter(|s| s.ended_at.is_none()).count();

    let mut body = format!(
        "<section class=\"card\"><h1>Needs you</h1><p class=\"muted small\">only four things reach this queue: an unanswered permission request, an agent waiting for input, the same failure twice with nothing superseding it, and two open work units editing the same paths. A completed turn, an idle session and a single failed tool call never do.</p><p class=\"muted small\">{} open session(s) in scope · confidence floor {:.2} · inference <code>{}</code></p></section>",
        open_sessions,
        attemptdb_project::DEFAULT_MIN_CONFIDENCE,
        esc(crate::INFERENCE_VERSION)
    );
    if items.is_empty() {
        body.push_str("<section class=\"card\"><p><b>Nothing needs you.</b></p><p class=\"muted small\">That is a claim about the observed hook events, not about the world: an agent waiting outside the hook surface is invisible here.</p></section>");
        return Ok(Html(layout(&v, scope, "Needs You", "/attention", &body)));
    }
    body.push_str("<section class=\"card attention\" data-live=\"attention\"><ol class=\"atn-list\">");
    for it in &items {
        body.push_str(&attention_item_html(it, scope, false));
    }
    body.push_str("</ol></section>");
    Ok(Html(layout(&v, scope, "Needs You", "/attention", &body)))
}
