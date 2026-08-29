//! Spool appender for the crash-consistency suite (`tests/crash.rs`).
//!
//! ```text
//! spool_writer <db_dir> <events> [tag]
//! ```
//!
//! Appends `events` events to the spool of `db_dir`, one locked append per
//! event exactly like a hook invocation does, then prints `DONE <n>`.
//! Several instances are started at once to exercise concurrent hook
//! writers sharing one inbox. Each event carries `attrs.writer = <tag>` and
//! `attrs.index = <i>` so the importer can prove nothing was lost or
//! duplicated. The device id comes from the database's identity file when
//! there is one.
//!
//! Fault injection: `ATTEMPTDB_FAILPOINT=<name>[:N]` aborts the process at
//! an engine failpoint (see `attemptdb_storage::failpoint`).

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::{Identity, SpoolWriter};
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: spool_writer <db_dir> <events> [tag]");
        std::process::exit(2);
    }
    let root = PathBuf::from(&args[1]);
    let count: usize = args[2].parse().expect("events must be a number");
    let tag = args.get(3).cloned().unwrap_or_else(|| std::process::id().to_string());
    if let Err(e) = run(&root, count, &tag) {
        eprintln!("spool_writer: {e}");
        std::process::exit(1);
    }
}

fn run(root: &std::path::Path, count: usize, tag: &str) -> Result<(), Box<dyn std::error::Error>> {
    let device = Identity::load(root).map(|id| id.device_id).unwrap_or_else(|_| DeviceId::new());
    let writer = SpoolWriter::new(root)?;
    let project = ProjectRef::derive("/home/dev/example/project", None, &device);
    for i in 0..count {
        let mut ev = Event::new(
            device,
            Provider::Codex,
            "PostToolUse",
            EventKind::ToolCallFinished,
            project.clone(),
            format!("spool-{tag}"),
            CaptureMode::MetadataOnly,
            "spool-writer/0.1",
        );
        ev.attrs.insert("writer".into(), serde_json::json!(tag));
        ev.attrs.insert("index".into(), serde_json::json!(i));
        writer.append(std::slice::from_ref(&ev))?;
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "DONE {count}")?;
    out.flush()?;
    Ok(())
}
