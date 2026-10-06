//! Opening the database on demand and keeping a few query engines warm until
//! the files underneath them change.
//!
//! Every request asks for a [`View`] for a scope. The store computes a cheap
//! filesystem fingerprint of the database (identity file, manifest
//! generations, WAL files, spool files); when it matches a cached engine's
//! fingerprint and the scope is the same, the engine is reused (the last
//! [`VIEW_SLOTS`] scopes are kept, so two browser tabs on different scopes do
//! not rebuild each other's view). Otherwise a fresh engine is built: the
//! spool is imported when the writer lock is free (the lock is let go at
//! once, not held for the load), the fingerprint is taken, and only then is
//! the database read, so an event written while a long load runs makes the
//! cached view stale and the next request sees it. A scope reads its own
//! rows; facts for scope resolution and the status come from a few columns
//! of every segment.
//!
//! This mirrors the MCP server's store on purpose; the UI does not depend on
//! the MCP crate.

use crate::UiConfig;
use anyhow::{Context, Result, anyhow, bail};
use attemptdb_capture::daemon::{self, Probe};
use attemptdb_capture::{Config, Locator, ingest};
use attemptdb_core::event::normalise_remote;
use attemptdb_core::{CaptureMode, Event, PortablePath, ProjectId, SessionId, Timestamp};
use attemptdb_query::{EngineCache, QueryEngine, StreamFacts, TimeExpr};
use attemptdb_storage::format::{IDENTITY_FILE, MANIFEST_DIR, SPOOL_DIR, WAL_DIR};
use attemptdb_storage::{Database, IngestReport, ScanFilter, snapshot};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tokio::sync::Mutex;

/// Scope arguments exactly as the caller passed them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopeArgs {
    pub project: Option<String>,
    pub all_projects: bool,
    pub session: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub captured_only: bool,
    /// Open the bundled demo database instead of this machine's own.
    pub demo: bool,
}

/// Parse a time argument the way the CLI does: RFC 3339, `YYYY-MM-DD`,
/// epoch, `now`, `today`, `yesterday`, or `-<n>(s|m|h|d|w)`.
pub fn parse_time(spec: &str) -> Option<Timestamp> {
    TimeExpr::parse_literal(spec).map(|e| e.resolve(Timestamp::now()))
}

fn time_arg(spec: &Option<String>, what: &str) -> Result<Option<Timestamp>> {
    match spec {
        None => Ok(None),
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => parse_time(s).map(Some).ok_or_else(|| {
            anyhow::anyhow!(
                "cannot parse {what} {s:?}: use RFC 3339, YYYY-MM-DD, now, today, yesterday or -<n>(s|m|h|d|w)"
            )
        }),
    }
}

/// The scope as the caller wrote it; the cache key together with the
/// fingerprint. Times stay as written: `-400d` is the same scope on the next
/// request, and resolving it to an instant would make every request a
/// different key (and a full reload). The instant is resolved when a view is
/// built and shows in its label; a view is rebuilt whenever the database
/// changes, so a relative window slides with it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ScopeKey {
    project: Option<String>,
    all_projects: bool,
    session: Option<String>,
    since: Option<String>,
    until: Option<String>,
    captured_only: bool,
    demo: bool,
}

