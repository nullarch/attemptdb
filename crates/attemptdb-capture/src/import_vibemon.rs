//! VibeMon backfill: replay an export of the legacy `hook_events` table into
//! the database (`attempt import vibemon-export`).
//!
//! The legacy client (`vibemon-hooks`, `notify.sh`) posted envelope v2 to a
//! Supabase Edge Function that stored one row per event. When an install
//! moves to AttemptDB, that history is replayed here so the timeline does
//! not start on migration day. The row → envelope → [`Event`] mapping lives
//! in `attemptdb_adapters::vibemon_export`; this module reads the file,
//! recovers what rows lost from their siblings, orders them, picks the
//! device, and ingests in batches.
//!
//! - **Formats.** NDJSON (one row per line) or a JSON array of rows,
//!   detected by the first non-whitespace byte. A bad NDJSON line is
//!   rejected with its line number; a syntactically broken array cannot be
//!   resynchronised and is an error.
//! - **Idempotent.** Event ids derive from the row's primary key
//!   (`ExportRow::event_id`), so a second run stores nothing and reports
//!   every row as a duplicate.
//! - **Ordered.** Rows are sorted by `created_at`, then `id`, before ingest,
//!   so `source_seq` (and the HLC) are monotone in event time.
//! - **Recovered, not invented.** Only `session_start`/`session_end` rows
//!   kept the working directory; the importer applies it to the other rows
//!   of the same session, and learns `project_id → cwd` from sessions that
//!   have both so sessions without a start row still land in the right
//!   project. Nothing else is guessed (`ProjectHint`).
//! - **Device.** `--device` fixes one id for every row; otherwise each row
//!   is attributed to `DeviceId::derive(["vibemon-export", <device column>])`
//!   when the export has one, else `["vibemon-export", <user_id>]`.

use crate::Result;
use crate::import::LossyLines;
use attemptdb_adapters::vibemon_export::{
    ExportRow, ProjectHint, RejectReason, normalise_row, parse_row,
};
use attemptdb_core::{DeviceId, Event, SessionId, Timestamp};
use attemptdb_storage::Database;
use serde::Serialize;
use serde::de::{SeqAccess, Visitor};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Events handed to the database per `ingest` call.
pub const INGEST_BATCH: usize = 5_000;

/// Rejection details kept in the summary; the rest are only counted.
pub const MAX_REJECTION_DETAILS: usize = 50;

/// How the file is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Ndjson,
    JsonArray,
}

impl ExportFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ndjson => "ndjson",
            Self::JsonArray => "json array",
        }
    }
}

/// One row that was not imported. `line` is the 1-based line for NDJSON
/// and the 1-based element index for an array.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Rejected {
    pub line: usize,
    pub reason: RejectReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The file, read: rows that parsed and rows that did not.
#[derive(Debug, Default)]
pub struct ParsedExport {
    /// `None` for an empty file.
    pub format: Option<ExportFormat>,
    /// Rows (non-blank lines or array elements) read.
    pub rows_read: usize,
    pub rows: Vec<ExportRow>,
    pub rejected: Vec<Rejected>,
}

/// Read and parse an export file.
pub fn parse_export_file(path: &Path) -> Result<ParsedExport> {
    let file = File::open(path).map_err(|e| crate::io_at(path, e))?;
    parse_export(BufReader::new(file))
}

/// Read and parse an export from any reader.
pub fn parse_export<R: BufRead>(mut reader: R) -> Result<ParsedExport> {
    let mut out = ParsedExport::default();
    let (format, newlines_skipped) = detect_format(&mut reader)
        .map_err(|e| crate::CaptureError::Other(format!("reading the export: {e}")))?;
    let Some(format) = format else {
        return Ok(out);
    };
    out.format = Some(format);
    match format {
        ExportFormat::Ndjson => parse_ndjson(reader, newlines_skipped, &mut out),
        ExportFormat::JsonArray => parse_array(reader, &mut out)?,
    }
    Ok(out)
}

