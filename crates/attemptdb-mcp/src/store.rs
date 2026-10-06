//! Opening the database on demand and keeping a few query engines warm until
//! the files underneath them change.
//!
//! Every tool call asks for a [`View`] for a scope. The store computes a
//! cheap filesystem fingerprint of the database (identity file, manifest
//! generations, WAL files, spool files); when it matches a cached engine's
//! fingerprint and the scope is the same, the engine is reused (the last
//! [`VIEW_SLOTS`] scopes are kept, so alternating between two projects does
//! not rebuild each time). Otherwise a fresh engine is built: the spool is
//! imported when the writer lock is free (the lock is let go at once, not
//! held for the load), the fingerprint is taken, and only then is the
//! database read — so an event written while a long load runs changes the
//! fingerprint the cached view was built under, and the next call sees it.
//! A scope reads its own rows; facts for scope resolution and the status
//! come from a few columns of every segment.

use crate::ServerConfig;
use crate::args::{opt_bool, opt_string};
use crate::text::{id, ts};
use anyhow::{Context, Result, anyhow, bail};
use attemptdb_capture::{Config, Locator, ingest};
use attemptdb_core::event::normalise_remote;
use attemptdb_core::{CaptureMode, PortablePath, ProjectId, SessionId, Timestamp};
use attemptdb_query::{EngineCache, QueryEngine, StreamFacts, TimeExpr};
use attemptdb_storage::format::{IDENTITY_FILE, MANIFEST_DIR, SPOOL_DIR, WAL_DIR};
use attemptdb_storage::{Database, IngestReport, ScanFilter, snapshot};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tokio::runtime::Runtime;

/// Default cap on rows/lines a single tool result may carry.
pub const DEFAULT_MAX_ROWS: usize = 200;

/// Scope arguments shared by most tools, exactly as the caller passed them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopeArgs {
    pub project: Option<String>,
    pub all_projects: bool,
    pub session: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub captured_only: bool,
}

impl ScopeArgs {
    pub fn from_json(args: &Map<String, Value>) -> std::result::Result<Self, String> {
        Ok(Self {
            project: opt_string(args, "project")?,
            all_projects: opt_bool(args, "all_projects")?.unwrap_or(false),
            session: opt_string(args, "session")?,
            since: opt_string(args, "since")?,
            until: opt_string(args, "until")?,
            captured_only: opt_bool(args, "captured_only")?.unwrap_or(false),
        })
    }
}

/// Parse a time argument the way the CLI does: RFC 3339, `YYYY-MM-DD`,
/// epoch, `now`, `today`, `yesterday`, or `-<n>(s|m|h|d|w)`.
pub fn parse_time(spec: &str) -> Option<Timestamp> {
    TimeExpr::parse_literal(spec).map(|e| e.resolve(Timestamp::now()))
}

fn time_arg(spec: &Option<String>, what: &str) -> Result<Option<Timestamp>> {
    match spec {
        None => Ok(None),
        Some(s) => parse_time(s).map(Some).ok_or_else(|| {
            anyhow::anyhow!(
                "cannot parse {what} {s:?}: use RFC 3339, YYYY-MM-DD, now, today, yesterday or -<n>(s|m|h|d|w)"
            )
        }),
    }
}

/// The scope as the caller wrote it; the cache key together with the
/// fingerprint. Times stay as written: `-400d` is the same scope on the next
/// call, and resolving it to an instant would make every call a different
/// key (and a full reload). The instant is resolved when a view is built and
/// shows in its label; a view is rebuilt whenever the database changes, so
/// a relative window slides with it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ScopeKey {
    project: Option<String>,
    all_projects: bool,
    session: Option<String>,
    since: Option<String>,
    until: Option<String>,
    captured_only: bool,
}

impl ScopeKey {
    fn from_args(args: &ScopeArgs) -> Result<Self> {
        // Reject an unreadable time now, with the caller's own words.
        time_arg(&args.since, "since")?;
        time_arg(&args.until, "until")?;
        let expr = |s: &Option<String>| s.as_ref().map(|s| s.trim().to_string());
        Ok(Self {
            project: args.project.clone(),
            all_projects: args.all_projects,
            session: args.session.clone(),
            since: expr(&args.since),
            until: expr(&args.until),
            captured_only: args.captured_only,
        })
    }

