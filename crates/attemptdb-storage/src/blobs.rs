//! Encrypted content blobs: where `content` and `raw` live once a segment is
//! written with an encryption key (RFC 0006 §7, `docs/storage-format.md`
//! §"Blobs").
//!
//! ```text
//! blobs/<first 2 hex of blob_id>/<blob_id>.blob
//!
//! offset  size  field
//! 0       4     magic "ATBL"
//! 4       2     format_version   u16 LE = 1
//! 6       16    key_id           UUID bytes of the master key that wraps this blob
//! 22      24    nonce            XChaCha20-Poly1305 nonce, random per blob
//! 46      4     plaintext_len    u32 LE
//! 50      4     ciphertext_len   u32 LE = plaintext_len + 16 (Poly1305 tag)
//! 54      n     ciphertext
//! 54+n    4     crc32c           u32 LE over bytes [0, 54+n)
//! ```
//!
//! - `blob_id = hex(HMAC-SHA256(key = hash_key, msg = plaintext))`: keyed, so
//!   a blob name never reveals a plaintext hash, while identical plaintext
//!   under the same key deduplicates to one file.
//! - `enc_key` and `hash_key` are derived from the 32-byte *master key* with
//!   HKDF-SHA256 (no salt; info `attemptdb/blob-enc/v1` and
//!   `attemptdb/blob-hash/v1`).
//! - AAD = `blob_id ‖ db_id ‖ device_id` (32 + 16 + 16 raw bytes), so a blob
//!   copied into another database does not decrypt there.
//! - The trailing CRC-32C catches ordinary corruption without a key; the
//!   AEAD tag catches everything else.
//! - Blobs are immutable and written via temp file + atomic rename.
//!   [`BlobStore::write`] and rotation make each blob durable on its own
//!   (file fsync + directory fsync). A segment write goes through a
//!   [`BlobSink`], which writes its blobs without any durability work and
//!   then makes them all durable in one barrier ([`BlobSink::sync_pending`],
//!   called by the segment writer before the segment file is written, so
//!   the order blobs -> segment -> manifest holds exactly as before).
//!   Re-encryption (key rotation, portable snapshots) keeps the blob
//!   id: the id is assigned once from the key that first wrote the blob and
//!   is bound into the AAD, not recomputed from the new key.
//!
//! The storage crate never touches an OS key store: it asks a
//! [`KeyProvider`] for master keys by [`KeyId`] and for the key new blobs
//! are written under.

use crate::format::{
    BLOB_FORMAT_VERSION, BLOB_HEADER_LEN, BLOB_TRAILER_LEN, BLOBS_DIR, MAGIC_BLOB,
    MAX_BLOB_PLAINTEXT, u16_le, u32_le,
};
use crate::{IoAt, Result, StorageError};
use attemptdb_core::DeviceId;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use uuid::{Builder, Uuid, Variant, Version};
use zeroize::Zeroizing;

/// Identity of a master key. Derived from the key itself (see
/// [`key_id_for`]), so any holder of the key material can name it.
pub type KeyId = Uuid;
/// A 32-byte master key.
pub type MasterKey = [u8; 32];

/// Poly1305 tag length.
pub const TAG_LEN: usize = 16;
/// XChaCha20 nonce length.
pub const NONCE_LEN: usize = 24;
/// HKDF info string for the encryption key.
pub const INFO_ENC: &[u8] = b"attemptdb/blob-enc/v1";
/// HKDF info string for the keyed-hash key.
pub const INFO_HASH: &[u8] = b"attemptdb/blob-hash/v1";
/// HKDF info string for the key id fingerprint.
pub const INFO_KEY_ID: &[u8] = b"attemptdb/key-id/v1";
/// AAD length: blob id (32) + db id (16) + device id (16).
pub const AAD_LEN: usize = 32 + 16 + 16;

/// Source of master keys. Implementations live outside the storage crate
/// (OS key stores, key files, passphrases); [`StaticKeyProvider`] is the
/// in-memory one used by tests, rotation, and portable exports.
pub trait KeyProvider: Send + Sync {
    /// The master key with this id, if held.
    fn key(&self, key_id: KeyId) -> Option<MasterKey>;
    /// The key new blobs are written under. `None` means "write inline".
    fn current(&self) -> Option<(KeyId, MasterKey)>;
}

impl fmt::Debug for dyn KeyProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.current() {
            Some((id, _)) => write!(f, "KeyProvider(current key_id {id})"),
            None => write!(f, "KeyProvider(no current key)"),
        }
    }
}

