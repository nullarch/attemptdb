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
//! What is trusted, and how far:
//!
//! - the version a release names (`update.json`, the API's tag, `--to`) must
//!   be a strict semantic version before it is compared, spliced into a URL
//!   or used in a path ([`valid_release_version`]);
//! - every download URL is built here from the repository, that version and
//!   a fixed asset name ([`release_asset_url`]), and a redirect is followed
//!   only to the same origin or, for GitHub, to GitHub's own hosts over
//!   https ([`redirect_allowed`]) — the same rule for the archive and for
//!   `SHA256SUMS`;
//! - `attempt-hook` is staged and run (`--version`) *before* anything is
//!   swapped, checked again once installed, and a failure at either point
//!   restores both binaries: a broken hook binary would break every agent
//!   call.
//!
//! Binaries managed by a package manager (Homebrew, cargo, Scoop, Nix, or
//! anything named in `ATTEMPTDB_MANAGED_BY`) are refused with the manager's
//! own upgrade command, and never auto-updated: two writers to one path is
//! how installs rot.

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
/// `SHA256SUMS` lists a handful of files.
const MAX_SUMS_BYTES: u64 = 1024 * 1024;
/// `update.json` and the API's release document are a few hundred bytes.
const MAX_POLICY_BYTES: u64 = 1024 * 1024;
/// Redirects followed per download (GitHub uses one or two).
const MAX_REDIRECTS: usize = 5;

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
///
/// The document is untrusted input: `latest` must be a strict semantic
/// version (it becomes part of a download URL), and a floor that is not one
/// is dropped rather than believed.
pub fn fetch_policy(agent: &ureq::Agent, opts: &UpdateOptions) -> Result<Policy> {
    let url = policy_url(&opts.download_base);
    let Some(resp) = fetch(agent, &url)? else {
        return Ok(Policy {
            latest: latest_via_api(agent, opts)?,
            ..Default::default()
        });
    };
    let body = read_limited(resp, MAX_POLICY_BYTES).with_context(|| url.clone())?;
    let mut p: Policy = serde_json::from_str(&body)
        .with_context(|| format!("{url}: not a release policy document"))?;
    p.latest = valid_release_version(&p.latest).ok_or_else(|| {
        anyhow!(
            "{url}: the policy names {:?}, which is not a release version; refusing",
            clip(&p.latest)
        )
    })?;
    p.required_below = p.required_below.as_deref().and_then(valid_release_version);
    Ok(p)
}

/// At most 80 characters of untrusted text, for an error message.
fn clip(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(80).collect()
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
    // A package manager owns this file: it updates it, we do not. No request
    // is made either — there is nothing this process could do with the answer.
    let binary = match &ctx.opts.binary {
        Some(p) => canonical_display_path(p),
        None => current_exe_path(),
    };
    if managed_install(&binary).is_some() {
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
/// it prints its version, and when a database exists here it reads it — the
/// failure an update must catch is a binary that runs but cannot read our
/// files.
pub fn health_check_for(locator: &crate::locator::Locator) -> impl Fn(&Path) -> Result<()> {
    health_check_with(locator, true)
}

/// [`health_check_for`], optionally without the database step
/// (`attempt update --no-health-check`; the version is still checked).
///
/// The database step is `attempt health`, not `attempt status`: `status`
/// opens the whole database, which on a large history takes longer than any
/// sensible timeout and says nothing the manifest does not. `health` loads
/// the identity and the newest valid manifest generation read-only — enough
/// to catch a format this binary cannot read — and answers in milliseconds.
/// A binary that predates `health` (an older release installed with `--to`)
/// is held to its version alone.
pub fn health_check_with(
    locator: &crate::locator::Locator,
    open_database: bool,
) -> impl Fn(&Path) -> Result<()> {
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
        if open_database && db_exists {
            let mut cmd = Command::new(bin);
            if let Some(d) = &data_dir {
                cmd.arg("--data-dir").arg(d);
            }
            if let Some(d) = &db_dir {
                cmd.arg("--db").arg(d);
            }
            cmd.arg("health");
            match run_with_timeout(&mut cmd, HEALTH_TIMEOUT) {
                Ok(_) => {}
                // clap's own refusal of an unknown subcommand: this build
                // has no `health`, so there is nothing more to ask it.
                Err(e) if format!("{e:#}").contains("unrecognized subcommand") => {}
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("{} health (read the database)", bin.display()));
                }
            }
        }
        Ok(())
    }
}

