//! Opening the database on demand and keeping one query engine warm until
//! the files underneath it change.
//!
//! Every request asks for a [`View`] for a scope. The store computes a cheap
//! filesystem fingerprint of the database (identity file, manifest
//! generations, WAL files, spool files); when it matches the cached engine's
//! fingerprint and the scope is the same, the engine is reused. Otherwise the
//! database is re-opened — as the writer when the lock is free (which imports
//! the spool), read-only when a daemon or another CLI holds it — and a fresh
//! engine is built. The `Database` handle (and with it any writer lock) is
//! dropped as soon as the engine exists, so the server never holds the lock
//! between requests.
//!
//! This mirrors the MCP server's store on purpose; the UI does not depend on
//! the MCP crate.

use crate::UiConfig;
use anyhow::{Context, Result, bail};
use attemptdb_capture::daemon::{self, Probe};
use attemptdb_capture::{Config, Locator, ingest};
use attemptdb_core::event::normalise_remote;
use attemptdb_core::{
    CaptureMode, Event, EventKind, PortablePath, ProjectId, SessionId, Timestamp,
};
use attemptdb_query::{QueryEngine, TimeExpr};
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

/// The scope with times resolved; the cache key together with the
/// fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ScopeKey {
    project: Option<String>,
    all_projects: bool,
    session: Option<String>,
    since: Option<Timestamp>,
    until: Option<Timestamp>,
    captured_only: bool,
}

