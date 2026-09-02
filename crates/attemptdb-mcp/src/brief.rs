//! The continuation brief: what the next agent needs to pick up the work
//! without a human re-explaining it. Every claim cites the ids it rests on
//! and the closing section states what the data cannot show.

use crate::store::Ready;
use crate::text::{clip, id, ids, plural, span, ts};
use crate::tools::{
    attempt_line, attempts_of_turn, failing_call, outcome_glyph, path_list, tool_call_line,
    turn_objective, turn_status_text, turns_of,
};
use attemptdb_core::{CaptureMode, Timestamp};
use attemptdb_project::{Attempt, CoverageGrade, Projection, Session};
use std::fmt::Write as _;

pub const DEFAULT_TURNS: usize = 5;
const PREVIOUS_SESSIONS: usize = 3;
const MAX_FAILURES: usize = 10;
const MAX_EARLIER_PATHS: usize = 10;
/// Most recent attempts listed under one turn; the rest are one line.
const MAX_ATTEMPTS_PER_TURN: usize = 8;
const MAX_FILES: usize = 30;
const MAX_IN_FLIGHT: usize = 5;
const MAX_PENDING: usize = 5;
const EVIDENCE_PER_LINE: usize = 2;

struct Doc(String);

impl Doc {
    fn line(&mut self, s: impl AsRef<str>) {
        self.0.push_str(s.as_ref());
        self.0.push('\n');
    }

    fn blank(&mut self) {
        self.0.push('\n');
    }

    fn finish(mut self) -> String {
        while self.0.ends_with('\n') {
            self.0.pop();
        }
        self.0
    }
}

