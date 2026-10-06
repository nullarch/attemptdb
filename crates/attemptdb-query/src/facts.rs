//! Facts about a slice of the stream that every reader summarises from the
//! events themselves rather than from the projection: which projects and
//! providers exist and how many events each has, which device wrote a
//! session's first event and how much of it was reconstructed, what each
//! device contributed. The server's `/v1/status`, `/v1/devices` and
//! project resolution, the UI's status page and scope bar, and the MCP
//! tools' status all read these.
//!
//! [`StreamFacts`] is derived from a segment's columns once (no `Event` is
//! decoded) and merged in stream order with [`StreamFacts::absorb`], so a
//! view over a thousand segments pays for the merge, not for a pass over
//! every event. The columns it reads are [`FACT_COLUMNS`]: a reader that
//! only wants facts hands the segment reader that list and never decodes
//! `content_json`, `raw_json` or the rest (see
//! `attemptdb_storage::CachedSegment::read_columns`).

use attemptdb_core::{DeviceId, EventKind, ProjectId, SessionId, Timestamp};
use attemptdb_project::is_meta_kind;
use attemptdb_storage::segment::{StrCol, col, fsb_col, ts_col};
use datafusion::arrow::array::{Array, RecordBatch};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};

/// One project as the events describe it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectFacts {
    pub project_id: ProjectId,
    pub name: String,
    pub root: String,
    pub repo_remote: Option<String>,
    pub events: u64,
    /// Of `events`, the synthetic capture-test events `attempt setup` and
    /// `attempt hook install` write to prove the pipeline: not work.
    pub capture_test_events: u64,
    pub sessions: HashSet<SessionId>,
}

/// One provider's share of the events.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderFacts {
    pub provider: String,
    pub events: u64,
    /// Of `events`, the synthetic capture-test events (not work).
    pub capture_test_events: u64,
    /// Latest `observed_at`, capture tests excluded.
    pub last_event_at: Option<Timestamp>,
    /// Events a hook captured as they happened: no capture test, nothing
    /// reconstructed from a transcript, no telemetry. Only these prove that
    /// the provider runs the hooks (`attempt doctor`'s "active").
    pub hook_events: u64,
    /// Latest `captured_at` among `hook_events`.
    pub last_hook_captured_at: Option<Timestamp>,
    /// A capture-test event (written by `attempt setup` / `hook install`)
    /// is stored.
    pub capture_test_seen: bool,
    /// OpenTelemetry records, by `x_otel_signal` (`unknown` when absent).
    pub telemetry: BTreeMap<String, SignalFacts>,
}

/// One OpenTelemetry signal's records from one provider.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SignalFacts {
    pub events: u64,
    pub last_observed_at: Option<Timestamp>,
}

/// Facts about one session that the projection does not carry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionFacts {
    /// The device that wrote the session's first event (in stream order).
    pub device_id: DeviceId,
    pub provider_session_id: String,
    pub provider: String,
    pub project_id: ProjectId,
    /// Hook-captured versus transcript-reconstructed events.
    pub captured: usize,
    pub reconstructed: usize,
    /// Latest `observed_at` and the kind of that event.
    pub last_event_at: Option<Timestamp>,
    pub last_kind: Option<EventKind>,
    /// The newest test run and build the session's tool calls reported —
    /// the countable signals a console may show as a number.
    pub last_tests: Option<TestSignal>,
    pub last_build: Option<BuildSignal>,
}

/// A test run's counts (from the adapters' `tests_*` attrs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestSignal {
    pub at: Timestamp,
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
}

/// A build command's outcome (`command_category = build` + `exit_code`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildSignal {
    pub at: Timestamp,
    pub ok: bool,
}

/// The newest event of a slice by `observed_at`, capture tests excluded:
/// what "is this user coding right now" is answered from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastEvent {
    pub at: Timestamp,
    pub kind: EventKind,
    pub provider: String,
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub tool: Option<String>,
}

/// One device's contribution. Meta events (corrections, retractions) are
/// kept apart so a server can leave out the ones it wrote itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceFacts {
    pub events: u64,
    pub sessions: HashSet<SessionId>,
    pub providers: HashSet<String>,
    pub first_observed_at: Option<Timestamp>,
    pub last_observed_at: Option<Timestamp>,
    pub last_ingested_at: Option<Timestamp>,
}

#[derive(Clone, Debug, Default)]
pub struct StreamFacts {
    pub events: u64,
    /// Of `events`, the synthetic capture-test events (not work).
    pub capture_test_events: u64,
    pub reconstructed: u64,
    pub projects: BTreeMap<ProjectId, ProjectFacts>,
    pub providers: BTreeMap<String, ProviderFacts>,
    /// In the order sessions were first seen.
    pub sessions: Vec<(SessionId, SessionFacts)>,
    /// Keyed by `(device, is meta event)`.
    pub devices: HashMap<(DeviceId, bool), DeviceFacts>,
    /// Latest `observed_at`, capture tests excluded.
    pub last_event_at: Option<Timestamp>,
    /// The event that set `last_event_at`.
    pub last_event: Option<LastEvent>,
    session_index: HashMap<SessionId, usize>,
}

