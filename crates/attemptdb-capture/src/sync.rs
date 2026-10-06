//! Client side of RFC 0006 §10: upload this database's events to one or
//! more sync servers ("peers") in batches, one batch in flight, in
//! `source_seq` order.
//!
//! The local database stays authoritative. The uploader reads it (read-only,
//! so it coexists with the daemon's writer), sends everything after the last
//! acknowledged `source_seq`, and advances the cursor only on an
//! acknowledgement. A failed batch leaves the cursor where it was; the next
//! run re-sends it, and the server's dedupe makes that a no-op.
//!
//! By default nothing content-bearing leaves the device: every event is
//! clamped to `metadata_only` before it is serialised, which removes
//! `content` and `raw`. `send_content` is the explicit opt-in.
//!
//! One device may upload to several peers. Each peer has its own
//! [`SyncProfile`] (what leaves: metadata only, plus inferences, plus
//! content), its own interval and repository policy, and its own cursor
//! under `<data_dir>/sync/`, so an unreachable peer never holds the others
//! back. The peer set lives in `<config_dir>/sync.json`; the daemon re-reads
//! it on every tick, so `attempt sync connect|add|remove` take effect without
//! a restart.

use crate::locator::Locator;
use anyhow::{Context, Result, anyhow, bail};
use attemptdb_core::event::repo_key;
use attemptdb_core::{
    CaptureMode, Event, EventId, EventKind, PortablePath, ProjectId, Timestamp, paths, secrets,
};
use attemptdb_storage::{Database, OpenOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const CONFIG_FILE: &str = "sync.json";
pub const DEFAULT_BATCH_EVENTS: usize = 1_000;
pub const DEFAULT_INTERVAL_SECS: u64 = 5;
/// Least time between two computations of the inference set.
pub const DEFAULT_INFERENCE_INTERVAL_SECS: u64 = 600;
/// How long a history found too large to project is left alone (six hours).
const SKIPPED_INFERENCE_RETRY_SECS: u64 = 6 * 3600;
/// Most events an inference set is projected from (see
/// [`PeerConfig::inference_max_events`]).
pub const DEFAULT_INFERENCE_MAX_EVENTS: usize = 250_000;
/// Largest body the server accepts by default (4 MiB); stay well under.
const MAX_BODY_BYTES: usize = 3 * 1024 * 1024;

/// The peer `attempt sync connect` writes, and the name a single-server
/// `sync.json` (top-level `url`) is read as.
pub const DEFAULT_PEER: &str = "default";
/// Longest peer name; the name is part of the cursor file name.
pub const MAX_PEER_NAME_LEN: usize = 32;
/// `attempt sync connect vibemon` resolves to this URL.
pub const VIBEMON_SYNC_URL: &str = "https://sync.vibemon.dev";
/// The word that stands for [`VIBEMON_SYNC_URL`] on the command line.
pub const VIBEMON_ALIAS: &str = "vibemon";
/// Environment variable that overrides [`VIBEMON_SYNC_URL`] (when non-empty).
pub const VIBEMON_SYNC_URL_ENV: &str = "VIBEMON_SYNC_URL";
/// How often the daemon looks for a `sync.json` while no peer is configured.
pub const CONFIG_POLL: Duration = Duration::from_secs(10);
/// A failing peer is retried after this long at first …
pub const BACKOFF_BASE: Duration = Duration::from_secs(5);
/// … doubling per consecutive failure, up to this.
pub const BACKOFF_MAX: Duration = Duration::from_secs(15 * 60);
/// Events one run may skip before it stops and says the server is refusing
/// this client rather than one event. Persisted across runs as a streak.
pub const MAX_QUARANTINE_STREAK: u32 = 25;
/// Quarantine records kept in the cursor file (newest last).
pub const MAX_QUARANTINE_RECORDS: usize = 100;

/// Wire schema of an inference upload (RFC 0006 §10.7, `spec/inference-v1.schema.json`).
pub const INFERENCE_SCHEMA: &str = "attemptdb.inference/v1";
/// Inference kinds that leave the device. Sessions, turns, and tool calls are
/// one-to-one with facts and derivable server-side; causal edges are the
/// largest table and equally derivable. These four are what a reader asks
/// about and what may differ when the device saw content the server did not.
pub const INFERENCE_KINDS: &[&str] = &["attempt", "handoff", "work_unit", "decision"];
/// Most items of one kind per upload; the newest are kept and the count of
/// dropped items is reported, never hidden.
pub const MAX_INFERENCE_ITEMS: usize = 20_000;
/// The server refuses a request body over 4 MiB (413) and replaces a kind's
/// document wholesale on every upload, so one kind cannot be split across
/// requests. Items past this budget are dropped oldest first instead.
pub const MAX_INFERENCE_BODY_BYTES: usize = 3 * 1024 * 1024;
/// A failed inference upload is retried no sooner than this (one minute),
/// not on every five-second tick.
const INFERENCE_RETRY_BACKOFF_MICROS: i64 = 60_000_000;

fn default_batch() -> usize {
    DEFAULT_BATCH_EVENTS
}
fn default_interval() -> u64 {
    DEFAULT_INTERVAL_SECS
}
fn default_inference_interval() -> u64 {
    DEFAULT_INFERENCE_INTERVAL_SECS
}
fn default_inference_max_events() -> usize {
    DEFAULT_INFERENCE_MAX_EVENTS
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

/// What leaves the device for one peer. A profile is a name for a
/// combination of the two stored flags (`send_content`, `send_inferences`);
/// the flags stay the stored truth so older `sync.json` files keep working.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncProfile {
    /// Metadata only: no content, no inferences. The default.
    MetadataOnly,
    /// Metadata plus this device's inferences, each with evidence ids,
    /// confidence, and algorithm version. Content still stays local, so the
    /// inferences' `objective`/`rationale` are removed before upload.
    Semantic,
    /// `semantic` plus the conversation's natural language: the user's
    /// prompts and the agent's messages (secret-redacted on the device).
    /// Commands, tool input and tool output stay local. The VibeMon
    /// installer's default.
    Messages,
    /// Metadata, inferences, and content (secret-redacted on the device;
    /// the server's capture-mode ceiling still applies).
    Full,
}

impl SyncProfile {
    pub const ALL: [SyncProfile; 4] = [
        SyncProfile::MetadataOnly,
        SyncProfile::Semantic,
        SyncProfile::Messages,
        SyncProfile::Full,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SyncProfile::MetadataOnly => "metadata_only",
            SyncProfile::Semantic => "semantic",
            SyncProfile::Messages => "messages",
            SyncProfile::Full => "full",
        }
    }

    /// `(send_content, send_inferences, send_messages)`.
    pub fn flags(self) -> (bool, bool, bool) {
        match self {
            SyncProfile::MetadataOnly => (false, false, false),
            SyncProfile::Semantic => (false, true, false),
            SyncProfile::Messages => (false, true, true),
            SyncProfile::Full => (true, true, true),
        }
    }

    /// The profile that names a flag combination. `send_content` covers
    /// everything and reports `full` whatever the other flags say — a reader
    /// must never see `metadata_only`, `semantic` or `messages` on a peer
    /// that receives commands and tool output. `send_messages` without
    /// inferences has no name of its own and reports `messages`: the
    /// conversation is the stronger signal.
    pub fn from_flags(send_content: bool, send_inferences: bool, send_messages: bool) -> Self {
        match (send_content, send_inferences, send_messages) {
            (true, _, _) => SyncProfile::Full,
            (false, _, true) => SyncProfile::Messages,
            (false, true, false) => SyncProfile::Semantic,
            (false, false, false) => SyncProfile::MetadataOnly,
        }
    }

    /// Flags for a command line: the profile (`semantic` when none is
    /// given) with the explicit `--send-content` / `--send-inferences` /
    /// `--send-messages` switches on top. The switches only ever add.
    pub fn resolve(
        profile: Option<SyncProfile>,
        send_content: bool,
        send_inferences: bool,
        send_messages: bool,
    ) -> (bool, bool, bool) {
        let (c, i, m) = profile.unwrap_or(SyncProfile::Semantic).flags();
        (c || send_content, i || send_inferences, m || send_messages)
    }

    /// One phrase for humans.
    pub fn summary(self) -> &'static str {
        match self {
            SyncProfile::MetadataOnly => "metadata only; content and inferences stay local",
            SyncProfile::Semantic => {
                "metadata and inferences (with evidence ids and confidence); content stays local"
            }
            SyncProfile::Messages => {
                "metadata, inferences, and the conversation (your prompts and the agent's messages, secrets redacted); commands and tool output stay local"
            }
            SyncProfile::Full => {
                "metadata, inferences, and content (secrets redacted on this device)"
            }
        }
    }
}

impl fmt::Display for SyncProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl FromStr for SyncProfile {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        SyncProfile::ALL
            .into_iter()
            .find(|p| p.as_str().eq_ignore_ascii_case(s) || p.as_str().replace('_', "-") == s)
            .ok_or_else(|| {
                anyhow!(
                    "unknown profile `{s}`: expected metadata_only, semantic, messages, or full"
                )
            })
    }
}

// ---------------------------------------------------------------------------
// Peers
// ---------------------------------------------------------------------------

/// Where and how to upload to one server.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerConfig {
    /// Base URL of the server, e.g. `https://sync.vibemon.dev`.
    pub url: String,
    /// Bearer key issued for this device.
    pub key: String,
    /// Upload `content`/`raw` too. Off by default: metadata only.
    #[serde(default)]
    pub send_content: bool,
    /// Also upload this device's Tier-1 inferences (attempts, handoffs, work
    /// units, decisions), each with its evidence ids, confidence, and
    /// algorithm version. Off by default; inferences never leave without
    /// provenance, and under `send_content == false` their content-bearing
    /// fields (`objective`, `rationale`) are removed first.
    #[serde(default)]
    pub send_inferences: bool,
    /// Upload the conversation's natural language — `content.prompt` of a
    /// submitted prompt and `content.message` of an agent message or turn
    /// stop, plus the same fields of OTel `user_prompt` /
    /// `assistant_response` records — and nothing else content-bearing.
    /// Commands, tool input and tool output never leave under this flag.
    #[serde(default)]
    pub send_messages: bool,
    #[serde(default = "default_batch")]
    pub batch_events: usize,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    /// Repository policy (RFC 0006 §10.5), evaluated on the device. Entries
    /// are normalised remotes (`github.com/owner/repo`) or project ids
    /// (`prj_…`). When `include` is non-empty only those projects upload;
    /// `exclude` always wins. Excluded projects never leave the device —
    /// not even their metadata.
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Plain `http://` to a host that is not this machine: keys and prompts
    /// would cross the network in the clear. Refused unless this was chosen
    /// explicitly (`attempt sync connect --allow-insecure-http`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_insecure_http: bool,
    /// Least seconds between two computations of the inference set (when
    /// `send_inferences` is on). The set is a projection of the whole
    /// policy-allowed history, minutes of work on a long one, so it is
    /// recomputed when new events have arrived and this long has passed since
    /// the last time — not on every tick that uploads an event. The first
    /// computation after connecting, `attempt sync now --inferences`, and a
    /// retry after a failed upload do not wait. `0` recomputes on every
    /// upload that carries new events.
    #[serde(default = "default_inference_interval")]
    pub inference_interval_secs: u64,
    /// Most policy-allowed, non-telemetry events the inference set is
    /// projected from. The projection keeps a few hundred bytes of every
    /// event it is fed (about 3.5 KiB measured once the projection is built:
    /// ~900 MB for 250,000 events), and a set built from only the newest
    /// events would carry ids that differ from the server's own projection
    /// of the same history. So a history larger than this is not projected
    /// here at all: the run says so (`attempt sync status`) and the server
    /// derives its own sets from the events it holds. `0` removes the limit.
    #[serde(default = "default_inference_max_events")]
    pub inference_max_events: usize,
    /// What the person agreed to when they connected (or last widened what
    /// leaves). Absent in a `sync.json` written before consent was recorded:
    /// such a peer uploads everything after its cursor, as it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent: Option<Consent>,
}

/// The consent marker `attempt sync connect` records: what was agreed, when,
/// and how far back the first upload may reach. The same facts go into the
/// log as a `config_changed` event (counts only; never the repository names).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Consent {
    /// When this profile and policy were agreed.
    pub at: Timestamp,
    pub profile: SyncProfile,
    /// The repository policy as agreed.
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Events the database already held when this was set are never
    /// uploaded: history that predates the connection was not agreed to.
    /// `None` when the person passed `--include-history` (or ran
    /// `attempt sync history include`). The time is for display and for
    /// imports (see [`Consent::withholds`]); [`Consent::history_before_seq`]
    /// is what decides for everything captured live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_before: Option<Timestamp>,
    /// The database's newest `source_seq` at the moment the watermark was set.
    /// Local sequence numbers are assigned by the single writer in order and
    /// never depend on a clock, so an event with `source_seq` at or below
    /// this was in the database before the person agreed, and one above it
    /// was captured afterwards, whatever the wall clock said (a machine whose
    /// clock was set back, a WSL2 guest that drifted). Absent in a
    /// `sync.json` written before this field existed: the time alone decides
    /// then, as it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_before_seq: Option<u64>,
}

impl Consent {
    /// Whether a watermark is in force: some history is kept local.
    pub fn has_watermark(&self) -> bool {
        self.history_before.is_some() || self.history_before_seq.is_some()
    }

    /// Whether `e` predates the connection and so stays on the device:
    ///
    /// - with a sequence watermark, every event the database already held
    ///   (`source_seq` at or below it); and, of the events that arrived after
    ///   it, only *imports* observed before the connection (a transcript or an
    ///   export read in after connecting is history all the same, and its
    ///   `observed_at` is the provider's, not the importer's);
    /// - without one (an older `sync.json`), every event observed before the
    ///   time.
    ///
    /// A live event captured after connecting is never withheld, whatever its
    /// timestamp says.
    pub fn withholds(&self, e: &Event) -> bool {
        match self.history_before_seq {
            Some(seq) => {
                e.source_seq <= seq
                    || (is_imported_history(e)
                        && self.history_before.is_some_and(|w| e.observed_at < w))
            }
            None => self.history_before.is_some_and(|w| e.observed_at < w),
        }
    }

    /// Everything the database holds now stays local from here on: the
    /// watermark moves to `at` and to `seq` (the database's newest
    /// `source_seq`). It never moves backwards.
    pub fn advance_watermark(&mut self, at: Timestamp, seq: u64) {
        self.history_before = Some(self.history_before.map_or(at, |w| w.max(at)));
        self.history_before_seq = Some(self.history_before_seq.map_or(seq, |w| w.max(seq)));
    }

    /// The explicit opt-in: history before the connection is included.
    pub fn clear_watermark(&mut self) {
        self.history_before = None;
        self.history_before_seq = None;
    }
}

/// An event that was read in from somewhere else rather than captured as it
/// happened: reconstructed from a provider transcript (`attrs.reconstructed`),
/// or backfilled from a VibeMon export (`attrs.x_vibemon_import`).
pub fn is_imported_history(e: &Event) -> bool {
    e.attrs.get("reconstructed").and_then(Value::as_bool) == Some(true)
        || e.attrs.contains_key("x_vibemon_import")
}

/// What a repository-policy entry names, in one canonical spelling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyKey {
    /// `prj_<uuid>` (or the bare uuid).
    Project(ProjectId),
    /// `host/owner/repo`, lower-case, without scheme, credentials or `.git`.
    Remote(String),
}

impl PolicyKey {
    /// `prj_<uuid>` or `host/owner/repo`: how an entry is stored.
    pub fn canonical(&self) -> String {
        match self {
            PolicyKey::Project(id) => format!("prj_{id}"),
            PolicyKey::Remote(r) => r.clone(),
        }
    }
}

/// The one function that reads a policy entry, used both when an entry is
/// stored and when it is matched against an event, so the two cannot drift.
/// `https://GitHub.com/Acme/Private.git`, `git@github.com:acme/private`,
/// `ssh://git@github.com/acme/private/` and `github.com/acme/private` are one
/// entry; `prj_<uuid>` and the bare uuid are one entry. `None` when the text
/// is neither a project id nor a remote with at least `host/owner/repo`.
pub fn parse_policy_entry(entry: &str) -> Option<PolicyKey> {
    let e = entry.trim();
    if e.is_empty() {
        return None;
    }
    if let Ok(id) = e.parse::<ProjectId>() {
        return Some(PolicyKey::Project(id));
    }
    canonical_remote(e).map(PolicyKey::Remote)
}

/// `host/owner/repo`, in one spelling, for an entry or for the remote an
/// event carries — the same function on both sides of every comparison:
/// [`attemptdb_core::event::repo_key`], which drops schemes, credentials,
/// ports and `.git`, maps ssh-only hosts (`ssh.github.com`) to their web host
/// and cuts browser-URL tails (`/tree/main`, `/issues/3`, `?tab=…`, `#readme`).
fn canonical_remote(s: &str) -> Option<String> {
    repo_key(s)
}

/// A repository policy, parsed once. Built by [`PeerConfig::policy`].
#[derive(Clone, Debug, Default)]
pub struct Policy {
    include: Vec<PolicyKey>,
    exclude: Vec<PolicyKey>,
}

impl Policy {
    /// True when any `include` or `exclude` entry exists. A telemetry event
    /// that cannot be tied to a project never uploads while this is true.
    pub fn is_configured(&self) -> bool {
        !self.include.is_empty() || !self.exclude.is_empty()
    }

    fn names(keys: &[PolicyKey], ev: &Event) -> bool {
        let remote = ev.project.repo_remote.as_deref().and_then(canonical_remote);
        keys.iter().any(|k| match k {
            PolicyKey::Project(id) => ev.project.project_id == *id,
            PolicyKey::Remote(r) => remote.as_deref() == Some(r.as_str()),
        })
    }

    /// Whether the project of `ev` may upload. `exclude` always wins, and
    /// `include` (when present) must name the project.
    pub fn allows(&self, ev: &Event) -> bool {
        // An OTel record the daemon could not tie to a hook session carries
        // the placeholder project `otel/unattributed`, so no entry can name
        // it — and a prompt or reply of an excluded repository can arrive in
        // exactly that state (the hook is not trusted yet, or the record came
        // first). Under any policy it stays on the device.
        if self.is_configured() && is_unattributed_telemetry(ev) {
            return false;
        }
        if Self::names(&self.exclude, ev) {
            return false;
        }
        self.include.is_empty() || Self::names(&self.include, ev)
    }
}

/// An OTel observation whose project is not known to be the project of the
/// session it belongs to: `x_otel_project_attributed` is not `true` (the
/// receiver writes `false`; an old row has no such key).
pub fn is_unattributed_telemetry(ev: &Event) -> bool {
    ev.attrs.get("source").and_then(Value::as_str) == Some("otel")
        && ev
            .attrs
            .get("x_otel_project_attributed")
            .and_then(Value::as_bool)
            != Some(true)
}

fn is_discarded_telemetry(ev: &Event) -> bool {
    ev.attrs.get("source").and_then(Value::as_str) == Some("otel")
        && attemptdb_adapters::otel::is_discarded(&ev.provider_event_name)
}

