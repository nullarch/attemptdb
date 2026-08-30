//! Bearer keys → principal.
//!
//! The key file never holds a key, only its SHA-256 digest, so reading the
//! file does not grant access and a leaked file is not a leaked credential.
//! Lookup hashes the presented key and indexes a map, which makes timing
//! independent of how close a wrong key is to a right one.

use crate::tenants::TenantId;
use anyhow::{Context, Result, bail};
use attemptdb_core::DeviceId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;

/// Who a request is: which tenant's database it writes, and which device it
/// claims to be. A batch must name this device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub tenant: TenantId,
    pub device_id: DeviceId,
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
            if by_digest
                .insert(
                    digest,
                    Principal {
                        tenant,
                        device_id: e.device_id,
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

    fn table() -> KeyTable {
        KeyTable::from_entries(vec![KeyEntry {
            sha256: digest_hex("secret-1"),
            tenant: "alpha".into(),
            device_id: DeviceId::derive(&["test", "d1"]),
            label: "d1".into(),
        }])
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
    fn rejects_bad_digests_tenants_and_duplicates() {
        let entry = |sha: &str, tenant: &str| KeyEntry {
            sha256: sha.into(),
            tenant: tenant.into(),
            device_id: DeviceId::derive(&["test", "d"]),
            label: String::new(),
        };
        assert!(KeyTable::from_entries(vec![entry("zz", "alpha")]).is_err());
        assert!(KeyTable::from_entries(vec![entry("abcd", "alpha")]).is_err());
        assert!(KeyTable::from_entries(vec![entry(&digest_hex("k"), "../etc")]).is_err());
        assert!(
            KeyTable::from_entries(vec![
                entry(&digest_hex("k"), "alpha"),
                entry(&digest_hex("k"), "beta")
            ])
            .is_err()
        );
    }
}
