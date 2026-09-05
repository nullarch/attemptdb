//! Rollback-safe self-update (RFC 0005, "Auto-update").
//!
//! The order of operations is the whole design:
//!
//! 1. resolve the release (latest, or a pinned version) for this binary's
//!    compile target;
//! 2. download the asset **and** `SHA256SUMS` into a staging directory next
//!    to the binary (same filesystem, so the final rename is atomic);
//! 3. verify the digest — a missing or mismatched digest aborts before
//!    anything is extracted;
//! 4. extract and stage the new binary as `<bin>.new`;
//! 5. health-check the staged binary (the caller supplies the check: at
//!    least `--version`, and `status` against the live database);
//! 6. swap: `<bin>` → `<bin>.prev`, `<bin>.new` → `<bin>`;
//! 7. health-check the swapped binary; on failure put `<bin>.prev` back.
//!
//! `<bin>.prev` is kept so `attempt update --rollback` can undo the last
//! update at any time. Nothing here touches the database or the hooks.
//!
//! Binaries managed by a package manager (Homebrew, cargo, Scoop) are refused
//! with the manager's own upgrade command: two writers to one path is how
//! installs rot.

use crate::platform::{canonical_display_path, current_exe_path};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const REPO: &str = "nullarch/attemptdb";
/// The compile target, from `build.rs`.
pub const TARGET: &str = env!("ATTEMPTDB_TARGET");
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_API_BASE: &str = "https://api.github.com";
pub const DEFAULT_DOWNLOAD_BASE: &str = "https://github.com";
/// Release assets are a few MB; anything past this is not ours.
const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct UpdateOptions {
    /// Pin a version (`1.2.3` or `v1.2.3`); `None` resolves the latest release.
    pub version: Option<String>,
    /// Install even when the resolved version is not newer.
    pub force: bool,
    /// Only report; download nothing.
    pub check_only: bool,
    /// The binary to replace; default: the running executable.
    pub binary: Option<PathBuf>,
    /// GitHub API base (tests point this at a local server).
    pub api_base: String,
    /// Release download base (tests point this at a local server).
    pub download_base: String,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        Self {
            version: None,
            force: false,
            check_only: false,
            binary: None,
            api_base: std::env::var("ATTEMPTDB_UPDATE_API")
                .unwrap_or_else(|_| DEFAULT_API_BASE.to_string()),
            download_base: std::env::var("ATTEMPTDB_UPDATE_DOWNLOAD")
                .unwrap_or_else(|_| DEFAULT_DOWNLOAD_BASE.to_string()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum Outcome {
    /// Already at the resolved version (and not forced).
    UpToDate,
    /// `check_only`: a newer version exists.
    Available,
    /// Swapped; the previous binary is kept at this path.
    Updated { previous: PathBuf },
    /// Swapped, the new binary failed its health check, and the previous
    /// binary was restored.
    RolledBack { reason: String },
    /// Not attempted, with the reason (package-managed path, unsupported target).
    Refused { reason: String },
}

#[derive(Clone, Debug, Serialize)]
pub struct UpdateReport {
    pub binary: PathBuf,
    pub target: String,
    pub current: String,
    pub resolved: String,
    /// The release policy marks the running binary as below its floor.
    #[serde(default)]
    pub required: bool,
    pub outcome: Outcome,
    pub notes: Vec<String>,
}

/// A caller-supplied check that a binary at `path` works. Runs twice: on
/// the staged file and on the swapped one.
pub type HealthCheck<'a> = &'a dyn Fn(&Path) -> Result<()>;

// ---------------------------------------------------------------------------
// The release policy: what a release says about the releases before it
// ---------------------------------------------------------------------------

/// `update.json`, published beside every release's assets by the Release
/// workflow from `RELEASE.toml`, read by installed clients once a day.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// The release this document belongs to — the newest.
    pub latest: String,
    /// Clients older than this update at once: the release fixed something
    /// that damages data, or the server will refuse them.
    #[serde(default)]
    pub required_below: Option<String>,
    /// The sync protocol version this release speaks.
    #[serde(default)]
    pub min_sync_version: Option<u32>,
    /// The release notes.
    #[serde(default)]
    pub notes: Option<String>,
}

/// What the policy says about the running binary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "version")]
pub enum Decision {
    UpToDate,
    /// A newer release exists; install it at a quiet moment.
    Optional(String),
    /// The running binary is below `required_below`; install it now.
    Required(String),
}

pub fn decide(current: &str, policy: &Policy) -> Decision {
    if !is_newer(current, &policy.latest) {
        return Decision::UpToDate;
    }
    match &policy.required_below {
        Some(floor) if is_newer(current, floor) => Decision::Required(policy.latest.clone()),
        _ => Decision::Optional(policy.latest.clone()),
    }
}

/// `update.json` sits behind the `releases/latest` redirect: a plain
/// download, no API, no rate limit — thirty machines behind one office
/// address can all ask once a day.
pub fn policy_url(download_base: &str) -> String {
    format!(
        "{}/{REPO}/releases/latest/download/update.json",
        download_base.trim_end_matches('/')
    )
}

/// The newest release's policy. Releases before 0.2.8 published none; for
/// those the API names the version and the policy carries no floor.
pub fn fetch_policy(agent: &ureq::Agent, opts: &UpdateOptions) -> Result<Policy> {
    let url = policy_url(&opts.download_base);
    match agent.get(&url).call() {
        Ok(resp) => {
            let body = resp.into_string()?;
            let mut p: Policy = serde_json::from_str(&body)
                .with_context(|| format!("{url}: not a release policy document"))?;
            p.latest = p.latest.trim_start_matches('v').to_string();
            if p.latest.is_empty() {
                bail!("{url}: the policy names no version");
            }
            Ok(p)
        }
        Err(ureq::Error::Status(404, _)) => Ok(Policy {
            latest: latest_via_api(agent, opts)?,
            ..Default::default()
        }),
        Err(e) => Err(anyhow!("{url}: {e}")),
    }
}

// ---------------------------------------------------------------------------
// The daily check, and applying what it decided
// ---------------------------------------------------------------------------

pub const CHECK_FILE: &str = "update-check.json";
/// How often the policy is fetched; between fetches the last answer stands.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// The last check, kept in the cache directory so `attempt doctor` can say
/// what is available without a request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckState {
    /// Unix seconds.
    pub checked_at: i64,
    /// The binary the decision was made for.
    pub current: String,
    pub policy: Policy,
    pub decision: Decision,
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl CheckState {
    pub fn path(cache_dir: &Path) -> PathBuf {
        cache_dir.join(CHECK_FILE)
    }

    pub fn load(cache_dir: &Path) -> Option<Self> {
        let bytes = fs::read(Self::path(cache_dir)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save(&self, cache_dir: &Path) -> Result<()> {
        fs::create_dir_all(cache_dir)?;
        let tmp = cache_dir.join(format!("{CHECK_FILE}.tmp"));
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&tmp, Self::path(cache_dir))?;
        Ok(())
    }

    pub fn age(&self) -> Duration {
        Duration::from_secs(unix_now().saturating_sub(self.checked_at).max(0) as u64)
    }

    /// Fresh enough to stand in for a request, and about this binary.
    pub fn is_current(&self, interval: Duration) -> bool {
        self.current == CURRENT_VERSION && self.age() < interval
    }
}

/// `ATTEMPTDB_NO_AUTO_UPDATE` set to anything but empty or `0`: never update
/// on our own — CI images, containers, machines someone else manages.
pub fn auto_update_disabled_by_env() -> bool {
    std::env::var("ATTEMPTDB_NO_AUTO_UPDATE")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

pub struct AutoContext {
    pub cache_dir: PathBuf,
    pub mode: crate::config::AutoUpdate,
    /// Nothing has been ingested for a while, so an optional release may
    /// go in now.
    pub quiet: bool,
    /// Applying is possible here at all: a supervised daemon that will be
    /// restarted, or a scheduled task — not a daemon someone started by hand.
    pub may_apply: bool,
    pub check_interval: Duration,
    pub opts: UpdateOptions,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AutoOutcome {
    /// Off by configuration or environment; nothing was fetched.
    Disabled,
    /// A decision stands (fresh, or just fetched) and nothing was applied.
    Checked {
        decision: Decision,
        fetched: bool,
        /// Why an available release was not applied.
        held: Option<String>,
    },
    Applied {
        report: UpdateReport,
    },
    Failed {
        error: String,
    },
}

/// One tick of automatic updating: fetch the policy if the last check is
/// stale, decide, and apply when the decision and the moment allow. Never
/// panics, never returns an `Err`: the caller is a loop that must go on.
pub fn auto_tick(ctx: &AutoContext, check: HealthCheck) -> AutoOutcome {
    use crate::config::AutoUpdate;
    if ctx.mode == AutoUpdate::Off || auto_update_disabled_by_env() {
        return AutoOutcome::Disabled;
    }
    let (state, fetched) =
        match CheckState::load(&ctx.cache_dir).filter(|s| s.is_current(ctx.check_interval)) {
            Some(s) => (s, false),
            None => {
                let policy = match fetch_policy(&agent(), &ctx.opts) {
                    Ok(p) => p,
                    Err(e) => {
                        return AutoOutcome::Failed {
                            error: format!("{e:#}"),
                        };
                    }
                };
                let s = CheckState {
                    checked_at: unix_now(),
                    current: CURRENT_VERSION.to_string(),
                    decision: decide(CURRENT_VERSION, &policy),
                    policy,
                };
                if let Err(e) = s.save(&ctx.cache_dir) {
                    return AutoOutcome::Failed {
                        error: format!("saving the check: {e:#}"),
                    };
                }
                (s, true)
            }
        };
    let held: Option<String> = match &state.decision {
        Decision::UpToDate => None,
        _ if !ctx.may_apply => {
            Some("nothing here can restart the daemon; run `attempt update`".into())
        }
        Decision::Required(_) => None,
        Decision::Optional(_) if ctx.mode == AutoUpdate::Required => {
            Some("auto_update is `required` and this release is optional".into())
        }
        Decision::Optional(_) if !ctx.quiet => Some("waiting for a quiet moment".into()),
        Decision::Optional(_) => None,
    };
    let target = match (&state.decision, &held) {
        (Decision::UpToDate, _) | (_, Some(_)) => {
            return AutoOutcome::Checked {
                decision: state.decision,
                fetched,
                held,
            };
        }
        (Decision::Optional(v) | Decision::Required(v), None) => v.clone(),
    };
    let opts = UpdateOptions {
        version: Some(target.clone()),
        ..ctx.opts.clone()
    };
    match run(&opts, check) {
        Ok(report) => {
            if matches!(report.outcome, Outcome::Updated { .. }) {
                // The file on disk is the new release; this process is not.
                // Record the new version so the next tick does not try again.
                let _ = CheckState {
                    checked_at: unix_now(),
                    current: target,
                    policy: state.policy,
                    decision: Decision::UpToDate,
                }
                .save(&ctx.cache_dir);
            }
            AutoOutcome::Applied { report }
        }
        Err(e) => AutoOutcome::Failed {
            error: format!("{e:#}"),
        },
    }
}

/// The check the daemon and `attempt maintenance` apply to a staged binary:
/// it prints its version, and when a database exists here it opens it —
/// the failure an update must catch is a binary that runs but cannot read
/// our files.
pub fn health_check_for(locator: &crate::locator::Locator) -> impl Fn(&Path) -> Result<()> {
    let data_dir =
        crate::service::is_portable(&locator.paths).then(|| locator.paths.data_dir.clone());
    let db_dir =
        (locator.source != crate::locator::DbSource::Default).then(|| locator.db_dir.clone());
    let db_exists = attemptdb_storage::Database::exists(&locator.db_dir);
    move |bin: &Path| {
        let out = run_with_timeout(Command::new(bin).arg("--version"), HEALTH_TIMEOUT)
            .with_context(|| format!("{} --version", bin.display()))?;
        if out.trim().is_empty() {
            bail!("{} --version printed nothing", bin.display());
        }
        if db_exists {
            let mut cmd = Command::new(bin);
            if let Some(d) = &data_dir {
                cmd.arg("--data-dir").arg(d);
            }
            if let Some(d) = &db_dir {
                cmd.arg("--db").arg(d);
            }
            cmd.args(["status", "--json"]);
            run_with_timeout(&mut cmd, HEALTH_TIMEOUT)
                .with_context(|| format!("{} status --json (open the database)", bin.display()))?;
        }
        Ok(())
    }
}

const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Run `cmd`, killing it after `timeout`. Returns stdout on exit 0.
pub fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<String> {
    use std::process::Stdio;
    let mut child = spawn_executable(
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .with_context(|| format!("spawning {:?}", cmd.get_program()))?;
    let started = std::time::Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            err.lines().next().unwrap_or("").trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Spawn a just-written executable, retrying briefly while Linux reports
/// `ETXTBSY`.
///
/// Linux refuses to `execve` a file that any process still holds open for
/// writing. Nothing in the update path keeps the staged binary open — `fs::copy`
/// closes both ends before it returns — but spawning a process forks, and a
/// child forked by one thread inherits every descriptor open at that instant,
/// including a write handle another thread is about to close. That inherited
/// handle keeps the file "being written" until the child execs, and a health
/// check landing inside that window fails with "Text file busy".
///
/// The window is milliseconds and closes on its own, so the answer is a short
/// bounded retry. Failing an update with `ETXTBSY` is not: the binary is
/// perfectly good and the caller would have no idea what to do about it.
pub fn spawn_executable(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match cmd.spawn() {
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            other => return other,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// `tag_name` from a GitHub release JSON document, without a leading `v`.
pub fn parse_release_tag(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let tag = v.get("tag_name")?.as_str()?;
    Some(tag.trim_start_matches('v').to_string())
}

/// `attempt-<version>-<target>`.
pub fn asset_stem(version: &str, target: &str) -> String {
    format!("attempt-{}-{target}", version.trim_start_matches('v'))
}

/// The archive name for a target (zip on Windows, tar.gz elsewhere).
pub fn asset_name(version: &str, target: &str) -> String {
    let stem = asset_stem(version, target);
    if target.contains("windows") {
        format!("{stem}.zip")
    } else {
        format!("{stem}.tar.gz")
    }
}

/// The digest listed for `asset` in a `SHA256SUMS` file (`<hex>  <name>`).
pub fn expected_digest(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let digest = parts.next()?;
        let name = parts.next()?;
        (name.trim_start_matches("./") == asset && digest.len() == 64)
            .then(|| digest.to_ascii_lowercase())
    })
}

/// Hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// `(major, minor, patch, pre-release)`; a pre-release sorts below the
/// release with the same numbers. Unparseable versions compare as `None`.
fn parse_version(v: &str) -> Option<(u64, u64, u64, Option<String>)> {
    let v = v.trim().trim_start_matches('v');
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p.to_string())),
        None => (v, None),
    };
    let core = core.split_once('+').map(|(c, _)| c).unwrap_or(core);
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch, pre))
}

/// True when `candidate` is strictly newer than `current`.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    match (parse_version(current), parse_version(candidate)) {
        (Some(a), Some(b)) => {
            let ka = (a.0, a.1, a.2, a.3.is_none());
            let kb = (b.0, b.1, b.2, b.3.is_none());
            if ka != kb {
                return kb > ka;
            }
            match (a.3, b.3) {
                (Some(pa), Some(pb)) => pb > pa,
                _ => false,
            }
        }
        _ => current.trim_start_matches('v') != candidate.trim_start_matches('v'),
    }
}

