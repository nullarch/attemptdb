//! A synthetic database whose event ids are deterministic (UUIDv5), like the
//! ones OTel and the newer hooks write: every segment's id range spans the
//! whole id space, so the manifest cannot prune a duplicate check by id.
//! Used to measure what opening a database with a few WAL events costs.
//!
//!   v5_db build <dir> <segments> <events-per-segment> <wal-events>
//!   v5_db open  <dir>          # read-only open, prints time and WAL events
//!   v5_db ingest <dir>         # writer open, then one new event (the first
//!                              # duplicate check loads every segment's ids)
//!
//! `<dir>` for `build` must not exist yet. Run `open` under `/usr/bin/time -l`
//! for the peak resident size.

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventId, EventKind, ProjectRef};
use attemptdb_storage::{Database, DurabilityPolicy, OpenOptions};
use std::path::PathBuf;
use std::time::Instant;

fn event(device: DeviceId, project: &ProjectRef, n: u64) -> Event {
    let mut ev = Event::new(
        device,
        Provider::ClaudeCode,
        "PostToolUse",
        EventKind::ToolCallFinished,
        project.clone(),
        format!("session-{}", n / 80),
        CaptureMode::LocalSemantic,
        "v5-db/1",
    );
    ev.event_id = EventId::derive(&["v5-db", &n.to_string()]);
    ev
}

type Res = Result<(), Box<dyn std::error::Error>>;

fn main() -> Res {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("build") if args.len() == 5 => {
            let dir = PathBuf::from(&args[1]);
            let (segments, per, wal): (u64, u64, u64) =
                (args[2].parse()?, args[3].parse()?, args[4].parse()?);
            if dir.exists() {
                return Err(format!("{} already exists", dir.display()).into());
            }
            let device = DeviceId::derive(&["v5-db"]);
            let mut db = Database::open(
                &dir,
                OpenOptions {
                    create: true,
                    device_id: Some(device),
                    durability: DurabilityPolicy::Relaxed,
                    flush_events: usize::MAX,
                    flush_bytes: usize::MAX,
                    ..Default::default()
                },
            )?;
            let project = ProjectRef::derive("/work/alpha", None, &device);
            let mut n = 0u64;
            for _ in 0..segments {
                for chunk in (0..per).collect::<Vec<_>>().chunks(5000) {
                    let batch: Vec<Event> = chunk
                        .iter()
                        .map(|_| {
                            n += 1;
                            event(device, &project, n)
                        })
                        .collect();
                    db.ingest(batch)?;
                }
                db.flush()?;
            }
            let batch: Vec<Event> = (0..wal)
                .map(|_| {
                    n += 1;
                    event(device, &project, n)
                })
                .collect();
            db.ingest(batch)?;
            println!(
                "{}: {} segments, {} events, {} left in the WAL",
                dir.display(),
                db.stats().segments,
                n,
                wal
            );
            // Dropped without `close`, which would flush the WAL events into
            // a segment: they stay in the WAL for `open` to replay.
            drop(db);
            Ok(())
        }
        Some("open") if args.len() == 2 => {
            let t = Instant::now();
            let db = Database::open(
                &PathBuf::from(&args[1]),
                OpenOptions {
                    read_only: true,
                    ..Default::default()
                },
            )?;
            println!(
                "read-only open: {:?}, {} WAL events replayed, {} segments",
                t.elapsed(),
                db.memtable_events().len(),
                db.manifest().segments.len()
            );
            Ok(())
        }
        Some("ingest") if args.len() == 2 => {
            let t = Instant::now();
            let mut db = Database::open(&PathBuf::from(&args[1]), OpenOptions::default())?;
            let opened = t.elapsed();
            let device = db.device_id();
            let project = ProjectRef::derive("/work/alpha", None, &device);
            let t = Instant::now();
            let report = db.ingest(vec![event(device, &project, 9_000_000_000)])?;
            println!(
                "writer open: {opened:?}; first ingest: {:?} ({} accepted, {} duplicates)",
                t.elapsed(),
                report.accepted,
                report.duplicates
            );
            let t = Instant::now();
            db.ingest(vec![event(device, &project, 9_000_000_001)])?;
            println!("second ingest: {:?}", t.elapsed());
            drop(db);
            Ok(())
        }
        _ => Err(
            "usage: v5_db build <dir> <segments> <events-per-segment> <wal-events> | v5_db open <dir> | v5_db ingest <dir>"
                .into(),
        ),
    }
}
