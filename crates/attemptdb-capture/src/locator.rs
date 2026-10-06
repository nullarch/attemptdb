//! Where is the database?
//!
//! ```text
//! data root  = --data-dir > $ATTEMPTDB_DATA_DIR > OS data dir
//! database   = --db / $ATTEMPTDB_DIR
//!            > nearest ancestor `.attemptdb/` of the working directory
//!              that contains an ATTEMPTDB identity file and that this user
//!              can trust (project-local)
//!            > <data root>/db/.attemptdb (per-user default)
//! ```
//!
//! A project-local database is found by walking up from the working
//! directory of whatever agent session fired the hook, so a repository one
//! has just cloned decides where the hook writes. Its `.attemptdb/` is
//! therefore trusted only when it is plausibly one this user made: see
//! [`trust_problem`]. Anything else is ignored (the user's own database is
//! used instead) and remembered in [`Locator::ignored_local`] for
//! `attempt doctor`.

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
    /// Project-local databases found on the way up from the working
    /// directory and not used, with the reason.
    pub ignored_local: Vec<IgnoredLocalDb>,
}

/// A `.attemptdb/` the locator walked past because it could not be trusted.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct IgnoredLocalDb {
    pub path: PathBuf,
    pub reason: String,
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
                ignored_local: Vec::new(),
            };
        }
        if let Some(db) = std::env::var_os(DB_DIR_ENV).filter(|v| !v.is_empty()) {
            return Self {
                paths,
                db_dir: PathBuf::from(db),
                source: DbSource::Explicit,
                ignored_local: Vec::new(),
            };
        }
        let (local, ignored_local) = scan_project_local(cwd);
        if let Some(local) = local {
            return Self {
                paths,
                db_dir: local,
                source: DbSource::ProjectLocal,
                ignored_local,
            };
        }
        let db_dir = default_db_dir(&paths);
        Self {
            paths,
            db_dir,
            source: DbSource::Default,
            ignored_local,
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

/// Walk up from `cwd` looking for an initialised, trusted project-local
/// database.
pub fn find_project_local(cwd: &Path) -> Option<PathBuf> {
    scan_project_local(cwd).0
}

/// [`find_project_local`], plus every `.attemptdb/` that was passed over
/// because [`trust_problem`] found something wrong with it.
pub fn scan_project_local(cwd: &Path) -> (Option<PathBuf>, Vec<IgnoredLocalDb>) {
    let mut ignored = Vec::new();
    let mut dir = Some(cwd);
    let mut depth = 0;
    while let Some(d) = dir {
        let candidate = d.join(LOCAL_DB_DIR_NAME);
        // One `lstat` per ancestor decides whether there is anything here.
        if std::fs::symlink_metadata(&candidate).is_ok() {
            match trust_problem(&candidate) {
                None => return (Some(candidate), ignored),
                Some(Problem::NotADatabase) => {}
                Some(Problem::Untrusted(reason)) => ignored.push(IgnoredLocalDb {
                    path: candidate,
                    reason,
                }),
            }
        }
        depth += 1;
        if depth > 64 {
            break;
        }
        dir = d.parent();
    }
    (None, ignored)
}

/// Why a `.attemptdb/` found by walking up is not used.
#[derive(Debug, PartialEq, Eq)]
pub enum Problem {
    /// No identity file inside: not a database (an empty directory, say).
    NotADatabase,
    /// A database, but not one to write to on a stranger's say-so.
    Untrusted(String),
}

/// Whether `dir` (a `.attemptdb` found by walking up) may be used, and if
/// not, why. A hook fires inside whatever repository the agent is working
/// in, and a repository is somebody else's files: a cloned `.attemptdb/`
/// can hold symlinks that make the spool writer clobber any file the user
/// can write (REPORT §5.1). Unix, in order:
///
/// 1. the `.attemptdb` entry is a real directory (not a symlink), owned by
///    the current user, and not writable by everybody;
/// 2. `ATTEMPTDB` inside it is a regular file (not a symlink) owned by the
///    current user;
/// 3. `spool/`, `wal/` and `manifest/`, when present, are real directories,
///    and nothing directly in them or in `.attemptdb` itself is a symlink.
///    Git preserves symlinks but not ownership, so a clone passes (1) and
///    (2): this is the check that catches the planted link.
///
/// Windows: the entry, the marker and `spool/` must not be reparse points
/// (symlinks, junctions); ownership is not checked.
pub fn trust_problem(dir: &Path) -> Option<Problem> {
    #[cfg(unix)]
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    #[cfg(not(unix))]
    let me = 0;
    trust_problem_as(dir, me)
}

/// [`trust_problem`] for a given user id (ignored off Unix), so that the
/// foreign-owner case can be tested without being root.
fn trust_problem_as(dir: &Path, me: u32) -> Option<Problem> {
    let _ = me;
    let marker = attemptdb_storage::Identity::path(dir);
    let marker_meta = match std::fs::symlink_metadata(&marker) {
        Ok(m) => m,
        Err(_) => return Some(Problem::NotADatabase),
    };
    let untrusted = |why: &str| Some(Problem::Untrusted(why.to_string()));
    let Ok(dir_meta) = std::fs::symlink_metadata(dir) else {
        return Some(Problem::NotADatabase);
    };
    if is_link(&dir_meta) {
        return untrusted("the .attemptdb entry is a symbolic link");
    }
    if !dir_meta.is_dir() {
        return untrusted("the .attemptdb entry is not a directory");
    }
    if is_link(&marker_meta) {
        return untrusted("the ATTEMPTDB identity file is a symbolic link");
    }
    if !marker_meta.is_file() {
        return untrusted("the ATTEMPTDB identity file is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if dir_meta.uid() != me {
            return untrusted("the .attemptdb directory is owned by another user");
        }
        if marker_meta.uid() != me {
            return untrusted("the ATTEMPTDB identity file is owned by another user");
        }
        if dir_meta.mode() & 0o002 != 0 {
            return untrusted("the .attemptdb directory is writable by everybody");
        }
    }
    // The directories a writer creates files in. Bounded scans: a busy spool
    // holds a few files, and a planted link needs only one entry looked at.
    use attemptdb_storage::format::{MANIFEST_DIR, SPOOL_DIR, WAL_DIR};
    for sub in [None, Some(SPOOL_DIR), Some(WAL_DIR), Some(MANIFEST_DIR)] {
        let path = sub.map_or_else(|| dir.to_path_buf(), |name| dir.join(name));
        if sub.is_some() {
            match std::fs::symlink_metadata(&path) {
                Ok(m) if is_link(&m) || !m.is_dir() => {
                    return untrusted(
                        "an internal directory is a symbolic link or not a directory",
                    );
                }
                Ok(_) => {}
                Err(_) => continue,
            }
        }
        let planted = std::fs::read_dir(&path)
            .into_iter()
            .flatten()
            .flatten()
            .take(4096)
            .any(|e| e.file_type().is_ok_and(|t| t.is_symlink()));
        if planted {
            return untrusted("the .attemptdb directory contains a symbolic link");
        }
    }
    None
}

/// A symbolic link, or on Windows any reparse point (a junction is one).
fn is_link(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink() || is_reparse_point(meta)
}

#[cfg(windows)]
fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_meta: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::run_hook;

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

    fn make_local_db(project: &Path) -> PathBuf {
        let db = project.join(".attemptdb");
        attemptdb_storage::Database::create(&db, attemptdb_core::DeviceId::new()).unwrap();
        db
    }

    #[test]
    fn a_database_this_user_made_is_trusted() {
        let tmp = tempfile::tempdir().unwrap();
        let db = make_local_db(tmp.path());
        assert_eq!(trust_problem(&db), None);
        let (found, ignored) = scan_project_local(tmp.path());
        assert_eq!(found, Some(db));
        assert!(ignored.is_empty());
    }

    #[test]
    fn a_directory_that_is_not_a_database_is_skipped_quietly() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".attemptdb")).unwrap();
        let (found, ignored) = scan_project_local(tmp.path());
        assert!(found.is_none());
        assert!(ignored.is_empty(), "an empty .attemptdb is not a finding");
    }

    #[cfg(unix)]
    mod hostile {
        use super::*;
        use std::os::unix::fs::{PermissionsExt, symlink};

        fn ignored_reason(project: &Path) -> String {
            let l = Locator::resolve(project, Some(&project.join("data")), None);
            assert_eq!(
                l.source,
                DbSource::Default,
                "an untrusted database is not used"
            );
            assert_eq!(l.ignored_local.len(), 1, "{:?}", l.ignored_local);
            assert_eq!(l.ignored_local[0].path, project.join(".attemptdb"));
            l.ignored_local[0].reason.clone()
        }

        #[test]
        fn a_planted_symlink_in_the_spool_makes_the_database_untrusted() {
            // The shape of REPORT §5.1: a cloned repository carries
            // `.attemptdb/ATTEMPTDB` and a spool sidecar linked at a victim.
            let tmp = tempfile::tempdir().unwrap();
            let victim = tmp.path().join("victim.txt");
            std::fs::write(&victim, "precious").unwrap();
            let project = tmp.path().join("cloned");
            let db = make_local_db(&project);
            symlink(&victim, db.join("spool").join("inbox.spool.committed.tmp")).unwrap();
            assert!(ignored_reason(&project).contains("symbolic link"));
            // And a hook run from inside it does not touch the victim.
            let out = run_hook(crate::hook::HookInput {
                provider_id: "claude-code",
                event_hint: None,
                payload_bytes: serde_json::json!({
                    "hook_event_name": "Stop",
                    "session_id": "s",
                    "cwd": project.to_string_lossy(),
                })
                .to_string()
                .into_bytes(),
                cwd_hint: None,
                data_dir_override: Some(tmp.path().join("data")),
                db_override: None,
            });
            assert_eq!(
                out.db_dir,
                tmp.path().join("data").join("db").join(".attemptdb")
            );
            assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        }

        #[test]
        fn a_symlinked_attemptdb_directory_is_untrusted() {
            let tmp = tempfile::tempdir().unwrap();
            let real = tmp.path().join("elsewhere");
            make_local_db(&real);
            let project = tmp.path().join("cloned");
            std::fs::create_dir_all(&project).unwrap();
            symlink(real.join(".attemptdb"), project.join(".attemptdb")).unwrap();
            assert!(ignored_reason(&project).contains("symbolic link"));
        }

        #[test]
        fn a_symlinked_identity_file_is_untrusted() {
            let tmp = tempfile::tempdir().unwrap();
            let real = tmp.path().join("elsewhere");
            let real_db = make_local_db(&real);
            let project = tmp.path().join("cloned");
            let dir = project.join(".attemptdb");
            std::fs::create_dir_all(&dir).unwrap();
            symlink(real_db.join("ATTEMPTDB"), dir.join("ATTEMPTDB")).unwrap();
            assert!(ignored_reason(&project).contains("identity file is a symbolic link"));
        }

        #[test]
        fn a_symlinked_spool_directory_is_untrusted() {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().join("cloned");
            let db = make_local_db(&project);
            let elsewhere = tmp.path().join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            std::fs::remove_dir(db.join("spool")).unwrap();
            symlink(&elsewhere, db.join("spool")).unwrap();
            assert!(ignored_reason(&project).contains("symbolic link"));
        }

        #[test]
        fn a_database_owned_by_somebody_else_is_untrusted() {
            let tmp = tempfile::tempdir().unwrap();
            let db = make_local_db(tmp.path());
            // Pretend to be a different user than the owner.
            let me = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&db).unwrap());
            let problem = trust_problem_as(&db, me.wrapping_add(1));
            assert!(
                matches!(&problem, Some(Problem::Untrusted(why)) if why.contains("owned by another user")),
                "{problem:?}"
            );
            assert_eq!(trust_problem_as(&db, me), None);
        }

        #[test]
        fn a_world_writable_database_directory_is_untrusted() {
            let tmp = tempfile::tempdir().unwrap();
            let db = make_local_db(tmp.path());
            std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o777)).unwrap();
            assert!(ignored_reason(tmp.path()).contains("writable by everybody"));
            // Group-writable alone is how a umask of 002 leaves its directories.
            std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o775)).unwrap();
            assert_eq!(trust_problem(&db), None);
        }

        #[test]
        fn an_untrusted_database_does_not_hide_a_trusted_one_above_it() {
            let tmp = tempfile::tempdir().unwrap();
            let outer = tmp.path().join("work");
            let outer_db = make_local_db(&outer);
            let inner = outer.join("cloned");
            let inner_db = make_local_db(&inner);
            symlink(
                tmp.path().join("victim"),
                inner_db.join("spool").join("inbox.spool"),
            )
            .unwrap();
            let (found, ignored) = scan_project_local(&inner);
            assert_eq!(found, Some(outer_db));
            assert_eq!(ignored.len(), 1);
            assert_eq!(ignored[0].path, inner_db);
        }
    }
}
