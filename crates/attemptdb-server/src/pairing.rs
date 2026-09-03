//! Pairing: how one command on a fresh machine ends with a device key
//! that nobody ever saw.
//!
//! The product's web app, server to server with the admin token, mints a
//! one-time `pair_…` token bound to a tenant (and a user); the user pastes
//! the one-line install command that carries it; the installer, before it
//! touches anything, checks the token, and once the local database exists
//! it exchanges the token plus the local `device_id` for a device key. The
//! key travels once, in that response, to the installer; the server keeps
//! the key's digest (as for every key) and the token's digest (as for
//! every token). What can appear in a shell history is a token that
//! expires in minutes and dies on first use.
//!
//! - `POST /v1/admin/pairings` (admin token) → a token, its expiry.
//! - `GET /v1/pair/{token}` (no auth) → valid / expired / used / unknown.
//! - `POST /v1/pair` (no auth) `{token, device_id, label?}` → the device
//!   key, bound to that `device_id` — the same id the device's sync
//!   batches will carry, so `/v1/sync` never answers `403` for a mismatch.
//!   Older device keys of the same device in the same tenant are revoked
//!   (a re-pair is the same machine coming back).
//!
//! Tokens are 32 random bytes; the store holds their SHA-256 in
//! `<data-dir>/pairings.json`, rewritten atomically, pruned of the expired
//! on every change. The public routes are rate limited (`crate::limiter`).

use crate::AppState;
use crate::auth::{self, KeyEntry, Scope};
use crate::tenants::TenantId;
use anyhow::{Context, Result};
use attemptdb_core::{DeviceId, Timestamp};
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};

pub const DEFAULT_TTL_SECS: u64 = 10 * 60;
pub const MAX_TTL_SECS: u64 = 60 * 60;
pub const TOKEN_PREFIX: &str = "pair_";

/// One pairing, as stored: the token's digest, never the token.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pairing {
    pub sha256: String,
    pub tenant: String,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub label: String,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    /// Set when the token was exchanged; a used token stays until it
    /// expires so a second exchange can be told apart from an unknown one.
    #[serde(default)]
    pub used_at: Option<Timestamp>,
    #[serde(default)]
    pub device_id: Option<DeviceId>,
}

