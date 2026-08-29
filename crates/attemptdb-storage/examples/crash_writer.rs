//! Writer process for the crash-consistency suite (`tests/crash.rs`).
//!
//! ```text
//! crash_writer <db_dir> <events_per_batch> <flush_every_batches> [max_batches]
//! ```
//!
//! Creates or opens the database at `db_dir` and ingests synthetic batches
//! until it is killed (or until `max_batches`). After every successful
//! `ingest` it prints exactly one line
//!
//! ```text
//! ACK <first_seq>..<last_seq> <last_event_id>
//! ```
//!
//! and after every `flush` one line `FLUSH <generation>`; stdout is flushed
//! after each line so the parent sees an acknowledgement the moment the
//! engine returned `Ok`. Automatic flush thresholds are disabled: a flush
//! happens only every `flush_every_batches` batches, where this file calls
//! `flush()` (0 disables flushing). On a normal exit the database is
//! dropped *without* a final flush so the WAL still holds the last batches;
//! the suite relies on that to corrupt a WAL tail.
//!
//! Fault injection: `ATTEMPTDB_FAILPOINT=<name>[:N]` aborts the process at
//! an engine failpoint (see `attemptdb_storage::failpoint`).

use attemptdb_core::event::{EventContent, Outcome, OutcomeStatus, Provider, ToolCategory, ToolRef};
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, PortablePath, ProjectRef};
use attemptdb_storage::{Database, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: crash_writer <db_dir> <events_per_batch> <flush_every_batches> [max_batches]");
        std::process::exit(2);
    }
    let root = PathBuf::from(&args[1]);
    let per_batch: usize = args[2].parse().expect("events_per_batch must be a number");
    let flush_every: u64 = args[3].parse().expect("flush_every_batches must be a number");
    let max_batches: Option<u64> = args.get(4).map(|s| s.parse().expect("max_batches must be a number"));
    if let Err(e) = run(&root, per_batch.max(1), flush_every, max_batches) {
        eprintln!("crash_writer: {e}");
        std::process::exit(1);
    }
}

fn run(
    root: &std::path::Path,
    per_batch: usize,
    flush_every: u64,
    max_batches: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut db = Database::open(
        root,
        OpenOptions { create: true, flush_events: usize::MAX, flush_bytes: usize::MAX, ..Default::default() },
    )?;
    let device = db.device_id();
    let session = format!("crash-writer-{}", std::process::id());
    let mut rng = Rng::new(u64::from(std::process::id()) ^ 0x9e37_79b9_7f4a_7c15);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut batch_no = 0u64;
    while max_batches.is_none_or(|m| batch_no < m) {
        let events: Vec<Event> = (0..per_batch).map(|i| synthetic_event(device, &session, &mut rng, batch_no, i)).collect();
        let last_id = events.last().expect("per_batch >= 1").event_id;
        let report = db.ingest(events)?;
        let last_seq = db.stats().last_source_seq;
        let first_seq = last_seq + 1 - report.accepted as u64;
        writeln!(out, "ACK {first_seq}..{last_seq} {last_id}")?;
        out.flush()?;
        batch_no += 1;
        if flush_every > 0 && batch_no.is_multiple_of(flush_every) {
            db.flush()?;
            writeln!(out, "FLUSH {}", db.manifest().generation)?;
            out.flush()?;
        }
    }
    Ok(())
}

/// One realistic `ToolCallFinished` event: allowlisted attrs plus a
/// `content.tool_output` of 200–2000 bytes.
fn synthetic_event(device: DeviceId, session: &str, rng: &mut Rng, batch: u64, i: usize) -> Event {
    let project = ProjectRef::derive("/home/dev/example/project", Some("git@github.com:example/project.git"), &device);
    let mut ev = Event::new(
        device,
        Provider::ClaudeCode,
        "PostToolUse",
        EventKind::ToolCallFinished,
        project,
        session,
        CaptureMode::LocalSemantic,
        "crash-writer/0.1",
    );
    let (tool, category) = match rng.next() % 4 {
        0 => ("Bash", ToolCategory::Shell),
        1 => ("Edit", ToolCategory::FileEdit),
        2 => ("Read", ToolCategory::FileRead),
        _ => ("Grep", ToolCategory::Search),
    };
    ev.tool = Some(ToolRef { name: tool.into(), category, call_id: Some(format!("toolu_{batch:04}_{i:03}")) });
    ev.paths.push(PortablePath::from_raw(
        &format!("/home/dev/example/project/src/module_{}/file_{}.rs", rng.next() % 12, rng.next() % 40),
        Some("/home/dev/example/project"),
    ));
    let failed = rng.next().is_multiple_of(9);
    ev.outcome = Some(Outcome {
        status: if failed { OutcomeStatus::Failure } else { OutcomeStatus::Success },
        class: failed.then(|| "nonzero_exit".to_string()),
        exit_code: Some(if failed { 1 } else { 0 }),
    });
    ev.duration_ms = Some(rng.next() % 5_000);
    ev.attrs.insert("batch".into(), serde_json::json!(batch));
    ev.attrs.insert("index".into(), serde_json::json!(i));
    ev.attrs.insert("file_ext".into(), serde_json::json!("rs"));
    ev.attrs.insert("output_bytes".into(), serde_json::json!(0));
    let output_len = 200 + (rng.next() % 1_801) as usize;
    let output = lorem(rng, output_len);
    ev.attrs["output_bytes"] = serde_json::json!(output.len());
    ev.content = Some(EventContent {
        command: Some(format!("cargo test -p crate_{} -- --nocapture", rng.next() % 7)),
        tool_output: Some(serde_json::Value::String(output)),
        ..Default::default()
    });
    ev
}

fn lorem(rng: &mut Rng, len: usize) -> String {
    const WORDS: &[&str] = &[
        "compiling", "warning:", "unused", "variable", "test", "result:", "ok.", "running", "3", "tests",
        "finished", "in", "0.42s", "error[E0308]:", "mismatched", "types", "-->", "src/lib.rs:12:5",
    ];
    let mut s = String::with_capacity(len + 16);
    while s.len() < len {
        s.push_str(WORDS[(rng.next() % WORDS.len() as u64) as usize]);
        s.push(if rng.next().is_multiple_of(11) { '\n' } else { ' ' });
    }
    s.truncate(len);
    s
}

/// Tiny xorshift generator; the example must not pull in a dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}