/// The light checks are quick; this only bounds a hung process.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(20);

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
/// `None` when the document has none or the tag is not a release version.
pub fn parse_release_tag(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    valid_release_version(v.get("tag_name")?.as_str()?)
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

/// A release version, strictly: `MAJOR.MINOR.PATCH`, optionally
/// `-<pre-release>` and `+<build>` (semver 2.0), with at most one leading
/// `v`. Returns it without the `v`. Anything else — whitespace, a path, a
/// query, `nightly`, `latest`, a fourth number, leading zeros — is `None`:
/// the value ends up in a download URL and a file name, so it must not be
/// able to carry anything but a version.
pub fn valid_release_version(v: &str) -> Option<String> {
    let bare = v.strip_prefix('v').unwrap_or(v);
    if bare.starts_with('v') {
        return None;
    }
    parse_version(bare).map(|_| bare.to_string())
}

/// `(major, minor, patch, pre-release)`; a pre-release sorts below the
/// release with the same numbers. A leading `v` is accepted; anything that
/// is not a strict semantic version (see [`valid_release_version`]) is `None`.
fn parse_version(v: &str) -> Option<(u64, u64, u64, Option<String>)> {
    fn numeric(part: &str) -> Option<u64> {
        let ok = !part.is_empty()
            && part.len() <= 9
            && part.bytes().all(|b| b.is_ascii_digit())
            && (part == "0" || !part.starts_with('0'));
        ok.then(|| part.parse().ok()).flatten()
    }
    fn identifiers(text: &str, numeric_without_zeros: bool) -> bool {
        !text.is_empty()
            && text.len() <= 32
            && text.split('.').all(|id| {
                !id.is_empty()
                    && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    && (!numeric_without_zeros
                        || !id.bytes().all(|b| b.is_ascii_digit())
                        || id == "0"
                        || !id.starts_with('0'))
            })
    }
    let v = v.strip_prefix('v').unwrap_or(v);
    if v.len() > 64 {
        return None;
    }
    let (rest, build) = match v.split_once('+') {
        Some((r, b)) => (r, Some(b)),
        None => (v, None),
    };
    if build.is_some_and(|b| !identifiers(b, false)) {
        return None;
    }
    let (core, pre) = match rest.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (rest, None),
    };
    if pre.is_some_and(|p| !identifiers(p, true)) {
        return None;
    }
    let mut it = core.split('.');
    let major = numeric(it.next()?)?;
    let minor = numeric(it.next()?)?;
    let patch = numeric(it.next()?)?;
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch, pre.map(str::to_string)))
}

/// True when `candidate` is strictly newer than `current`. Either side not
/// being a strict semantic version is never "newer": an unparseable name
/// must not trigger an install.
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
        _ => false,
    }
}

