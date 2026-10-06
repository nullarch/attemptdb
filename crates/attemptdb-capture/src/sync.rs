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
use attemptdb_core::event::normalise_remote;
use attemptdb_core::{
    CaptureMode, Event, EventId, EventKind, PortablePath, ProjectId, Timestamp, paths, secrets,
};
use attemptdb_storage::{Database, OpenOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const CONFIG_FILE: &str = "sync.json";
pub const DEFAULT_BATCH_EVENTS: usize = 1_000;
pub const DEFAULT_INTERVAL_SECS: u64 = 5;
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
    /// Events observed before this moment are never uploaded: history that
    /// predates the connection was not agreed to. `None` when the person
    /// passed `--include-history`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_before: Option<Timestamp>,
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
/// event carries — the same function on both sides of every comparison.
/// Lower-cased *before* normalising: the `.git` suffix is stripped
/// case-sensitively, and `…/Private.GIT` must name the same repository as
/// `…/private.git`.
fn canonical_remote(s: &str) -> Option<String> {
    normalise_remote(&s.trim().to_ascii_lowercase())
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
        match std::fs::read_to_string(&path) {
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
        match std::fs::read_to_string(path) {
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

/// Computes the inference set from the policy-allowed events. Supplied by
/// the binary (the projector lives above this crate), so the uploader stays
/// free of inference code.
pub type InferenceFn = dyn Fn(&[Event]) -> Result<InferenceSet> + Send + Sync;

#[derive(Clone)]
pub struct InferenceSource(pub Arc<InferenceFn>);

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

/// [`upload_once`], then — when `send_inferences` is on and a source is
/// supplied — the device's inference set computed from the same
/// policy-allowed events. `peer` selects the cursor file; it is not sent.
pub fn upload_once_with(
    locator: &Locator,
    peer: &str,
    cfg: &PeerConfig,
    source: Option<&InferenceSource>,
) -> Result<UploadReport> {
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
    let db = open_read_only(locator)?;
    let device_id = db.device_id();
    let (state, state_path) = SyncState::load_for(&locator.paths.data_dir, &locator.db_dir, peer)?;
    let mut state = state.bound_to(&cfg.url);
    let history_before = cfg.consent.as_ref().and_then(|c| c.history_before);
    let eligible = |e: &Event| {
        !is_discarded_telemetry(e)
            && policy.allows(e)
            && history_before.is_none_or(|w| e.observed_at >= w)
    };

    // Only what lies past the cursor is read: the manifest knows each
    // segment's `source_seq` range, so a tick that has nothing new decodes
    // nothing. Content (and so the encrypted blobs, one file each) is only
    // resolved when the profile sends it.
    let newest_seq = db.stats().last_source_seq;
    let after = state.last_acked_source_seq;
    let mut pending: Vec<Event> = if newest_seq > after {
        events_after(&db, cfg, after, cfg.sends_any_content(), false)?
    } else {
        Vec::new()
    };
    // History from before the person connected is not theirs to have agreed
    // to: counted, and kept local.
    let held_back = history_before.map_or(0, |w| {
        pending
            .iter()
            .filter(|e| e.observed_at < w && !is_discarded_telemetry(e) && policy.allows(e))
            .count()
    });
    pending.retain(|e| eligible(e));
    pending.sort_by_key(|e| e.source_seq);
    // The inference set is a function of the whole policy-allowed history,
    // so it is recomputed only when this tick uploaded something new, when
    // it was never uploaded, or when its last upload failed — never on an
    // idle tick (there are twelve of those a minute).
    let inference_retry_due = state
        .last_error
        .as_deref()
        .is_some_and(|e| e.starts_with("inferences:"))
        && state.last_error_at.is_none_or(|at| {
            Timestamp::now().as_micros() - at.as_micros() >= INFERENCE_RETRY_BACKOFF_MICROS
        });
    let recompute_inferences = cfg.send_inferences
        && source.is_some()
        && (!pending.is_empty() || state.last_inference_at.is_none() || inference_retry_due);
    let allowed: Vec<Event> = if recompute_inferences {
        // Inferences are computed from metadata; the whole history is
        // re-read here, so no blob is opened for it.
        let mut all = events_after(&db, cfg, 0, false, true)?;
        all.retain(|e| eligible(e));
        all.sort_by_key(|e| e.source_seq);
        // The server sees scrubbed paths, so its projection and this one
        // must agree on what a path is; and a path in an inference field
        // must not carry a home directory out.
        if cfg.profile() != SyncProfile::Full {
            for e in &mut all {
                scrub_paths(e);
            }
        }
        all
    } else {
        Vec::new()
    };
    drop(db);

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .build();
    let mut report = upload_events(
        &agent,
        cfg,
        device_id,
        pending,
        held_back,
        newest_seq,
        &mut state,
        &state_path,
    )?;
    if cfg.send_inferences {
        report.inferences = match source {
            Some(source) if recompute_inferences => Some(upload_inferences(
                &agent,
                cfg,
                device_id,
                &allowed,
                source,
                &mut state,
                &state_path,
            )?),
            // Nothing new since the last upload: the server holds the same
            // set already.
            Some(_) => Some(InferenceReport {
                unchanged: true,
                items: state.inference_items as usize,
                ..Default::default()
            }),
            // A caller without a projector (a bare uploader): report that
            // nothing was computed rather than pretend.
            None => Some(InferenceReport::default()),
        };
    }
    Ok(report)
}

/// Events past `after` in `source_seq` order, decoded from the segments
/// whose range reaches past it plus the WAL. Content is resolved only when
/// `with_content` asks for it (the profile sends it): the encrypted blobs
/// are one file each, and a metadata upload never opens them. Under
/// `messages` only the kinds that can carry something said open theirs.
///
/// A blob that cannot be read — no key, unreadable file — is an error, not
/// a silently empty event: the caller keeps the cursor and retries, so a
/// conversation never leaves the device as bare metadata by accident.
/// Events past `after`, oldest segment first. `skip_telemetry` drops OTel
/// observations while each batch is decoded, so they are never held in memory:
/// the projection ignores them, and they are most of a long-lived database.
fn events_after(
    db: &Database,
    cfg: &PeerConfig,
    after: u64,
    with_content: bool,
    skip_telemetry: bool,
) -> Result<Vec<Event>> {
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
    let mut out = Vec::new();
    for seg in &db.manifest().segments {
        if seg.max_source_seq <= after {
            continue;
        }
        let path = attemptdb_storage::segment::segments_dir(db.root()).join(&seg.file);
        for b in attemptdb_storage::segment::read_segment_batches(&path)
            .with_context(|| format!("reading segment {}", seg.file))?
        {
            out.extend(
                attemptdb_storage::segment::batch_to_events_where(
                    &b,
                    reader.as_ref(),
                    wants_content,
                )
                .with_context(|| format!("decoding segment {}", seg.file))?
                .into_iter()
                .filter(|e| e.source_seq > after && !(skip_telemetry && e.is_telemetry())),
            );
        }
    }
    if let Some(reader) = &reader {
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
    out.extend(
        db.memtable_events()
            .iter()
            .filter(|e| e.source_seq > after && !(skip_telemetry && e.is_telemetry()))
            .cloned(),
    );
    Ok(out)
}

/// Upload to every configured peer, one after another, in name order. A
/// failing peer keeps its own cursor and error and never stops the others;
/// the caller gets one result per peer.
pub fn upload_all(
    locator: &Locator,
    config: &SyncConfig,
    source: Option<&InferenceSource>,
) -> Vec<(String, Result<UploadReport>)> {
    config
        .peers
        .iter()
        .map(|(name, peer)| {
            let result = upload_once_with(locator, name, peer, source);
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
/// outside any repository; the project root the same way. Returns how many
/// values changed. `Event.paths[].original` keeps the provider's spelling on
/// the device; it does not leave (RFC 0006 §4.2).
pub fn scrub_paths(e: &mut Event) -> usize {
    let mut n = 0;
    for p in &mut e.paths {
        let shown = match &p.repo_relative {
            Some(rel) => rel.clone(),
            None => paths::elide_home(&p.logical),
        };
        if p.original != shown || p.logical != shown {
            n += 1;
        }
        let elided = shown.starts_with('~');
        *p = PortablePath {
            original: shown.clone(),
            logical: shown,
            repo_relative: p.repo_relative.take(),
            drive: if elided { None } else { p.drive.take() },
            unc: p.unc,
        };
    }
    let root = paths::elide_home(&e.project.root);
    if root != e.project.root {
        e.project.root = root;
        n += 1;
    }
    n
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
        stats = secrets::redact_event_content(&mut e);
    } else if cfg.send_messages && keep_messages_only(&mut e) {
        stats = secrets::redact_event_content(&mut e);
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

/// One run of the event uploader: the cursor, the counters, and the batch
/// size, with the rules for what to do when the server says no.
struct Run<'a> {
    agent: &'a ureq::Agent,
    cfg: &'a PeerConfig,
    device_id: attemptdb_core::DeviceId,
    state: &'a mut SyncState,
    state_path: &'a Path,
    report: UploadReport,
    capture_mode: CaptureMode,
    batch_size: usize,
}

impl Run<'_> {
    fn body(&self, events: &[Event]) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&json!({
            "sync_version": 1,
            "device_id": self.device_id,
            "batch_id": EventId::new().to_string(),
            "capture_mode": self.capture_mode.as_str(),
            "events": events,
        }))?)
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
        self.state.quarantine.push(QuarantineRecord {
            event_id: ev.event_id,
            source_seq: ev.source_seq,
            action: action.to_string(),
            status,
            reason: reason.chars().take(200).collect(),
            at: Timestamp::now(),
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

#[allow(clippy::too_many_arguments)]
fn upload_events(
    agent: &ureq::Agent,
    cfg: &PeerConfig,
    device_id: attemptdb_core::DeviceId,
    pending: Vec<Event>,
    held_back: usize,
    newest_seq: u64,
    state: &mut SyncState,
    state_path: &Path,
) -> Result<UploadReport> {
    let mut report = UploadReport {
        pending_before: pending.len(),
        before_consent: held_back,
        cursor: state.last_acked_source_seq,
        ..Default::default()
    };
    if held_back > 0 {
        state.before_consent += held_back as u64;
    }
    if pending.is_empty() {
        // Everything after the cursor was excluded by policy (or nothing is
        // new): advance the cursor so those events are not re-examined.
        if newest_seq > state.last_acked_source_seq || held_back > 0 {
            state.last_acked_source_seq = state.last_acked_source_seq.max(newest_seq);
            state.save(state_path)?;
            report.cursor = state.last_acked_source_seq;
        }
        return Ok(report);
    }
    let capture_mode = if cfg.sends_any_content() {
        CaptureMode::LocalSemantic
    } else {
        CaptureMode::MetadataOnly
    };
    let mut run = Run {
        agent,
        cfg,
        device_id,
        state,
        state_path,
        report,
        capture_mode,
        batch_size: cfg.batch_events.clamp(1, 5_000),
    };
    let mut redacted = 0usize;
    let mut start = 0;
    while start < pending.len() {
        let end = (start + run.batch_size).min(pending.len());
        let prepared: Vec<Event> = pending[start..end]
            .iter()
            .cloned()
            .map(|e| {
                let (e, stats) = prepare_for_upload(cfg, e);
                redacted += stats.spans;
                e
            })
            .collect();
        run.send(&prepared)?;
        start = end;
    }
    // Every event of the scan was either uploaded, skipped with a record, or
    // excluded by policy: the cursor covers the whole scan, so excluded
    // events are not re-examined on the next run.
    if newest_seq > run.state.last_acked_source_seq {
        run.state.last_acked_source_seq = newest_seq;
        run.state.save(state_path)?;
        run.report.cursor = newest_seq;
    }
    run.report.secrets_redacted = redacted;
    Ok(run.report)
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

fn upload_inferences(
    agent: &ureq::Agent,
    cfg: &PeerConfig,
    device_id: attemptdb_core::DeviceId,
    events: &[Event],
    source: &InferenceSource,
    state: &mut SyncState,
    state_path: &Path,
) -> Result<InferenceReport> {
    let set = (source.0)(events).context("computing inferences")?;
    let (items, content_removed) = prepare_inferences(cfg, set.items);
    let digest = inference_digest(&set.algorithm_version, &items)?;
    let mut report = InferenceReport {
        items: items.len(),
        content_removed,
        ..Default::default()
    };
    if state.last_inference_digest.as_deref() == Some(digest.as_str()) {
        report.unchanged = true;
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
        Err(ureq::Error::Transport(t)) => Err(PostError::Transport {
            url,
            message: t.to_string(),
        }),
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
