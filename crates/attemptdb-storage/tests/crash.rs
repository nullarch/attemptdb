//! Crash-consistency and fault-injection suite.
//!
//! The durability contract under test (`docs/storage-format.md`): once an
//! ingest call returned `Ok` under `DurabilityPolicy::Strict`, the events
//! are in the WAL and fsynced; every later `open` must show them exactly
//! once, `source_seq` must stay contiguous, the newest valid manifest
//! generation wins, and the spool is a transport rather than the
//! durability boundary.
//!
//! Crashes are real process deaths: the `crash_writer` example is run as a
//! child, its stdout is read live so the set of acknowledged batches is
//! known exactly, and it is killed either by SIGKILL at a random moment or
//! by an engine failpoint (`ATTEMPTDB_FAILPOINT=<name>[:N]`, which aborts
//! at a precise step of the flush protocol). Disk-full behaviour is tested
//! in-process through `failpoint::arm_io`.
//!
//! Reproducing a random-kill failure: the seed is printed; rerun with
//! `ATTEMPTDB_CRASH_SEED=<seed>`.

#![cfg(unix)]

use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{CaptureMode, DeviceId, Event, EventId, EventKind, ProjectRef};
use attemptdb_storage::format::MAGIC_WAL;
use attemptdb_storage::frame::FrameReader;
use attemptdb_storage::{Database, OpenOptions, ScanFilter, SpoolWriter, StorageError, failpoint};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SIGKILL: i32 = 9;
const SIGABRT: i32 = 6;

// ---------------------------------------------------------------------------
// Example binaries
// ---------------------------------------------------------------------------

/// Locate `target/<profile>/examples/<name>`; cargo builds examples before
/// integration tests, but build it on demand if it is missing.
fn example_bin(name: &str) -> PathBuf {
    static BUILD_LOCK: Mutex<()> = Mutex::new(());
    let exe = std::env::current_exe().expect("current_exe");
    // target/<profile>/deps/crash-<hash> -> target/<profile>
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| panic!("unexpected test binary location {}", exe.display()))
        .to_path_buf();
    let bin = profile_dir.join("examples").join(name);
    if bin.is_file() {
        return bin;
    }
    let _guard = BUILD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if !bin.is_file() {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut cmd = Command::new(cargo);
        cmd.args(["build", "-p", "attemptdb-storage", "--example", name]).current_dir(env!("CARGO_MANIFEST_DIR"));
        if profile_dir.file_name().and_then(|n| n.to_str()) == Some("release") {
            cmd.arg("--release");
        }
        let status = cmd.status().expect("run cargo build --example");
        assert!(status.success(), "building example {name} failed");
    }
    assert!(bin.is_file(), "example binary {} is missing", bin.display());
    bin
}

// ---------------------------------------------------------------------------
// Writer process wrapper
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Ack {
    first: u64,
    last: u64,
    last_id: EventId,
}

struct Run {
    status: ExitStatus,
    acks: Vec<Ack>,
    flushes: Vec<u64>,
    stderr: String,
}

impl Run {
    fn max_acked_seq(&self) -> u64 {
        self.acks.iter().map(|a| a.last).max().unwrap_or(0)
    }
}

