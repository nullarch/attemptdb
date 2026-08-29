//! Small terminal rendering helpers. Everything displayed from the database
//! is untrusted: we strip control characters so a tool output or prompt can
//! never inject terminal escape sequences.

use attemptdb_core::Timestamp;

/// Remove control characters (including ESC) from untrusted text.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() && c != '\t' { '\u{FFFD}' } else { c })
        .collect::<String>()
        .replace('\t', "  ")
}

pub fn truncate(s: &str, max: usize) -> String {
    let s = sanitize(s).replace('\n', " ⏎ ");
    if s.chars().count() <= max {
        s
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Local-time short timestamp `YYYY-MM-DD HH:MM:SS`.
pub fn ts_local(t: Timestamp) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_micros(t.as_micros())
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| t.to_string())
}

pub fn ts_time(t: Timestamp) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_micros(t.as_micros())
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
        .unwrap_or_else(|| t.to_string())
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

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

pub fn print_json<T: serde::Serialize>(v: &T) {
    match serde_json::to_string_pretty(v) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: cannot serialise output: {e}"),
    }
}
