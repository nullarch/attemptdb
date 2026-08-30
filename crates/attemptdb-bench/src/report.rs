//! Markdown tables from a results file, so `docs/benchmarks.md` never
//! contains a hand-transcribed number.

use crate::stats::human_bytes;
use serde_json::Value;

fn f(v: &Value, path: &[&str]) -> Option<f64> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_f64()
}

fn s(v: &Value, path: &[&str]) -> String {
    let mut cur = v;
    for p in path {
        match cur.get(p) {
            Some(c) => cur = c,
            None => return "—".into(),
        }
    }
    match cur {
        Value::String(s) => s.clone(),
        Value::Null => "—".into(),
        other => other.to_string(),
    }
}

fn ms(us: Option<f64>) -> String {
    match us {
        Some(u) if u >= 1e6 => format!("{:.2} s", u / 1e6),
        Some(u) if u >= 1e3 => format!("{:.2} ms", u / 1e3),
        Some(u) => format!("{u:.0} µs"),
        None => "—".into(),
    }
}

fn secs(v: Option<f64>) -> String {
    match v {
        Some(x) if x >= 60.0 => format!("{:.0} min {:.0} s", (x / 60.0).floor(), x % 60.0),
        Some(x) => format!("{x:.2} s"),
        None => "—".into(),
    }
}

fn bytes(v: Option<f64>) -> String {
    v.map(human_bytes).unwrap_or_else(|| "—".into())
}

fn num(v: Option<f64>) -> String {
    match v {
        Some(x) if x >= 100.0 => {
            let n = x.round() as i64;
            let mut out = String::new();
            let digits = n.abs().to_string();
            for (i, c) in digits.chars().enumerate() {
                if i > 0 && (digits.len() - i).is_multiple_of(3) {
                    out.push(',');
                }
                out.push(c);
            }
            if n < 0 {
                out.insert(0, '-');
            }
            out
        }
        Some(x) if (x - x.round()).abs() < 1e-9 => format!("{}", x.round() as i64),
        Some(x) => format!("{x:.2}"),
        None => "—".into(),
    }
}

fn pct(v: Option<f64>) -> String {
    v.map(|x| format!("{:.1}%", x * 100.0))
        .unwrap_or_else(|| "—".into())
}

fn lat(v: &Value, path: &[&str]) -> String {
    let base: Vec<&str> = path.to_vec();
    let get = |k: &str| {
        let mut p = base.clone();
        p.push(k);
        f(v, &p)
    };
    if get("n").is_none() {
        return "—".into();
    }
    format!(
        "{} / {} / {}",
        ms(get("p50_us")),
        ms(get("p95_us")),
        ms(get("p99_us"))
    )
}

fn status_note(step: &Value) -> Option<String> {
    let status = s(step, &["status"]);
    if status == "ok" {
        None
    } else {
        Some(format!("{status}: {}", s(step, &["reason"])))
    }
}

struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    fn new(header: &[&str]) -> Self {
        Self {
            header: header.iter().map(|h| (*h).to_string()).collect(),
            rows: Vec::new(),
        }
    }

    fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("| ");
        out.push_str(&self.header.join(" | "));
        out.push_str(" |\n|");
        for _ in &self.header {
            out.push_str("---|");
        }
        out.push('\n');
        for r in &self.rows {
            out.push_str("| ");
            out.push_str(&r.join(" | "));
            out.push_str(" |\n");
        }
        out
    }
}

fn section(out: &mut String, title: &str, table: &Table, notes: &[String]) {
    out.push_str(&format!("### {title}\n\n"));
    out.push_str(&table.render());
    for n in notes {
        out.push_str(&format!("\n> {n}\n"));
    }
    out.push('\n');
}

