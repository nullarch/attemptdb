//! Master-key management for encrypted content blobs (RFC 0006 §7.2).
//!
//! The storage crate encrypts `content`/`raw` into `blobs/` under a 32-byte
//! master key it obtains from a [`KeyProvider`]; this module is the provider.
//! A key is identified by [`KeyId`], a fingerprint derived from the key
//! itself, so a bare key file or passphrase can name its own key.
//!
//! Sources, in the order the first one found becomes the **current** key
//! (the one new blobs are written under):
//!
//! 1. an explicit key file (`--key-file`, or `ATTEMPTDB_KEY_FILE`);
//! 2. the OS key store via the `keyring` crate — macOS Keychain, Windows
//!    Credential Manager, Linux Secret Service — service `dev.attemptdb`,
//!    account `<db_id>`;
//! 3. `<data_dir>/keys/<db_id>.key` (mode `0600`; created only by
//!    `attempt keys init --key-file`);
//! 4. a passphrase in `ATTEMPTDB_PASSPHRASE`, stretched with Argon2id
//!    (m = 64 MiB, t = 3, p = 1; salt derived from the `db_id`).
//!
//! Every source found joins one ring, so a blob decrypts whichever source
//! wrote it. Keys retained by a rotation are looked up on demand under
//! `<db_id>/<key_id>` (key store) or `<db_id>.<key_id>.key` (key file).
//!
//! `ATTEMPTDB_KEYRING=off` disables the OS key store (CI, headless hosts,
//! tests). Key material is never logged; only key ids are.

use crate::config::EncryptionMode;
use crate::locator::Locator;
use crate::{CaptureError, Result};
use attemptdb_storage::blobs::{self, BlobStore, KeyId, KeyProvider, MasterKey};
use attemptdb_storage::manifest::Manifest;
use attemptdb_storage::{Database, Identity, OpenOptions, StorageError, segment};
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;
use zeroize::Zeroizing;

/// `keyring` service name.
pub const KEYRING_SERVICE: &str = "dev.attemptdb";
/// Environment variable naming an explicit key file.
pub const KEY_FILE_ENV: &str = "ATTEMPTDB_KEY_FILE";
/// Environment variable holding a passphrase.
pub const PASSPHRASE_ENV: &str = "ATTEMPTDB_PASSPHRASE";
/// Set to `off` to skip the OS key store entirely.
pub const KEYRING_ENV: &str = "ATTEMPTDB_KEYRING";
/// Subdirectory of the data directory holding key files.
pub const KEYS_DIR: &str = "keys";
/// Argon2id memory cost (KiB) for passphrase-derived keys.
pub const ARGON2_M_COST_KIB: u32 = 64 * 1024;
/// Argon2id iterations.
pub const ARGON2_T_COST: u32 = 3;
/// Argon2id parallelism.
pub const ARGON2_P_COST: u32 = 1;

/// Where the current key came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    /// The OS key store (`keyring`).
    Keyring,
    /// A key file at this path.
    KeyFile(PathBuf),
    /// Derived from a passphrase; nothing is stored.
    Passphrase,
    /// No key from any source.
    None,
}

impl fmt::Display for KeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeySource::Keyring => write!(f, "OS key store ({KEYRING_SERVICE})"),
            KeySource::KeyFile(p) => write!(f, "key file {}", p.display()),
            KeySource::Passphrase => write!(f, "passphrase (${PASSPHRASE_ENV})"),
            KeySource::None => write!(f, "none"),
        }
    }
}

/// How [`KeyStore::open_with`] consults its sources.
#[derive(Clone, Debug)]
pub struct KeyStoreOptions {
    /// Explicit key file; takes precedence over every other source.
    pub key_file: Option<PathBuf>,
    /// Consult the OS key store.
    pub use_keyring: bool,
    /// Passphrase to stretch into a key.
    pub passphrase: Option<String>,
}

impl KeyStoreOptions {
    /// `ATTEMPTDB_KEY_FILE`, `ATTEMPTDB_KEYRING`, `ATTEMPTDB_PASSPHRASE`.
    pub fn from_env() -> Self {
        let off = std::env::var(KEYRING_ENV)
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "off" | "0" | "false" | "no"
                )
            })
            .unwrap_or(false);
        Self {
            key_file: std::env::var_os(KEY_FILE_ENV)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            use_keyring: !off,
            passphrase: std::env::var(PASSPHRASE_ENV).ok().filter(|v| !v.is_empty()),
        }
    }

    /// No OS key store, no environment: only key files under the data
    /// directory (and whatever is set explicitly). Used by tests and CI.
    pub fn offline() -> Self {
        Self {
            key_file: None,
            use_keyring: false,
            passphrase: None,
        }
    }
}