/// The package manager that owns `path`, with its upgrade command, when the
/// path is one a manager writes to.
pub fn managed_by(path: &Path) -> Option<(&'static str, &'static str)> {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.contains("/Cellar/") || s.contains("/homebrew/") || s.contains("/linuxbrew/") {
        return Some(("Homebrew", "brew upgrade attempt"));
    }
    if s.contains("/.cargo/bin/") {
        return Some((
            "cargo",
            "cargo install --git https://github.com/nullarch/attemptdb attempt",
        ));
    }
    if s.contains("/scoop/") {
        return Some(("Scoop", "scoop update attempt"));
    }
    if s.contains("/nix/store/") {
        return Some(("Nix", "your Nix configuration"));
    }
    None
}

/// Paths used around a binary: staged new file, kept previous, failed new.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slots {
    pub current: PathBuf,
    pub new: PathBuf,
    pub prev: PathBuf,
    pub failed: PathBuf,
    pub staging: PathBuf,
}

pub fn slots(binary: &Path) -> Slots {
    let dir = binary.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = binary
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "attempt".to_string());
    // `.exe` stays last so Windows still treats the copies as executables.
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, "exe")) => (s.to_string(), ".exe".to_string()),
        _ => (name.clone(), String::new()),
    };
    Slots {
        current: binary.to_path_buf(),
        new: dir.join(format!("{stem}.new{ext}")),
        prev: dir.join(format!("{stem}.prev{ext}")),
        failed: dir.join(format!("{stem}.failed{ext}")),
        staging: dir.join(format!(".{stem}-update-{}", std::process::id())),
    }
}