/// A fresh random master key from the OS CSPRNG.
pub fn random_master_key() -> Result<MasterKey> {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key)
        .map_err(|e| StorageError::Other(format!("random source unavailable: {e}")))?;
    Ok(key)
}

/// The id of a master key: the first 16 bytes of
/// `HKDF-SHA256(master, info = "attemptdb/key-id/v1")`, formatted as an
/// RFC 9562 custom-version UUID. One-way, so the id reveals nothing about
/// the key; deterministic, so a key file or passphrase that carries no
/// metadata can still answer [`KeyProvider::key`].
pub fn key_id_for(master: &MasterKey) -> KeyId {
    let mut out = [0u8; 16];
    Hkdf::<Sha256>::new(None, master)
        .expand(INFO_KEY_ID, &mut out)
        .expect("16 bytes is a valid HKDF-SHA256 output length");
    Builder::from_bytes(out)
        .with_variant(Variant::RFC4122)
        .with_version(Version::Custom)
        .into_uuid()
}

/// Keys derived from one master key. Zeroed on drop.
pub struct DerivedKeys {
    enc: Zeroizing<[u8; 32]>,
    hash: Zeroizing<[u8; 32]>,
}

impl DerivedKeys {
    pub fn from_master(master: &MasterKey) -> Self {
        let hk = Hkdf::<Sha256>::new(None, master);
        let mut enc = Zeroizing::new([0u8; 32]);
        let mut hash = Zeroizing::new([0u8; 32]);
        hk.expand(INFO_ENC, enc.as_mut())
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        hk.expand(INFO_HASH, hash.as_mut())
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Self { enc, hash }
    }

    pub fn enc(&self) -> &[u8; 32] {
        &self.enc
    }

    pub fn hash(&self) -> &[u8; 32] {
        &self.hash
    }
}

/// In-memory key ring. Build it, then share it behind an `Arc`.
#[derive(Default)]
pub struct StaticKeyProvider {
    current: Option<KeyId>,
    keys: HashMap<KeyId, Zeroizing<MasterKey>>,
}

impl StaticKeyProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// A ring holding one key that is also the current key.
    pub fn with_current(master: MasterKey) -> Self {
        let mut p = Self::new();
        p.set_current(master);
        p
    }

    /// Add a key without making it current. Returns its id.
    pub fn add(&mut self, master: MasterKey) -> KeyId {
        let id = key_id_for(&master);
        self.keys.insert(id, Zeroizing::new(master));
        id
    }

    /// Add a key and make it the one new blobs are written under.
    pub fn set_current(&mut self, master: MasterKey) -> KeyId {
        let id = self.add(master);
        self.current = Some(id);
        id
    }

    pub fn remove(&mut self, key_id: KeyId) {
        self.keys.remove(&key_id);
        if self.current == Some(key_id) {
            self.current = None;
        }
    }

    pub fn key_ids(&self) -> Vec<KeyId> {
        let mut ids: Vec<KeyId> = self.keys.keys().copied().collect();
        ids.sort();
        ids
    }
}

impl KeyProvider for StaticKeyProvider {
    fn key(&self, key_id: KeyId) -> Option<MasterKey> {
        self.keys.get(&key_id).map(|k| **k)
    }

    fn current(&self) -> Option<(KeyId, MasterKey)> {
        let id = self.current?;
        Some((id, **self.keys.get(&id)?))
    }
}

/// Content address of a blob: HMAC-SHA256 of the plaintext under the
/// writer's hash key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobId([u8; 32]);

impl BlobId {
    pub fn compute(hash_key: &[u8; 32], plaintext: &[u8]) -> Self {
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(hash_key).expect("HMAC accepts any key length");
        mac.update(plaintext);
        Self(mac.finalize().into_bytes().into())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parse 64 lowercase hex characters.
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        let v = hex::decode(s).ok()?;
        let mut out = [0u8; 32];
        out.copy_from_slice(&v);
        Some(Self(out))
    }

    /// Two-character shard directory (first byte in hex).
    pub fn shard(&self) -> String {
        hex::encode(&self.0[..1])
    }

    pub fn file_name(&self) -> String {
        format!("{}.blob", self.to_hex())
    }

    /// Parse `<64 hex>.blob`.
    pub fn from_file_name(name: &str) -> Option<Self> {
        name.strip_suffix(".blob").and_then(Self::from_hex)
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlobId({})", self.to_hex())
    }
}

/// The fixed header of a blob file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobHeader {
    pub key_id: KeyId,
    pub nonce: [u8; NONCE_LEN],
    pub plaintext_len: u32,
    pub ciphertext_len: u32,
}

