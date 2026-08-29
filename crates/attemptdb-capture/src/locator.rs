//! Where is the database?
//!
//! ```text
//! data root  = --data-dir > $ATTEMPTDB_DATA_DIR > OS data dir
//! database   = --db / $ATTEMPTDB_DIR
//!            > nearest ancestor `.attemptdb/` of the working directory
//!              that contains an ATTEMPTDB identity file (project-local)
//!            > <data root>/db/.attemptdb (per-user default)
//! ```

use crate::platform::{AppPaths, app_paths};
use std::path::{Path, PathBuf};

pub const DB_DIR_ENV: &str = "ATTEMPTDB_DIR";
pub const LOCAL_DB_DIR_NAME: &str = ".attemptdb";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DbSource {
    /// `--db` flag or `ATTEMPTDB_DIR`.
    Explicit,
    /// A `.attemptdb/` directory found by walking up from the cwd.
    ProjectLocal,
    /// The per-user default under the data root.
    Default,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Locator {
    pub paths: AppPaths,
    pub db_dir: PathBuf,
    pub source: DbSource,
}

impl Locator {
    /// Resolve using the process environment and `cwd`.
    pub fn resolve(
        cwd: &Path,
        data_dir_override: Option<&Path>,
        db_override: Option<&Path>,
    ) -> Self {
        let paths = match data_dir_override {
            Some(root) => portable_paths(root),
            None => app_paths(),
        };
        if let Some(db) = db_override {
            return Self {
                paths,
                db_dir: db.to_path_buf(),
                source: DbSource::Explicit,
            };
        }
        if let Some(db) = std::env::var_os(DB_DIR_ENV).filter(|v| !v.is_empty()) {
            return Self {
                paths,
                db_dir: PathBuf::from(db),
                source: DbSource::Explicit,
            };
        }
        if let Some(local) = find_project_local(cwd) {
            return Self {
                paths,
                db_dir: local,
                source: DbSource::ProjectLocal,
            };
        }
        let db_dir = default_db_dir(&paths);
        Self {
            paths,
            db_dir,
            source: DbSource::Default,
        }
    }

    pub fn snapshot_cache_dir(&self) -> PathBuf {
        self.paths.cache_dir.join("snapshots")
    }
}

pub fn default_db_dir(paths: &AppPaths) -> PathBuf {
    paths.data_dir.join("db").join(LOCAL_DB_DIR_NAME)
}

fn portable_paths(root: &Path) -> AppPaths {
    AppPaths {
        data_dir: root.to_path_buf(),
        config_dir: root.join("config"),
        cache_dir: root.join("cache"),
        runtime_dir: root.join("run"),
        log_dir: root.join("logs"),
    }
}

/// Walk up from `cwd` looking for an initialised project-local database.
pub fn find_project_local(cwd: &Path) -> Option<PathBuf> {
    let mut dir = Some(cwd);
    let mut depth = 0;
    while let Some(d) = dir {
        let candidate = d.join(LOCAL_DB_DIR_NAME);
        if attemptdb_storage::Database::exists(&candidate) {
            return Some(candidate);
        }
        depth += 1;
        if depth > 64 {
            break;
        }
        dir = d.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_beats_local_beats_default() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = root.join("proj");
        let nested = project.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        // Without a local db → default under the (portable) data root.
        let l = Locator::resolve(&nested, Some(&root.join("data")), None);
        assert_eq!(l.source, DbSource::Default);
        assert_eq!(l.db_dir, root.join("data").join("db").join(".attemptdb"));
        // Create a project-local db → found from a nested cwd.
        attemptdb_storage::Database::create(
            &project.join(".attemptdb"),
            attemptdb_core::DeviceId::new(),
        )
        .unwrap();
        let l = Locator::resolve(&nested, Some(&root.join("data")), None);
        assert_eq!(l.source, DbSource::ProjectLocal);
        assert_eq!(l.db_dir, project.join(".attemptdb"));
        // Explicit override wins.
        let l = Locator::resolve(&nested, Some(&root.join("data")), Some(&root.join("x")));
        assert_eq!(l.source, DbSource::Explicit);
    }
}
