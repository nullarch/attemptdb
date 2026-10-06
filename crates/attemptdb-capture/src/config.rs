//! User configuration and the persistent device identity.
//!
//! Both files are small JSON documents so the hook path can read them in
//! microseconds without a TOML parser. Unknown keys are preserved.

use crate::{Result, io_at};
use attemptdb_core::{CaptureMode, DeviceId, Timestamp};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "config.json";
pub const DEVICE_FILE: &str = "device.json";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// Capture (privacy) mode applied to every new event.
    #[serde(default)]
    pub capture_mode: CaptureMode,
    /// Keep the original provider payload (`raw`) when the mode allows.
    #[serde(default = "default_true")]
    pub keep_raw_payload: bool,
    /// Mask credentials (password assignments, bearer tokens, URL
    /// credentials, issuer-prefixed keys, ...) in prompt, command, tool
    /// input/output and raw content before it is stored, as RFC 0006 §5
    /// says. On by default; `false` keeps content exactly as captured.
    #[serde(default = "default_true")]
    pub redact_secrets: bool,
    /// fsync every spool append. Off by default: the spool is a transport
    /// and the WAL is the durability boundary; fsync dominates hook latency.
    #[serde(default)]
    pub spool_sync: bool,
    /// Where HN/GitHub/CLI installs came from, for attribution. Never sent
    /// anywhere by the local product.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_source: Option<String>,
    /// Whether `content`/`raw` are moved into encrypted blobs at segment
    /// write (`crate::keys`). `attempt init --no-encryption` sets `Off`.
    #[serde(default)]
    pub encryption: EncryptionMode,
    /// Whether the daemon (and `attempt maintenance`) install releases on
    /// their own. `on`: releases the policy marks required at once, others
    /// within a day at a quiet moment. `required`: only the required ones.
    /// `off`: never — `attempt doctor` says one is available.
    /// `ATTEMPTDB_NO_AUTO_UPDATE=1` in the environment means `off` regardless.
    #[serde(default)]
    pub auto_update: AutoUpdate,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    /// Why `config.json` could not be used as written, when it could not.
    /// Set only by [`Config::load_or_default`] and never serialised: the
    /// value in this struct is then the fail-closed fallback (`capture_mode`
    /// is `metadata_only`), and `attempt doctor` and `attempt status` say
    /// so. Absent while the file is missing (that is a normal first run) or
    /// valid.
    #[serde(skip)]
    pub load_error: Option<String>,
}

/// How far the client goes on its own when a release is out (see
/// `crate::update`). The policy — which releases are required — is the
/// release's, published beside its assets; this is the machine's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutoUpdate {
    #[default]
    On,
    Required,
    Off,
}

impl AutoUpdate {
    pub fn as_str(self) -> &'static str {
        match self {
            AutoUpdate::On => "on",
            AutoUpdate::Required => "required",
            AutoUpdate::Off => "off",
        }
    }
}

/// Content-blob encryption policy of the writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EncryptionMode {
    /// Encrypt when a key is available from any source (OS key store, key
    /// file, passphrase); write inline otherwise. The default. One
    /// exception keeps a lost key from silently downgrading a database: when
    /// the database already holds encrypted blobs and no key can be had now,
    /// events wait in the spool for the key (up to 24 hours or 512 MiB, see
    /// `keys::ContentGate`) and are stored metadata-only after that, as under
    /// `Required`.
    #[default]
    Auto,
    /// Never encrypt new content; it stays inline in segments. Blobs
    /// written earlier stay readable while their key is available. This is
    /// the explicit choice (`attempt init --no-encryption`): plaintext
    /// content is written even to a database that holds encrypted blobs.
    Off,
    /// Never store content unencrypted. Without a key the writer still runs,
    /// but every event it stores is stored metadata-only (content stripped,
    /// `x_attemptdb_content_withheld` recorded) and `attempt doctor` says
    /// why; when a key existed (it was seen, or the database holds blobs) and
    /// cannot be read right now, events wait in the spool first, as under
    /// `Auto`. A key found later (the daemon re-checks) lifts the restriction.
    Required,
}

impl EncryptionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            EncryptionMode::Auto => "auto",
            EncryptionMode::Off => "off",
            EncryptionMode::Required => "required",
        }
    }
}

