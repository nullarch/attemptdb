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

    /// Open the database for reading, or a snapshot when `--snapshot` was
    /// given. Whatever the hooks spooled is imported first when the writer
    /// lock is free, and the lock is let go before this returns: the handle
    /// is read-only and holds nothing, so a long read does not keep the
    /// daemon from starting or a second CLI from importing.
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
                reopen: None,
            });
        }
        if !Database::exists(&self.locator.db_dir) {
            anyhow::bail!(
                "no database at {}\n  run `attempt init` first (or `attempt init --local` for a project-local database)",
                self.locator.db_dir.display()
            );
        }
        let (db, import, writer_busy) = ingest::open_for_read(&self.locator)?;
        Ok(Opened {
            db,
            import,
            read_only: writer_busy,
            source: self.locator.db_dir.display().to_string(),
            reopen: Some(self.locator.clone()),
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
    /// Another process holds the writer lock (or this is a snapshot): what
    /// was spooled has not been imported by this command.
    pub read_only: bool,
    pub source: String,
    /// Where to open the database again when a segment this handle lists is
    /// deleted by a compaction mid-read; `None` for a snapshot.
    reopen: Option<Locator>,
}

/// A fresh read-only handle on the database at `locator`: a newer manifest.
fn reopener(locator: Option<Locator>) -> impl FnMut() -> attemptdb_query::Result<Database> {
    move || match &locator {
        Some(l) => ingest::open_reader(l)
            .map_err(|e| attemptdb_query::QueryError::Exec(format!("reopening the database: {e}"))),
        None => Err(attemptdb_query::QueryError::Exec(
            "a snapshot cannot change underneath a read".into(),
        )),
    }
}

impl Opened {
    /// Everything counts and scope resolution need — events and sessions per
    /// provider and project, last seen, telemetry receipts — and nothing
    /// else. Each segment is read for the handful of columns facts are made
    /// of ([`attemptdb_query::facts::FACT_COLUMNS`]); no event is decoded, no
    /// projection built, and nothing but the facts is kept, so this costs a
    /// fraction of a full load in time and memory on a large database.
    pub fn facts(&self) -> Result<StreamFacts> {
        Ok(self.load()?.facts)
    }

    /// List the database's segments, read their facts, and hold the listing
    /// to build an engine from ([`Loaded::engine`]) or scan events
    /// (`Loaded::refreshed`). No segment is decoded in full here; a scope
    /// decodes only its own rows.
    pub fn load(&self) -> Result<Loaded> {
        let mut cache = EngineCache::new();
        let mut refreshed = cache
            .refresh_lazy(&self.db, &self.source)
            .context("reading the database")?;
        let mut reopen = reopener(self.reopen.clone());
        let facts = cache
            .retrying(&mut refreshed, &mut reopen, |c, r| c.facts(r))
            .context("reading the database")?;
        Ok(Loaded {
            cache,
            refreshed,
            facts,
            reopen: self.reopen.clone(),
        })
    }
}

/// One command's read of the database.
pub struct Loaded {
    pub cache: EngineCache,
    pub refreshed: Refreshed,
    pub facts: StreamFacts,
    reopen: Option<Locator>,
}

impl Loaded {
    /// The engine over `filter`'s scope: the scope's rows are read and
    /// projected, nothing else is. If a compaction deleted a segment since
    /// the listing was taken, the listing is renewed from a fresh manifest
    /// and the read repeated.
    pub fn engine(&mut self, filter: &ScanFilter) -> Result<QueryEngine> {
        let mut reopen = reopener(self.reopen.clone());
        self.cache
            .retrying(&mut self.refreshed, &mut reopen, |c, r| {
                c.engine_scoped(r, filter)
            })
            .context("building the query engine")
    }
}

/// Resolve `--project`: a `prj_` id, a project name, or a path. A name that
/// fits several projects is an error that lists them.
pub fn resolve_project(facts: &StreamFacts, spec: &str) -> Result<ProjectId> {
    facts
        .resolve_project(spec)
        .map_err(|e| anyhow::anyhow!("{e}"))
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

/// Resolve a session argument: a `ses_` id or provider session id, in full or
/// as a prefix of at least a few characters. A prefix that fits several
/// sessions, or is too short to mean anything, is an error that says so.
pub fn resolve_session(facts: &StreamFacts, spec: &str) -> Result<SessionId> {
    facts
        .resolve_session(spec)
        .map_err(|e| anyhow::anyhow!("{e}"))
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
