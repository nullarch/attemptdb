//! End-to-end self-update against a local release server: a tarball built
//! the way `release.yml` builds it, a `SHA256SUMS`, and the GitHub "latest
//! release" document, all served from a thread. Unix only: the fake binaries
//! are shell scripts.
#![cfg(unix)]

use attemptdb_capture::update::{self, Outcome, TARGET, UpdateOptions};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

const NEW_VERSION: &str = "9.9.9";

fn script(path: &Path, version: &str) {
    fs::write(path, format!("#!/bin/sh\necho \"attempt {version}\"\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn version_of(bin: &Path) -> String {
    let out = Command::new(bin).arg("--version").output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Build `attempt-<v>-<target>.tar.gz` exactly like the release workflow.
fn build_release(dir: &Path) -> (String, Vec<u8>) {
    let stem = update::asset_stem(NEW_VERSION, TARGET);
    let pkg = dir.join(&stem);
    fs::create_dir_all(&pkg).unwrap();
    script(&pkg.join("attempt"), NEW_VERSION);
    fs::write(pkg.join("README.md"), "# fake\n").unwrap();
    let archive = dir.join(format!("{stem}.tar.gz"));
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(dir)
        .arg(&stem)
        .status()
        .unwrap();
    assert!(status.success());
    (format!("{stem}.tar.gz"), fs::read(&archive).unwrap())
}

/// Serve a fixed map of paths from a background thread.
fn serve(routes: HashMap<String, Vec<u8>>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf);
            let path = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            log.lock().unwrap().push(path.clone());
            let response = match routes.get(&path) {
                Some(body) => {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    r.extend_from_slice(body);
                    r
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            };
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        }
    });
    (base, seen)
}

fn routes(asset: &str, archive: &[u8], sums: &str) -> HashMap<String, Vec<u8>> {
    let mut m = HashMap::new();
    m.insert(
        "/repos/nullarch/attemptdb/releases/latest".to_string(),
        format!(r#"{{"tag_name":"v{NEW_VERSION}","name":"v{NEW_VERSION}"}}"#).into_bytes(),
    );
    let dl = format!("/nullarch/attemptdb/releases/download/v{NEW_VERSION}");
    m.insert(format!("{dl}/{asset}"), archive.to_vec());
    m.insert(format!("{dl}/SHA256SUMS"), sums.as_bytes().to_vec());
    m
}

fn opts(base: &str, binary: &Path) -> UpdateOptions {
    UpdateOptions {
        version: None,
        force: false,
        check_only: false,
        binary: Some(binary.to_path_buf()),
        api_base: base.to_string(),
        download_base: base.to_string(),
    }
}

fn runs(bin: &Path) -> anyhow::Result<()> {
    let v = version_of(bin);
    anyhow::ensure!(
        v.starts_with("attempt "),
        "unexpected --version output {v:?}"
    );
    Ok(())
}

#[test]
fn update_downloads_verifies_swaps_and_keeps_the_previous_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (asset, archive) = build_release(&root.join("release"));
    let digest = hex::encode(Sha256::digest(&archive));
    let sums = format!("{digest}  {asset}\n");
    let (base, seen) = serve(routes(&asset, &archive, &sums));

    let bin_dir = root.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let bin = bin_dir.join("attempt");
    script(&bin, "0.1.0");

    // Check only: nothing downloaded.
    let mut o = opts(&base, &bin);
    o.check_only = true;
    let report = update::run(&o, &runs).unwrap();
    assert_eq!(report.outcome, Outcome::Available);
    assert_eq!(report.resolved, NEW_VERSION);
    assert!(seen.lock().unwrap().iter().all(|p| p.ends_with("/latest")));

    // The real thing.
    let report = update::run(&opts(&base, &bin), &runs).unwrap();
    let slots = update::slots(&bin);
    assert_eq!(
        report.outcome,
        Outcome::Updated {
            previous: slots.prev.clone()
        }
    );
    assert_eq!(version_of(&bin), format!("attempt {NEW_VERSION}"));
    assert_eq!(version_of(&slots.prev), "attempt 0.1.0");
    assert!(!slots.new.exists());
    assert!(!slots.staging.exists(), "staging directory is cleaned up");
    assert!(
        report.notes.iter().any(|n| n.contains("--rollback")),
        "{:?}",
        report.notes
    );
    let paths = seen.lock().unwrap().clone();
    assert!(paths.iter().any(|p| p.ends_with("/SHA256SUMS")));
    assert!(paths.iter().any(|p| p.ends_with(&asset)));

    // Roll back, then the replaced binary is kept too.
    let failed = update::rollback(&bin).unwrap();
    assert_eq!(version_of(&bin), "attempt 0.1.0");
    assert_eq!(version_of(&failed), format!("attempt {NEW_VERSION}"));
}

