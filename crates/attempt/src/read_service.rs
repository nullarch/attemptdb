//! The daemon's resident read engine, and the CLI's use of it.
//!
//! The capture daemon owns the writer and, with this service installed,
//! keeps an [`EngineCache`] next to it: decoded segments, the incremental
//! projection, per-segment derived parts. A `QUERY` frame refreshes that
//! cache on the writer thread (cheap unless a flush just happened), then
//! builds or reuses a view off it and answers. `attempt timeline`, `query`,
//! `why`, `trace`, `failures` and `handoffs` ask the daemon first and open
//! the database themselves only when no daemon serves it.
//!
//! Measured on a 200 k-event database: opening the database and projecting
//! it cold costs 0.85 s and 600 MB in every CLI process; the daemon's view
//! answers the same statement in the time DataFusion takes to run it.

use crate::cli::{Cli, ScopeArgs};
use crate::ctx::parse_time;
use anyhow::{Context, Result};
use attemptdb_capture::Locator;
use attemptdb_capture::daemon::ReadService;
use attemptdb_capture::ipc::{
    self, ProjectionTotals, ReadKind, ReadRequest, ReadResponse, ReadScope,
};
use attemptdb_core::{SessionId, Timestamp};
use attemptdb_project::Projection;
use attemptdb_query::{EngineCache, QueryEngine, QueryResult, ResultKind, StreamFacts};
use attemptdb_storage::{Database, Refreshed, ScanFilter};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Daemon side
// ---------------------------------------------------------------------------

/// The WAL/manifest state a view was built from: a new event changes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fingerprint {
    generation: u64,
    segments: usize,
    memtable_rows: usize,
}

impl Fingerprint {
    fn of(db: &Database) -> Self {
        let m = db.manifest();
        Self {
            generation: m.generation,
            segments: m.segments.len(),
            memtable_rows: db.memtable_events().len(),
        }
    }
}

/// One built engine over one scope.
struct View {
    engine: QueryEngine,
}

/// What the last refresh produced, shared with the views built from it.
struct Latest {
    fingerprint: Fingerprint,
    refreshed: Arc<Refreshed>,
    facts: Arc<StreamFacts>,
}

pub struct EngineService {
    cache: Mutex<EngineCache>,
    latest: Mutex<Option<Latest>>,
    /// Views by scope, all built from the fingerprint in `latest`; cleared
    /// when it changes.
    views: Mutex<HashMap<String, Arc<View>>>,
    /// When a query last used the engine; everything above is dropped
    /// after [`READ_IDLE`] without one.
    last_used: Mutex<std::time::Instant>,
}

/// How long the resident engine outlives its last query. A view over 200 k
/// events is ~850 MB; a daemon nobody reads from should not carry it.
const READ_IDLE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

impl Default for EngineService {
    fn default() -> Self {
        Self {
            cache: Mutex::new(EngineCache::new()),
            latest: Mutex::new(None),
            views: Mutex::new(HashMap::new()),
            last_used: Mutex::new(std::time::Instant::now()),
        }
    }
}

impl std::fmt::Debug for EngineService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EngineService")
    }
}

impl EngineService {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl ReadService for EngineService {
    fn tick(&self) {
        let idle = Self::lock(&self.last_used).elapsed();
        if idle < READ_IDLE {
            return;
        }
        let had = Self::lock(&self.latest).take().is_some();
        if had {
            Self::lock(&self.views).clear();
            Self::lock(&self.cache).clear();
        }
    }

    fn refresh(&self, db: &Database) -> std::result::Result<(), String> {
        *Self::lock(&self.last_used) = std::time::Instant::now();
        let fingerprint = Fingerprint::of(db);
        let mut latest = Self::lock(&self.latest);
        if latest
            .as_ref()
            .is_some_and(|l| l.fingerprint == fingerprint)
        {
            return Ok(());
        }
        let mut cache = Self::lock(&self.cache);
        let refreshed = cache
            .refresh(db, &db.root().display().to_string())
            .map_err(|e| e.to_string())?;
        let facts = cache.facts(&refreshed);
        *latest = Some(Latest {
            fingerprint,
            refreshed: Arc::new(refreshed),
            facts: Arc::new(facts),
        });
        Self::lock(&self.views).clear();
        Ok(())
    }

