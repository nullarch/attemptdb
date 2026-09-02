//! Per-tenant engine cache: the read side's one moving part.
//!
//! A tenant's database is written by the ingest path and read by the read
//! API through the same `Database` handle (the writer's exclusive lock is
//! the tenancy model, so there is exactly one). Every read goes through a
//! [`TenantCache`] that lives in the tenant's registry slot: it keeps the
//! decoded segments and the incremental projection
//! (`attemptdb_query::EngineCache`, shared with the local UI and the MCP
//! server) and the last built [`TenantView`].
//!
//! A view is keyed by a [`Fingerprint`] of the handle's manifest generation,
//! segment count and memtable size — the WAL/manifest state, read from
//! memory, no I/O. When it matches, the view is served as is. When it does
//! not, the refresh (decode newly listed segments, read the WAL) runs under
//! the database lock and everything else — re-projecting dirty sessions,
//! building the DataFusion engine — runs after the lock is released, so
//! ingest is blocked for the refresh only.
//!
//! The view is the same engine the local UI builds for the same events:
//! `QueryEngine::from_parts` over cached batches and the incremental
//! projection's snapshot. The numbers a dashboard reads here are the
//! numbers `attempt ui` shows on the device.

use crate::merge::{DeviceInferences, InferenceFingerprint};
use anyhow::{Context, Result};
use attemptdb_core::event::normalise_remote;
use attemptdb_core::{DeviceId, Event, ProjectId, SessionId, Timestamp};
use attemptdb_query::{CacheStats, EngineCache, QueryEngine};
use attemptdb_storage::db::DbStats;
use attemptdb_storage::{Database, Refreshed};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The WAL/manifest state of a writer handle. Ingest grows the memtable,
/// a flush bumps the generation (and empties the memtable), repair changes
/// the segment list: together they change whenever the content does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub generation: u64,
    pub segments: usize,
    pub memtable_rows: usize,
}

impl Fingerprint {
    pub fn of(db: &Database) -> Self {
        let m = db.manifest();
        Self {
            generation: m.generation,
            segments: m.segments.len(),
            memtable_rows: db.memtable_events().len(),
        }
    }
}

/// One project as the tenant's events describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectInfo {
    pub project_id: ProjectId,
    pub name: String,
    pub root: String,
    pub repo_remote: Option<String>,
    pub events: u64,
    pub sessions: u64,
}

/// Facts about one session that the projection does not carry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionFacts {
    /// The device that wrote the session's first event.
    pub device_id: DeviceId,
    /// Hook-captured versus transcript-reconstructed events.
    pub captured: usize,
    pub reconstructed: usize,
}

/// One provider's share of the tenant's events.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderInfo {
    pub provider: String,
    pub events: u64,
    /// Latest `observed_at`, capture tests excluded.
    pub last_event_at: Option<Timestamp>,
}

/// A built engine over one tenant's whole database, plus the facts around
/// it, immutable once built and shared by every request until the
/// fingerprint changes.
pub struct TenantView {
    pub engine: QueryEngine,
    /// Every event: the cached segments (shared with the cache) and a copy
    /// of the WAL, for `/v1/events` and evidence lookups.
    pub refreshed: Refreshed,
    pub projects: Vec<ProjectInfo>,
    pub providers: Vec<ProviderInfo>,
    pub sessions: HashMap<SessionId, SessionFacts>,
    pub last_event_at: Option<Timestamp>,
    pub fingerprint: Fingerprint,
    pub stats: DbStats,
    pub built_at: Timestamp,
}

impl TenantView {
    pub fn event_count(&self) -> usize {
        self.refreshed.event_count()
    }

    /// Resolve a `project` argument: a `prj_` id (or bare uuid), a
    /// normalised remote (`host/owner/repo`, in any spelling
    /// `normalise_remote` accepts), a project name, or a logical root.
    pub fn resolve_project(&self, spec: &str) -> std::result::Result<ProjectId, String> {
        resolve_project(&self.projects, spec)
    }
}

/// The per-tenant cache: engine state, the last view, and the device
/// inference documents (keyed by their own file fingerprint, since an
/// inference upload does not touch the event database).
#[derive(Default)]
pub struct TenantCache {
    engine: EngineCache,
    view: Option<Arc<TenantView>>,
    inferences: Option<(InferenceFingerprint, Arc<DeviceInferences>)>,
    /// Views built over the cache's lifetime (tests read it).
    pub rebuilds: u64,
    /// Sessions the last build re-projected: the ones new events touched.
    pub last_reprojected: usize,
}