/// Skip a BOM and leading whitespace; `[` means an array, anything else
/// NDJSON. Returns the format (`None` when the file is empty) and how many
/// newlines were consumed, so NDJSON line numbers stay right.
fn detect_format<R: BufRead>(reader: &mut R) -> std::io::Result<(Option<ExportFormat>, usize)> {
    let mut newlines = 0usize;
    let mut bom_checked = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok((None, newlines));
        }
        if !bom_checked {
            bom_checked = true;
            if buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
                reader.consume(3);
                continue;
            }
        }
        match buf.iter().position(|b| !b.is_ascii_whitespace()) {
            Some(i) => {
                newlines += buf[..i].iter().filter(|b| **b == b'\n').count();
                let format = if buf[i] == b'[' {
                    ExportFormat::JsonArray
                } else {
                    ExportFormat::Ndjson
                };
                reader.consume(i);
                return Ok((Some(format), newlines));
            }
            None => {
                newlines += buf.iter().filter(|b| **b == b'\n').count();
                let n = buf.len();
                reader.consume(n);
            }
        }
    }
}

fn parse_ndjson<R: BufRead>(reader: R, first_line: usize, out: &mut ParsedExport) {
    let mut line_no = first_line;
    for line in LossyLines::new(reader) {
        line_no += 1;
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        out.rows_read += 1;
        match serde_json::from_str::<Value>(text) {
            Ok(value) => accept(value, line_no, out),
            Err(e) => out.rejected.push(Rejected {
                line: line_no,
                reason: RejectReason::InvalidJson,
                detail: Some(e.to_string()),
            }),
        }
    }
}

fn parse_array<R: BufRead>(reader: R, out: &mut ParsedExport) -> Result<()> {
    let mut de = serde_json::Deserializer::from_reader(reader);
    serde::Deserializer::deserialize_seq(&mut de, RowSink { out })
        .and_then(|()| de.end())
        .map_err(|e| crate::CaptureError::Other(format!("invalid JSON array: {e}")))
}

/// Streams the array: each element is parsed and dropped before the next
/// is read, so memory is proportional to the rows kept, not the file.
struct RowSink<'a> {
    out: &'a mut ParsedExport,
}

impl<'de> Visitor<'de> for RowSink<'_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON array of hook_events rows")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        while let Some(value) = seq.next_element::<Value>()? {
            self.out.rows_read += 1;
            let index = self.out.rows_read;
            accept(value, index, self.out);
        }
        Ok(())
    }
}

fn accept(value: Value, line: usize, out: &mut ParsedExport) {
    match parse_row(&value) {
        Ok(row) => out.rows.push(row),
        Err(reason) => out.rejected.push(Rejected {
            line,
            reason,
            detail: detail_for(reason, &value),
        }),
    }
}

/// A short, single-line hint for the human summary: the offending value
/// for the reasons where one exists. Never a payload.
fn detail_for(reason: RejectReason, value: &Value) -> Option<String> {
    let column = match reason {
        RejectReason::UnknownEventType => "event_type",
        RejectReason::InvalidCreatedAt => "created_at",
        _ => return None,
    };
    let text = match value.get(column)? {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let text: String = text.chars().filter(|c| !c.is_control()).take(64).collect();
    Some(format!("{column} = {text:?}"))
}

/// Which device imported events are attributed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevicePolicy {
    /// Per row: a device column, else the account (`user_id`).
    Derived,
    /// One id for every row (`--device`).
    Fixed(DeviceId),
}

/// What a run read, planned, and (unless a dry run) wrote.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VibemonImportSummary {
    pub format: Option<ExportFormat>,
    pub rows_read: usize,
    pub rows_parsed: usize,
    pub rows_rejected: usize,
    pub rejected_by_reason: BTreeMap<String, usize>,
    /// The first [`MAX_REJECTION_DETAILS`] rejections.
    pub rejections: Vec<Rejected>,
    /// Events planned for ingest (rows parsed and normalised).
    pub events: usize,
    pub sessions: usize,
    pub rows_without_session: usize,
    /// Rows whose working directory could not be recovered from any row.
    pub rows_without_cwd: usize,
    /// Earliest and latest `created_at` among the planned events (RFC 3339).
    pub first_event: Option<String>,
    pub last_event: Option<String>,
    /// Device id → rows attributed to it.
    pub devices: BTreeMap<String, usize>,
    pub device_rule: &'static str,
    pub dry_run: bool,
    pub accepted: usize,
    pub duplicates: usize,
    /// Attrs dropped by the engine's contract check; expected to be 0.
    pub redactions: usize,
    pub batches: usize,
}