/// Swap a staged binary into place with a health check on both sides.
///
/// - the staged file fails its check → it is removed, nothing else changes;
/// - the swap itself fails half-way → the previous binary is put back;
/// - the swapped binary fails its check → it is moved to `.failed` and the
///   previous binary is restored (`Outcome::RolledBack`).
pub fn swap_with_rollback(slots: &Slots, check: HealthCheck) -> Result<Outcome> {
    if let Err(e) = check(&slots.new) {
        let _ = fs::remove_file(&slots.new);
        bail!("the downloaded binary failed its health check; nothing was changed: {e:#}");
    }
    let _ = fs::remove_file(&slots.prev);
    fs::rename(&slots.current, &slots.prev)
        .with_context(|| format!("moving {} aside", slots.current.display()))?;
    if let Err(e) = fs::rename(&slots.new, &slots.current) {
        // Put the old one back before reporting.
        let restore = fs::rename(&slots.prev, &slots.current);
        let _ = fs::remove_file(&slots.new);
        return Err(match restore {
            Ok(()) => anyhow!(
                "installing the new binary failed ({e}); the previous binary is back in place"
            ),
            Err(r) => anyhow!(
                "installing the new binary failed ({e}) AND restoring the previous one failed ({r}); it is at {}",
                slots.prev.display()
            ),
        });
    }
    if let Err(e) = check(&slots.current) {
        let _ = fs::remove_file(&slots.failed);
        let moved = fs::rename(&slots.current, &slots.failed);
        let restored = fs::rename(&slots.prev, &slots.current);
        return match (moved, restored) {
            (_, Ok(())) => Ok(Outcome::RolledBack {
                reason: format!("{e:#}"),
            }),
            (_, Err(r)) => Err(anyhow!(
                "the new binary failed its health check ({e:#}) and restoring the previous one failed ({r}); it is at {}",
                slots.prev.display()
            )),
        };
    }
    Ok(Outcome::Updated {
        previous: slots.prev.clone(),
    })
}

