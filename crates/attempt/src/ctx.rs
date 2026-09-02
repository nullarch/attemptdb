//! Shared command context: locate the database, open it (importing pending
//! spool data), or open a snapshot read-only.

use crate::cli::{Cli, ScopeArgs};
use anyhow::{Context, Result};
use attemptdb_capture::{Config, Locator, ingest};
use attemptdb_core::{ProjectId, SessionId, Timestamp};
use attemptdb_query::{EngineCache, QueryEngine, StreamFacts};
use attemptdb_storage::{Database, IngestReport, Refreshed, ScanFilter, snapshot};
use std::path::PathBuf;

pub struct Ctx {
    pub locator: Locator,
    pub config: Config,
    pub cwd: PathBuf,
}

impl Ctx {
    pub fn new(cli: &Cli) -> Result<Self> {
        let cwd = std::env::current_dir().context("reading current directory")?;
        let locator = Locator::resolve(&cwd, cli.data_dir.as_deref(), cli.db.as_deref());
        let config = Config::load_or_default(&locator.paths.config_dir);
        Ok(Self {
            locator,
            config,
            cwd,
        })
    }

    /// Open the database for reading (importing spool first when we can
    /// take the writer lock), or a snapshot when `--snapshot` was given.
    pub fn open(&self, cli: &Cli) -> Result<Opened> {
        if let Some(file) = &cli.snapshot {
            // A portable snapshot opens with the key file it was exported with;
            // otherwise the local database's own keys are tried (same-device backups).
            let keys = match &cli.key_file {
                Some(kf) => attemptdb_capture::keys::provider_with(
                    &self.locator,
                    uuid::Uuid::nil(),
                    attemptdb_capture::keys::KeyStoreOptions {
                        key_file: Some(kf.clone()),
                        use_keyring: false,
                        passphrase: None,
                    },
                ),
                None => {
                    attemptdb_capture::keys::provider_for_db(&self.locator, &self.locator.db_dir)
                }
            };
            let (db, dir) =
                snapshot::open_read_only_with(file, &self.locator.snapshot_cache_dir(), keys)
                    .with_context(|| format!("opening snapshot {}", file.display()))?;
            return Ok(Opened {
                db,
                import: None,
                read_only: true,
                source: format!("snapshot {} (cached at {})", file.display(), dir.display()),
            });
        }
        if !Database::exists(&self.locator.db_dir) {
            anyhow::bail!(
                "no database at {}\n  run `attempt init` first (or `attempt init --local` for a project-local database)",
                self.locator.db_dir.display()
            );
        }
        let (db, import, read_only) = ingest::open_fresh(&self.locator, false)?;
        Ok(Opened {
            db,
            import,
            read_only,
            source: self.locator.db_dir.display().to_string(),
        })
    }

    /// Build a scan filter from CLI scope flags, defaulting to the current
    /// repository when inside one and `--all-projects` is not given.
    /// `facts` is what the database's events say about projects and
    /// sessions ([`Loaded::facts`]).
    pub fn filter(&self, scope: &ScopeArgs, facts: &StreamFacts) -> Result<ScanFilter> {
        let mut f = ScanFilter::default();
        if let Some(p) = &scope.project {
            f.project_id = Some(resolve_project(facts, p)?);
        } else if !scope.all_projects {
            f.project_id = current_project(facts, &self.cwd);
        }
        if let Some(s) = &scope.session {
            f.session_id = Some(resolve_session(facts, s)?);
        }
        if let Some(t) = &scope.since {
            f.since = Some(parse_time(t).with_context(|| format!("cannot parse --since {t:?}"))?);
        }
        if let Some(t) = &scope.until {
            f.until = Some(parse_time(t).with_context(|| format!("cannot parse --until {t:?}"))?);
        }
        f.captured_only = scope.captured_only;
        Ok(f)
    }
}

pub struct Opened {
    pub db: Database,
    pub import: Option<IngestReport>,
    pub read_only: bool,
    pub source: String,
}

impl Opened {
    /// Refresh a throwaway engine cache over this database: the segments
    /// decoded to Arrow once (no blob opened), the facts to resolve a scope
    /// with, and the cache to build the engine from.
    pub fn load(&self) -> Result<Loaded> {
        let mut cache = EngineCache::new();
        let refreshed = cache
            .refresh(&self.db, &self.source)
            .context("reading the database")?;
        let facts = cache.facts(&refreshed);
        Ok(Loaded {
            cache,
            refreshed,
            facts,
        })
    }
}

/// One command's read of the database.
pub struct Loaded {
    pub cache: EngineCache,
    pub refreshed: Refreshed,
    pub facts: StreamFacts,
}

impl Loaded {
    /// The engine over `filter`'s scope.
    pub fn engine(&mut self, filter: &ScanFilter) -> Result<QueryEngine> {
        self.cache
            .engine_scoped(&self.refreshed, filter)
            .context("building the query engine")
    }
}

/// Resolve `--project`: a `prj_` id, a project name, or a path.
pub fn resolve_project(facts: &StreamFacts, spec: &str) -> Result<ProjectId> {
    match facts.resolve_project(spec) {
        Ok(id) => Ok(id),
        Err(known) => {
            let names: Vec<String> = known
                .iter()
                .map(|p| format!("{} ({})", p.name, p.project_id.short()))
                .collect();
            anyhow::bail!(
                "unknown project {spec:?}; known projects: {}",
                if names.is_empty() {
                    "none".into()
                } else {
                    names.join(", ")
                }
            )
        }
    }
}

/// The project of the repository containing `cwd`, if the database knows it.
pub fn current_project(facts: &StreamFacts, cwd: &std::path::Path) -> Option<ProjectId> {
    let git = attemptdb_capture::git::git_info(cwd)?;
    let root = attemptdb_core::PortablePath::from_raw(&git.root.to_string_lossy(), None).logical;
    let remote = git
        .remote
        .as_deref()
        .and_then(attemptdb_core::event::normalise_remote);
    facts.project_of(&root, remote.as_deref())
}

/// Resolve a session argument: a `ses_` id (full or short) or a provider
/// session id (Claude Code session ids are UUIDs too, so the data decides).
pub fn resolve_session(facts: &StreamFacts, spec: &str) -> Result<SessionId> {
    facts
        .resolve_session(spec)
        .ok_or_else(|| anyhow::anyhow!("unknown session {spec:?}"))
}

/// Absolute or relative time: RFC 3339, `YYYY-MM-DD`, epoch, `now`, `today`,
/// `yesterday`, or `-<n>(s|m|h|d|w)`.
pub fn parse_time(s: &str) -> Option<Timestamp> {
    let s = s.trim();
    let now = Timestamp::now();
    match s {
        "now" => return Some(now),
        "today" => {
            let d = chrono::Utc::now().date_naive();
            return Some(Timestamp::from_micros(
                d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_micros(),
            ));
        }
        "yesterday" => {
            let d = chrono::Utc::now().date_naive().pred_opt()?;
            return Some(Timestamp::from_micros(
                d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_micros(),
            ));
        }
        _ => {}
    }
    if let Some(rest) = s.strip_prefix('-') {
        let (num, unit) = rest.split_at(
            rest.trim_end_matches(|c: char| c.is_ascii_alphabetic())
                .len(),
        );
        let n: i64 = num.parse().ok()?;
        let secs = match unit {
            "s" => n,
            "m" | "min" => n * 60,
            "h" => n * 3600,
            "d" => n * 86_400,
            "w" => n * 7 * 86_400,
            _ => return None,
        };
        return Some(Timestamp::from_micros(now.as_micros() - secs * 1_000_000));
    }
    Timestamp::parse(s)
}
