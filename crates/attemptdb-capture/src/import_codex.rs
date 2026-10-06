//! Codex rollout import: reconstruct history from the rollout files Codex
//! keeps on disk.
//!
//! Codex writes every conversation to
//! `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<timestamp>-<thread-uuid>.jsonl`
//! (`~/.codex` when `CODEX_HOME` is unset; finished threads can be moved to
//! `archived_sessions/`). Hosted tools bypass Codex's hooks, so the live
//! database knows of them only through OpenTelemetry; the rollouts are the
//! record of what the agent actually did. The parser lives in
//! `attemptdb-adapters::transcript::codex`; this module finds the files,
//! resolves a project per file from the rollout's own `cwd` and git facts,
//! streams each file through the parser and hands the events to an
//! [`EventSink`] in bounded batches, so a 600 MB rollout is read in constant
//! memory.
//!
//! Everything imported is marked `attrs.reconstructed = true`; ids derive
//! from the rollout's own identifiers, so importing again, or importing a
//! rollout that has grown, only adds what is new.

use crate::agents::AgentKind;
use crate::config::Config;
use crate::git::git_info;
use crate::import::ImportSummary;
use crate::import_common::{Batcher, EventSink};
use crate::{Result, io_at};
use attemptdb_adapters::CaptureContext;
use attemptdb_adapters::transcript::{
    CodexRolloutOptions, RolloutMeta, parse_codex_rollout, peek_rollout_meta,
};
use attemptdb_core::event::ProjectRef;
use attemptdb_core::{DeviceId, SessionId, Timestamp};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

/// Environment variable Codex honours to relocate its home directory.
pub const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// Directory depth searched below a sessions directory (`YYYY/MM/DD/file`
/// needs 3; a little more tolerates Codex reorganising its tree).
const MAX_WALK_DEPTH: usize = 6;

/// Bytes read from the head of a rollout to find its `session_meta` line.
const PEEK_LIMIT: u64 = 4 * 1024 * 1024;

/// Warnings kept in an [`ImportSummary`]; the rest are counted.
const MAX_WARNINGS: usize = 200;

/// A rollout modified this recently belongs to a session that may still be
/// running: it gets no `session_ended` event yet.
const LIVE_WINDOW_MICROS: i64 = 5 * 60 * 1_000_000;

/// One rollout file to import.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RolloutSource {
    pub path: PathBuf,
    pub modified_at: Option<Timestamp>,
    pub bytes: u64,
}

impl RolloutSource {
    /// Describe a file on disk (size and mtime are best effort).
    pub fn from_path(path: &Path) -> Self {
        let meta = std::fs::metadata(path).ok();
        Self {
            path: path.to_path_buf(),
            modified_at: meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| Timestamp::from_micros(d.as_micros() as i64)),
            bytes: meta.map(|m| m.len()).unwrap_or(0),
        }
    }

    /// The thread uuid at the end of `rollout-<timestamp>-<uuid>.jsonl`, or
    /// the whole file stem when it does not end in one.
    pub fn thread_hint(&self) -> Option<String> {
        let stem = self.path.file_stem()?.to_string_lossy().into_owned();
        if stem.len() >= 36 {
            let tail = &stem[stem.len() - 36..];
            if is_uuid(tail) {
                return Some(tail.to_string());
            }
        }
        Some(stem)
    }
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Directories that may hold Codex rollouts: `sessions/` and
/// `archived_sessions/` under `$CODEX_HOME` (default `~/.codex`). Only
/// existing directories are returned.
pub fn codex_session_dirs() -> Vec<PathBuf> {
    let Some(home) = AgentKind::Codex.agent_dir() else {
        return Vec::new();
    };
    ["sessions", "archived_sessions"]
        .iter()
        .map(|d| home.join(d))
        .filter(|d| d.is_dir())
        .collect()
}

/// Every rollout (`rollout-*.jsonl`) below the given directories.
pub fn discover_rollouts(dirs: &[PathBuf]) -> Vec<RolloutSource> {
    let mut out = Vec::new();
    for dir in dirs {
        walk(dir, 0, true, &mut out);
    }
    finish(&mut out);
    out
}