struct Writer {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<String>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Writer {
    fn spawn(root: &Path, per_batch: usize, flush_every: u64, max_batches: Option<u64>, env: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(example_bin("crash_writer"));
        cmd.arg(root).arg(per_batch.to_string()).arg(flush_every.to_string());
        if let Some(m) = max_batches {
            cmd.arg(m.to_string());
        }
        cmd.env_remove(failpoint::ENV_ABORT).env_remove(failpoint::ENV_IO);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn crash_writer");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(String::new()));
        let mut threads = Vec::new();
        {
            let out = child.stdout.take().expect("piped stdout");
            let lines = Arc::clone(&lines);
            threads.push(std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    lines.lock().unwrap().push(line);
                }
            }));
        }
        {
            let mut err = child.stderr.take().expect("piped stderr");
            let stderr = Arc::clone(&stderr);
            threads.push(std::thread::spawn(move || {
                let mut s = String::new();
                let _ = err.read_to_string(&mut s);
                *stderr.lock().unwrap() = s;
            }));
        }
        Self { child, lines, stderr, threads }
    }

    fn ack_count(&self) -> usize {
        self.lines.lock().unwrap().iter().filter(|l| l.starts_with("ACK ")).count()
    }

    /// Wait until the writer acknowledged at least one batch (so the
    /// database exists and the kill lands on a running writer).
    fn wait_for_first_ack(&mut self, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            if self.ack_count() > 0 {
                return true;
            }
            if self.child.try_wait().expect("try_wait").is_some() {
                return self.ack_count() > 0;
            }
            if start.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
    }

    /// Wait for the process to exit (killing it after `timeout`), then
    /// collect everything it printed.
    fn finish(mut self, timeout: Duration) -> Run {
        let start = Instant::now();
        let status = loop {
            if let Some(s) = self.child.try_wait().expect("try_wait") {
                break s;
            }
            if start.elapsed() > timeout {
                self.kill();
                let s = self.child.wait().expect("wait");
                eprintln!("writer did not exit within {timeout:?}; killed");
                break s;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        for t in self.threads.drain(..) {
            t.join().expect("reader thread");
        }
        let lines = self.lines.lock().unwrap().clone();
        let stderr = self.stderr.lock().unwrap().clone();
        let mut acks = Vec::new();
        let mut flushes = Vec::new();
        for line in &lines {
            let parts: Vec<&str> = line.split_whitespace().collect();
            match parts.as_slice() {
                ["ACK", range, id] => {
                    let (first, last) = range.split_once("..").expect("ACK range");
                    acks.push(Ack {
                        first: first.parse().expect("first seq"),
                        last: last.parse().expect("last seq"),
                        last_id: id.parse().expect("event id"),
                    });
                }
                ["FLUSH", generation] => flushes.push(generation.parse().expect("generation")),
                _ => panic!("unexpected writer output line: {line:?}"),
            }
        }
        Run { status, acks, flushes, stderr }
    }
}

fn run_spool_writer(root: &Path, count: usize, tag: &str, env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(example_bin("spool_writer"));
    cmd.arg(root).arg(count.to_string()).arg(tag);
    cmd.env_remove(failpoint::ENV_ABORT).env_remove(failpoint::ENV_IO);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("run spool_writer")
}

// ---------------------------------------------------------------------------
// Invariants
// ---------------------------------------------------------------------------

/// Warnings a crash may legitimately produce on the first open afterwards.
fn is_benign_crash_warning(w: &str) -> bool {
    (w.starts_with("recovered ") && w.contains("torn tail"))
        || w.starts_with("removed stale temp file")
        || w.starts_with("unreferenced segment file")
}

#[derive(Clone, Copy, Debug)]
struct Summary {
    max_seq: u64,
    events: usize,
    generation: u64,
}

fn all_events(db: &Database) -> Vec<Event> {
    db.scan(&ScanFilter::default()).expect("scan")
}

/// Every ACKed batch present exactly once, `source_seq` contiguous from 1,
/// no duplicate ids, stats consistent with the scan.
fn assert_contents(db: &Database, acks: &[Ack], context: &str) -> Summary {
    let events = all_events(db);
    let mut seqs: Vec<u64> = events.iter().map(|e| e.source_seq).collect();
    seqs.sort_unstable();
    for (i, s) in seqs.iter().enumerate() {
        assert_eq!(*s, i as u64 + 1, "{context}: source_seq is not contiguous at index {i}: {seqs:?}");
    }
    let ids: HashSet<EventId> = events.iter().map(|e| e.event_id).collect();
    assert_eq!(ids.len(), events.len(), "{context}: duplicate event ids after recovery");
    let seq_of: HashMap<EventId, u64> = events.iter().map(|e| (e.event_id, e.source_seq)).collect();
    let max_seq = seqs.last().copied().unwrap_or(0);
    for a in acks {
        assert!(
            a.last <= max_seq,
            "{context}: acknowledged range {}..{} lost (highest recovered seq {max_seq})",
            a.first,
            a.last
        );
        assert_eq!(
            seq_of.get(&a.last_id),
            Some(&a.last),
            "{context}: acknowledged event {} (seq {}) missing or renumbered",
            a.last_id,
            a.last
        );
    }
    assert!(events.iter().all(|e| e.is_ingested() && e.ingested_at.is_some()), "{context}: un-ingested event");
    assert_eq!(db.stats().last_source_seq, max_seq, "{context}: stats disagree with scan");
    Summary { max_seq, events: events.len(), generation: db.manifest().generation }
}

/// Open after a crash as the writer, verify, then reopen read-only and as
/// a writer again (recovery must be idempotent).
fn check_recovered(root: &Path, acks: &[Ack], context: &str) -> Summary {
    let db = Database::open(root, OpenOptions::default()).unwrap_or_else(|e| panic!("{context}: open failed: {e}"));
    for w in &db.warnings {
        assert!(is_benign_crash_warning(w), "{context}: unexpected warning: {w}");
    }
    let problems = db.verify().unwrap_or_else(|e| panic!("{context}: verify failed: {e}"));
    assert!(problems.is_empty(), "{context}: verify problems: {problems:?}");
    let summary = assert_contents(&db, acks, context);
    drop(db);
    for read_only in [true, false] {
        let db = Database::open(root, OpenOptions { read_only, ..Default::default() })
            .unwrap_or_else(|e| panic!("{context}: reopen (read_only={read_only}) failed: {e}"));
        for w in &db.warnings {
            // Torn tails are truncated and temp files removed by the first
            // writer open; only the unreferenced-segment note may persist.
            assert!(w.starts_with("unreferenced segment file"), "{context}: warning on reopen: {w}");
        }
        assert!(db.verify().unwrap().is_empty(), "{context}: verify on reopen");
        let again = assert_contents(&db, acks, context);
        assert_eq!(again.max_seq, summary.max_seq, "{context}: reopen changed max seq");
        assert_eq!(again.events, summary.events, "{context}: reopen changed event count");
        assert_eq!(again.generation, summary.generation, "{context}: reopen changed generation");
    }
    summary
}

/// Open as a writer and keep going: the sequence continues without a gap,
/// a flush publishes the next generation, unflushed events survive a
/// reopen.
fn continue_writing(root: &Path, from: Summary, context: &str) {
    let mut db = Database::open(root, OpenOptions { flush_events: usize::MAX, flush_bytes: usize::MAX, ..Default::default() })
        .unwrap_or_else(|e| panic!("{context}: open for writing failed: {e}"));
    let device = db.device_id();
    let r = db.ingest(make_events(device, 7, "continue-a")).unwrap();
    assert_eq!(r.accepted, 7, "{context}");
    assert_eq!(db.stats().last_source_seq, from.max_seq + 7, "{context}: sequence did not continue");
    let meta = db.flush().unwrap().expect("something to flush");
    // The segment holds the new batch plus whatever the WAL still had.
    assert_eq!(meta.max_source_seq, from.max_seq + 7, "{context}: new segment ends at the last seq");
    assert!(meta.min_source_seq <= from.max_seq + 1, "{context}: new segment covers the unflushed tail");
    assert_eq!(db.manifest().generation, from.generation + 1, "{context}: generation did not advance by one");
    let r = db.ingest(make_events(device, 5, "continue-b")).unwrap();
    assert_eq!(r.accepted, 5, "{context}");
    drop(db); // no flush: the last batch stays in the WAL
    let db = Database::open(root, OpenOptions::default()).unwrap();
    assert!(db.warnings.iter().all(|w| w.starts_with("unreferenced segment file")), "{context}: {:?}", db.warnings);
    assert!(db.verify().unwrap().is_empty(), "{context}");
    let s = assert_contents(&db, &[], context);
    assert_eq!(s.events, from.events + 12, "{context}: events after continuing");
    assert_eq!(s.max_seq, from.max_seq + 12, "{context}: max seq after continuing");
}

fn make_events(device: DeviceId, n: usize, tag: &str) -> Vec<Event> {
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                device,
                Provider::ClaudeCode,
                "PostToolUse",
                EventKind::ToolCallFinished,
                ProjectRef::derive("/home/dev/example/project", None, &device),
                format!("session-{tag}"),
                CaptureMode::LocalSemantic,
                "crash-test/0.1",
            );
            ev.attrs.insert("tag".into(), serde_json::json!(tag));
            ev.attrs.insert("index".into(), serde_json::json!(i));
            ev.content = Some(EventContent {
                tool_output: Some(serde_json::Value::String("output ".repeat(40 + i))),
                ..Default::default()
            });
            ev
        })
        .collect()
}