/// `std::fs::read_to_string`, except that on Windows a file that another
/// process is replacing through [`write_atomic`] at this very moment can
/// refuse to open for an instant (it is delete-pending, and the error is
/// "access denied"): try again for a short while before giving up.
fn read_to_string_settled(path: &Path) -> std::io::Result<String> {
    #[cfg(windows)]
    {
        let mut attempt = 0;
        loop {
            match std::fs::read_to_string(path) {
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && attempt < 40 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                other => return other,
            }
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::read_to_string(path)
    }
}

/// Write `bytes` to `path` so that a crash or a second process never leaves
/// a torn file: a temp file whose name is unique to this process and call,
/// flushed to disk, then renamed over `path`. `private` makes it mode 0600.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = private;
        let mut f = options
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
        drop(f);
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

impl PeerConfig {
    /// A peer with the defaults for everything but the address and key.
    pub fn new(url: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            key: key.into(),
            send_content: false,
            send_inferences: false,
            send_messages: false,
            batch_events: DEFAULT_BATCH_EVENTS,
            interval_secs: DEFAULT_INTERVAL_SECS,
            inference_interval_secs: DEFAULT_INFERENCE_INTERVAL_SECS,
            inference_max_events: DEFAULT_INFERENCE_MAX_EVENTS,
            include: vec![],
            exclude: vec![],
            allow_insecure_http: false,
            consent: None,
        }
    }

    /// The name of this peer's flag combination (see [`SyncProfile::from_flags`]).
    pub fn profile(&self) -> SyncProfile {
        SyncProfile::from_flags(self.send_content, self.send_inferences, self.send_messages)
    }

    /// Set the flags from a profile.
    pub fn set_profile(&mut self, profile: SyncProfile) {
        let (c, i, m) = profile.flags();
        self.send_content = c;
        self.send_inferences = i;
        self.send_messages = m;
    }

    /// Whether any content-bearing field may leave under this peer.
    pub fn sends_any_content(&self) -> bool {
        self.send_content || self.send_messages
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs.max(5))
    }

    fn endpoint_inferences(&self) -> String {
        format!("{}/v1/sync/inferences", self.url.trim_end_matches('/'))
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/sync", self.url.trim_end_matches('/'))
    }

    /// The repository policy, parsed. An entry that is neither a project id
    /// nor a git remote is an error, not a silently inert rule: `exclude`
    /// that matches nothing would promise what it does not do.
    pub fn policy(&self) -> Result<Policy> {
        let parse = |what: &str, entries: &[String]| -> Result<Vec<PolicyKey>> {
            entries
                .iter()
                .map(|e| {
                    parse_policy_entry(e).ok_or_else(|| {
                        anyhow!(
                            "the {what} entry `{e}` is neither a project id (prj_…) nor a git \
                             remote (host/owner/repo, https://host/owner/repo.git, \
                             git@host:owner/repo); nothing is uploaded until it is fixed with \
                             `attempt sync policy remove`"
                        )
                    })
                })
                .collect()
        };
        Ok(Policy {
            include: parse("include", &self.include)?,
            exclude: parse("exclude", &self.exclude)?,
        })
    }

    /// Whether an event may be uploaded under this policy: its project must be
    /// allowed, and it must not be telemetry the intake discards. An entry
    /// that cannot be read allows nothing (see [`PeerConfig::policy`]).
    pub fn allows(&self, ev: &Event) -> bool {
        // Discarded telemetry never leaves the device, including rows stored
        // before the intake filter existed. Excluded events still advance the
        // cursor, so they are not re-examined.
        if is_discarded_telemetry(ev) {
            return false;
        }
        self.policy().is_ok_and(|p| p.allows(ev))
    }

    /// Refuse to talk to a host in the clear unless that was chosen.
    pub fn check_transport(&self) -> Result<()> {
        validate_url_opts(&self.url, self.allow_insecure_http).map(|_| ())
    }

    /// The key, masked for display.
    pub fn masked_key(&self) -> String {
        let k = &self.key;
        if k.len() <= 8 {
            "••••".to_string()
        } else {
            format!("{}…{}", &k[..4], &k[k.len() - 4..])
        }
    }
}

/// A peer name: `[A-Za-z0-9._-]{1,32}`. It is part of the cursor file name.
pub fn validate_peer_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() {
        bail!("the peer name is empty");
    }
    if n.len() > MAX_PEER_NAME_LEN {
        bail!("the peer name `{n}` is longer than {MAX_PEER_NAME_LEN} characters");
    }
    if !n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("the peer name `{n}` may only contain letters, digits, `.`, `_`, and `-`");
    }
    Ok(n.to_string())
}

/// Every peer this device uploads to, keyed by name. Stored at
/// `<config_dir>/sync.json` (mode 0600) as `{ "peers": { "<name>": … } }`.
/// A file in the older single-server shape (top-level `url`) is read as peer
/// [`DEFAULT_PEER`] and rewritten in the new shape on the next save.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncConfig {
    pub peers: BTreeMap<String, PeerConfig>,
}

#[derive(Serialize, Deserialize)]
struct PeersFile {
    peers: BTreeMap<String, PeerConfig>,
}