/// Events ready to ingest, in ingest order, plus the summary so far.
#[derive(Debug)]
pub struct ImportPlan {
    pub events: Vec<Event>,
    pub summary: VibemonImportSummary,
}

/// Order the rows, recover working directories, attribute devices, and
/// normalise. Pure: nothing is written. `summary.dry_run` is `true` until
/// [`import_vibemon_export`] runs the plan.
pub fn plan(parsed: ParsedExport, policy: DevicePolicy) -> ImportPlan {
    let mut summary = VibemonImportSummary {
        format: parsed.format,
        rows_read: parsed.rows_read,
        rows_parsed: parsed.rows.len(),
        device_rule: match policy {
            DevicePolicy::Derived => "derived",
            DevicePolicy::Fixed(_) => "fixed",
        },
        dry_run: true,
        ..Default::default()
    };
    let mut rejected = parsed.rejected;

    let mut rows = parsed.rows;
    rows.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    let hints = Hints::learn(&rows);

    let mut events: Vec<Event> = Vec::with_capacity(rows.len());
    let mut sessions: HashSet<SessionId> = HashSet::new();
    for row in &rows {
        let device = match policy {
            DevicePolicy::Fixed(d) => d,
            DevicePolicy::Derived => match row.derived_device() {
                Some(d) => d,
                None => {
                    rejected.push(Rejected {
                        line: 0,
                        reason: RejectReason::NoDeviceIdentity,
                        detail: Some(format!("row id {}", row.id)),
                    });
                    continue;
                }
            },
        };
        let hint = hints.for_row(row);
        if row.cwd().is_none() && hint.cwd.is_none() {
            summary.rows_without_cwd += 1;
        }
        if row.session_id.is_none() {
            summary.rows_without_session += 1;
        }
        match normalise_row(device, row, &hint) {
            Ok(ev) => {
                sessions.insert(ev.session_id);
                *summary.devices.entry(device.to_string()).or_insert(0) += 1;
                if summary.first_event.is_none() {
                    summary.first_event = Some(row.created_at.to_rfc3339());
                }
                summary.last_event = Some(row.created_at.to_rfc3339());
                events.push(ev);
            }
            Err(e) => rejected.push(Rejected {
                line: 0,
                reason: RejectReason::Adapter,
                detail: Some(format!("row id {}: {e}", row.id)),
            }),
        }
    }

    rejected.sort_by_key(|r| (r.line, r.reason));
    summary.rows_rejected = rejected.len();
    for r in &rejected {
        *summary
            .rejected_by_reason
            .entry(r.reason.as_str().to_string())
            .or_insert(0) += 1;
    }
    rejected.truncate(MAX_REJECTION_DETAILS);
    summary.rejections = rejected;
    summary.events = events.len();
    summary.sessions = sessions.len();
    ImportPlan { events, summary }
}

/// Run a plan against the writer: ingest in batches of [`INGEST_BATCH`]
/// and flush at the end so the history lands in a segment.
pub fn import_vibemon_export(db: &mut Database, plan: ImportPlan) -> Result<VibemonImportSummary> {
    let mut summary = plan.summary;
    summary.dry_run = false;
    let mut events = plan.events;
    while !events.is_empty() {
        let rest = events.split_off(events.len().min(INGEST_BATCH));
        let batch = std::mem::replace(&mut events, rest);
        let r = db.ingest(batch)?;
        summary.accepted += r.accepted;
        summary.duplicates += r.duplicates;
        summary.redactions += r.redactions;
        summary.batches += 1;
    }
    db.flush()?;
    Ok(summary)
}

/// What sibling rows know about a row's project, keyed by account so two
/// tenants in one export never share a directory.
struct Hints {
    session_cwd: HashMap<(String, String), String>,
    session_identifier: HashMap<(String, String), String>,
    project_cwd: HashMap<(String, String), String>,
    project_identifier: HashMap<(String, String), String>,
}