fn writer_options() -> OpenOptions {
    OpenOptions { create: true, flush_events: usize::MAX, flush_bytes: usize::MAX, ..Default::default() }
}

fn files_with_extension(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().and_then(|e| e.to_str()) == Some(ext)).collect())
        .unwrap_or_default();
    out.sort();
    out
}

fn temp_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("db.attemptdb");
    (dir, root)
}

/// xorshift64*, enough for reproducible delays.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

fn crash_seed() -> u64 {
    std::env::var("ATTEMPTDB_CRASH_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            (t.as_nanos() as u64) ^ u64::from(std::process::id()).rotate_left(32)
        })
        .max(1)
}

// ---------------------------------------------------------------------------
// Random SIGKILL
// ---------------------------------------------------------------------------

fn sigkill_rounds(rounds: u32, salt: u64) {
    let seed = crash_seed().wrapping_add(salt);
    println!("random SIGKILL seed = {seed} (rerun with ATTEMPTDB_CRASH_SEED={})", seed.wrapping_sub(salt));
    eprintln!("random SIGKILL seed = {seed} (rerun with ATTEMPTDB_CRASH_SEED={})", seed.wrapping_sub(salt));
    let mut rng = Rng(seed);
    for round in 0..rounds {
        let (_dir, root) = temp_root();
        let mut writer = Writer::spawn(&root, 25, 3, None, &[]);
        assert!(
            writer.wait_for_first_ack(Duration::from_secs(30)),
            "seed {seed} round {round}: writer never acknowledged a batch; stderr: {}",
            writer.stderr.lock().unwrap()
        );
        let delay = 10 + rng.next() % 291;
        std::thread::sleep(Duration::from_millis(delay));
        writer.kill();
        let run = writer.finish(Duration::from_secs(10));
        assert_eq!(run.status.signal(), Some(SIGKILL), "seed {seed} round {round}: {}", run.stderr);
        let context = format!(
            "seed {seed} round {round} (killed after {delay} ms, {} acks, {} flushes)",
            run.acks.len(),
            run.flushes.len()
        );
        let summary = check_recovered(&root, &run.acks, &context);
        assert!(summary.max_seq >= run.max_acked_seq(), "{context}");
        continue_writing(&root, summary, &context);
    }
}

#[test]
fn random_sigkill_rounds_a() {
    sigkill_rounds(5, 0);
}

#[test]
fn random_sigkill_rounds_b() {
    sigkill_rounds(5, 0x5eed);
}

// ---------------------------------------------------------------------------
// Abort failpoints
// ---------------------------------------------------------------------------

/// Run the writer until `spec` aborts it (12 batches of 20, a flush every
/// 2 batches, so every flush-protocol point is reached several times).
fn abort_run(root: &Path, spec: &str, max_batches: u64) -> Run {
    let writer = Writer::spawn(root, 20, 2, Some(max_batches), &[(failpoint::ENV_ABORT, spec)]);
    let run = writer.finish(Duration::from_secs(60));
    let name = spec.split(':').next().unwrap();
    assert_eq!(
        run.status.signal(),
        Some(SIGABRT),
        "failpoint {spec} did not abort the writer (exit {:?}); stderr: {}",
        run.status,
        run.stderr
    );
    assert!(run.stderr.contains(&format!("aborting at `{name}`")), "stderr: {}", run.stderr);
    run
}

fn abort_case(spec: &str) {
    let (_dir, root) = temp_root();
    let run = abort_run(&root, spec, 12);
    let context = format!("failpoint {spec} ({} acks, {} flushes)", run.acks.len(), run.flushes.len());
    let summary = check_recovered(&root, &run.acks, &context);
    continue_writing(&root, summary, &context);
}

#[test]
fn abort_wal_append_after_write() {
    abort_case(failpoint::WAL_APPEND_AFTER_WRITE);
    abort_case(&format!("{}:5", failpoint::WAL_APPEND_AFTER_WRITE));
}

#[test]
fn abort_wal_append_after_sync() {
    abort_case(failpoint::WAL_APPEND_AFTER_SYNC);
    abort_case(&format!("{}:4", failpoint::WAL_APPEND_AFTER_SYNC));
}

#[test]
fn abort_segment_after_tmp_write_leaves_a_tolerated_tmp_file() {
    for spec in [failpoint::SEGMENT_AFTER_TMP_WRITE.to_string(), format!("{}:2", failpoint::SEGMENT_AFTER_TMP_WRITE)] {
        let (_dir, root) = temp_root();
        let run = abort_run(&root, &spec, 12);
        let segments = root.join("segments");
        assert_eq!(files_with_extension(&segments, "tmp").len(), 1, "{spec}: torn temp segment left behind");
        // A reader tolerates it and does not touch it.
        let ro = Database::open(&root, OpenOptions { read_only: true, ..Default::default() }).unwrap();
        assert!(ro.warnings.is_empty(), "{spec}: {:?}", ro.warnings);
        drop(ro);
        assert_eq!(files_with_extension(&segments, "tmp").len(), 1);
        // The next writer removes it and says so.
        let context = format!("failpoint {spec}");
        let summary = check_recovered(&root, &run.acks, &context);
        assert!(files_with_extension(&segments, "tmp").is_empty(), "{spec}: stale temp file not removed");
        continue_writing(&root, summary, &context);
    }
}