pub fn render(results: &Value) -> String {
    let mut out = String::new();
    let steps = results.get("steps").cloned().unwrap_or(Value::Null);
    let step = |name: &str| steps.get(name).cloned().unwrap_or(Value::Null);

    // Machine.
    let m = results.get("machine").cloned().unwrap_or(Value::Null);
    let mut t = Table::new(&["Item", "Value"]);
    t.row(vec!["CPU".into(), s(&m, &["cpu"])]);
    t.row(vec!["Logical CPUs".into(), s(&m, &["logical_cpus"])]);
    t.row(vec!["Memory".into(), bytes(f(&m, &["memory_bytes"]))]);
    t.row(vec!["Disk".into(), s(&m, &["disk"])]);
    t.row(vec![
        "OS".into(),
        format!("{} ({})", s(&m, &["os_version"]), s(&m, &["arch"])),
    ]);
    t.row(vec!["rustc".into(), s(&m, &["rustc"])]);
    t.row(vec!["Commit".into(), format!("`{}`", s(&m, &["commit"]))]);
    t.row(vec!["Build profile".into(), s(&m, &["profile"])]);
    t.row(vec![
        "`attempt` binary".into(),
        format!(
            "`{}` ({})",
            s(&m, &["attempt_binary"]),
            bytes(f(&m, &["attempt_binary_bytes"]))
        ),
    ]);
    let params = results.get("params").cloned().unwrap_or(Value::Null);
    t.row(vec![
        "Run".into(),
        format!(
            "{} events, seed {}, time cap {} s per step, RSS cap {}, started {}",
            num(f(&params, &["events"])),
            s(&params, &["seed"]),
            s(&params, &["time_cap_secs"]),
            bytes(f(&params, &["rss_cap_bytes"])),
            s(results, &["started_at"])
        ),
    ]);
    section(&mut out, "Machine", &t, &[]);

    // Workload mix.
    let full = step("ingest_strict_full");
    if let Some(counts) = full.get("kind_counts").and_then(Value::as_object) {
        let total: f64 = counts.values().filter_map(Value::as_f64).sum();
        let mut t = Table::new(&["Kind", "Generated", "Share", "Sampled share"]);
        let mut rows: Vec<(&String, f64)> = counts
            .iter()
            .map(|(k, v)| (k, v.as_f64().unwrap_or(0.0)))
            .collect();
        rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (k, n) in rows {
            let sampled = crate::model::SAMPLED_KIND_MIX
                .iter()
                .find(|(name, _)| name == k)
                .map(|(_, p)| pct(Some(*p)))
                .unwrap_or_else(|| "—".into());
            t.row(vec![
                format!("`{k}`"),
                num(Some(n)),
                pct(Some(n / total.max(1.0))),
                sampled,
            ]);
        }
        section(
            &mut out,
            "Generated kind mix versus the sampled mix",
            &t,
            &[format!(
                "{} sessions were generated for the full run.",
                s(&full, &["sessions_started"])
            )],
        );
    }

    // Ingest.
    let mut t = Table::new(&[
        "Run",
        "Durability",
        "Events",
        "Ingest events/s",
        "Wall events/s",
        "Batch p50 / p95 / p99",
        "Flushes",
        "Segments on disk",
        "Manifests on disk",
        "Bytes/event (segments)",
        "WAL→segment ratio",
        "Peak RSS",
        "Note",
    ]);
    let mut notes = Vec::new();
    let mut names: Vec<String> = steps
        .as_object()
        .map(|o| {
            o.keys()
                .filter(|k| k.starts_with("ingest_"))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    names.sort_by_key(|k| {
        let st = step(k);
        (
            st.get("reader").is_some(),
            f(&st, &["events_requested"]).unwrap_or(0.0) as u64,
            k.clone(),
        )
    });
    for name in &names {
        let st = step(name);
        if let Some(n) = status_note(&st) {
            notes.push(format!("`{name}` {n}"));
        }
        t.row(vec![
            format!("`{name}`"),
            s(&st, &["durability"]),
            num(f(&st, &["events_ingested"])),
            num(f(&st, &["events_per_sec_ingest"])),
            num(f(&st, &["events_per_sec_wall"])),
            lat(&st, &["batch_latency"]),
            num(f(&st, &["flushes"])),
            bytes(f(&st, &["disk", "segments_bytes"])),
            bytes(f(&st, &["disk", "manifest_bytes"])),
            bytes(f(&st, &["segment_bytes_per_event"])),
            f(&st, &["compression_ratio_wal_to_segments"])
                .map(|r| format!("{r:.1}×"))
                .unwrap_or_else(|| "—".into()),
            bytes(f(&st, &["peak_rss_bytes"])),
            if st.get("reader").is_some() {
                "with concurrent reader".into()
            } else if st.get("capped") == Some(&Value::Bool(true)) {
                "stopped at time cap".into()
            } else {
                String::new()
            },
        ]);
    }
    section(&mut out, "Sustained ingest (batches of 100)", &t, &notes);

    // Concurrent reader.
    for name in &names {
        let st = step(name);
        let Some(r) = st.get("reader") else { continue };
        let mut t = Table::new(&["Reader metric", "Value"]);
        t.row(vec![
            "Iterations (one per second when it keeps up)".into(),
            s(r, &["iterations"]),
        ]);
        t.row(vec!["Errors".into(), s(r, &["errors"])]);
        t.row(vec![
            "Most events seen by one scan".into(),
            num(f(r, &["max_events_seen"])),
        ]);
        t.row(vec![
            "Open (read-only, WAL replay) p50 / p95 / p99".into(),
            lat(r, &["open"]),
        ]);
        t.row(vec!["Scan all p50 / p95 / p99".into(), lat(r, &["scan"])]);
        t.row(vec!["Project p50 / p95 / p99".into(), lat(r, &["project"])]);
        t.row(vec![
            "Open + scan + project p50 / p95 / p99".into(),
            lat(r, &["open_scan_project"]),
        ]);
        let baseline_name = name.trim_end_matches("_reader");
        let baseline = step(baseline_name);
        if let (Some(a), Some(b)) = (
            f(&baseline, &["events_per_sec_ingest"]),
            f(&st, &["events_per_sec_ingest"]),
        ) {
            t.row(vec![
                "Ingest events/s without / with reader".into(),
                format!(
                    "{} / {} ({:+.1}%)",
                    num(Some(a)),
                    num(Some(b)),
                    (b - a) / a * 100.0
                ),
            ]);
        }
        section(
            &mut out,
            &format!("Concurrent reader during `{name}`"),
            &t,
            &[],
        );
    }

    // WAL latency.
    let w = step("wal_latency");
    let mut t = Table::new(&["Path", "Samples", "p50 / p95 / p99", "Max"]);
    for (label, key) in [
        (
            "`Database::ingest`, one event, Strict (F_FULLFSYNC per call)",
            "ingest_strict",
        ),
        (
            "`Database::ingest`, one event, Relaxed (no sync)",
            "ingest_relaxed",
        ),
        (
            "`SpoolWriter::append`, one event, sync off (hook default)",
            "spool_append_nosync",
        ),
        (
            "`SpoolWriter::append`, one event, sync on",
            "spool_append_sync",
        ),
    ] {
        let st = w.get(key).cloned().unwrap_or(Value::Null);
        t.row(vec![
            label.into(),
            s(&st, &["samples"]),
            lat(&st, &["latency"]),
            ms(f(&st, &["latency", "max_us"])),
        ]);
    }
    let floor = w.get("fsync_floor").cloned().unwrap_or(Value::Null);
    for (label, key) in [
        (
            "4 KiB append + `File::sync_data` (std → F_FULLFSYNC on macOS)",
            "sync_data",
        ),
        ("4 KiB append + `fsync(2)`", "fsync"),
        ("4 KiB append + `fcntl(F_FULLFSYNC)`", "f_fullfsync"),
    ] {
        if floor.get(key).is_some() {
            t.row(vec![
                label.into(),
                s(&floor, &["samples"]),
                lat(&floor, &[key]),
                ms(f(&floor, &[key, "max_us"])),
            ]);
        }
    }
    let mut notes = Vec::new();
    if let Some(n) = status_note(&w) {
        notes.push(n);
    }
    if let Some(b) = f(&w, &["ingest_strict", "event_bytes_mean"]) {
        notes.push(format!(
            "Single-event samples use synthetic `tool_call_finished` shell events averaging {} of JSON.",
            bytes(Some(b))
        ));
    }
    section(&mut out, "WAL acknowledgement latency", &t, &notes);

    // Hook.
    let h = step("hook");
    let mut t = Table::new(&[
        "Path",
        "Spawns",
        "Wall p50 / p95 / p99",
        "Wall max",
        "In-process hook_us p50 / p95 / p99",
        "Events durable after",
    ]);
    t.row(vec![
        "`/usr/bin/true` (fork+exec floor)".into(),
        s(&h, &["spawn_floor_true", "n"]),
        lat(&h, &["spawn_floor_true"]),
        ms(f(&h, &["spawn_floor_true", "max_us"])),
        "—".into(),
        "—".into(),
    ]);
    t.row(vec![
        "`attempt --version` (binary load floor)".into(),
        s(&h, &["spawn_floor_attempt_version", "n"]),
        lat(&h, &["spawn_floor_attempt_version"]),
        ms(f(&h, &["spawn_floor_attempt_version", "max_us"])),
        "—".into(),
        "—".into(),
    ]);
    for (label, key) in [
        (
            "`attempt hook claude-code`, no daemon (spool append)",
            "spool",
        ),
        (
            "`attempt hook claude-code`, daemon Strict (IPC + F_FULLFSYNC)",
            "daemon_strict",
        ),
        (
            "`attempt hook claude-code`, daemon `--relaxed` (IPC, no sync)",
            "daemon_relaxed",
        ),
    ] {
        let st = h.get(key).cloned().unwrap_or(Value::Null);
        t.row(vec![
            label.into(),
            s(&st, &["spawns"]),
            lat(&st, &["wall"]),
            ms(f(&st, &["wall", "max_us"])),
            lat(&st, &["in_process_hook_us"]),
            format!(
                "{} of {}",
                s(&st, &["events_durable_after"]),
                s(&st, &["expected_events"])
            ),
        ]);
    }
    let mut notes = Vec::new();
    if let Some(n) = status_note(&h) {
        notes.push(n);
    }
    notes.push(format!(
        "Binary: `{}` ({}); payload {} bytes.",
        s(&h, &["attempt_binary"]),
        bytes(f(&h, &["attempt_binary_bytes"])),
        s(&h, &["payload_bytes"])
    ));
    section(&mut out, "Hook process wall clock", &t, &notes);

    // Recent timeline.
    let r = step("recent_timeline");
    let mut t = Table::new(&["Metric", "Value"]);
    t.row(vec![
        "Events in the last 24 h of synthetic time".into(),
        num(f(&r, &["events_in_window"])),
    ]);
    t.row(vec![
        "Segments in the database".into(),
        s(&r, &["segments_total"]),
    ]);
    t.row(vec![
        "`QueryEngine::from_database` (scan + projection + tables) p50 / p95 / p99".into(),
        lat(&r, &["engine_build"]),
    ]);
    t.row(vec![
        "Projected sessions / turns / attempts in the window".into(),
        format!(
            "{} / {} / {}",
            s(&r, &["projection", "sessions"]),
            s(&r, &["projection", "turns"]),
            s(&r, &["projection", "attempts"])
        ),
    ]);
    if let Some(q) = r.get("queries").and_then(Value::as_object) {
        for (stmt, v) in q {
            t.row(vec![
                format!("`{stmt}` p50 / p95 / p99 ({} rows)", s(v, &["rows"])),
                lat(v, &["latency"]),
            ]);
        }
    }
    t.row(vec!["Peak RSS".into(), bytes(f(&r, &["peak_rss_bytes"]))]);
    let notes: Vec<String> = status_note(&r).into_iter().collect();
    section(&mut out, "Recent timeline (last 24 h)", &t, &notes);

    // Scan + engine at sizes.
    let mut t = Table::new(&["Step", "Events", "Wall", "Rows/s", "Peak RSS", "Detail"]);
    let mut notes = Vec::new();
    let sp = step("scan_project_full");
    if let Some(n) = status_note(&sp) {
        notes.push(format!("`scan_project_full` {n}"));
    }
    t.row(vec![
        "`Database::scan` (all segments → `Vec<Event>`)".into(),
        num(f(&sp, &["events"])),
        secs(f(&sp, &["scan_secs"])),
        num(f(&sp, &["scan_rows_per_sec"])),
        "shared below".into(),
        format!("{} of segments", bytes(f(&sp, &["disk", "segments_bytes"]))),
    ]);
    t.row(vec![
        "`project()` over that `Vec<Event>`".into(),
        num(f(&sp, &["events"])),
        secs(f(&sp, &["project_secs"])),
        num(f(&sp, &["project_rows_per_sec"])),
        bytes(f(&sp, &["peak_rss_bytes"])),
        format!(
            "{} sessions, {} attempts, {} edges",
            s(&sp, &["projection", "sessions"]),
            s(&sp, &["projection", "attempts"]),
            s(&sp, &["projection", "edges"])
        ),
    ]);
    let mut engine_names: Vec<String> = steps
        .as_object()
        .map(|o| {
            o.keys()
                .filter(|k| k.starts_with("engine_"))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    engine_names.sort_by_key(|k| f(&step(k), &["events_requested"]).unwrap_or(0.0) as u64);
    for name in &engine_names {
        let st = step(name);
        if let Some(n) = status_note(&st) {
            notes.push(format!("`{name}` {n}"));
        }
        t.row(vec![
            format!("`QueryEngine::from_database` (`{name}`)"),
            num(f(&st, &["events_loaded"]).or(f(&st, &["events_requested"]))),
            secs(f(&st, &["engine_build_secs"])),
            num(f(&st, &["engine_rows_per_sec"])),
            bytes(f(&st, &["peak_rss_bytes"]).or(f(&st, &["peak_rss_observed_bytes"]))),
            if s(&st, &["status"]) != "ok" {
                "did not complete (see note)".into()
            } else if st.get("filtered") == Some(&Value::Bool(true)) {
                "prefix of the full database via `until` filter (re-encoded)".into()
            } else {
                "whole database".into()
            },
        ]);
    }
    section(&mut out, "Full historical scan", &t, &notes);

    // SQL over the engine.
    let mut t = Table::new(&["Engine", "Statement", "Rows", "p50 / p95 / p99"]);
    for name in &engine_names {
        let st = step(name);
        if let Some(q) = st.get("sql").and_then(Value::as_object) {
            for (stmt, v) in q {
                t.row(vec![
                    format!("`{name}`"),
                    format!("`{stmt}`"),
                    s(v, &["rows"]),
                    lat(v, &["latency"]),
                ]);
            }
        }
    }
    section(&mut out, "Queries over the loaded engine", &t, &[]);

    // State + trace.
    let mut t = Table::new(&["Engine", "Statement", "Rows", "p50 / p95 / p99"]);
    for name in &engine_names {
        let st = step(name);
        if st.get("state_at").is_some() {
            t.row(vec![
                format!("`{name}`"),
                format!(
                    "`STATE project AT <ts>` × {} points",
                    s(&st, &["state_at", "points"])
                ),
                s(&st, &["state_at", "rows_per_point"]),
                lat(&st, &["state_at", "latency"]),
            ]);
        }
        if st
            .get("trace_chain_depth_10")
            .and_then(|v| v.get("latency"))
            .is_some()
        {
            t.row(vec![
                format!("`{name}`"),
                format!("`{}`", s(&st, &["trace_chain_depth_10", "statement"])),
                s(&st, &["trace_chain_depth_10", "rows"]),
                lat(&st, &["trace_chain_depth_10", "latency"]),
            ]);
        }
    }
    let tc = step("trace_chain");
    if let Some(d) = tc.get("depths").and_then(Value::as_object) {
        for (depth, v) in d {
            t.row(vec![
                format!(
                    "`trace_chain` ({} events, {} chained attempts)",
                    num(f(&tc, &["events"])),
                    s(&tc, &["chain_attempts"])
                ),
                format!("`TRACE … CAUSES DEPTH {depth}`"),
                s(v, &["rows"]),
                lat(v, &["latency"]),
            ]);
        }
    }
    let notes: Vec<String> = status_note(&tc).into_iter().collect();
    section(&mut out, "Time travel and causal traversal", &t, &notes);

    // Projection curve.
    let mut t = Table::new(&[
        "Events",
        "Mode",
        "Generate",
        "`project()`",
        "Rows/s",
        "Peak RSS",
        "Sessions / attempts / edges",
    ]);
    let mut proj_names: Vec<String> = steps
        .as_object()
        .map(|o| {
            o.keys()
                .filter(|k| k.starts_with("projection_"))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    proj_names.sort_by_key(|k| {
        let st = step(k);
        (
            f(&st, &["events"]).unwrap_or(f(&st, &["events_requested"]).unwrap_or(0.0)) as u64,
            k.clone(),
        )
    });
    let mut notes = Vec::new();
    for name in &proj_names {
        let st = step(name);
        if let Some(n) = status_note(&st) {
            notes.push(format!("`{name}` {n}"));
        }
        t.row(vec![
            num(f(&st, &["events"]).or(f(&st, &["events_requested"]))),
            s(&st, &["mode"]),
            secs(f(&st, &["generate_secs"]).or(f(&st, &["generate_and_push_secs"]))),
            secs(f(&st, &["project_secs"])),
            num(f(&st, &["project_rows_per_sec"])),
            bytes(f(&st, &["peak_rss_bytes"]).or(f(&st, &["peak_rss_observed_bytes"]))),
            format!(
                "{} / {} / {}",
                s(&st, &["projection", "sessions"]),
                s(&st, &["projection", "attempts"]),
                s(&st, &["projection", "edges"])
            ),
        ]);
    }
    section(&mut out, "Projection cost versus event count", &t, &notes);

    // Size by kind.
    let sz = step("size_by_kind");
    let mut t = Table::new(&[
        "Profile",
        "JSON B/event",
        "JSONL+zstd(3) B/event",
        "Arrow IPC plain B/event",
        "Segment B/event",
        "Segment ratio vs JSON",
    ]);
    if let Some(rows) = sz.get("profiles").and_then(Value::as_array) {
        for r in rows {
            t.row(vec![
                format!("`{}`", s(r, &["profile"])),
                num(f(r, &["json_bytes_per_event"])),
                num(f(r, &["jsonl_zstd3_bytes_per_event"])),
                num(f(r, &["arrow_ipc_plain_bytes_per_event"])),
                num(f(r, &["segment_bytes_per_event"])),
                f(r, &["segment_ratio_vs_json"])
                    .map(|x| format!("{x:.1}×"))
                    .unwrap_or_else(|| "—".into()),
            ]);
        }
    }
    let mut notes = vec![format!(
        "Weighted by the generated mix ({} coverage): {} of JSON and {} of segment per event, ratio {:.1}×. The sampled live database: {} JSON → {} segment per event ({:.1}×).",
        pct(f(&sz, &["mix_coverage"])),
        bytes(f(&sz, &["weighted_json_bytes_per_event"])),
        bytes(f(&sz, &["weighted_segment_bytes_per_event"])),
        f(&sz, &["weighted_ratio"]).unwrap_or(0.0),
        bytes(f(&sz, &["sampled_real_database", "json_bytes_per_event"])),
        bytes(f(
            &sz,
            &["sampled_real_database", "segment_bytes_per_event"]
        )),
        f(&sz, &["sampled_real_database", "ratio"]).unwrap_or(0.0),
    )];
    if let Some(n) = status_note(&sz) {
        notes.push(n);
    }
    section(&mut out, "Size and compression by kind", &t, &notes);

    let mut t = Table::new(&["Profile", "Share of events", "Share of segment bytes"]);
    if let Some(rows) = sz.get("segment_byte_share").and_then(Value::as_array) {
        let mut rows: Vec<&Value> = rows.iter().collect();
        rows.sort_by(|a, b| {
            f(b, &["segment_byte_share"])
                .partial_cmp(&f(a, &["segment_byte_share"]))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for r in rows {
            t.row(vec![
                format!("`{}`", s(r, &["profile"])),
                pct(f(r, &["event_share"])),
                pct(f(r, &["segment_byte_share"])),
            ]);
        }
    }
    section(&mut out, "Where the bytes go", &t, &[]);

    // Segments.
    let sg = step("segments_100k");
    let mut t = Table::new(&[
        "`flush_events`",
        "Events",
        "Segments",
        "Segment bytes",
        "Manifest bytes",
        "Ingest events/s",
        "Open p50",
        "Scan all p50",
        "Batches all p50",
    ]);
    if let Some(vs) = sg.get("variants").and_then(Value::as_array) {
        for v in vs {
            t.row(vec![
                s(v, &["flush_events"]),
                num(f(v, &["events"])),
                s(v, &["segments"]),
                bytes(f(v, &["segment_bytes"])),
                bytes(f(v, &["manifest_bytes"])),
                num(f(v, &["ingest_events_per_sec"])),
                ms(f(v, &["open", "p50_us"])),
                ms(f(v, &["scan_all", "p50_us"])),
                ms(f(v, &["batches_all", "p50_us"])),
            ]);
        }
    }
    let notes: Vec<String> = status_note(&sg).into_iter().collect();
    section(
        &mut out,
        "Segment count versus read cost (no compaction)",
        &t,
        &notes,
    );

    // Compaction.
    let cp = step("compact_100k");
    let mut t = Table::new(&[
        "",
        "Segments",
        "Segment bytes",
        "Manifest bytes",
        "Open p50",
        "Scan all p50",
        "Batches all p50",
    ]);
    for (label, key) in [("Before", "before"), ("After", "after")] {
        let v = cp.get(key).cloned().unwrap_or(Value::Null);
        t.row(vec![
            label.into(),
            s(&v, &["segments"]),
            bytes(f(&v, &["segment_bytes"])),
            bytes(f(&v, &["manifest_bytes"])),
            ms(f(&v, &["open", "p50_us"])),
            ms(f(&v, &["scan_all", "p50_us"])),
            ms(f(&v, &["batches_all", "p50_us"])),
        ]);
    }
    let c = cp.get("compaction").cloned().unwrap_or(Value::Null);
    let mut notes: Vec<String> = status_note(&cp).into_iter().collect();
    if !c.is_null() {
        let open_x = f(&cp, &["speedup_open_p50"]).unwrap_or(0.0);
        let scan_x = f(&cp, &["speedup_scan_all_p50"]).unwrap_or(0.0);
        notes.push(format!(
            "`Database::compact`: {} for {} run(s) merging {} segment(s) ({}) into {} ({}), {} events ({} events/s); open {open_x:.2}× and full scan {scan_x:.2}× faster at p50.",
            secs(f(&c, &["secs"])),
            s(&c, &["runs"]),
            s(&c, &["inputs"]),
            bytes(f(&c, &["input_bytes"])),
            s(&c, &["runs"]),
            bytes(f(&c, &["output_bytes"])),
            num(f(&c, &["events"])),
            num(f(&c, &["events_per_sec"])),
        ));
    }
    section(&mut out, "Compaction", &t, &notes);

    // Step status overview.
    let mut t = Table::new(&[
        "Step",
        "Status",
        "Wall",
        "Peak RSS (getrusage)",
        "Peak RSS (observed by parent)",
    ]);
    if let Some(o) = steps.as_object() {
        for (name, st) in o {
            t.row(vec![
                format!("`{name}`"),
                s(st, &["status"]),
                secs(f(st, &["step_wall_secs"])),
                bytes(f(st, &["peak_rss_bytes"])),
                bytes(f(st, &["peak_rss_observed_bytes"])),
            ]);
        }
    }
    section(&mut out, "Step summary", &t, &[]);
    out
}
