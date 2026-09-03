//! HTML building blocks. Everything rendered from the database is untrusted
//! text (prompts, paths, tool names, provider strings): it goes through
//! [`esc`] before it reaches a page, ids are validated before they become
//! hrefs, and query values are percent-encoded.

use crate::INFERENCE_VERSION;
use crate::scope::ScopeQuery;
use crate::store::View;
use attemptdb_core::Timestamp;
use attemptdb_project::{AttemptOutcome, CoverageGrade, TurnStatus};
use attemptdb_query::PrefixedId;
use std::fmt::Write as _;

/// HTML-escape untrusted text. Control characters (except newline and tab)
/// are replaced so nothing can smuggle terminal or bidi tricks either.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if c.is_control() && c != '\n' && c != '\t' => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

/// Percent-encode a query-string value (everything but unreserved
/// characters).
pub fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// A path segment built from an id: only `[A-Za-z0-9_-]` pass through,
/// anything else is percent-encoded.
pub fn seg(s: &str) -> String {
    urlenc(s)
}

/// One-line, clipped rendering of untrusted text (escaped).
pub fn clip(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return esc(&one_line);
    }
    let mut out: String = one_line.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    esc(&out)
}

/// `2026-08-28 08:00:05Z` (seconds precision, UTC).
pub fn ts(t: Timestamp) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_micros(t.as_micros())
        .map(|d| d.format("%Y-%m-%d %H:%M:%SZ").to_string())
        .unwrap_or_else(|| t.to_string())
}

/// `08:00:05` (time of day, UTC).
pub fn ts_time(t: Timestamp) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_micros(t.as_micros())
        .map(|d| d.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| t.to_string())
}

/// RFC 3339 seconds precision, for `datetime` attributes and statements.
pub fn rfc3339(t: Timestamp) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_micros(t.as_micros())
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| t.to_rfc3339())
}

pub fn duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 3_600_000 {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    } else {
        format!("{}h{:02}m", ms / 3_600_000, (ms % 3_600_000) / 60_000)
    }
}

pub fn elapsed_ms(start: Timestamp, end: Timestamp) -> u64 {
    (end.as_millis() - start.as_millis()).max(0) as u64
}

pub fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

pub fn id<T: PrefixedId>(x: &T) -> String {
    x.readable()
}

/// `att_0191e3a2` — the prefix and the first eight hex digits.
pub fn short_id<T: PrefixedId>(x: &T) -> String {
    format!("{}{}", T::PREFIX, &x.hex()[..8])
}

pub fn badge(class: &str, text: &str) -> String {
    format!(
        "<span class=\"badge badge-{}\">{}</span>",
        esc(class),
        esc(text)
    )
}

pub fn outcome_badge(o: AttemptOutcome) -> String {
    let (class, glyph) = match o {
        AttemptOutcome::Succeeded => ("ok", "✓ succeeded"),
        AttemptOutcome::Failed => ("fail", "✗ failed"),
        AttemptOutcome::Superseded => ("sup", "↻ superseded"),
        AttemptOutcome::Abandoned => ("warn", "… abandoned"),
        AttemptOutcome::InProgress => ("live", "▶ in progress"),
        AttemptOutcome::Unknown => ("muted", "? unknown"),
    };
    badge(class, glyph)
}

pub fn turn_badge(s: TurnStatus) -> String {
    match s {
        TurnStatus::Completed => badge("ok", "completed"),
        TurnStatus::Failed => badge("fail", "failed"),
        TurnStatus::InProgress => badge("live", "in progress"),
        TurnStatus::Unknown => badge("muted", "no stop seen"),
    }
}

pub fn coverage_badge(c: CoverageGrade) -> String {
    let class = match c {
        CoverageGrade::Full => "ok",
        CoverageGrade::Partial => "warn",
        CoverageGrade::Minimal => "warn",
        CoverageGrade::Unknown => "muted",
    };
    badge(class, &format!("{} coverage", c.as_str()))
}

