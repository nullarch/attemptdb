//! Static export: the timeline, failures and handoffs of a scope as ONE
//! self-contained HTML file (inline CSS, no script, no token, no server).
//!
//! With `sanitized`, events go through the storage crate's
//! [`sanitize_event`](attemptdb_storage::snapshot::sanitize_event) before
//! projection (prompts, commands, tool output and raw payloads dropped,
//! absolute paths rewritten, provider session ids hashed, remote dropped),
//! and the renderer additionally never prints objectives, project roots or
//! the database path.

use crate::html::{clip, duration, elapsed_ms, esc, plural, short_id, ts, ts_time};
use crate::store::{CaptureCounts, capture_counts};
use anyhow::{Context, Result};
use attemptdb_core::{CaptureMode, Event, Timestamp};
use attemptdb_project::{
    Attempt, AttemptOutcome, CoverageGrade, Projection, Session, ToolCall, TurnStatus,
};
use attemptdb_query::{PrefixedId, QueryEngine};
use attemptdb_storage::snapshot::{SanitizePolicy, sanitize_event};
use attemptdb_storage::{Database, ScanFilter};
use std::collections::HashMap;
use std::fmt::Write as _;

/// What to render.
#[derive(Clone, Debug)]
pub struct ExportOptions {
    /// Strip content, paths outside the repository, and home directories.
    pub sanitized: bool,
    /// Append the "Built with AttemptDB" footer.
    pub attribution: bool,
    /// Sessions shown (newest first); the rest are counted.
    pub session_limit: usize,
    /// Human label of the scope (project, time window).
    pub scope_label: String,
    /// The capture mode of the source database.
    pub capture_mode: CaptureMode,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            sanitized: false,
            attribution: true,
            session_limit: 50,
            scope_label: "all projects".to_string(),
            capture_mode: CaptureMode::default(),
        }
    }
}

/// Everything the renderer needs, gathered from an engine.
pub struct ExportInput<'a> {
    pub projection: &'a Projection,
    pub event_count: usize,
    pub capture: HashMap<attemptdb_core::SessionId, CaptureCounts>,
    /// `(project name, project root)` — only printed when not sanitized.
    pub project_roots: Vec<(String, String)>,
    pub generated_at: Timestamp,
    pub options: ExportOptions,
}

/// Honest description of what the projection could and could not observe
/// for a session.
pub fn coverage_note(s: &Session) -> String {
    if s.coverage == CoverageGrade::Full {
        return "full: session start, session end, prompts and tool calls were all observed; only hook events are visible".to_string();
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
        "{} ({}); events may be missing",
        s.coverage.as_str(),
        missing.join(", ")
    )
}

/// Sanitize a copy of `events` with the publishing policy (plus hashed
/// session ids and no remote).
pub fn sanitize_all(events: &mut [Event]) {
    let policy = SanitizePolicy {
        drop_remote: true,
        hash_session_ids: true,
        ..SanitizePolicy::default()
    };
    for ev in events.iter_mut() {
        sanitize_event(ev, &policy);
    }
}

/// Scan `db` with `filter`, build the engine (over sanitized events when
/// asked), and render.
pub async fn render_database(
    db: &Database,
    filter: &ScanFilter,
    options: ExportOptions,
) -> Result<String> {
    let mut events = db.scan(filter).context("scanning events")?;
    if options.sanitized {
        sanitize_all(&mut events);
    }
    let capture = capture_counts(&events);
    let mut project_roots: Vec<(String, String)> = Vec::new();
    if !options.sanitized {
        for ev in &events {
            if !project_roots.iter().any(|(n, _)| n == &ev.project.name) {
                project_roots.push((ev.project.name.clone(), ev.project.root.clone()));
            }
        }
    }
    let event_count = events.len();
    let engine = QueryEngine::from_events(events)
        .await
        .context("building the query engine")?;
    Ok(render(&ExportInput {
        projection: engine.projection(),
        event_count,
        capture,
        project_roots,
        generated_at: Timestamp::now(),
        options,
    }))
}

fn outcome_text(o: AttemptOutcome) -> (&'static str, &'static str) {
    match o {
        AttemptOutcome::Succeeded => ("ok", "✓ succeeded"),
        AttemptOutcome::Failed => ("fail", "✗ failed"),
        AttemptOutcome::Superseded => ("sup", "↻ superseded"),
        AttemptOutcome::Abandoned => ("warn", "… abandoned"),
        AttemptOutcome::InProgress => ("live", "▶ in progress"),
        AttemptOutcome::Unknown => ("muted", "? unknown"),
    }
}