fn max_ts(a: Option<Timestamp>, b: Option<Timestamp>) -> Option<Timestamp> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

fn min_ts(a: Option<Timestamp>, b: Option<Timestamp>) -> Option<Timestamp> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// `attrs.reconstructed == true`, read without parsing attrs that cannot
/// carry it.
fn reconstructed_in(attrs_json: Option<&str>) -> bool {
    let Some(a) = attrs_json else { return false };
    if !a.contains("\"reconstructed\"") {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(a)
        .ok()
        .and_then(|v| v.get("reconstructed").and_then(serde_json::Value::as_bool))
        == Some(true)
}

/// One row's worth of input.
struct Row<'a> {
    project_id: ProjectId,
    project_name: &'a str,
    project_root: &'a str,
    repo_remote: Option<&'a str>,
    provider: &'a str,
    provider_session_id: &'a str,
    kind: EventKind,
    session_id: SessionId,
    device_id: DeviceId,
    observed_at: Timestamp,
    captured_at: Option<Timestamp>,
    ingested_at: Option<Timestamp>,
    reconstructed: bool,
    /// The OpenTelemetry signal when the row is a telemetry record.
    otel_signal: Option<Cow<'a, str>>,
    tool: Option<&'a str>,
    tests: Option<(u64, u64, u64)>,
    build_ok: Option<bool>,
}

/// `(tests_passed, tests_failed, tests_skipped)` when the attrs carry a
/// test run, and whether a build command succeeded.
fn signals_in(
    attrs: &serde_json::Map<String, serde_json::Value>,
) -> (Option<(u64, u64, u64)>, Option<bool>) {
    let n = |k: &str| attrs.get(k).and_then(serde_json::Value::as_u64);
    let tests = n("tests_passed").map(|p| {
        (
            p,
            n("tests_failed").unwrap_or(0),
            n("tests_skipped").unwrap_or(0),
        )
    });
    let build_ok = match (
        attrs
            .get("command_category")
            .and_then(serde_json::Value::as_str),
        attrs.get("exit_code").and_then(serde_json::Value::as_i64),
    ) {
        (Some("build"), Some(code)) => Some(code == 0),
        _ => None,
    };
    (tests, build_ok)
}

/// The columns a segment must be asked for to build facts from it
/// ([`StreamFacts::push_batch`] reads exactly these).
pub const FACT_COLUMNS: &[&str] = &[
    col::PROJECT_ID,
    col::SESSION_ID,
    col::DEVICE_ID,
    col::OBSERVED_AT,
    col::CAPTURED_AT,
    col::INGESTED_AT,
    col::PROJECT_NAME,
    col::PROJECT_ROOT,
    col::REPO_REMOTE,
    col::PROVIDER,
    col::PROVIDER_SESSION_ID,
    col::KIND,
    col::TOOL_NAME,
    col::ATTRS_JSON,
];

/// A telemetry record (`kind = unknown`, `attrs.source = "otel"`) and its
/// signal, parsing only attrs that can carry the marker.
///
/// Telemetry is most of a long-lived database, so the common shape is read
/// without a JSON parse: attrs are serialised compact, and in a flat object
/// without escapes the text `"source":"otel"` can only be the top-level key
/// `source` holding the string `otel` (a quote inside a string value would be
/// escaped; only keys are followed by a colon). Anything else — a nested
/// object, a backslash, a repeated key — takes the full parse, which decides
/// exactly as before.
fn otel_signal_in<'a>(kind: Option<&str>, attrs_json: Option<&'a str>) -> Option<Cow<'a, str>> {
    let a = attrs_json.filter(|a| kind == Some("unknown") && a.contains("otel"))?;
    if let Some(fast) = otel_signal_flat(a) {
        return fast;
    }
    let v = serde_json::from_str::<serde_json::Value>(a).ok()?;
    (v["source"] == "otel").then(|| Cow::Owned(otel_signal(v.get("x_otel_signal"))))
}

/// `Some(answer)` when `a` is a flat, escape-free, unambiguous object and the
/// answer could be read from its text; `None` when only a parse can say.
fn otel_signal_flat(a: &str) -> Option<Option<Cow<'_, str>>> {
    const SOURCE: &str = "\"source\":\"otel\"";
    const SIGNAL: &str = "\"x_otel_signal\":";
    // One pass: exactly one `{`, no escapes, nothing but a compact `"k":v`
    // around any colon (a colon inside a string value merely costs the parse).
    let bytes = a.as_bytes();
    let space = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r');
    let mut braces = 0;
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'{' => braces += 1,
            b'\\' => return None,
            b':' if (i > 0 && space(bytes[i - 1]))
                || bytes.get(i + 1).is_some_and(|n| space(*n)) =>
            {
                return None;
            }
            _ => {}
        }
    }
    if braces != 1 {
        return None;
    }
    if a.matches("\"source\"").count() > 1 {
        return None;
    }
    if !a.contains(SOURCE) {
        // No `source` key at all: not telemetry. A mention of the word
        // somewhere else (a value) is left to the parse.
        return if a.contains("\"source\"") {
            None
        } else {
            Some(None)
        };
    }
    let signal = match a.find(SIGNAL) {
        None => "unknown",
        Some(at) => {
            let rest = &a[at + SIGNAL.len()..];
            if rest.contains(SIGNAL) {
                return None;
            }
            match rest.strip_prefix('"') {
                Some(value) => value.split('"').next()?,
                // A number, a bool, null: not a string, so "unknown".
                None => "unknown",
            }
        }
    };
    Some(Some(Cow::Borrowed(signal)))
}