impl SyncConfig {
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(CONFIG_FILE)
    }

    /// Exactly one peer, named [`DEFAULT_PEER`].
    pub fn single(peer: PeerConfig) -> Self {
        Self {
            peers: BTreeMap::from([(DEFAULT_PEER.to_string(), peer)]),
        }
    }

    /// `None` when no sync has been configured (no file). A file with no
    /// peers loads as an empty configuration.
    pub fn load(config_dir: &Path) -> Result<Option<Self>> {
        let path = Self::path(config_dir);
        match read_to_string_settled(&path) {
            Ok(text) => Ok(Some(
                Self::parse(&text).with_context(|| format!("parsing {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Both file shapes: `{ "peers": {…} }`, or the single-server layout
    /// with a top-level `url`, which becomes peer `default`.
    pub fn parse(text: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(text)?;
        let peers = if value.get("peers").is_some() {
            serde_json::from_value::<PeersFile>(value)?.peers
        } else if value.get("url").is_some() {
            let peer: PeerConfig = serde_json::from_value(value)?;
            BTreeMap::from([(DEFAULT_PEER.to_string(), peer)])
        } else {
            bail!("expected a `peers` object or a single-server `url`");
        };
        for name in peers.keys() {
            validate_peer_name(name)?;
        }
        Ok(Self { peers })
    }

    /// The file's JSON, always in the `peers` shape.
    pub fn to_json(&self) -> Value {
        json!({ "peers": self.peers })
    }

    /// Write the file (mode 0600, atomic replace). An empty configuration
    /// removes the file instead: "not connected" has one representation.
    pub fn save(&self, config_dir: &Path) -> Result<()> {
        if self.peers.is_empty() {
            Self::remove(config_dir)?;
            return Ok(());
        }
        std::fs::create_dir_all(config_dir)
            .with_context(|| format!("creating {}", config_dir.display()))?;
        write_atomic(
            &Self::path(config_dir),
            &serde_json::to_vec_pretty(&self.to_json())?,
            true,
        )
    }

    /// Returns whether a configuration existed.
    pub fn remove(config_dir: &Path) -> Result<bool> {
        match std::fs::remove_file(Self::path(config_dir)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&PeerConfig> {
        self.peers.get(name)
    }

    pub fn names(&self) -> BTreeSet<String> {
        self.peers.keys().cloned().collect()
    }

    /// The names, comma-separated, for messages.
    pub fn names_list(&self) -> String {
        self.peers.keys().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// What changed between two readings of `sync.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerSetChange {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Present in both with a different configuration (URL, key, profile,
    /// interval, or policy).
    pub changed: Vec<String>,
}

impl PeerSetChange {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// Names added, removed, and changed from `before` to `after`.
pub fn peer_set_diff(before: &SyncConfig, after: &SyncConfig) -> PeerSetChange {
    let mut change = PeerSetChange::default();
    for (name, peer) in &after.peers {
        match before.peers.get(name) {
            None => change.added.push(name.clone()),
            Some(old) if old != peer => change.changed.push(name.clone()),
            Some(_) => {}
        }
    }
    for name in before.peers.keys() {
        if !after.peers.contains_key(name) {
            change.removed.push(name.clone());
        }
    }
    change
}

/// How long to hold a peer back after `failures` consecutive failed runs:
/// [`BACKOFF_BASE`] doubling each time up to [`BACKOFF_MAX`], then spread so
/// many devices that failed together do not retry together. `jitter` is in
/// `[0, 1)`: the delay lands between half the base and the whole of it (equal
/// jitter), never above [`BACKOFF_MAX`]. Zero failures hold nothing.
pub fn backoff_delay(failures: u32, jitter: f64) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let exp = failures.saturating_sub(1).min(20);
    let base = BACKOFF_BASE
        .saturating_mul(1u32 << exp)
        .min(BACKOFF_MAX)
        .as_secs_f64();
    let j = jitter.clamp(0.0, 1.0);
    Duration::from_secs_f64(base * (0.5 + 0.5 * j))
}

/// A random number in `[0, 1)` for [`backoff_delay`].
pub fn jitter_unit() -> f64 {
    let b = *EventId::new().as_bytes();
    f64::from(u32::from_le_bytes([b[12], b[13], b[14], b[15]])) / (f64::from(u32::MAX) + 1.0)
}

/// The daemon's per-peer timer: which peers are due, and how long to sleep
/// until the next one is. Pure bookkeeping over `Instant`s so it can be
/// tested without a clock. A peer whose last run failed is held back by
/// [`backoff_delay`] (on top of its interval) until a run succeeds, so an
/// unreachable or refusing server is asked every few minutes, not every five
/// seconds, and its backlog is not decoded on every tick.
#[derive(Debug, Default)]
pub struct PeerSchedule {
    last_attempt: BTreeMap<String, Instant>,
    failures: BTreeMap<String, u32>,
    hold_until: BTreeMap<String, Instant>,
}

impl PeerSchedule {
    /// Peers whose own interval has elapsed since their last attempt, and
    /// that are not being held back after a failure. A peer seen for the
    /// first time is scheduled from `now`, so its first upload happens one
    /// interval after it appeared — the same as the single-server daemon did.
    /// Peers no longer configured are forgotten.
    pub fn due(&mut self, cfg: &SyncConfig, now: Instant) -> Vec<String> {
        self.last_attempt.retain(|n, _| cfg.peers.contains_key(n));
        self.failures.retain(|n, _| cfg.peers.contains_key(n));
        self.hold_until.retain(|n, _| cfg.peers.contains_key(n));
        let mut due = Vec::new();
        for (name, peer) in &cfg.peers {
            match self.last_attempt.get(name) {
                None => {
                    self.last_attempt.insert(name.clone(), now);
                }
                Some(last)
                    if now.duration_since(*last) >= peer.interval()
                        && self.hold_until.get(name).is_none_or(|h| now >= *h) =>
                {
                    due.push(name.clone());
                }
                Some(_) => {}
            }
        }
        due
    }

    /// Record an attempt (successful or not) at `now`.
    pub fn mark(&mut self, name: &str, now: Instant) {
        self.last_attempt.insert(name.to_string(), now);
    }

    /// The attempt at `now` failed: hold the peer back for the next backoff
    /// step. Returns how long.
    pub fn failed(&mut self, name: &str, now: Instant, jitter: f64) -> Duration {
        let n = self.failures.entry(name.to_string()).or_default();
        *n = n.saturating_add(1);
        let delay = backoff_delay(*n, jitter);
        self.hold_until.insert(name.to_string(), now + delay);
        delay
    }

    /// The attempt succeeded: no more holding back.
    pub fn succeeded(&mut self, name: &str) {
        self.failures.remove(name);
        self.hold_until.remove(name);
    }

    /// Consecutive failed attempts of `name`.
    pub fn failures(&self, name: &str) -> u32 {
        self.failures.get(name).copied().unwrap_or(0)
    }

    /// Time until the earliest peer is due, at least one second, and never
    /// longer than the smallest configured interval — the tick at which
    /// `sync.json` is re-read. [`CONFIG_POLL`] when no peer is configured.
    /// A held-back peer is due when its hold ends, but the sleep still ends at
    /// the smallest interval so the file is re-read: a changed key or profile
    /// is noticed (and resets the hold) without waiting out a long backoff.
    pub fn next_sleep(&self, cfg: &SyncConfig, now: Instant) -> Duration {
        let mut sleep: Option<Duration> = None;
        let mut tick: Option<Duration> = None;
        for (name, peer) in &cfg.peers {
            let mut due_at = match self.last_attempt.get(name) {
                Some(last) => *last + peer.interval(),
                None => now + peer.interval(),
            };
            if let Some(hold) = self.hold_until.get(name) {
                due_at = due_at.max(*hold);
            }
            let remaining = due_at.saturating_duration_since(now);
            sleep = Some(sleep.map_or(remaining, |s| s.min(remaining)));
            tick = Some(tick.map_or(peer.interval(), |t| t.min(peer.interval())));
        }
        match (sleep, tick) {
            (Some(s), Some(t)) => s.min(t).max(Duration::from_secs(1)),
            _ => CONFIG_POLL,
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------

/// Per-database, per-peer upload cursor (RFC 0006 §10.1 `sync_state`). Lives
/// under `<data_dir>/sync/<hash of db dir>.<peer>.json` so several databases
/// on one machine, and several peers of one database, keep separate cursors.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncState {
    pub last_acked_source_seq: u64,
    pub last_acked_hlc: u64,
    pub batches: u64,
    pub events: u64,
    pub duplicates: u64,
    pub rejected: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<Timestamp>,
    /// Inference uploads that reached the server.
    #[serde(default)]
    pub inference_uploads: u64,
    /// Items stored by the last inference upload.
    #[serde(default)]
    pub inference_items: u64,
    /// Digest of the last uploaded inference set; an identical set is not
    /// re-sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_inference_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_inference_at: Option<Timestamp>,
    /// The server this cursor was advanced against. A peer re-added under
    /// the same name but pointing at a different server starts from zero
    /// instead of silently skipping everything the old server had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Events the server refused for a reason that is the event's own (too
    /// large, malformed for this server) and that the uploader therefore
    /// skipped so the cursor could move on. Counted, never silent: see
    /// [`SyncState::quarantine`] and `attempt sync status`.
    #[serde(default)]
    pub quarantined: u64,
    /// Of those, events whose text was withheld but whose metadata arrived.
    #[serde(default)]
    pub content_withheld: u64,
    /// Events skipped in a row without the server accepting one in between;
    /// at [`MAX_QUARANTINE_STREAK`] the uploader stops skipping and reports
    /// that the server is refusing this client, not one event.
    #[serde(default)]
    pub quarantine_streak: u32,
    /// The newest skipped events (at most [`MAX_QUARANTINE_RECORDS`]):
    /// ids, sequence numbers and the server's reason, never content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quarantine: Vec<QuarantineRecord>,
    /// Consecutive failed runs; reset by a success.
    #[serde(default)]
    pub failures: u32,
    /// Events withheld because they were observed before the consent marker.
    #[serde(default)]
    pub before_consent: u64,
    /// When this device last asked the server to forget its events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_forget_at: Option<Timestamp>,
    /// Events went to the server since the inference set was last computed,
    /// so the set is out of date. The recompute reads and projects the whole
    /// policy-allowed history, so it waits for
    /// [`PeerConfig::inference_interval_secs`] after the last one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inference_dirty: bool,
    /// When the inference set was last computed (uploaded or found
    /// unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_computed_at: Option<Timestamp>,
    /// Set-aside events a later `attempt sync retry-set-aside` delivered.
    #[serde(default)]
    pub set_aside_retried: u64,
    /// The last inference computation found more than this many events to
    /// project ([`PeerConfig::inference_max_events`]) and stopped: nothing was
    /// computed or uploaded, and the server derives its own sets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_skipped_over: Option<u64>,
}

/// One event the uploader did not send, and why.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuarantineRecord {
    pub event_id: EventId,
    pub source_seq: u64,
    /// `skipped` (nothing of it went) or `content_withheld` (metadata went).
    pub action: String,
    /// HTTP status of the refusal.
    pub status: u16,
    /// The server's reason, first 200 characters.
    pub reason: String,
    pub at: Timestamp,
    /// The server's version when it refused (`/v1/health`), when it said:
    /// what to compare against after an upgrade, before `attempt sync
    /// retry-set-aside`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
}

impl SyncState {
    /// The cursor to use against `url`: this one when it was advanced
    /// against the same server (or predates URL tracking), a fresh one
    /// otherwise. Either way the returned state is bound to `url`.
    pub fn bound_to(self, url: &str) -> Self {
        let same = self.url.as_deref().is_none_or(|u| u == url);
        let mut state = if same { self } else { Self::default() };
        state.url = Some(url.to_string());
        state
    }

    fn stem(db_dir: &Path) -> String {
        let digest = Sha256::digest(db_dir.to_string_lossy().as_bytes());
        hex::encode(&digest[..8])
    }

    /// `<data_dir>/sync/<hash>.<peer>.json`.
    pub fn path(data_dir: &Path, db_dir: &Path, peer: &str) -> PathBuf {
        data_dir
            .join("sync")
            .join(format!("{}.{peer}.json", Self::stem(db_dir)))
    }

    /// `<data_dir>/sync/<hash>.json`: the single-server layout, which is
    /// peer `default`'s cursor.
    pub fn legacy_path(data_dir: &Path, db_dir: &Path) -> PathBuf {
        data_dir
            .join("sync")
            .join(format!("{}.json", Self::stem(db_dir)))
    }

    /// A peer's cursor and the path it is saved to. For peer `default` the
    /// single-server file is read when the per-peer file is absent, so an
    /// upgraded install continues where it was; writes go to the new name.
    pub fn load_for(data_dir: &Path, db_dir: &Path, peer: &str) -> Result<(Self, PathBuf)> {
        let path = Self::path(data_dir, db_dir, peer);
        if peer == DEFAULT_PEER && !path.exists() {
            let legacy = Self::legacy_path(data_dir, db_dir);
            if legacy.exists() {
                return Ok((Self::load(&legacy)?, path));
            }
        }
        Ok((Self::load(&path)?, path))
    }

    pub fn load(path: &Path) -> Result<Self> {
        match read_to_string_settled(path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        write_atomic(path, &serde_json::to_vec_pretty(self)?, false)
    }
}

// ---------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------

/// What one run did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct UploadReport {
    /// Events after the cursor when the run started.
    pub pending_before: usize,
    pub batches: usize,
    pub accepted: usize,
    pub duplicates: usize,
    pub rejected: usize,
    pub redactions: usize,
    pub stripped_content: usize,
    /// Secret spans redacted from content before upload.
    pub secrets_redacted: usize,
    /// Events the server refused for their own sake and the run skipped
    /// (see [`SyncState::quarantine`]); of them `content_withheld` still
    /// delivered their metadata.
    pub quarantined: usize,
    pub content_withheld: usize,
    /// Events not uploaded because they were observed before the consent
    /// marker (see [`Consent::history_before`]).
    pub before_consent: usize,
    /// Cursor after the run.
    pub cursor: u64,
    /// Present when `send_inferences` is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inferences: Option<InferenceReport>,
}

/// One device-computed inference on the wire: what it is, what it was
/// derived from, how sure the algorithm was, and which algorithm.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InferenceItem {
    /// One of [`INFERENCE_KINDS`].
    pub kind: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Never empty: an inference without evidence is not uploaded.
    pub evidence: Vec<EventId>,
    /// Serialised rounded to four decimals so an `f32` such as `0.9` is
    /// `0.9` on the wire, not `0.8999999761581421`.
    #[serde(serialize_with = "round_confidence")]
    pub confidence: f32,
    pub algorithm_version: String,
    /// The projection row minus the provenance fields above.
    pub fields: Value,
}

fn round_confidence<S: serde::Serializer>(c: &f32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64((f64::from(*c) * 10_000.0).round() / 10_000.0)
}

/// Everything a device computed at one point in time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceSet {
    pub algorithm_version: String,
    pub computed_at: Timestamp,
    pub items: Vec<InferenceItem>,
}

/// Hands the events an inference set is computed from to a visitor, one at
/// a time, and returns when the last one has been visited. The uploader
/// supplies it; the events are decoded as they are visited and not kept, so
/// a source that keeps only what its projection needs (the projector retains
/// a few fields per event) never holds the history in memory.
pub type EventFeed<'a> = &'a mut dyn FnMut(&mut dyn FnMut(&Event)) -> Result<()>;

/// Computes the inference set from the policy-allowed events (metadata only,
/// no telemetry, paths scrubbed when the profile says so). Supplied by the
/// binary (the projector lives above this crate), so the uploader stays free
/// of inference code.
pub type InferenceFn = dyn Fn(EventFeed<'_>) -> Result<InferenceSet> + Send + Sync;

#[derive(Clone)]
pub struct InferenceSource(pub Arc<InferenceFn>);

impl InferenceSource {
    /// A source that reads its events from the feed as they arrive.
    pub fn streaming(
        f: impl Fn(EventFeed<'_>) -> Result<InferenceSet> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(f))
    }

    /// A source that wants the events as a slice. The feed is collected
    /// first, so memory grows with the history; for tests and small inputs.
    pub fn from_slice(
        f: impl Fn(&[Event]) -> Result<InferenceSet> + Send + Sync + 'static,
    ) -> Self {
        Self::streaming(move |feed| {
            let mut all: Vec<Event> = Vec::new();
            feed(&mut |e: &Event| all.push(e.clone()))?;
            f(&all)
        })
    }
}

impl fmt::Debug for InferenceSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InferenceSource")
    }
}

/// What the inference half of a run did.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct InferenceReport {
    /// Items computed (after the provenance and kind filters).
    pub items: usize,
    pub kinds: usize,
    /// Stored by the server.
    pub uploaded: usize,
    pub rejected: usize,
    /// Identical to the last upload: nothing was sent.
    pub unchanged: bool,
    /// Items beyond [`MAX_INFERENCE_ITEMS`] per kind, dropped (oldest first).
    pub truncated: usize,
    /// Content-bearing fields removed because `send_content` is off.
    pub content_removed: usize,
    /// More than this many events were to be projected
    /// ([`PeerConfig::inference_max_events`]): the set was not computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_over_events: Option<usize>,
}

#[derive(Deserialize)]
struct Ack {
    #[serde(default)]
    accepted: usize,
    #[serde(default)]
    duplicates: usize,
    #[serde(default)]
    rejected: Vec<Value>,
    #[serde(default)]
    redactions: usize,
    #[serde(default)]
    stripped_content: usize,
}

/// Open the database read-only: coexists with a running daemon. The key
/// provider comes along: once the daemon's periodic flush has moved an
/// event's content into an encrypted blob, a content profile can only
/// read the conversation back with the key.
fn open_read_only(locator: &Locator) -> Result<Database> {
    Database::open(
        &locator.db_dir,
        OpenOptions {
            read_only: true,
            keys: crate::keys::provider_for_db(locator, &locator.db_dir),
            ..Default::default()
        },
    )
    .with_context(|| format!("opening {} read-only", locator.db_dir.display()))
}

/// Upload everything after peer `peer`'s cursor, one batch at a time, in
/// order.
pub fn upload_once(locator: &Locator, peer: &str, cfg: &PeerConfig) -> Result<UploadReport> {
    upload_once_with(locator, peer, cfg, None)
}

/// A segment file the manifest in hand lists is gone: a compaction replaced
/// it while a long upload was reading. The scan starts again from the cursor
/// against a fresh manifest ([`retrying`]).
#[derive(Debug, thiserror::Error)]
#[error("segment {0} was replaced while uploading")]
struct Vanished(String);

/// The history to project is larger than [`PeerConfig::inference_max_events`].
#[derive(Debug, thiserror::Error)]
#[error("more than {0} events to project")]
struct InferenceTooLarge(usize);

/// Times one run re-opens the database because a segment vanished under it.
const MAX_REOPENS: usize = 5;

/// Run `f` against the database, opened read-only, and run it again against a
/// fresh open when a compaction replaced a segment it was reading.
fn retrying<T>(locator: &Locator, mut f: impl FnMut(&Database) -> Result<T>) -> Result<T> {
    let mut reopened = 0;
    loop {
        let db = open_read_only(locator)?;
        match f(&db) {
            Err(e) if e.downcast_ref::<Vanished>().is_some() && reopened < MAX_REOPENS => {
                reopened += 1;
            }
            other => return other,
        }
    }
}

/// What a caller may ask of one upload run beyond the defaults.
#[derive(Clone, Copy, Debug, Default)]
pub struct UploadOptions {
    /// Compute and upload the inference set now, however recently it was
    /// computed (`attempt sync now --inferences`).
    pub force_inferences: bool,
}

// ---------------------------------------------------------------------------
// The idle tick
// ---------------------------------------------------------------------------

/// What the database looks like from outside, without opening it: the name,
/// size and modification time of every generation of the manifest and every
/// WAL file. A flush, a compaction or one appended event changes it; a tick
/// that finds it unchanged since a run that had nothing left to do has
/// nothing to do.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DbStamp(Vec<(String, u64, u128)>);

fn dir_stamp(db_dir: &Path) -> Option<DbStamp> {
    let mut out = Vec::new();
    for dir in [
        attemptdb_storage::format::MANIFEST_DIR,
        attemptdb_storage::format::WAL_DIR,
    ] {
        let Ok(entries) = std::fs::read_dir(db_dir.join(dir)) else {
            continue;
        };
        for entry in entries {
            let entry = entry.ok()?;
            let md = entry.metadata().ok()?;
            let modified = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            out.push((
                format!("{dir}/{}", entry.file_name().to_string_lossy()),
                md.len(),
                modified,
            ));
        }
    }
    out.sort();
    Some(DbStamp(out))
}

fn file_stamp(path: &Path) -> Option<(u64, u128)> {
    let md = std::fs::metadata(path).ok()?;
    let modified = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    Some((md.len(), modified))
}

/// The end of a run that left nothing to do: valid for the same database
/// files, the same configuration, the same cursor file, and until `until`
/// (when a debounced inference recompute comes due).
#[derive(Clone, Debug)]
struct IdleMark {
    db: DbStamp,
    cfg: String,
    state: Option<(u64, u128)>,
    until: Option<std::time::Instant>,
    report: UploadReport,
}

type IdleKey = (PathBuf, String);

fn idle_marks() -> &'static std::sync::Mutex<BTreeMap<IdleKey, IdleMark>> {
    static MARKS: std::sync::OnceLock<std::sync::Mutex<BTreeMap<IdleKey, IdleMark>>> =
        std::sync::OnceLock::new();
    MARKS.get_or_init(Default::default)
}

/// A digest of everything in the peer's configuration that decides what a
/// run does.
fn cfg_signature(cfg: &PeerConfig) -> String {
    let text = serde_json::to_string(cfg).unwrap_or_default();
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// `Some(report)` when this tick can be answered without opening the
/// database: a previous run in this process ended with nothing left to do and
/// nothing has changed since — not the database files, the configuration, or
/// the cursor file. An idle tick then costs a few `stat` calls.
fn idle_tick(
    locator: &Locator,
    peer: &str,
    cfg: &PeerConfig,
    opts: UploadOptions,
) -> Option<UploadReport> {
    if opts.force_inferences {
        return None;
    }
    let key = (locator.db_dir.clone(), peer.to_string());
    let mark = idle_marks().lock().ok()?.get(&key).cloned()?;
    if mark.until.is_some_and(|t| std::time::Instant::now() >= t) {
        return None;
    }
    let state_path = SyncState::path(&locator.paths.data_dir, &locator.db_dir, peer);
    (mark.cfg == cfg_signature(cfg)
        && mark.state == file_stamp(&state_path)
        && Some(&mark.db) == dir_stamp(&locator.db_dir).as_ref())
    .then_some(mark.report)
}

/// Remember that this run left nothing to do (or forget the previous mark).
#[allow(clippy::too_many_arguments)]
fn mark_idle(
    locator: &Locator,
    peer: &str,
    cfg: &PeerConfig,
    state_path: &Path,
    db_before: Option<DbStamp>,
    report: &UploadReport,
    until: Option<std::time::Instant>,
    idle: bool,
) {
    let key = (locator.db_dir.clone(), peer.to_string());
    let Ok(mut marks) = idle_marks().lock() else {
        return;
    };
    // The files must be as they were when the run began: a hook or the daemon
    // wrote while it ran, and the next tick has events to look at.
    let after = dir_stamp(&locator.db_dir);
    match (idle, db_before, after) {
        (true, Some(before), Some(after)) if before == after => {
            marks.insert(
                key,
                IdleMark {
                    db: after,
                    cfg: cfg_signature(cfg),
                    state: file_stamp(state_path),
                    until,
                    report: UploadReport {
                        cursor: report.cursor,
                        inferences: report.inferences.as_ref().map(|i| InferenceReport {
                            unchanged: true,
                            items: i.items,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                },
            );
        }
        _ => {
            marks.remove(&key);
        }
    }
}

/// Whether the inference set should be computed this run. It is a function of
/// the whole policy-allowed history, so it is not recomputed on every tick
/// that uploads an event: once ever (the first run), when asked for, when its
/// last upload failed, or when events arrived since it was computed and the
/// configured interval has passed since the last time.
fn inference_due(
    cfg: &PeerConfig,
    state: &SyncState,
    uploaded_events: bool,
    retry_due: bool,
    forced: bool,
    now: Timestamp,
) -> bool {
    if !cfg.send_inferences {
        return false;
    }
    // Never computed (a run that found the history too large counts as
    // computed: it is not retried before the interval passes).
    if forced
        || retry_due
        || state.inference_computed_at.is_none() && state.last_inference_at.is_none()
    {
        return true;
    }
    if !(state.inference_dirty || uploaded_events) {
        return false;
    }
    let waited = state
        .inference_computed_at
        .map_or(i64::MAX, |t| now.as_micros() - t.as_micros());
    // A history found too large to project is looked at again only rarely:
    // each look decodes up to the limit before giving up.
    let interval = if state.inference_skipped_over.is_some() {
        cfg.inference_interval_secs
            .max(SKIPPED_INFERENCE_RETRY_SECS)
    } else {
        cfg.inference_interval_secs
    };
    waited >= (interval as i64).saturating_mul(1_000_000)
}

/// [`upload_once`], then — when `send_inferences` is on and a source is
/// supplied — the device's inference set computed from the same
/// policy-allowed events. `peer` selects the cursor file; it is not sent.
///
/// Events are streamed from storage one record batch at a time and sent in
/// batches of `batch_events`: memory holds one batch, not the backlog, so a
/// first upload of a long history costs what a steady-state one does. The
/// inference set is computed by the source from a stream of the
/// policy-allowed, non-telemetry events (telemetry rows are dropped before
/// they are decoded); see [`inference_due`] for when.
pub fn upload_once_with(
    locator: &Locator,
    peer: &str,
    cfg: &PeerConfig,
    source: Option<&InferenceSource>,
) -> Result<UploadReport> {
    upload_once_opts(locator, peer, cfg, source, UploadOptions::default())
}

/// [`upload_once_with`] with [`UploadOptions`].
pub fn upload_once_opts(
    locator: &Locator,
    peer: &str,
    cfg: &PeerConfig,
    source: Option<&InferenceSource>,
    opts: UploadOptions,
) -> Result<UploadReport> {
    // A tick with nothing new costs a few `stat` calls, not a database open.
    if let Some(report) = idle_tick(locator, peer, cfg, opts) {
        return Ok(report);
    }
    let db_before = dir_stamp(&locator.db_dir);
    // A policy entry that cannot be read, or a URL that would send the key
    // in the clear, stops the run before anything is read or sent.
    let preflight = cfg.check_transport().and_then(|()| cfg.policy());
    let policy = match preflight {
        Ok(p) => p,
        Err(e) => {
            let (state, state_path) =
                SyncState::load_for(&locator.paths.data_dir, &locator.db_dir, peer)?;
            let mut state = state.bound_to(&cfg.url);
            state.last_error = Some(format!("{e:#}"));
            state.last_error_at = Some(Timestamp::now());
            state.failures += 1;
            state.save(&state_path)?;
            return Err(e);
        }
    };
    let (state, state_path) = SyncState::load_for(&locator.paths.data_dir, &locator.db_dir, peer)?;
    let mut state = state.bound_to(&cfg.url);
    let consent = cfg.consent.clone();
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .build();

    // A failed inference upload is retried no sooner than a minute later,
    // not on every five-second tick.
    let inference_retry_due = state
        .last_error
        .as_deref()
        .is_some_and(|e| e.starts_with("inferences:"))
        && state.last_error_at.is_none_or(|at| {
            Timestamp::now().as_micros() - at.as_micros() >= INFERENCE_RETRY_BACKOFF_MICROS
        });

    // Only what lies past the cursor is read: the manifest knows each
    // segment's `source_seq` range, so a tick that has nothing new decodes
    // nothing. Content (and so the encrypted blobs, one file each) is only
    // resolved when the profile sends it.
    let mut report = UploadReport {
        cursor: state.last_acked_source_seq,
        ..Default::default()
    };
    let mut done = 0usize;
    let mut device_id = None;
    retrying(locator, |db| {
        device_id = Some(db.device_id());
        stream_upload(
            db,
            &agent,
            cfg,
            &policy,
            consent.as_ref(),
            &mut state,
            &state_path,
            &mut report,
            &mut done,
        )
    })?;
    let device_id = device_id.expect("the database was opened");
    let uploaded_events = report.pending_before > 0;
    let mut recomputed = false;

    if cfg.send_inferences {
        let due = source.is_some()
            && inference_due(
                cfg,
                &state,
                uploaded_events,
                inference_retry_due,
                opts.force_inferences,
                Timestamp::now(),
            );
        report.inferences = match source {
            Some(source) if due => {
                // The source projects a stream of what the policy and the
                // consent allow — a forgotten range is not rebuilt from the
                // device's own history — with paths scrubbed (the server sees
                // scrubbed paths, so its projection and this one must agree
                // on what a path is, and a path in an inference field must
                // not carry a home directory out).
                let computed = retrying(locator, |db| {
                    (source.0)(&mut |visit| {
                        feed_inference_events(db, cfg, &policy, consent.as_ref(), visit)
                    })
                });
                match computed {
                    Ok(set) => {
                        let r = upload_inferences(
                            &agent,
                            cfg,
                            device_id,
                            set,
                            &mut state,
                            &state_path,
                        )?;
                        recomputed = true;
                        state.inference_computed_at = Some(Timestamp::now());
                        state.inference_dirty = false;
                        state.inference_skipped_over = None;
                        state.save(&state_path)?;
                        Some(r)
                    }
                    // Too much history to project on this device: say so, do
                    // not try again until the interval has passed.
                    Err(e) if e.downcast_ref::<InferenceTooLarge>().is_some() => {
                        recomputed = true;
                        state.inference_computed_at = Some(Timestamp::now());
                        state.inference_dirty = false;
                        state.inference_skipped_over = Some(cfg.inference_max_events as u64);
                        state.save(&state_path)?;
                        Some(InferenceReport {
                            skipped_over_events: Some(cfg.inference_max_events),
                            ..Default::default()
                        })
                    }
                    Err(e) => return Err(e.context("computing inferences")),
                }
            }
            // Not due: the server holds the set from the last computation,
            // and the events since are waiting for the interval.
            Some(_) => {
                if uploaded_events && !state.inference_dirty {
                    state.inference_dirty = true;
                    state.save(&state_path)?;
                }
                Some(InferenceReport {
                    unchanged: true,
                    items: state.inference_items as usize,
                    ..Default::default()
                })
            }
            // A caller without a projector (a bare uploader): report that
            // nothing was computed rather than pretend.
            None => Some(InferenceReport::default()),
        };
    }

    // Nothing left to do until something changes? The next tick then costs
    // a few `stat` calls (see `idle_tick`). Events waiting for a debounced
    // inference recompute end the wait when it is due.
    let waiting = cfg.send_inferences && source.is_some() && state.inference_dirty && !recomputed;
    let until = waiting.then(|| {
        let at = state.inference_computed_at.map_or(0, |t| t.as_micros());
        let interval = if state.inference_skipped_over.is_some() {
            cfg.inference_interval_secs
                .max(SKIPPED_INFERENCE_RETRY_SECS)
        } else {
            cfg.inference_interval_secs
        };
        let due_at = at + (interval as i64).saturating_mul(1_000_000);
        let wait = (due_at - Timestamp::now().as_micros()).max(0) as u64;
        std::time::Instant::now() + Duration::from_micros(wait)
    });
    let idle = report.pending_before == 0
        && report.quarantined == 0
        && state.failures == 0
        && state.last_error.is_none()
        && (!cfg.send_inferences
            || source.is_none()
            || ((state.last_inference_at.is_some() || state.inference_computed_at.is_some())
                && !recomputed));
    mark_idle(
        locator,
        peer,
        cfg,
        &state_path,
        db_before,
        &report,
        until,
        idle,
    );
    Ok(report)
}

/// Visit the events an inference set may be computed from: policy-allowed,
/// not withheld by the consent watermark, telemetry dropped before its rows
/// are decoded. Paths are scrubbed unless the profile is `full`.
fn feed_inference_events(
    db: &Database,
    cfg: &PeerConfig,
    policy: &Policy,
    consent: Option<&Consent>,
    visit: &mut dyn FnMut(&Event),
) -> Result<()> {
    let mut fed = 0usize;
    scan_events(db, cfg, 0, false, true, &mut |mut e| {
        if is_discarded_telemetry(&e)
            || !policy.allows(&e)
            || consent.is_some_and(|c| c.withholds(&e))
        {
            return Ok(());
        }
        fed += 1;
        if cfg.inference_max_events > 0 && fed > cfg.inference_max_events {
            return Err(InferenceTooLarge(cfg.inference_max_events).into());
        }
        if cfg.profile() != SyncProfile::Full {
            scrub_paths(&mut e);
        }
        visit(&e);
        Ok(())
    })
}

/// Stream the events past the cursor to the peer, a batch at a time.
#[allow(clippy::too_many_arguments)]
fn stream_upload(
    db: &Database,
    agent: &ureq::Agent,
    cfg: &PeerConfig,
    policy: &Policy,
    consent: Option<&Consent>,
    state: &mut SyncState,
    state_path: &Path,
    report: &mut UploadReport,
    done: &mut usize,
) -> Result<()> {
    let newest_seq = db.stats().last_source_seq;
    let after = state.last_acked_source_seq;
    // The cursor file is rewritten only when this run changed something: a
    // tick that finds nothing must not touch the disk.
    let at_start = serde_json::to_string(&*state).unwrap_or_default();
    let capture_mode = if cfg.sends_any_content() {
        CaptureMode::LocalSemantic
    } else {
        CaptureMode::MetadataOnly
    };
    let mut run = Run {
        agent,
        cfg,
        device_id: db.device_id(),
        state,
        state_path,
        report,
        capture_mode,
        batch_size: cfg.batch_events.clamp(1, 5_000),
        withheld: VecDeque::new(),
        server_version: None,
    };
    let mut chunk: Vec<Event> = Vec::new();
    let mut redacted = 0usize;
    if newest_seq > after {
        scan_events(db, cfg, after, cfg.sends_any_content(), false, &mut |e| {
            // Telemetry the intake discards, and a project the policy keeps
            // out, never leave and are not counted.
            if is_discarded_telemetry(&e) || !policy.allows(&e) {
                return Ok(());
            }
            // History from before the person connected is not theirs to have
            // agreed to: counted (once the run has moved past it), and kept
            // local.
            if consent.is_some_and(|c| c.withholds(&e)) {
                run.withheld.push_back(e.source_seq);
                return Ok(());
            }
            let (e, stats) = prepare_for_upload(cfg, e);
            redacted += stats.spans;
            chunk.push(e);
            if chunk.len() >= run.batch_size {
                run.send(&chunk)?;
                *done += chunk.len();
                chunk.clear();
            }
            Ok(())
        })?;
        if !chunk.is_empty() {
            run.send(&chunk)?;
            *done += chunk.len();
            chunk.clear();
        }
    }
    // Every event of the scan was either uploaded, skipped with a record,
    // withheld, or excluded by policy: the cursor covers the whole scan, so
    // those events are not re-examined on the next run.
    if newest_seq > run.state.last_acked_source_seq {
        run.state.last_acked_source_seq = newest_seq;
    }
    run.settle_withheld(u64::MAX);
    if serde_json::to_string(&*run.state).unwrap_or_default() != at_start {
        run.state.save(state_path)?;
    }
    run.report.cursor = run.state.last_acked_source_seq;
    run.report.secrets_redacted += redacted;
    run.report.pending_before = *done;
    Ok(())
}

/// Events past `after` in `source_seq` order, decoded from the segments
/// whose range reaches past it plus the WAL, one record batch at a time:
/// `sink` sees each event as soon as its batch is decoded and nothing is
/// kept. Content is resolved only when `with_content` asks for it (the
/// profile sends it): the encrypted blobs are one file each, and a metadata
/// upload never opens them. Under `messages` only the kinds that can carry
/// something said open theirs. `skip_telemetry` drops OTel observations as
/// each batch is decoded: the projection ignores them, and they are most of a
/// long-lived database.
///
/// A blob that cannot be read — no key, unreadable file — is an error, not a
/// silently empty event: checked after each batch is decoded and before any
/// of its events reaches `sink`, so a conversation never leaves the device as
/// bare metadata by accident; the caller keeps the cursor and retries.
fn scan_events(
    db: &Database,
    cfg: &PeerConfig,
    after: u64,
    with_content: bool,
    skip_telemetry: bool,
    sink: &mut dyn FnMut(Event) -> Result<()>,
) -> Result<()> {
    let reader = with_content.then(|| {
        attemptdb_storage::blobs::BlobReader::new(
            db.blob_store(),
            db.key_provider().map(|k| k.as_ref()),
        )
    });
    let all_kinds = |_: EventKind| true;
    let said_kinds = |k: EventKind| {
        matches!(
            k,
            EventKind::PromptSubmitted
                | EventKind::AgentMessage
                | EventKind::TurnStopped
                | EventKind::Unknown
        )
    };
    let wants_content: &dyn Fn(EventKind) -> bool = if cfg.send_content {
        &all_kinds
    } else {
        &said_kinds
    };
    let unreadable = |reader: &Option<attemptdb_storage::blobs::BlobReader<'_>>| -> Result<()> {
        if let Some(reader) = reader {
            let notes = reader.notes();
            if !notes.is_empty() {
                bail!(
                    "content could not be read for the `{}` profile ({}); the upload is held so \
                     nothing leaves without its text — restore the key, or `attempt sync profile \
                     semantic` to send metadata only",
                    cfg.profile(),
                    notes.join("; ")
                );
            }
        }
        Ok(())
    };
    for seg in &db.manifest().segments {
        if seg.max_source_seq <= after {
            continue;
        }
        let path = attemptdb_storage::segment::segments_dir(db.root()).join(&seg.file);
        if !path.exists() {
            return Err(Vanished(seg.file.clone()).into());
        }
        // The walk's own error type is the storage layer's; whatever stops the
        // scan on this side (an unreadable blob, a failed upload) is carried
        // out through `stopped` and ends the walk.
        let mut stopped: Option<anyhow::Error> = None;
        attemptdb_storage::segment::for_each_segment_batch(&path, &mut |b| {
            let step = (|| -> Result<()> {
                // Telemetry rows are most of a long-lived database and the
                // projection ignores them: leave them out as Arrow, so they
                // cost no event decode.
                let b = if skip_telemetry {
                    drop_telemetry_rows(b)?
                } else {
                    b
                };
                let events = attemptdb_storage::segment::batch_to_events_where(
                    &b,
                    reader.as_ref(),
                    wants_content,
                )
                .with_context(|| format!("decoding segment {}", seg.file))?;
                unreadable(&reader)?;
                for e in events {
                    if e.source_seq > after && !(skip_telemetry && e.is_telemetry()) {
                        sink(e)?;
                    }
                }
                Ok(())
            })();
            match step {
                Ok(()) => Ok(true),
                Err(e) => {
                    stopped = Some(e);
                    Ok(false)
                }
            }
        })
        .with_context(|| format!("reading segment {}", seg.file))?;
        if let Some(e) = stopped {
            return Err(e);
        }
    }
    unreadable(&reader)?;
    for e in db.memtable_events() {
        if e.source_seq > after && !(skip_telemetry && e.is_telemetry()) {
            sink(e.clone())?;
        }
    }
    Ok(())
}

/// Whether an `attrs_json` document says `source = "otel"` (what
/// [`Event::is_telemetry`] checks, together with the kind). The writer's
/// compact form is matched by text; any other spelling that mentions `otel`
/// takes the parse.
fn attrs_say_otel(attrs: &str) -> bool {
    if !attrs.contains("otel") {
        return false;
    }
    if attrs.contains("\"source\":\"otel\"") {
        return true;
    }
    serde_json::from_str::<Value>(attrs).is_ok_and(|v| v.get("source") == Some(&json!("otel")))
}

/// `b` without its OpenTelemetry rows (`kind` decodes as `unknown` and
/// `attrs.source` is `otel`: what [`Event::is_telemetry`] decides per event),
/// read from the `kind` and `attrs_json` columns alone.
fn drop_telemetry_rows(
    b: arrow::record_batch::RecordBatch,
) -> Result<arrow::record_batch::RecordBatch> {
    use arrow::array::{Array, AsArray, BooleanArray};
    use arrow::datatypes::DataType;
    let (Some(kind), Some(attrs)) = (b.column_by_name("kind"), b.column_by_name("attrs_json"))
    else {
        return Ok(b);
    };
    let kind = arrow::compute::cast(kind, &DataType::Utf8)?;
    let attrs = arrow::compute::cast(attrs, &DataType::Utf8)?;
    let (kind, attrs) = (kind.as_string::<i32>(), attrs.as_string::<i32>());
    let mut keep = Vec::with_capacity(b.num_rows());
    let mut dropped = 0usize;
    for row in 0..b.num_rows() {
        // An unreadable or missing kind decodes as `Unknown`.
        let unknown = kind.is_null(row)
            || EventKind::parse(kind.value(row)).is_none_or(|k| k == EventKind::Unknown);
        let telemetry = unknown && !attrs.is_null(row) && attrs_say_otel(attrs.value(row));
        dropped += usize::from(telemetry);
        keep.push(!telemetry);
    }
    if dropped == 0 {
        return Ok(b);
    }
    Ok(arrow::compute::filter_record_batch(
        &b,
        &BooleanArray::from(keep),
    )?)
}

/// Upload to every configured peer, one after another, in name order. A
/// failing peer keeps its own cursor and error and never stops the others;
/// the caller gets one result per peer.
pub fn upload_all(
    locator: &Locator,
    config: &SyncConfig,
    source: Option<&InferenceSource>,
) -> Vec<(String, Result<UploadReport>)> {
    upload_all_opts(locator, config, source, UploadOptions::default())
}

/// [`upload_all`] with [`UploadOptions`].
pub fn upload_all_opts(
    locator: &Locator,
    config: &SyncConfig,
    source: Option<&InferenceSource>,
    opts: UploadOptions,
) -> Vec<(String, Result<UploadReport>)> {
    config
        .peers
        .iter()
        .map(|(name, peer)| {
            let result = upload_once_opts(locator, name, peer, source, opts);
            (name.clone(), result)
        })
        .collect()
}

/// Is this the record of something a person or an agent *said*? Only those
/// events keep their conversation text under `send_messages`.
pub fn is_message_event(e: &Event) -> bool {
    match e.kind {
        EventKind::PromptSubmitted | EventKind::AgentMessage | EventKind::TurnStopped => true,
        EventKind::Unknown => {
            e.attrs.get("source").and_then(Value::as_str) == Some("otel")
                && matches!(
                    e.provider_event_name.as_str(),
                    "user_prompt"
                        | "claude_code.user_prompt"
                        | "assistant_response"
                        | "claude_code.assistant_response"
                        | "codex.user_prompt"
                        | "codex.assistant_response"
                )
        }
        _ => false,
    }
}

/// The `messages` profile in one place: keep `content.prompt` and
/// `content.message` of a message event, drop every other content-bearing
/// field and the raw payload, and strip everything from any other event.
/// Returns whether the event still carries text (the caller redacts it).
pub fn keep_messages_only(e: &mut Event) -> bool {
    e.raw = None;
    if !is_message_event(e) {
        e.capture_mode = CaptureMode::MetadataOnly;
        e.apply_capture_mode();
        return false;
    }
    if let Some(c) = &mut e.content {
        c.command = None;
        c.error = None;
        c.tool_input = None;
        c.tool_output = None;
        c.extra.clear();
        if c.prompt.as_deref().is_some_and(str::is_empty) {
            c.prompt = None;
        }
        if c.message.as_deref().is_some_and(str::is_empty) {
            c.message = None;
        }
        if c.is_empty() {
            e.content = None;
        }
    }
    if e.content.is_none() {
        e.capture_mode = CaptureMode::MetadataOnly;
        e.apply_capture_mode();
        return false;
    }
    // The server clamps to its own ceiling; the batch says what it carries.
    e.capture_mode = CaptureMode::LocalSemantic;
    true
}

/// Replace what identifies the person's machine in an event's paths with
/// what the repository can show: the repo-relative path, or `~/…` for a path
/// outside any repository; the project root and a remote that is a local path
/// the same way. A home directory is found wherever it sits near the front
/// of a path (`/mnt/c/Users/<n>` under WSL, `/var/home/<n>`,
/// `/Volumes/<vol>/Users/<n>`, …, see [`paths::elide_home`]). Returns how many
/// values changed. `Event.paths[].original` keeps the provider's spelling on
/// the device; it does not leave (RFC 0006 §4.2).
pub fn scrub_paths(e: &mut Event) -> usize {
    let mut n = 0;
    for p in &mut e.paths {
        let shown = match &p.repo_relative {
            Some(rel) => paths::elide_home(rel),
            None => paths::elide_home(&p.logical),
        };
        if p.original != shown || p.logical != shown {
            n += 1;
        }
        let elided = shown.starts_with('~');
        *p = PortablePath {
            original: shown.clone(),
            logical: shown,
            repo_relative: p.repo_relative.take().map(|r| paths::elide_home(&r)),
            drive: if elided { None } else { p.drive.take() },
            unc: p.unc,
        };
    }
    let root = paths::elide_home(&e.project.root);
    if root != e.project.root {
        // A project rooted at the home directory itself is named after the
        // account (its name is the root's last segment).
        if root == "~"
            && e.project
                .root
                .trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()
                .is_some_and(|last| last == e.project.name)
        {
            e.project.name = "~".to_string();
        }
        e.project.root = root;
        n += 1;
    }
    if let Some(remote) = &e.project.repo_remote {
        // A remote that is a path on this machine (`/home/<n>/git/x.git`)
        // carries the home directory like a root does.
        let shown = paths::elide_home(remote);
        if shown != *remote {
            e.project.repo_remote = Some(shown);
            n += 1;
        }
    }
    n
}

/// Run the secret scanner over one short string (a branch, a name, a remote)
/// in place. A scanner that panics costs the string, not the process.
fn redact_short(text: &mut String) -> usize {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| secrets::redact(text))) {
        Ok((out, spans)) if spans > 0 => {
            *text = out;
            spans
        }
        Ok(_) => 0,
        Err(_) => {
            *text = "[REDACTED:scan_failed]".to_string();
            1
        }
    }
}

/// The free-text parts of the project reference — a branch can be named
/// `feat/ghp_…`, a project after a directory or a remote with a token in it —
/// get the same secret scan the content does: they travel under every
/// profile, content or not. Returns the spans redacted.
fn redact_project(e: &mut Event) -> usize {
    let mut spans = 0;
    spans += redact_short(&mut e.project.name);
    spans += redact_short(&mut e.project.root);
    for text in [
        &mut e.project.repo_remote,
        &mut e.project.branch,
        &mut e.project.head,
    ]
    .into_iter()
    .flatten()
    {
        spans += redact_short(text);
    }
    spans
}

/// One event as the peer's profile lets it leave the device: clamped to the
/// profile's content, secrets redacted from whatever text remains, and (for
/// any profile short of `full`) paths reduced to what a repository shows.
/// Returns the event and how many secret spans were redacted from it.
pub fn prepare_for_upload(cfg: &PeerConfig, mut e: Event) -> (Event, secrets::RedactionStats) {
    let mut stats = secrets::RedactionStats::default();
    if cfg.send_content {
        // Content leaves only on explicit opt-in, and never with a
        // credential in it (RFC 0006 §5).
        stats = secrets::redact_event_content_guarded(&mut e);
    } else if cfg.send_messages && keep_messages_only(&mut e) {
        stats = secrets::redact_event_content_guarded(&mut e);
    } else {
        e.capture_mode = CaptureMode::MetadataOnly;
        e.apply_capture_mode();
    }
    if e.content.is_some() || e.raw.is_some() {
        // Which ruleset scanned the text, so a later pass knows what ran.
        e.attrs.insert(
            "x_attemptdb_secrets_ruleset".into(),
            json!(secrets::RULESET),
        );
        if stats.spans > 0 {
            e.attrs
                .insert("x_attemptdb_secrets_redacted".into(), json!(stats.spans));
        }
    }
    if cfg.profile() != SyncProfile::Full {
        scrub_paths(&mut e);
    }
    let spans = redact_project(&mut e);
    if spans > 0 {
        stats.spans += spans;
        stats.fields += 1;
        *stats.by_rule.entry("project_field").or_default() += spans;
        e.attrs.insert(
            "x_attemptdb_secrets_ruleset".into(),
            json!(secrets::RULESET),
        );
        e.attrs.insert(
            "x_attemptdb_secrets_redacted".into(),
            json!(
                e.attrs
                    .get("x_attemptdb_secrets_redacted")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    + spans as u64
            ),
        );
    }
    (e, stats)
}

/// Whether a refusal says something about the events in the request rather
/// than about the client or the server: too large, malformed, or not
/// understood by this server. Authentication (401/403), rate limits and
/// server trouble are not — and a `sync_version` mismatch refuses every
/// event alike, so skipping events would only discard the backlog.
fn is_content_rejection(e: &PostError) -> bool {
    match e {
        PostError::TooLarge => true,
        PostError::Rejected { status, message } => {
            matches!(*status, 400 | 413 | 422) && !message.contains("sync_version")
        }
        _ => false,
    }
}

/// The wire body of one event batch (RFC 0006 §10.3).
fn batch_body(
    device_id: attemptdb_core::DeviceId,
    capture_mode: CaptureMode,
    events: &[Event],
) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&json!({
        "sync_version": 1,
        "device_id": device_id,
        "batch_id": EventId::new().to_string(),
        "capture_mode": capture_mode.as_str(),
        "events": events,
    }))?)
}

/// One run of the event uploader: the cursor, the counters, and the batch
/// size, with the rules for what to do when the server says no.
struct Run<'a> {
    agent: &'a ureq::Agent,
    cfg: &'a PeerConfig,
    device_id: attemptdb_core::DeviceId,
    state: &'a mut SyncState,
    state_path: &'a Path,
    report: &'a mut UploadReport,
    capture_mode: CaptureMode,
    batch_size: usize,
    /// `source_seq` of each event the consent watermark held back, in scan
    /// order, not yet counted: an event is counted once the cursor has moved
    /// past it, so a run that fails and is retried does not count it twice.
    withheld: VecDeque<u64>,
    /// The server's reported version, asked once per run when something is
    /// first set aside (recorded with that event).
    server_version: Option<Option<String>>,
}

impl Run<'_> {
    fn body(&self, events: &[Event]) -> Result<Vec<u8>> {
        batch_body(self.device_id, self.capture_mode, events)
    }

    /// Send `chunk` (events already prepared, in `source_seq` order). A batch
    /// the server finds too large or cannot read is split in half and each
    /// half sent in turn, so the one event at fault is found in a logarithmic
    /// number of requests and everything else goes through; a single event
    /// that is refused on its own merits is [`Run::refused`]. Any other
    /// failure stops the run with the cursor where the last success left it.
    fn send(&mut self, chunk: &[Event]) -> Result<()> {
        let body = self.body(chunk)?;
        if body.len() > MAX_BODY_BYTES && chunk.len() > 1 {
            self.batch_size = (chunk.len() / 2).max(1);
            return self.split(chunk);
        }
        match post(self.agent, self.cfg, &body) {
            Ok(ack) => {
                self.acked(chunk, &ack)?;
                Ok(())
            }
            Err(e) if is_content_rejection(&e) => {
                if matches!(e, PostError::TooLarge) {
                    self.batch_size = (chunk.len() / 2).max(1);
                }
                if chunk.len() > 1 {
                    self.split(chunk)
                } else {
                    self.refused(&chunk[0], e)
                }
            }
            Err(e) => Err(self.failed(e.to_string())),
        }
    }

    fn split(&mut self, chunk: &[Event]) -> Result<()> {
        let mid = chunk.len() / 2;
        self.send(&chunk[..mid])?;
        self.send(&chunk[mid..])
    }

    /// The server took `chunk`: the cursor moves to its last event.
    fn acked(&mut self, chunk: &[Event], ack: &Ack) -> Result<()> {
        let last = chunk.last().expect("non-empty chunk");
        self.advance(last);
        self.state.batches += 1;
        self.state.events += ack.accepted as u64;
        self.state.duplicates += ack.duplicates as u64;
        self.state.rejected += ack.rejected.len() as u64;
        self.state.last_ok_at = Some(Timestamp::now());
        self.state.last_error = None;
        self.state.last_error_at = None;
        self.state.failures = 0;
        self.state.quarantine_streak = 0;
        self.state.save(self.state_path)?;
        self.report.batches += 1;
        self.report.accepted += ack.accepted;
        self.report.duplicates += ack.duplicates;
        self.report.rejected += ack.rejected.len();
        self.report.redactions += ack.redactions;
        self.report.stripped_content += ack.stripped_content;
        self.report.cursor = self.state.last_acked_source_seq;
        Ok(())
    }

    fn advance(&mut self, ev: &Event) {
        if ev.source_seq > self.state.last_acked_source_seq {
            self.state.last_acked_source_seq = ev.source_seq;
            self.state.last_acked_hlc = ev.hlc.as_u64();
        }
        self.report.cursor = self.state.last_acked_source_seq;
        self.settle_withheld(ev.source_seq);
    }

    /// Count the withheld events the cursor has moved past (`upto`).
    fn settle_withheld(&mut self, upto: u64) {
        let mut n = 0usize;
        while self.withheld.front().is_some_and(|s| *s <= upto) {
            self.withheld.pop_front();
            n += 1;
        }
        if n > 0 {
            self.state.before_consent += n as u64;
            self.report.before_consent += n;
        }
    }

    /// The server's version, asked at most once per run.
    fn server_version(&mut self) -> Option<String> {
        if self.server_version.is_none() {
            self.server_version = Some(health_version(self.agent, self.cfg));
        }
        self.server_version.clone().flatten()
    }

    /// One event the server refuses on its own: too large, or something this
    /// server cannot read (a newer event kind than it knows, say). It must not
    /// wedge the cursor for everything behind it. Its text is withheld and its
    /// metadata tried alone; failing that it is skipped. Either way a
    /// quarantine record says which event, and what the server said, and the
    /// run reports it. A run of refusals with nothing accepted between them is
    /// a server refusing this client, not one event, and stops the skipping.
    fn refused(&mut self, ev: &Event, err: PostError) -> Result<()> {
        let (status, reason) = match &err {
            PostError::TooLarge => (
                413,
                "the event alone is larger than the server accepts".to_string(),
            ),
            PostError::Rejected { status, message } => (*status, message.clone()),
            other => (0, other.to_string()),
        };
        if self.state.quarantine_streak >= MAX_QUARANTINE_STREAK {
            return Err(self.failed(format!(
                "the server refused {} events in a row (last: {status}: {reason}); not skipping \
                 more — it is refusing this client, not one event",
                self.state.quarantine_streak
            )));
        }
        let mut action = "skipped";
        if ev.content.is_some() || ev.raw.is_some() {
            let mut bare = ev.clone();
            bare.content = None;
            bare.raw = None;
            bare.capture_mode = CaptureMode::MetadataOnly;
            let body = self.body(std::slice::from_ref(&bare))?;
            match post(self.agent, self.cfg, &body) {
                Ok(ack) => {
                    self.acked(std::slice::from_ref(ev), &ack)?;
                    action = "content_withheld";
                }
                Err(e) if is_content_rejection(&e) => {}
                Err(e) => return Err(self.failed(e.to_string())),
            }
        }
        if action == "skipped" {
            self.advance(ev);
            self.state.quarantine_streak += 1;
        } else {
            self.state.content_withheld += 1;
            self.report.content_withheld += 1;
        }
        self.state.quarantined += 1;
        self.report.quarantined += 1;
        let server_version = self.server_version();
        self.state.quarantine.push(QuarantineRecord {
            event_id: ev.event_id,
            source_seq: ev.source_seq,
            action: action.to_string(),
            status,
            reason: reason.chars().take(200).collect(),
            at: Timestamp::now(),
            server_version,
        });
        let extra = self
            .state
            .quarantine
            .len()
            .saturating_sub(MAX_QUARANTINE_RECORDS);
        self.state.quarantine.drain(..extra);
        self.state.save(self.state_path)?;
        Ok(())
    }

    /// Record a failure that leaves the cursor where it is.
    fn failed(&mut self, message: String) -> anyhow::Error {
        self.state.last_error = Some(message.clone());
        self.state.last_error_at = Some(Timestamp::now());
        self.state.failures += 1;
        let saved = self.state.save(self.state_path);
        let cursor = self.state.last_acked_source_seq;
        match saved {
            Ok(()) => anyhow!("{message} (cursor kept at {cursor})"),
            Err(e) => anyhow!("{message} (cursor kept at {cursor}; and the cursor file: {e:#})"),
        }
    }
}

/// Fields of an inference row that carry captured text. Removed unless the
/// device opted into `send_content`.
const CONTENT_FIELDS: &[&str] = &["objective", "rationale"];

/// Null out content-bearing fields; returns how many held a value.
pub fn strip_inference_content(fields: &mut Value) -> usize {
    let Some(obj) = fields.as_object_mut() else {
        return 0;
    };
    let mut n = 0;
    for key in CONTENT_FIELDS {
        if let Some(v) = obj.get_mut(*key)
            && !v.is_null()
        {
            *v = Value::Null;
            n += 1;
        }
    }
    n
}

/// Stable digest of an inference set: sorted by (kind, id), prefixed with
/// the algorithm version, so an unchanged projection is not re-sent.
pub fn inference_digest(algorithm_version: &str, items: &[InferenceItem]) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(algorithm_version.as_bytes());
    hasher.update(b"\n");
    hasher.update(serde_json::to_vec(items)?);
    Ok(hex::encode(hasher.finalize()))
}