fn corrupt(path: &Path, detail: impl Into<String>) -> StorageError {
    StorageError::Corrupt {
        what: "blob",
        path: path.to_path_buf(),
        detail: detail.into(),
    }
}

/// Encode one blob file: header, ciphertext, trailing CRC.
pub fn encode(
    id: &BlobId,
    key_id: KeyId,
    enc_key: &[u8; 32],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    if plaintext.len() > MAX_BLOB_PLAINTEXT as usize {
        return Err(StorageError::Other(format!(
            "blob {id} plaintext is {} bytes; the limit is {MAX_BLOB_PLAINTEXT}",
            plaintext.len()
        )));
    }
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce)
        .map_err(|e| StorageError::Other(format!("random source unavailable: {e}")))?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(enc_key));
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| StorageError::Other(format!("blob {id}: encryption failed")))?;
    let mut out = Vec::with_capacity(BLOB_HEADER_LEN + ciphertext.len() + BLOB_TRAILER_LEN);
    out.extend_from_slice(&MAGIC_BLOB);
    out.extend_from_slice(&BLOB_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(key_id.as_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&(plaintext.len() as u32).to_le_bytes());
    out.extend_from_slice(&(ciphertext.len() as u32).to_le_bytes());
    out.extend_from_slice(&ciphertext);
    let crc = crc32c::crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    Ok(out)
}

/// Parse a header from at least the first [`BLOB_HEADER_LEN`] bytes
/// (structure only; no CRC check, which needs the whole file).
pub fn parse_header(bytes: &[u8], path: &Path) -> Result<BlobHeader> {
    if bytes.len() < BLOB_HEADER_LEN {
        return Err(corrupt(path, "shorter than the blob header"));
    }
    if bytes[0..4] != MAGIC_BLOB {
        return Err(corrupt(path, "bad magic"));
    }
    let version = u16_le(&bytes[4..6]);
    if version != BLOB_FORMAT_VERSION {
        return Err(StorageError::UnsupportedFormat {
            what: "blob",
            found: version,
            supported: BLOB_FORMAT_VERSION,
        });
    }
    let mut key_id = [0u8; 16];
    key_id.copy_from_slice(&bytes[6..22]);
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&bytes[22..46]);
    let plaintext_len = u32_le(&bytes[46..50]);
    let ciphertext_len = u32_le(&bytes[50..54]);
    if plaintext_len > MAX_BLOB_PLAINTEXT
        || ciphertext_len as usize != plaintext_len as usize + TAG_LEN
    {
        return Err(corrupt(path, "inconsistent lengths in header"));
    }
    Ok(BlobHeader {
        key_id: Uuid::from_bytes(key_id),
        nonce,
        plaintext_len,
        ciphertext_len,
    })
}

/// Parse and CRC-check a whole blob file. Returns the header and the
/// ciphertext slice.
pub fn parse<'a>(bytes: &'a [u8], path: &Path) -> Result<(BlobHeader, &'a [u8])> {
    let header = parse_header(bytes, path)?;
    let expected_len = BLOB_HEADER_LEN + header.ciphertext_len as usize + BLOB_TRAILER_LEN;
    if bytes.len() != expected_len {
        return Err(corrupt(
            path,
            format!(
                "file is {} bytes, header implies {expected_len}",
                bytes.len()
            ),
        ));
    }
    let body = &bytes[..expected_len - BLOB_TRAILER_LEN];
    let stored = u32_le(&bytes[expected_len - BLOB_TRAILER_LEN..]);
    if crc32c::crc32c(body) != stored {
        return Err(corrupt(path, "crc32c mismatch"));
    }
    Ok((header, &body[BLOB_HEADER_LEN..]))
}

/// Decrypt a parsed blob. A failed tag is corruption, never partial output.
pub fn decrypt(
    header: &BlobHeader,
    ciphertext: &[u8],
    enc_key: &[u8; 32],
    aad: &[u8],
    path: &Path,
) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(enc_key));
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&header.nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| corrupt(path, "authentication failed (wrong key or tampered)"))?;
    if plaintext.len() != header.plaintext_len as usize {
        return Err(corrupt(path, "plaintext length does not match header"));
    }
    Ok(plaintext)
}

/// One blob on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobEntry {
    pub id: BlobId,
    /// File size in bytes.
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct BlobStats {
    pub count: u64,
    pub bytes: u64,
}

/// Outcome of re-encrypting every blob under a new key.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct RotationReport {
    /// Blobs rewritten under the new key.
    pub rewritten: u64,
    /// Blobs that already used the new key.
    pub skipped: u64,
    /// Blobs that could not be rewritten (missing key, corrupt), with why.
    pub failed: Vec<String>,
}