/// `<data_dir>/keys/<db_id>.key`.
pub fn default_key_file(locator: &Locator, db_id: Uuid) -> PathBuf {
    locator
        .paths
        .data_dir
        .join(KEYS_DIR)
        .join(format!("{db_id}.key"))
}

/// Where a rotation keeps the previous key of `current`: `<stem>.<key_id>.key`
/// next to it.
fn retained_key_file(current: &Path, key_id: KeyId) -> PathBuf {
    let stem = current
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "key".into());
    current.with_file_name(format!("{stem}.{key_id}.key"))
}

fn keyring_account(db_id: Uuid) -> String {
    db_id.to_string()
}

fn keyring_retained_account(db_id: Uuid, key_id: KeyId) -> String {
    format!("{db_id}/{key_id}")
}

struct Slot {
    master: Zeroizing<MasterKey>,
    #[allow(dead_code)]
    source: KeySource,
}

/// The ring of master keys available for one database.
pub struct KeyStore {
    db_id: Uuid,
    keys_dir: PathBuf,
    explicit_key_file: Option<PathBuf>,
    use_keyring: bool,
    source: KeySource,
    current: Option<KeyId>,
    /// Cache of every id ever asked for, including negative answers.
    ring: Mutex<HashMap<KeyId, Option<Slot>>>,
    notes: Vec<String>,
}

impl KeyStore {
    /// Open with [`KeyStoreOptions::from_env`]. Never fails: an unreadable
    /// source is skipped and described in [`KeyStore::notes`].
    pub fn open(locator: &Locator, db_id: Uuid) -> Self {
        Self::open_with(locator, db_id, KeyStoreOptions::from_env())
    }

    pub fn open_with(locator: &Locator, db_id: Uuid, opts: KeyStoreOptions) -> Self {
        let mut notes = Vec::new();
        let mut found: Vec<(Zeroizing<MasterKey>, KeySource)> = Vec::new();
        if let Some(p) = &opts.key_file {
            match blobs::read_key_file(p) {
                Ok(k) => found.push((Zeroizing::new(k), KeySource::KeyFile(p.clone()))),
                Err(e) => notes.push(format!("explicit key file unusable: {e}")),
            }
        }
        if opts.use_keyring {
            match keyring_get(&keyring_account(db_id)) {
                Ok(Some(k)) => found.push((Zeroizing::new(k), KeySource::Keyring)),
                Ok(None) => {}
                Err(e) => notes.push(format!("OS key store unavailable: {e}")),
            }
        }
        let default = default_key_file(locator, db_id);
        if default.is_file() {
            match blobs::read_key_file(&default) {
                Ok(k) => found.push((Zeroizing::new(k), KeySource::KeyFile(default.clone()))),
                Err(e) => notes.push(format!("key file unusable: {e}")),
            }
        }
        if let Some(pp) = &opts.passphrase {
            match master_from_passphrase(pp, db_id) {
                Ok(k) => found.push((Zeroizing::new(k), KeySource::Passphrase)),
                Err(e) => notes.push(format!("passphrase unusable: {e}")),
            }
        }
        let mut ring = HashMap::new();
        let mut current = None;
        let mut source = KeySource::None;
        for (master, src) in found {
            let id = blobs::key_id_for(&master);
            if current.is_none() {
                current = Some(id);
                source = src.clone();
            }
            ring.entry(id).or_insert(Some(Slot {
                master,
                source: src,
            }));
        }
        Self {
            db_id,
            keys_dir: locator.paths.data_dir.join(KEYS_DIR),
            explicit_key_file: opts.key_file,
            use_keyring: opts.use_keyring,
            source,
            current,
            ring: Mutex::new(ring),
            notes,
        }
    }

    pub fn db_id(&self) -> Uuid {
        self.db_id
    }

    /// Source of the current key ([`KeySource::None`] when there is none).
    pub fn source(&self) -> &KeySource {
        &self.source
    }

    pub fn current_key_id(&self) -> Option<KeyId> {
        self.current
    }

    pub fn has_key(&self) -> bool {
        self.current.is_some()
    }

    /// Why a source was skipped (never key material).
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Whether the ring can supply `key_id` (consults retained keys).
    pub fn holds(&self, key_id: KeyId) -> bool {
        self.key(key_id).is_some()
    }

    /// Look for a retained key (written by a rotation) with this id.
    fn find_retained(&self, key_id: KeyId) -> Option<Slot> {
        if self.use_keyring
            && let Ok(Some(k)) = keyring_get(&keyring_retained_account(self.db_id, key_id))
            && blobs::key_id_for(&k) == key_id
        {
            return Some(Slot {
                master: Zeroizing::new(k),
                source: KeySource::Keyring,
            });
        }
        let mut candidates = vec![retained_key_file(
            &self.keys_dir.join(format!("{}.key", self.db_id)),
            key_id,
        )];
        if let Some(p) = &self.explicit_key_file {
            candidates.push(retained_key_file(p, key_id));
        }
        for p in candidates {
            if p.is_file()
                && let Ok(k) = blobs::read_key_file(&p)
                && blobs::key_id_for(&k) == key_id
            {
                return Some(Slot {
                    master: Zeroizing::new(k),
                    source: KeySource::KeyFile(p),
                });
            }
        }
        None
    }
}

