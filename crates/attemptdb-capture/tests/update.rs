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
    // libtest runs these tests on parallel threads, each staging and then
    // executing its own binary, which is exactly the fork/`ETXTBSY` window
    // `spawn_executable` exists to close. Use it here for the same reason the
    // CLI does, rather than making the test the only caller that races.
    // `spawn` does not imply piped stdio the way `output` does.
    let out = update::spawn_executable(
        Command::new(bin)
            .arg("--version")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()),
    )
    .unwrap()
    .wait_with_output()
    .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Build `attempt-<v>-<target>.tar.gz` exactly like the release workflow.
fn build_release(dir: &Path) -> (String, Vec<u8>) {
    build_release_with_hook(dir, None)
}

/// The same, shipping an `attempt-hook` whose body is `hook` when given.
fn build_release_with_hook(dir: &Path, hook: Option<&str>) -> (String, Vec<u8>) {
    let stem = update::asset_stem(NEW_VERSION, TARGET);
    let pkg = dir.join(&stem);
    fs::create_dir_all(&pkg).unwrap();
    script(&pkg.join("attempt"), NEW_VERSION);
    if let Some(body) = hook {
        fs::write(pkg.join("attempt-hook"), body).unwrap();
        fs::set_permissions(pkg.join("attempt-hook"), fs::Permissions::from_mode(0o755)).unwrap();
    }
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

fn hook_script(version: &str) -> String {
    format!("#!/bin/sh\necho \"attempt-hook {version}\"\n")
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
                // `REDIRECT <location>`: answer 302 to that location.
                Some(body) if body.starts_with(b"REDIRECT ") => format!(
                    "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    String::from_utf8_lossy(&body[9..])
                )
                .into_bytes(),
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

    // Check only: nothing downloaded. This release publishes no policy
    // (`update.json` is a 404), so the resolver falls back to the API.
    let mut o = opts(&base, &bin);
    o.check_only = true;
    let report = update::run(&o, &runs).unwrap();
    assert_eq!(report.outcome, Outcome::Available);
    assert_eq!(report.resolved, NEW_VERSION);
    assert!(!report.required, "no policy, no floor");
    {
        let paths = seen.lock().unwrap();
        assert!(
            paths
                .iter()
                .all(|p| p.ends_with("/latest") || p.ends_with("/latest/download/update.json")),
            "{paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|p| p.ends_with("/latest/download/update.json")),
            "the policy is asked first"
        );
    }

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

#[test]
fn a_published_policy_is_read_without_the_api_and_names_the_floor() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (asset, archive) = build_release(root);
    let digest = update::sha256_file(&root.join(&asset)).unwrap();
    let sums = format!("{digest}  {asset}\n");
    let mut r = routes(&asset, &archive, &sums);
    r.insert(
        "/nullarch/attemptdb/releases/latest/download/update.json".to_string(),
        format!(r#"{{"latest":"v{NEW_VERSION}","required_below":"9.0.0","min_sync_version":1}}"#)
            .into_bytes(),
    );
    let (base, seen) = serve(r);
    let bin_dir = root.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let bin = bin_dir.join("attempt");
    script(&bin, "0.1.0");

    let mut o = opts(&base, &bin);
    o.check_only = true;
    let report = update::run(&o, &|_| Ok(())).unwrap();
    assert_eq!(report.outcome, Outcome::Available);
    assert_eq!(report.resolved, NEW_VERSION);
    assert!(report.required, "the running binary is below the floor");
    let paths = seen.lock().unwrap();
    assert!(
        paths
            .iter()
            .all(|p| p.ends_with("/latest/download/update.json")),
        "the policy answered; the API was never asked: {paths:?}"
    );
}

fn redirect(to: &str) -> Vec<u8> {
    format!("REDIRECT {to}").into_bytes()
}

fn policy_route(latest: &str, floor: Option<&str>) -> (String, Vec<u8>) {
    let floor = floor
        .map(|f| format!(r#","required_below":{}"#, serde_json::to_string(f).unwrap()))
        .unwrap_or_default();
    (
        "/nullarch/attemptdb/releases/latest/download/update.json".to_string(),
        format!(
            r#"{{"latest":{}{floor}}}"#,
            serde_json::to_string(latest).unwrap()
        )
        .into_bytes(),
    )
}

fn installed(root: &Path, version: &str) -> std::path::PathBuf {
    let bin_dir = root.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let bin = bin_dir.join("attempt");
    script(&bin, version);
    bin
}

#[test]
fn a_policy_with_a_malformed_latest_is_refused_before_any_download() {
    for bad in [
        "nightly",
        "../../evil",
        "9.9.9/../x",
        "9.9.9\n",
        "9.9.9?x=1",
        "latest",
        "",
        "9.9",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (asset, archive) = build_release(&root.join("release"));
        let digest = hex::encode(Sha256::digest(&archive));
        let mut r = routes(&asset, &archive, &format!("{digest}  {asset}\n"));
        let (path, body) = policy_route(bad, None);
        r.insert(path, body);
        let (base, seen) = serve(r);
        let bin = installed(&root, "0.1.0");
        let err = update::run(&opts(&base, &bin), &runs).unwrap_err();
        assert!(
            format!("{err:#}").contains("not a release version"),
            "{bad:?}: {err:#}"
        );
        assert_eq!(version_of(&bin), "attempt 0.1.0", "{bad:?}");
        let paths = seen.lock().unwrap().clone();
        assert!(
            paths.iter().all(|p| p.ends_with("/update.json")),
            "{bad:?}: nothing but the policy was asked for: {paths:?}"
        );
        assert!(!update::slots(&bin).staging.exists());
    }
}

#[test]
fn a_floor_that_is_not_a_version_is_ignored_and_a_malformed_api_tag_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let mut r = HashMap::new();
    let (path, body) = policy_route(NEW_VERSION, Some("whatever"));
    r.insert(path, body);
    let (base, _) = serve(r);
    let bin = installed(&root, "0.1.0");
    let mut o = opts(&base, &bin);
    o.check_only = true;
    let report = update::run(&o, &runs).unwrap();
    assert_eq!(report.outcome, Outcome::Available);
    assert!(!report.required, "a floor nobody can read forces nothing");

    // No policy document: the API names the version, and it must be one.
    let mut r = HashMap::new();
    r.insert(
        "/repos/nullarch/attemptdb/releases/latest".to_string(),
        br#"{"tag_name":"v1.0.0/../../x"}"#.to_vec(),
    );
    let (base, seen) = serve(r);
    let err = update::run(&opts(&base, &bin), &runs).unwrap_err();
    assert!(format!("{err:#}").contains("tag_name"), "{err:#}");
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|p| !p.contains("download/v"))
    );
}

#[test]
fn a_pinned_version_must_be_a_version_too() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (base, seen) = serve(HashMap::new());
    let bin = installed(&root, "0.1.0");
    for bad in ["../../x", "1.0.0/../x", "latest", "1.0.0 --force"] {
        let mut o = opts(&base, &bin);
        o.version = Some(bad.to_string());
        let err = update::run(&o, &runs).unwrap_err();
        assert!(
            format!("{err:#}").contains("not a release version"),
            "{bad}: {err:#}"
        );
    }
    assert!(seen.lock().unwrap().is_empty(), "no request for a bad pin");
}

