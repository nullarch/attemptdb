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
use attemptdb_capture::daemon::{ReadCancel, ReadError, ReadService};
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
    /// The most rows (events) a view the daemon builds may hold; a request
    /// whose scope reaches further is declined with `read_locally`.
    view_limit: u64,
    cache: Mutex<EngineCache>,
    latest: Mutex<Option<Latest>>,
    /// Views by scope, all built from the fingerprint in `latest`; cleared
    /// when it changes.
    views: Mutex<HashMap<String, Arc<View>>>,
    /// When a query last used the engine; everything above is dropped
    /// after [`READ_IDLE`] without one.
    last_used: Mutex<std::time::Instant>,
}

/// The most events a view built inside the daemon may cover. A view holds
/// the scope's rows decoded, their projection and the SQL layer over them:
/// 449 MB of daemon became 4.6 GB for a view over the whole of a 4-million
/// event database (16 s of the writer's machine, and ten minutes before the
/// idle timeout gave it back). The daemon is a background process with a
/// writer to keep fast; a scope larger than this is read by the client in its
/// own process, which is what the client falls back to anyway. Narrow the
/// scope (`--project`, `--session`, `--since`) to have the daemon serve it.
pub const VIEW_LIMIT_ROWS: u64 = 300_000;

/// How long the resident engine outlives its last query. A view over 200 k
/// events is ~850 MB; a daemon nobody reads from should not carry it.
const READ_IDLE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