/// Undo the last update: `<bin>.prev` becomes `<bin>` again. The binary
/// being replaced is kept as `<bin>.failed` so a rollback is itself
/// reversible.
pub fn rollback(binary: &Path) -> Result<PathBuf> {
    let s = slots(binary);
    if !s.prev.is_file() {
        bail!(
            "nothing to roll back to: {} does not exist",
            s.prev.display()
        );
    }
    let _ = fs::remove_file(&s.failed);
    fs::rename(&s.current, &s.failed)
        .with_context(|| format!("moving {} aside", s.current.display()))?;
    if let Err(e) = fs::rename(&s.prev, &s.current) {
        let _ = fs::rename(&s.failed, &s.current);
        return Err(e).with_context(|| format!("restoring {}", s.prev.display()));
    }
    if let Some(dir) = binary.parent() {
        let _ = rollback_hook_binary(dir);
    }
    Ok(s.failed)
}

// ---------------------------------------------------------------------------
// Network and archive steps
// ---------------------------------------------------------------------------

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(120))
        .user_agent(&format!(
            "attempt/{CURRENT_VERSION} (+https://github.com/{REPO})"
        ))
        .build()
}

fn latest_via_api(agent: &ureq::Agent, opts: &UpdateOptions) -> Result<String> {
    let url = format!(
        "{}/repos/{REPO}/releases/latest",
        opts.api_base.trim_end_matches('/')
    );
    let body = agent
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(404, _) => anyhow!(
                "no release found at {url} (is the repository public and a release published?)"
            ),
            other => anyhow!("resolving the latest release: {other}"),
        })?
        .into_string()?;
    parse_release_tag(&body).ok_or_else(|| anyhow!("unexpected release document from {url}"))
}