pub fn status_class(status: &str) -> &'static str {
    match status {
        "success" => "ok",
        "failure" => "fail",
        "denied" => "warn",
        "cancelled" => "warn",
        _ => "muted",
    }
}

/// Link helpers: the id is rendered from the typed value, so it is always a
/// well-formed `prefix_uuid`; the scope query string is percent-encoded.
pub fn attempt_link<T: PrefixedId>(id: &T, scope: &ScopeQuery) -> String {
    format!(
        "<a class=\"id\" href=\"/attempt/{}{}\">{}</a>",
        seg(&id.readable()),
        scope.query_string(&[]),
        short_id(id)
    )
}

pub fn session_link<T: PrefixedId>(id: &T, scope: &ScopeQuery) -> String {
    format!(
        "<a class=\"id\" href=\"/session/{}{}\">{}</a>",
        seg(&id.readable()),
        scope.query_string(&[]),
        short_id(id)
    )
}

pub fn evidence_link<T: PrefixedId>(id: &T, scope: &ScopeQuery) -> String {
    format!(
        "<a class=\"id ev\" href=\"/evidence/{}{}\">{}</a>",
        seg(&id.readable()),
        scope.query_string(&[]),
        short_id(id)
    )
}

/// A link for a readable id string coming out of a query result
/// (`ev_…`, `att_…`, `ses_…`, `spn_…`, `trn_…`). Unknown prefixes render as
/// text.
pub fn id_link(text: &str, scope: &ScopeQuery) -> String {
    let t = text.trim();
    let hexish = |s: &str| s.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    let short = |s: &str| {
        let hex: String = s.chars().filter(|c| *c != '-').collect();
        hex[..hex.len().min(8)].to_string()
    };
    let (page, rest) = if let Some(r) = t.strip_prefix("ev_") {
        ("evidence", r)
    } else if let Some(r) = t.strip_prefix("att_") {
        ("attempt", r)
    } else if let Some(r) = t.strip_prefix("ses_") {
        ("session", r)
    } else {
        return format!("<code>{}</code>", esc(t));
    };
    if !hexish(rest) || rest.len() < 4 {
        return format!("<code>{}</code>", esc(t));
    }
    format!(
        "<a class=\"id\" href=\"/{page}/{}{}\" title=\"{}\">{}{}</a>",
        seg(t),
        scope.query_string(&[]),
        esc(t),
        esc(&t[..t.find('_').map(|i| i + 1).unwrap_or(0)]),
        short(rest)
    )
}

/// Up to `max` evidence links then `(+n more)`.
pub fn evidence_links<T: PrefixedId>(ids: &[T], max: usize, scope: &ScopeQuery) -> String {
    if ids.is_empty() {
        return "<span class=\"muted\">no evidence</span>".to_string();
    }
    let mut s: String = ids
        .iter()
        .take(max)
        .map(|i| evidence_link(i, scope))
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

pub fn confidence(c: f32) -> String {
    format!("<span class=\"conf\" title=\"confidence\">conf {c}</span>")
}

/// A table from escaped header and cell HTML.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut s = String::from("<div class=\"scroll\"><table><thead><tr>");
    for h in headers {
        let _ = write!(s, "<th>{}</th>", esc(h));
    }
    s.push_str("</tr></thead><tbody>");
    for row in rows {
        s.push_str("<tr>");
        for cell in row {
            let _ = write!(s, "<td>{cell}</td>");
        }
        s.push_str("</tr>");
    }
    s.push_str("</tbody></table></div>");
    s
}

pub fn key_values(rows: &[(&str, String)]) -> String {
    let mut s = String::from("<dl class=\"kv\">");
    for (k, v) in rows {
        let _ = write!(s, "<dt>{}</dt><dd>{v}</dd>", esc(k));
    }
    s.push_str("</dl>");
    s
}

pub fn notes(notes: &[String]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut s = String::from("<ul class=\"notes\">");
    for n in notes {
        let _ = write!(s, "<li>{}</li>", esc(n));
    }
    s.push_str("</ul>");
    s
}

