//! One database per tenant.
//!
//! The engine's writer lock is exclusive, so a database has exactly one
//! open writer — here, one `Database` per tenant, shared through a mutex by
//! every request for that tenant. The registry keeps at most `max_open`
//! of them resident: the least recently used one is flushed and closed when
//! a new tenant needs a slot, and the sweeper closes any tenant idle for
//! longer than the configured window.
//!
//! Closing is safe at any point: acknowledged events are already in the
//! fsynced WAL, and a reopen replays it. Flushing first only turns the WAL
//! into segments so the next open is cheap.

use crate::engine::TenantCache;
use anyhow::{Context, Result, bail};
use attemptdb_core::DeviceId;
use attemptdb_query::CacheStats;
use attemptdb_storage::{CompactionPolicy, Database, OpenOptions};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A tenant name that is safe as a directory name: 1–64 characters of
/// `[A-Za-z0-9._-]`, not starting with a dot.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TenantId(String);

impl TenantId {
    pub fn parse(s: &str) -> Result<Self> {
        let ok = !s.is_empty()
            && s.len() <= 64
            && !s.starts_with('.')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-');
        if !ok {
            bail!("invalid tenant id {s:?}: 1-64 of [A-Za-z0-9._-], not starting with '.'");
        }
        Ok(Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

struct Slot {
    db: Arc<Mutex<Database>>,
    /// The read side's engine cache; leaves with the slot, so an evicted
    /// tenant never keeps a stale cache behind.
    cache: Arc<Mutex<TenantCache>>,
    last_used: Instant,
}

/// A resident tenant: its writer handle and its read cache. Holding one
/// keeps the tenant from being evicted.
pub struct Tenant {
    pub db: Arc<Mutex<Database>>,
    pub cache: Arc<Mutex<TenantCache>>,
}

pub struct Registry {
    root: PathBuf,
    max_open: usize,
    /// Applied when a tenant is flushed and closed; `None` never compacts.
    compaction: Option<CompactionPolicy>,
    inner: Mutex<HashMap<TenantId, Slot>>,
}

impl Registry {
    pub fn new(data_dir: &Path, max_open: usize) -> Result<Self> {
        let root = data_dir.join("tenants");
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Self {
            root,
            max_open: max_open.max(1),
            compaction: Some(CompactionPolicy::default()),
            inner: Mutex::new(HashMap::new()),
        })
    }

    /// Compaction policy applied on close (`None` disables it).
    pub fn with_compaction(mut self, policy: Option<CompactionPolicy>) -> Self {
        self.compaction = policy;
        self
    }

    /// Where a tenant's database lives.
    pub fn dir(&self, tenant: &TenantId) -> PathBuf {
        self.root.join(tenant.as_str())
    }

    /// The tenant's open database, opening (and creating) it if needed.
    /// Blocking: call from a blocking task.
    pub fn open(&self, tenant: &TenantId) -> Result<Arc<Mutex<Database>>> {
        self.open_tenant(tenant).map(|t| t.db)
    }

    /// The tenant's database and read cache together (the read path).
    pub fn open_tenant(&self, tenant: &TenantId) -> Result<Tenant> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("registry poisoned"))?;
        if let Some(slot) = map.get_mut(tenant) {
            slot.last_used = Instant::now();
            return Ok(Tenant {
                db: Arc::clone(&slot.db),
                cache: Arc::clone(&slot.cache),
            });
        }
        while map.len() >= self.max_open {
            // Evict the least recently used tenant that no request is
            // holding. A handle still in use keeps the writer lock; opening
            // a second one for the same tenant would fail with `Locked`.
            let victim = map
                .iter()
                .filter(|(_, s)| Arc::strong_count(&s.db) == 1)
                .min_by_key(|(_, s)| s.last_used)
                .map(|(id, _)| id.clone());
            match victim {
                Some(id) => {
                    let slot = map.remove(&id).expect("victim present");
                    close(&id, slot.db, self.compaction.as_ref());
                }
                None => break, // everything is in flight; exceed the cap briefly
            }
        }
        let dir = self.dir(tenant);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let db = Database::open(
            &dir,
            OpenOptions {
                create: true,
                device_id: Some(DeviceId::derive(&["attemptdb-server", tenant.as_str()])),
                ..Default::default()
            },
        )
        .with_context(|| format!("opening tenant {tenant}"))?;
        let db = Arc::new(Mutex::new(db));
        let cache = Arc::new(Mutex::new(TenantCache::new()));
        map.insert(
            tenant.clone(),
            Slot {
                db: Arc::clone(&db),
                cache: Arc::clone(&cache),
                last_used: Instant::now(),
            },
        );
        Ok(Tenant { db, cache })
    }