fn download(agent: &ureq::Agent, url: &str, dest: &Path) -> Result<()> {
    let resp = agent.get(url).call().map_err(|e| match e {
        ureq::Error::Status(404, _) => anyhow!("{url}: not found"),
        other => anyhow!("{url}: {other}"),
    })?;
    let mut reader = resp.into_reader().take(MAX_ASSET_BYTES + 1);
    let mut file =
        fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let copied = std::io::copy(&mut reader, &mut file)?;
    file.flush()?;
    if copied > MAX_ASSET_BYTES {
        bail!("{url}: larger than {} bytes; refusing", MAX_ASSET_BYTES);
    }
    Ok(())
}

/// Extract the release archive with the platform's `tar` (present on macOS,
/// Linux, and Windows 10+, where bsdtar also reads zip files) and return the
/// extracted binary.
pub fn extract(archive: &Path, dest: &Path, stem: &str) -> Result<PathBuf> {
    fs::create_dir_all(dest)?;
    let status = Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .status()
        .context("running `tar` (it is needed to unpack the release archive)")?;
    if !status.success() {
        bail!("`tar` failed to extract {}", archive.display());
    }
    let name = if cfg!(windows) {
        "attempt.exe"
    } else {
        "attempt"
    };
    let bin = dest.join(stem).join(name);
    if !bin.is_file() {
        bail!("the archive did not contain {stem}/{name}");
    }
    Ok(bin)
}

/// The dedicated hook executable's name on this platform.
pub fn hook_binary_name() -> &'static str {
    if cfg!(windows) {
        "attempt-hook.exe"
    } else {
        "attempt-hook"
    }
}

/// Where an extracted archive would hold `attempt-hook`, if it shipped one.
pub fn extracted_hook_binary(dest: &Path, stem: &str) -> Option<PathBuf> {
    let p = dest.join(stem).join(hook_binary_name());
    p.is_file().then_some(p)
}

