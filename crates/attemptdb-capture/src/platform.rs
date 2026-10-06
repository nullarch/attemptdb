//! Platform-specific locations and small OS helpers.
//!
//! Everything that depends on "where does this OS keep application data"
//! lives here so the rest of the capture runtime can stay path-agnostic.

use std::path::{Path, PathBuf};

/// Environment variable that switches AttemptDB into portable mode: every
/// directory in [`AppPaths`] is placed under this single root.
pub const DATA_DIR_ENV: &str = "ATTEMPTDB_DATA_DIR";

/// Name of the shipped binary (without extension).
pub const BINARY_NAME: &str = "attempt";

/// The set of directories the capture runtime uses.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AppPaths {
    /// Durable data: database, spool, manifests.
    pub data_dir: PathBuf,
    /// User configuration (`config.toml`).
    pub config_dir: PathBuf,
    /// Disposable caches.
    pub cache_dir: PathBuf,
    /// Sockets, pid files, and other per-boot state.
    pub runtime_dir: PathBuf,
    /// Log files.
    pub log_dir: PathBuf,
}

/// Resolve the application directories for the current platform.
///
/// Resolution order:
/// 1. `ATTEMPTDB_DATA_DIR` (portable mode): everything lives under that root
///    (`<root>`, `<root>/config`, `<root>/cache`, `<root>/run`, `<root>/logs`).
/// 2. macOS: `~/Library/Application Support/AttemptDB` for data *and* config
///    (a directory of TOML files does not belong in `~/Library/Preferences`,
///    which is reserved for plists), `~/Library/Caches/AttemptDB`,
///    `~/Library/Caches/AttemptDB/run` for sockets and pid files (not
///    `$TMPDIR`: a hook started from a sandboxed shell has a different
///    `$TMPDIR` than the launchd-started daemon, and would never find it),
///    `~/Library/Logs/AttemptDB`.
/// 3. Windows: `%LOCALAPPDATA%\AttemptDB{,\config,\cache,\run,\logs}`.
/// 4. Linux and everything else: XDG base directories with the documented
///    fallbacks (`~/.local/share`, `~/.config`, `~/.cache`, `<data>/run`,
///    `~/.local/state`).
///
/// This function never creates directories.
pub fn app_paths() -> AppPaths {
    if let Some(root) = std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        let root = PathBuf::from(root);
        return AppPaths {
            config_dir: root.join("config"),
            cache_dir: root.join("cache"),
            runtime_dir: root.join("run"),
            log_dir: root.join("logs"),
            data_dir: root,
        };
    }

    if cfg!(target_os = "macos") {
        return macos_paths();
    }
    if cfg!(windows) {
        return windows_paths();
    }
    xdg_paths()
}

fn fallback_home() -> PathBuf {
    home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn macos_paths() -> AppPaths {
    macos_paths_for(&fallback_home())
}

/// The macOS layout under `home`. Everything derives from the home
/// directory, which every process of one user agrees on; the runtime
/// directory is `<cache>/run` (created `0700` by the daemon), a stable
/// per-user path that does not depend on `$TMPDIR`. The socket path under it
/// is `<home>/Library/Caches/AttemptDB/run/attemptdb.sock`: 44 bytes more than
/// the home directory, inside the 100-byte budget for any home up to 56
/// bytes; a longer one takes the hashed fallback in `ipc::endpoint_for_runtime_dir`.
fn macos_paths_for(home: &Path) -> AppPaths {
    let library = home.join("Library");
    let data_dir = library.join("Application Support").join("AttemptDB");
    let cache_dir = library.join("Caches").join("AttemptDB");
    AppPaths {
        config_dir: data_dir.clone(),
        runtime_dir: cache_dir.join("run"),
        cache_dir,
        log_dir: library.join("Logs").join("AttemptDB"),
        data_dir,
    }
}

/// The runtime directory a daemon started by a build before the
/// `$TMPDIR`-independent layout listens in: `$TMPDIR/attemptdb-<uid>`, or
/// `<data>/run` when `$TMPDIR` was unset. `Some` only on macOS, for the
/// default layout (not `--data-dir`/`ATTEMPTDB_DATA_DIR`, which never
/// moved), and only when it differs from the current one. Clients try it
/// when nothing answers at the current endpoint, so an old daemon is still
/// found, and stopped, until it is restarted by the new binary.
pub fn legacy_runtime_dir(paths: &AppPaths) -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let current = macos_paths_for(&fallback_home());
    if paths.runtime_dir != current.runtime_dir {
        return None;
    }
    let legacy = std::env::var_os("TMPDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|tmp| tmp.join(format!("attemptdb-{}", current_uid_string())))
        .unwrap_or_else(|| current.data_dir.join("run"));
    (legacy != current.runtime_dir).then_some(legacy)
}

fn windows_paths() -> AppPaths {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| fallback_home().join("AppData").join("Local"));
    let root = base.join("AttemptDB");
    AppPaths {
        config_dir: root.join("config"),
        cache_dir: root.join("cache"),
        runtime_dir: root.join("run"),
        log_dir: root.join("logs"),
        data_dir: root,
    }
}

