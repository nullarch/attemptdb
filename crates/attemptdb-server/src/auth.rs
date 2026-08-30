//! Bearer keys → principal.
//!
//! The key file never holds a key, only its SHA-256 digest, so reading the
//! file does not grant access and a leaked file is not a leaked credential.
//! Lookup hashes the presented key and indexes a map, which makes timing
//! independent of how close a wrong key is to a right one.
//!
//! Every key has a **scope**. A `device` key is what an installer stores on
//! a user's machine: it may upload that one device's events and inferences
//! and read back what that device uploaded, nothing else. A `reader` key is
//! what the product's backend holds: it may read a tenant's projections but
//! never write. `admin` is a reader that may also manage the tenant. The
//! admin *token* (`ServerConfig::admin_token`) is a separate, single
//! operator credential for issuing keys; it is not a key.

use crate::tenants::TenantId;
use anyhow::{Context, Result, bail};
use attemptdb_core::DeviceId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;

/// What a key may do. Ordered: `Device < Reader < Admin` for reads, but
/// only `Device` may write — a reader is not a weaker device, it is a
/// different kind of principal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    Device,
    Reader,
    Admin,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Device => "device",
            Scope::Reader => "reader",
            Scope::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "device" => Some(Scope::Device),
            "reader" => Some(Scope::Reader),
            "admin" => Some(Scope::Admin),
            _ => None,
        }
    }

    pub const ALL: &'static [Scope] = &[Scope::Device, Scope::Reader, Scope::Admin];
}

/// Who a request is: which tenant's database it touches, which device it
/// claims to be, what it may do, and (when the product bound the key to a
/// person) whose it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub tenant: TenantId,
    pub device_id: DeviceId,
    pub scope: Scope,
    pub user_id: Option<String>,
}

impl Principal {
    /// May upload events and inferences for `device_id`.
    pub fn can_write(&self) -> bool {
        self.scope == Scope::Device
    }

    /// May read the whole tenant (every device's data and projections).
    pub fn can_read_tenant(&self) -> bool {
        self.scope >= Scope::Reader
    }
}

#[derive(Serialize, Deserialize)]
struct KeyFile {
    keys: Vec<KeyEntry>,
}

/// One line of the key file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyEntry {
    /// Lower-case hex SHA-256 of the bearer key.
    pub sha256: String,
    pub tenant: String,
    pub device_id: DeviceId,
    /// Operator note; never used for anything.
    #[serde(default)]
    pub label: String,
    /// Absent in files written before scopes existed: those keys were all
    /// device keys, and stay so.
    #[serde(default)]
    pub scope: Scope,
    /// The product's user id this key was issued to, if any. Opaque here:
    /// it is echoed in listings and carried on the principal so a reader
    /// can be attributed, never interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// Bounds on a `user_id`: opaque, but not arbitrary bytes. Printable,
/// single-line, at most 128 characters.
pub fn validate_user_id(s: &str) -> Result<String> {
    let s = s.trim();
    if s.is_empty() {
        bail!("user_id is empty");
    }
    if s.chars().count() > 128 {
        bail!("user_id is longer than 128 characters");
    }
    if s.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("user_id must be a single token without whitespace or control characters");
    }
    Ok(s.to_string())
}

pub struct KeyTable {
    by_digest: HashMap<[u8; 32], Principal>,
    entries: Vec<KeyEntry>,
}

impl KeyTable {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let file: KeyFile =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Self::from_entries(file.keys)
    }

    pub fn from_entries(entries: Vec<KeyEntry>) -> Result<Self> {
        let mut by_digest = HashMap::with_capacity(entries.len());
        let kept = entries.clone();
        for e in entries {
            let raw = hex::decode(e.sha256.trim())
                .with_context(|| format!("key digest for {:?} is not hex", e.label))?;
            let digest: [u8; 32] = raw
                .try_into()
                .map_err(|_| anyhow::anyhow!("key digest for {:?} is not 32 bytes", e.label))?;
            let tenant = TenantId::parse(&e.tenant)?;
            let user_id = match e.user_id.as_deref() {
                Some(u) => Some(validate_user_id(u)?),
                None => None,
            };
            if by_digest
                .insert(
                    digest,
                    Principal {
                        tenant,
                        device_id: e.device_id,
                        scope: e.scope,
                        user_id,
                    },
                )
                .is_some()
            {
                bail!("duplicate key digest {}", e.sha256);
            }
        }
        Ok(Self {
            by_digest,
            entries: kept,
        })
    }

    /// Every entry, as loaded (digests, never keys).
    pub fn entries(&self) -> Vec<KeyEntry> {
        self.entries.clone()
    }

    /// Write entries to `path` atomically (temp file + rename), mode 0600.
    pub fn save(entries: &[KeyEntry], path: &Path) -> Result<()> {
        let file = KeyFile {
            keys: entries.to_vec(),
        };
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&file)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.by_digest.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_digest.is_empty()
    }

    /// Resolve an `Authorization` header value. `None` for anything that is
    /// not exactly `Bearer <key>` with a known key.
    pub fn authenticate(&self, authorization: Option<&str>) -> Option<Principal> {
        let value = authorization?.trim();
        let (scheme, key) = value.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        let key = key.trim();
        if key.is_empty() {
            return None;
        }
        self.by_digest.get(&digest(key)).cloned()
    }
}