/// Apply the device policy to a computed set: drop unknown kinds and items
/// without evidence, strip or redact content, sort for a stable digest.
pub fn prepare_inferences(
    cfg: &PeerConfig,
    mut items: Vec<InferenceItem>,
) -> (Vec<InferenceItem>, usize) {
    items.retain(|it| INFERENCE_KINDS.contains(&it.kind.as_str()) && !it.evidence.is_empty());
    let mut content_removed = 0;
    for it in &mut items {
        if cfg.send_content {
            secrets::redact_value(&mut it.fields);
        } else {
            content_removed += strip_inference_content(&mut it.fields);
        }
    }
    items.sort_by(|a, b| (a.kind.as_str(), a.id.as_str()).cmp(&(b.kind.as_str(), b.id.as_str())));
    (items, content_removed)
}

/// The wire body of one inference upload (`spec/inference-v1.schema.json`).
pub fn inference_batch_body(
    device_id: attemptdb_core::DeviceId,
    kind: &str,
    algorithm_version: &str,
    computed_at: Timestamp,
    items: &[&InferenceItem],
) -> Value {
    json!({
        "sync_version": 1,
        "schema": INFERENCE_SCHEMA,
        "device_id": device_id,
        "batch_id": EventId::new().to_string(),
        "kind": kind,
        "algorithm_version": algorithm_version,
        "computed_at": computed_at,
        "items": items,
    })
}