/// Put the archive's `attempt-hook` next to `attempt`: stage, then rename
/// over the old one (kept as `attempt-hook.prev`). Hooks referencing the
/// path keep working through the rename; a hook that starts mid-swap runs
/// either the old or the new binary, both of which speak the same spool
/// format. Returns the installed path.
pub fn install_hook_binary(dir: &Path, extracted: &Path) -> Result<PathBuf> {
    let current = dir.join(hook_binary_name());
    let s = slots(&current);
    let _ = fs::remove_file(&s.new);
    fs::copy(extracted, &s.new).with_context(|| format!("staging {}", s.new.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&s.new, fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(&s.new)
            .output();
    }
    if s.current.is_file() {
        let _ = fs::remove_file(&s.prev);
        fs::rename(&s.current, &s.prev).with_context(|| format!("keeping {}", s.prev.display()))?;
    }
    fs::rename(&s.new, &s.current)
        .with_context(|| format!("installing {}", s.current.display()))?;
    Ok(s.current)
}

/// Undo [`install_hook_binary`] when `attempt-hook.prev` exists.
pub fn rollback_hook_binary(dir: &Path) -> Option<PathBuf> {
    let s = slots(&dir.join(hook_binary_name()));
    if !s.prev.is_file() {
        return None;
    }
    let _ = fs::remove_file(&s.failed);
    if s.current.is_file() && fs::rename(&s.current, &s.failed).is_err() {
        return None;
    }
    fs::rename(&s.prev, &s.current).ok().map(|_| s.current)
}

/// Download, verify, extract, stage, health-check, swap.
pub fn run(opts: &UpdateOptions, check: HealthCheck) -> Result<UpdateReport> {
    let binary = match &opts.binary {
        Some(p) => canonical_display_path(p),
        None => current_exe_path(),
    };
    let mut report = UpdateReport {
        binary: binary.clone(),
        target: TARGET.to_string(),
        current: CURRENT_VERSION.to_string(),
        resolved: String::new(),
        required: false,
        outcome: Outcome::UpToDate,
        notes: Vec::new(),
    };
    if let Some((manager, cmd)) = managed_by(&binary) {
        report.outcome = Outcome::Refused {
            reason: format!(
                "{} is managed by {manager}; update with `{cmd}`",
                binary.display()
            ),
        };
        return Ok(report);
    }
    if TARGET == "unknown" || TARGET.is_empty() {
        report.outcome = Outcome::Refused {
            reason: "this build does not know its target triple; reinstall from a release".into(),
        };
        return Ok(report);
    }
    let agent = agent();
    let (resolved, required) = match &opts.version {
        Some(v) => (v.trim_start_matches('v').to_string(), false),
        None => {
            let policy = fetch_policy(&agent, opts)?;
            let required = matches!(decide(CURRENT_VERSION, &policy), Decision::Required(_));
            (policy.latest, required)
        }
    };
    report.resolved = resolved.clone();
    report.required = required;
    if !opts.force && !is_newer(CURRENT_VERSION, &resolved) {
        report.outcome = Outcome::UpToDate;
        return Ok(report);
    }
    if opts.check_only {
        report.outcome = Outcome::Available;
        return Ok(report);
    }

    let s = slots(&binary);
    let dir = binary
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", binary.display()))?;
    // Fail early on a read-only install directory rather than after a download.
    let probe = dir.join(format!(".attempt-write-probe-{}", std::process::id()));
    fs::write(&probe, b"").with_context(|| {
        format!(
            "{} is not writable; run the update as the user that installed attempt",
            dir.display()
        )
    })?;
    let _ = fs::remove_file(&probe);

    let _ = fs::remove_dir_all(&s.staging);
    fs::create_dir_all(&s.staging)?;
    let mut hook_note: Option<String> = None;
    let result = (|| -> Result<Outcome> {
        let stem = asset_stem(&resolved, TARGET);
        let asset = asset_name(&resolved, TARGET);
        let base = format!(
            "{}/{REPO}/releases/download/v{resolved}",
            opts.download_base.trim_end_matches('/')
        );
        let archive = s.staging.join(&asset);
        let sums = s.staging.join("SHA256SUMS");
        download(&agent, &format!("{base}/{asset}"), &archive)
            .with_context(|| format!("no release asset for {TARGET} in v{resolved}"))?;
        download(&agent, &format!("{base}/SHA256SUMS"), &sums).with_context(|| {
            format!("v{resolved} publishes no SHA256SUMS; refusing an unverifiable binary")
        })?;
        let expected = expected_digest(&fs::read_to_string(&sums)?, &asset)
            .ok_or_else(|| anyhow!("{asset} is not listed in SHA256SUMS"))?;
        let actual = sha256_file(&archive)?;
        if actual != expected {
            bail!("checksum mismatch for {asset}\n  expected {expected}\n  actual   {actual}");
        }
        let extracted = extract(&archive, &s.staging, &stem)?;
        let _ = fs::remove_file(&s.new);
        fs::copy(&extracted, &s.new).with_context(|| format!("staging {}", s.new.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&s.new, fs::Permissions::from_mode(0o755))?;
        }
        #[cfg(target_os = "macos")]
        {
            let _ = Command::new("xattr")
                .args(["-d", "com.apple.quarantine"])
                .arg(&s.new)
                .output();
        }
        let outcome = swap_with_rollback(&s, check)?;
        // The pair stays in step: a release that ships `attempt-hook` puts
        // it next to `attempt`, whether or not one was there before.
        if matches!(outcome, Outcome::Updated { .. })
            && let Some(hook) = extracted_hook_binary(&s.staging, &stem)
        {
            match install_hook_binary(dir, &hook) {
                Ok(p) => hook_note = Some(format!("{} updated alongside", p.display())),
                Err(e) => hook_note = Some(format!("attempt-hook was NOT updated: {e:#}")),
            }
        }
        Ok(outcome)
    })();
    let _ = fs::remove_dir_all(&s.staging);
    report.outcome = result?;
    if let Some(n) = hook_note.take() {
        report.notes.push(n);
    }
    match &report.outcome {
        Outcome::Updated { previous } => report.notes.push(format!(
            "previous binary kept at {} — `attempt update --rollback` restores it",
            previous.display()
        )),
        Outcome::RolledBack { .. } => report.notes.push(format!(
            "the failed binary is at {} for inspection",
            s.failed.display()
        )),
        _ => {}
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hook_binary_is_installed_beside_attempt_and_rolls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let staged = dir.join("extracted");
        fs::create_dir_all(staged.join("stem")).unwrap();
        assert!(extracted_hook_binary(&staged, "stem").is_none());
        let shipped = staged.join("stem").join(hook_binary_name());
        fs::write(&shipped, b"v2").unwrap();
        assert_eq!(
            extracted_hook_binary(&staged, "stem"),
            Some(shipped.clone())
        );

        // First install: nothing to keep.
        let installed = install_hook_binary(dir, &shipped).unwrap();
        assert_eq!(installed, dir.join(hook_binary_name()));
        assert_eq!(fs::read(&installed).unwrap(), b"v2");
        assert!(rollback_hook_binary(dir).is_none(), "no previous copy yet");

        // Second install keeps the previous copy; rollback restores it.
        fs::write(&shipped, b"v3").unwrap();
        install_hook_binary(dir, &shipped).unwrap();
        assert_eq!(fs::read(&installed).unwrap(), b"v3");
        let prev = slots(&installed).prev;
        assert_eq!(fs::read(&prev).unwrap(), b"v2");
        assert_eq!(rollback_hook_binary(dir), Some(installed.clone()));
        assert_eq!(fs::read(&installed).unwrap(), b"v2");
        assert!(!prev.exists());
    }

    #[test]
    fn release_tag_asset_names_and_digest_lines_parse() {
        assert_eq!(
            parse_release_tag(r#"{"tag_name":"v0.2.0","name":"x"}"#).as_deref(),
            Some("0.2.0")
        );
        assert_eq!(parse_release_tag(r#"{"message":"Not Found"}"#), None);
        assert_eq!(
            asset_name("v0.2.0", "x86_64-unknown-linux-musl"),
            "attempt-0.2.0-x86_64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            asset_name("0.2.0", "x86_64-pc-windows-msvc"),
            "attempt-0.2.0-x86_64-pc-windows-msvc.zip"
        );
        let sums = "aaaa  attempt-0.2.0-aarch64-apple-darwin.tar.gz\n\
                    0123456789abcdef0123456789abcdef0123456789abcdef0123456789ABCDEF  ./attempt-0.2.0-x86_64-pc-windows-msvc.zip\n";
        assert_eq!(
            expected_digest(sums, "attempt-0.2.0-x86_64-pc-windows-msvc.zip").as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            expected_digest(sums, "attempt-0.2.0-aarch64-apple-darwin.tar.gz"),
            None,
            "short digest is not accepted"
        );
        assert_eq!(expected_digest(sums, "other"), None);
    }

    #[test]
    fn the_policy_decides_required_optional_or_up_to_date() {
        let p = Policy {
            latest: "0.2.8".into(),
            required_below: Some("0.2.4".into()),
            min_sync_version: Some(1),
            notes: None,
        };
        assert_eq!(decide("0.2.8", &p), Decision::UpToDate);
        assert_eq!(
            decide("0.2.9", &p),
            Decision::UpToDate,
            "ahead of the policy is up to date"
        );
        assert_eq!(decide("0.2.5", &p), Decision::Optional("0.2.8".into()));
        assert_eq!(
            decide("0.2.4", &p),
            Decision::Optional("0.2.8".into()),
            "the floor itself is fine"
        );
        assert_eq!(decide("0.2.3", &p), Decision::Required("0.2.8".into()));
        let no_floor = Policy {
            latest: "0.2.8".into(),
            ..Default::default()
        };
        assert_eq!(
            decide("0.1.0", &no_floor),
            Decision::Optional("0.2.8".into())
        );
    }

    #[test]
    fn a_policy_document_parses_with_only_a_version() {
        let p: Policy = serde_json::from_str(r#"{"latest":"v0.2.8"}"#).unwrap();
        assert_eq!(p.latest, "v0.2.8");
        assert_eq!(p.required_below, None);
        let full: Policy = serde_json::from_str(
            r#"{"latest":"0.2.8","required_below":"0.2.4","min_sync_version":1,"notes":"https://x"}"#,
        )
        .unwrap();
        assert_eq!(full.min_sync_version, Some(1));
        assert_eq!(
            policy_url("https://github.com/"),
            "https://github.com/nullarch/attemptdb/releases/latest/download/update.json"
        );
    }

    #[test]
    fn the_check_state_round_trips_and_ages() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(CheckState::load(tmp.path()).is_none());
        let s = CheckState {
            checked_at: unix_now() - 10,
            current: CURRENT_VERSION.into(),
            policy: Policy {
                latest: "9.9.9".into(),
                ..Default::default()
            },
            decision: Decision::Optional("9.9.9".into()),
        };
        s.save(tmp.path()).unwrap();
        let back = CheckState::load(tmp.path()).unwrap();
        assert_eq!(back.decision, s.decision);
        assert!(back.age() >= Duration::from_secs(10));
        assert!(back.is_current(CHECK_INTERVAL));
        assert!(
            !back.is_current(Duration::from_secs(5)),
            "older than the interval"
        );
        let other = CheckState {
            current: "0.0.1".into(),
            ..s
        };
        assert!(
            !other.is_current(CHECK_INTERVAL),
            "a check for another binary does not count"
        );
    }

    #[test]
    fn a_tick_honours_the_mode_the_environment_and_the_moment_without_a_request() {
        use crate::config::AutoUpdate;
        let tmp = tempfile::tempdir().unwrap();
        // A fresh decision on disk: no request is made, so the outcome is
        // decided entirely by mode and moment.
        let fresh = |decision: Decision| CheckState {
            checked_at: unix_now(),
            current: CURRENT_VERSION.into(),
            policy: Policy {
                latest: "9.9.9".into(),
                required_below: Some("9.0.0".into()),
                ..Default::default()
            },
            decision,
        };
        let ctx = |mode, quiet, may_apply| AutoContext {
            cache_dir: tmp.path().to_path_buf(),
            mode,
            quiet,
            may_apply,
            check_interval: CHECK_INTERVAL,
            opts: UpdateOptions {
                download_base: "http://127.0.0.1:9".into(),
                api_base: "http://127.0.0.1:9".into(),
                ..Default::default()
            },
        };
        let never = |_: &Path| -> Result<()> { panic!("no health check without an apply") };

        fresh(Decision::Required("9.9.9".into()))
            .save(tmp.path())
            .unwrap();
        assert!(matches!(
            auto_tick(&ctx(AutoUpdate::Off, true, true), &never),
            AutoOutcome::Disabled
        ));
        match auto_tick(&ctx(AutoUpdate::On, true, false), &never) {
            AutoOutcome::Checked {
                decision: Decision::Required(_),
                fetched: false,
                held: Some(h),
            } => {
                assert!(h.contains("attempt update"), "{h}")
            }
            other => panic!("{other:?}"),
        }

        fresh(Decision::Optional("9.9.9".into()))
            .save(tmp.path())
            .unwrap();
        match auto_tick(&ctx(AutoUpdate::On, false, true), &never) {
            AutoOutcome::Checked { held: Some(h), .. } => assert!(h.contains("quiet"), "{h}"),
            other => panic!("{other:?}"),
        }
        match auto_tick(&ctx(AutoUpdate::Required, true, true), &never) {
            AutoOutcome::Checked { held: Some(h), .. } => assert!(h.contains("optional"), "{h}"),
            other => panic!("{other:?}"),
        }

        fresh(Decision::UpToDate).save(tmp.path()).unwrap();
        assert!(matches!(
            auto_tick(&ctx(AutoUpdate::On, true, true), &never),
            AutoOutcome::Checked {
                decision: Decision::UpToDate,
                held: None,
                ..
            }
        ));

        // The environment switch wins over everything.
        // SAFETY: tests in this module do not run this variable-dependent code concurrently.
        unsafe { std::env::set_var("ATTEMPTDB_NO_AUTO_UPDATE", "1") };
        assert!(auto_update_disabled_by_env());
        assert!(matches!(
            auto_tick(&ctx(AutoUpdate::On, true, true), &never),
            AutoOutcome::Disabled
        ));
        unsafe { std::env::set_var("ATTEMPTDB_NO_AUTO_UPDATE", "0") };
        assert!(!auto_update_disabled_by_env());
        unsafe { std::env::remove_var("ATTEMPTDB_NO_AUTO_UPDATE") };
    }

    #[test]
    fn version_ordering_follows_semver_with_prereleases_below_releases() {
        assert!(is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.1.0", "v0.2.0"));
        assert!(is_newer("0.9.9", "1.0.0"));
        assert!(!is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(is_newer("0.2.0-rc.1", "0.2.0"));
        assert!(!is_newer("0.2.0", "0.2.0-rc.1"));
        assert!(is_newer("0.2.0-rc.1", "0.2.0-rc.2"));
        assert!(is_newer("0.1.0+build5", "0.1.1"));
        // Unparseable: any different string counts as an update candidate.
        assert!(is_newer("0.1.0", "nightly"));
        assert!(!is_newer("nightly", "nightly"));
    }

    #[test]
    fn package_managed_paths_are_recognised() {
        assert_eq!(
            managed_by(Path::new("/opt/homebrew/Cellar/attempt/0.1.0/bin/attempt")).map(|m| m.0),
            Some("Homebrew")
        );
        assert_eq!(
            managed_by(Path::new("/home/dev/.cargo/bin/attempt")).map(|m| m.0),
            Some("cargo")
        );
        assert_eq!(
            managed_by(Path::new(
                r"C:\Users\dev\scoop\apps\attempt\current\attempt.exe"
            ))
            .map(|m| m.0),
            Some("Scoop")
        );
        assert_eq!(managed_by(Path::new("/home/dev/.local/bin/attempt")), None);
        assert_eq!(
            managed_by(Path::new(r"C:\Users\dev\.local\bin\attempt.exe")),
            None
        );
    }

    #[test]
    fn slots_keep_the_exe_suffix_last() {
        let s = slots(Path::new(r"C:\tools\attempt.exe"));
        assert!(s.new.to_string_lossy().ends_with("attempt.new.exe"));
        assert!(s.prev.to_string_lossy().ends_with("attempt.prev.exe"));
        let s = slots(Path::new("/home/dev/.local/bin/attempt"));
        assert_eq!(s.new, PathBuf::from("/home/dev/.local/bin/attempt.new"));
        assert_eq!(s.prev, PathBuf::from("/home/dev/.local/bin/attempt.prev"));
        assert_eq!(
            s.failed,
            PathBuf::from("/home/dev/.local/bin/attempt.failed")
        );
    }

    fn contents(p: &Path) -> String {
        fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn swap_keeps_the_previous_binary_and_rollback_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("attempt");
        fs::write(&bin, "old").unwrap();
        let s = slots(&bin);
        fs::write(&s.new, "new").unwrap();
        let ok: HealthCheck = &|_p| Ok(());
        let outcome = swap_with_rollback(&s, ok).unwrap();
        assert_eq!(
            outcome,
            Outcome::Updated {
                previous: s.prev.clone()
            }
        );
        assert_eq!(contents(&bin), "new");
        assert_eq!(contents(&s.prev), "old");
        assert!(!s.new.exists());

        let failed = rollback(&bin).unwrap();
        assert_eq!(contents(&bin), "old");
        assert_eq!(contents(&failed), "new");
        assert!(!s.prev.exists());
        assert!(rollback(&bin).is_err(), "nothing left to roll back to");
    }

    #[test]
    fn a_staged_binary_that_fails_its_check_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("attempt");
        fs::write(&bin, "old").unwrap();
        let s = slots(&bin);
        fs::write(&s.new, "broken").unwrap();
        let reject_staged: HealthCheck = &|p| {
            if contents(p) == "broken" {
                bail!("exit 1")
            } else {
                Ok(())
            }
        };
        let err = swap_with_rollback(&s, reject_staged).unwrap_err();
        assert!(err.to_string().contains("nothing was changed"), "{err}");
        assert_eq!(contents(&bin), "old");
        assert!(!s.new.exists());
        assert!(!s.prev.exists());
    }

    #[test]
    fn a_swapped_binary_that_fails_its_check_is_rolled_back() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("attempt");
        fs::write(&bin, "old").unwrap();
        let s = slots(&bin);
        fs::write(&s.new, "new").unwrap();
        // Passes as the staged file, fails once it sits at the real path —
        // the shape of "runs, but cannot open this database".
        let final_path = bin.clone();
        let reject_final: HealthCheck = &|p| {
            if p == final_path {
                bail!("cannot open the database")
            } else {
                Ok(())
            }
        };
        let outcome = swap_with_rollback(&s, reject_final).unwrap();
        assert!(
            matches!(outcome, Outcome::RolledBack { ref reason } if reason.contains("database"))
        );
        assert_eq!(contents(&bin), "old");
        assert_eq!(contents(&s.failed), "new");
        assert!(!s.prev.exists());
        assert!(!s.new.exists());
    }
}