impl ScopeKey {
    fn from_args(args: &ScopeArgs) -> Result<Self> {
        Ok(Self {
            project: args.project.clone().filter(|p| !p.trim().is_empty()),
            all_projects: args.all_projects,
            session: args.session.clone().filter(|s| !s.trim().is_empty()),
            since: time_arg(&args.since, "since")?,
            until: time_arg(&args.until, "until")?,
            captured_only: args.captured_only,
        })
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
    /// Serving a snapshot: no daemon involved.
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
            DaemonState::NotApplicable => "n/a (snapshot)".to_string(),
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

struct Cached {
    fingerprint: Fingerprint,
    key: ScopeKey,
    view: Arc<View>,
}

struct Opened {
    db: Database,
    import: Option<IngestReport>,
    read_only: bool,
    snapshot: bool,
    source: String,
}

pub struct Store {
    config: UiConfig,
    locator: Locator,
    cache: Mutex<Option<Cached>>,
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
            cache: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &UiConfig {
        &self.config
    }

    pub fn locator(&self) -> &Locator {
        &self.locator
    }

    /// A view for `scope`, reusing the cached engine when neither the
    /// database files nor the scope changed.
    pub async fn view(&self, scope: &ScopeArgs) -> Result<Arc<View>> {
        let key = ScopeKey::from_args(scope)?;
        let fresh = self.fingerprint();
        let mut guard = self.cache.lock().await;
        if let Some(c) = guard.as_ref()
            && c.fingerprint == fresh
            && c.key == key
        {
            return Ok(Arc::clone(&c.view));
        }
        *guard = None;
        let loaded = self.load(key).await?;
        let view = Arc::clone(&loaded.view);
        *guard = Some(loaded);
        Ok(view)
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

    fn open(&self) -> Result<Opened> {
        if let Some(file) = &self.config.snapshot {
            let (db, dir) = snapshot::open_read_only(file, &self.locator.snapshot_cache_dir())
                .with_context(|| format!("opening snapshot {}", file.display()))?;
            return Ok(Opened {
                db,
                import: None,
                read_only: true,
                snapshot: true,
                source: format!("snapshot {} (cached at {})", file.display(), dir.display()),
            });
        }
        if !Database::exists(&self.config.db_dir) {
            bail!(
                "no database at {} — run `attempt init` (or `attempt init --local` inside the project) and install hooks with `attempt hook install`",
                self.config.db_dir.display()
            );
        }
        let (db, import, read_only) = ingest::open_fresh(&self.locator, false)
            .with_context(|| format!("opening {}", self.config.db_dir.display()))?;
        Ok(Opened {
            db,
            import,
            read_only,
            snapshot: false,
            source: self.config.db_dir.display().to_string(),
        })
    }

    async fn load(&self, key: ScopeKey) -> Result<Cached> {
        let opened = self.open()?;
        let all = opened
            .db
            .scan(&ScanFilter::default())
            .context("scanning events")?;
        let scope = self.resolve_scope(&key, &all)?;
        let engine = QueryEngine::from_database(&opened.db, &scope.filter())
            .await
            .context("building the query engine")?;
        let stats = opened.db.stats();
        let mut status = summarize(&all);
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
        let session_capture = capture_counts(&all);
        // Release the writer lock (if we held it) before anything else.
        drop(opened);
        status.daemon = if status.snapshot {
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
        let fingerprint = self.fingerprint();
        Ok(Cached {
            fingerprint,
            key,
            view: Arc::new(View {
                engine,
                scope,
                status,
                session_capture,
            }),
        })
    }

    fn resolve_scope(&self, key: &ScopeKey, all: &[Event]) -> Result<ScopeInfo> {
        let (project_id, default_reason) = if let Some(spec) = &key.project {
            (Some(resolve_project(all, spec)?), None)
        } else if key.all_projects {
            (None, None)
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
        let project_name = project_id.and_then(|pid| {
            all.iter()
                .find(|e| e.project.project_id == pid)
                .map(|e| e.project.name.clone())
        });
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
        if let Some(t) = key.since {
            parts.push(format!("since {}", t.to_rfc3339()));
        }
        if let Some(t) = key.until {
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
            since: key.since,
            until: key.until,
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

fn summarize(all: &[Event]) -> DbStatus {
    let mut providers: BTreeMap<String, ProviderStat> = BTreeMap::new();
    let mut projects: BTreeMap<String, (Option<ProjectId>, String, u64, HashSet<SessionId>)> =
        BTreeMap::new();
    let mut sessions: HashSet<SessionId> = HashSet::new();
    let mut status = DbStatus {
        events: all.len(),
        ..Default::default()
    };
    for ev in all {
        let p = providers
            .entry(ev.provider.as_str().to_string())
            .or_insert_with(|| ProviderStat {
                provider: ev.provider.as_str().to_string(),
                ..Default::default()
            });
        p.events += 1;
        if ev.kind != EventKind::CaptureTest {
            p.last_event_at = Some(
                p.last_event_at
                    .map_or(ev.observed_at, |t| t.max(ev.observed_at)),
            );
            status.last_event_at = Some(
                status
                    .last_event_at
                    .map_or(ev.observed_at, |t| t.max(ev.observed_at)),
            );
        }
        let pr = projects.entry(ev.project.name.clone()).or_insert_with(|| {
            (
                Some(ev.project.project_id),
                ev.project.root.clone(),
                0,
                HashSet::new(),
            )
        });
        pr.2 += 1;
        pr.3.insert(ev.session_id);
        sessions.insert(ev.session_id);
        if is_reconstructed(ev) {
            status.reconstructed_events += 1;
        } else {
            status.captured_events += 1;
        }
    }
    status.sessions = sessions.len();
    status.providers = providers.into_values().collect();
    status.projects = projects
        .into_iter()
        .map(|(name, (project_id, root, events, s))| ProjectStat {
            project_id,
            name,
            root,
            events,
            sessions: s.len() as u64,
        })
        .collect();
    status
}

pub fn capture_counts(all: &[Event]) -> HashMap<SessionId, CaptureCounts> {
    let mut out: HashMap<SessionId, CaptureCounts> = HashMap::new();
    for ev in all {
        let c = out.entry(ev.session_id).or_default();
        if is_reconstructed(ev) {
            c.reconstructed += 1;
        } else {
            c.captured += 1;
        }
    }
    out
}

/// Resolve a project argument: a `prj_` id, a project name, or a path.
fn resolve_project(all: &[Event], spec: &str) -> Result<ProjectId> {
    if let Ok(pid) = spec.parse::<ProjectId>()
        && all.iter().any(|ev| ev.project.project_id == pid)
    {
        return Ok(pid);
    }
    let mut candidates: Vec<(ProjectId, String, String)> = Vec::new();
    for ev in all {
        if !candidates.iter().any(|c| c.0 == ev.project.project_id) {
            candidates.push((
                ev.project.project_id,
                ev.project.name.clone(),
                ev.project.root.clone(),
            ));
        }
    }
    let spec_norm = PortablePath::from_raw(spec, None).logical;
    if let Some(c) = candidates.iter().find(|c| {
        c.1.eq_ignore_ascii_case(spec) || c.2 == spec_norm || c.1.ends_with(&format!("/{spec}"))
    }) {
        return Ok(c.0);
    }
    let names: Vec<String> = candidates
        .iter()
        .map(|c| format!("{} (prj_{})", c.1, c.0))
        .collect();
    bail!(
        "unknown project {spec:?}; known projects: {}",
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        }
    )
}

/// The project of the repository containing `root`, if the database knows it.
fn current_project(all: &[Event], root: &Path) -> Option<ProjectId> {
    let git = attemptdb_capture::git::git_info(root)?;
    let root_logical = PortablePath::from_raw(&git.root.to_string_lossy(), None).logical;
    let remote = git.remote.as_deref().and_then(normalise_remote);
    let mut best: Option<ProjectId> = None;
    for ev in all {
        if remote.is_some() && ev.project.repo_remote == remote {
            return Some(ev.project.project_id);
        }
        if ev.project.root == root_logical {
            best = Some(ev.project.project_id);
        }
    }
    best
}

/// Resolve a session argument: a `ses_` id (full or short), or a provider
/// session id.
fn resolve_session(all: &[Event], spec: &str) -> Result<SessionId> {
    let canonical = spec.parse::<SessionId>().ok();
    if let Some(sid) = canonical
        && all.iter().any(|ev| ev.session_id == sid)
    {
        return Ok(sid);
    }
    let needle = spec.trim_start_matches("ses_");
    for ev in all {
        if ev.provider_session_id == spec
            || ev.session_id.short() == spec
            || ev.session_id.to_string().starts_with(needle)
            || ev.session_id.0.simple().to_string().starts_with(needle)
            || ev.provider_session_id.starts_with(spec)
        {
            return Ok(ev.session_id);
        }
    }
    bail!("unknown session {spec:?} (expected a ses_ id or a provider session id)")
}

#[cfg(test)]
mod tests {
    use super::*;

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