#[derive(Deserialize)]
struct InferenceAck {
    #[serde(default)]
    stored: usize,
    #[serde(default)]
    rejected: Vec<Value>,
}

/// An inference upload went through: the failure that was waiting to be
/// retried is over.
fn clear_inference_error(state: &mut SyncState) {
    if state
        .last_error
        .as_deref()
        .is_some_and(|e| e.starts_with("inferences:"))
    {
        state.last_error = None;
        state.last_error_at = None;
        state.failures = 0;
    }
}

fn upload_inferences(
    agent: &ureq::Agent,
    cfg: &PeerConfig,
    device_id: attemptdb_core::DeviceId,
    set: InferenceSet,
    state: &mut SyncState,
    state_path: &Path,
) -> Result<InferenceReport> {
    let (items, content_removed) = prepare_inferences(cfg, set.items);
    let digest = inference_digest(&set.algorithm_version, &items)?;
    let mut report = InferenceReport {
        items: items.len(),
        content_removed,
        ..Default::default()
    };
    if state.last_inference_digest.as_deref() == Some(digest.as_str()) {
        report.unchanged = true;
        clear_inference_error(state);
        return Ok(report);
    }
    let mut by_kind: BTreeMap<&str, Vec<&InferenceItem>> = BTreeMap::new();
    for it in &items {
        by_kind.entry(it.kind.as_str()).or_default().push(it);
    }
    for (kind, list) in by_kind {
        let keep = list.len().min(MAX_INFERENCE_ITEMS);
        report.truncated += list.len() - keep;
        let list = &list[list.len() - keep..];
        let sizes: Vec<usize> = list
            .iter()
            .map(|it| serde_json::to_vec(it).map_or(0, |v| v.len()))
            .collect();
        let fit = newest_within_budget(&sizes, MAX_INFERENCE_BODY_BYTES);
        report.truncated += list.len() - fit;
        if fit == 0 {
            continue;
        }
        let list = &list[list.len() - fit..];
        let body = serde_json::to_vec(&inference_batch_body(
            device_id,
            kind,
            &set.algorithm_version,
            set.computed_at,
            list,
        ))?;
        match post_inferences(agent, cfg, &body) {
            Ok(ack) => {
                report.kinds += 1;
                report.uploaded += ack.stored;
                report.rejected += ack.rejected.len();
            }
            Err(e) => {
                state.last_error = Some(format!("inferences: {e}"));
                state.last_error_at = Some(Timestamp::now());
                state.failures += 1;
                state.save(state_path)?;
                return Err(anyhow!("inferences ({kind}): {e}"));
            }
        }
    }
    clear_inference_error(state);
    state.inference_uploads += 1;
    state.inference_items = report.uploaded as u64;
    state.last_inference_digest = Some(digest);
    state.last_inference_at = Some(Timestamp::now());
    state.save(state_path)?;
    Ok(report)
}

/// How many of the newest items fit within `budget` serialised bytes
/// (`sizes` is oldest first; each item costs its size plus a separator).
fn newest_within_budget(sizes: &[usize], budget: usize) -> usize {
    let mut used = 0usize;
    let mut keep = 0usize;
    for size in sizes.iter().rev() {
        used = used.saturating_add(size + 1);
        if used > budget {
            break;
        }
        keep += 1;
    }
    keep
}

fn post_inferences(
    agent: &ureq::Agent,
    cfg: &PeerConfig,
    body: &[u8],
) -> Result<InferenceAck, PostError> {
    let url = cfg.endpoint_inferences();
    let response = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", cfg.key))
        .set("Content-Type", "application/json")
        .send_bytes(body);
    match response {
        Ok(r) => {
            let text = r
                .into_string()
                .map_err(|e| PostError::BadAck(e.to_string()))?;
            serde_json::from_str(&text).map_err(|e| PostError::BadAck(e.to_string()))
        }
        Err(ureq::Error::Status(413, _)) => Err(PostError::TooLarge),
        Err(ureq::Error::Status(status, r)) => {
            let text = r.into_string().unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(String::from))
                .unwrap_or(text);
            if status == 429 || status >= 500 {
                Err(PostError::Retryable { status, message })
            } else {
                Err(PostError::Rejected { status, message })
            }
        }
        Err(ureq::Error::Transport(t)) => Err(PostError::Transport {
            url,
            message: t.to_string(),
        }),
    }
}

#[derive(Debug, thiserror::Error)]
enum PostError {
    #[error("server refused the body as too large")]
    TooLarge,
    /// The server or the network is the problem: keep the batch, retry later.
    #[error("upload failed ({status}): {message}; will retry")]
    Retryable { status: u16, message: String },
    /// Something about this client is wrong: stop and say so.
    #[error("server rejected the request ({status}): {message}")]
    Rejected { status: u16, message: String },
    #[error("cannot reach {url}: {message}")]
    Transport { url: String, message: String },
    #[error("unreadable acknowledgement: {0}")]
    BadAck(String),
}

/// The body limit the server runs with unless its operator changed it.
const SERVER_BODY_LIMIT: usize = 4 * 1024 * 1024;

fn health_url(cfg: &PeerConfig) -> String {
    format!("{}/v1/health", cfg.url.trim_end_matches('/'))
}

/// The server's own version (`server_version` of `/v1/health`), when it says.
fn health_version(agent: &ureq::Agent, cfg: &PeerConfig) -> Option<String> {
    let r = agent
        .get(&health_url(cfg))
        .timeout(Duration::from_secs(10))
        .call()
        .ok()?;
    let v: Value = serde_json::from_str(&r.into_string().ok()?).ok()?;
    v["server_version"].as_str().map(str::to_string)
}

/// Whether something HTTP answers at the server's address (any status).
fn server_answers(agent: &ureq::Agent, cfg: &PeerConfig) -> bool {
    matches!(
        agent
            .get(&health_url(cfg))
            .timeout(Duration::from_secs(10))
            .call(),
        Ok(_) | Err(ureq::Error::Status(..))
    )
}

fn post(agent: &ureq::Agent, cfg: &PeerConfig, body: &[u8]) -> Result<Ack, PostError> {
    let url = cfg.endpoint();
    let response = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", cfg.key))
        .set("Content-Type", "application/json")
        .send_bytes(body);
    match response {
        Ok(r) => {
            let text = r
                .into_string()
                .map_err(|e| PostError::BadAck(e.to_string()))?;
            serde_json::from_str(&text).map_err(|e| PostError::BadAck(format!("{e}: {text}")))
        }
        Err(ureq::Error::Status(status, r)) => {
            let text = r.into_string().unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or(text);
            match status {
                413 => Err(PostError::TooLarge),
                s if s >= 500 || s == 408 || s == 429 => {
                    Err(PostError::Retryable { status: s, message })
                }
                s => Err(PostError::Rejected { status: s, message }),
            }
        }
        Err(ureq::Error::Transport(t)) => {
            // A server that refuses a body over its limit may drop the
            // connection while the client is still writing, before its 413
            // can be read (older servers did, and a proxy in front may). A
            // reset on a body past the default limit, while the server
            // answers a plain request, is that refusal: the event is too
            // large, not the network down — retrying it for ever would wedge
            // everything behind it.
            if body.len() > SERVER_BODY_LIMIT
                && t.kind() == ureq::ErrorKind::Io
                && server_answers(agent, cfg)
            {
                return Err(PostError::TooLarge);
            }
            Err(PostError::Transport {
                url,
                message: t.to_string(),
            })
        }
    }
}

/// What the server said when a key was tried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handshake {
    pub device_id: attemptdb_core::DeviceId,
    pub url: String,
}

/// Prove a peer's key works for this device before anything depends on
/// it: an empty batch under the key. The server authenticates it, checks
/// the batch's device against the key's, and answers `accepted: 0` — or
/// `401` (unknown key), `403` (a key for another device), which are the
/// two ways a connection is wrong and the two things a health check
/// cannot tell.
pub fn handshake(locator: &Locator, cfg: &PeerConfig) -> Result<Handshake> {
    cfg.check_transport()?;
    let db = open_read_only(locator)?;
    let device_id = db.device_id();
    drop(db);
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build();
    let body = serde_json::to_vec(&json!({
        "sync_version": 1,
        "device_id": device_id,
        "batch_id": EventId::new().to_string(),
        "capture_mode": CaptureMode::MetadataOnly.as_str(),
        "events": [],
    }))?;
    match post(&agent, cfg, &body) {
        Ok(_) => Ok(Handshake {
            device_id,
            url: cfg.url.clone(),
        }),
        Err(PostError::Rejected { status: 401, .. }) => Err(anyhow!(
            "the server at {} does not know this key (401): it may have been revoked, or belongs to another server",
            cfg.url
        )),
        Err(PostError::Rejected {
            status: 403,
            message,
        }) => Err(anyhow!(
            "the key is not for this device (403: {message}); this database's device is dev_{device_id} — pair again from this machine",
        )),
        Err(e) => Err(anyhow!("{e}")),
    }
}

/// What a pairing exchange returns: the key, once.
#[derive(Clone, Debug, Deserialize)]
pub struct Paired {
    pub key: String,
    pub tenant: String,
    pub device_id: attemptdb_core::DeviceId,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub user_id: Option<String>,
}

/// Is a pairing token still good? `Ok(tenant)` when it is; the error says
/// why not (expired, used, unknown, unreachable).
pub fn check_pairing(url: &str, token: &str) -> Result<String> {
    let url = url.trim_end_matches('/');
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build();
    match agent.get(&format!("{url}/v1/pair/{}", token.trim())).call() {
        Ok(r) => {
            let text = r.into_string().context("reading the pairing check")?;
            let v: Value = serde_json::from_str(&text).context("parsing the pairing check")?;
            Ok(v["tenant"].as_str().unwrap_or_default().to_string())
        }
        Err(ureq::Error::Status(status, r)) => {
            let text = r.into_string().unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or(text);
            Err(anyhow!("pairing token refused ({status}): {message}"))
        }
        Err(ureq::Error::Transport(t)) => Err(anyhow!("cannot reach {url}: {t}")),
    }
}

/// Exchange a one-time pairing token and this database's device id for a
/// device key. The token dies on the server whether or not the key is
/// saved afterwards, so callers save first and report after.
pub fn pair(locator: &Locator, url: &str, token: &str, label: Option<&str>) -> Result<Paired> {
    let db = open_read_only(locator)?;
    let device_id = db.device_id();
    drop(db);
    let url = url.trim_end_matches('/');
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build();
    let body = json!({ "token": token.trim(), "device_id": device_id, "label": label });
    match agent
        .post(&format!("{url}/v1/pair"))
        .set("Content-Type", "application/json")
        .send_bytes(&serde_json::to_vec(&body)?)
    {
        Ok(r) => {
            let text = r.into_string().context("reading the pairing response")?;
            serde_json::from_str(&text).context("parsing the pairing response")
        }
        Err(ureq::Error::Status(status, r)) => {
            let text = r.into_string().unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or(text);
            Err(anyhow!("pairing refused ({status}): {message}"))
        }
        Err(ureq::Error::Transport(t)) => Err(anyhow!("cannot reach {url}: {t}")),
    }
}

/// What asking the server to revoke this device's key came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevokeOutcome {
    /// The server revoked the key: its next request gets 401.
    Revoked,
    /// The server does not know the key (revoked earlier, or never issued
    /// by it): nothing to do there.
    AlreadyGone,
    /// An older server with no revoke route: its operator has to do it.
    Unsupported,
    /// The server could not be reached; the key is still valid there.
    Unreachable(String),
    /// The server answered something else.
    Refused(u16, String),
}

fn error_message(r: ureq::Response) -> String {
    let text = r.into_string().unwrap_or_default();
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or(text)
}

/// Ask the server to revoke the key this peer holds (`POST /v1/sync/revoke`).
/// Best effort: the caller drops the peer from `sync.json` whatever this
/// returns, and reports it. What was uploaded stays on the server — this
/// revokes the key, it does not delete data (see [`forget_remote`]).
pub fn revoke_key(cfg: &PeerConfig) -> RevokeOutcome {
    if let Err(e) = cfg.check_transport() {
        return RevokeOutcome::Unreachable(format!("{e:#}"));
    }
    let url = format!("{}/v1/sync/revoke", cfg.url.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build();
    match agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", cfg.key))
        .set("Content-Type", "application/json")
        .send_bytes(b"{}")
    {
        Ok(_) => RevokeOutcome::Revoked,
        Err(ureq::Error::Status(401, _)) => RevokeOutcome::AlreadyGone,
        Err(ureq::Error::Status(404 | 405, _)) => RevokeOutcome::Unsupported,
        Err(ureq::Error::Status(status, r)) => RevokeOutcome::Refused(status, error_message(r)),
        Err(ureq::Error::Transport(t)) => RevokeOutcome::Unreachable(format!("{url}: {t}")),
    }
}

/// What a server-side deletion of this device's events did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetReport {
    pub events_deleted: u64,
    /// Rows left in the tenant (other devices' and the server's own).
    pub events_kept: u64,
    pub inference_documents_removed: u64,
    /// What a deletion cannot reach, as the server states it.
    pub not_reached: Vec<String>,
}

/// Ask the server to delete every event this device uploaded
/// (`POST /v1/sync/forget`). The key stays valid and the local cursor is not
/// touched: nothing already past it is uploaded again.
pub fn forget_remote(cfg: &PeerConfig) -> Result<ForgetReport> {
    cfg.check_transport()?;
    let url = format!("{}/v1/sync/forget", cfg.url.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(300))
        .build();
    match agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", cfg.key))
        .set("Content-Type", "application/json")
        .send_bytes(br#"{"confirm":true}"#)
    {
        Ok(r) => {
            let text = r.into_string().context("reading the answer")?;
            let v: Value = serde_json::from_str(&text).context("parsing the answer")?;
            Ok(ForgetReport {
                events_deleted: v["outcome"]["events_deleted"].as_u64().unwrap_or(0),
                events_kept: v["outcome"]["events_kept"].as_u64().unwrap_or(0),
                inference_documents_removed: v["outcome"]["inference_documents_removed"]
                    .as_u64()
                    .unwrap_or(0),
                not_reached: v["not_reached"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        }
        Err(ureq::Error::Status(401, _)) => Err(anyhow!(
            "the server does not know this key (401): it was revoked, or belongs to another server"
        )),
        Err(ureq::Error::Status(404 | 405, _)) => Err(anyhow!(
            "{} has no deletion route: it runs an older server. Ask its operator to delete this \
             device's events (DELETE /v1/admin/devices/<device>/events once upgraded)",
            cfg.url
        )),
        Err(ureq::Error::Status(status, r)) => Err(anyhow!(
            "the server refused ({status}): {}",
            error_message(r)
        )),
        Err(ureq::Error::Transport(t)) => Err(anyhow!("cannot reach {url}: {t}")),
    }
}

// ---------------------------------------------------------------------------
// What this device has recorded, for the policy; events set aside
// ---------------------------------------------------------------------------

/// A project this device has recorded events for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeenProject {
    pub project_id: ProjectId,
    pub name: String,
    /// The remote in [`repo_key`] form, when the project has one.
    pub remote: Option<String>,
    pub events: u64,
}

/// The database's newest `source_seq` (0 when there is no database yet): the
/// point the consent watermark is set at.
pub fn local_newest_seq(locator: &Locator) -> Result<u64> {
    if !Database::exists(&locator.db_dir) {
        return Ok(0);
    }
    Ok(open_read_only(locator)?.stats().last_source_seq)
}

/// Every project the database has events for, with its remote in matching
/// form. Reads three columns of each segment and the WAL's events; nothing is
/// decoded into events.
pub fn seen_projects(locator: &Locator) -> Result<Vec<SeenProject>> {
    use arrow::array::{Array, AsArray};
    use arrow::datatypes::DataType;
    if !Database::exists(&locator.db_dir) {
        return Ok(Vec::new());
    }
    type Seen = BTreeMap<ProjectId, (String, Option<String>, u64)>;
    let tally = |seen: &mut Seen, id: ProjectId, name: &str, remote: Option<&str>, n: u64| {
        let slot = seen
            .entry(id)
            .or_insert_with(|| (name.to_string(), remote.and_then(repo_key), 0));
        slot.2 += n;
    };
    let mut seen = Seen::new();
    retrying(locator, |db| {
        seen.clear();
        for seg in &db.manifest().segments {
            let path = attemptdb_storage::segment::segments_dir(db.root()).join(&seg.file);
            if !path.exists() {
                return Err(Vanished(seg.file.clone()).into());
            }
            attemptdb_storage::segment::for_each_segment_columns(
                &path,
                &["project_id", "project_name", "repo_remote"],
                &mut |b| {
                    let (Some(ids), Some(names), Some(remotes)) = (
                        b.column_by_name("project_id"),
                        b.column_by_name("project_name"),
                        b.column_by_name("repo_remote"),
                    ) else {
                        return Ok(true);
                    };
                    let ids = ids.as_fixed_size_binary();
                    let names = arrow::compute::cast(names, &DataType::Utf8)?;
                    let remotes = arrow::compute::cast(remotes, &DataType::Utf8)?;
                    let (names, remotes) = (names.as_string::<i32>(), remotes.as_string::<i32>());
                    for row in 0..b.num_rows() {
                        if ids.is_null(row) {
                            continue;
                        }
                        let mut raw = [0u8; 16];
                        raw.copy_from_slice(ids.value(row));
                        tally(
                            &mut seen,
                            ProjectId::from_bytes(raw),
                            if names.is_null(row) {
                                ""
                            } else {
                                names.value(row)
                            },
                            (!remotes.is_null(row)).then(|| remotes.value(row)),
                            1,
                        );
                    }
                    Ok(true)
                },
            )
            .with_context(|| format!("reading segment {}", seg.file))?;
        }
        for e in db.memtable_events() {
            tally(
                &mut seen,
                e.project.project_id,
                &e.project.name,
                e.project.repo_remote.as_deref(),
                1,
            );
        }
        Ok(())
    })?;
    Ok(seen
        .into_iter()
        .map(|(project_id, (name, remote, events))| SeenProject {
            project_id,
            name,
            remote,
            events,
        })
        .collect())
}

/// Whether a policy entry names a project this device has recorded.
pub fn entry_matches_seen(entry: &PolicyKey, seen: &[SeenProject]) -> bool {
    seen.iter().any(|p| match entry {
        PolicyKey::Project(id) => p.project_id == *id,
        PolicyKey::Remote(r) => p.remote.as_deref() == Some(r.as_str()),
    })
}

/// Levenshtein distance, for "did you mean".
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The recorded projects an entry that matched none is probably meant to
/// name, nearest first (at most three): the same `owner/repo` under another
/// host (an ssh alias), the same repository name, then spellings a few
/// characters apart.
pub fn nearest_projects<'a>(entry: &PolicyKey, seen: &'a [SeenProject]) -> Vec<&'a SeenProject> {
    let PolicyKey::Remote(want) = entry else {
        return Vec::new();
    };
    let tail = |s: &str| -> (String, String) {
        let parts: Vec<&str> = s.split('/').collect();
        let n = parts.len();
        let repo = parts.last().copied().unwrap_or("").to_string();
        let owner_repo = if n >= 3 {
            parts[n - 2..].join("/")
        } else {
            s.to_string()
        };
        (owner_repo, repo)
    };
    let (want_pair, want_repo) = tail(want);
    let mut scored: Vec<(usize, std::cmp::Reverse<u64>, &SeenProject)> = seen
        .iter()
        .filter_map(|p| {
            let remote = p.remote.as_deref()?;
            let (pair, repo) = tail(remote);
            let score = if pair == want_pair {
                0
            } else if repo == want_repo {
                1
            } else {
                let d = edit_distance(want, remote);
                if d <= (want.len() / 4).max(3) {
                    2 + d
                } else {
                    return None;
                }
            };
            Some((score, std::cmp::Reverse(p.events), p))
        })
        .collect();
    scored.sort_by_key(|s| (s.0, s.1));
    scored.into_iter().take(3).map(|(_, _, p)| p).collect()
}

/// What delivering the set-aside events again came to.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct RetryReport {
    /// Set-aside events looked at.
    pub tried: usize,
    /// Accepted by the server this time (their records are gone).
    pub delivered: usize,
    /// Refused again (their records stay, with the server's new answer).
    pub refused_again: usize,
    /// No longer deliverable: not in the database any more, or no longer
    /// allowed by the policy or the consent (their records are dropped).
    pub gone: usize,
}