impl std::fmt::Display for EncryptionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for EncryptionMode {
    type Err = crate::CaptureError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(EncryptionMode::Auto),
            "off" | "none" | "disabled" => Ok(EncryptionMode::Off),
            "required" | "on" => Ok(EncryptionMode::Required),
            other => Err(crate::CaptureError::Other(format!(
                "unknown encryption mode '{other}' (expected auto, off, required)"
            ))),
        }
    }
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            capture_mode: CaptureMode::LocalSemantic,
            keep_raw_payload: true,
            redact_secrets: true,
            spool_sync: false,
            install_source: None,
            encryption: EncryptionMode::Auto,
            auto_update: AutoUpdate::On,
            extra: Default::default(),
            load_error: None,
        }
    }
}

/// The `capture_mode` text in `config.json` when it is not one of the three
/// canonical names.
fn unrecognised_capture_mode(text: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(text).ok()?;
    let name = value.get("capture_mode")?.as_str()?;
    (!matches!(name, "metadata_only" | "local_semantic" | "full_sync")).then(|| name.to_string())
}

impl Config {
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(CONFIG_FILE)
    }

    /// Load the config for the hook, the daemon and the CLI. It never fails
    /// (the hook path must not break an agent because of a config file), and
    /// it **fails closed**:
    ///
    /// - no file: the defaults (a normal first run);
    /// - a valid file: what it says;
    /// - a file that exists but cannot be used (unreadable, empty, not
    ///   UTF-8, not JSON, a trailing comma, an unknown `capture_mode`, any
    ///   field of the wrong type): `capture_mode = metadata_only` and
    ///   [`Config::load_error`] says why. A typo must never turn into full
    ///   prompts in the spool; `encryption` keeps what the file says when it
    ///   can be read, and an unreadable value means `required`.
    pub fn load_or_default(config_dir: &Path) -> Self {
        let path = Self::path(config_dir);
        match std::fs::read(&path) {
            Ok(bytes) => Self::from_bytes(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => Self::fail_closed(format!("{CONFIG_FILE} cannot be read: {e}")),
        }
    }

    /// [`Config::load_or_default`] for bytes already read.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let text = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
        // The usual case, in one pass.
        if let Ok(mut config) = serde_json::from_slice::<Config>(text) {
            // `CaptureMode` reads a name it does not know as `metadata_only`
            // (the safe direction), so a typo no longer fails the parse. It
            // is still a problem the person must hear about.
            if let Some(name) = unrecognised_capture_mode(text) {
                config.load_error = Some(format!(
                    "{CONFIG_FILE}: capture_mode \"{name}\" is not one of metadata_only, local_semantic, full_sync"
                ));
            }
            return config;
        }
        if text.iter().all(u8::is_ascii_whitespace) {
            return Self::fail_closed(format!("{CONFIG_FILE} is empty"));
        }
        let value: serde_json::Value = match serde_json::from_slice(text) {
            Ok(v) => v,
            Err(e) => return Self::fail_closed(format!("{CONFIG_FILE} is not valid JSON: {e}")),
        };
        let Some(map) = value.as_object() else {
            return Self::fail_closed(format!("{CONFIG_FILE} is not a JSON object"));
        };
        match serde_json::from_value::<Config>(value.clone()) {
            // Not reachable (the one-pass parse above takes it), kept so
            // this function is correct on its own.
            Ok(config) => config,
            Err(e) => {
                let mut config = Self::fail_closed(format!("{CONFIG_FILE}: {e}"));
                // Keep the independent settings that are readable. An
                // `encryption` value that is not understood is the strict
                // one: it only matters when content flows, and then it must
                // not fall back to the permissive default.
                config.encryption = pick(map, "encryption").unwrap_or(EncryptionMode::Required);
                config.keep_raw_payload = pick(map, "keep_raw_payload").unwrap_or(true);
                config.redact_secrets = pick(map, "redact_secrets").unwrap_or(true);
                config.spool_sync = pick(map, "spool_sync").unwrap_or(false);
                config.install_source = pick(map, "install_source");
                config.auto_update = pick(map, "auto_update").unwrap_or_default();
                config
            }
        }
    }

    fn fail_closed(error: String) -> Self {
        Self {
            capture_mode: CaptureMode::MetadataOnly,
            load_error: Some(error),
            ..Self::default()
        }
    }

    /// Write the config. A config that came from [`Config::load_or_default`]
    /// with a [`load_error`](Config::load_error) replaces a file the user
    /// wrote; that file is kept next to it as `config.json.invalid-<unix
    /// seconds>` first, and the CLI says so on stderr.
    pub fn save(&self, config_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(config_dir).map_err(|e| io_at(config_dir, e))?;
        let path = Self::path(config_dir);
        if let Some(why) = &self.load_error
            && path.exists()
        {
            let kept = config_dir.join(format!(
                "{CONFIG_FILE}.invalid-{}",
                Timestamp::now().as_micros() / 1_000_000
            ));
            if std::fs::rename(&path, &kept).is_ok() {
                eprintln!(
                    "warning: {why}; kept it as {} and wrote a fresh {CONFIG_FILE} (capture_mode {})",
                    kept.display(),
                    self.capture_mode
                );
            }
        }
        // A temp file of this process's own: several first runs (`setup` in
        // parallel, a hook beside `init`) write the config at once, and with
        // one shared temp name the second rename found the file already moved.
        // Each one now writes a whole file and renames it into place, and the
        // last rename wins.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = config_dir.join(format!(
            "{CONFIG_FILE}.tmp-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&tmp, bytes).map_err(|e| io_at(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            io_at(&path, e)
        })?;
        Ok(())
    }
}

