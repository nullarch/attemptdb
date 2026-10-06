//! macOS: a home directory over 56 bytes puts the daemon's socket under a
//! temp directory (the plain path would not fit `sun_path`). That directory
//! must not be whatever `$TMPDIR` happens to say in the calling shell, or a
//! hook started from a nix shell or an IDE terminal says "daemon not running"
//! while the launchd-started daemon is up. Its own test binary and a single
//! test: it sets `HOME` and `TMPDIR`, which everything else would see.
#![cfg(target_os = "macos")]

use attemptdb_capture::ipc;
use attemptdb_capture::locator::Locator;
use sha2::{Digest, Sha256};
use std::os::unix::net::UnixListener;
use std::path::Path;

fn locator_with_tmpdir(cwd: &Path, tmpdir: &Path) -> Locator {
    // SAFETY: the only test in this binary; nothing else reads the
    // environment while it changes.
    unsafe { std::env::set_var("TMPDIR", tmpdir) };
    Locator::resolve(cwd, None, None)
}

#[test]
fn a_long_home_keeps_the_socket_where_every_shell_finds_it() {
    let tmp = tempfile::Builder::new()
        .prefix("a")
        .tempdir_in("/tmp")
        .unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("h".repeat(60));
    std::fs::create_dir_all(&home).unwrap();
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::remove_var("ATTEMPTDB_DATA_DIR");
    }
    let (tmp_a, tmp_b) = (root.join("ta"), root.join("tb"));
    let a = locator_with_tmpdir(&root, &tmp_a);
    let b = locator_with_tmpdir(&root, &tmp_b);

    // The plain path is too long, so the fallback applies, and the reason is
    // something a person can read.
    let plain = a.paths.runtime_dir.join(ipc::SOCKET_FILE);
    assert!(plain.as_os_str().len() > 100, "{}", plain.display());
    let why = ipc::endpoint_fallback_reason(&a).expect("a reason for the fallback");
    assert!(
        why.contains("bytes") && why.contains("temporary directory"),
        "{why}"
    );

    // The same home gives the same endpoint from shells with different
    // `$TMPDIR`s, and it is under neither.
    assert_eq!(ipc::endpoint(&a), ipc::endpoint(&b));
    let socket = ipc::endpoint(&a).socket_path().unwrap().to_path_buf();
    assert!(socket.as_os_str().len() <= 100, "{}", socket.display());
    assert!(
        !socket.starts_with(&tmp_a) && !socket.starts_with(&tmp_b),
        "{}",
        socket.display()
    );
    assert!(!ipc::daemon_reachable(&a));

    // A daemon at that endpoint is found from a shell with another `$TMPDIR`.
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let daemon = UnixListener::bind(&socket).unwrap();
    assert!(ipc::daemon_reachable(&a));
    assert!(ipc::daemon_reachable(&b), "found whatever TMPDIR says");
    assert_eq!(
        ipc::client_endpoint(&b).socket_path(),
        Some(socket.as_path())
    );
    drop(daemon);
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir(socket.parent().unwrap());
    assert!(!ipc::daemon_reachable(&b));

    // A daemon from an earlier build put its socket under the temp directory
    // of ITS environment: a client sharing that `$TMPDIR` still finds it.
    let hash: String = Sha256::digest(a.paths.runtime_dir.as_os_str().as_encoded_bytes())[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let old_socket = tmp_b
        .join(format!("attemptdb-{}", ipc::current_uid().unwrap()))
        .join(format!("{hash}.sock"));
    std::fs::create_dir_all(old_socket.parent().unwrap()).unwrap();
    let _old_daemon = UnixListener::bind(&old_socket).unwrap();
    let b = locator_with_tmpdir(&root, &tmp_b);
    assert!(
        ipc::daemon_reachable(&b),
        "found under the client's own TMPDIR"
    );
    assert_eq!(
        ipc::client_endpoint(&b).socket_path(),
        Some(old_socket.as_path())
    );
    let elsewhere = locator_with_tmpdir(&root, &tmp_a);
    assert!(
        !ipc::daemon_reachable(&elsewhere),
        "a different TMPDIR cannot guess it"
    );
}