fn otel_signal(signal: Option<&serde_json::Value>) -> String {
    signal
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn signals_in_json(attrs_json: Option<&str>) -> (Option<(u64, u64, u64)>, Option<bool>) {
    let Some(a) = attrs_json else {
        return (None, None);
    };
    if !(a.contains("\"tests_passed\"") || a.contains("\"build\"")) {
        return (None, None);
    }
    match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(a) {
        Ok(map) => signals_in(&map),
        Err(_) => (None, None),
    }
}

/// The [`EventKind`] of a kind column, parsing each distinct dictionary
/// value once instead of once per row.
struct KindLookup {
    parsed: Option<Vec<EventKind>>,
}

impl KindLookup {
    fn new(col: &StrCol<'_>) -> Self {
        let parsed = match col {
            StrCol::Dict { values, .. } => Some(
                (0..values.len())
                    .map(|i| {
                        if values.is_null(i) {
                            EventKind::Unknown
                        } else {
                            EventKind::parse(values.value(i)).unwrap_or(EventKind::Unknown)
                        }
                    })
                    .collect(),
            ),
            _ => None,
        };
        Self { parsed }
    }

    fn get(&self, col: &StrCol<'_>, row: usize) -> EventKind {
        match (&self.parsed, col) {
            (Some(parsed), StrCol::Dict { keys, .. }) => {
                if keys.is_null(row) {
                    EventKind::Unknown
                } else {
                    parsed
                        .get(keys.value(row) as usize)
                        .copied()
                        .unwrap_or(EventKind::Unknown)
                }
            }
            _ => col
                .get(row)
                .and_then(EventKind::parse)
                .unwrap_or(EventKind::Unknown),
        }
    }
}

/// The rows of `b` the projection reads: everything except OpenTelemetry
/// records (`kind = unknown`, `attrs.source = "otel"`, what
/// `Event::is_telemetry` decides per event). Returned as a keep mask, or
/// `None` when no row is left out, with the number of rows left out. Read
/// from the `kind` and `attrs_json` columns alone, so the telemetry rows,
/// which are most of a long-lived database, are never decoded into events.
pub(crate) fn non_telemetry_rows(
    b: &RecordBatch,
) -> (Option<datafusion::arrow::array::BooleanArray>, u64) {
    let kind = StrCol::new(b, col::KIND);
    let attrs = StrCol::new(b, col::ATTRS_JSON);
    if matches!(kind, StrCol::Absent) || matches!(attrs, StrCol::Absent) {
        return (None, 0);
    }
    let kind_of = KindLookup::new(&kind);
    let mut keep = Vec::with_capacity(b.num_rows());
    let mut skipped = 0u64;
    for row in 0..b.num_rows() {
        // Whatever the stored text of the kind, `Event` decodes an unreadable
        // one as `Unknown`, so the check is on the decoded kind.
        let telemetry = kind_of.get(&kind, row) == EventKind::Unknown
            && otel_signal_in(Some("unknown"), attrs.get(row)).is_some();
        skipped += u64::from(telemetry);
        keep.push(!telemetry);
    }
    if skipped == 0 {
        return (None, 0);
    }
    (
        Some(datafusion::arrow::array::BooleanArray::from(keep)),
        skipped,
    )
}

impl StreamFacts {
    pub fn from_events<'a>(events: impl IntoIterator<Item = &'a attemptdb_core::Event>) -> Self {
        let mut f = Self::default();
        for ev in events {
            f.push(Row {
                project_id: ev.project.project_id,
                project_name: &ev.project.name,
                project_root: &ev.project.root,
                repo_remote: ev.project.repo_remote.as_deref(),
                provider: ev.provider.as_str(),
                provider_session_id: &ev.provider_session_id,
                kind: ev.kind,
                otel_signal: ev
                    .is_telemetry()
                    .then(|| Cow::Owned(otel_signal(ev.attrs.get("x_otel_signal")))),
                session_id: ev.session_id,
                device_id: ev.device_id,
                observed_at: ev.observed_at,
                captured_at: Some(ev.captured_at),
                ingested_at: ev.ingested_at,
                reconstructed: ev
                    .attrs
                    .get("reconstructed")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true),
                tool: ev.tool.as_ref().map(|t| t.name.as_str()),
                tests: signals_in(&ev.attrs).0,
                build_ok: signals_in(&ev.attrs).1,
            });
        }
        f
    }

    /// From the columns of canonical-schema batches (or of batches holding at
    /// least [`FACT_COLUMNS`]).
    pub fn from_batches(batches: &[RecordBatch]) -> Self {
        let mut f = Self::default();
        for b in batches {
            f.push_batch(b);
        }
        f
    }

    /// Fold one batch in, stream order. Reads [`FACT_COLUMNS`] in place: no
    /// column is cast or copied, and a batch that holds only those columns
    /// (a projected read of a segment) gives exactly what the full batch
    /// would.
    pub fn push_batch(&mut self, b: &RecordBatch) {
        let (Some(project_id), Some(session_id), Some(device_id), Some(observed_at)) = (
            fsb_col(b, col::PROJECT_ID),
            fsb_col(b, col::SESSION_ID),
            fsb_col(b, col::DEVICE_ID),
            ts_col(b, col::OBSERVED_AT),
        ) else {
            return;
        };
        let captured_at = ts_col(b, col::CAPTURED_AT);
        let ingested_at = ts_col(b, col::INGESTED_AT);
        let project_name = StrCol::new(b, col::PROJECT_NAME);
        let project_root = StrCol::new(b, col::PROJECT_ROOT);
        let repo_remote = StrCol::new(b, col::REPO_REMOTE);
        let provider = StrCol::new(b, col::PROVIDER);
        let provider_session_id = StrCol::new(b, col::PROVIDER_SESSION_ID);
        let kind = StrCol::new(b, col::KIND);
        let tool_name = StrCol::new(b, col::TOOL_NAME);
        let attrs = StrCol::new(b, col::ATTRS_JSON);
        // A dictionary holds a few dozen distinct kinds; parse each once.
        let kind_of = KindLookup::new(&kind);
        let id16 = |a: &datafusion::arrow::array::FixedSizeBinaryArray, row: usize| {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(a.value(row));
            bytes
        };
        let ts_at = |a: Option<&datafusion::arrow::array::TimestampMicrosecondArray>, row| {
            a.filter(|a| !a.is_null(row))
                .map(|a| Timestamp::from_micros(a.value(row)))
        };
        for row in 0..b.num_rows() {
            if project_id.is_null(row)
                || session_id.is_null(row)
                || device_id.is_null(row)
                || observed_at.is_null(row)
            {
                continue;
            }
            let kind_str = kind.get(row);
            let attrs_json = attrs.get(row);
            let otel_signal = otel_signal_in(kind_str, attrs_json);
            let (tests, build_ok) = if otel_signal.is_none() {
                signals_in_json(attrs_json)
            } else {
                (None, None)
            };
            self.push(Row {
                project_id: ProjectId::from_bytes(id16(project_id, row)),
                project_name: project_name.get(row).unwrap_or_default(),
                project_root: project_root.get(row).unwrap_or_default(),
                repo_remote: repo_remote.get(row),
                provider: provider.get(row).unwrap_or_default(),
                provider_session_id: provider_session_id.get(row).unwrap_or_default(),
                kind: kind_of.get(&kind, row),
                otel_signal,
                session_id: SessionId::from_bytes(id16(session_id, row)),
                device_id: DeviceId::from_bytes(id16(device_id, row)),
                observed_at: Timestamp::from_micros(observed_at.value(row)),
                captured_at: ts_at(captured_at, row),
                ingested_at: ts_at(ingested_at, row),
                reconstructed: reconstructed_in(attrs_json),
                tool: tool_name.get(row),
                tests,
                build_ok,
            });
        }
    }

    fn push(&mut self, r: Row<'_>) {
        self.events += 1;
        if r.reconstructed {
            self.reconstructed += 1;
        }
        let d = self
            .devices
            .entry((r.device_id, is_meta_kind(r.kind)))
            .or_default();
        d.events += 1;
        if r.otel_signal.is_none() {
            d.sessions.insert(r.session_id);
        }
        if !d.providers.contains(r.provider) {
            d.providers.insert(r.provider.to_string());
        }
        d.first_observed_at = min_ts(d.first_observed_at, Some(r.observed_at));
        d.last_observed_at = max_ts(d.last_observed_at, Some(r.observed_at));
        d.last_ingested_at = max_ts(d.last_ingested_at, r.ingested_at);
        if !self.providers.contains_key(r.provider) {
            self.providers.insert(
                r.provider.to_string(),
                ProviderFacts {
                    provider: r.provider.to_string(),
                    ..Default::default()
                },
            );
        }
        let pr = self
            .providers
            .get_mut(r.provider)
            .expect("inserted just above");
        pr.events += 1;

        // Telemetry proves collection, not work or a project/session lifecycle.
        if let Some(signal) = r.otel_signal {
            if !pr.telemetry.contains_key(signal.as_ref()) {
                pr.telemetry
                    .insert(signal.to_string(), SignalFacts::default());
            }
            let t = pr
                .telemetry
                .get_mut(signal.as_ref())
                .expect("inserted just above");
            t.events += 1;
            t.last_observed_at = max_ts(t.last_observed_at, Some(r.observed_at));
            return;
        }
        if r.kind == EventKind::CaptureTest {
            pr.capture_test_seen = true;
            pr.capture_test_events += 1;
            self.capture_test_events += 1;
        } else if !r.reconstructed {
            pr.hook_events += 1;
            pr.last_hook_captured_at = max_ts(pr.last_hook_captured_at, r.captured_at);
        }
        let p = self
            .projects
            .entry(r.project_id)
            .or_insert_with(|| ProjectFacts {
                project_id: r.project_id,
                name: r.project_name.to_string(),
                root: r.project_root.to_string(),
                repo_remote: None,
                events: 0,
                capture_test_events: 0,
                sessions: HashSet::new(),
            });
        p.events += 1;
        if r.kind == EventKind::CaptureTest {
            p.capture_test_events += 1;
        }
        if p.repo_remote.is_none()
            && let Some(remote) = r.repo_remote
        {
            p.repo_remote = Some(remote.to_string());
        }
        p.sessions.insert(r.session_id);
        if r.kind != EventKind::CaptureTest {
            pr.last_event_at = max_ts(pr.last_event_at, Some(r.observed_at));
            if self.last_event_at.is_none_or(|t| r.observed_at >= t) {
                self.last_event_at = Some(r.observed_at);
                self.last_event = Some(LastEvent {
                    at: r.observed_at,
                    kind: r.kind,
                    provider: r.provider.to_string(),
                    session_id: r.session_id,
                    project_id: r.project_id,
                    tool: r.tool.map(str::to_string),
                });
            }
        }
        let i = *self.session_index.entry(r.session_id).or_insert_with(|| {
            self.sessions.push((
                r.session_id,
                SessionFacts {
                    device_id: r.device_id,
                    provider_session_id: r.provider_session_id.to_string(),
                    provider: r.provider.to_string(),
                    project_id: r.project_id,
                    captured: 0,
                    reconstructed: 0,
                    last_event_at: None,
                    last_kind: None,
                    last_tests: None,
                    last_build: None,
                },
            ));
            self.sessions.len() - 1
        });
        let s = &mut self.sessions[i].1;
        if r.reconstructed {
            s.reconstructed += 1;
        } else {
            s.captured += 1;
        }
        if r.kind != EventKind::CaptureTest && s.last_event_at.is_none_or(|t| r.observed_at >= t) {
            s.last_event_at = Some(r.observed_at);
            s.last_kind = Some(r.kind);
        }
        if let Some((passed, failed, skipped)) = r.tests
            && s.last_tests.is_none_or(|t| r.observed_at >= t.at)
        {
            s.last_tests = Some(TestSignal {
                at: r.observed_at,
                passed,
                failed,
                skipped,
            });
        }
        if let Some(ok) = r.build_ok
            && s.last_build.is_none_or(|b| r.observed_at >= b.at)
        {
            s.last_build = Some(BuildSignal {
                at: r.observed_at,
                ok,
            });
        }
    }

    /// Add `other`, which follows `self` in stream order.
    pub fn absorb(&mut self, other: &StreamFacts) {
        self.events += other.events;
        self.capture_test_events += other.capture_test_events;
        self.reconstructed += other.reconstructed;
        for (pid, info) in &other.projects {
            let p = self.projects.entry(*pid).or_insert_with(|| ProjectFacts {
                events: 0,
                capture_test_events: 0,
                sessions: HashSet::new(),
                repo_remote: None,
                ..info.clone()
            });
            p.events += info.events;
            p.capture_test_events += info.capture_test_events;
            if p.repo_remote.is_none() {
                p.repo_remote = info.repo_remote.clone();
            }
            p.sessions.extend(info.sessions.iter().copied());
        }
        for (name, info) in &other.providers {
            let pr = self
                .providers
                .entry(name.clone())
                .or_insert_with(|| ProviderFacts {
                    provider: name.clone(),
                    ..Default::default()
                });
            pr.events += info.events;
            pr.capture_test_events += info.capture_test_events;
            pr.last_event_at = max_ts(pr.last_event_at, info.last_event_at);
            pr.hook_events += info.hook_events;
            pr.last_hook_captured_at = max_ts(pr.last_hook_captured_at, info.last_hook_captured_at);
            pr.capture_test_seen |= info.capture_test_seen;
            for (signal, t) in &info.telemetry {
                let mine = pr.telemetry.entry(signal.clone()).or_default();
                mine.events += t.events;
                mine.last_observed_at = max_ts(mine.last_observed_at, t.last_observed_at);
            }
        }
        if other
            .last_event_at
            .is_some_and(|t| self.last_event_at.is_none_or(|mine| t >= mine))
        {
            self.last_event_at = other.last_event_at;
            self.last_event = other.last_event.clone();
        }
        for (sid, f) in &other.sessions {
            match self.session_index.get(sid) {
                Some(&i) => {
                    let mine = &mut self.sessions[i].1;
                    mine.captured += f.captured;
                    mine.reconstructed += f.reconstructed;
                    if f.last_event_at
                        .is_some_and(|t| mine.last_event_at.is_none_or(|m| t >= m))
                    {
                        mine.last_event_at = f.last_event_at;
                        mine.last_kind = f.last_kind;
                    }
                    if f.last_tests
                        .is_some_and(|t| mine.last_tests.is_none_or(|m| t.at >= m.at))
                    {
                        mine.last_tests = f.last_tests;
                    }
                    if f.last_build
                        .is_some_and(|b| mine.last_build.is_none_or(|m| b.at >= m.at))
                    {
                        mine.last_build = f.last_build;
                    }
                }
                None => {
                    self.session_index.insert(*sid, self.sessions.len());
                    self.sessions.push((*sid, f.clone()));
                }
            }
        }
        for (key, d) in &other.devices {
            let mine = self.devices.entry(*key).or_default();
            mine.events += d.events;
            mine.sessions.extend(d.sessions.iter().copied());
            mine.providers.extend(d.providers.iter().cloned());
            mine.first_observed_at = min_ts(mine.first_observed_at, d.first_observed_at);
            mine.last_observed_at = max_ts(mine.last_observed_at, d.last_observed_at);
            mine.last_ingested_at = max_ts(mine.last_ingested_at, d.last_ingested_at);
        }
    }

    pub fn session(&self, sid: &SessionId) -> Option<&SessionFacts> {
        self.session_index.get(sid).map(|&i| &self.sessions[i].1)
    }

    pub fn has_session(&self, sid: &SessionId) -> bool {
        self.session_index.contains_key(sid)
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Sessions that hold at least one event that is not a capture test: the
    /// number a person means by "sessions". (A capture test is a synthetic
    /// event `attempt setup` writes per agent, in a session of its own.)
    pub fn work_session_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|(_, s)| s.last_event_at.is_some())
            .count()
    }

    /// Resolve a project argument: a `prj_` id (or bare uuid), a
    /// normalised remote (`host/owner/repo`, in any spelling
    /// `normalise_remote` accepts), a project name (exact, case-insensitive,
    /// or the last path components), or a logical root. A spelling that
    /// names several projects (two checkouts called `app`, one project seen
    /// from two devices) is [`ResolveError::Ambiguous`] and lists them, not
    /// whichever sorts first; an exact name or root outranks a suffix.
    pub fn resolve_project(&self, spec: &str) -> std::result::Result<ProjectId, ResolveError> {
        let spec = spec.trim();
        if let Ok(pid) = spec.parse::<ProjectId>()
            && self.projects.contains_key(&pid)
        {
            return Ok(pid);
        }
        let one_of =
            |hits: Vec<&ProjectFacts>| -> Option<std::result::Result<ProjectId, ResolveError>> {
                match hits.as_slice() {
                    [] => None,
                    [p] => Some(Ok(p.project_id)),
                    many => Some(Err(ResolveError::Ambiguous {
                        what: "project",
                        spec: spec.to_string(),
                        candidates: many.iter().take(8).map(|p| p.describe()).collect(),
                        total: many.len(),
                    })),
                }
            };
        let remote = attemptdb_core::event::normalise_remote(spec);
        if let Some(remote) = &remote
            && let Some(found) = one_of(
                self.projects
                    .values()
                    .filter(|p| p.repo_remote.as_ref() == Some(remote))
                    .collect(),
            )
        {
            return found;
        }
        let spec_norm = attemptdb_core::PortablePath::from_raw(spec, None).logical;
        if let Some(found) = one_of(
            self.projects
                .values()
                .filter(|p| p.name.eq_ignore_ascii_case(spec) || p.root == spec_norm)
                .collect(),
        ) {
            return found;
        }
        let suffix = format!("/{spec}");
        if let Some(found) = one_of(
            self.projects
                .values()
                .filter(|p| p.name.ends_with(&suffix))
                .collect(),
        ) {
            return found;
        }
        Err(ResolveError::Unknown {
            what: "project",
            spec: spec.to_string(),
            known: self.projects.values().map(|p| p.describe()).collect(),
        })
    }

    /// The project of a repository: by normalised remote first, then by
    /// logical root.
    pub fn project_of(&self, root_logical: &str, remote: Option<&str>) -> Option<ProjectId> {
        if let Some(r) = remote
            && let Some(p) = self
                .projects
                .values()
                .find(|p| p.repo_remote.as_deref() == Some(r))
        {
            return Some(p.project_id);
        }
        self.projects
            .values()
            .find(|p| p.root == root_logical)
            .map(|p| p.project_id)
    }

    /// Resolve a session argument: a `ses_` id (full), a provider session id
    /// (full), or a prefix of either, at least [`MIN_SESSION_PREFIX`]
    /// characters long. A prefix that fits several sessions is
    /// [`ResolveError::Ambiguous`] and lists them; one that is too short to
    /// mean anything (`0`) is [`ResolveError::TooShort`]. Neither picks a
    /// session on the reader's behalf.
    pub fn resolve_session(&self, spec: &str) -> std::result::Result<SessionId, ResolveError> {
        let spec = spec.trim();
        if let Ok(sid) = spec.parse::<SessionId>()
            && self.has_session(&sid)
        {
            return Ok(sid);
        }
        let ambiguous = |hits: &[&(SessionId, SessionFacts)]| ResolveError::Ambiguous {
            what: "session",
            spec: spec.to_string(),
            candidates: hits
                .iter()
                .take(8)
                .map(|(s, f)| describe_session(s, f))
                .collect(),
            total: hits.len(),
        };
        // A provider session id as written, in full.
        let exact: Vec<&(SessionId, SessionFacts)> = self
            .sessions
            .iter()
            .filter(|(_, f)| f.provider_session_id == spec)
            .collect();
        match exact.as_slice() {
            [] => {}
            [one] => return Ok(one.0),
            many => return Err(ambiguous(many)),
        }
        let needle = spec.strip_prefix("ses_").unwrap_or(spec);
        if needle.chars().count() < MIN_SESSION_PREFIX {
            return Err(ResolveError::TooShort {
                spec: spec.to_string(),
                min: MIN_SESSION_PREFIX,
            });
        }
        let needle_lower = needle.to_ascii_lowercase();
        let hits: Vec<&(SessionId, SessionFacts)> = self
            .sessions
            .iter()
            .filter(|(sid, f)| {
                sid.0.hyphenated().to_string().starts_with(&needle_lower)
                    || sid.0.simple().to_string().starts_with(&needle_lower)
                    || f.provider_session_id.starts_with(spec)
            })
            .collect();
        match hits.as_slice() {
            [] => Err(ResolveError::Unknown {
                what: "session",
                spec: spec.to_string(),
                known: Vec::new(),
            }),
            [one] => Ok(one.0),
            many => Err(ambiguous(many)),
        }
    }
}

