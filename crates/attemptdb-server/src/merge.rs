//! Device-uploaded inferences and the rule that decides which computation a
//! read returns.
//!
//! A device may upload its own Tier-1 inferences (`POST /v1/sync/inferences`,
//! RFC 0006 §10.7); they are stored beside the tenant's event database, one
//! document per `(device, kind)`. The read API computes the same kinds on
//! the server from the tenant's events. When both exist for the same
//! `(kind, id)`, exactly one is returned, whole:
//!
//! - the **device** item, when its `algorithm_version` is the same as or
//!   newer than the server's `attemptdb_project::ALGORITHM_VERSION`;
//! - the **server** item otherwise, including whenever the device's
//!   version cannot be compared.
//!
//! "Newer" is defined by [`version_number`]: a version is `<family>-v<n>`;
//! two versions compare only within the same family, by `n`. Anything else
//! never wins. Every returned inference says which computation it came from
//! (`computed_by`), and fields of the two are never mixed.

use crate::inferences::{KINDS, store_path};
use anyhow::{Context, Result};
use attemptdb_core::DeviceId;
use attemptdb_project::ALGORITHM_VERSION;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// `<family>-v<n>` → `(family, n)`. `tier1-v0` → `("tier1", 0)`.
pub fn version_number(version: &str) -> Option<(&str, u64)> {
    let v = version.trim();
    let (family, n) = v.rsplit_once("-v")?;
    if family.is_empty() || n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((family, n.parse().ok()?))
}

/// Whether an item computed under `device` replaces one computed under
/// `server`: same family, device's number >= server's. Unknown formats and
/// other families lose.
pub fn device_wins(device: &str, server: &str) -> bool {
    match (version_number(device), version_number(server)) {
        (Some((df, dn)), Some((sf, sn))) => df == sf && dn >= sn,
        _ => false,
    }
}

/// The id form both sides agree on: bare lowercase uuid(s), no display
/// prefix. Handoff ids are `<from>:<to>` session ids.
pub fn normalise_id(kind: &str, id: &str) -> String {
    fn strip(part: &str) -> String {
        let p = part.trim();
        let p = ["att_", "wu_", "dec_", "ses_", "ev_", "trn_"]
            .iter()
            .find_map(|prefix| p.strip_prefix(prefix))
            .unwrap_or(p);
        p.to_ascii_lowercase()
    }
    if kind == "handoff" {
        id.split(':').map(strip).collect::<Vec<_>>().join(":")
    } else {
        strip(id)
    }
}

/// One item of a device document, as stored (provenance validated at
/// upload time).
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceItem {
    pub device_id: DeviceId,
    pub kind: String,
    /// Normalised (see [`normalise_id`]).
    pub id: String,
    pub algorithm_version: String,
    pub evidence: Vec<String>,
    pub confidence: f64,
    pub session_id: Option<String>,
    pub project_id: Option<String>,
    /// The projection row as the device uploaded it (content fields already
    /// removed by the server's ceiling where it applies).
    pub fields: Value,
    pub computed_at: Value,
    pub received_at: Value,
}

/// Every device item of a tenant, indexed by `(kind, id)`.
#[derive(Debug, Default)]
pub struct DeviceInferences {
    items: HashMap<(String, String), DeviceItem>,
    pub documents: usize,
    pub items_total: usize,
}

/// Sizes and mtimes of every document: changes whenever an upload lands.
pub type InferenceFingerprint = Vec<(String, u64, u128)>;

/// `<tenant>/inferences/<device>/<kind>.json` files, as `(name, len, mtime)`.
pub fn fingerprint(tenant_dir: &Path) -> InferenceFingerprint {
    let mut out = Vec::new();
    let Ok(devices) = std::fs::read_dir(tenant_dir.join("inferences")) else {
        return out;
    };
    for device in devices.flatten() {
        let Ok(files) = std::fs::read_dir(device.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = format!(
                "{}/{}",
                device.file_name().to_string_lossy(),
                file.file_name().to_string_lossy()
            );
            if let Ok(meta) = file.metadata() {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                out.push((name, meta.len(), mtime));
            }
        }
    }
    out.sort();
    out
}