/// The nav entries: `(href path, label)`.
pub const NAV: &[(&str, &str)] = &[
    ("/", "Overview"),
    ("/timeline", "Timeline"),
    ("/work", "Work"),
    ("/attention", "Needs You"),
    ("/failures", "Failures"),
    ("/handoffs", "Handoffs"),
    ("/why", "Why"),
    ("/state", "State"),
    ("/query", "Query"),
];

/// Wrap `body` in the shared chrome: header with database facts, nav, scope
/// bar, footer.
pub fn layout(view: &View, scope: &ScopeQuery, title: &str, active: &str, body: &str) -> String {
    let st = &view.status;
    // The queue's size is part of the navigation: a badge here is the only
    // place a waiting agent is visible from every page.
    let attention = view
        .engine
        .projection()
        .attention_at(Timestamp::now(), attemptdb_project::DEFAULT_MIN_CONFIDENCE)
        .len();
    let mut nav = String::new();
    for (href, label) in NAV {
        let count = if *href == "/attention" && attention > 0 {
            format!(" <span class=\"nav-count\">{attention}</span>")
        } else {
            String::new()
        };
        let _ = write!(
            nav,
            "<a href=\"{}{}\"{}>{}{}</a>",
            href,
            scope.without_session().query_string(&[]),
            if *href == active {
                " class=\"active\""
            } else {
                ""
            },
            label,
            count
        );
    }
    let ro = if st.snapshot {
        badge("muted", "snapshot · read-only")
    } else if st.read_only {
        badge("warn", "read-only (another writer holds the lock)")
    } else {
        badge("ok", "writer")
    };
    let daemon = match st.daemon.state() {
        "running" => badge("ok", &format!("daemon {}", st.daemon.label())),
        "n/a" => String::new(),
        "not_running" => badge("muted", "daemon not running"),
        _ => badge("warn", &format!("daemon {}", st.daemon.label())),
    };
    let capture = badge(
        match st.capture_mode {
            attemptdb_core::CaptureMode::MetadataOnly => "muted",
            attemptdb_core::CaptureMode::LocalSemantic => "ok",
            attemptdb_core::CaptureMode::FullSync => "warn",
        },
        &format!("capture {}", st.capture_mode.as_str()),
    );
    let mut projects = String::new();
    let _ = write!(
        projects,
        "<option value=\"\"{}>current repository</option><option value=\"__all__\"{}>all projects</option>",
        if scope.project.as_deref().unwrap_or("").is_empty() && !scope.all_projects() {
            " selected"
        } else {
            ""
        },
        if scope.all_projects() {
            " selected"
        } else {
            ""
        }
    );
    for p in &st.projects {
        let _ = write!(
            projects,
            "<option value=\"{}\"{}>{}</option>",
            esc(&p.name),
            if scope.project.as_deref() == Some(p.name.as_str()) {
                " selected"
            } else {
                ""
            },
            esc(&p.name)
        );
    }
    // Demo mode is stated on every page: nothing here was captured on this
    // machine, and the database it comes from is a different one.
    let demo_banner = if st.demo {
        let mut leave = scope.without_session();
        leave.demo = None;
        format!(
            "<div class=\"demo-banner\"><span>Demo data — a synthesized AttemptDB build history, not captured on this machine. Every event is marked <code>reconstructed</code> and lives in a separate database.</span><a href=\"/{}\">leave the demo</a></div>",
            leave.query_string(&[])
        )
    } else {
        String::new()
    };
    let session_hidden = scope
        .session
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| {
            format!(
                "<input type=\"hidden\" name=\"session\" value=\"{}\">",
                esc(s)
            )
        })
        .unwrap_or_default();
    let default_reason = view
        .scope
        .default_reason
        .as_deref()
        .map(|r| {
            format!(
                "<span class=\"muted small\" title=\"{}\">(default)</span>",
                esc(r)
            )
        })
        .unwrap_or_default();
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · AttemptDB</title>
<link rel="stylesheet" href="/assets/app.css">
</head>
<body>
{demo_banner}
<header class="top">
  <div class="brand"><a href="/{scope_qs}">AttemptDB</a> <span class="sub">AgentTimeline</span></div>
  <nav>{nav}</nav>