/// Rollouts at or below an explicit path: the file itself when it is a
/// `.jsonl`, otherwise every `.jsonl` found under the directory.
pub fn collect_rollouts(path: &Path) -> Vec<RolloutSource> {
    let mut out = Vec::new();
    if path.is_file() {
        if is_jsonl(path) {
            out.push(RolloutSource::from_path(path));
        }
    } else if path.is_dir() {
        walk(path, 0, false, &mut out);
    }
    finish(&mut out);
    out
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
}

fn is_rollout_name(path: &Path) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy())
        .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
}

fn walk(dir: &Path, depth: usize, rollouts_only: bool, out: &mut Vec<RolloutSource>) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            walk(&path, depth + 1, rollouts_only, out);
        } else if is_jsonl(&path) && (!rollouts_only || is_rollout_name(&path)) {
            out.push(RolloutSource::from_path(&path));
        }
    }
}

/// Newest first (so a bounded run keeps the recent history), duplicates
/// removed.
fn finish(sources: &mut Vec<RolloutSource>) {
    sort_newest_first(sources);
    sources.dedup_by(|a, b| a.path == b.path);
}

/// Order by modification time, newest first; ties by path.
pub fn sort_newest_first(sources: &mut [RolloutSource]) {
    sources.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| b.path.cmp(&a.path))
    });
}

// ---------------------------------------------------------------------------
// Planning (no parsing)
// ---------------------------------------------------------------------------

/// What a set of rollouts holds, from file metadata and each file's first
/// line only.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct RolloutCounts {
    pub files: usize,
    /// Distinct sessions: a subagent thread belongs to its parent's session.
    pub sessions: usize,
    pub bytes: u64,
}

/// Count rollouts without parsing them: the sizes come from the file
/// system, the session of each file from its first line.
pub fn count_rollouts(sources: &[RolloutSource]) -> RolloutCounts {
    let mut sessions: HashSet<String> = HashSet::new();
    for s in sources {
        let id = peek_meta(&s.path)
            .ok()
            .flatten()
            .and_then(|m| m.session_id)
            .or_else(|| s.thread_hint());
        if let Some(id) = id {
            sessions.insert(id);
        }
    }
    RolloutCounts {
        files: sources.len(),
        sessions: sessions.len(),
        bytes: sources.iter().map(|s| s.bytes).sum(),
    }
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// Import rollouts into `sink`, in the order given (callers sort newest
/// first). Each file gets its own capture context whose project comes from
/// the rollout's `cwd` and git facts. A file that cannot be read is
/// reported and skipped; an error from the sink (a full disk, a lock lost
/// mid-run) stops the import.
pub fn import_codex_rollouts(
    sink: &mut dyn EventSink,
    sources: &[RolloutSource],
    config: &Config,
    device: DeviceId,
) -> Result<ImportSummary> {
    let mut summary = ImportSummary::default();
    let mut suppressed = 0usize;
    let mut sessions: HashSet<SessionId> = HashSet::new();
    let now = Timestamp::now();
    let mut warn = |summary: &mut ImportSummary, message: String| {
        if summary.warnings.len() < MAX_WARNINGS {
            summary.warnings.push(message);
        } else {
            suppressed += 1;
        }
    };

    for source in sources {
        summary.files += 1;
        let label = source
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| source.path.display().to_string());
        let meta = match peek_meta(&source.path) {
            Ok(m) => m,
            Err(e) => {
                summary.files_failed += 1;
                warn(&mut summary, format!("{label}: cannot read: {e}"));
                continue;
            }
        };
        let file = match File::open(&source.path) {
            Ok(f) => f,
            Err(e) => {
                summary.files_failed += 1;
                warn(&mut summary, format!("{label}: cannot open: {e}"));
                continue;
            }
        };
        let (project, project_warning) = project_for(meta.as_ref(), source, &device);
        if let Some(w) = project_warning {
            warn(&mut summary, format!("{label}: {w}"));
        }
        let ctx = CaptureContext {
            device_id: device,
            capture_mode: config.capture_mode,
            project,
            captured_at: Timestamp::now(),
            provider_version: None,
            hook_version: None,
        };
        let mut opts = CodexRolloutOptions::for_capture_mode(config.capture_mode);
        opts.session_id_hint = source.thread_hint();
        opts.emit_session_end = source
            .modified_at
            .is_none_or(|m| now.as_micros() - m.as_micros() > LIVE_WINDOW_MICROS);

        let mut batcher = Batcher::new(sink);
        let parsed = parse_codex_rollout(
            BufReader::with_capacity(256 * 1024, file),
            &ctx,
            &opts,
            |ev| {
                sessions.insert(ev.session_id);
                batcher.push(ev)
            },
        )?;
        batcher.flush()?;
        summary.accepted += batcher.total.accepted;
        summary.duplicates += batcher.total.duplicates;
        summary.queued += batcher.total.queued;
        summary.events_seen += parsed.events;
        summary.lines_skipped += parsed.stats.lines_skipped();
        summary.bytes += parsed.stats.bytes;
        for w in parsed.warnings {
            warn(&mut summary, format!("{label}: {w}"));
        }
    }
    sink.finish()?;
    summary.sessions = sessions.len();
    if suppressed > 0 {
        summary
            .warnings
            .push(format!("{suppressed} further warning(s) suppressed"));
    }
    Ok(summary)
}