#[test]
fn a_redirect_to_another_host_is_refused_for_the_archive_and_for_the_checksums() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (asset, archive) = build_release(&root.join("release"));
    let digest = hex::encode(Sha256::digest(&archive));
    let sums = format!("{digest}  {asset}\n");
    let dl = format!("/nullarch/attemptdb/releases/download/v{NEW_VERSION}");
    let bin = installed(&root, "0.1.0");

    // The attacker's server holds a perfectly valid archive and checksums.
    let mut evil_routes = HashMap::new();
    evil_routes.insert(format!("{dl}/{asset}"), archive.clone());
    evil_routes.insert(format!("{dl}/SHA256SUMS"), sums.clone().into_bytes());
    let (evil, evil_seen) = serve(evil_routes);

    // The archive redirects away.
    let mut r = routes(&asset, &archive, &sums);
    r.insert(
        format!("{dl}/{asset}"),
        redirect(&format!("{evil}{dl}/{asset}")),
    );
    let (base, _) = serve(r);
    let err = update::run(&opts(&base, &bin), &runs).unwrap_err();
    assert!(
        format!("{err:#}").contains("not an allowed download host"),
        "{err:#}"
    );

    // The checksums redirect away (a mirror of both would verify).
    let mut r = routes(&asset, &archive, &sums);
    r.insert(
        format!("{dl}/SHA256SUMS"),
        redirect(&format!("{evil}{dl}/SHA256SUMS")),
    );
    let (base, _) = serve(r);
    let err = update::run(&opts(&base, &bin), &runs).unwrap_err();
    assert!(
        format!("{err:#}").contains("not an allowed download host"),
        "{err:#}"
    );

    // A scheme downgrade or a different scheme is no better.
    let mut r = routes(&asset, &archive, &sums);
    r.insert(format!("{dl}/{asset}"), redirect("file:///etc/passwd"));
    let (base, _) = serve(r);
    assert!(update::run(&opts(&base, &bin), &runs).is_err());

    assert!(
        evil_seen.lock().unwrap().is_empty(),
        "the other host was never contacted: {:?}",
        evil_seen.lock().unwrap()
    );
    assert_eq!(version_of(&bin), "attempt 0.1.0");
    let slots = update::slots(&bin);
    assert!(!slots.new.exists() && !slots.prev.exists() && !slots.staging.exists());
}