#[test]
fn abort_segment_after_rename() {
    abort_case(failpoint::SEGMENT_AFTER_RENAME);
    let spec = format!("{}:3", failpoint::SEGMENT_AFTER_RENAME);
    let (_dir, root) = temp_root();
    let run = abort_run(&root, &spec, 12);
    // The segment is published but no generation names it: the WAL still
    // holds every event, and the file is reported as unreferenced.
    let db = Database::open(&root, OpenOptions::default()).unwrap();
    assert!(db.warnings.iter().any(|w| w.starts_with("unreferenced segment file")), "{:?}", db.warnings);
    drop(db);
    let summary = check_recovered(&root, &run.acks, &spec);
    continue_writing(&root, summary, &spec);
}

/// The first manifest write is the one in `Database::create`; a crash
/// there leaves a directory that is not yet a database (no `ATTEMPTDB`
/// file). Nothing was acknowledged, and `create: true` finishes the job.
#[test]
fn abort_during_create_is_recoverable() {
    for spec in [failpoint::MANIFEST_AFTER_TMP_WRITE, failpoint::MANIFEST_AFTER_RENAME] {
        let (_dir, root) = temp_root();
        let run = abort_run(&root, spec, 4);
        assert!(run.acks.is_empty(), "{spec}: nothing can be acknowledged before the database exists");
        assert!(!Database::exists(&root), "{spec}");
        assert!(matches!(Database::open(&root, OpenOptions::default()), Err(StorageError::NotADatabase(_))), "{spec}");
        let db = Database::open(&root, writer_options()).unwrap_or_else(|e| panic!("{spec}: create after crash: {e}"));
        assert!(db.warnings.is_empty(), "{spec}: {:?}", db.warnings);
        assert_eq!(db.manifest().generation, 1);
        drop(db);
        continue_writing(&root, Summary { max_seq: 0, events: 0, generation: 1 }, spec);
    }
}

#[test]
fn abort_manifest_after_tmp_write_leaves_a_tolerated_tmp_file() {
    // Hit 1 is the generation written by `create` (covered above); hits 2
    // and 3 are the first and second flush.
    for spec in [format!("{}:2", failpoint::MANIFEST_AFTER_TMP_WRITE), format!("{}:3", failpoint::MANIFEST_AFTER_TMP_WRITE)] {
        let (_dir, root) = temp_root();
        let run = abort_run(&root, &spec, 12);
        let manifests = root.join("manifest");
        assert_eq!(files_with_extension(&manifests, "tmp").len(), 1, "{spec}: torn temp manifest left behind");
        let ro = Database::open(&root, OpenOptions { read_only: true, ..Default::default() }).unwrap();
        // The generation before the crash is current; the flushed segment
        // exists but is unreferenced; the .tmp is ignored, not rejected.
        assert!(ro.warnings.iter().all(|w| w.starts_with("unreferenced segment file")), "{spec}: {:?}", ro.warnings);
        assert_eq!(ro.warnings.len(), 1, "{spec}: the published-but-unreferenced segment: {:?}", ro.warnings);
        assert_eq!(ro.manifest().generation as usize, run.flushes.len() + 1, "{spec}: generation before the crash");
        drop(ro);
        assert_eq!(files_with_extension(&manifests, "tmp").len(), 1);
        let context = format!("failpoint {spec}");
        let summary = check_recovered(&root, &run.acks, &context);
        assert!(files_with_extension(&manifests, "tmp").is_empty(), "{spec}: stale temp file not removed");
        continue_writing(&root, summary, &context);
    }
}

#[test]
fn abort_manifest_after_rename() {
    // Hit 1 is `create` (covered above); hits 2 and 3 are flushes.
    abort_case(&format!("{}:2", failpoint::MANIFEST_AFTER_RENAME));
    abort_case(&format!("{}:3", failpoint::MANIFEST_AFTER_RENAME));
}

#[test]
fn abort_flush_after_manifest_before_wal_truncate() {
    abort_case(failpoint::FLUSH_AFTER_MANIFEST_BEFORE_WAL_TRUNCATE);
    let spec = format!("{}:3", failpoint::FLUSH_AFTER_MANIFEST_BEFORE_WAL_TRUNCATE);
    let (_dir, root) = temp_root();
    let run = abort_run(&root, &spec, 12);
    // The generation is durable and the old WAL file survived: replay must
    // deduplicate against the segment, and the generation must be the one
    // the writer had just published (not reported as a flush, it died
    // before printing).
    let db = Database::open(&root, OpenOptions::default()).unwrap();
    assert_eq!(db.manifest().generation as usize, run.flushes.len() + 2, "generation published before the crash");
    assert!(files_with_extension(&root.join("wal"), "wal").len() >= 2, "old WAL file still present");
    drop(db);
    let summary = check_recovered(&root, &run.acks, &spec);
    continue_writing(&root, summary, &spec);
}

