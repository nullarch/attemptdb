//! macOS: where the daemon's socket lives must not depend on `$TMPDIR`.
//!
//! A hook started from a sandboxed shell (nix, an IDE terminal) has a
//! different `$TMPDIR` than the launchd-started daemon, so a `$TMPDIR`-keyed
//! socket path was never found from it. Its own test binary and a single
//! test: it sets `HOME` and `TMPDIR`, which everything else would see.
#![cfg(target_os = "macos")]

use attemptdb_capture::ipc;
use attemptdb_capture::locator::Locator;
use attemptdb_capture::platform::{app_paths, legacy_runtime_dir};
use std::os::unix::net::UnixListener;
use std::path::Path;

fn locator_with_tmpdir(cwd: &Path, tmpdir: &Path) -> Locator {
    // SAFETY: the only test in this binary; nothing else reads the
    // environment while it changes.
    unsafe { std::env::set_var("TMPDIR", tmpdir) };
    Locator::resolve(cwd, None, None)
}

#[test]
fn the_socket_does_not_move_with_tmpdir_and_an_older_daemons_is_still_found() {
    // A short path: the socket path must stay under `sun_path`'s limit.
    let tmp = tempfile::Builder::new()
        .prefix("a")
        .tempdir_in("/tmp")
        .unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("h");
    std::fs::create_dir_all(&home).unwrap();
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::remove_var("ATTEMPTDB_DATA_DIR");
    }
    let (tmp_a, tmp_b) = (root.join("ta"), root.join("tb"));

    // The same home gives the same runtime directory and endpoint from
    // shells with different temp directories.
    let a = locator_with_tmpdir(&root, &tmp_a);
    let b = locator_with_tmpdir(&root, &tmp_b);
    assert_eq!(a.paths.runtime_dir, b.paths.runtime_dir);
    assert_eq!(
        a.paths.runtime_dir,
        home.join("Library/Caches/AttemptDB/run")
    );
    assert_eq!(ipc::endpoint(&a), ipc::endpoint(&b));
    let socket = ipc::endpoint(&a).socket_path().unwrap().to_path_buf();
    assert_eq!(socket, a.paths.runtime_dir.join("attemptdb.sock"));
    assert!(socket.as_os_str().len() <= 100, "{}", socket.display());
    assert_eq!(app_paths(), a.paths);

    // Nothing runs yet.
    assert!(!ipc::daemon_reachable(&a));

    // A daemon started by an older build listens under *its* `$TMPDIR`.
    let b = locator_with_tmpdir(&root, &tmp_b);
    let old_dir = legacy_runtime_dir(&b.paths).expect("macOS default layout has a legacy dir");
    assert!(old_dir.starts_with(&tmp_b), "{}", old_dir.display());
    std::fs::create_dir_all(&old_dir).unwrap();
    let old_socket = ipc::endpoint_for_runtime_dir(&old_dir)
        .socket_path()
        .unwrap()
        .to_path_buf();
    let _old_daemon = UnixListener::bind(&old_socket).unwrap();
    assert!(ipc::daemon_reachable(&b), "found through the legacy path");
    assert_eq!(
        ipc::client_endpoint(&b).socket_path(),
        Some(old_socket.as_path())
    );
    // Only from a shell that has the same `$TMPDIR` the old daemon had; the
    // daemon itself would bind the new path either way.
    assert_eq!(ipc::endpoint(&b).socket_path(), Some(socket.as_path()));
    let elsewhere = locator_with_tmpdir(&root, &tmp_a);
    assert!(!ipc::daemon_reachable(&elsewhere));

    // Once a daemon runs at the new path it wins.
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _new_daemon = UnixListener::bind(&socket).unwrap();
    assert!(ipc::daemon_reachable(&elsewhere));
    assert_eq!(
        ipc::client_endpoint(&b).socket_path(),
        Some(socket.as_path())
    );

    // A scoped (`--data-dir`) layout never had a legacy location.
    let scoped = Locator::resolve(&root, Some(&root.join("data")), None);
    assert_eq!(legacy_runtime_dir(&scoped.paths), None);
}