/// The blob directory of one database.
#[derive(Clone, Debug)]
pub struct BlobStore {
    root: PathBuf,
    db_id: Uuid,
    device_id: DeviceId,
    sync: Arc<SyncState>,
}

/// Durability bookkeeping shared by every clone of one [`BlobStore`] (the
/// database handle and the sinks it creates per flush).
#[derive(Debug, Default)]
struct SyncState {
    /// Blobs this process knows are on stable storage (written and
    /// barriered, or written durably). A dedupe hit on any other existing
    /// blob joins the next barrier: it may be the unsynced leftover of a
    /// flush that crashed before its barrier.
    durable: Mutex<HashSet<BlobId>>,
    file_syncs: AtomicU64,
    dir_syncs: AtomicU64,
}

/// More than this many ids and the durable set is dropped (the cost is a
/// few redundant fsyncs, never correctness).
const DURABLE_SET_MAX: usize = 1 << 18;
/// Existing blobs up to this size are fully CRC-checked before a dedupe hit
/// is trusted; larger ones are checked by size and header only.
const DEDUP_VERIFY_MAX: u64 = 1 << 20;

impl BlobStore {
    /// `root` is the database directory; `db_id`/`device_id` are bound into
    /// every blob's AAD and must be the identity of the database whose
    /// segments reference the blobs.
    pub fn new(root: &Path, db_id: Uuid, device_id: DeviceId) -> Self {
        Self {
            root: root.to_path_buf(),
            db_id,
            device_id,
            sync: Arc::new(SyncState::default()),
        }
    }

    /// How many file and directory fsyncs blob writes through this store
    /// (and its clones) have issued: `(files, directories)`. For tests and
    /// diagnostics.
    pub fn sync_counts(&self) -> (u64, u64) {
        (
            self.sync.file_syncs.load(Ordering::Relaxed),
            self.sync.dir_syncs.load(Ordering::Relaxed),
        )
    }

    fn is_known_durable(&self, id: &BlobId) -> bool {
        self.sync
            .durable
            .lock()
            .map(|d| d.contains(id))
            .unwrap_or(false)
    }

    fn mark_durable(&self, ids: impl IntoIterator<Item = BlobId>) {
        if let Ok(mut d) = self.sync.durable.lock() {
            if d.len() > DURABLE_SET_MAX {
                d.clear();
            }
            d.extend(ids);
        }
    }