impl Pairing {
    pub fn is_expired(&self, now: Timestamp) -> bool {
        now.as_micros() >= self.expires_at.as_micros()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PairingFile {
    pairings: Vec<Pairing>,
}

/// The store: in memory behind a mutex, mirrored to `pairings.json`.
#[derive(Debug)]
pub struct PairingTable {
    path: PathBuf,
    inner: Mutex<Vec<Pairing>>,
}

impl PairingTable {
    pub fn path_in(data_dir: &FsPath) -> PathBuf {
        data_dir.join("pairings.json")
    }

    pub fn load(data_dir: &FsPath) -> Result<Self> {
        let path = Self::path_in(data_dir);
        let pairings = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice::<PairingFile>(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?
                    .pairings
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self {
            path,
            inner: Mutex::new(pairings),
        })
    }

    fn save(&self, pairings: &[Pairing]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(&PairingFile {
            pairings: pairings.to_vec(),
        })?;
        std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming {} into place", tmp.display()))?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Pairing>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Mint a token: returned once, stored as a digest.
    pub fn issue(
        &self,
        tenant: &TenantId,
        user_id: Option<String>,
        label: String,
        ttl_secs: u64,
        now: Timestamp,
    ) -> Result<(String, Pairing)> {
        let token = mint_token();
        let pairing = Pairing {
            sha256: auth::digest_hex(&token),
            tenant: tenant.as_str().to_string(),
            user_id,
            label,
            created_at: now,
            expires_at: Timestamp::from_micros(now.as_micros() + (ttl_secs as i64) * 1_000_000),
            used_at: None,
            device_id: None,
        };
        let mut all = self.lock();
        all.retain(|p| !p.is_expired(now));
        all.push(pairing.clone());
        self.save(&all)?;
        Ok((token, pairing))
    }

    /// What a token is, without changing it.
    pub fn check(&self, token: &str, now: Timestamp) -> TokenState {
        let digest = auth::digest_hex(token.trim());
        let all = self.lock();
        match all
            .iter()
            .find(|p| auth::eq_ct(p.sha256.as_bytes(), digest.as_bytes()))
        {
            None => TokenState::Unknown,
            Some(p) if p.used_at.is_some() => TokenState::Used,
            Some(p) if p.is_expired(now) => TokenState::Expired,
            Some(p) => TokenState::Valid(p.clone()),
        }
    }

    /// Consume a token: valid → marked used with the device, atomically.
    pub fn consume(&self, token: &str, device_id: DeviceId, now: Timestamp) -> Result<TokenState> {
        let digest = auth::digest_hex(token.trim());
        let mut all = self.lock();
        let Some(i) = all
            .iter()
            .position(|p| auth::eq_ct(p.sha256.as_bytes(), digest.as_bytes()))
        else {
            return Ok(TokenState::Unknown);
        };
        if all[i].used_at.is_some() {
            return Ok(TokenState::Used);
        }
        if all[i].is_expired(now) {
            return Ok(TokenState::Expired);
        }
        all[i].used_at = Some(now);
        all[i].device_id = Some(device_id);
        let used = all[i].clone();
        all.retain(|p| !p.is_expired(now) || p.used_at.is_none());
        self.save(&all)?;
        Ok(TokenState::Valid(used))
    }

    /// Outstanding (unused, unexpired) pairings: digests only.
    pub fn outstanding(&self, now: Timestamp) -> Vec<Pairing> {
        self.lock()
            .iter()
            .filter(|p| p.used_at.is_none() && !p.is_expired(now))
            .cloned()
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenState {
    Valid(Pairing),
    Expired,
    Used,
    Unknown,
}

fn mint_token() -> String {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).expect("OS randomness");
    format!("{TOKEN_PREFIX}{}", hex::encode(raw))
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

#[derive(Debug, Deserialize)]
pub struct IssueRequest {
    pub tenant: String,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub label: String,
    /// Seconds until the token dies; default 600, at most 3600.
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

/// `POST /v1/admin/pairings` — mint a pairing token (admin token).
pub async fn issue(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<IssueRequest>, JsonRejection>,
) -> Response {
    if let Err(r) = crate::admin::gate(&state, &headers) {
        return *r;
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error(e.status(), e.body_text()),
    };
    let tenant = match TenantId::parse(&req.tenant) {
        Ok(t) => t,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let user_id = match req.user_id.as_deref() {
        None => None,
        Some(u) => match auth::validate_user_id(u) {
            Ok(u) => Some(u),
            Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
        },
    };
    let ttl = req.ttl_secs.unwrap_or(DEFAULT_TTL_SECS);
    if ttl == 0 || ttl > MAX_TTL_SECS {
        return error(
            StatusCode::BAD_REQUEST,
            format!("ttl_secs must be 1..={MAX_TTL_SECS}"),
        );
    }
    let now = Timestamp::now();
    let st = Arc::clone(&state);
    let label = req.label.trim().to_string();
    let minted =
        tokio::task::spawn_blocking(move || st.pairings.issue(&tenant, user_id, label, ttl, now))
            .await;
    match minted {
        Ok(Ok((token, p))) => (
            StatusCode::CREATED,
            Json(json!({
                "token": token,
                "sha256": p.sha256,
                "tenant": p.tenant,
                "user_id": p.user_id,
                "label": p.label,
                "expires_at": p.expires_at.to_rfc3339(),
                "ttl_secs": ttl,
                "note": "one use, then dead; the server keeps only its digest",
            })),
        )
            .into_response(),
        Ok(Err(e)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("cannot save the pairing file: {e:#}"),
        ),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, format!("task failed: {e}")),
    }
}

/// `GET /v1/admin/pairings` — outstanding tokens' digests (admin token).
pub async fn list(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = crate::admin::gate(&state, &headers) {
        return *r;
    }
    let now = Timestamp::now();
    let items: Vec<_> = state
        .pairings
        .outstanding(now)
        .into_iter()
        .map(|p| {
            json!({
                "sha256": p.sha256, "tenant": p.tenant, "user_id": p.user_id,
                "label": p.label, "created_at": p.created_at.to_rfc3339(),
                "expires_at": p.expires_at.to_rfc3339(),
            })
        })
        .collect();
    Json(json!({ "pairings": items })).into_response()
}

fn token_ok(token: &str) -> bool {
    token.starts_with(TOKEN_PREFIX)
        && token.len() == TOKEN_PREFIX.len() + 64
        && token[TOKEN_PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
}

/// `GET /v1/pair/{token}` — is this token good to go? No auth: the token
/// is the credential. An installer calls this before it changes anything.
pub async fn check(State(state): State<Arc<AppState>>, Path(token): Path<String>) -> Response {
    if !token_ok(token.trim()) {
        return error(StatusCode::BAD_REQUEST, "not a pairing token");
    }
    match state.pairings.check(&token, Timestamp::now()) {
        TokenState::Valid(p) => Json(json!({
            "valid": true,
            "tenant": p.tenant,
            "label": p.label,
            "expires_at": p.expires_at.to_rfc3339(),
        }))
        .into_response(),
        TokenState::Expired => error(StatusCode::GONE, "pairing token expired"),
        TokenState::Used => error(StatusCode::GONE, "pairing token already used"),
        TokenState::Unknown => error(StatusCode::NOT_FOUND, "unknown pairing token"),
    }
}

#[derive(Debug, Deserialize)]
pub struct ExchangeRequest {
    pub token: String,
    /// The local database's device id: the key is bound to it.
    pub device_id: DeviceId,
    #[serde(default)]
    pub label: Option<String>,
}

/// `POST /v1/pair` — exchange a token and the local device id for a device
/// key. No auth: the token is the credential, and it dies here.
pub async fn exchange(
    State(state): State<Arc<AppState>>,
    body: Result<Json<ExchangeRequest>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error(e.status(), e.body_text()),
    };
    if !token_ok(req.token.trim()) {
        return error(StatusCode::BAD_REQUEST, "not a pairing token");
    }
    let now = Timestamp::now();
    let st = Arc::clone(&state);
    let token = req.token.trim().to_string();
    let device_id = req.device_id;
    let label = req.label.unwrap_or_default();
    let result =
        tokio::task::spawn_blocking(move || -> Result<Result<(String, KeyEntry), TokenState>> {
            let pairing = match st.pairings.consume(&token, device_id, now)? {
                TokenState::Valid(p) => p,
                other => return Ok(Err(other)),
            };
            let tenant = TenantId::parse(&pairing.tenant)?;
            // The same machine coming back: its earlier device keys in this
            // tenant are retired with the new one.
            st.remove_keys_where(|e| {
                !(e.tenant == tenant.as_str()
                    && e.device_id == device_id
                    && e.scope == Scope::Device)
            })?;
            let key = auth::mint_key();
            let entry = KeyEntry {
                sha256: auth::digest_hex(&key),
                tenant: tenant.as_str().to_string(),
                device_id,
                label: if label.trim().is_empty() {
                    pairing.label.clone()
                } else {
                    label.trim().to_string()
                },
                scope: Scope::Device,
                user_id: pairing.user_id.clone(),
                issued_at: Some(now),
            };
            st.add_key(entry.clone())?;
            Ok(Ok((key, entry)))
        })
        .await;
    match result {
        Ok(Ok(Ok((key, entry)))) => (
            StatusCode::CREATED,
            Json(json!({
                "key": key,
                "sha256": entry.sha256,
                "tenant": entry.tenant,
                "device_id": entry.device_id,
                "label": entry.label,
                "scope": "device",
                "user_id": entry.user_id,
                "note": "store the key now; the server keeps only its digest, and the token is spent",
            })),
        )
            .into_response(),
        Ok(Ok(Err(TokenState::Expired))) => error(StatusCode::GONE, "pairing token expired"),
        Ok(Ok(Err(TokenState::Used))) => error(StatusCode::GONE, "pairing token already used"),
        Ok(Ok(Err(_))) => error(StatusCode::NOT_FOUND, "unknown pairing token"),
        Ok(Err(e)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("cannot complete the pairing: {e:#}"),
        ),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, format!("task failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_valid_once_then_used_and_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let table = PairingTable::load(tmp.path()).unwrap();
        let t0 = Timestamp::from_micros(1_000_000);
        let tenant = TenantId::parse("acme").unwrap();
        let (token, p) = table
            .issue(
                &tenant,
                Some("usr_1".into()),
                "kevin laptop".into(),
                600,
                t0,
            )
            .unwrap();
        assert!(token_ok(&token));
        assert_ne!(p.sha256, token, "the file holds a digest");
        assert!(matches!(table.check(&token, t0), TokenState::Valid(_)));
        assert!(matches!(table.check("pair_nope", t0), TokenState::Unknown));
        // Reloading from disk sees it.
        let again = PairingTable::load(tmp.path()).unwrap();
        assert!(matches!(again.check(&token, t0), TokenState::Valid(_)));
        let dev = DeviceId::new();
        let used = again.consume(&token, dev, t0).unwrap();
        assert!(matches!(used, TokenState::Valid(ref p) if p.device_id == Some(dev)));
        assert_eq!(again.check(&token, t0), TokenState::Used);
        assert_eq!(again.consume(&token, dev, t0).unwrap(), TokenState::Used);
        // Expiry.
        let (t2, _) = again.issue(&tenant, None, "".into(), 60, t0).unwrap();
        let later = Timestamp::from_micros(t0.as_micros() + 61_000_000);
        assert_eq!(again.check(&t2, later), TokenState::Expired);
        assert_eq!(again.consume(&t2, dev, later).unwrap(), TokenState::Expired);
        assert!(again.outstanding(later).is_empty());
        let file = std::fs::read_to_string(PairingTable::path_in(tmp.path())).unwrap();
        assert!(!file.contains(&token[5..]), "no token in the file");
    }
}