/// Deliver the events the server once refused (`attempt sync status` lists
/// them) a second time, one by one: after the server was upgraded, say. An
/// event the server refuses again stays on the list with the new answer; one
/// it takes is removed from it. Only the newest [`MAX_QUARANTINE_RECORDS`] are
/// remembered, so only those can be retried.
pub fn retry_set_aside(locator: &Locator, peer: &str, cfg: &PeerConfig) -> Result<RetryReport> {
    cfg.check_transport()?;
    let policy = cfg.policy()?;
    let (state, state_path) = SyncState::load_for(&locator.paths.data_dir, &locator.db_dir, peer)?;
    let mut state = state.bound_to(&cfg.url);
    let mut report = RetryReport::default();
    if state.quarantine.is_empty() {
        return Ok(report);
    }
    let want: BTreeMap<u64, EventId> = state
        .quarantine
        .iter()
        .map(|r| (r.source_seq, r.event_id))
        .collect();
    let first = want.keys().next().copied().unwrap_or(1).saturating_sub(1);
    let mut found: BTreeMap<u64, Event> = BTreeMap::new();
    retrying(locator, |db| {
        found.clear();
        scan_events(db, cfg, first, cfg.sends_any_content(), false, &mut |e| {
            if want.get(&e.source_seq) == Some(&e.event_id) {
                found.insert(e.source_seq, e);
            }
            Ok(())
        })
    })?;
    let device_id = open_read_only(locator)?.device_id();
    let capture_mode = if cfg.sends_any_content() {
        CaptureMode::LocalSemantic
    } else {
        CaptureMode::MetadataOnly
    };
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .build();
    let version = health_version(&agent, cfg);
    let consent = cfg.consent.clone();
    let mut kept: Vec<QuarantineRecord> = Vec::new();
    let records = std::mem::take(&mut state.quarantine);
    let mut records = records.into_iter();
    while let Some(mut rec) = records.next() {
        report.tried += 1;
        let deliverable = found.remove(&rec.source_seq).filter(|e| {
            !is_discarded_telemetry(e)
                && policy.allows(e)
                && !consent.as_ref().is_some_and(|c| c.withholds(e))
        });
        let Some(ev) = deliverable else {
            report.gone += 1;
            if rec.action == "content_withheld" {
                state.content_withheld = state.content_withheld.saturating_sub(1);
            }
            continue;
        };
        let (prepared, _) = prepare_for_upload(cfg, ev);
        let body = batch_body(device_id, capture_mode, std::slice::from_ref(&prepared))?;
        match post(&agent, cfg, &body) {
            Ok(ack) => {
                report.delivered += 1;
                state.events += ack.accepted as u64;
                state.duplicates += ack.duplicates as u64;
                state.set_aside_retried += 1;
                if rec.action == "content_withheld" {
                    state.content_withheld = state.content_withheld.saturating_sub(1);
                }
            }
            Err(e) if is_content_rejection(&e) => {
                report.refused_again += 1;
                let (status, reason) = match &e {
                    PostError::TooLarge => (
                        413,
                        "the event alone is larger than the server accepts".to_string(),
                    ),
                    PostError::Rejected { status, message } => (*status, message.clone()),
                    other => (0, other.to_string()),
                };
                rec.status = status;
                rec.reason = reason.chars().take(200).collect();
                rec.at = Timestamp::now();
                rec.server_version = version.clone();
                kept.push(rec);
            }
            Err(e) => {
                // The server (or the network) is the problem, not the event:
                // keep this record and every one not yet tried.
                kept.push(rec);
                kept.extend(records);
                state.quarantine = kept;
                state.save(&state_path)?;
                return Err(anyhow!("{e}"));
            }
        }
    }
    state.quarantine = kept;
    state.save(&state_path)?;
    Ok(report)
}

/// Human-readable summary line.
pub fn describe(report: &UploadReport) -> String {
    if report.pending_before == 0 {
        let mut s = format!("nothing to upload (cursor {})", report.cursor);
        if report.before_consent > 0 {
            s.push_str(&format!(
                "; {} from before you connected kept local",
                report.before_consent
            ));
        }
        if let Some(i) = &report.inferences {
            s.push_str(&describe_inferences(i));
        }
        return s;
    }
    let mut s = format!(
        "uploaded {} event(s) in {} batch(es): {} new, {} duplicate(s)",
        report.pending_before, report.batches, report.accepted, report.duplicates
    );
    if report.rejected > 0 {
        s.push_str(&format!(", {} rejected", report.rejected));
    }
    if report.redactions > 0 {
        s.push_str(&format!(
            ", {} attr(s) redacted by the server",
            report.redactions
        ));
    }
    if report.secrets_redacted > 0 {
        s.push_str(&format!(
            ", {} secret(s) redacted before upload",
            report.secrets_redacted
        ));
    }
    if report.quarantined > 0 {
        s.push_str(&format!(
            ", {} refused by the server and set aside ({} kept their metadata)",
            report.quarantined, report.content_withheld
        ));
    }
    if report.before_consent > 0 {
        s.push_str(&format!(
            ", {} from before you connected kept local",
            report.before_consent
        ));
    }
    s.push_str(&format!("; cursor {}", report.cursor));
    if let Some(i) = &report.inferences {
        s.push_str(&describe_inferences(i));
    }
    s
}

fn describe_inferences(i: &InferenceReport) -> String {
    if let Some(n) = i.skipped_over_events {
        return format!(
            "; inferences not computed: more than {n} events to project on this device (the server derives its own from the events it holds; `inference_max_events` in sync.json raises the limit)"
        );
    }
    if i.unchanged {
        return format!("; inferences unchanged ({} item(s))", i.items);
    }
    let mut s = format!(
        "; inferences: {} item(s) in {} kind(s), {} stored",
        i.items, i.kinds, i.uploaded
    );
    if i.rejected > 0 {
        s.push_str(&format!(", {} rejected", i.rejected));
    }
    if i.truncated > 0 {
        s.push_str(&format!(", {} dropped (per-kind limit)", i.truncated));
    }
    if i.content_removed > 0 {
        s.push_str(&format!(
            ", {} content field(s) removed before upload",
            i.content_removed
        ));
    }
    s
}

/// Resolve what the user typed for `attempt sync connect` / `add`: the
/// [`VIBEMON_ALIAS`] becomes [`VIBEMON_SYNC_URL`] (or the non-empty value of
/// [`VIBEMON_SYNC_URL_ENV`]); anything else is validated as a URL. Plain
/// `http://` is accepted for this machine only.
pub fn resolve_url(input: &str) -> Result<String> {
    resolve_url_opts(input, false)
}

/// [`resolve_url`], with `allow_insecure_http` for a plain `http://` URL to
/// a host that is not this machine (`--allow-insecure-http`).
pub fn resolve_url_opts(input: &str, allow_insecure_http: bool) -> Result<String> {
    let env = std::env::var(VIBEMON_SYNC_URL_ENV).ok();
    resolve_url_with_opts(input, env.as_deref(), allow_insecure_http)
}

/// [`resolve_url`] with the environment override supplied by the caller.
pub fn resolve_url_with(input: &str, env_override: Option<&str>) -> Result<String> {
    resolve_url_with_opts(input, env_override, false)
}

fn resolve_url_with_opts(
    input: &str,
    env_override: Option<&str>,
    allow_insecure_http: bool,
) -> Result<String> {
    if input.trim().eq_ignore_ascii_case(VIBEMON_ALIAS) {
        return match env_override.map(str::trim).filter(|s| !s.is_empty()) {
            Some(url) => validate_url_opts(url, allow_insecure_http)
                .with_context(|| format!("{VIBEMON_SYNC_URL_ENV} is set but not usable")),
            None => Ok(VIBEMON_SYNC_URL.to_string()),
        };
    }
    validate_url_opts(input, allow_insecure_http)
}

/// Validate a URL the user typed for `attempt sync connect`: `https://`, or
/// `http://` for this machine only.
pub fn validate_url(url: &str) -> Result<String> {
    validate_url_opts(url, false)
}

/// The host of an `http(s)` URL, without credentials or port.
fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    if let Some(v6) = host_port.strip_prefix('[') {
        return v6.split(']').next();
    }
    host_port.split(':').next()
}