/// One field of a config object, when it is present and well-formed.
fn pick<T: serde::de::DeserializeOwned>(
    map: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<T> {
    serde_json::from_value(map.get(key)?.clone()).ok()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceRecord {
    pub device_id: DeviceId,
    pub created_at: Timestamp,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl DeviceRecord {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(DEVICE_FILE)
    }

    /// Load the device identity, creating it on first use. See
    /// [`DeviceRecord::load_or_create_checked`].
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        Ok(Self::load_or_create_checked(data_dir)?.0)
    }

    /// Load the device identity, creating it on first use; the second value
    /// is where an unusable `device.json` was moved to, when this call had
    /// to replace one.
    ///
    /// Hooks race here on a machine's first use (parallel sub-agents), so
    /// the file is created with `create_new`: exactly one process writes it
    /// and every other one reads the winner's. A reader can catch the
    /// winner between creating the file and finishing the write, so an
    /// unparseable file is re-read for a short while before it counts as
    /// corrupt. A corrupt file is never overwritten, because that would
    /// quietly give the machine a new identity: it is moved aside to
    /// `device.json.corrupt-<unix seconds>` (see
    /// [`DeviceRecord::corrupt_backups`], which `attempt doctor` lists) and
    /// a new one is created, under a lock so that concurrent hooks repair it
    /// once. A file that cannot be read at all (permissions, I/O error) is
    /// an error, not a reason to re-key.
    pub fn load_or_create_checked(data_dir: &Path) -> Result<(Self, Option<PathBuf>)> {
        Self::load_or_create_waiting(data_dir, DEVICE_READ_RETRIES)
    }

    /// [`DeviceRecord::load_or_create_checked`] with the number of re-reads
    /// before an unparseable file counts as corrupt (tests use few).
    fn load_or_create_waiting(data_dir: &Path, retries: usize) -> Result<(Self, Option<PathBuf>)> {
        let path = Self::path(data_dir);
        match read_device(&path)? {
            Slot::Valid(rec) => return Ok((rec, None)),
            Slot::Missing | Slot::Unusable { .. } => {}
        }
        std::fs::create_dir_all(data_dir).map_err(|e| io_at(data_dir, e))?;
        let mut garbled_looks = 0;
        for _ in 0..retries {
            match read_device(&path)? {
                Slot::Valid(rec) => return Ok((rec, None)),
                Slot::Missing => {
                    if let Some(rec) = create_device(&path)? {
                        return Ok((rec, None));
                    }
                    // Lost the race: the winner's file is there now.
                }
                Slot::Unusable { empty } => {
                    // An empty file is a writer that has not written yet: wait
                    // for it. A file WITH content that does not parse is
                    // corrupt, not in flight (the record is linked into place
                    // complete): a couple of looks are enough. Waiting the
                    // full 0.4 s made every hook slow while a data directory
                    // could not be repaired (read-only, full).
                    if !empty {
                        garbled_looks += 1;
                        if garbled_looks > GARBLED_LOOKS {
                            break;
                        }
                    }
                    std::thread::sleep(DEVICE_READ_RETRY_PAUSE);
                }
            }
        }
        // Still unusable after waiting for a writer to finish: corrupt.
        repair_device(data_dir, &path)
    }

    /// Files an earlier repair moved a corrupt `device.json` to, oldest
    /// first. Each one marks a moment this machine got a new device id.
    pub fn corrupt_backups(data_dir: &Path) -> Vec<PathBuf> {
        let prefix = format!("{DEVICE_FILE}.corrupt-");
        let mut found: Vec<PathBuf> = std::fs::read_dir(data_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .map(|e| e.path())
            .collect();
        found.sort();
        found
    }
}

/// How often and how long a hook re-reads a `device.json` it cannot parse
/// before treating it as corrupt (about 0.4 s in all).
const DEVICE_READ_RETRIES: usize = 40;
/// Looks at a non-empty `device.json` that does not parse before it is repaired.
const GARBLED_LOOKS: usize = 2;
const DEVICE_READ_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(10);

enum Slot {
    Valid(DeviceRecord),
    Missing,
    /// Present but empty, torn or not a device record. `empty` is a file with
    /// no content yet: a writer between creating it and writing it.
    Unusable {
        empty: bool,
    },
}

fn read_device(path: &Path) -> Result<Slot> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(match serde_json::from_slice::<DeviceRecord>(&bytes) {
            Ok(rec) => Slot::Valid(rec),
            Err(_) => Slot::Unusable {
                empty: bytes.iter().all(u8::is_ascii_whitespace),
            },
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Slot::Missing),
        Err(e) => Err(io_at(path, e)),
    }
}

/// Create `device.json` if nobody has: `Some` is the record this call
/// wrote, `None` means the file already existed.
///
/// The record is written in full to a private file first and then linked
/// into place: `hard_link` fails if the target exists, so exactly one caller
/// wins and nobody ever sees a half-written `device.json`. A file system
/// without hard links gets the plain `create_new` + write, where a reader can
/// catch the writer in between (the callers re-read for a while for that).
fn create_device(path: &Path) -> Result<Option<DeviceRecord>> {
    use std::io::Write;
    let rec = DeviceRecord {
        device_id: DeviceId::new(),
        created_at: Timestamp::now(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        extra: Default::default(),
    };
    let bytes = serde_json::to_vec_pretty(&rec)?;
    let private = path.with_extension(format!("json.tmp-{}", rec.device_id));
    let staged = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&private)
        .and_then(|mut f| f.write_all(&bytes).and_then(|()| f.sync_all()));
    if staged.is_ok() {
        let linked = std::fs::hard_link(&private, path);
        let _ = std::fs::remove_file(&private);
        match linked {
            Ok(()) => return Ok(Some(rec)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
            Err(_) => {} // no hard links here: publish in place
        }
    } else {
        let _ = std::fs::remove_file(&private);
    }
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
        Err(e) => return Err(io_at(path, e)),
    };
    let written = file.write_all(&bytes).and_then(|()| file.sync_all());
    if let Err(e) = written {
        // Do not leave a torn file for the next hook to call corrupt.
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(io_at(path, e));
    }
    Ok(Some(rec))
}

/// Replace a corrupt `device.json` once, however many hooks notice it.
fn repair_device(data_dir: &Path, path: &Path) -> Result<(DeviceRecord, Option<PathBuf>)> {
    let lock_path = data_dir.join(format!("{DEVICE_FILE}.lock"));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| io_at(&lock_path, e))?;
    lock.lock().map_err(|e| io_at(&lock_path, e))?;
    let mut moved = None;
    let mut confirmed = false;
    let result = (|| loop {
        match read_device(path)? {
            // Another hook repaired it while this one waited for the lock.
            Slot::Valid(rec) => return Ok((rec, moved.take())),
            Slot::Missing => {
                if let Some(rec) = create_device(path)? {
                    return Ok((rec, moved.take()));
                }
            }
            Slot::Unusable { .. } if !confirmed => {
                // A file created without hard links can be caught mid-write
                // by this very loop; look once more before calling it corrupt.
                confirmed = true;
                std::thread::sleep(DEVICE_READ_RETRY_PAUSE * 3);
            }
            Slot::Unusable { .. } => {
                confirmed = false;
                let secs = Timestamp::now().as_micros() / 1_000_000;
                let mut aside = data_dir.join(format!("{DEVICE_FILE}.corrupt-{secs}"));
                if aside.exists() {
                    aside = data_dir.join(format!(
                        "{DEVICE_FILE}.corrupt-{secs}-{}",
                        std::process::id()
                    ));
                }
                std::fs::rename(path, &aside).map_err(|e| io_at(path, e))?;
                moved = Some(aside);
            }
        }
    })();
    let _ = lock.unlock();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(bytes: &[u8]) -> (tempfile::TempDir, Config) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(Config::path(tmp.path()), bytes).unwrap();
        let config = Config::load_or_default(tmp.path());
        (tmp, config)
    }

    #[test]
    fn a_missing_config_is_the_default_and_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config::load_or_default(tmp.path());
        assert_eq!(config.capture_mode, CaptureMode::LocalSemantic);
        assert_eq!(config.load_error, None);
        // Not even the directory exists yet (a first run).
        let config = Config::load_or_default(&tmp.path().join("never-created"));
        assert_eq!(config, Config::default());
    }

    #[test]
    fn a_valid_config_is_read_as_written() {
        let (_tmp, config) = write_config(
            br#"{"capture_mode":"full_sync","spool_sync":true,"encryption":"off","future_key":1}"#,
        );
        assert_eq!(config.capture_mode, CaptureMode::FullSync);
        assert!(config.spool_sync);
        assert_eq!(config.encryption, EncryptionMode::Off);
        assert_eq!(config.load_error, None);
        assert!(config.extra.contains_key("future_key"));
        // A UTF-8 byte order mark (Notepad adds one) is not a syntax error.
        let (_tmp, config) = write_config(b"\xEF\xBB\xBF{\"capture_mode\":\"metadata_only\"}");
        assert_eq!(config.capture_mode, CaptureMode::MetadataOnly);
        assert_eq!(config.load_error, None);
    }

    #[test]
    fn every_unusable_config_fails_closed_to_metadata_only() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "hyphenated typo",
                br#"{"capture_mode":"metadata-only"}"#.to_vec(),
            ),
            (
                "trailing comma",
                br#"{"capture_mode":"local_semantic",}"#.to_vec(),
            ),
            (
                "future enum value",
                br#"{"capture_mode":"semantic_v2"}"#.to_vec(),
            ),
            ("empty file", Vec::new()),
            ("whitespace only", b" \n\t ".to_vec()),
            ("non-UTF-8", vec![0xff, 0xfe, b'{', 0x80, b'}']),
            ("not an object", b"[\"local_semantic\"]".to_vec()),
            ("a string", b"\"local_semantic\"".to_vec()),
            (
                "valid mode, field of the wrong type",
                br#"{"capture_mode":"local_semantic","spool_sync":"yes"}"#.to_vec(),
            ),
            ("truncated", br#"{"capture_mode":"local_sem"#.to_vec()),
        ];
        for (name, bytes) in cases {
            let (_tmp, config) = write_config(&bytes);
            assert_eq!(config.capture_mode, CaptureMode::MetadataOnly, "{name}");
            assert!(
                config.load_error.is_some(),
                "{name}: the problem is recorded"
            );
        }
    }

    #[test]
    fn an_unreadable_config_fails_closed_too() {
        // A directory where the file should be: a read error that is not
        // "not found".
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(Config::path(tmp.path())).unwrap();
        let config = Config::load_or_default(tmp.path());
        assert_eq!(config.capture_mode, CaptureMode::MetadataOnly);
        assert!(
            config
                .load_error
                .as_deref()
                .unwrap()
                .contains("cannot be read")
        );
    }

    #[test]
    fn the_error_names_the_problem_and_independent_settings_survive() {
        let (_tmp, config) = write_config(
            br#"{"capture_mode":"metadata-only","encryption":"required","auto_update":"off","spool_sync":true,"install_source":"hn"}"#,
        );
        let why = config.load_error.unwrap();
        assert!(why.contains("metadata-only"), "{why}");
        assert_eq!(config.encryption, EncryptionMode::Required);
        assert_eq!(config.auto_update, AutoUpdate::Off);
        assert!(config.spool_sync);
        assert_eq!(config.install_source.as_deref(), Some("hn"));
        // An `encryption` value that is not understood is the strict one.
        let (_tmp, config) = write_config(br#"{"encryption":"requird"}"#);
        assert_eq!(config.encryption, EncryptionMode::Required);
        assert_eq!(config.capture_mode, CaptureMode::MetadataOnly);
    }

    #[test]
    fn saving_over_a_broken_file_keeps_it() {
        let (tmp, config) = write_config(br#"{"capture_mode":"metadata-only"}"#);
        config.save(tmp.path()).unwrap();
        let kept: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("config.json.invalid-")
            })
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(
            std::fs::read(kept[0].path()).unwrap(),
            br#"{"capture_mode":"metadata-only"}"#
        );
        let reloaded = Config::load_or_default(tmp.path());
        assert_eq!(reloaded.load_error, None);
        assert_eq!(reloaded.capture_mode, CaptureMode::MetadataOnly);
    }

    #[test]
    fn a_config_without_a_problem_has_no_error_to_write() {
        let tmp = tempfile::tempdir().unwrap();
        Config::default().save(tmp.path()).unwrap();
        assert!(
            !String::from_utf8_lossy(&std::fs::read(Config::path(tmp.path())).unwrap())
                .contains("load_error")
        );
        assert_eq!(Config::load_or_default(tmp.path()), Config::default());
    }

    /// Review regression: a `device.json` with content that does not parse
    /// was re-read for 0.4 s before it counted as corrupt, so a data
    /// directory that could not be repaired cost EVERY hook 460 ms. Content
    /// that does not parse is corrupt, not in flight: a couple of looks.
    #[test]
    fn a_garbled_device_file_is_repaired_in_milliseconds_not_after_a_long_wait() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(DEVICE_FILE), b"{\"device_id\": \"").unwrap();
        let started = std::time::Instant::now();
        let (_, moved) = DeviceRecord::load_or_create_waiting(tmp.path(), DEVICE_READ_RETRIES)
            .expect("a writable directory is repaired");
        assert!(moved.is_some(), "the garbled file was moved aside");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(300),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn racing_first_use_creates_one_device_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(32));
        let threads: Vec<_> = (0..32)
            .map(|_| {
                let dir = dir.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    DeviceRecord::load_or_create_checked(&dir).unwrap()
                })
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let first = results[0].0.device_id;
        assert!(results.iter().all(|(rec, _)| rec.device_id == first));
        assert!(results.iter().all(|(_, repaired)| repaired.is_none()));
        assert_eq!(DeviceRecord::load_or_create(&dir).unwrap().device_id, first);
        assert!(DeviceRecord::corrupt_backups(&dir).is_empty());
    }

    #[test]
    fn a_corrupt_device_file_is_moved_aside_once_and_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let path = DeviceRecord::path(dir);
        let first = DeviceRecord::load_or_create(dir).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        // Several hooks notice at once; the repair happens once.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.to_path_buf();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    DeviceRecord::load_or_create_checked(&dir).unwrap()
                })
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let second = results[0].0.device_id;
        assert_ne!(
            second, first.device_id,
            "a corrupt file means a new identity"
        );
        assert!(results.iter().all(|(rec, _)| rec.device_id == second));
        assert_eq!(
            results.iter().filter(|(_, moved)| moved.is_some()).count(),
            1,
            "exactly one process did the repair"
        );
        let backups = DeviceRecord::corrupt_backups(dir);
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(&backups[0]).unwrap(), b"{ not json");
        // And it is stable from then on.
        assert_eq!(DeviceRecord::load_or_create(dir).unwrap().device_id, second);
    }

    #[test]
    fn repairing_and_first_use_racing_never_split_the_identity() {
        // Hooks that find the file corrupt, hooks that find it just moved
        // aside (and create the new one) and hooks that read it while it is
        // being replaced: one identity, one repair, and no valid file ever
        // mistaken for a corrupt one. Many rounds, because the bad
        // interleaving is narrow.
        for round in 0..60 {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path();
            let corrupt: &[u8] = if round % 2 == 0 { b"{ not json" } else { b"" };
            std::fs::write(DeviceRecord::path(dir), corrupt).unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    let dir = dir.to_path_buf();
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        DeviceRecord::load_or_create_waiting(&dir, 2).unwrap()
                    })
                })
                .collect();
            let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
            let id = results[0].0.device_id;
            assert!(
                results.iter().all(|(rec, _)| rec.device_id == id),
                "round {round}: {results:?}"
            );
            assert_eq!(
                DeviceRecord::load_or_create(dir).unwrap().device_id,
                id,
                "round {round}: the file on disk is the one everybody returned"
            );
            let backups = DeviceRecord::corrupt_backups(dir);
            assert_eq!(backups.len(), 1, "round {round}: {backups:?}");
            assert_eq!(std::fs::read(&backups[0]).unwrap(), corrupt);
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_device_file_is_an_error_not_a_new_identity() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let rec = DeviceRecord::load_or_create(dir).unwrap();
        let path = DeviceRecord::path(dir);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            return; // root reads anything; nothing to prove here
        }
        assert!(DeviceRecord::load_or_create(dir).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            DeviceRecord::load_or_create(dir).unwrap().device_id,
            rec.device_id
        );
        assert!(DeviceRecord::corrupt_backups(dir).is_empty());
    }
}