    /// Whether the blob file for `id` is present and complete for a
    /// plaintext of `plaintext_len` bytes: the exact size the format
    /// implies, and for files up to 1 MiB a valid CRC. A partially written
    /// or damaged file fails this and is rewritten instead of being trusted
    /// by dedupe forever.
    fn is_intact(&self, id: &BlobId, plaintext_len: usize) -> bool {
        let path = self.path(id);
        let Ok(meta) = std::fs::metadata(&path) else {
            return false;
        };
        let expected = BLOB_HEADER_LEN + plaintext_len + TAG_LEN + BLOB_TRAILER_LEN;
        if !meta.is_file() || meta.len() != expected as u64 {
            return false;
        }
        if meta.len() <= DEDUP_VERIFY_MAX {
            match std::fs::read(&path) {
                Ok(bytes) => parse(&bytes, &path)
                    .is_ok_and(|(h, _)| h.plaintext_len as usize == plaintext_len),
                Err(_) => false,
            }
        } else {
            read_header_at(&path).is_ok_and(|h| h.plaintext_len as usize == plaintext_len)
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db_id(&self) -> Uuid {
        self.db_id
    }

    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    pub fn dir(&self) -> PathBuf {
        self.root.join(BLOBS_DIR)
    }

    pub fn path(&self, id: &BlobId) -> PathBuf {
        self.dir().join(id.shard()).join(id.file_name())
    }

    pub fn exists(&self, id: &BlobId) -> bool {
        self.path(id).is_file()
    }

    /// Additional authenticated data for `id` in this database.
    pub fn aad(&self, id: &BlobId) -> [u8; AAD_LEN] {
        let mut aad = [0u8; AAD_LEN];
        aad[..32].copy_from_slice(id.as_bytes());
        aad[32..48].copy_from_slice(self.db_id.as_bytes());
        aad[48..].copy_from_slice(self.device_id.as_bytes());
        aad
    }

    /// Encrypt `plaintext` under `key_id`/`keys` and store it durably.
    /// Returns the blob id. An existing blob with that id is reused (dedupe)
    /// only when it is complete (size and CRC verified); a truncated or
    /// damaged file is replaced atomically.
    pub fn write(&self, key_id: KeyId, keys: &DerivedKeys, plaintext: &[u8]) -> Result<BlobId> {
        let id = BlobId::compute(keys.hash(), plaintext);
        if self.is_intact(&id, plaintext.len()) {
            return Ok(id);
        }
        let bytes = encode(&id, key_id, keys.enc(), &self.aad(&id), plaintext)?;
        self.publish(&id, &bytes)?;
        Ok(id)
    }

    /// Write encoded blob bytes with no durability work: temp file, atomic
    /// rename. The caller owns making it durable (see [`BlobSink`]).
    fn publish_unsynced(&self, id: &BlobId, bytes: &[u8]) -> Result<()> {
        let path = self.path(id);
        let dir = path.parent().expect("blob path has a shard directory");
        std::fs::create_dir_all(dir).at(dir)?;
        let tmp = dir.join(format!("{}.tmp", id.file_name()));
        let mut f = crate::safe_fs::create_new_replacing(&tmp).at(&tmp)?;
        f.write_all(bytes).at(&tmp)?;
        drop(f);
        crate::manifest::rename_replacing(&tmp, &path)
    }

    /// Store already-encoded blob bytes under `id` (rotation, snapshot
    /// extraction). Replaces an existing file atomically.
    pub fn publish(&self, id: &BlobId, bytes: &[u8]) -> Result<()> {
        let path = self.path(id);
        let dir = path.parent().expect("blob path has a shard directory");
        std::fs::create_dir_all(dir).at(dir)?;
        let tmp = dir.join(format!("{}.tmp", id.file_name()));
        crate::manifest::write_tmp_synced(&tmp, bytes, None)?;
        crate::manifest::publish_tmp(&tmp, &path)?;
        self.sync.file_syncs.fetch_add(1, Ordering::Relaxed);
        self.sync.dir_syncs.fetch_add(1, Ordering::Relaxed);
        self.mark_durable([*id]);
        Ok(())
    }

    /// The raw bytes of a blob file (no parsing).
    pub fn read_raw(&self, id: &BlobId) -> Result<Vec<u8>> {
        let path = self.path(id);
        std::fs::read(&path).at(&path)
    }

    /// Read just the header (cheap: 54 bytes).
    pub fn header(&self, id: &BlobId) -> Result<BlobHeader> {
        let path = self.path(id);
        read_header_at(&path)
    }

    /// Structure + CRC check without a key.
    pub fn verify(&self, id: &BlobId) -> Result<BlobHeader> {
        let path = self.path(id);
        let bytes = std::fs::read(&path).at(&path)?;
        parse(&bytes, &path).map(|(h, _)| h)
    }

    /// Decrypt a blob with a key from `provider`.
    pub fn read(&self, provider: &dyn KeyProvider, id: &BlobId) -> Result<Vec<u8>> {
        let path = self.path(id);
        let bytes = std::fs::read(&path).at(&path)?;
        let (header, ciphertext) = parse(&bytes, &path)?;
        let master = provider.key(header.key_id).ok_or(StorageError::NoKey {
            key_id: header.key_id,
        })?;
        let keys = DerivedKeys::from_master(&master);
        decrypt(&header, ciphertext, keys.enc(), &self.aad(id), &path)
    }

    /// Every blob on disk, sorted by id.
    pub fn list(&self) -> Result<Vec<BlobEntry>> {
        let dir = self.dir();
        let mut out = Vec::new();
        if !dir.is_dir() {
            return Ok(out);
        }
        for shard in std::fs::read_dir(&dir).at(&dir)? {
            let shard = shard.at(&dir)?.path();
            if !shard.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&shard).at(&shard)? {
                let entry = entry.at(&shard)?;
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(id) = BlobId::from_file_name(&name) {
                    let bytes = entry.metadata().at(&entry.path())?.len();
                    out.push(BlobEntry { id, bytes });
                }
            }
        }
        out.sort_by_key(|a| a.id);
        Ok(out)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.list()?.is_empty())
    }

    pub fn stats(&self) -> Result<BlobStats> {
        let list = self.list()?;
        Ok(BlobStats {
            count: list.len() as u64,
            bytes: list.iter().map(|e| e.bytes).sum(),
        })
    }