fn digest(key: &str) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}

/// A fresh bearer key: 32 random bytes, hex, with a recognisable prefix so a
/// leaked one is easy to search for.
pub fn mint_key() -> String {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).expect("OS randomness");
    format!("atk_{}", hex::encode(raw))
}

/// Constant-time byte comparison.
pub fn eq_ct(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The digest an operator writes into the key file for a given key.
pub fn digest_hex(key: &str) -> String {
    hex::encode(digest(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, tenant: &str, scope: Scope) -> KeyEntry {
        KeyEntry {
            sha256: digest_hex(key),
            tenant: tenant.into(),
            device_id: DeviceId::derive(&["test", key]),
            label: key.into(),
            scope,
            user_id: None,
        }
    }

    fn table() -> KeyTable {
        KeyTable::from_entries(vec![
            entry("secret-1", "alpha", Scope::Device),
            entry("reader-1", "alpha", Scope::Reader),
        ])
        .unwrap()
    }

    #[test]
    fn bearer_lookup() {
        let t = table();
        let p = t.authenticate(Some("Bearer secret-1")).unwrap();
        assert_eq!(p.tenant.as_str(), "alpha");
        assert!(t.authenticate(Some("bearer  secret-1 ")).is_some());
        assert!(t.authenticate(Some("Bearer secret-2")).is_none());
        assert!(t.authenticate(Some("Basic secret-1")).is_none());
        assert!(t.authenticate(Some("secret-1")).is_none());
        assert!(t.authenticate(Some("Bearer ")).is_none());
        assert!(t.authenticate(None).is_none());
    }

    #[test]
    fn scopes_separate_writing_from_reading() {
        let t = table();
        let device = t.authenticate(Some("Bearer secret-1")).unwrap();
        let reader = t.authenticate(Some("Bearer reader-1")).unwrap();
        assert!(device.can_write() && !device.can_read_tenant());
        assert!(!reader.can_write() && reader.can_read_tenant());
        assert!(Scope::Admin > Scope::Reader && Scope::Reader > Scope::Device);
        for s in Scope::ALL {
            assert_eq!(Scope::parse(s.as_str()), Some(*s));
        }
        assert_eq!(Scope::parse("root"), None);
    }

    #[test]
    fn files_without_a_scope_hold_device_keys() {
        let json = serde_json::json!({
            "sha256": digest_hex("k"), "tenant": "alpha",
            "device_id": DeviceId::derive(&["test", "k"]), "label": "old"
        });
        let e: KeyEntry = serde_json::from_value(json).unwrap();
        assert_eq!(e.scope, Scope::Device);
        assert_eq!(e.user_id, None);
        let text = serde_json::to_string(&e).unwrap();
        assert!(text.contains("\"scope\":\"device\""));
        assert!(!text.contains("user_id"), "absent user_id is not written");
    }

    #[test]
    fn user_ids_are_opaque_single_tokens() {
        assert_eq!(validate_user_id(" usr_42 ").unwrap(), "usr_42");
        assert!(validate_user_id("").is_err());
        assert!(validate_user_id("two words").is_err());
        assert!(validate_user_id("tab\there").is_err());
        assert!(validate_user_id(&"x".repeat(129)).is_err());
        let mut e = entry("k", "alpha", Scope::Reader);
        e.user_id = Some("bad id".into());
        assert!(KeyTable::from_entries(vec![e]).is_err());
    }

    #[test]
    fn rejects_bad_digests_tenants_and_duplicates() {
        let e = |sha: &str, tenant: &str| KeyEntry {
            sha256: sha.into(),
            tenant: tenant.into(),
            device_id: DeviceId::derive(&["test", "d"]),
            label: String::new(),
            scope: Scope::Device,
            user_id: None,
        };
        assert!(KeyTable::from_entries(vec![e("zz", "alpha")]).is_err());
        assert!(KeyTable::from_entries(vec![e("abcd", "alpha")]).is_err());
        assert!(KeyTable::from_entries(vec![e(&digest_hex("k"), "../etc")]).is_err());
        assert!(
            KeyTable::from_entries(vec![
                e(&digest_hex("k"), "alpha"),
                e(&digest_hex("k"), "beta")
            ])
            .is_err()
        );
    }
}