    /// The window's instants, resolved against now.
    fn times(&self) -> Result<(Option<Timestamp>, Option<Timestamp>)> {
        Ok((
            time_arg(&self.since, "since")?,
            time_arg(&self.until, "until")?,
        ))
    }
}

/// What the loaded engine covers.
#[derive(Clone, Debug)]
pub struct ScopeInfo {
    /// Human label, e.g. `project acme/repo (prj_…) · since 2026-08-28T00:00:00Z`.
    pub label: String,
    /// Why the project scope is what it is when the caller did not choose it.
    pub default_reason: Option<String>,
    pub project_id: Option<ProjectId>,
    pub project_name: Option<String>,
    pub session_id: Option<SessionId>,
    pub since: Option<Timestamp>,
    pub until: Option<Timestamp>,
    pub captured_only: bool,
}

impl ScopeInfo {
    fn filter(&self) -> ScanFilter {
        ScanFilter {
            project_id: self.project_id,
            session_id: self.session_id,
            since: self.since,
            until: self.until,
            captured_only: self.captured_only,
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProviderStat {
    pub provider: String,
    pub events: u64,
    /// Latest `observed_at`, capture tests excluded.
    pub last_event_at: Option<Timestamp>,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectStat {
    pub project_id: Option<ProjectId>,
    pub name: String,
    pub events: u64,
    pub sessions: u64,
}

/// Database-wide facts gathered when the engine was (re)built.
#[derive(Clone, Debug, Default)]
pub struct DbStatus {
    pub source: String,
    pub read_only: bool,
    pub snapshot: bool,
    pub capture_mode: CaptureMode,
    pub generation: u64,
    pub segments: usize,
    pub segment_rows: u64,
    pub memtable_rows: usize,
    pub wal_bytes: u64,
    pub spool_pending: bool,
    pub events: usize,
    pub sessions: usize,
    pub captured_events: usize,
    pub reconstructed_events: usize,
    pub last_event_at: Option<Timestamp>,
    pub providers: Vec<ProviderStat>,
    pub projects: Vec<ProjectStat>,
    pub import: Option<IngestReport>,
    pub warnings: Vec<String>,
    pub loaded_at: Timestamp,
}

/// Hook-captured versus transcript-reconstructed events of one session.
#[derive(Clone, Copy, Debug, Default)]
pub struct CaptureCounts {
    pub captured: usize,
    pub reconstructed: usize,
}

/// A query engine over one scope plus the facts around it.
pub struct View {
    pub engine: QueryEngine,
    pub scope: ScopeInfo,
    pub status: DbStatus,
    pub session_capture: HashMap<SessionId, CaptureCounts>,
}

/// Everything a tool needs for one call: the view, a runtime to drive the
/// async query engine, and the locator for daemon probes.
pub struct Ready<'a> {
    pub view: &'a View,
    pub locator: &'a Locator,
    pub config: &'a ServerConfig,
    rt: &'a Runtime,
}

impl Ready<'_> {
    pub fn block_on<F: Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }
}

/// Files that change whenever the database content changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint(Vec<(String, u64, u128)>);

struct Cached {
    fingerprint: Fingerprint,
    key: ScopeKey,
    view: View,
}

/// How many scopes stay warm: the current one and the one before it (or two).
pub const VIEW_SLOTS: usize = 3;

struct Opened {
    db: Database,
    import: Option<IngestReport>,
    read_only: bool,
    snapshot: bool,
    source: String,
    /// Taken after the spool import and before the database was read.
    fingerprint: Fingerprint,
}

pub struct Store {
    config: ServerConfig,
    locator: Locator,
    rt: Runtime,
    /// Most recently used first; every entry was built from the same files
    /// (entries from older states are dropped on the next call).
    views: Vec<Cached>,
    engine_cache: EngineCache,
    /// Runs after the database is opened and fingerprinted, before it is
    /// read: tests write an event here to model a slow load.
    #[cfg(test)]
    load_hook: Option<Box<dyn FnMut()>>,
}

impl Store {
    pub fn new(config: ServerConfig) -> Result<Self> {
        let cwd = config
            .project_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let locator = Locator::resolve(&cwd, config.data_dir.as_deref(), Some(&config.db_dir));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building the tokio runtime")?;
        Ok(Self {
            config,
            locator,
            rt,
            views: Vec::new(),
            engine_cache: EngineCache::new(),
            #[cfg(test)]
            load_hook: None,
        })
    }

    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Drop the cached engines; the next call re-opens the database.
    pub fn invalidate(&mut self) {
        self.views.clear();
    }