impl Default for EngineService {
    fn default() -> Self {
        Self {
            view_limit: VIEW_LIMIT_ROWS,
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

    /// A service with another view limit (tests).
    #[cfg(test)]
    pub fn with_view_limit(rows: u64) -> Self {
        Self {
            view_limit: rows,
            ..Self::default()
        }
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
        // Listed, not decoded: the facts read a few columns of each new
        // segment, and a view decodes only the rows of its own scope.
        let refreshed = cache
            .refresh_lazy(db, &db.root().display().to_string())
            .map_err(|e| e.to_string())?;
        let facts = cache.facts(&refreshed).map_err(|e| e.to_string())?;
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
        cancel: &ReadCancel,
    ) -> std::result::Result<ReadResponse, ReadError> {
        let (refreshed, facts) = {
            let latest = Self::lock(&self.latest);
            let l = latest
                .as_ref()
                .ok_or_else(|| ReadError::failed("no refresh has run"))?;
            (Arc::clone(&l.refreshed), Arc::clone(&l.facts))
        };
        let filter = filter_for(&req.scope, &facts)?;
        // The default scope found no project for the client's repository, so
        // this answer covers every project: the client says so (see
        // `take_widened_warning`).
        let widened = req.scope.project.is_none()
            && !req.scope.all_projects
            && req.scope.session.is_none()
            && req.scope.repo_root.is_some()
            && filter.project_id.is_none();
        let key = format!("{filter:?}");
        let view = {
            let existing = Self::lock(&self.views).get(&key).cloned();
            match existing {
                Some(v) => v,
                None => {
                    if cancel.is_cancelled() {
                        return Err(ReadError::cancelled());
                    }
                    // Before anything is decoded: how many rows the scope
                    // could make the daemon hold.
                    let rows = scope_rows(&refreshed, &facts, &filter);
                    if rows > self.view_limit {
                        return Err(ReadError::read_locally(format!(
                            "read locally: this scope covers about {rows} events and the daemon builds views of at most {} (it would grow by gigabytes and stall its writer); the client reads the database itself. Narrow the scope with --project, --session or --since to have the daemon answer it",
                            self.view_limit
                        )));
                    }
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
                    return Err("empty statement".to_string().into());
                }
                // The statement runs until the client goes away: dropping
                // the query future stops the plan (DataFusion's streams and
                // the tasks behind them are dropped with it), so a client
                // that timed out and read the database itself does not leave
                // this one scanning blobs for the next quarter of an hour.
                let result = rt
                    .block_on(async {
                        tokio::select! {
                            r = view.engine.query(&statement) => r.map_err(|e| e.to_string()),
                            _ = cancel.cancelled() => Err(String::new()),
                        }
                    })
                    .map_err(|e| {
                        if e.is_empty() {
                            ReadError::cancelled()
                        } else {
                            ReadError::failed(e)
                        }
                    })?;
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
        if widened {
            resp.notes.push(crate::ctx::WIDENED_WARNING.to_string());
        }
        Ok(resp)
    }
}

/// The most rows a view over `filter` could decode, from what is already
/// known (the manifest's per-segment statistics and the facts), without
/// reading a segment: the rows of every segment the scope can touch, held
/// down by what the facts say of the project or session asked for.
fn scope_rows(refreshed: &Refreshed, facts: &StreamFacts, filter: &ScanFilter) -> u64 {
    let mut rows: u64 = refreshed.memtable.len() as u64
        + refreshed
            .segments
            .iter()
            .filter(|s| s.may_match(filter))
            .map(|s| s.meta.rows)
            .sum::<u64>();
    let in_memtable = refreshed.memtable.len() as u64;
    if let Some(p) = filter.project_id
        && let Some(project) = facts.projects.get(&p)
    {
        rows = rows.min(project.events + in_memtable);
    }
    if let Some(s) = filter.session_id
        && let Some(session) = facts.session(&s)
    {
        rows = rows.min((session.captured + session.reconstructed) as u64 + in_memtable);
    }
    rows
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
    out.conflicts.clear();
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

/// One read from the daemon. When the daemon declines to build a view that
/// large (`read_locally`) the reason is said once on stderr, then `None`
/// like any other failure: the caller opens the database itself.
fn read_or_note(locator: &Locator, req: &ReadRequest) -> Option<ReadResponse> {
    match ipc::Client::read(locator, req) {
        Ok(resp) => Some(resp),
        Err(ipc::IpcError::Nack(n)) if n.code == ipc::READ_LOCALLY_CODE => {
            eprintln!("note: the daemon declined this read; {}", n.message);
            None
        }
        Err(_) => None,
    }
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
    let resp = read_or_note(locator, &req)?;
    let bytes = ipc::base64_decode(resp.arrow_ipc_base64.as_deref()?)?;
    let kind = match resp.result_kind.as_deref()? {
        "rows" => ResultKind::Rows,
        "explanation" => ResultKind::Explanation,
        _ => ResultKind::Empty,
    };
    QueryResult::from_ipc_bytes(&bytes, kind, take_widened_warning(resp.notes)).ok()
}

/// The daemon marks an answer that covers every project because the
/// client's repository is unknown with [`crate::ctx::WIDENED_WARNING`] among
/// the notes. That is a warning for stderr, as when the CLI reads the
/// database itself, not a line of the result.
fn take_widened_warning(notes: Vec<String>) -> Vec<String> {
    notes
        .into_iter()
        .filter(|n| {
            let widened = n == crate::ctx::WIDENED_WARNING;
            if widened {
                eprintln!("warning: {n}");
            }
            !widened
        })
        .collect()
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
    let resp = read_or_note(locator, &req)?;
    let p: Projection = serde_json::from_value(resp.projection?).ok()?;
    take_widened_warning(resp.notes);
    Some((p, resp.totals.unwrap_or_default(), resp.event_count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::event::Provider;
    use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
    use attemptdb_storage::OpenOptions;
    use std::time::{Duration, Instant};

    /// A flushed database with `n` tool events in each `(root, n)` project.
    fn database(dir: &std::path::Path, projects: &[(&str, usize)]) -> Database {
        let mut db = Database::open(
            dir,
            OpenOptions {
                create: true,
                ..Default::default()
            },
        )
        .unwrap();
        let device = DeviceId::new();
        for (root, n) in projects {
            let events: Vec<Event> = (0..*n)
                .map(|i| {
                    Event::new(
                        device,
                        Provider::ClaudeCode,
                        "PostToolUse",
                        EventKind::ToolCallFinished,
                        ProjectRef::derive(root, None, &device),
                        format!("{root}-session-{}", i / 50),
                        CaptureMode::LocalSemantic,
                        "read-service-test/0.1",
                    )
                })
                .collect();
            db.ingest(events).unwrap();
            db.flush().unwrap();
        }
        db
    }

    fn query(statement: &str, scope: ReadScope) -> ReadRequest {
        ReadRequest {
            kind: ReadKind::Query,
            statement: Some(statement.into()),
            scope,
            session_limit: None,
            all_sessions: false,
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn a_scope_over_the_limit_is_declined_with_the_reason_and_a_smaller_one_is_served() {
        let tmp = tempfile::tempdir().unwrap();
        let db = database(
            &tmp.path().join("db"),
            &[("/home/dev/big", 400), ("/home/dev/small", 30)],
        );
        let service = EngineService::with_view_limit(100);
        service.refresh(&db).unwrap();
        let rt = runtime();
        let ask = |scope: ReadScope| {
            service.handle(
                query("SELECT count(*) AS n FROM events", scope),
                rt.handle(),
                &ReadCancel::never(),
            )
        };

        // The whole history: 430 events against a limit of 100.
        let err = ask(ReadScope {
            all_projects: true,
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(err.code, ipc::READ_LOCALLY_CODE);
        assert!(
            err.message.contains("read locally")
                && err.message.contains("430")
                && err.message.contains("--project"),
            "{}",
            err.message
        );
        // So is the big project on its own.
        let err = ask(ReadScope {
            project: Some("big".into()),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(err.code, ipc::READ_LOCALLY_CODE, "{}", err.message);
        // A scope that fits is answered.
        let ok = ask(ReadScope {
            project: Some("small".into()),
            ..Default::default()
        })
        .expect("a small scope is served");
        assert_eq!(ok.event_count, 30);
        assert_eq!(ok.result_kind.as_deref(), Some("rows"));

        // The same service with the real limit serves the whole history.
        let roomy = EngineService::new();
        roomy.refresh(&db).unwrap();
        let ok = roomy
            .handle(
                query(
                    "SELECT count(*) AS n FROM events",
                    ReadScope {
                        all_projects: true,
                        ..Default::default()
                    },
                ),
                rt.handle(),
                &ReadCancel::never(),
            )
            .expect("under the real limit");
        assert_eq!(ok.event_count, 430);
        // A limit a real project fits in.
        const { assert!(VIEW_LIMIT_ROWS >= 100_000) };
    }

    #[test]
    fn a_cancelled_statement_stops_instead_of_running_to_the_end() {
        let tmp = tempfile::tempdir().unwrap();
        let db = database(&tmp.path().join("db"), &[("/home/dev/p", 1200)]);
        let service = Arc::new(EngineService::new());
        service.refresh(&db).unwrap();
        let rt = runtime();

        // Four billion rows through a filter DataFusion cannot answer from
        // statistics: minutes of work if nobody stops it.
        let (stop, cancel) = ReadCancel::channel();
        let handle = rt.handle().clone();
        let svc = Arc::clone(&service);
        let worker = std::thread::spawn(move || {
            let began = Instant::now();
            let r = svc.handle(
                query(
                    "SELECT count(*) FROM generate_series(1, 4000000000) AS g(v) WHERE v % 3 = 1",
                    ReadScope {
                        all_projects: true,
                        ..Default::default()
                    },
                ),
                &handle,
                &cancel,
            );
            (r, began.elapsed())
        });
        std::thread::sleep(Duration::from_millis(400));
        if worker.is_finished() {
            let (r, took) = worker.join().unwrap();
            panic!(
                "the statement ended by itself after {took:?}: {:?}",
                r.map(|r| r.event_count)
            );
        }
        let raised = Instant::now();
        stop.send(true).unwrap();
        let (result, _) = worker.join().unwrap();
        let err = result.expect_err("a cancelled read has no answer");
        assert_eq!(err.code, "cancelled", "{}", err.message);
        assert!(
            raised.elapsed() < Duration::from_secs(5),
            "stopped {:?} after the cancel",
            raised.elapsed()
        );

        // Nothing is left half-built: the next read is served.
        let ok = service
            .handle(
                query(
                    "SELECT count(*) AS n FROM events",
                    ReadScope {
                        all_projects: true,
                        ..Default::default()
                    },
                ),
                rt.handle(),
                &ReadCancel::never(),
            )
            .unwrap();
        assert_eq!(ok.result_kind.as_deref(), Some("rows"));
    }
}