impl KeyProvider for KeyStore {
    fn key(&self, key_id: KeyId) -> Option<MasterKey> {
        let mut ring = self.ring.lock().ok()?;
        if let Some(slot) = ring.get(&key_id) {
            return slot.as_ref().map(|s| *s.master);
        }
        let slot = self.find_retained(key_id);
        let out = slot.as_ref().map(|s| *s.master);
        ring.insert(key_id, slot);
        out
    }

    fn current(&self) -> Option<(KeyId, MasterKey)> {
        let id = self.current?;
        let ring = self.ring.lock().ok()?;
        ring.get(&id)?.as_ref().map(|s| (id, *s.master))
    }
}

impl fmt::Debug for KeyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyStore")
            .field("db_id", &self.db_id)
            .field("source", &self.source)
            .field("current", &self.current)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Providers for the CLI / daemon
// ---------------------------------------------------------------------------

/// The provider every database open should receive: `Some` when any source
/// yields a current key, `None` otherwise (the database then reads and
/// writes inline content).
pub fn provider_for(locator: &Locator, db_id: Uuid) -> Option<Arc<dyn KeyProvider>> {
    provider_with(locator, db_id, KeyStoreOptions::from_env())
}

/// [`provider_for`] with explicit options.
pub fn provider_with(
    locator: &Locator,
    db_id: Uuid,
    opts: KeyStoreOptions,
) -> Option<Arc<dyn KeyProvider>> {
    let store = KeyStore::open_with(locator, db_id, opts);
    if store.has_key() {
        Some(Arc::new(store))
    } else {
        None
    }
}

/// [`provider_for`] for the database directory `db_dir` (reads its identity
/// file). `None` when the directory is not a database yet.
pub fn provider_for_db(locator: &Locator, db_dir: &Path) -> Option<Arc<dyn KeyProvider>> {
    let identity = Identity::load(db_dir).ok()?;
    provider_for(locator, identity.db_id)
}

/// Apply the configured [`EncryptionMode`]: `Off` never returns a provider,
/// `Auto` returns one when a key exists, `Required` fails without one.
pub fn provider_for_mode(
    locator: &Locator,
    db_id: Uuid,
    mode: EncryptionMode,
    opts: Option<KeyStoreOptions>,
) -> Result<Option<Arc<dyn KeyProvider>>> {
    let opts = opts.unwrap_or_else(KeyStoreOptions::from_env);
    match mode {
        EncryptionMode::Off => Ok(None),
        EncryptionMode::Auto => Ok(provider_with(locator, db_id, opts)),
        EncryptionMode::Required => provider_with(locator, db_id, opts)
            .map(Some)
            .ok_or_else(|| {
                CaptureError::Other(format!(
                    "encryption is required but no key is available; run `attempt keys init`, or set {KEY_FILE_ENV} / {PASSPHRASE_ENV}"
                ))
            }),
    }
}

// ---------------------------------------------------------------------------
// Passphrases
// ---------------------------------------------------------------------------

/// Stretch a passphrase into a master key with Argon2id. The salt is
/// `SHA-256("attemptdb/passphrase/v1" ‖ db_id)[..16]`, so the same
/// passphrase yields a different key per database.
pub fn master_from_passphrase(passphrase: &str, db_id: Uuid) -> Result<MasterKey> {
    master_from_passphrase_with(passphrase, db_id, ARGON2_M_COST_KIB, ARGON2_T_COST)
}

fn master_from_passphrase_with(
    passphrase: &str,
    db_id: Uuid,
    m_cost_kib: u32,
    t_cost: u32,
) -> Result<MasterKey> {
    use argon2::{Algorithm, Argon2, Params, Version};
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"attemptdb/passphrase/v1");
    h.update(db_id.as_bytes());
    let digest = h.finalize();
    let params = Params::new(m_cost_kib, t_cost, ARGON2_P_COST, Some(32))
        .map_err(|e| CaptureError::Other(format!("argon2 parameters: {e}")))?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), &digest[..16], &mut out)
        .map_err(|e| CaptureError::Other(format!("argon2: {e}")))?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// OS key store
// ---------------------------------------------------------------------------

fn keyring_available() -> std::result::Result<(), String> {
    match keyring::Entry::store_status() {
        Ok(()) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn keyring_entry(account: &str) -> std::result::Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, account).map_err(|e| e.to_string())
}

