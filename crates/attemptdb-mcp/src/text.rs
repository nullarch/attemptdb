//! Compact text rendering shared by the tools. Everything rendered from the
//! database is untrusted text (prompts, paths, tool names): control
//! characters are stripped and long values are clipped.

use attemptdb_core::Timestamp;
use attemptdb_query::{PrefixedId, QueryResult, ResultKind};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;
use std::fmt::Write as _;

/// Seconds-precision RFC 3339, always UTC.
pub fn ts(t: Timestamp) -> String {
    DateTime::<Utc>::from_timestamp_micros(t.as_micros())
        .map(|d| d.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| t.to_string())
}

/// `start → end`, with the end reduced to its time of day when both fall on
/// the same UTC date.
pub fn span(start: Timestamp, end: Option<Timestamp>) -> String {
    let s = ts(start);
    match end {
        None => format!("{s} → open"),
        Some(e) => {
            let e = ts(e);
            if s.len() == e.len() && s[..10] == e[..10] {
                format!("{s} → {}", &e[11..])
            } else {
                format!("{s} → {e}")
            }
        }
    }
}

/// One line, control characters removed, whitespace collapsed, clipped to
/// `max` characters with an ellipsis.
pub fn clip(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let one_line = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let mut out: String = one_line.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
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

/// `prefix + hyphenated uuid` (`att_0191…`), the form every tool accepts.
pub fn id<T: PrefixedId>(x: &T) -> String {
    x.readable()
}

pub fn id_opt<T: PrefixedId>(x: &Option<T>) -> Option<String> {
    x.as_ref().map(id)
}

pub fn id_vec<T: PrefixedId>(list: &[T]) -> Vec<String> {
    list.iter().map(id).collect()
}

/// Up to `max` ids joined by `, `, then `(+n more)`.
pub fn ids<T: PrefixedId>(list: &[T], max: usize) -> String {
    if list.is_empty() {
        return "none".to_string();
    }
    let mut s = list.iter().take(max).map(id).collect::<Vec<_>>().join(", ");
    if list.len() > max {
        let _ = write!(s, " (+{} more)", list.len() - max);
    }
    s
}

pub fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// A `f32` confidence as the short JSON number people expect (`0.9`).
pub fn conf(c: f32) -> Value {
    c.to_string()
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// Text form of a JSON cell: strings raw, arrays joined, null empty.
pub fn cell_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(cell_text).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

fn is_blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

/// Explanation-style rows as numbered key/value records; blank cells are
/// skipped so a 20-column `STATE` row stays readable.
pub fn records(r: &QueryResult, max_rows: usize) -> String {
    let rows = r.to_json();
    let rows = rows.as_array().cloned().unwrap_or_default();
    let mut out = String::new();
    for (i, row) in rows.iter().take(max_rows).enumerate() {
        let Some(obj) = row.as_object() else { continue };
        let _ = writeln!(out, "[{}]", i + 1);
        for (k, v) in obj {
            if is_blank(v) {
                continue;
            }
            let _ = writeln!(out, "  {k}: {}", clip(&cell_text(v), 1200));
        }
    }
    let _ = write!(out, "({}", plural(rows.len(), "row"));
    if rows.len() > max_rows {
        let _ = write!(out, ", first {max_rows} shown");
    }
    out.push(')');
    out
}

/// Row-style results as a pipe table with clipped cells.
pub fn table(r: &QueryResult, max_rows: usize) -> String {
    let names = r.column_names();
    let cells = r.cells();
    let mut out = String::new();
    if !names.is_empty() {
        let _ = writeln!(out, "{}", names.join(" | "));
    }
    for row in cells.iter().take(max_rows) {
        let _ = writeln!(
            out,
            "{}",
            row.iter()
                .map(|c| clip(c, 100))
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    let _ = write!(out, "({}", plural(cells.len(), "row"));
    if cells.len() > max_rows {
        let _ = write!(out, ", first {max_rows} shown");
    }
    out.push(')');
    out
}

/// Render a result the way the CLI would: records for explanations, a
/// table for rows, `(no rows)` for empty results, then the notes.
pub fn result_text(r: &QueryResult, max_rows: usize) -> String {
    let mut out = if r.row_count() == 0 && matches!(r.kind, ResultKind::Empty) {
        "(no rows)".to_string()
    } else if matches!(r.kind, ResultKind::Explanation) {
        records(r, max_rows)
    } else {
        table(r, max_rows)
    };
    for n in &r.notes {
        let _ = write!(out, "\nnote: {}", clip(n, 600));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_and_span() {
        assert_eq!(clip("a\tb\n\x1bc", 10), "a b c");
        assert_eq!(clip("abcdef", 4), "abc…");
        let t = Timestamp::from_micros(1_787_904_000_000_000);
        assert_eq!(ts(t), "2026-08-28T08:00:00Z");
        assert_eq!(
            span(t, Some(Timestamp::from_micros(t.as_micros() + 5_000_000))),
            "2026-08-28T08:00:00Z → 08:00:05Z"
        );
        assert_eq!(span(t, None), "2026-08-28T08:00:00Z → open");
        assert_eq!(duration(1500), "1.5s");
        assert_eq!(conf(0.9), Value::from(0.9));
    }
}