    fn handle(
        &self,
        req: ReadRequest,
        rt: &tokio::runtime::Handle,
    ) -> std::result::Result<ReadResponse, String> {
        let (refreshed, facts) = {
            let latest = Self::lock(&self.latest);
            let l = latest.as_ref().ok_or("no refresh has run")?;
            (Arc::clone(&l.refreshed), Arc::clone(&l.facts))
        };
        let filter = filter_for(&req.scope, &facts)?;
        let key = format!("{filter:?}");
        let view = {
            let existing = Self::lock(&self.views).get(&key).cloned();
            match existing {
                Some(v) => v,
                None => {
                    let engine = Self::lock(&self.cache)
                        .engine_scoped(&refreshed, &filter)
                        .map_err(|e| e.to_string())?;
                    let v = Arc::new(View { engine });
                    Self::lock(&self.views).insert(key, Arc::clone(&v));
                    v
                }
            }
        };
        let mut resp = ReadResponse {
            event_count: view.engine.event_count(),
            ..Default::default()
        };
        match req.kind {
            ReadKind::Query => {
                let statement = req.statement.as_deref().unwrap_or("").trim().to_string();
                if statement.is_empty() {
                    return Err("empty statement".to_string());
                }
                let result = rt
                    .block_on(view.engine.query(&statement))
                    .map_err(|e| e.to_string())?;
                resp.result_kind = Some(
                    match result.kind {
                        ResultKind::Rows => "rows",
                        ResultKind::Explanation => "explanation",
                        ResultKind::Empty => "empty",
                    }
                    .to_string(),
                );
                resp.notes = result.notes.clone();
                resp.arrow_ipc_base64 = Some(ipc::base64_encode(
                    &result.to_ipc_bytes().map_err(|e| e.to_string())?,
                ));
            }
            ReadKind::Timeline => {
                let p = view.engine.projection();
                let (trimmed, listed) = trim_projection(p, req.session_limit, req.all_sessions);
                resp.totals = Some(ProjectionTotals {
                    sessions: p.sessions.len(),
                    turns: p.turns.len(),
                    attempts: p.attempts.len(),
                    handoffs: p.handoffs.len(),
                    listed,
                });
                resp.projection = Some(serde_json::to_value(&trimmed).map_err(|e| e.to_string())?);
            }
        }
        Ok(resp)
    }
}

/// The scan filter a request's scope means, resolved against the facts.
fn filter_for(scope: &ReadScope, facts: &StreamFacts) -> std::result::Result<ScanFilter, String> {
    let mut f = ScanFilter::default();
    if let Some(p) = &scope.project {
        f.project_id = Some(crate::ctx::resolve_project(facts, p).map_err(|e| e.to_string())?);
    } else if !scope.all_projects
        && let Some(root) = &scope.repo_root
    {
        f.project_id = facts.project_of(root, scope.repo_remote.as_deref());
    }
    if let Some(s) = &scope.session {
        f.session_id = Some(crate::ctx::resolve_session(facts, s).map_err(|e| e.to_string())?);
    }
    f.since = scope.since_micros.map(Timestamp::from_micros);
    f.until = scope.until_micros.map(Timestamp::from_micros);
    f.captured_only = scope.captured_only;
    Ok(f)
}

/// The newest `limit` sessions the timeline would show, and only their
/// entities, plus how many sessions were eligible before the limit.
/// Counts of the whole projection travel in `totals`.
fn trim_projection(p: &Projection, limit: Option<usize>, all: bool) -> (Projection, usize) {
    let mut sessions: Vec<_> = p
        .sessions
        .iter()
        .filter(|s| all || s.prompt_count > 0 || s.tool_call_count > 0)
        .collect();
    let listed = sessions.len();
    let Some(limit) = limit else {
        return (p.clone(), listed);
    };
    sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
    let keep: std::collections::HashSet<SessionId> =
        sessions.iter().take(limit).map(|s| s.session_id).collect();
    let mut out = p.clone();
    out.sessions.retain(|s| keep.contains(&s.session_id));
    out.turns.retain(|t| keep.contains(&t.session_id));
    out.tool_calls.retain(|c| keep.contains(&c.session_id));
    out.attempts.retain(|a| keep.contains(&a.session_id));
    out.signals.retain(|s| keep.contains(&s.session_id));
    out.commits.retain(|c| keep.contains(&c.session_id));
    // Handoffs are listed in full (there are few); edges and work units are
    // not rendered by the timeline, and dropping them keeps the answer
    // small.
    out.edges.clear();
    out.work_units.clear();
    out.decisions.clear();
    (out, listed)
}

// ---------------------------------------------------------------------------
// CLI side
// ---------------------------------------------------------------------------

/// Whether this invocation may ask the daemon: a live database (no
/// snapshot), and no `ATTEMPTDB_NO_DAEMON` opt-out.
pub fn daemon_allowed(cli: &Cli) -> bool {
    cli.snapshot.is_none() && std::env::var_os("ATTEMPTDB_NO_DAEMON").is_none()
}

/// The request scope for `scope`, with the client's repository for the
/// default per-repository scope.
pub fn read_scope(scope: &ScopeArgs, cwd: &std::path::Path) -> Result<ReadScope> {
    let mut s = ReadScope {
        project: scope.project.clone(),
        all_projects: scope.all_projects,
        session: scope.session.clone(),
        captured_only: scope.captured_only,
        ..Default::default()
    };
    if let Some(t) = &scope.since {
        s.since_micros = Some(
            parse_time(t)
                .with_context(|| format!("cannot parse --since {t:?}"))?
                .as_micros(),
        );
    }
    if let Some(t) = &scope.until {
        s.until_micros = Some(
            parse_time(t)
                .with_context(|| format!("cannot parse --until {t:?}"))?
                .as_micros(),
        );
    }
    if s.project.is_none()
        && !s.all_projects
        && let Some(git) = attemptdb_capture::git::git_info(cwd)
    {
        s.repo_root =
            Some(attemptdb_core::PortablePath::from_raw(&git.root.to_string_lossy(), None).logical);
        s.repo_remote = git
            .remote
            .as_deref()
            .and_then(attemptdb_core::event::normalise_remote);
    }
    Ok(s)
}

/// Run `statement` on the daemon serving `locator`'s database. `None` when
/// no daemon answers (not running, another database, no read service, a
/// result too large): the caller opens the database itself.
pub fn query_via_daemon(
    locator: &Locator,
    scope: ReadScope,
    statement: &str,
) -> Option<QueryResult> {
    let req = ReadRequest {
        kind: ReadKind::Query,
        statement: Some(statement.to_string()),
        scope,
        session_limit: None,
        all_sessions: false,
    };
    let resp = ipc::Client::read(locator, &req).ok()?;
    let bytes = ipc::base64_decode(resp.arrow_ipc_base64.as_deref()?)?;
    let kind = match resp.result_kind.as_deref()? {
        "rows" => ResultKind::Rows,
        "explanation" => ResultKind::Explanation,
        _ => ResultKind::Empty,
    };
    QueryResult::from_ipc_bytes(&bytes, kind, resp.notes).ok()
}

/// The timeline's projection from the daemon, trimmed to `session_limit`
/// sessions; `None` as for [`query_via_daemon`].
pub fn timeline_via_daemon(
    locator: &Locator,
    scope: ReadScope,
    session_limit: Option<usize>,
    all_sessions: bool,
) -> Option<(Projection, ProjectionTotals, usize)> {
    let req = ReadRequest {
        kind: ReadKind::Timeline,
        statement: None,
        scope,
        session_limit,
        all_sessions,
    };
    let resp = ipc::Client::read(locator, &req).ok()?;
    let p: Projection = serde_json::from_value(resp.projection?).ok()?;
    Some((p, resp.totals.unwrap_or_default(), resp.event_count))
}