</header>
<div class="facts">
  <span class="fact" title="database"><span class="k">database</span> <code>{source}</code></span>
  {ro} {daemon} {capture}
  <span class="fact"><span class="k">inference</span> <code>{version}</code></span>
  <span class="fact live-wrap" id="live-wrap" data-scope="{scope_qs}" hidden><span class="k">updates</span> <span id="live-state" class="live-state">connecting…</span> <button type="button" id="live-pause" class="ghost">pause</button></span>
  <span class="tagline">{tagline}</span>
</div>
<form class="scope" method="get" action="{active}">
  <label>project <select name="project" data-all-value="__all__">{projects}</select></label>
  <input type="hidden" name="all" value="{all}">
  {demo_hidden}
  {session_hidden}
  <label>since <input name="since" value="{since}" placeholder="-2h, today, 2026-08-28" size="14"></label>
  <label>until <input name="until" value="{until}" placeholder="now" size="14"></label>
  <label><input type="checkbox" name="captured_only" value="1"{captured}> hook-captured only</label>
  <button type="submit">Apply</button>
  <span class="scope-label">scope: {scope_label} {default_reason}</span>
</form>
<main>
{body}
</main>
<footer>
  <span>{events} in scope · {sessions} · {attempts} · read at {loaded}</span>
  <span class="tagline">{tagline}</span>
</footer>
<script src="/assets/app.js"></script>
</body>
</html>
"#,
        title = esc(title),
        demo_banner = demo_banner,
        scope_qs = scope.without_session().query_string(&[]),
        nav = nav,
        source = esc(&st.source),
        ro = ro,
        daemon = daemon,
        capture = capture,
        version = esc(INFERENCE_VERSION),
        tagline = esc(crate::TAGLINE),
        active = esc(active),
        projects = projects,
        all = if scope.all_projects() { "1" } else { "" },
        demo_hidden = if st.demo {
            "<input type=\"hidden\" name=\"demo\" value=\"1\">"
        } else {
            ""
        },
        session_hidden = session_hidden,
        since = esc(scope.since.as_deref().unwrap_or("")),
        until = esc(scope.until.as_deref().unwrap_or("")),
        captured = if scope.captured_only() {
            " checked"
        } else {
            ""
        },
        scope_label = esc(&view.scope.label),
        default_reason = default_reason,
        body = body,
        events = plural(view.engine.event_count(), "event"),
        sessions = plural(view.engine.projection().sessions.len(), "session"),
        attempts = plural(view.engine.projection().attempts.len(), "attempt"),
        loaded = ts(st.loaded_at),
    )
}

/// A page without database facts (errors before the database could be
/// opened, the 401 page).
pub fn bare(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · AttemptDB</title>
<link rel="stylesheet" href="/assets/app.css">
</head>
<body>
<header class="top"><div class="brand"><a href="/">AttemptDB</a> <span class="sub">AgentTimeline</span></div></header>
<main>{body}</main>
</body>
</html>
"#,
        title = esc(title),
        body = body
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_everything_dangerous() {
        assert_eq!(
            esc("<script>alert('x')</script>&\"\u{1b}"),
            "&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;&amp;&quot;\u{FFFD}"
        );
        assert_eq!(urlenc("a b/c?d=é"), "a%20b%2Fc%3Fd%3D%C3%A9");
        assert_eq!(clip("a  b\nc", 10), "a b c");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(duration(1500), "1.5s");
    }

    #[test]
    fn id_links_only_for_well_formed_ids() {
        let s = ScopeQuery::default();
        assert!(id_link("att_0191e3a2-0000-7000-8000-000000000000", &s).starts_with("<a "));
        assert_eq!(id_link("att_<x>", &s), "<code>att_&lt;x&gt;</code>");
        assert_eq!(id_link("prj_abcd", &s), "<code>prj_abcd</code>");
    }
}