    /// Key ids seen in the header of one blob of every shard directory (at
    /// most 256 small reads). Exact once a rotation has completed; a good
    /// approximation while one is in progress.
    ///
    /// A shard is read lazily and the first blob whose header reads is the
    /// sample: listing and sorting a whole shard (16,000 files each on a
    /// database of 4 million blobs) cost more than 30 s on every open and
    /// bought nothing, since which blob of a shard is sampled never mattered.
    pub fn sample_key_ids(&self) -> Result<BTreeSet<KeyId>> {
        let dir = self.dir();
        let mut out = BTreeSet::new();
        if !dir.is_dir() {
            return Ok(out);
        }
        for shard in std::fs::read_dir(&dir).at(&dir)? {
            let shard = shard.at(&dir)?.path();
            if !shard.is_dir() {
                continue;
            }
            // `read_dir` yields entries in directory order, a batch at a
            // time: stopping at the first usable one reads one batch.
            let sample = std::fs::read_dir(&shard)
                .at(&shard)?
                .filter_map(|e| e.ok())
                .filter(|e| BlobId::from_file_name(&e.file_name().to_string_lossy()).is_some())
                .find_map(|e| read_header_at(&e.path()).ok());
            if let Some(h) = sample {
                out.insert(h.key_id);
            }
        }
        Ok(out)
    }

    /// Key ids in the header of every blob (reads every file's header).
    pub fn all_key_ids(&self) -> Result<BTreeSet<KeyId>> {
        let mut out = BTreeSet::new();
        for e in self.list()? {
            out.insert(self.header(&e.id)?.key_id);
        }
        Ok(out)
    }

    /// Rewrite one blob under `new_key_id`, keeping its id. Returns `false`
    /// when it already used that key.
    pub fn reencrypt(
        &self,
        provider: &dyn KeyProvider,
        id: &BlobId,
        new_key_id: KeyId,
        new_keys: &DerivedKeys,
    ) -> Result<bool> {
        let path = self.path(id);
        let bytes = std::fs::read(&path).at(&path)?;
        let (header, ciphertext) = parse(&bytes, &path)?;
        if header.key_id == new_key_id {
            return Ok(false);
        }
        let master = provider.key(header.key_id).ok_or(StorageError::NoKey {
            key_id: header.key_id,
        })?;
        let old = DerivedKeys::from_master(&master);
        let aad = self.aad(id);
        let plaintext = Zeroizing::new(decrypt(&header, ciphertext, old.enc(), &aad, &path)?);
        let encoded = encode(id, new_key_id, new_keys.enc(), &aad, &plaintext)?;
        self.publish(id, &encoded)?;
        Ok(true)
    }

    /// Rewrite every blob not yet under `new_key_id`. One blob at a time,
    /// each replaced atomically, so an interrupted run leaves a mix of old
    /// and new wrappings that both keys can still read; running again
    /// finishes the job.
    pub fn reencrypt_all(
        &self,
        provider: &dyn KeyProvider,
        new_key_id: KeyId,
        new_master: &MasterKey,
    ) -> Result<RotationReport> {
        let new_keys = DerivedKeys::from_master(new_master);
        let mut report = RotationReport::default();
        for entry in self.list()? {
            match self.reencrypt(provider, &entry.id, new_key_id, &new_keys) {
                Ok(true) => report.rewritten += 1,
                Ok(false) => report.skipped += 1,
                Err(e) => report.failed.push(format!("blob {}: {e}", entry.id)),
            }
        }
        Ok(report)
    }
}

/// Write a master key to `path` as 64 lowercase hex characters plus a
/// newline. The file is created exclusively (an existing file is never
/// overwritten) with mode `0600` on Unix; on Windows the file inherits the
/// directory's ACL (keep it under the user profile). The directory is
/// created with mode `0700` on Unix when missing.
pub fn write_key_file(path: &Path, master: &MasterKey) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).at(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            StorageError::Other(format!(
                "key file {} already exists; refusing to overwrite key material",
                path.display()
            ))
        } else {
            StorageError::io(path, e)
        }
    })?;
    let hex = Zeroizing::new(format!("{}\n", hex::encode(master)));
    f.write_all(hex.as_bytes()).at(path)?;
    f.sync_all().at(path)?;
    Ok(())
}

/// Read a key file written by [`write_key_file`] (or by hand: 64 hex
/// characters, surrounding whitespace ignored). On Unix the file must not
/// be readable by group or others.
pub fn read_key_file(path: &Path) -> Result<MasterKey> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).at(path)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(StorageError::Other(format!(
                "key file {} has mode {mode:03o}; it must be 0600 (chmod 600)",
                path.display()
            )));
        }
    }
    let text = Zeroizing::new(std::fs::read_to_string(path).at(path)?);
    let trimmed = text.trim();
    let bytes = hex::decode(trimmed).ok().filter(|b| b.len() == 32);
    match bytes {
        Some(b) => {
            let mut key = [0u8; 32];
            key.copy_from_slice(&b);
            Ok(key)
        }
        None => Err(StorageError::Other(format!(
            "key file {} is not 64 hex characters",
            path.display()
        ))),
    }
}

