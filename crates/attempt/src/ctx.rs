//! Shared command context: locate the database, open it (importing pending
//! spool data), or open a snapshot read-only.

use crate::cli::{Cli, ScopeArgs};
use anyhow::{Context, Result};
use attemptdb_capture::{Config, Locator, ingest};
use attemptdb_core::{ProjectId, SessionId, Timestamp};
use attemptdb_storage::{Database, IngestReport, ScanFilter, snapshot};
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
            let (db, dir) = snapshot::open_read_only(file, &self.locator.snapshot_cache_dir())
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
    pub fn filter(&self, scope: &ScopeArgs, db: &Database) -> Result<ScanFilter> {
        let mut f = ScanFilter::default();
        if let Some(p) = &scope.project {
            f.project_id = Some(resolve_project(db, p)?);
        } else if !scope.all_projects {
            f.project_id = current_project(db, &self.cwd);
        }
        if let Some(s) = &scope.session {
            f.session_id = Some(resolve_session(db, s)?);
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

/// Resolve `--project`: a `prj_` id, a project name, or a path.
pub fn resolve_project(db: &Database, spec: &str) -> Result<ProjectId> {
    let all = db.scan(&ScanFilter::default())?;
    if let Ok(id) = spec.parse::<ProjectId>()
        && all.iter().any(|ev| ev.project.project_id == id)
    {
        return Ok(id);
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
    let spec_norm = attemptdb_core::PortablePath::from_raw(spec, None).logical;
    if let Some(c) = candidates
        .iter()
        .find(|c| c.1 == spec || c.2 == spec_norm || c.1.ends_with(&format!("/{spec}")))
    {
        return Ok(c.0);
    }
    let names: Vec<String> = candidates
        .iter()
        .map(|c| format!("{} ({})", c.1, c.0.short()))
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

/// The project of the repository containing `cwd`, if the database knows it.
pub fn current_project(db: &Database, cwd: &std::path::Path) -> Option<ProjectId> {
    let git = attemptdb_capture::git::git_info(cwd)?;
    let root = attemptdb_core::PortablePath::from_raw(&git.root.to_string_lossy(), None).logical;
    let remote = git
        .remote
        .as_deref()
        .and_then(attemptdb_core::event::normalise_remote);
    let mut best: Option<ProjectId> = None;
    for ev in db
        .scan(&ScanFilter {
            limit: Some(50_000),
            ..Default::default()
        })
        .ok()?
    {
        if remote.is_some() && ev.project.repo_remote == remote {
            return Some(ev.project.project_id);
        }
        if ev.project.root == root {
            best = Some(ev.project.project_id);
        }
    }
    best
}

pub fn resolve_session(db: &Database, spec: &str) -> Result<SessionId> {
    // A bare UUID may be a canonical `ses_` id or a provider session id
    // (Claude Code session ids are UUIDs too), so check what the data says.
    let canonical = spec.parse::<SessionId>().ok();
    let events = db.scan(&ScanFilter::default())?;
    if let Some(id) = canonical
        && events.iter().any(|ev| ev.session_id == id)
    {
        return Ok(id);
    }
    let needle = spec.trim_start_matches("ses_");
    for ev in &events {
        if ev.provider_session_id == spec
            || ev.session_id.short() == spec
            || ev.session_id.to_string().starts_with(needle)
            || ev.provider_session_id.starts_with(spec)
        {
            return Ok(ev.session_id);
        }
    }
    anyhow::bail!("unknown session {spec:?}")
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