impl TenantCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> CacheStats {
        self.engine.stats()
    }

    /// The view of `db`, reused while the fingerprint is unchanged. Blocking
    /// (call from a blocking task); `handle` drives the engine build.
    pub fn view(
        &mut self,
        db: &Mutex<Database>,
        source: &str,
        handle: &tokio::runtime::Handle,
    ) -> Result<Arc<TenantView>> {
        // Under the database lock: the fingerprint and, when stale, the
        // refresh (decode new segments, copy the WAL). Nothing else.
        let (fingerprint, refreshed, stats) = {
            let db = db
                .lock()
                .map_err(|_| anyhow::anyhow!("tenant database poisoned"))?;
            let fingerprint = Fingerprint::of(&db);
            if let Some(v) = &self.view
                && v.fingerprint == fingerprint
                && self.engine.source() == source
            {
                return Ok(Arc::clone(v));
            }
            let refreshed = self
                .engine
                .refresh(&db, source)
                .context("refreshing the tenant's engine cache")?;
            (fingerprint, refreshed, db.stats())
        };
        // Lock released: project the dirty sessions, build the engine. The
        // segments' derived parts are shared with the cache; only the WAL's
        // are built here.
        self.last_reprojected = self.engine.stats().pending_sessions;
        let engine = self
            .engine
            .engine(&refreshed)
            .context("building the query engine")?;
        let _ = handle;
        let facts = Facts::from_stream(engine.facts());
        let view = Arc::new(TenantView {
            engine,
            refreshed,
            projects: facts.projects,
            providers: facts.providers,
            sessions: facts.sessions,
            last_event_at: facts.last_event_at,
            fingerprint,
            stats,
            built_at: Timestamp::now(),
        });
        self.view = Some(Arc::clone(&view));
        self.rebuilds += 1;
        Ok(view)
    }

    /// The tenant's device-uploaded inferences, re-read only when a
    /// document under `<tenant>/inferences/` changed.
    pub fn inferences(&mut self, tenant_dir: &Path) -> Result<Arc<DeviceInferences>> {
        let fp = crate::merge::fingerprint(tenant_dir);
        if let Some((cached, docs)) = &self.inferences
            && *cached == fp
        {
            return Ok(Arc::clone(docs));
        }
        let docs = Arc::new(DeviceInferences::load(tenant_dir)?);
        self.inferences = Some((fp, Arc::clone(&docs)));
        Ok(docs)
    }
}

#[derive(Default)]
struct Facts {
    projects: Vec<ProjectInfo>,
    providers: Vec<ProviderInfo>,
    sessions: HashMap<SessionId, SessionFacts>,
    last_event_at: Option<Timestamp>,
}

pub fn is_reconstructed(ev: &Event) -> bool {
    ev.attrs
        .get("reconstructed")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

impl Facts {
    /// The view's facts from the engine's merged stream facts: a pass over
    /// projects, providers and sessions, never over events.
    fn from_stream(f: &attemptdb_query::StreamFacts) -> Self {
        Facts {
            projects: f
                .projects
                .values()
                .map(|p| ProjectInfo {
                    project_id: p.project_id,
                    name: p.name.clone(),
                    root: p.root.clone(),
                    repo_remote: p.repo_remote.clone(),
                    events: p.events,
                    sessions: p.sessions.len() as u64,
                })
                .collect(),
            providers: f
                .providers
                .values()
                .map(|p| ProviderInfo {
                    provider: p.provider.clone(),
                    events: p.events,
                    last_event_at: p.last_event_at,
                })
                .collect(),
            sessions: f
                .sessions
                .iter()
                .map(|(sid, sf)| {
                    (
                        *sid,
                        SessionFacts {
                            device_id: sf.device_id,
                            captured: sf.captured,
                            reconstructed: sf.reconstructed,
                        },
                    )
                })
                .collect(),
            last_event_at: f.last_event_at,
        }
    }
}

/// One pass over the events: projects, providers, per-session facts.
#[cfg(test)]
fn summarize<'a>(events: impl Iterator<Item = &'a Event>) -> Facts {
    let events: Vec<&Event> = events.collect();
    Facts::from_stream(&attemptdb_query::StreamFacts::from_events(events))
}