fn read_header_at(path: &Path) -> Result<BlobHeader> {
    let mut f = std::fs::File::open(path).at(path)?;
    let mut buf = [0u8; BLOB_HEADER_LEN];
    let mut filled = 0;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..]).at(path)?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    parse_header(&buf[..filled], path)
}

/// Writer-side handle: the key new blobs go under plus the store.
///
/// `put` writes a blob with no durability work. [`BlobSink::sync_pending`] is
/// the single barrier that makes everything put so far durable: one fsync per
/// new blob file, one per touched shard directory, one each for `blobs/` and
/// the database root. Callers must run it before anything that refers to the
/// blobs becomes durable (the segment writer does, before the segment file).
/// Dropping a sink without the barrier leaves unreferenced blob files, which
/// are harmless: the events are still in the WAL.
pub struct BlobSink {
    store: BlobStore,
    key_id: KeyId,
    keys: DerivedKeys,
    pending: Mutex<PendingBlobs>,
}

#[derive(Default)]
struct PendingBlobs {
    /// Every id this sink has handled (written, or found intact).
    seen: HashSet<BlobId>,
    /// Blobs that still need to reach stable storage.
    unsynced: Vec<BlobId>,
}

impl BlobSink {
    pub fn new(store: BlobStore, key_id: KeyId, master: &MasterKey) -> Self {
        Self {
            store,
            key_id,
            keys: DerivedKeys::from_master(master),
            pending: Mutex::new(PendingBlobs::default()),
        }
    }

    pub fn key_id(&self) -> KeyId {
        self.key_id
    }

    pub fn store(&self) -> &BlobStore {
        &self.store
    }

    /// Store `plaintext` as a blob (deduplicated) and return its id. Not
    /// durable until [`BlobSink::sync_pending`] returns.
    pub fn put(&self, plaintext: &[u8]) -> Result<BlobId> {
        let id = BlobId::compute(self.keys.hash(), plaintext);
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending.seen.contains(&id) {
            return Ok(id);
        }
        if self.store.is_intact(&id, plaintext.len()) {
            // Already on disk. Unless this process knows it reached stable
            // storage, it may be the unsynced remains of a flush that
            // crashed before its barrier, so it joins this one.
            if !self.store.is_known_durable(&id) {
                pending.unsynced.push(id);
            }
        } else {
            let bytes = encode(
                &id,
                self.key_id,
                self.keys.enc(),
                &self.store.aad(&id),
                plaintext,
            )?;
            self.store.publish_unsynced(&id, &bytes)?;
            pending.unsynced.push(id);
        }
        pending.seen.insert(id);
        Ok(id)
    }

    /// The durability barrier for everything put so far. No-op when nothing
    /// is pending.
    pub fn sync_pending(&self) -> Result<()> {
        let unsynced = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut pending.unsynced)
        };
        if unsynced.is_empty() {
            return Ok(());
        }
        let mut shards = BTreeSet::new();
        for id in &unsynced {
            let path = self.store.path(id);
            // Reopen rather than hold N descriptors open (macOS allows 256
            // by default). Never creates: a blob that vanished is an error.
            let file = crate::safe_fs::open_existing_rw(&path).at(&path)?;
            file.sync_all().at(&path)?;
            self.store.sync.file_syncs.fetch_add(1, Ordering::Relaxed);
            if let Some(dir) = path.parent() {
                shards.insert(dir.to_path_buf());
            }
        }
        for dir in &shards {
            crate::wal::sync_dir(dir)?;
            self.store.sync.dir_syncs.fetch_add(1, Ordering::Relaxed);
        }
        // New shard directories are entries of `blobs/`, and `blobs/` itself
        // may be new in the database root.
        for dir in [self.store.dir(), self.store.root().to_path_buf()] {
            crate::wal::sync_dir(&dir)?;
            self.store.sync.dir_syncs.fetch_add(1, Ordering::Relaxed);
        }
        self.store.mark_durable(unsynced);
        Ok(())
    }
}

/// Reader-side handle for one scan: caches derived keys per key id and
/// records what could not be resolved instead of failing the scan.
pub struct BlobReader<'a> {
    store: &'a BlobStore,
    provider: Option<&'a dyn KeyProvider>,
    enc_keys: RefCell<HashMap<KeyId, Option<Zeroizing<[u8; 32]>>>>,
    missing_keys: RefCell<BTreeSet<KeyId>>,
    problems: RefCell<Vec<String>>,
}