#[test]
fn abort_wal_truncate_mid_between_two_deletions() {
    // A crash right after the manifest rename leaves an extra WAL file, so
    // the next flush has two files to delete and `wal.truncate.mid` lands
    // between them.
    let (_dir, root) = temp_root();
    let first = abort_run(&root, &format!("{}:2", failpoint::MANIFEST_AFTER_RENAME), 12);
    let wal_dir = root.join("wal");
    assert_eq!(files_with_extension(&wal_dir, "wal").len(), 2);
    let second = abort_run(&root, failpoint::WAL_TRUNCATE_MID, 12);
    assert_eq!(
        second.acks.first().map(|a| a.first),
        Some(first.max_acked_seq() + 1),
        "second run continues the sequence (first run stopped at {})",
        first.max_acked_seq()
    );
    let remaining = files_with_extension(&wal_dir, "wal");
    assert_eq!(remaining.len(), 2, "exactly one of the two old files was deleted: {remaining:?}");
    let mut acks = first.acks.clone();
    acks.extend(second.acks.iter().cloned());
    let summary = check_recovered(&root, &acks, "wal.truncate.mid");
    continue_writing(&root, summary, "wal.truncate.mid");
    // Plain single-file case too.
    abort_case(failpoint::WAL_TRUNCATE_MID);
}

fn spool_abort_case(spec: &str, written_before_abort: usize) {
    let (_dir, root) = temp_root();
    Database::create(&root, DeviceId::new()).unwrap();
    let out = run_spool_writer(&root, 10, "crashy", &[(failpoint::ENV_ABORT, spec)]);
    assert_eq!(out.status.signal(), Some(SIGABRT), "{spec}: {}", String::from_utf8_lossy(&out.stderr));
    // The next hook appends with the stale committed-length sidecar.
    let out = run_spool_writer(&root, 5, "after", &[]);
    assert!(out.status.success(), "{spec}: {}", String::from_utf8_lossy(&out.stderr));
    let mut db = Database::open(&root, OpenOptions::default()).unwrap();
    let r = db.import_spool().unwrap();
    assert!(db.warnings.is_empty(), "{spec}: a complete record was written before the abort: {:?}", db.warnings);
    assert_eq!(r.undecodable, 0, "{spec}");
    assert_eq!(r.accepted, written_before_abort + 5, "{spec}");
    let events = all_events(&db);
    let crashy: HashSet<u64> = events
        .iter()
        .filter(|e| e.attr_str("writer") == Some("crashy"))
        .map(|e| e.attrs["index"].as_u64().unwrap())
        .collect();
    assert_eq!(crashy, (0..written_before_abort as u64).collect(), "{spec}");
    assert!(db.verify().unwrap().is_empty());
}

#[test]
fn abort_spool_append_after_write() {
    spool_abort_case(failpoint::SPOOL_APPEND_AFTER_WRITE, 1);
    spool_abort_case(&format!("{}:3", failpoint::SPOOL_APPEND_AFTER_WRITE), 3);
}

#[test]
fn abort_spool_committed_before_write() {
    spool_abort_case(failpoint::SPOOL_COMMITTED_BEFORE_WRITE, 1);
    spool_abort_case(&format!("{}:7", failpoint::SPOOL_COMMITTED_BEFORE_WRITE), 7);
}

/// Every abort point the engine defines has a test above.
#[test]
fn every_abort_point_is_covered() {
    const COVERED: &[&str] = &[
        failpoint::WAL_APPEND_AFTER_WRITE,
        failpoint::WAL_APPEND_AFTER_SYNC,
        failpoint::SEGMENT_AFTER_TMP_WRITE,
        failpoint::SEGMENT_AFTER_RENAME,
        failpoint::MANIFEST_AFTER_TMP_WRITE,
        failpoint::MANIFEST_AFTER_RENAME,
        failpoint::FLUSH_AFTER_MANIFEST_BEFORE_WAL_TRUNCATE,
        failpoint::WAL_TRUNCATE_MID,
        failpoint::SPOOL_APPEND_AFTER_WRITE,
        failpoint::SPOOL_COMMITTED_BEFORE_WRITE,
    ];
    let mut covered = COVERED.to_vec();
    covered.sort_unstable();
    let mut defined = failpoint::ABORT_POINTS.to_vec();
    defined.sort_unstable();
    assert_eq!(covered, defined, "add a crash test for every new abort point");
    let mut io_covered = vec![failpoint::WAL_WRITE, failpoint::SEGMENT_WRITE, failpoint::MANIFEST_WRITE, failpoint::SPOOL_WRITE];
    io_covered.sort_unstable();
    let mut io_defined = failpoint::IO_POINTS.to_vec();
    io_defined.sort_unstable();
    assert_eq!(io_covered, io_defined, "add a disk-full test for every new I/O point");
}

// ---------------------------------------------------------------------------
// Disk full (in-process)
// ---------------------------------------------------------------------------

fn is_enospc(e: &StorageError) -> bool {
    matches!(e, StorageError::Io { source, .. } if source.kind() == std::io::ErrorKind::StorageFull)
}

#[test]
fn disk_full_on_wal_write_keeps_sequence_and_recovers() {
    let (_dir, root) = temp_root();
    let mut db = Database::open(&root, writer_options()).unwrap();
    let device = db.device_id();
    db.ingest(make_events(device, 10, "a")).unwrap();
    let second = make_events(device, 10, "b");
    failpoint::arm_io(failpoint::WAL_WRITE);
    let err = db.ingest(second.clone()).unwrap_err();
    assert!(is_enospc(&err), "{err}");
    assert!(!failpoint::io_armed());
    // The half-written batch was discarded; nothing acknowledged is lost.
    assert!(db.verify().unwrap().is_empty(), "torn batch must not stay in the WAL");
    assert_eq!(db.stats().last_source_seq, 10);
    assert_eq!(all_events(&db).len(), 10);
    // The retry continues the sequence without a gap.
    let r = db.ingest(second).unwrap();
    assert_eq!(r.accepted, 10);
    assert_contents(&db, &[], "after retry");
    assert_eq!(db.stats().last_source_seq, 20);
    db.flush().unwrap();
    drop(db);
    let db = Database::open(&root, OpenOptions::default()).unwrap();
    assert!(db.warnings.is_empty(), "{:?}", db.warnings);
    assert!(db.verify().unwrap().is_empty());
    let s = assert_contents(&db, &[], "after reopen");
    assert_eq!(s.events, 20);
}