/// The `session_meta` facts from the first line of a rollout.
fn peek_meta(path: &Path) -> Result<Option<RolloutMeta>> {
    let file = File::open(path).map_err(|e| io_at(path, e))?;
    let mut reader = BufReader::new(file.take(PEEK_LIMIT));
    let mut line = Vec::new();
    reader
        .read_until(b'\n', &mut line)
        .map_err(|e| io_at(path, e))?;
    Ok(peek_rollout_meta(&line))
}

/// The project a rollout belongs to. Identity comes from the repository
/// containing the rollout's `cwd` when that directory still exists, so
/// reconstructed and hook-captured events of one repository share a project
/// id. When the directory is gone the rollout's own git facts stand in:
/// `session_meta.git.repository_url` is the same remote a hook would have
/// read, and a remote is what project identity is made of, so those events
/// still land in the right project.
///
/// Known inconsistency (shared with the Claude importer): a repository
/// without a remote, whose checkout no longer exists, falls back to the
/// `cwd` text. That is the repository root only when the session started at
/// the root; a session that started in a subdirectory gets a project of its
/// own, different from the one hooks would have derived from the root.
///
/// The branch is the one recorded in the rollout (the branch *at the time*)
/// and `head` is left unknown for the same reason.
fn project_for(
    meta: Option<&RolloutMeta>,
    source: &RolloutSource,
    device: &DeviceId,
) -> (ProjectRef, Option<String>) {
    match meta.and_then(|m| m.cwd.as_deref()) {
        Some(cwd) => {
            let meta = meta.expect("a cwd came from the meta");
            let cwd_path = Path::new(cwd);
            let git = if cwd_path.is_dir() {
                git_info(cwd_path)
            } else {
                None
            };
            let mut project = match &git {
                Some(g) => ProjectRef::derive(
                    &g.root.to_string_lossy(),
                    g.remote.as_deref().or(meta.git_remote.as_deref()),
                    device,
                ),
                None => ProjectRef::derive(cwd, meta.git_remote.as_deref(), device),
            };
            project.branch = meta
                .git_branch
                .clone()
                .or_else(|| git.as_ref().and_then(|g| g.branch.clone()));
            (project, None)
        }
        None => {
            let root = source
                .path
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_string());
            (
                ProjectRef::derive(&root, None, device),
                Some(
                    "no `cwd` in the rollout's session_meta; project derived from its directory"
                        .to_string(),
                ),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import_common::{DbSink, ImportTarget, SpoolSink};
    use crate::locator::Locator;
    use attemptdb_core::{CaptureMode, EventKind};
    use attemptdb_storage::{Database, OpenOptions, ScanFilter};
    use serde_json::Value;
    use std::time::Duration;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/transcripts/codex")
            .join(format!("{name}.jsonl"))
    }

    fn open_db(root: &Path) -> (Database, DeviceId) {
        let device = DeviceId::derive(&["import-codex-tests"]);
        let dir = root.join(".attemptdb");
        Database::create(&dir, device).unwrap();
        let db = Database::open(
            &dir,
            OpenOptions {
                create: false,
                ..Default::default()
            },
        )
        .unwrap();
        (db, device)
    }

    /// A tree shaped like `~/.codex/sessions`.
    fn sessions_tree(root: &Path) -> PathBuf {
        let day = root.join("sessions/2026/08/28");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::copy(
            fixture("modern_turn"),
            day.join("rollout-2026-08-28T08-00-00-22222222-2222-4222-8222-222222222222.jsonl"),
        )
        .unwrap();
        std::fs::copy(
            fixture("classic_turn"),
            day.join("rollout-2026-08-28T09-00-00-33333333-3333-4333-8333-333333333333.jsonl"),
        )
        .unwrap();
        std::fs::write(day.join("notes.jsonl"), "{}\n").unwrap();
        std::fs::write(day.join("rollout-readme.txt"), "x").unwrap();
        root.join("sessions")
    }

    #[test]
    fn discovery_finds_rollouts_and_counts_them_without_parsing() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = sessions_tree(tmp.path());
        let found = discover_rollouts(std::slice::from_ref(&sessions));
        assert_eq!(found.len(), 2, "only rollout-*.jsonl: {found:?}");
        assert!(found.iter().all(|s| s.bytes > 0 && s.modified_at.is_some()));
        assert!(found[0].modified_at >= found[1].modified_at, "newest first");
        let counts = count_rollouts(&found);
        assert_eq!(counts.files, 2);
        assert_eq!(counts.sessions, 2);
        assert_eq!(counts.bytes, found.iter().map(|s| s.bytes).sum::<u64>());

        // An explicit path takes any .jsonl.
        assert_eq!(collect_rollouts(&sessions).len(), 3);
        assert_eq!(collect_rollouts(&fixture("modern_turn")).len(), 1);
        assert!(collect_rollouts(&tmp.path().join("missing")).is_empty());
        assert_eq!(
            RolloutSource::from_path(Path::new(
                "/x/rollout-2026-08-28T08-00-00-22222222-2222-4222-8222-222222222222.jsonl"
            ))
            .thread_hint()
            .as_deref(),
            Some("22222222-2222-4222-8222-222222222222")
        );
        assert_eq!(
            RolloutSource::from_path(Path::new("/x/odd.jsonl"))
                .thread_hint()
                .as_deref(),
            Some("odd")
        );
    }

    #[test]
    fn import_is_idempotent_and_marks_events_reconstructed() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut db, device) = open_db(tmp.path());
        let config = Config::default();
        let sources = discover_rollouts(&[sessions_tree(tmp.path())]);

        let first =
            import_codex_rollouts(&mut DbSink::new(&mut db), &sources, &config, device).unwrap();
        assert_eq!(first.files, 2);
        assert_eq!(first.files_failed, 0);
        assert_eq!(first.sessions, 2);
        assert_eq!(first.accepted, first.events_seen);
        assert_eq!((first.duplicates, first.queued), (0, 0));
        assert!(first.warnings.is_empty(), "{:?}", first.warnings);
        assert!(first.bytes > 0);
        assert!(first.accepted > 40, "{first:?}");

        let second =
            import_codex_rollouts(&mut DbSink::new(&mut db), &sources, &config, device).unwrap();
        assert_eq!(second.accepted, 0, "a second import stores nothing");
        assert_eq!(second.duplicates, first.accepted);

        let events = db.scan(&ScanFilter::default()).unwrap();
        assert_eq!(events.len(), first.accepted);
        assert!(
            events
                .iter()
                .all(|e| e.attrs.get("reconstructed") == Some(&Value::Bool(true)))
        );
        assert!(
            events
                .iter()
                .all(|e| e.hook_version.is_none() && e.raw.is_none() && e.is_ingested())
        );
        // The checkout does not exist here: the rollout's remote still gives
        // the project the identity a hook would have computed.
        let want = ProjectRef::derive(
            "/home/dev/example/project",
            Some("git@github.com:example/project.git"),
            &device,
        );
        assert!(
            events
                .iter()
                .all(|e| e.project.project_id == want.project_id),
            "projects: {:?}",
            events
                .iter()
                .map(|e| e.project.project_id)
                .collect::<HashSet<_>>()
        );
        assert_eq!(db.stats().memtable_rows, 0, "flushed at the end");
    }

    #[test]
    fn a_grown_rollout_adds_only_the_new_events() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut db, device) = open_db(tmp.path());
        let config = Config::default();
        let text = std::fs::read_to_string(fixture("classic_turn")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let path = tmp
            .path()
            .join("rollout-2026-08-28T09-00-00-33333333-3333-4333-8333-333333333333.jsonl");
        std::fs::write(&path, format!("{}\n", lines[..20].join("\n"))).unwrap();
        let sources = collect_rollouts(&path);
        let first =
            import_codex_rollouts(&mut DbSink::new(&mut db), &sources, &config, device).unwrap();
        assert!(first.accepted > 10);

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        writeln!(f, "{}", lines[20..].join("\n")).unwrap();
        drop(f);
        let third = import_codex_rollouts(
            &mut DbSink::new(&mut db),
            &collect_rollouts(&path),
            &config,
            device,
        )
        .unwrap();
        // Everything seen before (bar the end-of-session marker, which moved)
        // is a duplicate; only the new tail is stored.
        assert!(third.accepted > 0);
        assert!(
            third.duplicates >= first.accepted - 1,
            "{first:?} {third:?}"
        );
        let events = db.scan(&ScanFilter::default()).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == EventKind::PromptSubmitted)
                .count(),
            1
        );
    }

    #[test]
    fn a_rollout_written_in_the_last_minutes_is_a_session_still_going() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut db, device) = open_db(tmp.path());
        // Just written (a copy may keep the old mtime): its mtime is now.
        let path = tmp
            .path()
            .join("rollout-2026-08-28T09-00-00-33333333-3333-4333-8333-333333333333.jsonl");
        std::fs::write(&path, std::fs::read(fixture("classic_turn")).unwrap()).unwrap();
        import_codex_rollouts(
            &mut DbSink::new(&mut db),
            &collect_rollouts(&path),
            &Config::default(),
            device,
        )
        .unwrap();
        let events = db.scan(&ScanFilter::default()).unwrap();
        assert!(events.iter().all(|e| e.kind != EventKind::SessionEnded));
        assert!(events.iter().any(|e| e.kind == EventKind::SessionStarted));
    }

    #[test]
    fn metadata_only_import_stores_no_content() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut db, device) = open_db(tmp.path());
        let config = Config {
            capture_mode: CaptureMode::MetadataOnly,
            ..Config::default()
        };
        let sources = discover_rollouts(&[sessions_tree(tmp.path())]);
        let summary =
            import_codex_rollouts(&mut DbSink::new(&mut db), &sources, &config, device).unwrap();
        assert!(summary.accepted > 40);
        let events = db.scan(&ScanFilter::default()).unwrap();
        let serialised = serde_json::to_string(&events).unwrap();
        assert!(!serialised.contains("CANARY_"));
        assert!(
            events
                .iter()
                .all(|e| e.content.is_none() && e.raw.is_none())
        );
    }

    #[test]
    fn unreadable_and_odd_sources_are_reported_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut db, device) = open_db(tmp.path());
        let missing = RolloutSource {
            path: tmp.path().join("rollout-missing.jsonl"),
            modified_at: None,
            bytes: 0,
        };
        let no_meta = tmp.path().join("rollout-no-meta.jsonl");
        std::fs::write(
            &no_meta,
            "{\"timestamp\":\"2026-08-28T08:00:01.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hi\"}}\n\u{FF}not json\n",
        )
        .unwrap();
        let sources = vec![missing, RolloutSource::from_path(&no_meta)];
        let summary = import_codex_rollouts(
            &mut DbSink::new(&mut db),
            &sources,
            &Config::default(),
            device,
        )
        .unwrap();
        assert_eq!((summary.files, summary.files_failed), (2, 1));
        assert_eq!(
            summary.accepted, 2,
            "started and the prompt; the file is live"
        );
        assert_eq!(summary.lines_skipped, 1);
        assert!(summary.warnings.iter().any(|w| w.contains("cannot read")));
        assert!(summary.warnings.iter().any(|w| w.contains("no `cwd`")));
        assert!(summary.warnings.iter().any(|w| w.contains("invalid JSON")));
    }

    /// The spool a daemon would drain: the events land there, nothing is
    /// stored, and importing the spool gives the same result as importing
    /// directly.
    #[test]
    fn a_locked_database_gets_its_events_through_the_spool() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&data).unwrap();
        let device = DeviceId::derive(&["import-codex-spool"]);
        Database::create(&db_dir, device).unwrap();
        let locator = Locator::resolve(tmp.path(), Some(&data), Some(&db_dir));
        let sources = discover_rollouts(&[sessions_tree(tmp.path())]);

        // The "daemon": a writer holding the lock.
        let mut holder = crate::ingest::open_writer(&locator, false).unwrap();
        let target = crate::import_common::open_import_target(&locator).unwrap();
        assert!(
            target.is_spool(),
            "the lock is held: fall back to the spool"
        );
        let ImportTarget::Spool(mut spool) = target else {
            panic!("spool expected")
        };
        let queued =
            import_codex_rollouts(&mut spool, &sources, &Config::default(), device).unwrap();
        assert_eq!((queued.accepted, queued.duplicates), (0, 0));
        assert!(queued.queued > 40, "{queued:?}");
        assert_eq!(queued.queued, queued.events_seen);
        assert!(holder.stats().spool_pending, "the daemon has work");
        assert_eq!(holder.stats().memtable_rows, 0, "nothing was stored yet");

        // The daemon's next tick.
        let report = holder.import_spool().unwrap();
        assert_eq!(report.accepted, queued.queued);
        // Importing again (setup re-run) queues the same events; the daemon
        // reports them as duplicates.
        let again =
            import_codex_rollouts(&mut spool, &sources, &Config::default(), device).unwrap();
        assert_eq!(again.queued, queued.queued);
        let report = holder.import_spool().unwrap();
        assert_eq!((report.accepted, report.duplicates), (0, queued.queued));
        drop(holder);

        // With the lock free the same call writes directly.
        let target = crate::import_common::open_import_target(&locator).unwrap();
        assert!(!target.is_spool());
    }

    #[test]
    fn a_spool_waits_for_a_busy_inbox_to_drain_but_not_forever() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        let device = DeviceId::derive(&["import-codex-pace"]);
        Database::create(&db_dir, device).unwrap();
        let locator = Locator::resolve(tmp.path(), Some(&tmp.path().join("data")), Some(&db_dir));
        let sources = discover_rollouts(&[sessions_tree(tmp.path())]);
        // A one-byte high-water mark: every append waits; nobody drains, so
        // the first wait times out and the sink stops waiting.
        let mut spool = SpoolSink::with_limits(&locator, 1, Duration::from_millis(150)).unwrap();
        let started = std::time::Instant::now();
        let summary =
            import_codex_rollouts(&mut spool, &sources, &Config::default(), device).unwrap();
        assert!(summary.queued > 40);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited once, then gave up: {:?}",
            started.elapsed()
        );
    }
}