impl<'a> BlobReader<'a> {
    pub fn new(store: &'a BlobStore, provider: Option<&'a dyn KeyProvider>) -> Self {
        Self {
            store,
            provider,
            enc_keys: RefCell::new(HashMap::new()),
            missing_keys: RefCell::new(BTreeSet::new()),
            problems: RefCell::new(Vec::new()),
        }
    }

    fn enc_key(&self, key_id: KeyId) -> Option<Zeroizing<[u8; 32]>> {
        if let Some(cached) = self.enc_keys.borrow().get(&key_id) {
            return cached.clone();
        }
        let derived = self
            .provider
            .and_then(|p| p.key(key_id))
            .map(|m| Zeroizing::new(*DerivedKeys::from_master(&m).enc()));
        self.enc_keys.borrow_mut().insert(key_id, derived.clone());
        derived
    }

    /// Decrypt one blob, failing with `NoKey`, `Corrupt`, or `Io`.
    pub fn read(&self, id: &BlobId) -> Result<Vec<u8>> {
        let path = self.store.path(id);
        let bytes = std::fs::read(&path).at(&path)?;
        let (header, ciphertext) = parse(&bytes, &path)?;
        let enc = self.enc_key(header.key_id).ok_or(StorageError::NoKey {
            key_id: header.key_id,
        })?;
        decrypt(&header, ciphertext, &enc, &self.store.aad(id), &path)
    }

    /// Like [`read`](Self::read) but never fails: a missing key or an
    /// unreadable blob is recorded and yields `None`.
    pub fn resolve(&self, id: &BlobId) -> Option<Vec<u8>> {
        match self.read(id) {
            Ok(bytes) => Some(bytes),
            Err(StorageError::NoKey { key_id }) => {
                self.missing_keys.borrow_mut().insert(key_id);
                None
            }
            Err(e) => {
                let mut problems = self.problems.borrow_mut();
                if problems.len() < 32 {
                    problems.push(format!("blob {id} unreadable: {e}"));
                }
                None
            }
        }
    }

    pub fn missing_keys(&self) -> Vec<KeyId> {
        self.missing_keys.borrow().iter().copied().collect()
    }

    pub fn problems(&self) -> Vec<String> {
        self.problems.borrow().clone()
    }

    /// Human-readable notes: one per missing key, then blob problems.
    pub fn notes(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .missing_keys()
            .into_iter()
            .map(|k| StorageError::NoKey { key_id: k }.to_string())
            .collect();
        out.extend(self.problems());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_id_is_a_stable_fingerprint() {
        let k = [7u8; 32];
        let a = key_id_for(&k);
        let b = key_id_for(&k);
        assert_eq!(a, b);
        assert_eq!(a.get_version(), Some(Version::Custom));
        assert_ne!(a, key_id_for(&[8u8; 32]));
    }

    #[test]
    fn derived_keys_differ_by_purpose_and_master() {
        let a = DerivedKeys::from_master(&[1u8; 32]);
        let b = DerivedKeys::from_master(&[2u8; 32]);
        assert_ne!(a.enc(), a.hash());
        assert_ne!(a.enc(), b.enc());
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn blob_id_hex_roundtrip() {
        let id = BlobId::compute(&[3u8; 32], b"hello");
        let hex = id.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(BlobId::from_hex(&hex), Some(id));
        assert_eq!(BlobId::from_file_name(&id.file_name()), Some(id));
        assert_eq!(BlobId::from_hex("zz"), None);
        assert_eq!(&hex[..2], id.shard());
    }

    #[test]
    fn encode_parse_decrypt_roundtrip() {
        let master = [9u8; 32];
        let keys = DerivedKeys::from_master(&master);
        let key_id = key_id_for(&master);
        let plaintext = b"{\"prompt\":\"hi\"}";
        let id = BlobId::compute(keys.hash(), plaintext);
        let aad = [0xAAu8; AAD_LEN];
        let bytes = encode(&id, key_id, keys.enc(), &aad, plaintext).unwrap();
        assert_eq!(&bytes[..4], b"ATBL");
        assert_eq!(
            bytes.len(),
            BLOB_HEADER_LEN + plaintext.len() + TAG_LEN + BLOB_TRAILER_LEN
        );
        let p = Path::new("x.blob");
        let (header, ct) = parse(&bytes, p).unwrap();
        assert_eq!(header.key_id, key_id);
        assert_eq!(header.plaintext_len as usize, plaintext.len());
        assert_eq!(
            decrypt(&header, ct, keys.enc(), &aad, p).unwrap(),
            plaintext
        );
        // Wrong AAD → authentication failure.
        assert!(decrypt(&header, ct, keys.enc(), &[0u8; AAD_LEN], p).is_err());
    }
}