#[test]
fn disk_full_on_segment_write_keeps_memtable_and_recovers() {
    let (_dir, root) = temp_root();
    let mut db = Database::open(&root, writer_options()).unwrap();
    let device = db.device_id();
    db.ingest(make_events(device, 10, "a")).unwrap();
    failpoint::arm_io(failpoint::SEGMENT_WRITE);
    let err = db.flush().unwrap_err();
    assert!(is_enospc(&err), "{err}");
    let segments = root.join("segments");
    assert_eq!(files_with_extension(&segments, "tmp").len(), 1, "torn temp segment left behind");
    // Nothing changed: the events are still served and still deduplicated.
    assert_eq!(db.stats().memtable_rows, 10);
    assert_eq!(db.manifest().generation, 1);
    assert_eq!(all_events(&db).len(), 10);
    assert!(db.verify().unwrap().is_empty());
    let r = db.ingest(make_events(device, 5, "b")).unwrap();
    assert_eq!(r.accepted, 5);
    let meta = db.flush().unwrap().unwrap();
    assert_eq!(meta.rows, 15);
    assert_eq!(db.manifest().generation, 2);
    assert_eq!(db.stats().memtable_rows, 0);
    drop(db);
    let db = Database::open(&root, OpenOptions::default()).unwrap();
    assert_eq!(db.warnings.len(), 1, "{:?}", db.warnings);
    assert!(db.warnings[0].starts_with("removed stale temp file"), "{:?}", db.warnings);
    assert!(files_with_extension(&segments, "tmp").is_empty());
    assert!(db.verify().unwrap().is_empty());
    let s = assert_contents(&db, &[], "after reopen");
    assert_eq!(s.events, 15);
}

#[test]
fn disk_full_on_manifest_write_keeps_previous_generation_and_recovers() {
    let (_dir, root) = temp_root();
    let mut db = Database::open(&root, writer_options()).unwrap();
    let device = db.device_id();
    db.ingest(make_events(device, 10, "a")).unwrap();
    failpoint::arm_io(failpoint::MANIFEST_WRITE);
    let err = db.flush().unwrap_err();
    assert!(is_enospc(&err), "{err}");
    let manifests = root.join("manifest");
    assert_eq!(files_with_extension(&manifests, "tmp").len(), 1, "torn temp manifest left behind");
    assert_eq!(db.manifest().generation, 1);
    assert_eq!(db.stats().memtable_rows, 10, "memtable must survive a failed publish");
    assert_eq!(all_events(&db).len(), 10);
    assert!(db.verify().unwrap().is_empty());
    // Retry: a fresh segment and the generation the failed attempt meant to write.
    let meta = db.flush().unwrap().unwrap();
    assert_eq!(meta.rows, 10);
    assert_eq!(db.manifest().generation, 2);
    assert_eq!(db.manifest().segments.len(), 1);
    assert!(files_with_extension(&manifests, "tmp").is_empty(), "retry reused and published the temp name");
    db.ingest(make_events(device, 3, "b")).unwrap();
    drop(db);
    let db = Database::open(&root, OpenOptions::default()).unwrap();
    // The segment of the failed attempt is left in place and reported.
    assert_eq!(db.warnings.len(), 1, "{:?}", db.warnings);
    assert!(db.warnings[0].starts_with("unreferenced segment file"), "{:?}", db.warnings);
    assert!(db.verify().unwrap().is_empty());
    let s = assert_contents(&db, &[], "after reopen");
    assert_eq!(s.events, 13);
    assert_eq!(s.generation, 2);
}

#[test]
fn disk_full_on_spool_write_discards_the_torn_batch() {
    let (_dir, root) = temp_root();
    let device = DeviceId::new();
    Database::create(&root, device).unwrap();
    let writer = SpoolWriter::new(&root).unwrap();
    writer.append(&make_events(device, 3, "a")).unwrap();
    failpoint::arm_io(failpoint::SPOOL_WRITE);
    let err = writer.append(&make_events(device, 3, "b")).unwrap_err();
    assert!(is_enospc(&err), "{err}");
    writer.append(&make_events(device, 2, "c")).unwrap();
    let mut db = Database::open(&root, OpenOptions::default()).unwrap();
    let r = db.import_spool().unwrap();
    assert!(db.warnings.is_empty(), "no torn tail expected: {:?}", db.warnings);
    assert_eq!((r.accepted, r.undecodable, r.spool_files), (5, 0, 1));
    let tags: Vec<String> = all_events(&db).iter().map(|e| e.attr_str("tag").unwrap().to_string()).collect();
    assert_eq!(tags.iter().filter(|t| *t == "a").count(), 3);
    assert_eq!(tags.iter().filter(|t| *t == "c").count(), 2);
    assert!(!tags.iter().any(|t| t == "b"));
}

// ---------------------------------------------------------------------------
// Concurrent spool writers
// ---------------------------------------------------------------------------