    /// A view for `scope`, reusing a cached engine when neither the
    /// database files nor the scope changed.
    pub fn view(&mut self, scope: &ScopeArgs) -> Result<Ready<'_>> {
        let key = ScopeKey::from_args(scope)?;
        let fresh = self.fingerprint();
        // A view built from older files can never be served again.
        self.views.retain(|c| c.fingerprint == fresh);
        match self.views.iter().position(|c| c.key == key) {
            Some(0) => {}
            Some(i) => {
                let hit = self.views.remove(i);
                self.views.insert(0, hit);
            }
            None => {
                let loaded = self.load(key)?;
                self.views.insert(0, loaded);
                self.views.truncate(VIEW_SLOTS);
            }
        }
        let cached = &self.views[0];
        Ok(Ready {
            view: &cached.view,
            locator: &self.locator,
            config: &self.config,
            rt: &self.rt,
        })
    }

    /// Cheap staleness probe: sizes and mtimes of every file that ingestion
    /// or a flush touches (or the snapshot file itself).
    pub fn fingerprint(&self) -> Fingerprint {
        let mut entries = Vec::new();
        if let Some(file) = &self.config.snapshot {
            push_meta(&mut entries, "snapshot", file);
            return Fingerprint(entries);
        }
        let root = &self.config.db_dir;
        push_meta(&mut entries, IDENTITY_FILE, &root.join(IDENTITY_FILE));
        for sub in [MANIFEST_DIR, WAL_DIR, SPOOL_DIR] {
            let Ok(rd) = std::fs::read_dir(root.join(sub)) else {
                continue;
            };
            for entry in rd.flatten() {
                let name = format!("{sub}/{}", entry.file_name().to_string_lossy());
                if let Ok(meta) = entry.metadata() {
                    entries.push((name, meta.len(), mtime_nanos(&meta)));
                }
            }
        }
        entries.sort();
        Fingerprint(entries)
    }

    /// Open the database for a read. The spool is imported when the writer
    /// lock is free and the lock is let go; the fingerprint is taken after
    /// that and before the read-only handle is opened, so whatever lands later
    /// changes it (and is picked up by the next call), and whatever landed
    /// earlier is in the handle.
    fn open(&self) -> Result<Opened> {
        if let Some(file) = &self.config.snapshot {
            let fingerprint = self.fingerprint();
            let (db, dir) = snapshot::open_read_only(file, &self.locator.snapshot_cache_dir())
                .with_context(|| format!("opening snapshot {}", file.display()))?;
            return Ok(Opened {
                db,
                import: None,
                read_only: true,
                snapshot: true,
                source: format!("snapshot {} (cached at {})", file.display(), dir.display()),
                fingerprint,
            });
        }
        if !Database::exists(&self.config.db_dir) {
            bail!(
                "no database at {} — run `attempt init` (or `attempt init --local` inside the project) and install hooks with `attempt hook install`",
                self.config.db_dir.display()
            );
        }
        let pending = ingest::import_pending(&self.locator)
            .with_context(|| format!("opening {}", self.config.db_dir.display()))?;
        let fingerprint = self.fingerprint();
        let db = ingest::open_reader(&self.locator)
            .with_context(|| format!("opening {}", self.config.db_dir.display()))?;
        Ok(Opened {
            db,
            import: pending.report,
            read_only: pending.writer_busy,
            snapshot: false,
            source: self.config.db_dir.display().to_string(),
            fingerprint,
        })
    }