fn keyring_get(account: &str) -> std::result::Result<Option<MasterKey>, String> {
    let entry = keyring_entry(account)?;
    match entry.get_password() {
        Ok(hex) => {
            let hex = Zeroizing::new(hex);
            parse_hex_key(hex.trim())
                .map(Some)
                .ok_or_else(|| format!("entry {account} does not hold a 64-hex-character key"))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn keyring_set(account: &str, master: &MasterKey) -> std::result::Result<(), String> {
    let hex = Zeroizing::new(hex::encode(master));
    keyring_entry(account)?
        .set_password(&hex)
        .map_err(|e| e.to_string())
}

fn keyring_delete(account: &str) -> std::result::Result<(), String> {
    match keyring_entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn parse_hex_key(s: &str) -> Option<MasterKey> {
    let bytes = hex::decode(s).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Some(out)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct InitOptions {
    /// Create a key file under the data directory instead of using the OS
    /// key store.
    pub key_file: bool,
    /// Derive the key from the passphrase in this environment variable
    /// (nothing is persisted; the variable must be set for every command).
    pub passphrase_env: Option<String>,
    /// Source options; `None` means [`KeyStoreOptions::from_env`].
    pub store: Option<KeyStoreOptions>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InitReport {
    /// `false` when a key already existed.
    pub created: bool,
    pub source: KeySource,
    pub key_id: KeyId,
    /// Why this source was chosen.
    pub reason: String,
}

/// Create a master key for `db_id` unless one already exists. Prefers the
/// OS key store; falls back to a key file (saying why); a passphrase
/// source is only used on request.
pub fn init(locator: &Locator, db_id: Uuid, opts: &InitOptions) -> Result<InitReport> {
    let store_opts = opts.store.clone().unwrap_or_else(KeyStoreOptions::from_env);
    let existing = KeyStore::open_with(locator, db_id, store_opts.clone());
    if let Some(key_id) = existing.current_key_id() {
        return Ok(InitReport {
            created: false,
            source: existing.source().clone(),
            key_id,
            reason: "a key already exists".into(),
        });
    }
    if let Some(var) = &opts.passphrase_env {
        let passphrase = std::env::var(var)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                CaptureError::Other(format!("{var} is not set; export it and run again"))
            })?;
        let master = Zeroizing::new(master_from_passphrase(&passphrase, db_id)?);
        return Ok(InitReport {
            created: true,
            source: KeySource::Passphrase,
            key_id: blobs::key_id_for(&master),
            reason: format!(
                "derived from the passphrase in ${var}; nothing is stored, so the variable must be set for every command"
            ),
        });
    }
    let master = Zeroizing::new(blobs::random_master_key()?);
    let key_id = blobs::key_id_for(&master);
    let reason = if opts.key_file {
        "--key-file requested".to_string()
    } else if !store_opts.use_keyring {
        format!("OS key store disabled ({KEYRING_ENV}=off)")
    } else {
        match keyring_available().and_then(|_| keyring_set(&keyring_account(db_id), &master)) {
            Ok(()) => {
                return Ok(InitReport {
                    created: true,
                    source: KeySource::Keyring,
                    key_id,
                    reason: "stored in the OS key store".into(),
                });
            }
            Err(e) => format!("OS key store unavailable: {e}"),
        }
    };
    let path = default_key_file(locator, db_id);
    blobs::write_key_file(&path, &master)?;
    Ok(InitReport {
        created: true,
        source: KeySource::KeyFile(path),
        key_id,
        reason: format!("{reason}; key file written"),
    })
}

/// Write the current master key to `out` as a 0600 hex file. Refuses to
/// overwrite. Returns the key id.
pub fn export_master(
    locator: &Locator,
    db_id: Uuid,
    opts: Option<KeyStoreOptions>,
    out: &Path,
) -> Result<KeyId> {
    let store = KeyStore::open_with(
        locator,
        db_id,
        opts.unwrap_or_else(KeyStoreOptions::from_env),
    );
    let (key_id, master) = store
        .current()
        .ok_or_else(|| CaptureError::Other("no key to export; run `attempt keys init`".into()))?;
    let master = Zeroizing::new(master);
    blobs::write_key_file(out, &master)?;
    Ok(key_id)
}

#[derive(Clone, Debug, Serialize)]
pub struct RotateReport {
    pub old_key_id: KeyId,
    pub new_key_id: KeyId,
    pub source: KeySource,
    /// Blobs rewritten under the new key.
    pub rewritten: u64,
    /// Blobs that already used the new key.
    pub skipped: u64,
    /// Blobs that could not be rewritten (the old key stays retained).
    pub failed: Vec<String>,
    /// Whether the old key was removed from its source.
    pub forgot_old: bool,
}

/// Rotate the master key: generate a new one, retain the old one under its
/// id, install the new one as current, then re-encrypt every blob under
/// the writer lock (one temp file + rename per blob). With `forget_old`
/// the retained copy is removed once every blob is rewritten. Content in
/// the WAL is unaffected: it is encrypted under the current key at its
/// next flush.
pub fn rotate(
    locator: &Locator,
    db_dir: &Path,
    opts: Option<KeyStoreOptions>,
    forget_old: bool,
) -> Result<RotateReport> {
    let identity = Identity::load(db_dir)?;
    let db_id = identity.db_id;
    let store_opts = opts.unwrap_or_else(KeyStoreOptions::from_env);
    let old = KeyStore::open_with(locator, db_id, store_opts.clone());
    let (old_key_id, old_master) = old.current().ok_or_else(|| {
        CaptureError::Other("no current key; run `attempt keys init` first".into())
    })?;
    let old_master = Zeroizing::new(old_master);
    let source = old.source().clone();
    drop(old);
    let new_master = Zeroizing::new(blobs::random_master_key()?);
    let new_key_id = blobs::key_id_for(&new_master);

    // 1. Retain the old key under its id, *then* install the new one, so a
    //    crash in between never leaves blobs without a key.
    match &source {
        KeySource::Keyring => {
            keyring_set(&keyring_retained_account(db_id, old_key_id), &old_master)
                .map_err(|e| CaptureError::Other(format!("OS key store: {e}")))?;
            keyring_set(&keyring_account(db_id), &new_master)
                .map_err(|e| CaptureError::Other(format!("OS key store: {e}")))?;
        }
        KeySource::KeyFile(path) => {
            let retained = retained_key_file(path, old_key_id);
            if !retained.exists() {
                blobs::write_key_file(&retained, &old_master)?;
            }
            replace_key_file(path, &new_master)?;
        }
        KeySource::Passphrase | KeySource::None => {
            return Err(CaptureError::Other(
                "rotation needs a key stored in the OS key store or a key file; a passphrase-derived key cannot be rotated in place".into(),
            ));
        }
    }

    // 2. Re-encrypt under the writer lock with a ring that holds both keys.
    let provider: Arc<dyn KeyProvider> = Arc::new(KeyStore::open_with(locator, db_id, store_opts));
    if provider.current().map(|(id, _)| id) != Some(new_key_id) {
        return Err(CaptureError::Other(
            "the new key did not become current; another source takes precedence (explicit key file?)".into(),
        ));
    }
    let db = Database::open(
        db_dir,
        OpenOptions {
            keys: Some(provider.clone()),
            ..Default::default()
        },
    )
    .map_err(|e| match e {
        StorageError::Locked(p) => CaptureError::Other(format!(
            "database {} is locked by another writer (a running daemon?); stop it and retry — both keys are kept",
            p.display()
        )),
        other => other.into(),
    })?;
    let report = db
        .blob_store()
        .reencrypt_all(provider.as_ref(), new_key_id, &new_master)?;
    drop(db);

    // 3. Forget the old key only once nothing needs it.
    let mut forgot_old = false;
    if forget_old && report.failed.is_empty() {
        match &source {
            KeySource::Keyring => {
                keyring_delete(&keyring_retained_account(db_id, old_key_id))
                    .map_err(|e| CaptureError::Other(format!("OS key store: {e}")))?;
            }
            KeySource::KeyFile(path) => {
                let retained = retained_key_file(path, old_key_id);
                if retained.exists() {
                    std::fs::remove_file(&retained).map_err(|e| crate::io_at(&retained, e))?;
                }
            }
            KeySource::Passphrase | KeySource::None => {}
        }
        forgot_old = true;
    }
    Ok(RotateReport {
        old_key_id,
        new_key_id,
        source,
        rewritten: report.rewritten,
        skipped: report.skipped,
        failed: report.failed,
        forgot_old,
    })
}

/// Atomically replace a key file: write `<path>.tmp` (0600, exclusive),
/// then rename over `path`.
fn replace_key_file(path: &Path, master: &MasterKey) -> Result<()> {
    let tmp = path.with_extension("key.tmp");
    if tmp.exists() {
        std::fs::remove_file(&tmp).map_err(|e| crate::io_at(&tmp, e))?;
    }
    blobs::write_key_file(&tmp, master)?;
    std::fs::rename(&tmp, path).map_err(|e| crate::io_at(path, e))?;
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct KeysStatus {
    pub db_id: Uuid,
    pub source: KeySource,
    pub key_id: Option<KeyId>,
    /// Key ids found in blob headers.
    pub blob_key_ids: Vec<KeyId>,
    /// Of those, the ids no source can supply (content locked).
    pub missing_key_ids: Vec<KeyId>,
    pub blobs: u64,
    pub blob_bytes: u64,
    pub segments: usize,
    /// Format 1 segments: content inline, unencrypted.
    pub inline_segments: usize,
    pub notes: Vec<String>,
}

/// Key source, key id, blob count/bytes, and how many segments still hold
/// unencrypted inline content.
pub fn status(
    locator: &Locator,
    db_dir: &Path,
    opts: Option<KeyStoreOptions>,
) -> Result<KeysStatus> {
    let identity = Identity::load(db_dir)?;
    let store = KeyStore::open_with(
        locator,
        identity.db_id,
        opts.unwrap_or_else(KeyStoreOptions::from_env),
    );
    let blob_store = BlobStore::new(db_dir, identity.db_id, identity.device_id);
    let stats = blob_store.stats()?;
    let blob_key_ids: Vec<KeyId> = blob_store.all_key_ids()?.into_iter().collect();
    let missing_key_ids = blob_key_ids
        .iter()
        .copied()
        .filter(|id| !store.holds(*id))
        .collect();
    let mut segments = 0;
    let mut inline_segments = 0;
    if let Some((manifest, _)) = Manifest::load_latest(db_dir)? {
        for seg in &manifest.segments {
            segments += 1;
            let path = segment::segments_dir(db_dir).join(&seg.file);
            if segment::segment_format_version(&path)? == 1 {
                inline_segments += 1;
            }
        }
    }
    Ok(KeysStatus {
        db_id: identity.db_id,
        source: store.source().clone(),
        key_id: store.current_key_id(),
        blob_key_ids,
        missing_key_ids,
        blobs: stats.count,
        blob_bytes: stats.bytes,
        segments,
        inline_segments,
        notes: store.notes().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::event::{EventContent, Provider};
    use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
    use attemptdb_storage::ScanFilter;

    struct Sandbox {
        _tmp: tempfile::TempDir,
        locator: Locator,
    }

    fn sandbox() -> Sandbox {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let locator = Locator::resolve(&project, Some(&data_dir), None);
        Sandbox { _tmp: tmp, locator }
    }

    fn offline_init(key_file: bool) -> InitOptions {
        InitOptions {
            key_file,
            passphrase_env: None,
            store: Some(KeyStoreOptions::offline()),
        }
    }

    #[test]
    fn key_file_init_is_idempotent_and_provides_the_key() {
        let sb = sandbox();
        let db_id = Uuid::now_v7();
        let r = init(&sb.locator, db_id, &offline_init(true)).unwrap();
        assert!(r.created);
        let path = match &r.source {
            KeySource::KeyFile(p) => p.clone(),
            other => panic!("expected key file, got {other:?}"),
        };
        assert_eq!(path, default_key_file(&sb.locator, db_id));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.trim().len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // Second init reports the existing key.
        let again = init(&sb.locator, db_id, &offline_init(true)).unwrap();
        assert!(!again.created);
        assert_eq!(again.key_id, r.key_id);

        let store = KeyStore::open_with(&sb.locator, db_id, KeyStoreOptions::offline());
        assert_eq!(store.current_key_id(), Some(r.key_id));
        assert!(store.key(r.key_id).is_some());
        assert!(store.key(Uuid::now_v7()).is_none());
        assert!(store.notes().is_empty(), "{:?}", store.notes());
        let provider = provider_with(&sb.locator, db_id, KeyStoreOptions::offline()).unwrap();
        assert_eq!(provider.current().map(|(id, _)| id), Some(r.key_id));
        // Another database has no key.
        assert!(provider_with(&sb.locator, Uuid::now_v7(), KeyStoreOptions::offline()).is_none());
    }

    #[test]
    fn init_without_keyring_falls_back_to_a_key_file_and_says_why() {
        let sb = sandbox();
        let r = init(&sb.locator, Uuid::now_v7(), &offline_init(false)).unwrap();
        assert!(matches!(r.source, KeySource::KeyFile(_)));
        assert!(r.reason.contains("disabled"), "{}", r.reason);
    }

    #[test]
    fn explicit_key_file_is_current_and_the_ring_merges_sources() {
        let sb = sandbox();
        let db_id = Uuid::now_v7();
        let default = init(&sb.locator, db_id, &offline_init(true)).unwrap();
        let explicit_path = sb.locator.paths.data_dir.join("portable.key");
        let explicit = blobs::random_master_key().unwrap();
        blobs::write_key_file(&explicit_path, &explicit).unwrap();
        let store = KeyStore::open_with(
            &sb.locator,
            db_id,
            KeyStoreOptions {
                key_file: Some(explicit_path.clone()),
                ..KeyStoreOptions::offline()
            },
        );
        assert_eq!(store.current_key_id(), Some(blobs::key_id_for(&explicit)));
        assert_eq!(store.source(), &KeySource::KeyFile(explicit_path));
        assert!(
            store.key(default.key_id).is_some(),
            "default key still decrypts"
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_readable_key_file_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let sb = sandbox();
        let db_id = Uuid::now_v7();
        let r = init(&sb.locator, db_id, &offline_init(true)).unwrap();
        let path = default_key_file(&sb.locator, db_id);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let store = KeyStore::open_with(&sb.locator, db_id, KeyStoreOptions::offline());
        assert!(!store.has_key());
        assert!(store.key(r.key_id).is_none());
        assert!(
            store.notes().iter().any(|n| n.contains("0600")),
            "{:?}",
            store.notes()
        );
    }

    #[test]
    fn passphrase_key_is_deterministic_per_database() {
        let a = master_from_passphrase_with("correct horse", Uuid::nil(), 1024, 1).unwrap();
        let b = master_from_passphrase_with("correct horse", Uuid::nil(), 1024, 1).unwrap();
        let c = master_from_passphrase_with("correct horse", Uuid::max(), 1024, 1).unwrap();
        let d = master_from_passphrase_with("wrong horse", Uuid::nil(), 1024, 1).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    #[test]
    fn passphrase_source_provides_a_current_key() {
        let sb = sandbox();
        let db_id = Uuid::now_v7();
        let opts = KeyStoreOptions {
            passphrase: Some("hunter2".into()),
            ..KeyStoreOptions::offline()
        };
        let store = KeyStore::open_with(&sb.locator, db_id, opts.clone());
        assert_eq!(store.source(), &KeySource::Passphrase);
        let id = store.current_key_id().unwrap();
        assert_eq!(
            id,
            blobs::key_id_for(&master_from_passphrase("hunter2", db_id).unwrap())
        );
        let again = KeyStore::open_with(&sb.locator, db_id, opts);
        assert_eq!(again.current_key_id(), Some(id));
        // A passphrase key cannot be rotated in place.
        let db_dir = sb.locator.paths.data_dir.join("db");
        Database::create(&db_dir, DeviceId::new()).unwrap();
        let identity = Identity::load(&db_dir).unwrap();
        let err = rotate(
            &sb.locator,
            &db_dir,
            Some(KeyStoreOptions {
                passphrase: Some("hunter2".into()),
                ..KeyStoreOptions::offline()
            }),
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("passphrase"), "{err}");
        let _ = identity;
    }

    #[test]
    fn export_master_writes_a_0600_hex_file_once() {
        let sb = sandbox();
        let db_id = Uuid::now_v7();
        let r = init(&sb.locator, db_id, &offline_init(true)).unwrap();
        let out = sb.locator.paths.data_dir.join("exported.key");
        let id = export_master(&sb.locator, db_id, Some(KeyStoreOptions::offline()), &out).unwrap();
        assert_eq!(id, r.key_id);
        let key = blobs::read_key_file(&out).unwrap();
        assert_eq!(blobs::key_id_for(&key), r.key_id);
        assert!(
            export_master(&sb.locator, db_id, Some(KeyStoreOptions::offline()), &out).is_err(),
            "must not overwrite"
        );
    }

    fn event_with_content(dev: DeviceId, i: usize) -> Event {
        let mut ev = Event::new(
            dev,
            Provider::ClaudeCode,
            "PostToolUse",
            EventKind::ToolCallFinished,
            ProjectRef::derive("/p", None, &dev),
            "s1",
            CaptureMode::LocalSemantic,
            "t",
        );
        ev.content = Some(EventContent {
            command: Some(format!("echo secret-{i}")),
            ..Default::default()
        });
        ev.raw = Some(serde_json::json!({"i": i}));
        ev
    }

    #[test]
    fn rotate_reencrypts_every_blob_and_forgets_the_old_key() {
        let sb = sandbox();
        let db_dir = sb.locator.paths.data_dir.join("db").join(".attemptdb");
        let dev = DeviceId::new();
        Database::create(&db_dir, dev).unwrap();
        let db_id = Identity::load(&db_dir).unwrap().db_id;
        let first = init(&sb.locator, db_id, &offline_init(true)).unwrap();
        let provider = provider_with(&sb.locator, db_id, KeyStoreOptions::offline()).unwrap();
        {
            let mut db = Database::open(
                &db_dir,
                OpenOptions {
                    keys: Some(provider.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
            db.ingest((0..3).map(|i| event_with_content(dev, i)).collect())
                .unwrap();
            db.flush().unwrap();
            assert_eq!(db.blob_stats().unwrap().count, 6);
        }
        let before = status(&sb.locator, &db_dir, Some(KeyStoreOptions::offline())).unwrap();
        assert_eq!(before.key_id, Some(first.key_id));
        assert_eq!(before.blob_key_ids, vec![first.key_id]);
        assert!(before.missing_key_ids.is_empty());
        assert_eq!(before.blobs, 6);
        assert_eq!(before.segments, 1);
        assert_eq!(before.inline_segments, 0);

        let report = rotate(&sb.locator, &db_dir, Some(KeyStoreOptions::offline()), true).unwrap();
        assert_eq!(report.old_key_id, first.key_id);
        assert_ne!(report.new_key_id, first.key_id);
        assert_eq!(report.rewritten, 6);
        assert_eq!(report.skipped, 0);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(report.forgot_old);
        let retained = retained_key_file(&default_key_file(&sb.locator, db_id), first.key_id);
        assert!(!retained.exists(), "old key forgotten");

        // A fresh store only holds the new key, and everything still reads.
        let after = status(&sb.locator, &db_dir, Some(KeyStoreOptions::offline())).unwrap();
        assert_eq!(after.key_id, Some(report.new_key_id));
        assert_eq!(after.blob_key_ids, vec![report.new_key_id]);
        assert!(after.missing_key_ids.is_empty());
        let provider = provider_with(&sb.locator, db_id, KeyStoreOptions::offline()).unwrap();
        assert!(provider.key(first.key_id).is_none());
        let db = Database::open(
            &db_dir,
            OpenOptions {
                read_only: true,
                keys: Some(provider),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        let events = db.scan(&ScanFilter::default()).unwrap();
        assert_eq!(events.len(), 3);
        for (i, ev) in events.iter().enumerate() {
            assert_eq!(
                ev.content.as_ref().unwrap().command.as_deref(),
                Some(format!("echo secret-{i}").as_str())
            );
            assert_eq!(ev.raw, Some(serde_json::json!({"i": i})));
        }
        assert!(db.content_warnings().is_empty());
    }

    #[test]
    fn rotate_without_forget_keeps_the_old_key_readable() {
        let sb = sandbox();
        let db_dir = sb.locator.paths.data_dir.join("db").join(".attemptdb");
        Database::create(&db_dir, DeviceId::new()).unwrap();
        let db_id = Identity::load(&db_dir).unwrap().db_id;
        let first = init(&sb.locator, db_id, &offline_init(true)).unwrap();
        let report = rotate(
            &sb.locator,
            &db_dir,
            Some(KeyStoreOptions::offline()),
            false,
        )
        .unwrap();
        assert!(!report.forgot_old);
        assert_eq!(report.rewritten, 0);
        let store = KeyStore::open_with(&sb.locator, db_id, KeyStoreOptions::offline());
        assert_eq!(store.current_key_id(), Some(report.new_key_id));
        assert!(store.holds(first.key_id), "retained key found on demand");
    }

    #[test]
    fn provider_for_mode_respects_the_policy() {
        let sb = sandbox();
        let db_id = Uuid::now_v7();
        let off = KeyStoreOptions::offline();
        assert!(
            provider_for_mode(&sb.locator, db_id, EncryptionMode::Auto, Some(off.clone()))
                .unwrap()
                .is_none()
        );
        assert!(
            provider_for_mode(
                &sb.locator,
                db_id,
                EncryptionMode::Required,
                Some(off.clone())
            )
            .is_err()
        );
        init(&sb.locator, db_id, &offline_init(true)).unwrap();
        assert!(
            provider_for_mode(&sb.locator, db_id, EncryptionMode::Off, Some(off.clone()))
                .unwrap()
                .is_none()
        );
        assert!(
            provider_for_mode(&sb.locator, db_id, EncryptionMode::Required, Some(off))
                .unwrap()
                .is_some()
        );
    }

    /// Touches the real OS key store; opt in with `ATTEMPTDB_TEST_KEYRING=1`.
    #[test]
    fn keyring_roundtrip_opt_in() {
        if std::env::var("ATTEMPTDB_TEST_KEYRING").as_deref() != Ok("1") {
            eprintln!("skipped: set ATTEMPTDB_TEST_KEYRING=1 to exercise the OS key store");
            return;
        }
        let sb = sandbox();
        let db_id = Uuid::now_v7();
        let opts = KeyStoreOptions {
            key_file: None,
            use_keyring: true,
            passphrase: None,
        };
        let r = init(
            &sb.locator,
            db_id,
            &InitOptions {
                key_file: false,
                passphrase_env: None,
                store: Some(opts.clone()),
            },
        )
        .unwrap();
        assert_eq!(r.source, KeySource::Keyring, "{}", r.reason);
        let store = KeyStore::open_with(&sb.locator, db_id, opts);
        assert_eq!(store.current_key_id(), Some(r.key_id));
        keyring_delete(&keyring_account(db_id)).unwrap();
        assert!(keyring_get(&keyring_account(db_id)).unwrap().is_none());
    }
}