#[test]
fn a_checksum_mismatch_or_missing_sums_leaves_the_binary_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (asset, archive) = build_release(&root.join("release"));
    let bin_dir = root.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let bin = bin_dir.join("attempt");
    script(&bin, "0.1.0");
    let slots = update::slots(&bin);

    // Wrong digest.
    let bad = format!("{}  {asset}\n", "0".repeat(64));
    let (base, _) = serve(routes(&asset, &archive, &bad));
    let err = update::run(&opts(&base, &bin), &runs).unwrap_err();
    assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
    assert_eq!(version_of(&bin), "attempt 0.1.0");
    assert!(!slots.new.exists() && !slots.prev.exists() && !slots.staging.exists());

    // No SHA256SUMS at all.
    let mut r = routes(&asset, &archive, "");
    r.remove(&format!(
        "/nullarch/attemptdb/releases/download/v{NEW_VERSION}/SHA256SUMS"
    ));
    let (base, _) = serve(r);
    let err = update::run(&opts(&base, &bin), &runs).unwrap_err();
    assert!(format!("{err:#}").contains("SHA256SUMS"), "{err:#}");
    assert_eq!(version_of(&bin), "attempt 0.1.0");
    assert!(!slots.new.exists() && !slots.prev.exists() && !slots.staging.exists());
}

#[test]
fn a_new_binary_that_cannot_open_the_database_is_rolled_back() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (asset, archive) = build_release(&root.join("release"));
    let digest = hex::encode(Sha256::digest(&archive));
    let (base, _) = serve(routes(&asset, &archive, &format!("{digest}  {asset}\n")));
    let bin_dir = root.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let bin = bin_dir.join("attempt");
    script(&bin, "0.1.0");
    let slots = update::slots(&bin);

    // Passes while staged, fails once it is the real binary: the shape of
    // "runs, but cannot read this database".
    let real = bin.clone();
    let check = move |p: &Path| -> anyhow::Result<()> {
        runs(p)?;
        if p == real && version_of(p).ends_with(NEW_VERSION) {
            anyhow::bail!("status: cannot open the database");
        }
        Ok(())
    };
    let report = update::run(&opts(&base, &bin), &check).unwrap();
    assert!(
        matches!(&report.outcome, Outcome::RolledBack { reason } if reason.contains("database"))
    );
    assert_eq!(version_of(&bin), "attempt 0.1.0");
    assert_eq!(version_of(&slots.failed), format!("attempt {NEW_VERSION}"));
    assert!(!slots.prev.exists() && !slots.new.exists() && !slots.staging.exists());
}

#[test]
fn pinned_current_version_is_up_to_date_and_package_managed_paths_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let bin = root.join("attempt");
    script(&bin, update::CURRENT_VERSION);
    let (base, seen) = serve(HashMap::new());
    let mut o = opts(&base, &bin);
    o.version = Some(update::CURRENT_VERSION.to_string());
    let report = update::run(&o, &runs).unwrap();
    assert_eq!(report.outcome, Outcome::UpToDate);
    assert!(
        seen.lock().unwrap().is_empty(),
        "a pinned version needs no API call"
    );

    let cellar = root.join("Cellar").join("attempt").join("bin");
    fs::create_dir_all(&cellar).unwrap();
    let brew_bin = cellar.join("attempt");
    script(&brew_bin, "0.1.0");
    let report = update::run(&opts(&base, &brew_bin), &runs).unwrap();
    assert!(
        matches!(&report.outcome, Outcome::Refused { reason } if reason.contains("brew upgrade"))
    );
}