fn xdg_dir(var: &str, fallback: impl FnOnce() -> PathBuf) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(fallback)
}

fn xdg_paths() -> AppPaths {
    let home = fallback_home();
    let data_dir = xdg_dir("XDG_DATA_HOME", || home.join(".local").join("share")).join("attemptdb");
    let config_dir = xdg_dir("XDG_CONFIG_HOME", || home.join(".config")).join("attemptdb");
    let cache_dir = xdg_dir("XDG_CACHE_HOME", || home.join(".cache")).join("attemptdb");
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
        .map(|p| p.join("attemptdb"))
        .unwrap_or_else(|| data_dir.join("run"));
    let log_dir = xdg_dir("XDG_STATE_HOME", || home.join(".local").join("state"))
        .join("attemptdb")
        .join("logs");
    AppPaths {
        data_dir,
        config_dir,
        cache_dir,
        runtime_dir,
        log_dir,
    }
}

/// The numeric user id as a string (Unix), or the user name on other
/// platforms. Used to namespace per-user runtime directories.
fn current_uid_string() -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // The home directory is owned by the current user in every sane
        // setup; reading its uid avoids a libc dependency.
        if let Some(home) = home_dir()
            && let Ok(meta) = std::fs::metadata(&home)
        {
            return meta.uid().to_string();
        }
    }
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".to_string())
}

/// The current user's home directory, if it can be determined.
pub fn home_dir() -> Option<PathBuf> {
    dirs::home_dir().filter(|p| !p.as_os_str().is_empty())
}

/// Absolute, canonicalised path of the running executable.
pub fn current_exe_path() -> PathBuf {
    let raw = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(BINARY_NAME));
    canonical_display_path(&raw)
}

/// Absolute path of the running binary as a display string. On Windows the
/// `\\?\` verbatim prefix that `canonicalize` adds is stripped so the drive
/// letter form (`C:\...`) is kept.
pub fn current_exe_display() -> String {
    current_exe_path().to_string_lossy().into_owned()
}

/// Canonicalise a path for display / config purposes: resolve symlinks when
/// possible and strip Windows verbatim prefixes.
pub fn canonical_display_path(path: &Path) -> PathBuf {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    strip_verbatim_prefix(&canon)
}

/// Remove the `\\?\` (and `\\?\UNC\`) prefix Windows canonicalisation adds.
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

/// Quote a path for inclusion in a hook command line.
///
/// POSIX: single quotes with the `'\''` idiom, which is safe for every byte
/// except NUL. Windows: double quotes. Double quotes are not legal in Windows
/// file names, so no inner escaping is required.
pub fn quote_for_shell(path: &Path) -> String {
    let s = path.to_string_lossy();
    if is_windows() {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// True when compiled for Windows.
pub fn is_windows() -> bool {
    cfg!(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_posix_escapes_single_quotes() {
        if is_windows() {
            return;
        }
        assert_eq!(quote_for_shell(Path::new("/a/b")), "'/a/b'");
        assert_eq!(quote_for_shell(Path::new("/it's/here")), "'/it'\\''s/here'");
    }

    #[test]
    fn strip_verbatim() {
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\C:\x\attempt.exe")),
            PathBuf::from(r"C:\x\attempt.exe")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\UNC\srv\share\a")),
            PathBuf::from(r"\\srv\share\a")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new("/usr/bin/attempt")),
            PathBuf::from("/usr/bin/attempt")
        );
    }

    #[test]
    fn the_macos_runtime_dir_hangs_off_home_not_tmpdir() {
        let paths = macos_paths_for(Path::new("/Users/dev"));
        assert_eq!(
            paths.runtime_dir,
            Path::new("/Users/dev/Library/Caches/AttemptDB/run")
        );
        assert!(paths.runtime_dir.starts_with(&paths.cache_dir));
        // No process-wide state is consulted: the same home gives the same
        // answer from a shell with any `$TMPDIR` (nix, an IDE sandbox, launchd).
        assert_eq!(paths, macos_paths_for(Path::new("/Users/dev")));
        // The socket path fits `sun_path` for a normal home, and the layout
        // still hashes to a short fallback for an enormous one.
        let socket = paths.runtime_dir.join("attemptdb.sock");
        assert!(socket.as_os_str().len() <= 100, "{}", socket.display());
        let ep = crate::ipc::endpoint_for_runtime_dir(
            &macos_paths_for(Path::new(&format!("/Users/{}", "x".repeat(80)))).runtime_dir,
        );
        if !cfg!(windows) {
            let p = ep.socket_path().unwrap();
            assert!(p.as_os_str().len() <= 100, "{}", p.display());
        }
    }

    #[test]
    fn app_paths_are_absolute_or_portable() {
        let p = app_paths();
        // Not asserting absolute: a missing HOME would yield "." in CI.
        assert!(
            p.data_dir.ends_with("AttemptDB")
                || p.data_dir.ends_with("attemptdb")
                || std::env::var_os(DATA_DIR_ENV).is_some()
        );
        assert!(!p.log_dir.as_os_str().is_empty());
    }
}