/// See [`TenantView::resolve_project`].
pub fn resolve_project(
    projects: &[ProjectInfo],
    spec: &str,
) -> std::result::Result<ProjectId, String> {
    let spec = spec.trim();
    if let Ok(pid) = spec.parse::<ProjectId>()
        && let Some(p) = projects.iter().find(|p| p.project_id == pid)
    {
        return Ok(p.project_id);
    }
    let remote = normalise_remote(spec);
    if let Some(p) = projects.iter().find(|p| {
        p.repo_remote.as_deref().is_some_and(|r| {
            r.eq_ignore_ascii_case(spec) || remote.as_deref().is_some_and(|n| n == r)
        })
    }) {
        return Ok(p.project_id);
    }
    if let Some(p) = projects
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(spec) || p.root == spec)
    {
        return Ok(p.project_id);
    }
    let known: Vec<String> = projects
        .iter()
        .map(|p| format!("{} (prj_{})", p.name, p.project_id))
        .collect();
    Err(format!(
        "unknown project {spec:?}; known projects: {}",
        if known.is_empty() {
            "none".to_string()
        } else {
            known.join(", ")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::event::Provider;
    use attemptdb_core::{CaptureMode, EventKind, ProjectRef};
    use attemptdb_storage::OpenOptions;

    fn event(dev: DeviceId, project: &ProjectRef, session: &str) -> Event {
        Event::new(
            dev,
            Provider::ClaudeCode,
            "PostToolUse",
            EventKind::ToolCallFinished,
            project.clone(),
            session,
            CaptureMode::MetadataOnly,
            "engine-test/0",
        )
    }

    #[test]
    fn projects_resolve_by_id_remote_name_and_root() {
        let dev = DeviceId::derive(&["engine-test"]);
        let acme = ProjectRef::derive("/home/dev/acme", Some("git@github.com:acme/repo.git"), &dev);
        let local = ProjectRef::derive("/home/dev/scratch", None, &dev);
        let facts = summarize(
            [
                event(dev, &acme, "s1"),
                event(dev, &acme, "s2"),
                event(dev, &local, "s3"),
            ]
            .iter(),
        );
        assert_eq!(facts.projects.len(), 2);
        let p = &facts.projects;
        assert_eq!(
            resolve_project(p, &format!("prj_{}", acme.project_id)),
            Ok(acme.project_id)
        );
        assert_eq!(
            resolve_project(p, &acme.project_id.to_string()),
            Ok(acme.project_id)
        );
        assert_eq!(
            resolve_project(p, "github.com/acme/repo"),
            Ok(acme.project_id)
        );
        assert_eq!(
            resolve_project(p, "https://github.com/acme/repo.git"),
            Ok(acme.project_id)
        );
        assert_eq!(resolve_project(p, "ACME/REPO"), Ok(acme.project_id));
        assert_eq!(resolve_project(p, "scratch"), Ok(local.project_id));
        assert_eq!(resolve_project(p, &local.root), Ok(local.project_id));
        let err = resolve_project(p, "nowhere").unwrap_err();
        assert!(err.contains("unknown project") && err.contains("acme/repo"));
        assert_eq!(facts.sessions.len(), 3);
        assert_eq!(
            facts.sessions[&SessionId::derive(&["claude_code", "s1"])].device_id,
            dev
        );
        assert_eq!(p[0].sessions + p[1].sessions, 3);
    }

    #[test]
    fn views_are_reused_until_the_fingerprint_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = DeviceId::derive(&["engine-test"]);
        let project = ProjectRef::derive("/home/dev/acme", None, &dev);
        let db = Database::open(
            tmp.path(),
            OpenOptions {
                create: true,
                device_id: Some(dev),
                ..Default::default()
            },
        )
        .unwrap();
        let db = Mutex::new(db);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let handle = rt.handle().clone();
        let mut cache = TenantCache::new();

        let v0 = cache.view(&db, "t", &handle).unwrap();
        assert_eq!(v0.event_count(), 0);
        assert!(Arc::ptr_eq(&v0, &cache.view(&db, "t", &handle).unwrap()));

        db.lock()
            .unwrap()
            .ingest(vec![event(dev, &project, "s1"), event(dev, &project, "s2")])
            .unwrap();
        let v1 = cache.view(&db, "t", &handle).unwrap();
        assert!(!Arc::ptr_eq(&v0, &v1));
        assert_eq!(v1.event_count(), 2);
        assert_eq!(v1.engine.projection().sessions.len(), 2);
        assert_eq!(cache.rebuilds, 2);
        assert_eq!(cache.last_reprojected, 2, "both new sessions");
        assert_eq!(cache.stats().decodes, 0, "WAL only: nothing decoded");

        db.lock().unwrap().flush().unwrap();
        let v2 = cache.view(&db, "t", &handle).unwrap();
        assert_ne!(v1.fingerprint, v2.fingerprint);
        assert_eq!(v2.event_count(), 2);
        let s = cache.stats();
        assert_eq!((s.decodes, s.events), (1, 2));
        assert_eq!(
            cache.last_reprojected, 0,
            "events that moved from the WAL into a segment are not re-projected"
        );
    }
}