/// Whether `host` is this machine: `localhost`, `127.0.0.0/8`, `::1`.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.');
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match h.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// [`validate_url`] with the explicit opt-in for plain `http://` to another
/// host. A URL that carries credentials is refused either way: the key
/// belongs in the config, not in an address that ends up in logs.
pub fn validate_url_opts(url: &str, allow_insecure_http: bool) -> Result<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let https = trimmed
        .get(.."https://".len())
        .is_some_and(|p| p.eq_ignore_ascii_case("https://"));
    let http = trimmed
        .get(.."http://".len())
        .is_some_and(|p| p.eq_ignore_ascii_case("http://"));
    if !(https || http) {
        bail!("the sync URL must start with https:// (http:// is accepted for this machine only)");
    }
    let host = url_host(trimmed).unwrap_or("");
    if host.is_empty() {
        bail!("the sync URL has no host");
    }
    let authority = trimmed
        .split_once("://")
        .map(|(_, r)| r.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    if authority.contains('@') {
        bail!("the sync URL carries credentials; give the key with --key or --pair instead");
    }
    if http && !is_loopback_host(host) && !allow_insecure_http {
        bail!(
            "refusing http://{host}: the key and everything uploaded would cross the network in \
             the clear. Use https://, or pass --allow-insecure-http if you accept that for this \
             server (a trusted private network, say)"
        );
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inference(kind: &str, id: &str, evidence: usize, objective: Option<&str>) -> InferenceItem {
        InferenceItem {
            kind: kind.into(),
            id: id.into(),
            session_id: None,
            project_id: None,
            evidence: (0..evidence).map(|_| EventId::new()).collect(),
            confidence: 0.9,
            algorithm_version: "test-v0".into(),
            fields: json!({ "objective": objective, "approach": "edit src/lib.rs" }),
        }
    }

    fn policy(send_content: bool) -> PeerConfig {
        PeerConfig {
            send_content,
            send_inferences: true,
            batch_events: 1,
            interval_secs: 5,
            ..PeerConfig::new("https://x", "k")
        }
    }

    #[test]
    fn inferences_without_evidence_or_of_unknown_kinds_never_leave() {
        let items = vec![
            inference("attempt", "att_b", 2, Some("fix the build")),
            inference("attempt", "att_a", 0, Some("no evidence")),
            inference("causal_edge", "edge_1", 3, None),
            inference("decision", "dec_1", 1, None),
        ];
        let (kept, removed) = prepare_inferences(&policy(false), items);
        let ids: Vec<&str> = kept.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            ["att_b", "dec_1"],
            "sorted by (kind, id); dropped without evidence or of an unsynced kind"
        );
        assert_eq!(removed, 1, "one objective held text");
        assert!(kept[0].fields["objective"].is_null());
        assert_eq!(kept[0].fields["approach"], json!("edit src/lib.rs"));
    }

    #[test]
    fn with_content_opt_in_objectives_travel_but_secrets_do_not() {
        let items = vec![inference(
            "attempt",
            "att_1",
            1,
            Some("use token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef0123 please"),
        )];
        let (kept, removed) = prepare_inferences(&policy(true), items);
        assert_eq!(removed, 0);
        let objective = kept[0].fields["objective"].as_str().unwrap();
        assert!(objective.contains("[REDACTED:github_token]"), "{objective}");
        assert!(!objective.contains("ghp_"));
    }

    #[test]
    fn inference_digest_is_stable_across_order_and_changes_with_content() {
        let a = inference("attempt", "att_1", 1, None);
        let b = inference("attempt", "att_2", 1, None);
        let (one, _) = prepare_inferences(&policy(false), vec![a.clone(), b.clone()]);
        let (two, _) = prepare_inferences(&policy(false), vec![b.clone(), a.clone()]);
        assert_eq!(
            inference_digest("v", &one).unwrap(),
            inference_digest("v", &two).unwrap()
        );
        assert_ne!(
            inference_digest("v", &one).unwrap(),
            inference_digest("v2", &one).unwrap()
        );
        let mut changed = a.clone();
        changed.confidence = 0.5;
        let (three, _) = prepare_inferences(&policy(false), vec![changed, b]);
        assert_ne!(
            inference_digest("v", &one).unwrap(),
            inference_digest("v", &three).unwrap()
        );
    }

    #[test]
    fn describe_mentions_inferences() {
        let mut r = UploadReport::default();
        assert!(!describe(&r).contains("inferences"));
        r.inferences = Some(InferenceReport {
            items: 3,
            kinds: 2,
            uploaded: 2,
            rejected: 1,
            truncated: 0,
            unchanged: false,
            content_removed: 3,
            skipped_over_events: None,
        });
        let s = describe(&r);
        assert!(
            s.contains("3 item(s) in 2 kind(s), 2 stored, 1 rejected, 3 content field(s) removed"),
            "{s}"
        );
        r.inferences = Some(InferenceReport {
            unchanged: true,
            items: 3,
            ..Default::default()
        });
        assert!(describe(&r).contains("inferences unchanged (3 item(s))"));
    }

    // -- profiles -----------------------------------------------------------

    fn spoken(kind: EventKind, name: &str, source_otel: bool) -> Event {
        let device = attemptdb_core::DeviceId::new();
        let mut e = Event::new(
            device,
            attemptdb_core::event::Provider::ClaudeCode,
            name,
            kind,
            attemptdb_core::ProjectRef::derive("/home/dev/example", None, &device),
            "fixture-session",
            CaptureMode::LocalSemantic,
            "test",
        );
        if source_otel {
            e.attrs.insert("source".into(), json!("otel"));
        }
        let mut c = attemptdb_core::event::EventContent {
            prompt: Some("make the retries idempotent".into()),
            message: Some("I will read the webhook handler first.".into()),
            command: Some("npm test -- CANARY_COMMAND".into()),
            error: Some("CANARY_ERROR".into()),
            tool_input: Some(json!({"command":"CANARY_INPUT"})),
            tool_output: Some(json!("CANARY_OUTPUT")),
            ..Default::default()
        };
        c.extra
            .insert("elicitation_content".into(), json!("CANARY_EXTRA"));
        e.content = Some(c);
        e.raw = Some(json!({"prompt":"CANARY_RAW"}));
        e
    }

    #[test]
    fn messages_profile_keeps_only_what_was_said() {
        // A hook prompt, a turn stop, an agent message and an OTel reply keep
        // their text; everything else content-bearing is gone, and so is raw.
        for (kind, name, otel) in [
            (EventKind::PromptSubmitted, "UserPromptSubmit", false),
            (EventKind::TurnStopped, "Stop", false),
            (EventKind::AgentMessage, "transcript", false),
            (EventKind::Unknown, "assistant_response", true),
            (EventKind::Unknown, "user_prompt", true),
        ] {
            let mut e = spoken(kind, name, otel);
            assert!(keep_messages_only(&mut e), "{name}");
            let text = serde_json::to_string(&e).unwrap();
            assert!(text.contains("make the retries idempotent"), "{name}");
            assert!(
                text.contains("I will read the webhook handler first."),
                "{name}"
            );
            assert!(!text.contains("CANARY"), "{name}: {text}");
            assert_eq!(e.capture_mode, CaptureMode::LocalSemantic);
            assert!(e.raw.is_none());
        }
        // A tool call is not a message: nothing content-bearing leaves.
        let mut e = spoken(EventKind::ToolCallFinished, "PostToolUse", false);
        assert!(!keep_messages_only(&mut e));
        assert!(e.content.is_none() && e.raw.is_none());
        assert_eq!(e.capture_mode, CaptureMode::MetadataOnly);
        // An OTel record that is not a prompt or reply is metadata only.
        let mut e = spoken(EventKind::Unknown, "api_request", true);
        assert!(!keep_messages_only(&mut e));
        assert!(e.content.is_none());
        // A message event whose text was redacted upstream carries nothing.
        let mut e = spoken(EventKind::Unknown, "assistant_response", true);
        e.content.as_mut().unwrap().message = None;
        e.content.as_mut().unwrap().prompt = None;
        assert!(!keep_messages_only(&mut e));
        assert!(e.content.is_none());
    }

    #[test]
    fn profile_flag_table() {
        // (send_content, send_inferences, send_messages) → profile.
        let table = [
            (false, false, false, SyncProfile::MetadataOnly),
            (false, true, false, SyncProfile::Semantic),
            (false, true, true, SyncProfile::Messages),
            // Messages without inferences have no name; the conversation is
            // the stronger signal, so it reports `messages`.
            (false, false, true, SyncProfile::Messages),
            (true, true, true, SyncProfile::Full),
            // Content without inferences or messages has no name; content is
            // the stronger signal, so it reports `full`.
            (true, false, false, SyncProfile::Full),
            (true, true, false, SyncProfile::Full),
        ];
        for (content, inferences, messages, expected) in table {
            assert_eq!(
                SyncProfile::from_flags(content, inferences, messages),
                expected,
                "({content}, {inferences}, {messages})"
            );
            let mut peer = PeerConfig::new("https://x", "k");
            peer.send_content = content;
            peer.send_inferences = inferences;
            peer.send_messages = messages;
            assert_eq!(peer.profile(), expected);
        }
        // Named profiles round-trip through their flags.
        for p in SyncProfile::ALL {
            let (c, i, m) = p.flags();
            assert_eq!(SyncProfile::from_flags(c, i, m), p);
            let mut peer = PeerConfig::new("https://x", "k");
            peer.set_profile(p);
            assert_eq!(peer.profile(), p);
            assert_eq!(p.as_str().parse::<SyncProfile>().unwrap(), p);
            assert_eq!(serde_json::to_value(p).unwrap(), json!(p.as_str()));
            assert_eq!(
                serde_json::from_value::<SyncProfile>(json!(p.as_str())).unwrap(),
                p
            );
        }
        assert_eq!(
            "metadata-only".parse::<SyncProfile>().unwrap(),
            SyncProfile::MetadataOnly
        );
        assert!("everything".parse::<SyncProfile>().is_err());
        assert_eq!(
            format!("{:<9}|", SyncProfile::Full),
            "full     |",
            "pads in tables"
        );
    }

    #[test]
    fn profile_resolution_with_explicit_overrides() {
        // No profile given: `semantic` (the 2026-08-31 decision) —
        // inferences travel, content and messages do not.
        assert_eq!(
            SyncProfile::resolve(None, false, false, false),
            (false, true, false)
        );
        assert_eq!(
            SyncProfile::resolve(None, true, false, false),
            (true, true, false)
        );
        assert_eq!(
            SyncProfile::resolve(None, false, true, false),
            (false, true, false)
        );
        assert_eq!(
            SyncProfile::resolve(None, false, false, true),
            (false, true, true)
        );
        assert_eq!(
            SyncProfile::resolve(Some(SyncProfile::MetadataOnly), false, false, false),
            (false, false, false)
        );
        assert_eq!(
            SyncProfile::resolve(Some(SyncProfile::Semantic), false, false, false),
            (false, true, false)
        );
        assert_eq!(
            SyncProfile::resolve(Some(SyncProfile::Semantic), true, false, false),
            (true, true, false),
            "--send-content on top of semantic"
        );
        assert_eq!(
            SyncProfile::resolve(Some(SyncProfile::Semantic), false, false, true),
            (false, true, true),
            "--send-messages on top of semantic is the messages profile"
        );
        assert_eq!(
            SyncProfile::resolve(Some(SyncProfile::Messages), false, false, false),
            (false, true, true)
        );
        assert_eq!(
            SyncProfile::resolve(Some(SyncProfile::Full), false, false, false),
            (true, true, true)
        );
        // The switches only ever add: a profile cannot be narrowed by them.
        assert_eq!(
            SyncProfile::resolve(Some(SyncProfile::Full), false, false, false),
            SyncProfile::Full.flags()
        );
    }

    // -- peers --------------------------------------------------------------

    #[test]
    fn peer_names() {
        for ok in ["default", "work", "a", "team.eu-1_x", &"n".repeat(32)] {
            assert_eq!(validate_peer_name(ok).unwrap(), ok, "{ok}");
        }
        assert_eq!(validate_peer_name("  work ").unwrap(), "work");
        for bad in ["", "  ", "a/b", "a b", "ünïcode", &"n".repeat(33), "x:y"] {
            assert!(validate_peer_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn config_round_trips_and_is_private() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(SyncConfig::load(tmp.path()).unwrap().is_none());
        let peer = PeerConfig {
            batch_events: 10,
            interval_secs: 5,
            ..PeerConfig::new("https://sync.example.test", "k-0123456789")
        };
        let cfg = SyncConfig::single(peer.clone());
        cfg.save(tmp.path()).unwrap();
        assert_eq!(SyncConfig::load(tmp.path()).unwrap(), Some(cfg.clone()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(SyncConfig::path(tmp.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(peer.masked_key(), "k-01…6789");
        assert_eq!(peer.endpoint(), "https://sync.example.test/v1/sync");
        assert_eq!(peer.profile(), SyncProfile::MetadataOnly);
        assert!(SyncConfig::remove(tmp.path()).unwrap());
        assert!(!SyncConfig::remove(tmp.path()).unwrap());

        // Saving an empty configuration leaves no file behind.
        cfg.save(tmp.path()).unwrap();
        SyncConfig::default().save(tmp.path()).unwrap();
        assert!(SyncConfig::load(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn single_server_file_loads_as_peer_default_and_is_rewritten_with_peers() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let old = json!({
            "url": "https://sync.example.test",
            "key": "k-0123456789",
            "send_content": true,
            "interval_secs": 45,
            "exclude": ["github.com/acme/private"]
        });
        std::fs::write(SyncConfig::path(tmp.path()), old.to_string()).unwrap();
        let cfg = SyncConfig::load(tmp.path()).unwrap().unwrap();
        assert_eq!(cfg.names_list(), "default");
        let peer = cfg.get(DEFAULT_PEER).unwrap();
        assert_eq!(peer.url, "https://sync.example.test");
        assert!(peer.send_content && !peer.send_inferences);
        assert_eq!(peer.profile(), SyncProfile::Full);
        assert_eq!(peer.interval_secs, 45);
        assert_eq!(
            peer.batch_events, DEFAULT_BATCH_EVENTS,
            "defaults still fill in"
        );
        assert_eq!(peer.exclude, ["github.com/acme/private"]);

        // The next save writes the peers shape, and it reads back the same.
        cfg.save(tmp.path()).unwrap();
        let text = std::fs::read_to_string(SyncConfig::path(tmp.path())).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert!(v.get("url").is_none());
        assert_eq!(
            v["peers"]["default"]["url"],
            json!("https://sync.example.test")
        );
        assert_eq!(SyncConfig::load(tmp.path()).unwrap().unwrap(), cfg);

        // A second peer sits beside it.
        let mut two = cfg.clone();
        two.peers.insert(
            "team".into(),
            PeerConfig::new("https://team.example.test", "k2-00000000"),
        );
        two.save(tmp.path()).unwrap();
        assert_eq!(
            SyncConfig::load(tmp.path()).unwrap().unwrap().names_list(),
            "default, team"
        );

        // Neither shape: a clear error naming the file.
        std::fs::write(SyncConfig::path(tmp.path()), "{}").unwrap();
        let err = SyncConfig::load(tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("sync.json"), "{err:#}");
        // An invalid peer name in a hand-edited file is refused.
        std::fs::write(
            SyncConfig::path(tmp.path()),
            json!({"peers": {"a/b": {"url": "https://x", "key": "k"}}}).to_string(),
        )
        .unwrap();
        assert!(SyncConfig::load(tmp.path()).is_err());
    }

    #[test]
    fn peer_set_changes_are_detected() {
        let a = PeerConfig::new("https://a", "k");
        let b = PeerConfig::new("https://b", "k");
        let mut before = SyncConfig::default();
        before.peers.insert("default".into(), a.clone());
        before.peers.insert("gone".into(), b.clone());
        let mut after = SyncConfig::default();
        let mut a2 = a.clone();
        a2.set_profile(SyncProfile::Semantic);
        after.peers.insert("default".into(), a2);
        after.peers.insert("new".into(), b);

        let change = peer_set_diff(&before, &after);
        assert_eq!(change.added, ["new"]);
        assert_eq!(change.removed, ["gone"]);
        assert_eq!(change.changed, ["default"]);
        assert!(!change.is_empty());
        assert!(peer_set_diff(&after, &after).is_empty());
        // From nothing: everything is an addition (what the daemon logs at
        // start).
        let first = peer_set_diff(&SyncConfig::default(), &after);
        assert_eq!(first.added, ["default", "new"]);
        assert!(first.removed.is_empty() && first.changed.is_empty());
        // To nothing (file removed): everything is a removal.
        let last = peer_set_diff(&after, &SyncConfig::default());
        assert_eq!(last.removed, ["default", "new"]);
    }

    #[test]
    fn peers_are_due_on_their_own_intervals() {
        let fast = PeerConfig {
            interval_secs: 5,
            ..PeerConfig::new("https://fast", "k")
        };
        let slow = PeerConfig {
            interval_secs: 30,
            ..PeerConfig::new("https://slow", "k")
        };
        let mut cfg = SyncConfig::default();
        cfg.peers.insert("fast".into(), fast);
        cfg.peers.insert("slow".into(), slow);
        let mut schedule = PeerSchedule::default();
        let t0 = Instant::now();

        // First sight: nothing is due; the first upload comes one interval
        // later. The next tick is the smallest interval.
        assert!(schedule.due(&cfg, t0).is_empty());
        assert_eq!(schedule.next_sleep(&cfg, t0), Duration::from_secs(5));

        let t5 = t0 + Duration::from_secs(5);
        assert_eq!(schedule.due(&cfg, t5), ["fast"]);
        schedule.mark("fast", t5);
        assert_eq!(schedule.next_sleep(&cfg, t5), Duration::from_secs(5));

        // At 30 s both are due; an attempt that took a while pushes the next
        // wake-up, never below one second.
        let t30 = t0 + Duration::from_secs(30);
        assert_eq!(schedule.due(&cfg, t30), ["fast", "slow"]);
        schedule.mark("fast", t30);
        schedule.mark("slow", t30);
        let t34 = t30 + Duration::from_millis(4_600);
        assert_eq!(schedule.next_sleep(&cfg, t34), Duration::from_secs(1));

        // A peer that disappears from the file is forgotten; one that
        // appears starts its own clock.
        cfg.peers.remove("fast");
        cfg.peers.insert(
            "late".into(),
            PeerConfig {
                interval_secs: 30,
                ..PeerConfig::new("https://late", "k")
            },
        );
        let t35 = t0 + Duration::from_secs(35);
        assert!(schedule.due(&cfg, t35).is_empty());
        assert!(!schedule.last_attempt.contains_key("fast"));
        assert!(schedule.last_attempt.contains_key("late"));
        assert_eq!(schedule.next_sleep(&cfg, t35), Duration::from_secs(25));

        // No peers at all: poll for a configuration.
        let none = SyncConfig::default();
        assert!(schedule.due(&none, t35).is_empty());
        assert_eq!(schedule.next_sleep(&none, t35), CONFIG_POLL);
    }

    #[test]
    fn repository_policy() {
        use attemptdb_core::event::Provider;
        use attemptdb_core::{DeviceId, EventKind, ProjectRef};
        let d = DeviceId::derive(&["t", "d"]);
        let mk = |remote: Option<&str>| {
            Event::new(
                d,
                Provider::ClaudeCode,
                "x",
                EventKind::Unknown,
                ProjectRef::derive("/home/dev/p", remote, &d),
                "s",
                CaptureMode::MetadataOnly,
                "t/0",
            )
        };
        let public = mk(Some("github.com/acme/public"));
        let private = mk(Some("github.com/acme/private"));
        let local = mk(None);
        let mut cfg = PeerConfig {
            batch_events: 1,
            interval_secs: 5,
            ..PeerConfig::new("https://x", "k")
        };
        assert!(cfg.allows(&public) && cfg.allows(&private) && cfg.allows(&local));
        cfg.exclude = vec!["GitHub.com/acme/private".into()];
        assert!(cfg.allows(&public) && !cfg.allows(&private) && cfg.allows(&local));
        cfg.include = vec!["github.com/acme/public".into()];
        assert!(cfg.allows(&public) && !cfg.allows(&private) && !cfg.allows(&local));
        cfg.include = vec![format!("prj_{}", local.project.project_id)];
        assert!(!cfg.allows(&public) && cfg.allows(&local));
    }

    #[test]
    fn url_validation() {
        assert_eq!(validate_url(" https://a.b/ ").unwrap(), "https://a.b");
        assert!(validate_url("a.b").is_err());
        assert!(validate_url("https://").is_err());
    }

    #[test]
    fn vibemon_alias_resolves_to_the_hosted_url_unless_overridden() {
        assert_eq!(resolve_url_with("vibemon", None).unwrap(), VIBEMON_SYNC_URL);
        assert_eq!(
            resolve_url_with(" VibeMon ", None).unwrap(),
            VIBEMON_SYNC_URL
        );
        // An empty override is no override.
        assert_eq!(
            resolve_url_with("vibemon", Some("  ")).unwrap(),
            VIBEMON_SYNC_URL
        );
        assert_eq!(
            resolve_url_with("vibemon", Some("http://127.0.0.1:8797/")).unwrap(),
            "http://127.0.0.1:8797"
        );
        let err = resolve_url_with("vibemon", Some("not a url")).unwrap_err();
        assert!(format!("{err:#}").contains(VIBEMON_SYNC_URL_ENV), "{err:#}");
        // Anything else is plain URL validation; the override never applies.
        assert_eq!(
            resolve_url_with("https://sync.example.test/", Some("http://x")).unwrap(),
            "https://sync.example.test"
        );
        assert!(resolve_url_with("vibemon.dev", None).is_err());
    }

    #[test]
    fn state_is_per_database_and_per_peer() {
        let a = SyncState::path(Path::new("/d"), Path::new("/x/.attemptdb"), "default");
        let b = SyncState::path(Path::new("/d"), Path::new("/y/.attemptdb"), "default");
        let c = SyncState::path(Path::new("/d"), Path::new("/x/.attemptdb"), "team");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("/d/sync"));
        assert!(a.to_string_lossy().ends_with(".default.json"));
        assert!(c.to_string_lossy().ends_with(".team.json"));
        let legacy = SyncState::legacy_path(Path::new("/d"), Path::new("/x/.attemptdb"));
        assert_eq!(
            legacy.parent(),
            a.parent(),
            "same directory, one fewer name component"
        );
        assert_ne!(legacy, a);
    }

    #[test]
    fn single_server_cursor_is_peer_defaults_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, db) = (tmp.path().join("data"), tmp.path().join("db"));
        let legacy = SyncState::legacy_path(&data, &db);
        SyncState {
            last_acked_source_seq: 4_473,
            batches: 5,
            ..Default::default()
        }
        .save(&legacy)
        .unwrap();

        // Read through the old name, write to the new one.
        let (state, path) = SyncState::load_for(&data, &db, DEFAULT_PEER).unwrap();
        assert_eq!(state.last_acked_source_seq, 4_473);
        assert_eq!(path, SyncState::path(&data, &db, DEFAULT_PEER));
        let mut advanced = state.clone();
        advanced.last_acked_source_seq = 4_500;
        advanced.save(&path).unwrap();
        // From now on the per-peer file wins, even if the old one lingers.
        let (again, _) = SyncState::load_for(&data, &db, DEFAULT_PEER).unwrap();
        assert_eq!(again.last_acked_source_seq, 4_500);
        assert_eq!(
            SyncState::load(&legacy).unwrap().last_acked_source_seq,
            4_473,
            "the old file is left alone"
        );
        // Another peer never inherits it.
        let (team, team_path) = SyncState::load_for(&data, &db, "team").unwrap();
        assert_eq!(team.last_acked_source_seq, 0);
        assert!(!team_path.exists());
    }
}

#[cfg(test)]
mod cursor_binding {
    use super::SyncState;

    #[test]
    fn a_cursor_follows_its_server_not_its_peer_name() {
        let advanced = SyncState {
            last_acked_source_seq: 4_473,
            batches: 5,
            events: 4_473,
            url: Some("https://a.example".into()),
            ..Default::default()
        };
        let same = advanced.clone().bound_to("https://a.example");
        assert_eq!(same.last_acked_source_seq, 4_473);
        let moved = advanced.clone().bound_to("https://b.example");
        assert_eq!(
            moved.last_acked_source_seq, 0,
            "a different server starts over"
        );
        assert_eq!(moved.batches, 0);
        assert_eq!(moved.url.as_deref(), Some("https://b.example"));
        // Files written before URL tracking keep their cursor.
        let legacy = SyncState {
            last_acked_source_seq: 12,
            ..Default::default()
        };
        let bound = legacy.bound_to("https://a.example");
        assert_eq!(bound.last_acked_source_seq, 12);
        assert_eq!(bound.url.as_deref(), Some("https://a.example"));
    }
}

#[cfg(test)]
mod budget_tests {
    use super::newest_within_budget;

    #[test]
    fn the_newest_items_that_fit_are_kept() {
        // Sizes are oldest first; each item costs its size plus a separator.
        assert_eq!(newest_within_budget(&[], 100), 0);
        assert_eq!(newest_within_budget(&[10, 10, 10], 100), 3);
        assert_eq!(newest_within_budget(&[10, 10, 10], 21), 1);
        // An old, large item does not displace the newer small ones.
        assert_eq!(newest_within_budget(&[500, 10, 10, 10], 35), 3);
        // One item larger than the whole budget leaves nothing to send.
        assert_eq!(newest_within_budget(&[200], 100), 0);
    }
}

#[cfg(test)]
mod privacy_tests {
    use super::*;
    use attemptdb_core::event::{EventContent, Provider};
    use attemptdb_core::{DeviceId, ProjectRef};

    fn device() -> DeviceId {
        DeviceId::derive(&["privacy-test"])
    }

    fn event(root: &str, remote: Option<&str>) -> Event {
        let d = device();
        Event::new(
            d,
            Provider::ClaudeCode,
            "PostToolUse",
            EventKind::ToolCallFinished,
            ProjectRef::derive(root, remote, &d),
            "s",
            CaptureMode::LocalSemantic,
            "t",
        )
    }

    /// An OTel observation as the receiver stores it before (or without) a
    /// hook to attribute it to.
    fn otel(attributed: Option<bool>) -> Event {
        let d = device();
        let mut e = Event::new(
            d,
            Provider::Codex,
            "codex.user_prompt",
            EventKind::Unknown,
            ProjectRef::derive("otel/unattributed", None, &d),
            "otel-unattributed-codex",
            CaptureMode::LocalSemantic,
            "otel-json-v1",
        );
        e.attrs.insert("source".into(), json!("otel"));
        if let Some(a) = attributed {
            e.attrs.insert("x_otel_project_attributed".into(), json!(a));
        }
        e
    }

    fn peer(include: &[&str], exclude: &[&str]) -> PeerConfig {
        PeerConfig {
            include: include.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
            ..PeerConfig::new("https://x", "k")
        }
    }

    #[test]
    fn every_spelling_of_a_remote_is_one_entry() {
        let canonical = Some(PolicyKey::Remote("github.com/acme/private".into()));
        for spelling in [
            "github.com/acme/private",
            "GitHub.com/Acme/Private",
            "github.com/acme/private/",
            "github.com/acme/private.git",
            "https://github.com/acme/private",
            "https://github.com/acme/private.git",
            "https://github.com/acme/private.git/",
            "HTTPS://GITHUB.COM/ACME/PRIVATE.GIT",
            "http://github.com/acme/private",
            "git@github.com:acme/private",
            "git@github.com:acme/private.git",
            "ssh://git@github.com/acme/private.git",
            "git://github.com/acme/private.git",
            "https://user:token@github.com/acme/private.git",
            "  https://github.com/acme/private.git  ",
        ] {
            assert_eq!(parse_policy_entry(spelling), canonical, "{spelling:?}");
        }
        let id = ProjectId::derive(&["x"]);
        for spelling in [
            format!("prj_{id}"),
            id.to_string(),
            format!("prj_{}", id.to_string().to_uppercase()),
            format!("  {id}  "),
        ] {
            assert_eq!(
                parse_policy_entry(&spelling),
                Some(PolicyKey::Project(id)),
                "{spelling:?}"
            );
        }
        assert_eq!(
            parse_policy_entry(&format!("prj_{id}"))
                .unwrap()
                .canonical(),
            format!("prj_{id}")
        );
        // Neither a project nor a remote: no entry, so nothing is stored for it.
        for bad in [
            "",
            "  ",
            "acme/private",
            "private",
            "prj_not-a-uuid",
            "github.com/acme",
        ] {
            assert_eq!(parse_policy_entry(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn exclude_matches_an_event_whatever_spelling_either_side_used() {
        // The event's stored remote is whatever an adapter wrote — here a URL,
        // not the normalised form `ProjectRef::derive` would have produced.
        let mut private = event("/home/dev/private", None);
        private.project.repo_remote = Some("https://GitHub.com/Acme/Private.git".into());
        let public = event("/home/dev/public", Some("github.com/acme/public"));
        for entry in [
            "github.com/acme/private",
            "https://github.com/acme/private.git",
            "git@github.com:acme/private",
            "GitHub.com/Acme/Private/",
        ] {
            let c = peer(&[], &[entry]);
            assert!(
                !c.allows(&private),
                "{entry} must exclude the private repository"
            );
            assert!(c.allows(&public), "{entry} must not exclude the public one");
        }
        // A project id, in either spelling.
        let c = peer(&[], &[&format!("PRJ_{}", private.project.project_id)]);
        // `PRJ_` is not the prefix; the bare uuid still parses only when the
        // prefix is right, so this entry is unreadable and nothing uploads.
        assert!(c.policy().is_err());
        let c = peer(&[], &[&private.project.project_id.to_string()]);
        assert!(!c.allows(&private) && c.allows(&public));
        // include, same.
        let c = peer(&["git@github.com:acme/public.git"], &[]);
        assert!(c.allows(&public) && !c.allows(&private));
    }

    /// The remote an event carries is whatever `git remote get-url origin`
    /// said when the project was first seen, normalised for identity
    /// (`ProjectRef::derive`): ports become path segments and an ssh alias
    /// stays the host. A person writes the entry from the web page, the
    /// clone URL or the ssh config. Every entry spelling must meet every
    /// remote spelling of the same repository — in both directions: an
    /// `exclude` that misses leaks a private repository's metadata, and an
    /// `include` that misses uploads nothing.
    #[test]
    fn every_entry_spelling_meets_every_remote_spelling_of_one_repository() {
        let remotes = [
            "https://github.com/acme/private.git",
            "git@github.com:acme/private.git",
            "ssh://git@github.com/acme/private.git",
            "ssh://git@github.com:22/acme/private.git",
            "ssh://git@ssh.github.com:443/acme/private.git",
            "git@ssh.github.com:acme/private.git",
            "https://github.com:443/acme/private",
            "git://github.com/acme/private",
            "https://x-access-token:tok@github.com/acme/private.git",
        ];
        let entries = [
            "github.com/acme/private",
            "GitHub.com/Acme/Private",
            "https://github.com/acme/private",
            "https://github.com/acme/private.git",
            "git@github.com:acme/private.git",
            "ssh://git@ssh.github.com:443/acme/private.git",
            "ssh://git@github.com:22/acme/private.git",
            "https://github.com/acme/private/tree/main",
            "https://github.com/acme/private/tree/main/src",
            "https://github.com/acme/private/blob/main/README.md",
            "https://github.com/acme/private/issues/3",
            "https://github.com/acme/private/pull/9",
            "https://github.com/acme/private?tab=readme-ov-file",
            "https://github.com/acme/private#readme",
            "https://github.com/acme/private/",
        ];
        let public = event("/home/dev/public", Some("git@github.com:acme/public.git"));
        for remote in remotes {
            let private = event("/home/dev/private", Some(remote));
            for entry in entries {
                let excl = peer(&[], &[entry]);
                assert!(
                    !excl.allows(&private),
                    "exclude `{entry}` must hold back a repository cloned from `{remote}` \
                     (stored as {:?})",
                    private.project.repo_remote
                );
                assert!(excl.allows(&public), "exclude `{entry}` spares the others");
                let incl = peer(&[entry], &[]);
                assert!(
                    incl.allows(&private),
                    "include `{entry}` must let a repository cloned from `{remote}` through"
                );
                assert!(!incl.allows(&public), "include `{entry}` is only that one");
            }
        }
    }

    /// An ssh host alias (`Host github-work` in ~/.ssh/config) cannot be
    /// resolved without that file: an entry written with the same alias
    /// matches; one written with the real host does not — and
    /// `entry_matches_seen` / `nearest_projects` are how a person finds out.
    #[test]
    fn an_ssh_alias_matches_only_itself_and_the_nearest_projects_say_so() {
        let aliased = event(
            "/home/dev/private",
            Some("git@github-work:acme/private.git"),
        );
        let plain = event("/home/dev/other", Some("git@github.com:acme/other.git"));
        let same_alias = peer(&[], &["git@github-work:acme/private.git"]);
        assert!(!same_alias.allows(&aliased));
        assert!(same_alias.allows(&plain));
        let real_host = peer(&[], &["https://github.com/acme/private"]);
        assert!(
            real_host.allows(&aliased),
            "the real host name cannot know the alias; that is what the warning is for"
        );
        let seen: Vec<SeenProject> = [&aliased, &plain]
            .iter()
            .map(|e| SeenProject {
                project_id: e.project.project_id,
                name: e.project.name.clone(),
                remote: e.project.repo_remote.as_deref().and_then(repo_key),
                events: 3,
            })
            .collect();
        let entry = parse_policy_entry("https://github.com/acme/private").unwrap();
        assert!(!entry_matches_seen(&entry, &seen));
        let near = nearest_projects(&entry, &seen);
        assert_eq!(
            near.first().and_then(|p| p.remote.as_deref()),
            Some("github-work/acme/private"),
            "the same owner/repo under the alias comes first: {near:?}"
        );
        let alias_entry = parse_policy_entry("git@github-work:acme/private.git").unwrap();
        assert!(entry_matches_seen(&alias_entry, &seen));
        // A project id is matched by id.
        let by_id = PolicyKey::Project(aliased.project.project_id);
        assert!(entry_matches_seen(&by_id, &seen));
        assert!(!entry_matches_seen(
            &PolicyKey::Project(ProjectId::derive(&["nobody"])),
            &seen
        ));
        // A typo is one edit from the real thing.
        let typo = parse_policy_entry("github.com/acme/othre").unwrap();
        assert!(!entry_matches_seen(&typo, &seen));
        assert_eq!(
            nearest_projects(&typo, &seen)
                .first()
                .and_then(|p| p.remote.as_deref()),
            Some("github.com/acme/other")
        );
        // Nothing near: nothing offered.
        let far = parse_policy_entry("gitlab.com/zzz/qqq").unwrap();
        assert!(nearest_projects(&far, &seen).is_empty());
    }

    /// Every home-directory spelling that reached the server in the 2026-10
    /// review, in every path-shaped field of the upload.
    #[test]
    fn no_home_directory_spelling_survives_the_upload_scrub() {
        for home in [
            "/mnt/c/Users/alice",
            "/var/home/alice",
            "/Volumes/Data/Users/alice",
            "/System/Volumes/Data/Users/alice",
            "/usr/home/alice",
            "/home/alice",
            "/Users/alice",
            "C:/Users/alice",
            "//wsl$/Ubuntu/home/alice",
        ] {
            let root = format!("{home}/work/repo");
            let mut e = event(&root, None);
            e.paths = vec![
                PortablePath::from_raw(&format!("{root}/src/main.rs"), Some(&root)),
                PortablePath::from_raw(&format!("{home}/.config/tool/settings.json"), Some(&root)),
            ];
            let n = scrub_paths(&mut e);
            let text = serde_json::to_string(&e).unwrap();
            assert!(n > 0, "{home}");
            assert!(!text.contains("alice"), "{home}: {text}");
            assert_eq!(e.project.root, "~/work/repo", "{home}");
            assert_eq!(e.paths[0].logical, "src/main.rs");
            assert_eq!(e.paths[1].logical, "~/.config/tool/settings.json", "{home}");
        }
        // A project rooted at the home directory itself is named for the
        // account: root and name both go.
        let mut e = event("/Volumes/Data/Users/alice", None);
        assert_eq!(e.project.name, "alice");
        scrub_paths(&mut e);
        assert_eq!(
            (e.project.root.as_str(), e.project.name.as_str()),
            ("~", "~")
        );
        // A remote that is a local path carries the home directory too.
        let mut e = event("/srv/x", None);
        e.project.repo_remote = Some("/var/home/alice/git/x".into());
        scrub_paths(&mut e);
        assert_eq!(e.project.repo_remote.as_deref(), Some("~/git/x"));
        // Nothing is invented where there is no home.
        let mut e = event("/opt/build/repo", Some("github.com/acme/repo"));
        e.paths = vec![PortablePath::from_raw(
            "/etc/hosts",
            Some("/opt/build/repo"),
        )];
        assert_eq!(scrub_paths(&mut e), 0, "nothing to hide, nothing changed");
        assert_eq!(e.project.root, "/opt/build/repo");
        assert_eq!(e.paths[0].logical, "/etc/hosts");
    }

    #[test]
    fn the_inference_recompute_waits_for_its_interval_unless_it_is_owed() {
        let mut c = PeerConfig {
            send_inferences: true,
            inference_interval_secs: 600,
            ..PeerConfig::new("https://x", "k")
        };
        let now = Timestamp::now();
        let minutes_ago = |m: i64| Timestamp::from_micros(now.as_micros() - m * 60_000_000);
        let mut st = SyncState::default();
        // Never uploaded: now, whatever else.
        assert!(inference_due(&c, &st, false, false, false, now));
        st.last_inference_at = Some(minutes_ago(30));
        st.inference_computed_at = Some(minutes_ago(1));
        // Computed a minute ago: new events wait; nothing new never recomputes.
        assert!(!inference_due(&c, &st, true, false, false, now));
        assert!(!inference_due(&c, &st, false, false, false, now));
        // Asked for, or a failed upload to retry: no waiting.
        assert!(inference_due(&c, &st, false, false, true, now));
        assert!(inference_due(&c, &st, false, true, false, now));
        // Ten minutes on, with events since (this tick's or an earlier one's).
        st.inference_computed_at = Some(minutes_ago(11));
        assert!(inference_due(&c, &st, true, false, false, now));
        st.inference_dirty = true;
        assert!(inference_due(&c, &st, false, false, false, now));
        st.inference_dirty = false;
        assert!(!inference_due(&c, &st, false, false, false, now));
        // Interval 0: every upload with new events.
        c.inference_interval_secs = 0;
        st.inference_computed_at = Some(minutes_ago(0));
        assert!(inference_due(&c, &st, true, false, false, now));
        // Off: never.
        c.send_inferences = false;
        assert!(!inference_due(&c, &st, true, true, true, now));
    }

    #[test]
    fn a_scan_restarts_on_a_fresh_open_when_a_segment_vanishes() {
        let tmp = tempfile::tempdir().unwrap();
        let locator = Locator::resolve(tmp.path(), Some(tmp.path()), None);
        crate::ingest::open_writer(&locator, true).unwrap();
        let mut calls = 0;
        let out = retrying(&locator, |_| {
            calls += 1;
            if calls < 3 {
                Err(Vanished("seg".into()).into())
            } else {
                Ok(calls)
            }
        })
        .unwrap();
        assert_eq!(out, 3);
        // A persistent failure is not retried for ever.
        let mut calls = 0;
        let err = retrying(&locator, |_| -> Result<()> {
            calls += 1;
            Err(Vanished("seg".into()).into())
        })
        .unwrap_err();
        assert!(err.downcast_ref::<Vanished>().is_some());
        assert_eq!(calls, MAX_REOPENS + 1);
        // Anything else is not retried at all.
        let mut calls = 0;
        let _ = retrying(&locator, |_| -> Result<()> {
            calls += 1;
            bail!("something else")
        });
        assert_eq!(calls, 1);
    }

    #[test]
    fn an_entry_that_names_nothing_stops_uploads_instead_of_excluding_nothing() {
        let c = peer(&[], &["acme/private"]);
        let err = c.policy().unwrap_err().to_string();
        assert!(
            err.contains("`acme/private`") && err.contains("exclude"),
            "{err}"
        );
        assert!(err.contains("host/owner/repo"), "{err}");
        let public = event("/home/dev/public", Some("github.com/acme/public"));
        assert!(!c.allows(&public), "an unreadable policy allows nothing");
        let c = peer(&["oops"], &[]);
        assert!(c.policy().unwrap_err().to_string().contains("include"));
    }

    #[test]
    fn unattributed_telemetry_never_uploads_under_any_policy() {
        let unattributed = [otel(Some(false)), otel(None)];
        let private_hook = {
            let mut e = event("/home/dev/private", Some("github.com/acme/private"));
            e.attrs.insert("source".into(), json!("otel"));
            e.attrs
                .insert("x_otel_project_attributed".into(), json!(true));
            e
        };
        let public_hook = {
            let mut e = event("/home/dev/public", Some("github.com/acme/public"));
            e.attrs.insert("source".into(), json!("otel"));
            e.attrs
                .insert("x_otel_project_attributed".into(), json!(true));
            e
        };
        // No policy: everything uploads (the telemetry is the user's own).
        let open = peer(&[], &[]);
        assert!(unattributed.iter().all(|e| open.allows(e)));
        // An exclude list: an OTel prompt that could not be tied to a project
        // might be the excluded repository's — it stays.
        let excl = peer(&[], &["https://github.com/acme/private.git"]);
        for e in &unattributed {
            assert!(!excl.allows(e), "{:?}", e.attrs);
        }
        assert!(
            !excl.allows(&private_hook),
            "attributed to the excluded repo"
        );
        assert!(excl.allows(&public_hook));
        // An include list: the same, and an included repo's attributed
        // telemetry still goes.
        let incl = peer(&["github.com/acme/public"], &[]);
        for e in &unattributed {
            assert!(!incl.allows(e));
        }
        assert!(incl.allows(&public_hook) && !incl.allows(&private_hook));
        // Even an include list that names the placeholder project itself.
        let placeholder = format!("prj_{}", unattributed[0].project.project_id);
        let sneaky = peer(&[&placeholder], &[]);
        assert!(!sneaky.allows(&unattributed[0]));
    }

    #[test]
    fn the_url_must_be_https_or_this_machine() {
        for ok in [
            "https://sync.example.test",
            "HTTPS://sync.example.test/",
            "http://localhost:8787",
            "http://LOCALHOST",
            "http://127.0.0.1:8797/",
            "http://127.5.5.5",
            "http://[::1]:8787",
        ] {
            validate_url(ok).unwrap_or_else(|e| panic!("{ok}: {e:#}"));
        }
        for refused in [
            "http://sync.example.test",
            "http://10.0.0.5:8787",
            "http://192.168.1.2",
            "http://localhost.evil.example",
            "http://127.0.0.1.evil.example",
            "http://[::2]:8787",
            "http://0.0.0.0:8787",
            "http://evil.example@127.0.0.1",
        ] {
            let e = validate_url(refused).unwrap_err().to_string();
            assert!(
                e.contains("--allow-insecure-http") || e.contains("credentials"),
                "{refused}: {e}"
            );
        }
        // The explicit opt-in.
        assert_eq!(
            validate_url_opts("http://10.0.0.5:8787/", true).unwrap(),
            "http://10.0.0.5:8787"
        );
        // Credentials in the address are refused either way.
        assert!(validate_url_opts("https://user:pw@sync.example.test", true).is_err());
        assert!(validate_url("ftp://x").is_err());
        // The alias with an environment override cannot smuggle http in.
        assert!(resolve_url_with("vibemon", Some("http://evil.example")).is_err());
        assert_eq!(
            resolve_url_with_opts("vibemon", Some("http://evil.example/"), true).unwrap(),
            "http://evil.example"
        );
        assert_eq!(
            resolve_url_with("vibemon", Some("http://127.0.0.1:8797")).unwrap(),
            "http://127.0.0.1:8797"
        );
        // A hand-edited sync.json that names a plain-http host is not used.
        let mut c = PeerConfig::new("http://sync.example.test", "k");
        assert!(c.check_transport().is_err());
        c.allow_insecure_http = true;
        assert!(c.check_transport().is_ok());
        assert!(
            PeerConfig::new("http://localhost:1", "k")
                .check_transport()
                .is_ok()
        );
    }

    #[test]
    fn upload_paths_show_what_a_repository_shows_and_not_the_home_directory() {
        use attemptdb_core::PortablePath;
        let mut e = event("/Users/alice/work/repo", Some("github.com/acme/repo"));
        e.paths = vec![
            PortablePath::from_raw(
                "/Users/alice/work/repo/src/lib.rs",
                Some("/Users/alice/work/repo"),
            ),
            PortablePath::from_raw("/Users/alice/.ssh/config", Some("/Users/alice/work/repo")),
            PortablePath::from_raw("/etc/hosts", Some("/Users/alice/work/repo")),
            PortablePath::from_raw("C:\\Users\\Bob\\proj\\a.rs", None),
            PortablePath::from_raw("/home/carol/notes.md", Some("/Users/alice/work/repo")),
        ];
        let before = serde_json::to_string(&e).unwrap();
        for who in ["alice", "Bob", "carol"] {
            assert!(before.contains(who), "the fixture carries {who}");
        }
        for profile in [
            SyncProfile::MetadataOnly,
            SyncProfile::Semantic,
            SyncProfile::Messages,
        ] {
            let mut c = PeerConfig::new("https://x", "k");
            c.set_profile(profile);
            let (out, _) = prepare_for_upload(&c, e.clone());
            let text = serde_json::to_string(&out).unwrap();
            for who in ["alice", "Bob", "carol", "/Users/", "/home/"] {
                assert!(
                    !text.contains(who),
                    "{profile}: {who} left the device: {text}"
                );
            }
            let shown: Vec<&str> = out.paths.iter().map(|p| p.logical.as_str()).collect();
            assert_eq!(
                shown,
                [
                    "src/lib.rs",
                    "~/.ssh/config",
                    "/etc/hosts",
                    "~/proj/a.rs",
                    "~/notes.md"
                ],
                "{profile}"
            );
            assert!(out.paths.iter().all(|p| p.original == p.logical));
            assert_eq!(out.project.root, "~/work/repo");
            // The identity of the project is unchanged: ids are not paths.
            assert_eq!(out.project.project_id, e.project.project_id);
        }
        // `full` is the explicit opt-in to everything: paths are as captured.
        let mut full = PeerConfig::new("https://x", "k");
        full.set_profile(SyncProfile::Full);
        let (out, _) = prepare_for_upload(&full, e.clone());
        assert_eq!(out.paths, e.paths);
        assert_eq!(out.project.root, "/Users/alice/work/repo");
    }

    #[test]
    fn what_leaves_is_redacted_for_every_profile_that_sends_text() {
        let mut e = event("/home/dev/p", None);
        e.kind = EventKind::PromptSubmitted;
        e.content = Some(EventContent {
            prompt: Some(
                "deploy with DB_PASSWORD=hunter2 and Authorization: Bearer abc123def456ghi789"
                    .into(),
            ),
            command: Some("psql postgres://app:hunter2@db/app".into()),
            ..Default::default()
        });
        e.raw = Some(json!({"env": {"API_TOKEN": "tok-abc123"}}));
        // messages: the prompt only, redacted.
        let mut m = PeerConfig::new("https://x", "k");
        m.set_profile(SyncProfile::Messages);
        let (out, stats) = prepare_for_upload(&m, e.clone());
        let text = serde_json::to_string(&out).unwrap();
        assert!(
            !text.contains("hunter2") && !text.contains("abc123def456"),
            "{text}"
        );
        assert!(
            !text.contains("psql"),
            "commands never leave under messages"
        );
        assert_eq!(stats.spans, 2, "{stats:?}");
        assert_eq!(out.attrs["x_attemptdb_secrets_ruleset"], secrets::RULESET);
        assert_eq!(out.attrs["x_attemptdb_secrets_redacted"], 2);
        // full: everything, redacted.
        let mut f = PeerConfig::new("https://x", "k");
        f.set_profile(SyncProfile::Full);
        let (out, stats) = prepare_for_upload(&f, e.clone());
        let text = serde_json::to_string(&out).unwrap();
        for leaked in ["hunter2", "abc123def456", "tok-abc123"] {
            assert!(!text.contains(leaked), "{leaked}: {text}");
        }
        assert_eq!(stats.spans, 4, "{stats:?}");
        // metadata only: no text, so nothing was scanned and no stamp is made.
        let (out, stats) = prepare_for_upload(&PeerConfig::new("https://x", "k"), e);
        assert!(out.content.is_none() && out.raw.is_none() && stats.is_empty());
        assert!(!out.attrs.contains_key("x_attemptdb_secrets_ruleset"));
    }

    #[test]
    fn backoff_doubles_from_five_seconds_to_fifteen_minutes_with_jitter() {
        assert_eq!(backoff_delay(0, 0.5), Duration::ZERO);
        // jitter 1.0 is the whole step: 5, 10, 20 … s, capped at 900 s.
        let steps: Vec<u64> = (1..=10).map(|n| backoff_delay(n, 1.0).as_secs()).collect();
        assert_eq!(steps, [5, 10, 20, 40, 80, 160, 320, 640, 900, 900]);
        // jitter 0.0 is half of it: never below half, never above the step.
        assert_eq!(backoff_delay(3, 0.0), Duration::from_secs(10));
        for n in [1u32, 4, 9, 50, u32::MAX] {
            for j in [0.0, 0.25, 0.5, 0.999] {
                let d = backoff_delay(n, j);
                assert!(d <= BACKOFF_MAX && d >= BACKOFF_BASE / 2, "{n} {j} {d:?}");
            }
        }
        let j = jitter_unit();
        assert!((0.0..1.0).contains(&j));
    }

    #[test]
    fn a_failing_peer_is_held_back_until_a_run_succeeds() {
        let mut cfg = SyncConfig::default();
        cfg.peers
            .insert("default".into(), PeerConfig::new("https://x", "k"));
        let mut sch = PeerSchedule::default();
        let t0 = Instant::now();
        assert!(sch.due(&cfg, t0).is_empty()); // first sight
        let t5 = t0 + Duration::from_secs(5);
        assert_eq!(sch.due(&cfg, t5), ["default"]);
        sch.mark("default", t5);
        // The run failed: held for the first backoff step (5 s at full jitter).
        let wait = sch.failed("default", t5, 1.0);
        assert_eq!(wait, Duration::from_secs(5));
        assert_eq!(sch.failures("default"), 1);
        // Ten seconds on, the interval has elapsed and so has the hold: due.
        let t15 = t5 + Duration::from_secs(10);
        assert_eq!(sch.due(&cfg, t15), ["default"]);
        sch.mark("default", t15);
        // Second and third failure: 10 s, 20 s. At t15+9 s the interval (5 s)
        // is over but the hold (10 s) is not.
        assert_eq!(sch.failed("default", t15, 1.0), Duration::from_secs(10));
        assert!(sch.due(&cfg, t15 + Duration::from_secs(9)).is_empty());
        assert_eq!(sch.due(&cfg, t15 + Duration::from_secs(10)), ["default"]);
        // The hold has nine seconds to run, but the sleep still ends at the
        // interval (five), so sync.json is re-read in the meantime.
        assert_eq!(
            sch.next_sleep(&cfg, t15 + Duration::from_secs(1)),
            Duration::from_secs(5)
        );
        // A success resets everything.
        sch.succeeded("default");
        assert_eq!(sch.failures("default"), 0);
        sch.mark("default", t15);
        assert_eq!(sch.due(&cfg, t15 + Duration::from_secs(5)), ["default"]);
    }

    #[test]
    fn concurrent_saves_of_the_cursor_and_the_config_never_tear_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = SyncState::path(tmp.path(), Path::new("/db"), "default");
        let dir = tmp.path().join("config");
        let handles: Vec<_> = (0..6)
            .map(|i| {
                let (path, dir) = (path.clone(), dir.clone());
                std::thread::spawn(move || {
                    for n in 0..40u64 {
                        SyncState {
                            last_acked_source_seq: i * 1000 + n,
                            last_error: Some("x".repeat(3000)),
                            ..Default::default()
                        }
                        .save(&path)
                        .unwrap();
                        SyncState::load(&path).expect("a whole cursor file, never a torn one");
                        SyncConfig::single(PeerConfig::new(
                            format!("https://h{i}.example.test"),
                            format!("key-{i}-{n}"),
                        ))
                        .save(&dir)
                        .unwrap();
                        SyncConfig::load(&dir).unwrap().expect("a whole config");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        for d in [path.parent().unwrap(), dir.as_path()] {
            let temps: Vec<_> = std::fs::read_dir(d)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".tmp"))
                .collect();
            assert!(temps.is_empty(), "{temps:?}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(SyncConfig::path(&dir))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the key file stays private");
        }
    }
}