#[test]
fn concurrent_spool_writers_produce_exactly_their_events() {
    const WRITERS: usize = 8;
    const EACH: usize = 200;
    let (_dir, root) = temp_root();
    Database::create(&root, DeviceId::new()).unwrap();
    let bin = example_bin("spool_writer");
    let children: Vec<(String, Child)> = (0..WRITERS)
        .map(|w| {
            let tag = format!("w{w}");
            let child = Command::new(&bin)
                .arg(&root)
                .arg(EACH.to_string())
                .arg(&tag)
                .env_remove(failpoint::ENV_ABORT)
                .env_remove(failpoint::ENV_IO)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn spool_writer");
            (tag, child)
        })
        .collect();
    for (tag, child) in children {
        let out = child.wait_with_output().expect("wait spool_writer");
        assert!(out.status.success(), "{tag}: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("DONE {EACH}"), "{tag}");
    }
    let mut db = Database::open(&root, OpenOptions::default()).unwrap();
    let r = db.import_spool().unwrap();
    assert!(db.warnings.is_empty(), "torn spool record: {:?}", db.warnings);
    assert_eq!(r.spool_files, 1);
    assert_eq!(r.undecodable, 0);
    assert_eq!(r.duplicates, 0);
    assert_eq!(r.accepted, WRITERS * EACH);
    let events = all_events(&db);
    assert_eq!(events.len(), WRITERS * EACH);
    let ids: HashSet<EventId> = events.iter().map(|e| e.event_id).collect();
    assert_eq!(ids.len(), WRITERS * EACH, "duplicate event ids");
    let mut per_writer: HashMap<String, Vec<u64>> = HashMap::new();
    for e in &events {
        per_writer
            .entry(e.attr_str("writer").unwrap().to_string())
            .or_default()
            .push(e.attrs["index"].as_u64().unwrap());
    }
    assert_eq!(per_writer.len(), WRITERS);
    for (tag, mut idx) in per_writer {
        idx.sort_unstable();
        assert_eq!(idx, (0..EACH as u64).collect::<Vec<_>>(), "{tag}: indices");
    }
    assert!(!db.stats().spool_pending);
    assert!(db.verify().unwrap().is_empty());
    // A second import finds nothing.
    let again = db.import_spool().unwrap();
    assert_eq!((again.accepted, again.spool_files), (0, 0));
}

// ---------------------------------------------------------------------------
// Truncated / corrupted files
// ---------------------------------------------------------------------------

/// A clean writer run: 7 batches of 20, flush every 2 batches, so three
/// generations of segments exist and the last batch is still in the WAL.
fn clean_run(root: &Path) -> Run {
    let writer = Writer::spawn(root, 20, 2, Some(7), &[]);
    let run = writer.finish(Duration::from_secs(60));
    assert!(run.status.success(), "clean run failed: {}", run.stderr);
    assert_eq!(run.acks.len(), 7);
    assert_eq!(run.flushes, vec![2, 3, 4]);
    run
}

fn flip_byte(path: &Path, offset: usize) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes[offset] ^= 0xff;
    std::fs::write(path, bytes).unwrap();
}

fn newest_manifest(root: &Path) -> PathBuf {
    files_with_extension(&root.join("manifest"), "json").pop().expect("a manifest")
}

#[test]
fn corrupt_newest_manifest_falls_back_with_warnings() {
    let (_dir, root) = temp_root();
    let run = clean_run(&root);
    // Remember what generation 4 referenced before damaging it.
    let before = Database::open(&root, OpenOptions { read_only: true, ..Default::default() }).unwrap();
    assert_eq!(before.manifest().generation, 4);
    let newest_segment = before.manifest().segments.last().unwrap().clone();
    drop(before);
    let newest = newest_manifest(&root);
    let len = std::fs::metadata(&newest).unwrap().len() as usize;
    flip_byte(&newest, len / 2);

    for read_only in [true, false] {
        let db = Database::open(&root, OpenOptions { read_only, ..Default::default() })
            .unwrap_or_else(|e| panic!("open after manifest corruption (read_only={read_only}): {e}"));
        assert_eq!(db.manifest().generation, 3, "previous generation wins");
        assert!(db.warnings.iter().any(|w| w.contains("manifest generation 4 rejected")), "{:?}", db.warnings);
        assert!(
            db.warnings.iter().any(|w| w.starts_with("unreferenced segment file") && w.contains(&newest_segment.file)),
            "the segment only generation 4 named must be reported: {:?}",
            db.warnings
        );
        assert!(db.verify().unwrap().is_empty());
        // Events of generations <= 3 and of the WAL are intact; the ones
        // only the rejected generation's segment held are hidden, never
        // silently: they are the reported file.
        let events = all_events(&db);
        let seqs: HashSet<u64> = events.iter().map(|e| e.source_seq).collect();
        for a in &run.acks {
            let in_hidden_segment = a.first >= newest_segment.min_source_seq && a.last <= newest_segment.max_source_seq;
            assert_eq!(
                seqs.contains(&a.last),
                !in_hidden_segment,
                "ack {}..{} (hidden segment {}..{})",
                a.first,
                a.last,
                newest_segment.min_source_seq,
                newest_segment.max_source_seq
            );
        }
        assert_eq!(events.len(), 140 - newest_segment.rows as usize);
    }
    // Truncated (unparseable) newest manifest: same fallback.
    let (_dir2, root2) = temp_root();
    clean_run(&root2);
    let newest = newest_manifest(&root2);
    let bytes = std::fs::read(&newest).unwrap();
    std::fs::write(&newest, &bytes[..bytes.len() / 3]).unwrap();
    let db = Database::open(&root2, OpenOptions::default()).unwrap();
    assert_eq!(db.manifest().generation, 3);
    assert!(db.warnings.iter().any(|w| w.contains("manifest generation 4 rejected")));
}

#[test]
fn corrupt_every_manifest_is_a_clear_error() {
    let (_dir, root) = temp_root();
    clean_run(&root);
    for path in files_with_extension(&root.join("manifest"), "json") {
        flip_byte(&path, 40);
    }
    match Database::open(&root, OpenOptions::default()) {
        Err(StorageError::Corrupt { what: "manifest", .. }) => {}
        Err(e) => panic!("expected a manifest corruption error, got {e}"),
        Ok(_) => panic!("open must not succeed without a valid generation"),
    }
}