    /// Names of the tenants currently resident.
    pub fn open_names(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|m| m.keys().map(|t| t.as_str().to_string()).collect())
            .unwrap_or_default()
    }

    pub fn open_count(&self) -> usize {
        self.inner.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// The read cache's counters for a resident tenant, plus how many
    /// sessions its last build re-projected; `None` when the tenant is not
    /// open (nothing is opened to answer).
    pub fn cache_stats(&self, tenant: &TenantId) -> Option<(CacheStats, usize)> {
        let map = self.inner.lock().ok()?;
        let slot = map.get(tenant)?;
        slot.cache
            .lock()
            .ok()
            .map(|c| (c.stats(), c.last_reprojected))
    }

    /// Flush and close every tenant idle for longer than `older_than` that
    /// no request is holding. Returns how many were closed.
    pub fn flush_idle(&self, older_than: Duration) -> usize {
        let Ok(mut map) = self.inner.lock() else {
            return 0;
        };
        let now = Instant::now();
        let idle: Vec<TenantId> = map
            .iter()
            .filter(|(_, s)| {
                now.duration_since(s.last_used) >= older_than && Arc::strong_count(&s.db) == 1
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &idle {
            if let Some(slot) = map.remove(id) {
                close(id, slot.db, self.compaction.as_ref());
            }
        }
        idle.len()
    }

    /// Flush every open tenant without closing it (shutdown, or a periodic
    /// durability point for busy tenants).
    pub fn flush_all(&self) {
        let Ok(map) = self.inner.lock() else {
            return;
        };
        for (id, slot) in map.iter() {
            if let Ok(mut db) = slot.db.lock()
                && let Err(e) = db.flush()
            {
                eprintln!("tenant {id}: flush failed: {e}");
            }
        }
    }
}

/// Most compaction steps on one close: a tenant that accumulated many small
/// segments is worked off over several idle sweeps, never in one long hold
/// of the registry lock.
const COMPACTION_STEPS_PER_CLOSE: usize = 4;

fn close(id: &TenantId, db: Arc<Mutex<Database>>, compaction: Option<&CompactionPolicy>) {
    if let Ok(mut db) = db.lock() {
        if let Err(e) = db.flush() {
            eprintln!("tenant {id}: flush before close failed: {e}");
        }
        if let Some(policy) = compaction {
            for _ in 0..COMPACTION_STEPS_PER_CLOSE {
                match db.compact(policy) {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("tenant {id}: compaction failed: {e}");
                        break;
                    }
                }
            }
        }
    }
    drop(db); // last reference: releases the writer lock
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_ids_are_directory_safe() {
        assert!(TenantId::parse("alpha").is_ok());
        assert!(TenantId::parse("user_01.acme-x").is_ok());
        assert!(TenantId::parse("").is_err());
        assert!(TenantId::parse(".hidden").is_err());
        assert!(TenantId::parse("..").is_err());
        assert!(TenantId::parse("a/b").is_err());
        assert!(TenantId::parse("a\\b").is_err());
        assert!(TenantId::parse(&"x".repeat(65)).is_err());
    }

    #[test]
    fn lru_evicts_unheld_tenants_only() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::new(tmp.path(), 1).unwrap();
        let a = TenantId::parse("a").unwrap();
        let b = TenantId::parse("b").unwrap();
        let held = reg.open(&a).unwrap();
        // `a` is held, so opening `b` exceeds the cap rather than evicting it.
        let _b = reg.open(&b).unwrap();
        assert_eq!(reg.open_count(), 2);
        drop(held);
        drop(_b);
        // Now a third tenant evicts the least recently used unheld one.
        let c = TenantId::parse("c").unwrap();
        let _c = reg.open(&c).unwrap();
        assert_eq!(reg.open_count(), 1);
        assert!(reg.dir(&a).join("MANIFEST").exists() || reg.dir(&a).exists());
        // Reopening an evicted tenant works: its lock was released.
        let _a = reg.open(&a).unwrap();
    }

    #[test]
    fn idle_sweep_compacts_small_segments_before_closing() {
        use attemptdb_core::event::Provider;
        use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::new(tmp.path(), 8)
            .unwrap()
            .with_compaction(Some(CompactionPolicy {
                max_segments: 2,
                small_segment_bytes: u64::MAX,
                min_inputs: 2,
            }));
        let a = TenantId::parse("a").unwrap();
        let dev = DeviceId::derive(&["tenants-test", "d"]);
        {
            let db = reg.open(&a).unwrap();
            let mut db = db.lock().unwrap();
            for i in 0..3 {
                let ev = Event::new(
                    dev,
                    Provider::ClaudeCode,
                    "PostToolUse",
                    EventKind::ToolCallFinished,
                    ProjectRef::derive("/home/dev/example/project", None, &dev),
                    format!("s{i}"),
                    CaptureMode::MetadataOnly,
                    "test/0",
                );
                db.ingest(vec![ev]).unwrap();
                db.flush().unwrap();
            }
            assert_eq!(db.stats().segments, 3);
        }
        assert_eq!(reg.flush_idle(Duration::ZERO), 1);
        let ro = Database::open(
            &reg.dir(&a),
            OpenOptions {
                read_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ro.stats().segments, 1, "three small segments became one");
        assert_eq!(
            ro.scan(&attemptdb_storage::ScanFilter::default())
                .unwrap()
                .len(),
            3
        );
        // Reopening through the registry still works (the lock was released).
        drop(reg.open(&a).unwrap());
    }

    #[test]
    fn idle_sweep_closes_and_reopen_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::new(tmp.path(), 8).unwrap();
        let a = TenantId::parse("a").unwrap();
        drop(reg.open(&a).unwrap());
        assert_eq!(reg.flush_idle(Duration::ZERO), 1);
        assert_eq!(reg.open_count(), 0);
        drop(reg.open(&a).unwrap());
    }

    #[test]
    fn the_read_cache_lives_and_dies_with_the_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::new(tmp.path(), 8).unwrap();
        let a = TenantId::parse("a").unwrap();
        assert!(reg.cache_stats(&a).is_none(), "not open: no cache");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        {
            let t = reg.open_tenant(&a).unwrap();
            let mut cache = t.cache.lock().unwrap();
            cache.view(&t.db, "a", rt.handle()).unwrap();
            assert_eq!(cache.rebuilds, 1);
        }
        assert_eq!(reg.cache_stats(&a).unwrap().0.refreshes, 1);
        // The same slot serves the same cache.
        {
            let t = reg.open_tenant(&a).unwrap();
            let mut cache = t.cache.lock().unwrap();
            cache.view(&t.db, "a", rt.handle()).unwrap();
            assert_eq!(cache.rebuilds, 1, "fingerprint unchanged: no rebuild");
        }
        // Evicted with the tenant; a reopen starts from an empty cache.
        assert_eq!(reg.flush_idle(Duration::ZERO), 1);
        assert!(reg.cache_stats(&a).is_none());
        let t = reg.open_tenant(&a).unwrap();
        assert_eq!(t.cache.lock().unwrap().rebuilds, 0);
        assert_eq!(reg.cache_stats(&a).unwrap().0.refreshes, 0);
    }
}