/// The package manager that owns `path`, with its upgrade command, when the
/// path is one a manager writes to. Homebrew is matched on its prefixes
/// (`/opt/homebrew`, `/usr/local/Cellar` and `/usr/local/Homebrew`,
/// `/home/linuxbrew`) without regard to case.
pub fn managed_by(path: &Path) -> Option<(&'static str, &'static str)> {
    let s = path.to_string_lossy().replace('\\', "/");
    let lower = s.to_ascii_lowercase();
    if lower.contains("/cellar/") || lower.contains("/homebrew/") || lower.contains("/linuxbrew/") {
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

/// Who owns the binary at `path`, when something other than `attempt update`
/// does: a package manager by path ([`managed_by`]), or whatever the
/// installer of a managed machine named in `ATTEMPTDB_MANAGED_BY` (a value
/// that is empty or `0` means nobody). Returns the owner and how to update.
pub fn managed_install(path: &Path) -> Option<(String, String)> {
    if let Ok(v) = std::env::var("ATTEMPTDB_MANAGED_BY")
        && !v.trim().is_empty()
        && v != "0"
    {
        let owner = clip(v.trim());
        let how = format!("update it with {owner} (ATTEMPTDB_MANAGED_BY is set)");
        return Some((owner, how));
    }
    managed_by(path).map(|(m, cmd)| (m.to_string(), format!("update with `{cmd}`")))
}

// ---------------------------------------------------------------------------
// Where a download may come from
// ---------------------------------------------------------------------------

/// `<base>/<repo>/releases/download/v<version>/<name>` — the only shape a
/// release asset URL takes. `version` must already be a valid release
/// version and `name` an asset name from [`asset_name`] or `SHA256SUMS`.
pub fn release_asset_url(base: &str, version: &str, name: &str) -> String {
    format!(
        "{}/{REPO}/releases/download/v{version}/{name}",
        base.trim_end_matches('/')
    )
}

/// `scheme`, lower-cased `authority` (`host[:port]`) and the rest of a URL.
fn split_url(url: &str) -> Option<(&str, String, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    if url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    Some((scheme, authority.to_ascii_lowercase(), &rest[end..]))
}

fn host_of(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority;
    }
    authority.split(':').next().unwrap_or(authority)
}

/// Hosts GitHub serves releases from: the site, its API, and the CDN that
/// release assets are redirected to (`objects.githubusercontent.com`,
/// `release-assets.githubusercontent.com`, ...).
fn is_github_host(host: &str) -> bool {
    host == "github.com"
        || host == "api.github.com"
        || host == "githubusercontent.com"
        || host.ends_with(".githubusercontent.com")
}

/// May a request for `from` be answered with a redirect to `to`?
///
/// Yes to the same origin (a mirror or a local test server that redirects
/// within itself). Otherwise only from GitHub to GitHub's own hosts over
/// https. Never to another host, and never from https down to http.
pub fn redirect_allowed(from: &str, to: &str) -> bool {
    let (Some((from_scheme, from_auth, _)), Some((to_scheme, to_auth, _))) =
        (split_url(from), split_url(to))
    else {
        return false;
    };
    if from_scheme == "https" && to_scheme != "https" {
        return false;
    }
    if from_auth == to_auth {
        return true;
    }
    to_scheme == "https"
        && is_github_host(host_of(&from_auth))
        && is_github_host(host_of(&to_auth))
        && !to_auth.contains(':')
}

/// Resolve a `Location` header against the URL that answered with it.
fn resolve_location(current: &str, location: &str) -> Option<String> {
    let (scheme, authority, rest) = split_url(current)?;
    if location
        .chars()
        .any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    if location.contains("://") {
        return Some(location.to_string());
    }
    if let Some(network_path) = location.strip_prefix("//") {
        return Some(format!("{scheme}://{network_path}"));
    }
    if location.starts_with('/') {
        return Some(format!("{scheme}://{authority}{location}"));
    }
    let path = rest.split(['?', '#']).next().unwrap_or("");
    let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    Some(format!("{scheme}://{authority}{dir}/{location}"))
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
    // Redirects are followed by `fetch`, hop by hop, so that each one can be
    // checked against `redirect_allowed`.
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(120))
        .redirects(0)
        .user_agent(&format!(
            "attempt/{CURRENT_VERSION} (+https://github.com/{REPO})"
        ))
        .build()
}

/// GET `url`, following redirects only where [`redirect_allowed`] says so.
/// `Ok(None)` is a 404 at the end of the chain; a redirect anywhere else is
/// an error naming the host that was refused.
fn fetch(agent: &ureq::Agent, url: &str) -> Result<Option<ureq::Response>> {
    fetch_with(agent, url, &[])
}

fn fetch_with(
    agent: &ureq::Agent,
    url: &str,
    accept: &[(&str, &str)],
) -> Result<Option<ureq::Response>> {
    if split_url(url).is_none() {
        bail!("{url}: not an http(s) URL");
    }
    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let mut request = agent.get(&current);
        for (name, value) in accept {
            request = request.set(name, value);
        }
        let resp = match request.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(e) => bail!("{current}: {e}"),
        };
        if !(300..400).contains(&resp.status()) {
            return Ok(Some(resp));
        }
        let next = resp
            .header("location")
            .and_then(|l| resolve_location(&current, l))
            .ok_or_else(|| anyhow!("{current}: a redirect without a usable Location; refusing"))?;
        if !redirect_allowed(&current, &next) {
            bail!(
                "{current}: redirected to {}, which is not an allowed download host; refusing",
                clip(&next)
            );
        }
        current = next;
    }
    bail!("{url}: more than {MAX_REDIRECTS} redirects; refusing")
}

/// The body as text, at most `limit` bytes (more is an error, not a truncation).
fn read_limited(resp: ureq::Response, limit: u64) -> Result<String> {
    let mut body = String::new();
    resp.into_reader()
        .take(limit + 1)
        .read_to_string(&mut body)
        .context("reading the response")?;
    if body.len() as u64 > limit {
        bail!("the response is larger than {limit} bytes; refusing");
    }
    Ok(body)
}