/// The fewest characters of a `ses_` id or provider session id that
/// [`StreamFacts::resolve_session`] takes as a prefix.
pub const MIN_SESSION_PREFIX: usize = 4;

fn describe_session(sid: &SessionId, f: &SessionFacts) -> String {
    format!(
        "{} ({} {})",
        sid.short(),
        f.provider,
        if f.provider_session_id.is_empty() {
            "-"
        } else {
            f.provider_session_id.as_str()
        }
    )
}

impl ProjectFacts {
    fn describe(&self) -> String {
        format!("{} ({})", self.name, self.project_id.short())
    }
}

/// Why a project or session argument did not name exactly one thing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// Nothing matched. `known` lists what exists (projects only).
    Unknown {
        what: &'static str,
        spec: String,
        known: Vec<String>,
    },
    /// Several things matched; `candidates` lists the first few.
    Ambiguous {
        what: &'static str,
        spec: String,
        candidates: Vec<String>,
        total: usize,
    },
    /// A session prefix too short to mean anything.
    TooShort { spec: String, min: usize },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Unknown { what, spec, known } if *what == "project" => write!(
                f,
                "unknown project {spec:?}; known projects: {}",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            ),
            ResolveError::Unknown { what, spec, .. } => write!(
                f,
                "unknown {what} {spec:?} (expected a ses_ id, a provider session id, or the first {MIN_SESSION_PREFIX}+ characters of one)"
            ),
            ResolveError::Ambiguous {
                what,
                spec,
                candidates,
                total,
            } => write!(
                f,
                "{what} {spec:?} matches {total} {what}s: {}{}; give more of the name or the full id",
                candidates.join(", "),
                if *total > candidates.len() {
                    ", …"
                } else {
                    ""
                }
            ),
            ResolveError::TooShort { spec, min } => write!(
                f,
                "session {spec:?} is too short to identify a session: give at least {min} characters of a ses_ id or provider session id, or the whole provider session id"
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// What `otel_signal_in` decided before the text shortcut existed: a full
    /// parse of every attrs string that mentions `otel`.
    fn reference(kind: Option<&str>, attrs: Option<&str>) -> Option<String> {
        let a = attrs.filter(|a| kind == Some("unknown") && a.contains("otel"))?;
        let v = serde_json::from_str::<Value>(a).ok()?;
        (v["source"] == "otel").then(|| otel_signal(v.get("x_otel_signal")))
    }

    fn check(kind: Option<&str>, attrs: &str) {
        let got = otel_signal_in(kind, Some(attrs)).map(|c| c.into_owned());
        assert_eq!(got, reference(kind, Some(attrs)), "{kind:?} {attrs}");
    }

    #[test]
    fn the_text_shortcut_decides_as_a_full_parse_does_on_the_shapes_that_occur() {
        for attrs in [
            r#"{"source":"otel"}"#,
            r#"{"source":"otel","x_otel_signal":"logs"}"#,
            r#"{"x_otel_signal":"metrics","source":"otel"}"#,
            r#"{"source":"otel","x_otel_signal":5}"#,
            r#"{"source":"otel","x_otel_signal":null}"#,
            r#"{"source":"otel","x_otel_signal":""}"#,
            r#"{"source":"otel","x_otel_signal":"lo\u0067s"}"#,
            r#"{"source":"otel","x_otel_signal":"a\"b"}"#,
            r#"{"source":"hook","x_otel_signal":"logs"}"#,
            r#"{"source":"otel2"}"#,
            r#"{"source": "otel"}"#,
            r#"{"x_otel_record_type":"log"}"#,
            r#"{"provider":{"source":"otel"}}"#,
            r#"{"source":"hook","provider":{"source":"otel"}}"#,
            r#"{"source":"otel","provider":{"k":"v"},"x_otel_signal":"traces"}"#,
            r#"{"reason":"he said \"source\":\"otel\"","source":"hook"}"#,
            r#"{"reason":"{","source":"otel","x_otel_signal":"logs"}"#,
            r#"{"source":"otel","source":"hook"}"#,
            r#"{"source":"hook","source":"otel"}"#,
            r#"{"source":"otel","x_otel_signal":"logs","x_otel_signal":"metrics"}"#,
            r#"{"note":"x_otel_signal","source":"otel"}"#,
            r#"["source","otel"]"#,
            r#""otel""#,
            r#"not json at all otel"#,
            "",
            "{}",
        ] {
            for kind in [Some("unknown"), Some("tool_call_finished"), None] {
                check(kind, attrs);
            }
        }
    }

    /// The same, over objects assembled from the awkward pieces by a
    /// deterministic generator.
    #[test]
    fn the_text_shortcut_never_disagrees_with_a_full_parse() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let keys = [
            "source",
            "x_otel_signal",
            "x_otel_record_type",
            "reason",
            "provider",
            "note",
        ];
        let values: Vec<Value> = vec![
            json!("otel"),
            json!("hook"),
            json!("logs"),
            json!("metrics"),
            json!(""),
            json!(5),
            json!(null),
            json!(true),
            json!("a\"b"),
            json!("back\\slash"),
            json!("{brace"),
            json!("\"source\":\"otel\""),
            json!({"source": "otel"}),
            json!({"k": "v"}),
            json!(["otel", "logs"]),
            json!("lo\u{e9}gs"),
        ];
        for _ in 0..4000 {
            let n = next() % 5;
            let mut map = serde_json::Map::new();
            for _ in 0..n {
                let k = keys[(next() % keys.len() as u64) as usize];
                let v = values[(next() % values.len() as u64) as usize].clone();
                map.insert(k.to_string(), v);
            }
            let text = serde_json::to_string(&Value::Object(map)).unwrap();
            check(Some("unknown"), &text);
        }
    }

    /// The projector's skip decides as `Event::is_telemetry` does.
    #[test]
    fn telemetry_rows_are_the_rows_event_decoding_calls_telemetry() {
        use attemptdb_core::event::Provider;
        use attemptdb_core::{CaptureMode, Event};
        let device = DeviceId::derive(&["facts-unit"]);
        let mk = |kind: EventKind, attrs: Value| {
            let mut ev = Event::new(
                device,
                Provider::Codex,
                "x",
                kind,
                attemptdb_core::ProjectRef::derive("/p", None, &device),
                "s",
                CaptureMode::MetadataOnly,
                "t/1",
            );
            for (k, v) in attrs.as_object().unwrap() {
                ev.attrs.insert(k.clone(), v.clone());
            }
            ev
        };
        let events = vec![
            mk(EventKind::Unknown, json!({"source": "otel"})),
            mk(
                EventKind::Unknown,
                json!({"source": "otel", "x_otel_signal": "logs"}),
            ),
            mk(EventKind::Unknown, json!({"source": "hook"})),
            mk(EventKind::ToolCallFinished, json!({"source": "otel"})),
            mk(EventKind::Unknown, json!({"provider": {"source": "otel"}})),
            mk(EventKind::Unknown, json!({})),
        ];
        let batch = attemptdb_storage::segment::events_to_batch(&events).unwrap();
        let (mask, skipped) = non_telemetry_rows(&batch);
        let want: Vec<bool> = events.iter().map(|e| !e.is_telemetry()).collect();
        let mask = mask.expect("some rows are telemetry");
        let got: Vec<bool> = (0..mask.len()).map(|i| mask.value(i)).collect();
        assert_eq!(got, want);
        assert_eq!(skipped, want.iter().filter(|k| !**k).count() as u64);
        // No telemetry: no mask.
        let batch = attemptdb_storage::segment::events_to_batch(&events[2..]).unwrap();
        assert!(non_telemetry_rows(&batch).0.is_none());
    }
}