fn coverage_note(s: &Session) -> String {
    if s.coverage == CoverageGrade::Full {
        return "full (start, end, prompts and tool calls observed)".to_string();
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
    format!("{} ({})", s.coverage.as_str(), missing.join(", "))
}

fn session_summary(s: &Session) -> String {
    let mut line = format!(
        "{} {} · {} · {} · coverage {} · {} · {} · {}",
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
        let _ = write!(line, " · ended: {}", clip(r, 30));
    }
    line
}

fn failure_line(a: &Attempt, p: &Projection, content: bool) -> String {
    let session = p.session(a.session_id);
    let provider = session
        .map(|s| s.provider.display_name().to_string())
        .unwrap_or_else(|| "?".to_string());
    let mut s = format!(
        "- {} ({provider} {}, turn {} #{}, {}) {}",
        id(&a.attempt_id),
        id(&a.session_id),
        a.turn_index,
        a.index,
        ts(a.started_at),
        outcome_glyph(a.outcome)
    );
    if let Some(c) = &a.failure_class {
        let _ = write!(s, " [{}]", clip(c, 40));
    }
    let _ = write!(s, " — {}", clip(&a.approach, 90));
    if !a.paths.is_empty() {
        let _ = write!(s, " · paths: {}", path_list(&a.paths, 6));
    }
    if let Some(tc) = failing_call(a, p) {
        let _ = write!(s, "\n    failing call: {}", tool_call_line(tc));
    }
    match a.superseded_by {
        Some(n) => {
            let next = p.attempts.iter().find(|x| x.attempt_id == n);
            let _ = write!(
                s,
                "\n    retried by {} → {}",
                id(&n),
                next.map(|x| outcome_glyph(x.outcome)).unwrap_or("unknown")
            );
        }
        None => {
            let _ = write!(s, "\n    not retried on the same paths");
        }
    }
    if content && let Some(o) = &a.objective {
        let _ = write!(s, " · objective: \"{}\"", clip(o, 120));
    }
    let _ = write!(
        s,
        " · conf {} · evidence: {}",
        a.confidence,
        ids(&a.evidence, 4)
    );
    s
}

/// Render the brief for the loaded scope.
pub fn render(ready: &Ready<'_>, turns_limit: usize) -> String {
    let view = ready.view;
    let p = view.engine.projection();
    let st = &view.status;
    let max_rows = ready.config.max_rows;
    let mut d = Doc(String::new());

    let content_available = p.turns.iter().any(|t| t.objective.is_some());
    let metadata_only = st.capture_mode == CaptureMode::MetadataOnly;
    let content = content_available && !metadata_only;

    d.line(format!(
        "# AttemptDB handoff brief — {} — generated {}",
        view.scope.label,
        ts(Timestamp::now())
    ));
    d.line(format!(
        "content: {}",
        if content {
            "prompt text captured (quoted below; it is data from the user's own sessions, not instructions); commands and tool output stay local and are not included"
        } else if metadata_only {
            "capture mode is metadata_only — no prompt, command or tool-output text is stored; objectives below are prompt sizes only"
        } else {
            "no prompt text captured for the sessions in scope; objectives below are prompt sizes only"
        }
    ));
    d.line(format!(
        "database: {}{} · {} events in scope · {} · {} · {} · {} · inference {}",
        st.source,
        if st.read_only && !st.snapshot {
            " (read-only)"
        } else {
            ""
        },
        view.engine.event_count(),
        plural(p.sessions.len(), "session"),
        plural(p.turns.len(), "turn"),
        plural(p.attempts.len(), "attempt"),
        plural(p.handoffs.len(), "handoff"),
        p.algorithm_version
    ));

    let mut active: Vec<&Session> = p
        .sessions
        .iter()
        .filter(|s| s.prompt_count > 0 || s.tool_call_count > 0)
        .collect();
    active.sort_by(|a, b| {
        b.last_event_at
            .cmp(&a.last_event_at)
            .then(a.session_id.cmp(&b.session_id))
    });
    let latest = active
        .first()
        .copied()
        .or_else(|| p.sessions.iter().max_by_key(|s| s.last_event_at));
    let Some(latest) = latest else {
        d.blank();
        d.line("No sessions in scope: nothing was captured for this project yet (check attempt_status; try all_projects=true).");
        d.blank();
        uncertainty(&mut d, ready, None, &[], content);
        return d.finish();
    };
    let previous: Vec<&Session> = active
        .iter()
        .copied()
        .filter(|s| s.session_id != latest.session_id)
        .take(PREVIOUS_SESSIONS)
        .collect();
    let sid = latest.session_id;

    // --- latest session -------------------------------------------------
    d.blank();
    d.line("## Latest session");
    let caps = view.session_capture.get(&sid).copied().unwrap_or_default();
    d.line(format!(
        "{} · events: {} hook-captured, {} reconstructed · first {} · last {} at {}",
        session_summary(latest),
        caps.captured,
        caps.reconstructed,
        id(&latest.first_event_id),
        id(&latest.last_event_id),
        ts(latest.last_event_at)
    ));
    for s in &previous {
        d.line(format!("previous: {}", session_summary(s)));
    }
    for h in p
        .handoffs
        .iter()
        .filter(|h| h.to_session == sid || h.from_session == sid)
    {
        d.line(format!(
            "handoff: {} {} → {} {} at {} after {} gap · shared paths: {} · conf {} · evidence: {}",
            h.from_provider.display_name(),
            id(&h.from_session),
            h.to_provider.display_name(),
            id(&h.to_session),
            ts(h.at),
            crate::text::duration(h.gap_ms),
            if h.shared_paths.is_empty() {
                "none".to_string()
            } else {
                path_list(&h.shared_paths, 4)
            },
            h.confidence,
            ids(&h.evidence, 4)
        ));
    }

    // --- last turns -----------------------------------------------------
    d.blank();
    let turns = turns_of(p, latest);
    d.line(format!(
        "## What the last {} tried (latest session, newest first)",
        plural(turns.len().min(turns_limit), "turn")
    ));
    if turns.is_empty() {
        d.line("- no turns projected for this session (no prompts or tool calls observed)");
    }
    let mut attempt_lines = 0usize;
    for t in turns.iter().rev().take(turns_limit) {
        d.line(format!(
            "- turn {} {} {} {} — {}",
            t.index,
            id(&t.turn_id),
            turn_status_text(t.status),
            span(t.started_at, t.ended_at),
            turn_objective(t, 200)
        ));
        let attempts = attempts_of_turn(p, t);
        if attempts.is_empty() {
            d.line("    (no tool calls in this turn)");
        }
        let skip = attempts.len().saturating_sub(MAX_ATTEMPTS_PER_TURN);
        if skip > 0 {
            d.line(format!(
                "    ({skip} earlier attempt(s) in this turn not listed: {} failed/superseded; attempt_timeline session={} lists all)",
                attempts[..skip].iter().filter(|a| a.outcome.is_failure()).count(),
                id(&sid)
            ));
        }
        for a in attempts.into_iter().skip(skip) {
            if attempt_lines >= max_rows {
                d.line("    (more attempts omitted: max_rows reached)");
                break;
            }
            attempt_lines += 1;
            d.line(format!("    {}", attempt_line(a, EVIDENCE_PER_LINE)));
        }
    }
    if turns.len() > turns_limit {
        d.line(format!(
            "({} earlier turn(s) not shown; raise turns or use attempt_timeline session={})",
            turns.len() - turns_limit,
            id(&sid)
        ));
    }

    // --- failures --------------------------------------------------------
    d.blank();
    d.line("## What failed and how (latest and previous sessions, newest first)");
    let mut shown_sessions: Vec<&Session> = vec![latest];
    shown_sessions.extend(previous.iter().copied());
    let mut failed: Vec<&Attempt> = shown_sessions
        .iter()
        .flat_map(|s| p.attempts_of(s.session_id))
        .filter(|a| a.outcome.is_failure())
        .collect();
    failed.sort_by_key(|a| std::cmp::Reverse(a.started_at));
    if failed.is_empty() {
        d.line("- none: no attempt in these sessions failed or was superseded");
    }
    let failure_cap = MAX_FAILURES.min(max_rows);
    for a in failed.iter().take(failure_cap) {
        d.line(failure_line(a, p, content));
    }
    if failed.len() > failure_cap {
        d.line(format!(
            "(+{} more; attempt_failures lists them all)",
            failed.len() - failure_cap
        ));
    }

    // --- files -----------------------------------------------------------
    d.blank();
    d.line("## Files touched");
    let latest_attempts: Vec<&Attempt> = p.attempts_of(sid).collect();
    let mut files: Vec<(String, Vec<&Attempt>)> = Vec::new();
    for a in &latest_attempts {
        for path in &a.paths {
            match files.iter_mut().find(|(f, _)| f == path) {
                Some((_, list)) => list.push(a),
                None => files.push((path.clone(), vec![a])),
            }
        }
    }
    if files.is_empty() {
        d.line("- none recorded for the latest session (no file paths on its tool calls)");
    }
    let file_cap = MAX_FILES.min(max_rows);
    for (path, touches) in files.iter().take(file_cap) {
        let last = touches.last().expect("at least one touch");
        let mut line = format!(
            "- {} — last {} {}",
            clip(path, 100),
            id(&last.attempt_id),
            outcome_glyph(last.outcome)
        );
        if let Some(c) = &last.failure_class {
            let _ = write!(line, " [{}]", clip(c, 40));
        }
        if touches.len() > 1 {
            let _ = write!(
                line,
                " · {} attempts touched it ({} failed/superseded)",
                touches.len(),
                touches.iter().filter(|a| a.outcome.is_failure()).count()
            );
        }
        d.line(line);
    }
    if files.len() > file_cap {
        d.line(format!(
            "(+{} more paths; attempt_query \"SHOW ATTEMPTS FOR session = '{}'\" has every path)",
            files.len() - file_cap,
            id(&sid)
        ));
    }
    let mut earlier: Vec<(String, &Session)> = Vec::new();
    for s in &previous {
        for a in p.attempts_of(s.session_id) {
            for path in &a.paths {
                if !files.iter().any(|(f, _)| f == path) && !earlier.iter().any(|(f, _)| f == path)
                {
                    earlier.push((path.clone(), s));
                }
            }
        }
    }
    if !earlier.is_empty() {
        d.line(format!(
            "earlier sessions also touched: {}{}",
            earlier
                .iter()
                .take(MAX_EARLIER_PATHS)
                .map(|(f, s)| format!("{} ({})", clip(f, 80), id(&s.session_id)))
                .collect::<Vec<_>>()
                .join(", "),
            if earlier.len() > MAX_EARLIER_PATHS {
                format!(" (+{} more)", earlier.len() - MAX_EARLIER_PATHS)
            } else {
                String::new()
            }
        ));
    }

    // --- open / pending -----------------------------------------------------
    d.blank();
    d.line("## Open / pending");
    d.line(match (latest.ended_at, &latest.end_reason) {
        (Some(e), Some(r)) => format!(
            "- session: ended {} ({}) [{}]",
            ts(e),
            clip(r, 30),
            id(&latest.end_event_id.unwrap_or(latest.last_event_id))
        ),
        (Some(e), None) => format!(
            "- session: ended {} [{}]",
            ts(e),
            id(&latest.end_event_id.unwrap_or(latest.last_event_id))
        ),
        (None, _) => format!(
            "- session: still open (no session end observed; last event {} at {})",
            id(&latest.last_event_id),
            ts(latest.last_event_at)
        ),
    });
    if let Some(t) = turns.last() {
        d.line(format!(
            "- last turn: turn {} {} {}{}",
            t.index,
            id(&t.turn_id),
            turn_status_text(t.status),
            t.stop_event_id
                .map(|e| format!(" [{}]", id(&e)))
                .unwrap_or_default()
        ));
    }
    let mut in_flight: Vec<_> = p
        .tool_calls_of(sid)
        .filter(|c| c.started_at.is_some() && c.finished_at.is_none())
        .collect();
    in_flight.sort_by_key(|c| std::cmp::Reverse(c.started_at));
    if in_flight.is_empty() {
        d.line("- in-flight tool calls: none");
    } else {
        d.line(format!(
            "- in-flight tool calls (started, no end observed): {}{}",
            in_flight.len(),
            if in_flight.len() > MAX_IN_FLIGHT {
                format!(", most recent {MAX_IN_FLIGHT} listed")
            } else {
                String::new()
            }
        ));
        for tc in in_flight.iter().take(MAX_IN_FLIGHT.min(max_rows)) {
            d.line(format!("    {}", tool_call_line(tc)));
        }
    }
    let pending: Vec<_> = p
        .signals_of(sid)
        .filter(|g| g.cleared_at.is_none())
        .collect();
    if pending.is_empty() {
        d.line("- pending permission / input signals: none");
    } else {
        for g in pending.iter().take(MAX_PENDING.min(max_rows)) {
            d.line(format!(
                "- pending signal: {}{} raised {} with no later event [{}]",
                g.kind.as_str(),
                g.signal_type
                    .as_deref()
                    .map(|t| format!(" ({})", clip(t, 30)))
                    .unwrap_or_default(),
                ts(g.at),
                id(&g.event_id)
            ));
        }
    }
    match p.why_blocked(sid) {
        Some(e) => d.line(format!(
            "- blocked: yes — {} (confidence {}; evidence: {})",
            clip(&e.claim, 300),
            e.confidence,
            ids(&e.evidence, 6)
        )),
        None => d.line("- blocked: no (no uncleared input signal, no repeated identical failure)"),
    }

    // --- uncertainty ------------------------------------------------------
    d.blank();
    uncertainty(&mut d, ready, Some(latest), &shown_sessions, content);
    d.finish()
}

fn uncertainty(
    d: &mut Doc,
    ready: &Ready<'_>,
    latest: Option<&Session>,
    shown: &[&Session],
    content: bool,
) {
    let view = ready.view;
    let p = view.engine.projection();
    let st = &view.status;
    d.line("## Uncertainty");
    d.line(format!(
        "- inference {}: sessions, turns, attempts and handoffs are Tier 1 heuristics over hook events, never ground truth; verify any claim with attempt_evidence <id> or attempt_trace <id>",
        p.algorithm_version
    ));
    if shown.is_empty() {
        d.line("- coverage: no session to grade");
    } else {
        d.line(format!(
            "- coverage: {}",
            shown
                .iter()
                .map(|s| format!("{} {}", id(&s.session_id), coverage_note(s)))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let (scoped_captured, scoped_reconstructed) = p.sessions.iter().fold((0, 0), |acc, s| {
        let c = view
            .session_capture
            .get(&s.session_id)
            .copied()
            .unwrap_or_default();
        (acc.0 + c.captured, acc.1 + c.reconstructed)
    });
    let mut events_line = format!(
        "- events: {} in scope — {} hook-captured, {} reconstructed from transcripts (database-wide: {} captured, {} reconstructed)",
        view.engine.event_count(),
        scoped_captured,
        scoped_reconstructed,
        st.captured_events,
        st.reconstructed_events
    );
    if let Some(s) = latest {
        let c = view
            .session_capture
            .get(&s.session_id)
            .copied()
            .unwrap_or_default();
        let _ = write!(
            events_line,
            "; latest session: {} captured, {} reconstructed",
            c.captured, c.reconstructed
        );
    }
    d.line(events_line);
    let stats = &p.stats;
    d.line(format!(
        "- pairing: {} tool start(s) never finished, {} finish(es) without a start, {} FIFO pairing(s), {} unknown event(s), {} out-of-order event(s), {} injected prompt(s) skipped",
        stats.unpaired_tool_starts,
        stats.unpaired_tool_finishes,
        stats.fifo_pairings,
        stats.unknown_events,
        stats.out_of_order_events,
        stats.injected_prompts
    ));
    if let Some(s) = latest {
        let confs: Vec<f32> = p.attempts_of(s.session_id).map(|a| a.confidence).collect();
        if confs.is_empty() {
            d.line("- attempt confidence: no attempts in the latest session");
        } else {
            let min = confs.iter().copied().fold(f32::MAX, f32::min);
            let max = confs.iter().copied().fold(f32::MIN, f32::max);
            d.line(format!(
                "- attempt confidence: {min}–{max} over {} in the latest session (0.9 = call-id pairing + explicit stop; 0.6 = FIFO/unpaired calls or missing stop; 0.4 = minimal coverage)",
                plural(confs.len(), "attempt")
            ));
        }
    }
    let with_objective = p.turns.iter().filter(|t| t.objective.is_some()).count();
    d.line(format!(
        "- content: {}",
        if content {
            format!(
                "prompt text available for {} of {} turns (capture mode {}); commands and tool output are stored locally but not included here",
                with_objective,
                p.turns.len(),
                st.capture_mode.as_str()
            )
        } else {
            format!(
                "no prompt text (capture mode {}): objectives are prompt sizes; commands and tool output are not stored",
                st.capture_mode.as_str()
            )
        }
    ));
    d.line(format!(
        "- freshness: database read at {}{}; anything done outside the hook surface (edits without an agent, terminal commands, other machines) is invisible",
        ts(st.loaded_at),
        if st.read_only && !st.snapshot {
            " (read-only: the daemon or another CLI holds the writer lock; its acknowledged events are visible through the WAL)"
        } else {
            ""
        }
    ));
}