impl Hints {
    /// Rows must already be in ingest order: the first value seen wins.
    fn learn(rows: &[ExportRow]) -> Self {
        let mut h = Self {
            session_cwd: HashMap::new(),
            session_identifier: HashMap::new(),
            project_cwd: HashMap::new(),
            project_identifier: HashMap::new(),
        };
        for row in rows {
            let Some(session) = row.session_id.as_deref() else {
                continue;
            };
            let key = (account(row), session.to_string());
            if let Some(cwd) = row.cwd() {
                h.session_cwd
                    .entry(key.clone())
                    .or_insert_with(|| cwd.to_string());
            }
            if let Some(id) = row.identifier() {
                h.session_identifier
                    .entry(key)
                    .or_insert_with(|| id.to_string());
            }
        }
        for row in rows {
            let (Some(session), Some(project)) =
                (row.session_id.as_deref(), row.project_id.as_deref())
            else {
                continue;
            };
            let session_key = (account(row), session.to_string());
            let project_key = (account(row), project.to_string());
            if let Some(cwd) = h.session_cwd.get(&session_key) {
                h.project_cwd
                    .entry(project_key.clone())
                    .or_insert_with(|| cwd.clone());
            }
            if let Some(id) = h.session_identifier.get(&session_key) {
                h.project_identifier
                    .entry(project_key)
                    .or_insert_with(|| id.clone());
            }
        }
        // A session with no start row but with tool_use rows names its
        // project; when another session taught us that project's directory,
        // the whole session inherits it.
        for row in rows {
            let (Some(session), Some(project)) =
                (row.session_id.as_deref(), row.project_id.as_deref())
            else {
                continue;
            };
            let session_key = (account(row), session.to_string());
            let project_key = (account(row), project.to_string());
            if let Some(cwd) = h.project_cwd.get(&project_key) {
                h.session_cwd
                    .entry(session_key.clone())
                    .or_insert_with(|| cwd.clone());
            }
            if let Some(id) = h.project_identifier.get(&project_key) {
                h.session_identifier
                    .entry(session_key)
                    .or_insert_with(|| id.clone());
            }
        }
        h
    }

    fn for_row(&self, row: &ExportRow) -> ProjectHint {
        let session_key = row
            .session_id
            .as_deref()
            .map(|s| (account(row), s.to_string()));
        let project_key = row
            .project_id
            .as_deref()
            .map(|p| (account(row), p.to_string()));
        let lookup = |by_session: &HashMap<(String, String), String>,
                      by_project: &HashMap<(String, String), String>| {
            session_key
                .as_ref()
                .and_then(|k| by_session.get(k))
                .or_else(|| project_key.as_ref().and_then(|k| by_project.get(k)))
                .cloned()
        };
        ProjectHint {
            cwd: lookup(&self.session_cwd, &self.project_cwd),
            identifier: lookup(&self.session_identifier, &self.project_identifier),
        }
    }
}

fn account(row: &ExportRow) -> String {
    row.user_id.clone().unwrap_or_default()
}