#[test]
fn corrupt_segment_is_reported_by_verify_never_panics() {
    let (_dir, root) = temp_root();
    let run = clean_run(&root);
    let segments = files_with_extension(&root.join("segments"), "arrow");
    assert_eq!(segments.len(), 3);
    // Middle of the newest segment (compressed buffers) ...
    let target = segments.last().unwrap();
    let len = std::fs::metadata(target).unwrap().len() as usize;
    flip_byte(target, len / 2);
    let db = Database::open(&root, OpenOptions::default()).expect("open checks existence, not content");
    assert!(db.warnings.is_empty(), "{:?}", db.warnings);
    let problems = db.verify().unwrap();
    assert!(problems.iter().any(|p| p.contains("sha256 mismatch")), "{problems:?}");
    let file = target.file_name().unwrap().to_str().unwrap();
    assert!(problems.iter().any(|p| p.contains(file)), "{problems:?}");
    // Reading may fail (clear error) or succeed; it must never panic, and
    // the WAL part is still served.
    match db.scan(&ScanFilter::default()) {
        Ok(events) => assert!(events.iter().any(|e| e.source_seq == run.max_acked_seq())),
        Err(e) => assert!(matches!(e, StorageError::Corrupt { what: "segment", .. }), "{e}"),
    }
    drop(db);
    // ... and a truncated segment (footer gone) is a clear corruption error
    // on read, with `verify` naming it.
    let bytes = std::fs::read(target).unwrap();
    std::fs::write(target, &bytes[..bytes.len() / 2]).unwrap();
    let db = Database::open(&root, OpenOptions::default()).unwrap();
    let problems = db.verify().unwrap();
    assert!(problems.iter().any(|p| p.contains(file)), "{problems:?}");
    match db.scan(&ScanFilter::default()) {
        Err(StorageError::Corrupt { what: "segment", path, .. }) => assert!(path.ends_with(file)),
        Err(e) => panic!("expected segment corruption, got {e}"),
        Ok(_) => panic!("a segment without a footer must not decode"),
    }
    // The other generations' segments are fine: a filter that prunes the
    // damaged one still works.
    let first_seg = &db.manifest().segments[0];
    let early = db
        .scan(&ScanFilter { until: Some(first_seg.max_observed_at), ..Default::default() })
        .unwrap();
    assert!(!early.is_empty());
}

#[test]
fn torn_wal_tail_is_reported_and_only_the_torn_records_are_lost() {
    let (_dir, root) = temp_root();
    let run = clean_run(&root);
    let wal_files = files_with_extension(&root.join("wal"), "wal");
    let active = wal_files.last().unwrap().clone();
    let scan = FrameReader::scan(&active, MAGIC_WAL).unwrap();
    assert_eq!(scan.records.len(), 20, "the last batch is in the WAL");
    let full_len = std::fs::metadata(&active).unwrap().len();

    // Truncate inside the last record.
    std::fs::OpenOptions::new().write(true).open(&active).unwrap().set_len(full_len - 5).unwrap();
    let ro = Database::open(&root, OpenOptions { read_only: true, ..Default::default() }).unwrap();
    assert!(ro.warnings.iter().any(|w| w.contains("torn tail")), "{:?}", ro.warnings);
    assert_eq!(all_events(&ro).len(), 139, "exactly the torn record is missing");
    assert_eq!(ro.stats().last_source_seq, 139);
    assert!(!ro.verify().unwrap().is_empty(), "a reader reports the torn tail");
    drop(ro);
    assert_eq!(std::fs::metadata(&active).unwrap().len(), full_len - 5, "a reader never modifies the WAL");

    // The writer truncates the tail, reports it once, and carries on.
    let summary = check_recovered(&root, &run.acks[..6], "torn tail");
    assert_eq!(summary.events, 139);
    assert_eq!(std::fs::metadata(&active).unwrap().len(), scan.records[19].offset);
    continue_writing(&root, summary, "torn tail");
}

#[test]
fn corrupt_wal_record_in_the_middle_drops_it_and_everything_after() {
    let (_dir, root) = temp_root();
    let run = clean_run(&root);
    let wal_files = files_with_extension(&root.join("wal"), "wal");
    let active = wal_files.last().unwrap().clone();
    let scan = FrameReader::scan(&active, MAGIC_WAL).unwrap();
    // Damage the 4th record's payload: records 1-3 survive, 4-20 are the
    // reported tail (the format truncates at the last good record).
    flip_byte(&active, scan.records[3].offset as usize + 12 + 10);
    let db = Database::open(&root, OpenOptions::default()).unwrap();
    assert_eq!(db.warnings.len(), 1, "{:?}", db.warnings);
    assert!(db.warnings[0].contains("torn tail"));
    let s = assert_contents(&db, &run.acks[..6], "corrupt middle record");
    assert_eq!(s.events, 120 + 3);
    assert!(db.verify().unwrap().is_empty(), "writer truncated the bad tail");
    drop(db);
    continue_writing(&root, s, "corrupt middle record");
}

/// A crash between creating the next WAL file (rotation) and writing its
/// header leaves a file shorter than the header. It is what the writer
/// re-initialises, not corruption.
#[test]
fn wal_file_shorter_than_its_header_is_started_over() {
    for len in [0usize, 10] {
        let (_dir, root) = temp_root();
        let run = clean_run(&root);
        let wal_dir = root.join("wal");
        let highest = files_with_extension(&wal_dir, "wal").last().unwrap().clone();
        let number: u64 = highest.file_stem().unwrap().to_str().unwrap().parse().unwrap();
        let short = wal_dir.join(format!("{:06}.wal", number + 1));
        std::fs::write(&short, vec![0u8; len]).unwrap();
        let context = format!("short wal file ({len} bytes)");
        let ro = Database::open(&root, OpenOptions { read_only: true, ..Default::default() }).unwrap();
        assert!(ro.warnings.iter().any(|w| w.contains("torn tail")), "{context}: {:?}", ro.warnings);
        assert_eq!(all_events(&ro).len(), 140, "{context}");
        drop(ro);
        assert_eq!(std::fs::metadata(&short).unwrap().len() as usize, len, "{context}: reader left it alone");
        let summary = check_recovered(&root, &run.acks, &context);
        assert_eq!(summary.events, 140, "{context}");
        assert_eq!(std::fs::metadata(&short).unwrap().len(), 32, "{context}: writer wrote a fresh header");
        continue_writing(&root, summary, &context);
    }
}