fn turn_text(s: TurnStatus) -> (&'static str, &'static str) {
    match s {
        TurnStatus::Completed => ("ok", "completed"),
        TurnStatus::Failed => ("fail", "failed"),
        TurnStatus::InProgress => ("live", "in progress"),
        TurnStatus::Unknown => ("muted", "no stop seen"),
    }
}

fn badge(class: &str, text: &str) -> String {
    format!("<span class=\"badge badge-{class}\">{}</span>", esc(text))
}

fn anchor<T: PrefixedId>(id: &T) -> String {
    format!(
        "<a class=\"id\" href=\"#{}\" title=\"{}\">{}</a>",
        esc(&id.readable()),
        esc(&id.readable()),
        short_id(id)
    )
}

fn ev_ids<T: PrefixedId>(ids: &[T], max: usize) -> String {
    if ids.is_empty() {
        return "<span class=\"muted\">no evidence</span>".to_string();
    }
    let mut s: String = ids
        .iter()
        .take(max)
        .map(|i| {
            format!(
                "<code class=\"ev\" title=\"{}\">{}</code>",
                esc(&i.readable()),
                short_id(i)
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    if ids.len() > max {
        let _ = write!(
            s,
            " <span class=\"muted\">(+{} more)</span>",
            ids.len() - max
        );
    }
    s
}

fn tool_call_line(tc: &ToolCall, sanitized: bool) -> String {
    let status = match &tc.outcome {
        Some(o) => {
            let mut t = o.status.as_str().to_string();
            if let Some(c) = &o.class {
                let _ = write!(t, ":{c}");
            }
            badge(crate::html::status_class(o.status.as_str()), &t)
        }
        None if tc.finished_at.is_none() => badge("live", "in flight"),
        None => badge("muted", "no end observed"),
    };
    let dur = tc
        .duration_ms
        .or_else(|| match (tc.started_at, tc.finished_at) {
            (Some(s), Some(e)) => Some(elapsed_ms(s, e)),
            _ => None,
        })
        .map(duration)
        .unwrap_or_default();
    let path = tc
        .paths
        .first()
        .map(|p| {
            if sanitized {
                format!("<code class=\"path\">{}</code>", clip(p.display(), 60))
            } else {
                format!(
                    "<code class=\"path\" title=\"{}\">{}</code>",
                    esc(&p.logical),
                    clip(p.display(), 60)
                )
            }
        })
        .unwrap_or_default();
    format!(
        "<li class=\"call\"><span class=\"when\">{}</span> <code>{}</code> {} {} <span class=\"muted small\">{}</span> <span class=\"evidence\">{}</span></li>",
        tc.started_at.map(ts_time).unwrap_or_else(|| "—".into()),
        esc(&tc.tool.name),
        status,
        path,
        dur,
        ev_ids(
            &tc.start_event_id
                .into_iter()
                .chain(tc.end_event_id)
                .collect::<Vec<_>>(),
            2
        )
    )
}

fn attempt_block(a: &Attempt, p: &Projection, sanitized: bool) -> String {
    let (class, text) = outcome_text(a.outcome);
    let dur = a
        .ended_at
        .map(|e| duration(elapsed_ms(a.started_at, e)))
        .unwrap_or_default();
    let mut s = format!(
        "<li class=\"attempt\" id=\"{}\"><div>{} {}{} <span class=\"approach\">{}</span> <span class=\"muted small\">{} · {} · conf {}</span>{}{}</div>",
        esc(&a.attempt_id.readable()),
        anchor(&a.attempt_id),
        badge(class, text),
        a.failure_class
            .as_deref()
            .map(|c| format!(" {}", badge("class", c)))
            .unwrap_or_default(),
        clip(&a.approach, 100),
        plural(a.paths.len(), "path"),
        dur,
        a.confidence,
        a.superseded_by
            .map(|n| format!(" <span class=\"muted\">retried by</span> {}", anchor(&n)))
            .unwrap_or_default(),
        a.supersedes
            .map(|n| format!(" <span class=\"muted\">retries</span> {}", anchor(&n)))
            .unwrap_or_default()
    );
    if !a.paths.is_empty() {
        let _ = write!(
            s,
            "<div class=\"paths\">{}</div>",
            a.paths
                .iter()
                .take(12)
                .map(|p| format!("<code class=\"path\">{}</code>", clip(p, 80)))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    let calls: Vec<&ToolCall> = a
        .tool_call_ids
        .iter()
        .filter_map(|id| p.tool_calls.iter().find(|c| &c.tool_call_id == id))
        .collect();
    if !calls.is_empty() {
        s.push_str("<ul class=\"calls\">");
        for tc in calls {
            s.push_str(&tool_call_line(tc, sanitized));
        }
        s.push_str("</ul>");
    }
    let _ = write!(
        s,
        "<div class=\"evidence\">evidence: {}</div></li>",
        ev_ids(&a.evidence, 6)
    );
    s
}

/// Render the export document.
pub fn render(input: &ExportInput<'_>) -> String {
    let p = input.projection;
    let o = &input.options;
    let sanitized = o.sanitized;
    let mut body = String::new();

    let (captured, reconstructed) = p.sessions.iter().fold((0, 0), |acc, s| {
        let c = input
            .capture
            .get(&s.session_id)
            .copied()
            .unwrap_or_default();
        (acc.0 + c.captured, acc.1 + c.reconstructed)
    });
    let _ = write!(
        body,
        "<header class=\"top\"><div class=\"brand\">AttemptDB <span class=\"sub\">AgentTimeline</span></div></header>\
         <div class=\"facts\"><span class=\"fact\"><span class=\"k\">scope</span> {}</span> \
         <span class=\"fact\"><span class=\"k\">generated</span> {}</span> \
         {} {} \
         <span class=\"fact\"><span class=\"k\">inference</span> <code>{}</code></span> \
         <span class=\"tagline\">{}</span></div>",
        esc(&o.scope_label),
        ts(input.generated_at),
        badge(
            if sanitized { "ok" } else { "warn" },
            if sanitized {
                "sanitized: no prompts, commands, tool output, absolute paths or home directories"
            } else {
                "not sanitized: prompt text and full paths included — review before sharing"
            }
        ),
        badge("muted", &format!("capture {}", o.capture_mode.as_str())),
        esc(crate::INFERENCE_VERSION),
        esc(crate::TAGLINE)
    );

    let mut sessions: Vec<&Session> = p
        .sessions
        .iter()
        .filter(|s| s.prompt_count > 0 || s.tool_call_count > 0)
        .collect();
    sessions.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    let _ = write!(
        body,
        "<section class=\"card\"><h1>Timeline</h1><p class=\"muted small\">{} · {} · {} · {} from {} events ({} hook-captured, {} reconstructed from transcripts){}</p></section>",
        plural(p.sessions.len(), "session"),
        plural(p.turns.len(), "turn"),
        plural(p.attempts.len(), "attempt"),
        plural(p.handoffs.len(), "handoff"),
        input.event_count,
        captured,
        reconstructed,
        if sessions.len() > o.session_limit {
            format!("; newest {} sessions shown", o.session_limit)
        } else {
            String::new()
        }
    );
    if sessions.is_empty() {
        body.push_str("<section class=\"card\"><p class=\"muted\">no sessions with prompts or tool calls in scope</p></section>");
    }
    for s in sessions.iter().take(o.session_limit) {
        let caps = input
            .capture
            .get(&s.session_id)
            .copied()
            .unwrap_or_default();
        let root = if sanitized {
            String::new()
        } else {
            input
                .project_roots
                .iter()
                .find(|(n, _)| n == &s.project_name)
                .map(|(_, r)| {
                    format!(
                        " <span class=\"muted small\">root <code>{}</code></span>",
                        esc(r)
                    )
                })
                .unwrap_or_default()
        };
        let _ = write!(
            body,
            "<section class=\"session card\" id=\"{}\"><h2><span class=\"provider\">{}</span> <span class=\"project\">{}</span>{} <span class=\"when\">{} → {}</span> {} <span class=\"muted small\">{} · {} · {} · {} captured / {} reconstructed · {}</span></h2><p class=\"muted small\">coverage: {}</p><ol class=\"turns\">",
            esc(&s.session_id.readable()),
            esc(s.provider.display_name()),
            clip(&s.project_name, 40),
            root,
            ts(s.started_at),
            s.ended_at.map(ts_time).unwrap_or_else(|| "open".into()),
            badge(
                match s.coverage {
                    CoverageGrade::Full => "ok",
                    CoverageGrade::Unknown => "muted",
                    _ => "warn",
                },
                &format!("{} coverage", s.coverage.as_str())
            ),
            plural(s.turn_count as usize, "turn"),
            plural(s.tool_call_count as usize, "tool call"),
            plural(s.failure_count as usize, "failure"),
            caps.captured,
            caps.reconstructed,
            short_id(&s.session_id),
            esc(&coverage_note(s))
        );
        let mut turns: Vec<_> = p.turns_of(s.session_id).collect();
        turns.sort_by_key(|t| t.index);
        for t in turns {
            let (tc, tt) = turn_text(t.status);
            let objective = if sanitized {
                match t.prompt_chars {
                    Some(n) => format!("<span class=\"muted\">(prompt of {n} chars)</span>"),
                    None if t.index == 0 => {
                        "<span class=\"muted\">(activity before the first prompt)</span>"
                            .to_string()
                    }
                    None => "<span class=\"muted\">(prompt)</span>".to_string(),
                }
            } else {
                match (&t.objective, t.prompt_chars) {
                    (Some(o), _) => format!("<span class=\"objective\">{}</span>", clip(o, 160)),
                    (None, Some(n)) => format!(
                        "<span class=\"muted\">(prompt of {n} chars; text not captured)</span>"
                    ),
                    (None, None) if t.index == 0 => {
                        "<span class=\"muted\">(activity before the first prompt)</span>"
                            .to_string()
                    }
                    (None, None) => {
                        "<span class=\"muted\">(prompt; text not captured)</span>".to_string()
                    }
                }
            };
            let _ = write!(
                body,
                "<li class=\"turn\"><div class=\"turn-head\"><span class=\"when\">{}</span> <b>turn {}</b> {} {}</div><ul class=\"attempts\">",
                ts_time(t.started_at),
                t.index,
                badge(tc, tt),
                objective
            );
            let mut attempts: Vec<&Attempt> = p
                .attempts
                .iter()
                .filter(|a| a.turn_id == t.turn_id)
                .collect();
            attempts.sort_by_key(|a| a.index);
            if attempts.is_empty() {
                body.push_str("<li class=\"muted small\">no tool calls in this turn</li>");
            }
            for a in attempts {
                body.push_str(&attempt_block(a, p, sanitized));
            }
            body.push_str("</ul></li>");
        }
        body.push_str("</ol></section>");
    }

    // Work units.
    let mut units: Vec<&attemptdb_project::WorkUnit> = p.work_units.iter().collect();
    units.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let _ = write!(
        body,
        "<section class=\"card\"><h1>Work units</h1><p class=\"muted small\">turns grouped by shared paths, adjacency or handoffs; phase and status are {} heuristics</p>",
        esc(crate::INFERENCE_VERSION)
    );
    if units.is_empty() {
        body.push_str("<p class=\"muted\">none projected</p>");
    } else {
        body.push_str("<div class=\"scroll\"><table><thead><tr><th>work unit</th><th>phase</th><th>status</th><th>objective</th><th>actors</th><th>attempts</th><th>paths</th><th>span</th><th>confidence</th><th>evidence</th></tr></thead><tbody>");
        for w in units.iter().take(o.session_limit) {
            let objective = if sanitized {
                "<span class=\"muted\">(not included)</span>".to_string()
            } else {
                w.objective
                    .as_deref()
                    .map(|t| clip(t, 100))
                    .unwrap_or_else(|| "<span class=\"muted\">(not captured)</span>".into())
            };
            let _ = write!(
                body,
                "<tr id=\"wu_{}\"><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}{}</td><td>{}</td><td>{} → {}</td><td>{}</td><td>{}</td></tr>",
                esc(&w.work_unit_id.to_string()),
                short_id(&w.work_unit_id),
                badge(
                    match w.phase {
                        attemptdb_project::Phase::Blocked => "fail",
                        attemptdb_project::Phase::Deliver | attemptdb_project::Phase::Verify =>
                            "ok",
                        attemptdb_project::Phase::Debug => "warn",
                        _ => "muted",
                    },
                    w.phase.as_str()
                ),
                badge(
                    match w.status {
                        attemptdb_project::WorkUnitStatus::Open => "live",
                        attemptdb_project::WorkUnitStatus::Completed => "ok",
                        attemptdb_project::WorkUnitStatus::Abandoned => "warn",
                        attemptdb_project::WorkUnitStatus::Unknown => "muted",
                    },
                    w.status.as_str()
                ),
                objective,
                w.actors
                    .iter()
                    .map(|a| esc(a.display_name()))
                    .collect::<Vec<_>>()
                    .join(", "),
                w.attempts.len(),
                if w.failure_count > 0 {
                    format!(" ({} failed)", w.failure_count)
                } else {
                    String::new()
                },
                w.paths
                    .iter()
                    .take(4)
                    .map(|x| format!("<code class=\"path\">{}</code>", clip(x, 60)))
                    .collect::<Vec<_>>()
                    .join(" "),
                ts(w.started_at),
                w.ended_at.map(ts_time).unwrap_or_else(|| "open".into()),
                w.confidence,
                ev_ids(&w.evidence, 3)
            );
        }
        body.push_str("</tbody></table></div>");
    }
    body.push_str("</section>");

    // Failures.
    let mut failed: Vec<&Attempt> = p
        .attempts
        .iter()
        .filter(|a| a.outcome.is_failure())
        .collect();
    failed.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    let _ = write!(
        body,
        "<section class=\"card\"><h1>Failures</h1><p class=\"muted small\">{} of {} attempts failed or were superseded; attempts are Tier 1 inferences ({})</p>",
        failed.len(),
        p.attempts.len(),
        esc(crate::INFERENCE_VERSION)
    );
    if failed.is_empty() {
        body.push_str("<p class=\"muted\">none</p>");
    } else {
        body.push_str("<div class=\"scroll\"><table><thead><tr><th>attempt</th><th>session</th><th>started</th><th>outcome</th><th>failure class</th><th>approach</th><th>retried by</th><th>confidence</th><th>evidence</th></tr></thead><tbody>");
        for a in &failed {
            let (c, t) = outcome_text(a.outcome);
            let session = p.session(a.session_id);
            let _ = write!(
                body,
                "<tr><td>{}</td><td>{} {}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                anchor(&a.attempt_id),
                esc(session.map(|s| s.provider.display_name()).unwrap_or("?")),
                anchor(&a.session_id),
                ts(a.started_at),
                badge(c, t),
                a.failure_class
                    .as_deref()
                    .map(|c| badge("class", c))
                    .unwrap_or_default(),
                clip(&a.approach, 80),
                a.superseded_by
                    .map(|n| anchor(&n))
                    .unwrap_or_else(|| "<span class=\"muted\">not retried</span>".into()),
                a.confidence,
                ev_ids(&a.evidence, 3)
            );
        }
        body.push_str("</tbody></table></div>");
    }
    body.push_str("</section>");

    // Handoffs.
    let _ = write!(
        body,
        "<section class=\"card\"><h1>Handoffs</h1><p class=\"muted small\">work moving between agents ({} heuristic)</p>",
        esc(crate::INFERENCE_VERSION)
    );
    if p.handoffs.is_empty() {
        body.push_str("<p class=\"muted\">none detected</p>");
    } else {
        body.push_str("<div class=\"scroll\"><table><thead><tr><th>at</th><th>from</th><th>to</th><th>gap</th><th>shared paths</th><th>confidence</th><th>evidence</th></tr></thead><tbody>");
        let mut list: Vec<_> = p.handoffs.iter().collect();
        list.sort_by(|a, b| b.at.cmp(&a.at));
        for h in list {
            let _ = write!(
                body,
                "<tr><td>{}</td><td>{} {}</td><td>{} {}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                ts(h.at),
                esc(h.from_provider.display_name()),
                anchor(&h.from_session),
                esc(h.to_provider.display_name()),
                anchor(&h.to_session),
                duration(h.gap_ms),
                h.shared_paths
                    .iter()
                    .take(4)
                    .map(|x| format!("<code class=\"path\">{}</code>", clip(x, 60)))
                    .collect::<Vec<_>>()
                    .join(" "),
                h.confidence,
                ev_ids(&h.evidence, 4)
            );
        }
        body.push_str("</tbody></table></div>");
    }
    body.push_str("</section>");

    let _ = write!(
        body,
        "<footer><span>{}</span>{}</footer>",
        esc(crate::TAGLINE),
        if o.attribution {
            " <span class=\"attribution\">Built with AttemptDB</span>"
        } else {
            ""
        }
    );

    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<meta name=\"referrer\" content=\"no-referrer\">\n<title>AgentTimeline · {}</title>\n<style>\n{}\n</style>\n</head>\n<body class=\"export\">\n{}\n</body>\n</html>\n",
        esc(&o.scope_label),
        crate::APP_CSS,
        body
    )
}