/// Earliest and latest `observed_at` of a set of events.
pub fn time_span(events: &[Event]) -> Option<(Timestamp, Timestamp)> {
    let first = events.iter().map(|e| e.observed_at).min()?;
    let last = events.iter().map(|e| e.observed_at).max()?;
    Some((first, last))
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::{CaptureMode, EventKind, conformance};
    use attemptdb_storage::{OpenOptions, ScanFilter};
    use serde_json::json;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/vibemon-export")
            .join(name)
    }

    fn open_db(root: &Path) -> Database {
        let dir = root.join(".attemptdb");
        Database::create(&dir, DeviceId::derive(&["vibemon-import-tests"])).unwrap();
        Database::open(
            &dir,
            OpenOptions {
                create: false,
                ..Default::default()
            },
        )
        .unwrap()
    }

    const USER_A: &str = "00000000-0000-4000-8000-00000000000a";

    fn row(id: u32, event_type: &str, created_at: &str, extra: Value) -> Value {
        let mut v = json!({
            "id": format!("22222222-2222-4222-8222-{id:012}"),
            "user_id": USER_A,
            "created_at": created_at,
            "event_type": event_type,
            "agent": "claude_code",
            "signals": {},
            "envelope_version": 2
        });
        for (k, val) in extra.as_object().unwrap() {
            v[k.as_str()] = val.clone();
        }
        v
    }

    fn ndjson(rows: &[Value]) -> String {
        rows.iter()
            .map(|r| serde_json::to_string(r).unwrap() + "\n")
            .collect()
    }

    #[test]
    fn ndjson_fixture_parses_and_counts_rejections_by_line() {
        let parsed = parse_export_file(&fixture("hook_events.ndjson")).unwrap();
        assert_eq!(parsed.format, Some(ExportFormat::Ndjson));
        assert_eq!(parsed.rows_read, 12);
        assert_eq!(parsed.rows.len(), 10);
        let rejected: Vec<(usize, RejectReason)> =
            parsed.rejected.iter().map(|r| (r.line, r.reason)).collect();
        assert_eq!(
            rejected,
            vec![
                (7, RejectReason::InvalidJson),
                (10, RejectReason::UnknownEventType)
            ]
        );
        assert!(
            parsed.rejected[1]
                .detail
                .as_deref()
                .unwrap()
                .contains("coffee_break")
        );
        // Unknown columns (`_fixture_note`, `xp`) are ignored.
        assert_eq!(parsed.rows[0].event, "session_start");
        assert_eq!(parsed.rows[0].cwd(), Some("/home/dev/example/project"));
    }

    #[test]
    fn json_array_fixture_parses_the_same_rows() {
        let parsed = parse_export_file(&fixture("hook_events.json")).unwrap();
        assert_eq!(parsed.format, Some(ExportFormat::JsonArray));
        assert_eq!(parsed.rows_read, 11);
        assert_eq!(parsed.rows.len(), 10);
        assert_eq!(parsed.rejected.len(), 1);
        assert_eq!(parsed.rejected[0].line, 9, "1-based element index");
        assert_eq!(parsed.rejected[0].reason, RejectReason::UnknownEventType);

        let from_lines = parse_export_file(&fixture("hook_events.ndjson")).unwrap();
        let ids = |p: &ParsedExport| {
            let mut v: Vec<String> = p.rows.iter().map(|r| r.id.clone()).collect();
            v.sort();
            v
        };
        assert_eq!(ids(&parsed), ids(&from_lines));

        // Whitespace, a BOM, and blank lines do not confuse detection.
        let text = format!(
            "\u{FEFF}\n\n  {}\n\n{}\n",
            serde_json::to_string(&row(1, "prompt", "2026-08-20T09:00:00Z", json!({}))).unwrap(),
            "{not json"
        );
        let p = parse_export(text.as_bytes()).unwrap();
        assert_eq!(p.format, Some(ExportFormat::Ndjson));
        assert_eq!((p.rows_read, p.rows.len()), (2, 1));
        assert_eq!(p.rejected[0].line, 5, "line numbers count skipped blanks");

        let p = parse_export("  \n[ ]".as_bytes()).unwrap();
        assert_eq!(p.format, Some(ExportFormat::JsonArray));
        assert_eq!(p.rows_read, 0);
        let p = parse_export("".as_bytes()).unwrap();
        assert_eq!(p.format, None);
        // A broken array cannot be resynchronised: that is an error, with a position.
        let err = parse_export("[{\"id\": 1}, {\"id\": ".as_bytes()).unwrap_err();
        assert!(err.to_string().contains("invalid JSON array"), "{err}");
    }

    #[test]
    fn plan_orders_by_time_and_attributes_devices() {
        let parsed = parse_export_file(&fixture("hook_events.ndjson")).unwrap();
        let planned = plan(parsed, DevicePolicy::Derived);
        assert_eq!(planned.summary.events, 10);
        assert_eq!(planned.summary.rows_rejected, 2);
        assert_eq!(planned.summary.rejected_by_reason["invalid_json"], 1);
        assert_eq!(planned.summary.rejected_by_reason["unknown_event_type"], 1);
        assert_eq!(planned.summary.sessions, 2);
        assert_eq!(planned.summary.rows_without_session, 0);
        assert_eq!(
            planned.summary.rows_without_cwd, 0,
            "session rows carry cwd for both"
        );
        assert!(planned.summary.dry_run);
        let times: Vec<i64> = planned
            .events
            .iter()
            .map(|e| e.observed_at.as_micros())
            .collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
        // The two out-of-order rows in the file landed where their time says.
        let names: Vec<&str> = planned
            .events
            .iter()
            .map(|e| e.provider_event_name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "session_start",
                "prompt",
                "activity",
                "tool_failure",
                "bash",
                "stop",
                "session_start",
                "permission",
                "activity",
                "session_end"
            ]
        );
        assert_eq!(
            planned.summary.first_event.as_deref(),
            Some("2026-08-20T09:00:00.100000Z")
        );
        assert_eq!(
            planned.summary.last_event.as_deref(),
            Some("2026-08-21T10:05:00.000000Z")
        );
        // One account in the file → one derived device.
        let derived = DeviceId::derive(&["vibemon-export", USER_A]);
        assert_eq!(planned.summary.devices.len(), 1);
        assert_eq!(planned.summary.devices[&derived.to_string()], 10);
        assert!(planned.events.iter().all(|e| e.device_id == derived));
        assert_eq!(planned.summary.device_rule, "derived");
        // The agent column changes the provider, never the device.
        assert!(
            planned
                .events
                .iter()
                .filter(|e| e.provider == attemptdb_core::event::Provider::Codex)
                .count()
                >= 1
        );

        // A device column outranks the account; --device outranks both.
        let rows = vec![
            row(
                1,
                "prompt",
                "2026-08-20T09:00:00Z",
                json!({"machine_id": "mac-7"}),
            ),
            row(2, "prompt", "2026-08-20T09:00:01Z", json!({})),
            row(
                3,
                "prompt",
                "2026-08-20T09:00:02Z",
                json!({"user_id": null}),
            ),
        ];
        let p = plan(
            parse_export(ndjson(&rows).as_bytes()).unwrap(),
            DevicePolicy::Derived,
        );
        assert_eq!(p.summary.events, 2);
        assert_eq!(p.summary.rejected_by_reason["no_device_identity"], 1);
        assert_eq!(
            p.events[0].device_id,
            DeviceId::derive(&["vibemon-export", "mac-7"])
        );
        assert_eq!(p.events[1].device_id, derived);
        let fixed = DeviceId::derive(&["tests", "fixed"]);
        let p = plan(
            parse_export(ndjson(&rows).as_bytes()).unwrap(),
            DevicePolicy::Fixed(fixed),
        );
        assert_eq!(p.summary.events, 3, "no identity needed with --device");
        assert!(p.events.iter().all(|e| e.device_id == fixed));
        assert_eq!(p.summary.device_rule, "fixed");
    }

    #[test]
    fn working_directories_flow_from_session_rows_to_the_rest() {
        const S1: &str = "3f1c2b8e-0001-4000-8000-000000000001";
        const S2: &str = "3f1c2b8e-0002-4000-8000-000000000002";
        const P1: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
        let rows = vec![
            // Session 1 has a start row (cwd) and a tool_use (project uuid).
            row(
                1,
                "session_start",
                "2026-08-20T09:00:00Z",
                json!({"session_id": S1,
                "payload": {"cwd": "/home/dev/example/project", "timestamp": "2026-08-20T09:00:00Z", "client_version": "29"}}),
            ),
            row(
                2,
                "tool_use",
                "2026-08-20T09:00:10Z",
                json!({"session_id": S1, "project_id": P1,
                "tool": "Edit", "file_path": "/home/dev/example/project/src/a.rs", "lines_added": 1, "lines_removed": 0}),
            ),
            row(
                3,
                "prompt",
                "2026-08-20T09:00:20Z",
                json!({"session_id": S1,
                "payload": {"project_root": "example/project"}}),
            ),
            // Session 2 started before the export window: no start row, but
            // the same project uuid.
            row(
                4,
                "tool_use",
                "2026-08-22T09:00:00Z",
                json!({"session_id": S2, "project_id": P1,
                "tool": "Write", "file_path": "/home/dev/example/project/README.md"}),
            ),
            row(
                5,
                "bash",
                "2026-08-22T09:00:05Z",
                json!({"session_id": S2,
                "payload": {"tool_name": "Bash"}, "signals": {"bash.category": "pkg.test"}}),
            ),
            // No session, no payload: nothing to recover.
            row(
                6,
                "permission",
                "2026-08-22T09:00:06Z",
                json!({"payload": {"tool_name": "Bash"}}),
            ),
        ];
        let p = plan(
            parse_export(ndjson(&rows).as_bytes()).unwrap(),
            DevicePolicy::Derived,
        );
        assert_eq!(p.summary.events, 6);
        assert_eq!(p.summary.rows_without_session, 1);
        assert_eq!(p.summary.rows_without_cwd, 1);
        let roots: Vec<&str> = p.events.iter().map(|e| e.project.root.as_str()).collect();
        assert_eq!(
            roots,
            [
                "/home/dev/example/project",
                "/home/dev/example/project",
                "/home/dev/example/project",
                "/home/dev/example/project",
                "/home/dev/example/project",
                "/"
            ]
        );
        // Session 1's rows share one project id; session 2 joins it through
        // the project uuid; the orphan does not.
        let ids: Vec<_> = p.events.iter().map(|e| e.project.project_id).collect();
        assert!(ids[..5].iter().all(|id| *id == ids[0]));
        assert_ne!(ids[5], ids[0]);
        // The identifier learned from the prompt row names the whole project.
        assert!(
            p.events[..5]
                .iter()
                .all(|e| e.project.name == "example/project"),
            "{:?}",
            p.events.iter().map(|e| &e.project.name).collect::<Vec<_>>()
        );
        assert_eq!(p.events[1].kind, EventKind::ToolCallFinished);
        assert_eq!(p.events[1].attrs["file_ext"], "rs");
        assert_eq!(p.events[3].attrs["x_vibemon_project_id"], P1);
        assert_eq!(p.events[4].attrs["command_subcategory"], "pkg.test");
    }

    #[test]
    fn import_is_idempotent_conformant_and_dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open_db(tmp.path());

        // Dry run: a plan is not a write.
        let dry = plan(
            parse_export_file(&fixture("hook_events.ndjson")).unwrap(),
            DevicePolicy::Derived,
        );
        assert!(dry.summary.dry_run);
        assert_eq!((dry.summary.accepted, dry.summary.duplicates), (0, 0));
        assert!(db.scan(&ScanFilter::default()).unwrap().is_empty());
        assert_eq!(db.stats().memtable_rows, 0);

        let first = import_vibemon_export(&mut db, dry).unwrap();
        assert!(!first.dry_run);
        assert_eq!(
            (
                first.accepted,
                first.duplicates,
                first.batches,
                first.redactions
            ),
            (10, 0, 1, 0)
        );
        assert_eq!(db.stats().memtable_rows, 0, "flushed at the end");

        let second = import_vibemon_export(
            &mut db,
            plan(
                parse_export_file(&fixture("hook_events.json")).unwrap(),
                DevicePolicy::Derived,
            ),
        )
        .unwrap();
        assert_eq!((second.accepted, second.duplicates), (0, 10));

        let events = db.scan(&ScanFilter::default()).unwrap();
        assert_eq!(events.len(), 10);
        assert!(events.iter().all(|e| e.is_ingested()));
        assert!(
            events
                .iter()
                .all(|e| e.attrs.get("x_vibemon_import") == Some(&json!("hook_events")))
        );
        assert!(
            events
                .iter()
                .all(|e| !e.attrs.contains_key("reconstructed"))
        );
        assert!(events.iter().all(|e| !e.attrs.contains_key("redactions")));
        assert!(
            events
                .iter()
                .all(|e| e.capture_mode == CaptureMode::MetadataOnly
                    && e.content.is_none()
                    && e.raw.is_none())
        );
        let serialised = serde_json::to_string(&events).unwrap();
        assert!(
            !serialised.contains("settlement endpoint"),
            "commit.message is content and does not survive metadata_only"
        );
        assert!(
            events
                .iter()
                .all(|e| e.hook_version.as_deref() == Some("vibemon-envelope-v2"))
        );
        // Stored order is event time (source_seq monotone in created_at).
        let seqs: Vec<(u64, i64)> = events
            .iter()
            .map(|e| (e.source_seq, e.observed_at.as_micros()))
            .collect();
        assert!(
            seqs.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 <= w[1].1),
            "{seqs:?}"
        );
        let commit = events
            .iter()
            .find(|e| e.attrs.get("command_subcategory") == Some(&json!("git.commit")))
            .expect("git.commit row imported");
        assert_eq!(commit.attrs["command_category"], "git");
        assert_eq!(commit.tool.as_ref().unwrap().name, "Bash");

        let report = conformance::check_parsed(&events);
        assert!(
            report.compatible(),
            "{}",
            serde_json::to_string_pretty(&report).unwrap()
        );
        assert_eq!(
            time_span(&events).map(|(a, _)| a.to_rfc3339()).as_deref(),
            Some("2026-08-20T09:00:00.100000Z")
        );
    }
}