    fn load(&mut self, key: ScopeKey) -> Result<Cached> {
        let opened = self.open()?;
        #[cfg(test)]
        if let Some(hook) = self.load_hook.as_mut() {
            hook();
        }
        let mut refreshed = self
            .engine_cache
            .refresh_lazy(&opened.db, &opened.source)
            .context("listing the database's segments")?;
        // A segment a compaction deletes mid-read is not an error: the
        // listing is renewed from a fresh manifest and the read repeated.
        let locator = (!opened.snapshot).then(|| self.locator.clone());
        let mut reopen = move || match &locator {
            Some(l) => ingest::open_reader(l).map_err(|e| {
                attemptdb_query::QueryError::Exec(format!("reopening the database: {e}"))
            }),
            None => Err(attemptdb_query::QueryError::Exec(
                "a snapshot cannot change underneath a read".into(),
            )),
        };
        let facts = self
            .engine_cache
            .retrying(&mut refreshed, &mut reopen, |c, r| c.facts(r))
            .context("reading the database's facts")?;
        let scope = self.resolve_scope(&key, &facts)?;
        let filter = scope.filter();
        let engine = self
            .engine_cache
            .retrying(&mut refreshed, &mut reopen, |c, r| {
                c.engine_scoped(r, &filter)
            })
            .context("building the query engine")?;
        let stats = opened.db.stats();
        let mut status = summarize(&facts);
        status.source = opened.source.clone();
        status.read_only = opened.read_only;
        status.snapshot = opened.snapshot;
        status.capture_mode = Config::load_or_default(&self.locator.paths.config_dir).capture_mode;
        status.generation = stats.generation;
        status.segments = stats.segments;
        status.segment_rows = stats.segment_rows;
        status.memtable_rows = stats.memtable_rows;
        status.wal_bytes = stats.wal_bytes;
        status.spool_pending = stats.spool_pending;
        status.import = opened.import.clone();
        status.warnings = opened.db.warnings.clone();
        status.loaded_at = Timestamp::now();
        let session_capture = capture_counts(&facts);
        Ok(Cached {
            // Taken before the read: an event written during it makes the
            // next call's fingerprint differ from this one.
            fingerprint: opened.fingerprint,
            key,
            view: View {
                engine,
                scope,
                status,
                session_capture,
            },
        })
    }

    fn resolve_scope(&self, key: &ScopeKey, all: &StreamFacts) -> Result<ScopeInfo> {
        let (project_id, default_reason) = if let Some(spec) = &key.project {
            (Some(resolve_project(all, spec)?), None)
        } else if key.all_projects {
            (None, None)
        } else if let Some(root) = &self.config.project_root {
            match current_project(all, root) {
                Some(id) => (
                    Some(id),
                    Some(format!(
                        "default scope is the repository at {}; pass all_projects=true or project=<name> to change it",
                        root.display()
                    )),
                ),
                None => (
                    None,
                    Some(format!(
                        "default scope is all projects: no events are recorded for the repository at {}",
                        root.display()
                    )),
                ),
            }
        } else {
            (None, Some("default scope is all projects".to_string()))
        };
        let (since, until) = key.times()?;
        let project_name =
            project_id.and_then(|pid| all.projects.get(&pid).map(|p| p.name.clone()));
        let session_id = match &key.session {
            Some(spec) => Some(resolve_session(all, spec)?),
            None => None,
        };
        let mut parts = vec![match (project_id, &project_name) {
            (Some(pid), Some(name)) => format!("project {name} ({})", id(&pid)),
            (Some(pid), None) => format!("project {}", id(&pid)),
            (None, _) => "all projects".to_string(),
        }];
        if let Some(sid) = session_id {
            parts.push(format!("session {}", id(&sid)));
        }
        if let Some(t) = since {
            // The window drops events before projecting, so a session that
            // began earlier is reported from where the window starts.
            parts.push(format!(
                "since {} (events before it are left out; a session that began earlier is partial)",
                ts(t)
            ));
        }
        if let Some(t) = until {
            parts.push(format!("until {}", ts(t)));
        }
        if key.captured_only {
            parts.push("hook-captured events only".to_string());
        }
        Ok(ScopeInfo {
            label: parts.join(" · "),
            default_reason,
            project_id,
            project_name,
            session_id,
            since,
            until,
            captured_only: key.captured_only,
        })
    }
}

fn mtime_nanos(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn push_meta(entries: &mut Vec<(String, u64, u128)>, name: &str, path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        entries.push((name.to_string(), meta.len(), mtime_nanos(&meta)));
    }
}

