//! A binary some other tool owns is never updated by `attempt` — not by
//! `attempt update`, not by the daemon's automatic tick — and the refusal
//! names the owner. Its own test binary (and a single test) because it sets
//! `ATTEMPTDB_MANAGED_BY`, which every other update test would see.
#![cfg(unix)]

use attemptdb_capture::config::AutoUpdate;
use attemptdb_capture::update::{
    self, AutoContext, AutoOutcome, CHECK_INTERVAL, Outcome, UpdateOptions, auto_tick,
};
use std::fs;
use std::io::Read;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn script(path: &Path, version: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("#!/bin/sh\necho \"attempt {version}\"\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A server that records every request and answers none of them well.
fn watch() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).unwrap_or(0);
            log.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..n]).to_string());
        }
    });
    (base, seen)
}

fn opts(base: &str, bin: &Path) -> UpdateOptions {
    UpdateOptions {
        version: None,
        force: false,
        check_only: false,
        binary: Some(bin.to_path_buf()),
        api_base: base.to_string(),
        download_base: base.to_string(),
    }
}

fn tick(base: &str, bin: &Path, cache: &Path) -> AutoOutcome {
    auto_tick(
        &AutoContext {
            cache_dir: cache.to_path_buf(),
            mode: AutoUpdate::On,
            quiet: true,
            may_apply: true,
            check_interval: CHECK_INTERVAL,
            opts: opts(base, bin),
        },
        &|_| panic!("a managed install is never health-checked for an update"),
    )
}

#[test]
fn a_managed_install_is_never_updated_and_the_refusal_names_the_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let cache = root.join("cache");
    let (base, seen) = watch();

    // Homebrew's prefixes, by path.
    for prefix in [
        "opt/homebrew/bin",
        "usr/local/Cellar/attempt/0.3.0/bin",
        "home/linuxbrew/.linuxbrew/bin",
    ] {
        let bin = root.join(prefix).join("attempt");
        script(&bin, "0.1.0");
        let report = update::run(&opts(&base, &bin), &|_| Ok(())).unwrap();
        assert!(
            matches!(&report.outcome, Outcome::Refused { reason }
                if reason.contains("Homebrew") && reason.contains("brew upgrade attempt")),
            "{prefix}: {report:?}"
        );
        assert!(
            matches!(tick(&base, &bin, &cache), AutoOutcome::Disabled),
            "{prefix}: automatic updates are off"
        );
    }

    // Anything else a machine's installer says owns the file.
    let plain = root.join("home/dev/.local/bin/attempt");
    script(&plain, "0.1.0");
    // SAFETY: this is the only test in this binary, so nothing else reads
    // the environment while it changes.
    unsafe { std::env::set_var("ATTEMPTDB_MANAGED_BY", "nix-darwin") };
    let report = update::run(&opts(&base, &plain), &|_| Ok(())).unwrap();
    assert!(
        matches!(&report.outcome, Outcome::Refused { reason }
            if reason.contains("nix-darwin") && reason.contains("ATTEMPTDB_MANAGED_BY")),
        "{report:?}"
    );
    assert!(matches!(tick(&base, &plain, &cache), AutoOutcome::Disabled));
    assert_eq!(update::managed_install(&plain).unwrap().0, "nix-darwin");

    // `0` or empty means nobody manages it: the request goes out again.
    for off in ["0", "", "  "] {
        unsafe { std::env::set_var("ATTEMPTDB_MANAGED_BY", off) };
        assert!(update::managed_install(&plain).is_none(), "{off:?}");
    }
    assert!(
        seen.lock().unwrap().is_empty(),
        "a managed install makes no request at all: {:?}",
        seen.lock().unwrap()
    );
    unsafe { std::env::remove_var("ATTEMPTDB_MANAGED_BY") };
    assert!(update::managed_install(&plain).is_none());
}
