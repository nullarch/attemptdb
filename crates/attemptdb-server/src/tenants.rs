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

use anyhow::{Context, Result, bail};
use attemptdb_core::DeviceId;
use attemptdb_storage::{Database, OpenOptions};
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
    last_used: Instant,
}

pub struct Registry {
    root: PathBuf,
    max_open: usize,
    inner: Mutex<HashMap<TenantId, Slot>>,
}

impl Registry {
    pub fn new(data_dir: &Path, max_open: usize) -> Result<Self> {
        let root = data_dir.join("tenants");
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Self {
            root,
            max_open: max_open.max(1),
            inner: Mutex::new(HashMap::new()),
        })
    }

    /// Where a tenant's database lives.
    pub fn dir(&self, tenant: &TenantId) -> PathBuf {
        self.root.join(tenant.as_str())
    }

    /// The tenant's open database, opening (and creating) it if needed.
    /// Blocking: call from a blocking task.
    pub fn open(&self, tenant: &TenantId) -> Result<Arc<Mutex<Database>>> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("registry poisoned"))?;
        if let Some(slot) = map.get_mut(tenant) {
            slot.last_used = Instant::now();
            return Ok(Arc::clone(&slot.db));
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
                    close(&id, slot.db);
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
        map.insert(
            tenant.clone(),
            Slot {
                db: Arc::clone(&db),
                last_used: Instant::now(),
            },
        );
        Ok(db)
    }

    pub fn open_count(&self) -> usize {
        self.inner.lock().map(|m| m.len()).unwrap_or(0)
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
                close(id, slot.db);
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

fn close(id: &TenantId, db: Arc<Mutex<Database>>) {
    if let Ok(mut db) = db.lock()
        && let Err(e) = db.flush()
    {
        eprintln!("tenant {id}: flush before close failed: {e}");
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
    fn idle_sweep_closes_and_reopen_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::new(tmp.path(), 8).unwrap();
        let a = TenantId::parse("a").unwrap();
        drop(reg.open(&a).unwrap());
        assert_eq!(reg.flush_idle(Duration::ZERO), 1);
        assert_eq!(reg.open_count(), 0);
        drop(reg.open(&a).unwrap());
    }
}