fn summarize(f: &StreamFacts) -> DbStatus {
    // Projects are listed by name (two ids sharing a name merge, as before).
    let mut projects: BTreeMap<String, (Option<ProjectId>, u64, HashSet<SessionId>)> =
        BTreeMap::new();
    for p in f.projects.values() {
        let pr = projects
            .entry(p.name.clone())
            .or_insert_with(|| (Some(p.project_id), 0, HashSet::new()));
        pr.1 += p.events;
        pr.2.extend(p.sessions.iter().copied());
    }
    DbStatus {
        events: f.events as usize,
        sessions: f.session_count(),
        captured_events: (f.events - f.reconstructed) as usize,
        reconstructed_events: f.reconstructed as usize,
        last_event_at: f.last_event_at,
        providers: f
            .providers
            .values()
            .map(|p| ProviderStat {
                provider: p.provider.clone(),
                events: p.events,
                last_event_at: p.last_event_at,
            })
            .collect(),
        projects: projects
            .into_iter()
            .map(|(name, (project_id, events, s))| ProjectStat {
                project_id,
                name,
                events,
                sessions: s.len() as u64,
            })
            .collect(),
        ..Default::default()
    }
}

fn capture_counts(f: &StreamFacts) -> HashMap<SessionId, CaptureCounts> {
    f.sessions
        .iter()
        .map(|(sid, s)| {
            (
                *sid,
                CaptureCounts {
                    captured: s.captured,
                    reconstructed: s.reconstructed,
                },
            )
        })
        .collect()
}

/// Resolve a project argument: a `prj_` id, a project name, or a path. A name
/// that fits several projects is an error that lists them.
fn resolve_project(f: &StreamFacts, spec: &str) -> Result<ProjectId> {
    f.resolve_project(spec).map_err(|e| anyhow!("{e}"))
}

/// The project of the repository at `root`: by remote first, then by
/// logical root.
fn current_project(f: &StreamFacts, root: &Path) -> Option<ProjectId> {
    let git = attemptdb_capture::git::git_info(root)?;
    let root_logical = PortablePath::from_raw(&git.root.to_string_lossy(), None).logical;
    let remote = git.remote.as_deref().and_then(normalise_remote);
    f.project_of(&root_logical, remote.as_deref())
}