impl ScopeKey {
    fn from_args(args: &ScopeArgs) -> Result<Self> {
        // Reject an unreadable time now, with the caller's own words.
        time_arg(&args.since, "since")?;
        time_arg(&args.until, "until")?;
        let expr = |s: &Option<String>| {
            s.as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        Ok(Self {
            project: args.project.clone().filter(|p| !p.trim().is_empty()),
            all_projects: args.all_projects,
            session: args.session.clone().filter(|s| !s.trim().is_empty()),
            since: expr(&args.since),
            until: expr(&args.until),
            captured_only: args.captured_only,
            demo: args.demo,
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
    /// Human label, e.g. `project acme/repo · since 2026-08-28T00:00:00Z`.
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
    pub root: String,
    pub events: u64,
    pub sessions: u64,
}

/// What the daemon probe said when the view was built.
#[derive(Clone, Debug)]
pub enum DaemonState {
    /// Serving a snapshot or the demo: no daemon involved.
    NotApplicable,
    Running {
        pid: u32,
        endpoint: String,
        events_ingested: u64,
    },
    NotRunning,
    Unresponsive(String),
}

impl DaemonState {
    pub fn label(&self) -> String {
        match self {
            DaemonState::NotApplicable => {
                "n/a (this database is not written by the daemon)".to_string()
            }
            DaemonState::Running { pid, .. } => format!("running (pid {pid})"),
            DaemonState::NotRunning => "not running".to_string(),
            DaemonState::Unresponsive(e) => format!("not answering ({e})"),
        }
    }

    pub fn state(&self) -> &'static str {
        match self {
            DaemonState::NotApplicable => "n/a",
            DaemonState::Running { .. } => "running",
            DaemonState::NotRunning => "not_running",
            DaemonState::Unresponsive(_) => "unresponsive",
        }
    }
}

/// Database-wide facts gathered when the engine was (re)built.
#[derive(Clone, Debug)]
pub struct DbStatus {
    pub source: String,
    pub read_only: bool,
    pub snapshot: bool,
    /// Serving the bundled demo database, not this machine's own.
    pub demo: bool,
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
    pub daemon: DaemonState,
    pub loaded_at: Timestamp,
}

impl Default for DbStatus {
    fn default() -> Self {
        Self {
            source: String::new(),
            read_only: false,
            snapshot: false,
            demo: false,
            capture_mode: CaptureMode::default(),
            generation: 0,
            segments: 0,
            segment_rows: 0,
            memtable_rows: 0,
            wal_bytes: 0,
            spool_pending: false,
            events: 0,
            sessions: 0,
            captured_events: 0,
            reconstructed_events: 0,
            last_event_at: None,
            providers: Vec::new(),
            projects: Vec::new(),
            import: None,
            warnings: Vec::new(),
            daemon: DaemonState::NotRunning,
            loaded_at: Timestamp::default(),
        }
    }
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

impl View {
    /// Captured / reconstructed counts over the sessions in scope.
    pub fn scoped_capture(&self) -> CaptureCounts {
        self.engine
            .projection()
            .sessions
            .iter()
            .fold(CaptureCounts::default(), |acc, s| {
                let c = self
                    .session_capture
                    .get(&s.session_id)
                    .copied()
                    .unwrap_or_default();
                CaptureCounts {
                    captured: acc.captured + c.captured,
                    reconstructed: acc.reconstructed + c.reconstructed,
                }
            })
    }
}

/// Files that change whenever the database content changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint(Vec<(String, u64, u128)>);

impl Fingerprint {
    /// A short opaque revision string: equal fingerprints give equal
    /// revisions. This is what the live stream announces — the client
    /// compares it and refetches the one resource its page is about.
    pub fn revision(&self) -> String {
        // FNV-1a, 64 bit. Nothing about the database leaks: the input is
        // file names, sizes and mtimes, and the output is 16 hex digits.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |bytes: &[u8]| {
            for b in bytes {
                h ^= *b as u64;
                h = h.wrapping_mul(0x1000_0000_01b3);
            }
        };
        for (name, len, mtime) in &self.0 {
            eat(name.as_bytes());
            eat(&len.to_le_bytes());
            eat(&mtime.to_le_bytes());
        }
        format!("{h:016x}")
    }
}

struct Cached {
    fingerprint: Fingerprint,
    key: ScopeKey,
    view: Arc<View>,
}

/// How many scopes stay warm: the current one and the ones before it.
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
    config: UiConfig,
    locator: Locator,
    /// Most recently used first; every entry was built from the same files
    /// (entries from older states are dropped on the next request).
    cache: Mutex<Vec<Cached>>,
    engine_cache: Mutex<EngineCache>,
    /// Runs after the database is opened and fingerprinted, before it is
    /// read: tests write an event here to model a slow load.
    #[cfg(test)]
    load_hook: std::sync::Mutex<Option<Box<dyn FnMut() + Send>>>,
}

impl Store {
    pub fn new(config: UiConfig) -> Self {
        let cwd = config
            .project_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let locator = Locator::resolve(&cwd, config.data_dir.as_deref(), Some(&config.db_dir));
        Self {
            config,
            locator,
            cache: Mutex::new(Vec::new()),
            engine_cache: Mutex::new(EngineCache::new()),
            #[cfg(test)]
            load_hook: std::sync::Mutex::new(None),
        }
    }

    pub fn config(&self) -> &UiConfig {
        &self.config
    }

    /// (segments decoded, refreshes served, events held by the projector)
    /// — what the cache has cost and holds so far.
    pub async fn cache_stats(&self) -> (u64, u64, usize) {
        let s = self.engine_cache.lock().await.stats();
        (s.decodes, s.refreshes, s.events)
    }

    pub fn locator(&self) -> &Locator {
        &self.locator
    }

    /// A view for `scope`, reusing a cached engine when neither the
    /// database files nor the scope changed.
    pub async fn view(&self, scope: &ScopeArgs) -> Result<Arc<View>> {
        let key = ScopeKey::from_args(scope)?;
        let fresh = self.fingerprint_of(key.demo);
        let mut guard = self.cache.lock().await;
        // A view built from older files can never be served again.
        guard.retain(|c| c.fingerprint == fresh);
        if let Some(i) = guard.iter().position(|c| c.key == key) {
            let hit = guard.remove(i);
            let view = Arc::clone(&hit.view);
            guard.insert(0, hit);
            return Ok(view);
        }
        let loaded = self.load(key).await?;
        let view = Arc::clone(&loaded.view);
        guard.insert(0, loaded);
        guard.truncate(VIEW_SLOTS);
        Ok(view)
    }

    /// Cheap staleness probe: sizes and mtimes of every file that ingestion
    /// or a flush touches (or the snapshot file itself).
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint_of(false)
    }

    /// The same probe for the database a scope actually opens.
    pub fn fingerprint_of(&self, demo: bool) -> Fingerprint {
        let mut entries = Vec::new();
        let demo_dir;
        if let Some(file) = &self.config.snapshot
            && !demo
        {
            push_meta(&mut entries, "snapshot", file);
            return Fingerprint(entries);
        }
        let root = if demo {
            demo_dir = crate::demo::demo_dir(&self.locator.paths.cache_dir);
            &demo_dir
        } else {
            &self.config.db_dir
        };
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
    /// changes it (and is picked up by the next request), and whatever landed
    /// earlier is in the handle.
    fn open(&self, demo: bool) -> Result<Opened> {
        if demo {
            let dir = crate::demo::ensure(&self.locator.paths.cache_dir)
                .context("preparing the bundled demo database")?;
            let fingerprint = self.fingerprint_of(true);
            let db = Database::open(&dir, attemptdb_storage::OpenOptions::default())
                .with_context(|| format!("opening the demo database {}", dir.display()))?;
            return Ok(Opened {
                db,
                import: None,
                read_only: false,
                snapshot: false,
                // Deliberately not the path: the demo is what gets
                // screenshotted, and a cache path names the user.
                source: "bundled demo".to_string(),
                fingerprint,
            });
        }
        if let Some(file) = &self.config.snapshot {
            let fingerprint = self.fingerprint_of(false);
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
                "no database at {} — run `attempt setup` (database, agent hooks and background daemon in one go), or `attempt init` to create only the database{}",
                self.config.db_dir.display(),
                inside_hint(&self.config.db_dir)
            );
        }
        let pending = ingest::import_pending(&self.locator)
            .with_context(|| format!("opening {}", self.config.db_dir.display()))?;
        let fingerprint = self.fingerprint_of(false);
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

    async fn load(&self, key: ScopeKey) -> Result<Cached> {
        let demo = key.demo;
        let opened = self.open(demo)?;
        #[cfg(test)]
        if let Ok(mut hook) = self.load_hook.lock()
            && let Some(hook) = hook.as_mut()
        {
            hook();
        }
        let mut engine_cache = self.engine_cache.lock().await;
        let mut refreshed = engine_cache
            .refresh_lazy(&opened.db, &opened.source)
            .context("listing the database's segments")?;
        // A segment a compaction deletes mid-read is not an error: the
        // listing is renewed from a fresh manifest and the read repeated.
        let locator = (!opened.snapshot && !demo).then(|| self.locator.clone());
        let mut reopen = move || match &locator {
            Some(l) => ingest::open_reader(l).map_err(|e| {
                attemptdb_query::QueryError::Exec(format!("reopening the database: {e}"))
            }),
            None => Err(attemptdb_query::QueryError::Exec(
                "this database cannot change underneath a read".into(),
            )),
        };
        let facts = engine_cache
            .retrying(&mut refreshed, &mut reopen, |c, r| c.facts(r))
            .context("reading the database's facts")?;
        let scope = self.resolve_scope(&key, &facts)?;
        let filter = scope.filter();
        // Unfiltered: cached batches, incremental projection, per-segment
        // derived parts shared with the cache. Scoped: the scoped rows, read
        // and projected on their own.
        let engine = engine_cache
            .retrying(&mut refreshed, &mut reopen, |c, r| {
                c.engine_scoped(r, &filter)
            })
            .context("building the query engine")?;
        let stats = opened.db.stats();
        let mut status = summarize(&facts);
        status.source = opened.source.clone();
        status.read_only = opened.read_only;
        status.snapshot = opened.snapshot;
        status.demo = demo;
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
        drop(engine_cache);
        status.daemon = if status.snapshot || demo {
            DaemonState::NotApplicable
        } else {
            match daemon::probe(&self.locator) {
                Probe::Running(s) => DaemonState::Running {
                    pid: s.pid,
                    endpoint: s.endpoint.clone(),
                    events_ingested: s.events_ingested,
                },
                Probe::NotRunning => DaemonState::NotRunning,
                Probe::Unresponsive(e) => DaemonState::Unresponsive(e.to_string()),
            }
        };
        Ok(Cached {
            // Taken before the read: an event written during it makes the
            // next request's fingerprint differ from this one.
            fingerprint: opened.fingerprint,
            key,
            view: Arc::new(View {
                engine,
                scope,
                status,
                session_capture,
            }),
        })
    }

    fn resolve_scope(&self, key: &ScopeKey, all: &StreamFacts) -> Result<ScopeInfo> {
        let (project_id, default_reason) = if let Some(spec) = &key.project {
            (Some(resolve_project(all, spec)?), None)
        } else if key.all_projects {
            (None, None)
        } else if key.demo {
            // The demo has one project; the working directory is irrelevant
            // to it.
            (
                all.projects
                    .iter()
                    .max_by_key(|(_, p)| p.events)
                    .map(|(id, _)| *id),
                Some("demo data: the bundled build history of AttemptDB itself".to_string()),
            )
        } else if let Some(root) = &self.config.project_root {
            match current_project(all, root) {
                Some(id) => (
                    Some(id),
                    Some(format!(
                        "default scope is the repository at {}; choose another project or all projects in the scope bar",
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
            (Some(_), Some(name)) => format!("project {name}"),
            (Some(pid), None) => format!("project prj_{pid}"),
            (None, _) => "all projects".to_string(),
        }];
        if let Some(sid) = session_id {
            parts.push(format!("session ses_{sid}"));
        }
        if let Some(t) = since {
            // The window drops events before projecting, so a session that
            // began earlier is reported from where the window starts.
            parts.push(format!(
                "since {} (events before it are left out; a session that began earlier is partial)",
                t.to_rfc3339()
            ));
        }
        if let Some(t) = until {
            parts.push(format!("until {}", t.to_rfc3339()));
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

pub fn is_reconstructed(ev: &Event) -> bool {
    ev.attrs
        .get("reconstructed")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

fn summarize(f: &StreamFacts) -> DbStatus {
    // Projects are listed by name (two ids sharing a name merge, as before).
    let mut projects: BTreeMap<String, (Option<ProjectId>, String, u64, HashSet<SessionId>)> =
        BTreeMap::new();
    for p in f.projects.values() {
        let pr = projects
            .entry(p.name.clone())
            .or_insert_with(|| (Some(p.project_id), p.root.clone(), 0, HashSet::new()));
        pr.2 += p.events;
        pr.3.extend(p.sessions.iter().copied());
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
            .map(|(name, (project_id, root, events, s))| ProjectStat {
                project_id,
                name,
                root,
                events,
                sessions: s.len() as u64,
            })
            .collect(),
        ..Default::default()
    }
}

pub fn capture_counts(f: &StreamFacts) -> HashMap<SessionId, CaptureCounts> {
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

/// When `db_dir` is not a database but holds a `.attemptdb` that is one, say
/// to point `--db` at it.
fn inside_hint(db_dir: &Path) -> String {
    let inside = db_dir.join(attemptdb_capture::locator::LOCAL_DB_DIR_NAME);
    if Database::exists(&inside) {
        format!(
            "; {} holds a `.attemptdb` database: use --db {} instead",
            db_dir.display(),
            inside.display()
        )
    } else {
        String::new()
    }
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

    use attemptdb_core::event::Provider;
    use attemptdb_core::{DeviceId, EventKind, ProjectRef};
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
                device_id: Some(DeviceId::derive(&["store-test"])),
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
    /// next request: the fingerprint is taken before the read, so the write
    /// makes the cached view stale.
    #[tokio::test]
    async fn an_event_written_during_a_slow_load_is_seen_by_the_next_request() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        let mut db = writer(&db_dir);
        let device = db.device_id();
        db.ingest(events(device, "alpha", "s1", 3)).unwrap();
        db.flush().unwrap();
        drop(db);

        let store = Store::new(UiConfig::new(&db_dir));
        let during = db_dir.clone();
        let mut fired = false;
        *store.load_hook.lock().unwrap() = Some(Box::new(move || {
            if std::mem::replace(&mut fired, true) {
                return;
            }
            // The load holds no writer lock, so a writer can append.
            let mut db = writer(&during);
            db.ingest(events(device, "alpha", "s1", 1)).unwrap();
        }));
        let first = store.view(&all_projects()).await.unwrap();
        assert_eq!(first.engine.event_count(), 3, "landed after the read began");
        let second = store.view(&all_projects()).await.unwrap();
        assert_eq!(
            second.engine.event_count(),
            4,
            "must not serve the stale view"
        );
        let third = store.view(&all_projects()).await.unwrap();
        assert_eq!(third.engine.event_count(), 4);
        assert!(Arc::ptr_eq(&second, &third), "and then it is cached");
    }

    /// Two scopes alternate without reloading, and a relative `since` is the
    /// same scope on every request.
    #[tokio::test]
    async fn scopes_alternate_without_reloading_and_relative_times_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        let mut db = writer(&db_dir);
        let device = db.device_id();
        db.ingest(events(device, "alpha", "s1", 3)).unwrap();
        db.ingest(events(device, "beta", "s2", 5)).unwrap();
        db.flush().unwrap();
        drop(db);

        let store = Store::new(UiConfig::new(&db_dir));
        let alpha = ScopeArgs {
            project: Some("alpha".into()),
            ..Default::default()
        };
        let beta = ScopeArgs {
            project: Some("beta".into()),
            ..Default::default()
        };
        let a1 = store.view(&alpha).await.unwrap();
        let b1 = store.view(&beta).await.unwrap();
        assert_eq!((a1.engine.event_count(), b1.engine.event_count()), (3, 5));
        let a2 = store.view(&alpha).await.unwrap();
        let b2 = store.view(&beta).await.unwrap();
        assert!(Arc::ptr_eq(&a1, &a2) && Arc::ptr_eq(&b1, &b2), "no reload");
        assert_eq!(store.cache_stats().await.1, 2, "one load per scope");

        let recent = ScopeArgs {
            all_projects: true,
            since: Some("-400d".into()),
            ..Default::default()
        };
        let r1 = store.view(&recent).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let r2 = store.view(&recent).await.unwrap();
        assert!(Arc::ptr_eq(&r1, &r2));
        assert!(
            r1.scope
                .label
                .contains("a session that began earlier is partial"),
            "{}",
            r1.scope.label
        );
    }

    /// The store keeps no writer lock between requests or during a load.
    #[tokio::test]
    async fn the_store_does_not_hold_the_writer_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        let mut db = writer(&db_dir);
        let device = db.device_id();
        db.ingest(events(device, "alpha", "s1", 2)).unwrap();
        db.flush().unwrap();
        drop(db);
        let store = Store::new(UiConfig::new(&db_dir));
        let during = db_dir.clone();
        *store.load_hook.lock().unwrap() = Some(Box::new(move || {
            // Would fail with `Locked` if the load held the lock.
            drop(writer(&during));
        }));
        assert_eq!(
            store
                .view(&all_projects())
                .await
                .unwrap()
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
        let store = Store::new(UiConfig::new(&db_dir));
        let a = store.fingerprint();
        std::fs::write(db_dir.join(WAL_DIR).join("000009.wal"), b"x").unwrap();
        let b = store.fingerprint();
        assert_ne!(a, b);
        assert_eq!(b, store.fingerprint());
    }

    #[test]
    fn parses_times() {
        assert!(parse_time("now").is_some());
        assert!(parse_time("-2h").is_some());
        assert!(parse_time("2026-08-28").is_some());
        assert!(parse_time("soon").is_none());
    }
}