#[test]
fn a_redirect_within_the_release_server_is_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (asset, archive) = build_release(&root.join("release"));
    let digest = hex::encode(Sha256::digest(&archive));
    let dl = format!("/nullarch/attemptdb/releases/download/v{NEW_VERSION}");
    let mut r = routes(&asset, &archive, &format!("{digest}  {asset}\n"));
    // The CDN hop GitHub puts in front of every release asset.
    r.insert(format!("{dl}/{asset}"), redirect(&format!("/cdn/{asset}")));
    r.insert(format!("{dl}/SHA256SUMS"), redirect("/cdn/SHA256SUMS"));
    r.insert(format!("/cdn/{asset}"), archive.clone());
    r.insert(
        "/cdn/SHA256SUMS".to_string(),
        format!("{digest}  {asset}\n").into_bytes(),
    );
    let (base, _) = serve(r);
    let bin = installed(&root, "0.1.0");
    let report = update::run(&opts(&base, &bin), &runs).unwrap();
    assert!(
        matches!(report.outcome, Outcome::Updated { .. }),
        "{report:?}"
    );
    assert_eq!(version_of(&bin), format!("attempt {NEW_VERSION}"));
}

/// A release that ships `attempt-hook` with this body, an installed
/// `attempt` (0.1.0) and, optionally, an installed hook (0.1.0).
struct HookRelease {
    _tmp: tempfile::TempDir,
    base: String,
    bin: std::path::PathBuf,
    hook: std::path::PathBuf,
}