/// Resolve a session argument: a `ses_` id or provider session id, in full or
/// as a prefix of a few characters. A prefix that fits several sessions, or is
/// too short to mean anything, is an error that says so.
fn resolve_session(f: &StreamFacts, spec: &str) -> Result<SessionId> {
    f.resolve_session(spec).map_err(|e| anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_times() {
        assert!(parse_time("now").is_some());
        assert!(parse_time("-2h").is_some());
        assert!(parse_time("2026-08-28").is_some());
        assert!(parse_time("2026-08-28T08:00:00Z").is_some());
        assert!(parse_time("soon").is_none());
    }

    use attemptdb_core::event::Provider;
    use attemptdb_core::{DeviceId, Event, EventKind, ProjectRef};
    use attemptdb_storage::OpenOptions;

    fn events(device: DeviceId, project: &str, session: &str, n: usize) -> Vec<Event> {
        (0..n)
            .map(|_| {
                Event::new(
                    device,
                    Provider::ClaudeCode,
                    "PostToolUse",
                    EventKind::ToolCallFinished,
                    ProjectRef::derive(&format!("/home/dev/{project}"), None, &device),
                    session.to_string(),
                    CaptureMode::MetadataOnly,
                    "store-test/0",
                )
            })
            .collect()
    }

    fn writer(db_dir: &Path) -> Database {
        Database::open(
            db_dir,
            OpenOptions {
                create: true,
                device_id: Some(attemptdb_core::DeviceId::derive(&["store-test"])),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn all_projects() -> ScopeArgs {
        ScopeArgs {
            all_projects: true,
            ..Default::default()
        }
    }

    /// An event written while the database is being loaded must show on the
    /// next call. The view's fingerprint is taken before the read, so the
    /// write makes it stale; taken after, the write would be baked into the
    /// fingerprint and the cached view (which lacks the event) served until
    /// some later event arrived.
    #[test]
    fn an_event_written_during_a_slow_load_is_seen_by_the_next_call() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        let mut db = writer(&db_dir);
        let device = db.device_id();
        db.ingest(events(device, "alpha", "s1", 3)).unwrap();
        db.flush().unwrap();
        drop(db);

        let mut store = Store::new(ServerConfig::new(&db_dir)).unwrap();
        let during = db_dir.clone();
        let mut fired = false;
        store.load_hook = Some(Box::new(move || {
            if std::mem::replace(&mut fired, true) {
                return;
            }
            // The load holds no writer lock, so a writer can append.
            let mut db = writer(&during);
            db.ingest(events(device, "alpha", "s1", 1)).unwrap();
        }));
        let first = store
            .view(&all_projects())
            .unwrap()
            .view
            .engine
            .event_count();
        assert_eq!(first, 3, "the event landed after the read began");
        let second = store
            .view(&all_projects())
            .unwrap()
            .view
            .engine
            .event_count();
        assert_eq!(second, 4, "the next call must not serve the stale view");
        let third = store
            .view(&all_projects())
            .unwrap()
            .view
            .engine
            .event_count();
        assert_eq!(third, 4);
        assert_eq!(store.engine_cache.stats().refreshes, 2, "no third load");
    }

    /// Alternating between scopes does not reload each time, and a relative
    /// `since` is the same scope on every call.
    #[test]
    fn scopes_alternate_without_reloading_and_relative_times_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        let mut db = writer(&db_dir);
        let device = db.device_id();
        db.ingest(events(device, "alpha", "s1", 3)).unwrap();
        db.ingest(events(device, "beta", "s2", 5)).unwrap();
        db.flush().unwrap();
        drop(db);

        let mut store = Store::new(ServerConfig::new(&db_dir)).unwrap();
        let alpha = ScopeArgs {
            project: Some("alpha".into()),
            ..Default::default()
        };
        let beta = ScopeArgs {
            project: Some("beta".into()),
            ..Default::default()
        };
        let count =
            |store: &mut Store, s: &ScopeArgs| store.view(s).unwrap().view.engine.event_count();
        assert_eq!(count(&mut store, &alpha), 3);
        assert_eq!(count(&mut store, &beta), 5);
        assert_eq!(count(&mut store, &alpha), 3);
        assert_eq!(count(&mut store, &beta), 5);
        assert_eq!(count(&mut store, &all_projects()), 8);
        assert_eq!(count(&mut store, &alpha), 3, "still among the last three");
        assert_eq!(
            store.engine_cache.stats().refreshes,
            3,
            "one load per scope, none for the revisits"
        );

        // A relative window is one scope however often it is asked for.
        let recent = ScopeArgs {
            all_projects: true,
            since: Some("-400d".into()),
            ..Default::default()
        };
        let before = store.engine_cache.stats().refreshes;
        store.view(&recent).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.view(&recent).unwrap();
        assert_eq!(store.engine_cache.stats().refreshes, before + 1);
        // The label says what the window does to sessions.
        let label = store.view(&recent).unwrap().view.scope.label.clone();
        assert!(
            label.contains("a session that began earlier is partial"),
            "{label}"
        );
    }

    /// The store keeps no writer lock between calls or during a load.
    #[test]
    fn the_store_does_not_hold_the_writer_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        let mut db = writer(&db_dir);
        let device = db.device_id();
        db.ingest(events(device, "alpha", "s1", 2)).unwrap();
        db.flush().unwrap();
        drop(db);
        let mut store = Store::new(ServerConfig::new(&db_dir)).unwrap();
        let during = db_dir.clone();
        store.load_hook = Some(Box::new(move || {
            // Would fail with `Locked` if the load held the lock.
            drop(writer(&during));
        }));
        assert_eq!(
            store
                .view(&all_projects())
                .unwrap()
                .view
                .engine
                .event_count(),
            2
        );
    }

    #[test]
    fn fingerprint_tracks_wal_and_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        Database::create(&db_dir, attemptdb_core::DeviceId::new()).unwrap();
        let store = Store::new(ServerConfig::new(&db_dir)).unwrap();
        let a = store.fingerprint();
        std::fs::write(db_dir.join(WAL_DIR).join("000009.wal"), b"x").unwrap();
        let b = store.fingerprint();
        assert_ne!(a, b);
        assert_eq!(b, store.fingerprint());
    }
}