impl DeviceInferences {
    /// Read every stored document of the tenant. A device directory whose
    /// name is not a device id is skipped; a document that does not parse
    /// is an error (it was written by this server).
    pub fn load(tenant_dir: &Path) -> Result<Self> {
        let mut out = Self::default();
        let Ok(devices) = std::fs::read_dir(tenant_dir.join("inferences")) else {
            return Ok(out);
        };
        for entry in devices.flatten() {
            let Ok(device_id) = entry.file_name().to_string_lossy().parse::<DeviceId>() else {
                continue;
            };
            for kind in KINDS {
                let path = store_path(tenant_dir, &device_id, kind);
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
                };
                let doc: Value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                out.documents += 1;
                out.absorb(device_id, kind, &doc);
            }
        }
        Ok(out)
    }

    /// Index one document. When two devices carry the same `(kind, id)`,
    /// the newer algorithm version wins, then the later `received_at`.
    pub fn absorb(&mut self, device_id: DeviceId, kind: &str, doc: &Value) {
        let doc_version = doc
            .get("algorithm_version")
            .and_then(Value::as_str)
            .unwrap_or("");
        let computed_at = doc.get("computed_at").cloned().unwrap_or(Value::Null);
        let received_at = doc.get("received_at").cloned().unwrap_or(Value::Null);
        let Some(items) = doc.get("items").and_then(Value::as_array) else {
            return;
        };
        for raw in items {
            let Some(id) = raw.get("id").and_then(Value::as_str) else {
                continue;
            };
            self.items_total += 1;
            let item = DeviceItem {
                device_id,
                kind: kind.to_string(),
                id: normalise_id(kind, id),
                algorithm_version: raw
                    .get("algorithm_version")
                    .and_then(Value::as_str)
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or(doc_version)
                    .to_string(),
                evidence: raw
                    .get("evidence")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                confidence: raw.get("confidence").and_then(Value::as_f64).unwrap_or(0.0),
                session_id: raw
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                project_id: raw
                    .get("project_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                fields: raw.get("fields").cloned().unwrap_or(Value::Null),
                computed_at: computed_at.clone(),
                received_at: received_at.clone(),
            };
            let key = (item.kind.clone(), item.id.clone());
            match self.items.get(&key) {
                Some(existing) if !prefer(&item, existing) => {}
                _ => {
                    self.items.insert(key, item);
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// The device item stored for `(kind, id)`, whatever its version.
    pub fn get(&self, kind: &str, id: &str) -> Option<&DeviceItem> {
        self.items.get(&(kind.to_string(), normalise_id(kind, id)))
    }

    /// The device item for `(kind, id)` **only when it wins** against the
    /// server's algorithm version; `None` means "return the server item".
    pub fn winner(&self, kind: &str, id: &str) -> Option<&DeviceItem> {
        self.get(kind, id)
            .filter(|d| device_wins(&d.algorithm_version, ALGORITHM_VERSION))
    }
}

/// Between two device items for the same key: the newer version, then the
/// later receipt.
fn prefer(candidate: &DeviceItem, existing: &DeviceItem) -> bool {
    let c = version_number(&candidate.algorithm_version).map(|(_, n)| n);
    let e = version_number(&existing.algorithm_version).map(|(_, n)| n);
    match (c, e) {
        (Some(c), Some(e)) if c != e => c > e,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        _ => {
            candidate.received_at.as_i64().unwrap_or(0) > existing.received_at.as_i64().unwrap_or(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions_compare_within_a_family_by_number() {
        assert_eq!(version_number("tier1-v0"), Some(("tier1", 0)));
        assert_eq!(version_number(" tier1-v12 "), Some(("tier1", 12)));
        assert_eq!(version_number("tier1"), None);
        assert_eq!(version_number("tier1-vx"), None);
        assert_eq!(version_number("-v1"), None);
        assert_eq!(version_number("tier1-v"), None);
        assert!(device_wins("tier1-v0", "tier1-v0"), "same version: device");
        assert!(device_wins("tier1-v1", "tier1-v0"), "newer: device");
        assert!(!device_wins("tier1-v0", "tier1-v1"), "older: server");
        assert!(!device_wins("tier2-v0", "tier1-v0"), "other family: server");
        assert!(!device_wins("v1", "tier1-v0"), "unknown format: server");
        assert!(!device_wins("", "tier1-v0"));
        assert!(!device_wins("tier1-v9", "custom"));
    }

    #[test]
    fn ids_normalise_to_bare_uuids() {
        assert_eq!(
            normalise_id("attempt", "att_0192A7C4-2b3e-7f10-8d4a-0e1f2a3b4c5d"),
            "0192a7c4-2b3e-7f10-8d4a-0e1f2a3b4c5d"
        );
        assert_eq!(normalise_id("work_unit", "wu_x"), "x");
        assert_eq!(normalise_id("decision", "dec_x"), "x");
        assert_eq!(normalise_id("handoff", "ses_A:ses_B"), "a:b");
        assert_eq!(normalise_id("handoff", "a:b"), "a:b");
        assert_eq!(normalise_id("attempt", "plain"), "plain");
    }

    fn doc(version: &str, received_at: i64, items: Value) -> Value {
        json!({
            "schema": "attemptdb.inference/v1",
            "kind": "attempt",
            "algorithm_version": version,
            "computed_at": 1,
            "received_at": received_at,
            "items": items,
        })
    }

    #[test]
    fn the_winner_is_the_device_only_for_a_current_or_newer_version() {
        let d1 = DeviceId::derive(&["merge", "1"]);
        let mut inf = DeviceInferences::default();
        inf.absorb(
            d1,
            "attempt",
            &doc(
                ALGORITHM_VERSION,
                10,
                json!([
                    { "id": "att_aaa", "evidence": ["ev_1"], "confidence": 0.5, "algorithm_version": "tier1-v9", "fields": {"approach": "new"} },
                    { "id": "bbb", "evidence": ["ev_2"], "confidence": 0.5, "algorithm_version": "tier0-v3", "fields": {} },
                    { "id": "ccc", "evidence": ["ev_3"], "confidence": 0.5, "algorithm_version": "", "fields": {} },
                ]),
            ),
        );
        assert_eq!(inf.len(), 3);
        assert_eq!(inf.items_total, 3);
        assert_eq!(
            inf.winner("attempt", "aaa").unwrap().fields["approach"],
            "new"
        );
        assert!(inf.winner("attempt", "att_aaa").is_some());
        assert!(inf.get("attempt", "bbb").is_some());
        assert!(inf.winner("attempt", "bbb").is_none(), "other family");
        assert_eq!(
            inf.get("attempt", "ccc").unwrap().algorithm_version,
            ALGORITHM_VERSION,
            "empty item version falls back to the document's"
        );
        assert!(
            inf.winner("attempt", "ccc").is_some(),
            "same as the server's"
        );
        assert!(
            inf.winner("work_unit", "aaa").is_none(),
            "kinds are separate"
        );
    }

    #[test]
    fn two_devices_with_the_same_item_prefer_the_newer_then_the_later() {
        let d1 = DeviceId::derive(&["merge", "1"]);
        let d2 = DeviceId::derive(&["merge", "2"]);
        let mut inf = DeviceInferences::default();
        let item = |v: &str, tag: &str| json!([{ "id": "x", "evidence": ["ev_1"], "confidence": 0.5, "algorithm_version": v, "fields": {"tag": tag} }]);
        inf.absorb(d1, "attempt", &doc("tier1-v0", 10, item("tier1-v0", "old")));
        inf.absorb(
            d2,
            "attempt",
            &doc("tier1-v1", 5, item("tier1-v1", "newer")),
        );
        assert_eq!(inf.get("attempt", "x").unwrap().fields["tag"], "newer");
        inf.absorb(
            d1,
            "attempt",
            &doc("tier1-v1", 20, item("tier1-v1", "later")),
        );
        assert_eq!(inf.get("attempt", "x").unwrap().fields["tag"], "later");
        inf.absorb(
            d2,
            "attempt",
            &doc("tier1-v1", 15, item("tier1-v1", "earlier")),
        );
        assert_eq!(inf.get("attempt", "x").unwrap().fields["tag"], "later");
        assert_eq!(inf.get("attempt", "x").unwrap().device_id, d1);
    }

    #[test]
    fn documents_are_loaded_from_the_tenant_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let d1 = DeviceId::derive(&["merge", "1"]);
        assert!(fingerprint(tmp.path()).is_empty());
        let empty = DeviceInferences::load(tmp.path()).unwrap();
        assert!(empty.is_empty() && empty.documents == 0);
        let path = store_path(tmp.path(), &d1, "work_unit");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            doc(
                ALGORITHM_VERSION,
                1,
                json!([{ "id": "wu_1", "evidence": ["ev_1"], "confidence": 0.7, "algorithm_version": ALGORITHM_VERSION, "fields": {"phase": "verify"} }]),
            )
            .to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("inferences").join("not-a-device")).unwrap();
        let fp = fingerprint(tmp.path());
        assert_eq!(fp.len(), 1);
        assert!(fp[0].0.ends_with("/work_unit.json"));
        let loaded = DeviceInferences::load(tmp.path()).unwrap();
        assert_eq!(loaded.documents, 1);
        assert_eq!(
            loaded.winner("work_unit", "1").unwrap().fields["phase"],
            "verify"
        );
        std::fs::write(&path, "{not json").unwrap();
        assert!(DeviceInferences::load(tmp.path()).is_err());
    }
}