fn latest_via_api(agent: &ureq::Agent, opts: &UpdateOptions) -> Result<String> {
    let url = format!(
        "{}/repos/{REPO}/releases/latest",
        opts.api_base.trim_end_matches('/')
    );
    let resp = fetch_with(agent, &url, &[("Accept", "application/vnd.github+json")])
        .map_err(|e| anyhow!("resolving the latest release: {e:#}"))?
        .ok_or_else(|| {
            anyhow!("no release found at {url} (is the repository public and a release published?)")
        })?;
    let body = read_limited(resp, MAX_POLICY_BYTES).with_context(|| url.clone())?;
    parse_release_tag(&body)
        .ok_or_else(|| anyhow!("unexpected release document from {url} (no valid tag_name)"))
}

/// Download `url` into `dest`, at most `max` bytes.
fn download(agent: &ureq::Agent, url: &str, dest: &Path, max: u64) -> Result<()> {
    let resp = fetch(agent, url)?.ok_or_else(|| anyhow!("{url}: not found"))?;
    let mut reader = resp.into_reader().take(max + 1);
    let mut file =
        fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let copied = std::io::copy(&mut reader, &mut file)?;
    file.flush()?;
    if copied > max {
        bail!("{url}: larger than {max} bytes; refusing");
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

/// Copy the archive's `attempt-hook` to `attempt-hook.new` beside `attempt`
/// (executable, quarantine flag cleared) without touching the installed one.
pub fn stage_hook_binary(dir: &Path, extracted: &Path) -> Result<PathBuf> {
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
    Ok(s.new)
}

/// Run a staged or installed `attempt-hook --version`: it must exit 0 and
/// name itself. A hook binary that cannot even do that fails every agent
/// call it is wired into.
pub fn check_hook_binary(path: &Path) -> Result<()> {
    let out = run_with_timeout(Command::new(path).arg("--version"), HEALTH_TIMEOUT)
        .with_context(|| format!("{} --version", path.display()))?;
    if !out.trim_start().starts_with("attempt-hook") {
        bail!(
            "{} --version printed {:?}, not an attempt-hook version",
            path.display(),
            clip(out.lines().next().unwrap_or(""))
        );
    }
    Ok(())
}

/// Move the staged `attempt-hook.new` into place, keeping the installed one
/// as `attempt-hook.prev`. Hooks referencing the path keep working through
/// the rename; a hook that starts mid-swap runs either the old or the new
/// binary, both of which speak the same spool format. Returns the installed
/// path and whether there was a previous copy to keep.
fn swap_staged_hook(dir: &Path) -> Result<(PathBuf, bool)> {
    let s = slots(&dir.join(hook_binary_name()));
    let had_previous = s.current.is_file();
    if had_previous {
        let _ = fs::remove_file(&s.prev);
        fs::rename(&s.current, &s.prev).with_context(|| format!("keeping {}", s.prev.display()))?;
    }
    if let Err(e) = fs::rename(&s.new, &s.current) {
        if had_previous {
            let _ = fs::rename(&s.prev, &s.current);
        }
        return Err(e).with_context(|| format!("installing {}", s.current.display()));
    }
    Ok((s.current, had_previous))
}

/// Put the archive's `attempt-hook` next to `attempt`: stage, then rename
/// over the old one (kept as `attempt-hook.prev`). Returns the installed path.
pub fn install_hook_binary(dir: &Path, extracted: &Path) -> Result<PathBuf> {
    stage_hook_binary(dir, extracted)?;
    Ok(swap_staged_hook(dir)?.0)
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
    if let Some((manager, how)) = managed_install(&binary) {
        report.outcome = Outcome::Refused {
            reason: format!("{} is managed by {manager}; {how}", binary.display()),
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
        Some(v) => (
            valid_release_version(v).ok_or_else(|| {
                anyhow!(
                    "{:?} is not a release version (expected like 1.2.3)",
                    clip(v)
                )
            })?,
            false,
        ),
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
    if opts.download_base.trim_end_matches('/') != DEFAULT_DOWNLOAD_BASE {
        report.notes.push(format!(
            "release files were fetched from {} (ATTEMPTDB_UPDATE_DOWNLOAD), not github.com",
            opts.download_base
        ));
    }

    let _ = fs::remove_dir_all(&s.staging);
    fs::create_dir_all(&s.staging)?;
    let hook_slots = slots(&dir.join(hook_binary_name()));
    let mut hook_note: Option<String> = None;
    let result = (|| -> Result<Outcome> {
        let stem = asset_stem(&resolved, TARGET);
        let asset = asset_name(&resolved, TARGET);
        let archive = s.staging.join(&asset);
        let sums = s.staging.join("SHA256SUMS");
        download(
            &agent,
            &release_asset_url(&opts.download_base, &resolved, &asset),
            &archive,
            MAX_ASSET_BYTES,
        )
        .with_context(|| format!("no release asset for {TARGET} in v{resolved}"))?;
        download(
            &agent,
            &release_asset_url(&opts.download_base, &resolved, "SHA256SUMS"),
            &sums,
            MAX_SUMS_BYTES,
        )
        .with_context(|| {
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
        // The pair stays in step: a release that ships `attempt-hook` puts
        // it next to `attempt`, whether or not one was there before. It is
        // staged and run BEFORE anything is swapped: a hook binary that does
        // not start would break every agent call, so it gets the same veto
        // over the update that `attempt` does.
        let hook_staged = match extracted_hook_binary(&s.staging, &stem) {
            Some(hook) => {
                let staged = stage_hook_binary(dir, &hook)?;
                if let Err(e) = check_hook_binary(&staged) {
                    let _ = fs::remove_file(&staged);
                    let _ = fs::remove_file(&s.new);
                    bail!(
                        "the downloaded attempt-hook failed its check; nothing was changed: {e:#}"
                    );
                }
                true
            }
            None => false,
        };
        let outcome = match swap_with_rollback(&s, check) {
            Ok(o) => o,
            Err(e) => {
                let _ = fs::remove_file(&hook_slots.new);
                return Err(e);
            }
        };
        if !matches!(outcome, Outcome::Updated { .. }) {
            let _ = fs::remove_file(&hook_slots.new);
            return Ok(outcome);
        }
        if !hook_staged {
            return Ok(outcome);
        }
        match swap_staged_hook(dir) {
            Ok((installed, had_previous)) => match check_hook_binary(&installed) {
                Ok(()) => {
                    hook_note = Some(format!("{} updated alongside", installed.display()));
                }
                Err(e) => {
                    // Both binaries go back: the pair is only good together.
                    let reason = format!("attempt-hook failed its check once installed: {e:#}");
                    if !had_previous {
                        let _ = fs::remove_file(&hook_slots.failed);
                        let _ = fs::rename(&hook_slots.current, &hook_slots.failed);
                    }
                    return match rollback(&binary) {
                        Ok(_) => Ok(Outcome::RolledBack { reason }),
                        Err(r) => Err(anyhow!(
                            "{reason}, and restoring the previous binaries failed ({r:#}); they are at {} and {}",
                            s.prev.display(),
                            hook_slots.prev.display()
                        )),
                    };
                }
            },
            Err(e) => {
                let _ = fs::remove_file(&hook_slots.new);
                hook_note = Some(format!("attempt-hook was NOT updated: {e:#}"));
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
        // Unparseable: never an update candidate, whichever side it is on.
        assert!(!is_newer("0.1.0", "nightly"));
        assert!(!is_newer("nightly", "0.1.0"));
        assert!(!is_newer("nightly", "nightly"));
        assert!(!is_newer("0.1.0", "9.9.9/../x"));
    }

    #[test]
    fn only_strict_semantic_versions_are_release_versions() {
        for ok in [
            "0.2.8",
            "v0.2.8",
            "1.0.0",
            "10.20.30",
            "1.2.3-rc.1",
            "1.2.3-alpha-2+build.5",
            "1.2.3+20260101",
            "1.2.3-0.3.7",
        ] {
            let bare = ok.strip_prefix('v').unwrap_or(ok);
            assert_eq!(valid_release_version(ok).as_deref(), Some(bare), "{ok}");
        }
        for bad in [
            "",
            "v",
            "latest",
            "nightly",
            "1",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "vv1.2.3",
            " 1.2.3",
            "1.2.3 ",
            "1.2.3\n",
            "1.2.3-",
            "1.2.3+",
            "1.2.3-01",
            "1.2.3-rc..1",
            "1.2.3-rc_1",
            "../1.2.3",
            "1.2.3/../x",
            "1.2.3?x=1",
            "1.2.3#frag",
            "1.2.3\\evil",
            "1.2.3%2f..",
            "1.2.3;rm -rf",
            "1.2.-3",
            "+1.2.3",
            "9999999999.0.0",
            "1.2.3-\u{e9}",
        ] {
            assert_eq!(valid_release_version(bad), None, "{bad:?}");
        }
        assert_eq!(
            valid_release_version(&format!("1.2.3-{}", "a".repeat(80))),
            None,
            "bounded"
        );
        assert_eq!(parse_release_tag(r#"{"tag_name":"nightly"}"#), None);
        assert_eq!(parse_release_tag(r#"{"tag_name":"v1.2.3/../x"}"#), None);
    }

    #[test]
    fn release_asset_urls_have_exactly_one_shape() {
        assert_eq!(
            release_asset_url(
                "https://github.com/",
                "0.3.0",
                "attempt-0.3.0-aarch64-apple-darwin.tar.gz"
            ),
            "https://github.com/nullarch/attemptdb/releases/download/v0.3.0/attempt-0.3.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            release_asset_url("http://127.0.0.1:9", "0.3.0", "SHA256SUMS"),
            "http://127.0.0.1:9/nullarch/attemptdb/releases/download/v0.3.0/SHA256SUMS"
        );
    }

    #[test]
    fn redirects_stay_on_the_same_origin_or_inside_githubs_own_hosts() {
        let asset = "https://github.com/nullarch/attemptdb/releases/download/v1.2.3/a.tar.gz";
        for ok in [
            "https://objects.githubusercontent.com/github-production-release-asset/1/2?x=y",
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/2",
            "https://github.com/nullarch/attemptdb/releases/download/v1.2.3/b",
            "https://GITHUB.com/x",
        ] {
            assert!(redirect_allowed(asset, ok), "{ok}");
        }
        for bad in [
            "http://objects.githubusercontent.com/x",
            "https://evil.example/nullarch/attemptdb/releases/download/v1.2.3/a.tar.gz",
            "https://github.com.evil.example/x",
            "https://evilgithub.com/x",
            "https://evilgithubusercontent.com/x",
            "https://githubusercontent.com.evil.example/x",
            "https://user@github.com/x",
            "https://objects.githubusercontent.com:8443/x",
            "ftp://github.com/x",
            "file:///etc/passwd",
            "//github.com/x",
            "",
        ] {
            assert!(!redirect_allowed(asset, bad), "{bad}");
        }
        // The CDN may only send you on within GitHub.
        let cdn = "https://objects.githubusercontent.com/x";
        assert!(redirect_allowed(cdn, "https://github.com/y"));
        assert!(!redirect_allowed(cdn, "https://evil.example/y"));
        // A mirror or test server redirects within itself, and only there.
        let local = "http://127.0.0.1:8080/a";
        assert!(redirect_allowed(local, "http://127.0.0.1:8080/b"));
        assert!(!redirect_allowed(local, "http://127.0.0.1:8081/b"));
        assert!(!redirect_allowed(local, "http://localhost:8080/b"));
        assert!(!redirect_allowed(local, "https://github.com/b"));
        // https never drops to http, even on the same host.
        assert!(!redirect_allowed(asset, "http://github.com/x"));
        assert!(redirect_allowed(
            "http://github.com/x",
            "https://github.com/x"
        ));
    }

    #[test]
    fn a_location_is_resolved_against_the_url_that_sent_it() {
        let from = "https://github.com/o/r/releases/latest/download/update.json?x=1";
        assert_eq!(
            resolve_location(from, "https://cdn.example/a").as_deref(),
            Some("https://cdn.example/a")
        );
        assert_eq!(
            resolve_location(from, "/o/r/releases/download/v1/update.json").as_deref(),
            Some("https://github.com/o/r/releases/download/v1/update.json")
        );
        assert_eq!(
            resolve_location(from, "//objects.githubusercontent.com/x").as_deref(),
            Some("https://objects.githubusercontent.com/x")
        );
        assert_eq!(
            resolve_location(from, "update2.json").as_deref(),
            Some("https://github.com/o/r/releases/latest/download/update2.json")
        );
        assert_eq!(resolve_location(from, "/x y"), None);
        assert_eq!(resolve_location(from, "/x\r\nSet-Cookie: a=b"), None);
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
        for brew in [
            "/opt/homebrew/bin/attempt",
            "/usr/local/Cellar/attempt/0.3.0/bin/attempt",
            "/usr/local/Homebrew/bin/attempt",
            "/home/linuxbrew/.linuxbrew/bin/attempt",
        ] {
            assert_eq!(
                managed_by(Path::new(brew)).map(|m| m.0),
                Some("Homebrew"),
                "{brew}"
            );
        }
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