fn hook_release(new_hook: &str, installed_hook: bool) -> HookRelease {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (asset, archive) = build_release_with_hook(&root.join("release"), Some(new_hook));
    let digest = hex::encode(Sha256::digest(&archive));
    let (base, _) = serve(routes(&asset, &archive, &format!("{digest}  {asset}\n")));
    let bin = installed(&root, "0.1.0");
    let hook = bin.parent().unwrap().join("attempt-hook");
    if installed_hook {
        fs::write(&hook, hook_script("0.1.0")).unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    HookRelease {
        _tmp: tmp,
        base,
        bin,
        hook,
    }
}

#[test]
fn the_hook_binary_is_swapped_with_attempt_and_kept_for_rollback() {
    let r = hook_release(&hook_script(NEW_VERSION), true);
    let report = update::run(&opts(&r.base, &r.bin), &runs).unwrap();
    assert!(
        matches!(report.outcome, Outcome::Updated { .. }),
        "{report:?}"
    );
    assert_eq!(version_of(&r.hook), format!("attempt-hook {NEW_VERSION}"));
    assert_eq!(
        version_of(&update::slots(&r.hook).prev),
        "attempt-hook 0.1.0"
    );
    assert!(report.notes.iter().any(|n| n.contains("updated alongside")));
    update::rollback(&r.bin).unwrap();
    assert_eq!(version_of(&r.bin), "attempt 0.1.0");
    assert_eq!(version_of(&r.hook), "attempt-hook 0.1.0", "both roll back");
}

#[test]
fn a_hook_binary_that_does_not_start_vetoes_the_whole_update() {
    for installed_hook in [true, false] {
        let r = hook_release("#!/bin/sh\necho nope >&2\nexit 1\n", installed_hook);
        let err = update::run(&opts(&r.base, &r.bin), &runs).unwrap_err();
        assert!(
            format!("{err:#}").contains("attempt-hook failed its check"),
            "{err:#}"
        );
        assert!(
            format!("{err:#}").contains("nothing was changed"),
            "{err:#}"
        );
        // Nothing moved: not `attempt`, not the installed hook.
        assert_eq!(version_of(&r.bin), "attempt 0.1.0");
        let (a, h) = (update::slots(&r.bin), update::slots(&r.hook));
        for p in [
            &a.new, &a.prev, &a.failed, &a.staging, &h.new, &h.prev, &h.failed,
        ] {
            assert!(!p.exists(), "{}", p.display());
        }
        assert_eq!(r.hook.exists(), installed_hook);
        if installed_hook {
            assert_eq!(version_of(&r.hook), "attempt-hook 0.1.0");
        }
    }
    // A hook that runs but is not attempt-hook at all is no better.
    let r = hook_release("#!/bin/sh\necho hello\n", true);
    assert!(update::run(&opts(&r.base, &r.bin), &runs).is_err());
    assert_eq!(version_of(&r.bin), "attempt 0.1.0");
}

#[test]
fn a_hook_binary_that_fails_once_installed_rolls_both_binaries_back() {
    // Passes as `attempt-hook.new`, fails as `attempt-hook`: the shape of a
    // binary that starts from staging and breaks where agents will run it.
    let flaky = format!(
        "#!/bin/sh\ncase \"$0\" in\n  *.new) echo \"attempt-hook {NEW_VERSION}\" ;;\n  *) echo broken >&2; exit 1 ;;\nesac\n"
    );
    for installed_hook in [true, false] {
        let r = hook_release(&flaky, installed_hook);
        let report = update::run(&opts(&r.base, &r.bin), &runs).unwrap();
        assert!(
            matches!(&report.outcome, Outcome::RolledBack { reason } if reason.contains("attempt-hook")),
            "{report:?}"
        );
        assert_eq!(version_of(&r.bin), "attempt 0.1.0", "attempt is back");
        assert_eq!(r.hook.exists(), installed_hook, "the hook is back or gone");
        if installed_hook {
            assert_eq!(version_of(&r.hook), "attempt-hook 0.1.0");
        }
        let (a, h) = (update::slots(&r.bin), update::slots(&r.hook));
        assert_eq!(version_of(&a.failed), format!("attempt {NEW_VERSION}"));
        assert!(h.failed.exists(), "the broken hook is kept for inspection");
        assert!(!a.prev.exists() && !h.prev.exists() && !h.new.exists());
    }
}

#[test]
fn the_health_check_reads_the_manifest_through_a_light_command_never_status() {
    use attemptdb_capture::locator::Locator;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let data = root.join("data");
    let loc = Locator::resolve(&root, Some(&data), None);
    attemptdb_storage::Database::create(&loc.db_dir, attemptdb_core::DeviceId::default()).unwrap();
    let log = root.join("calls.log");
    let stub = |name: &str, on_health: &str| {
        let p = root.join(name);
        fs::write(
            &p,
            format!(
                "#!/bin/sh\necho \"$@\" >> {log}\nfor a in \"$@\"; do\n  case \"$a\" in\n    --version) echo \"attempt 9.9.9\"; exit 0 ;;\n    health) {on_health} ;;\n    status|doctor) echo \"full scan\" >&2; exit 9 ;;\n  esac\ndone\nexit 0\n",
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    };
    let check = update::health_check_with(&loc, true);

    // Healthy: `--version`, then `health` scoped to this data directory.
    let ok = stub("ok", "echo ok; exit 0");
    check(&ok).unwrap();
    let calls = fs::read_to_string(&log).unwrap();
    assert!(calls.lines().any(|l| l == "--version"), "{calls}");
    assert!(
        calls
            .lines()
            .any(|l| l == format!("--data-dir {} health", data.display())),
        "{calls}"
    );
    assert!(!calls.contains("status"), "never a full status: {calls}");

    // A binary that cannot read the database fails the check.
    let bad = stub("bad", "echo \"manifest: unsupported format\" >&2; exit 1");
    let err = check(&bad).unwrap_err();
    assert!(format!("{err:#}").contains("manifest"), "{err:#}");

    // One that predates `health` is held to its version alone.
    let old = stub(
        "old",
        "echo \"error: unrecognized subcommand 'health'\" >&2; exit 2",
    );
    check(&old).unwrap();

    // Not asking for the database step (`--no-health-check`) skips it.
    fs::write(&log, "").unwrap();
    update::health_check_with(&loc, false)(&bad).unwrap();
    assert_eq!(fs::read_to_string(&log).unwrap().trim(), "--version");
}
