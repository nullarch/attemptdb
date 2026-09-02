//! Facts about a slice of the stream that every reader summarises from the
//! events themselves rather than from the projection: which projects and
//! providers exist and how many events each has, which device wrote a
//! session's first event and how much of it was reconstructed, what each
//! device contributed. The server's `/v1/status`, `/v1/devices` and
//! project resolution, the UI's status page and scope bar, and the MCP
//! tools' status all read these.
//!
//! [`StreamFacts`] is derived from a segment's columns once (no `Event` is
//! decoded) and merged in stream order with [`StreamFacts::absorb`], so a
//! view over a thousand segments pays for the merge, not for a pass over
//! every event.

use attemptdb_core::{DeviceId, EventKind, ProjectId, SessionId, Timestamp};
use attemptdb_project::is_meta_kind;
use attemptdb_storage::segment::{Cols, col};
use datafusion::arrow::array::RecordBatch;
use std::collections::{BTreeMap, HashMap, HashSet};

/// One project as the events describe it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectFacts {
    pub project_id: ProjectId,
    pub name: String,
    pub root: String,
    pub repo_remote: Option<String>,
    pub events: u64,
    pub sessions: HashSet<SessionId>,
}

/// One provider's share of the events.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderFacts {
    pub provider: String,
    pub events: u64,
    /// Latest `observed_at`, capture tests excluded.
    pub last_event_at: Option<Timestamp>,
}

/// Facts about one session that the projection does not carry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionFacts {
    /// The device that wrote the session's first event (in stream order).
    pub device_id: DeviceId,
    pub provider_session_id: String,
    pub provider: String,
    pub project_id: ProjectId,
    /// Hook-captured versus transcript-reconstructed events.
    pub captured: usize,
    pub reconstructed: usize,
    /// Latest `observed_at` and the kind of that event.
    pub last_event_at: Option<Timestamp>,
    pub last_kind: Option<EventKind>,
}

/// The newest event of a slice by `observed_at`, capture tests excluded:
/// what "is this user coding right now" is answered from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastEvent {
    pub at: Timestamp,
    pub kind: EventKind,
    pub provider: String,
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub tool: Option<String>,
}

/// One device's contribution. Meta events (corrections, retractions) are
/// kept apart so a server can leave out the ones it wrote itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceFacts {
    pub events: u64,
    pub sessions: HashSet<SessionId>,
    pub providers: HashSet<String>,
    pub first_observed_at: Option<Timestamp>,
    pub last_observed_at: Option<Timestamp>,
    pub last_ingested_at: Option<Timestamp>,
}

#[derive(Clone, Debug, Default)]
pub struct StreamFacts {
    pub events: u64,
    pub reconstructed: u64,
    pub projects: BTreeMap<ProjectId, ProjectFacts>,
    pub providers: BTreeMap<String, ProviderFacts>,
    /// In the order sessions were first seen.
    pub sessions: Vec<(SessionId, SessionFacts)>,
    /// Keyed by `(device, is meta event)`.
    pub devices: HashMap<(DeviceId, bool), DeviceFacts>,
    /// Latest `observed_at`, capture tests excluded.
    pub last_event_at: Option<Timestamp>,
    /// The event that set `last_event_at`.
    pub last_event: Option<LastEvent>,
    session_index: HashMap<SessionId, usize>,
}

fn max_ts(a: Option<Timestamp>, b: Option<Timestamp>) -> Option<Timestamp> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

fn min_ts(a: Option<Timestamp>, b: Option<Timestamp>) -> Option<Timestamp> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// `attrs.reconstructed == true`, read without parsing attrs that cannot
/// carry it.
fn reconstructed_in(attrs_json: Option<&str>) -> bool {
    let Some(a) = attrs_json else { return false };
    if !a.contains("\"reconstructed\"") {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(a)
        .ok()
        .and_then(|v| v.get("reconstructed").and_then(serde_json::Value::as_bool))
        == Some(true)
}

/// One row's worth of input.
struct Row<'a> {
    project_id: ProjectId,
    project_name: &'a str,
    project_root: &'a str,
    repo_remote: Option<&'a str>,
    provider: &'a str,
    provider_session_id: &'a str,
    kind: EventKind,
    session_id: SessionId,
    device_id: DeviceId,
    observed_at: Timestamp,
    ingested_at: Option<Timestamp>,
    reconstructed: bool,
    tool: Option<&'a str>,
}

impl StreamFacts {
    pub fn from_events<'a>(events: impl IntoIterator<Item = &'a attemptdb_core::Event>) -> Self {
        let mut f = Self::default();
        for ev in events {
            f.push(Row {
                project_id: ev.project.project_id,
                project_name: &ev.project.name,
                project_root: &ev.project.root,
                repo_remote: ev.project.repo_remote.as_deref(),
                provider: ev.provider.as_str(),
                provider_session_id: &ev.provider_session_id,
                kind: ev.kind,
                session_id: ev.session_id,
                device_id: ev.device_id,
                observed_at: ev.observed_at,
                ingested_at: ev.ingested_at,
                reconstructed: ev
                    .attrs
                    .get("reconstructed")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true),
                tool: ev.tool.as_ref().map(|t| t.name.as_str()),
            });
        }
        f
    }

    /// From the columns of canonical-schema batches.
    pub fn from_batches(batches: &[RecordBatch]) -> Self {
        let mut f = Self::default();
        for b in batches {
            let Ok(c) = Cols::new(b.clone()) else {
                continue;
            };
            for row in 0..c.num_rows() {
                let (Some(pid), Some(sid), Some(did), Some(at)) = (
                    c.fsb(col::PROJECT_ID, row),
                    c.fsb(col::SESSION_ID, row),
                    c.fsb(col::DEVICE_ID, row),
                    c.ts(col::OBSERVED_AT, row),
                ) else {
                    continue;
                };
                f.push(Row {
                    project_id: ProjectId::from_bytes(pid),
                    project_name: c.str_ref(col::PROJECT_NAME, row).unwrap_or_default(),
                    project_root: c.str_ref(col::PROJECT_ROOT, row).unwrap_or_default(),
                    repo_remote: c.str_ref(col::REPO_REMOTE, row),
                    provider: c.str_ref(col::PROVIDER, row).unwrap_or_default(),
                    provider_session_id: c
                        .str_ref(col::PROVIDER_SESSION_ID, row)
                        .unwrap_or_default(),
                    kind: c
                        .str_ref(col::KIND, row)
                        .and_then(EventKind::parse)
                        .unwrap_or(EventKind::Unknown),
                    session_id: SessionId::from_bytes(sid),
                    device_id: DeviceId::from_bytes(did),
                    observed_at: at,
                    ingested_at: c.ts(col::INGESTED_AT, row),
                    reconstructed: reconstructed_in(c.str_ref(col::ATTRS_JSON, row)),
                    tool: c.str_ref(col::TOOL_NAME, row),
                });
            }
        }
        f
    }

    fn push(&mut self, r: Row<'_>) {
        self.events += 1;
        if r.reconstructed {
            self.reconstructed += 1;
        }
        let p = self
            .projects
            .entry(r.project_id)
            .or_insert_with(|| ProjectFacts {
                project_id: r.project_id,
                name: r.project_name.to_string(),
                root: r.project_root.to_string(),
                repo_remote: None,
                events: 0,
                sessions: HashSet::new(),
            });
        p.events += 1;
        if p.repo_remote.is_none()
            && let Some(remote) = r.repo_remote
        {
            p.repo_remote = Some(remote.to_string());
        }
        p.sessions.insert(r.session_id);
        let pr = self
            .providers
            .entry(r.provider.to_string())
            .or_insert_with(|| ProviderFacts {
                provider: r.provider.to_string(),
                ..Default::default()
            });
        pr.events += 1;
        if r.kind != EventKind::CaptureTest {
            pr.last_event_at = max_ts(pr.last_event_at, Some(r.observed_at));
            if self.last_event_at.is_none_or(|t| r.observed_at >= t) {
                self.last_event_at = Some(r.observed_at);
                self.last_event = Some(LastEvent {
                    at: r.observed_at,
                    kind: r.kind,
                    provider: r.provider.to_string(),
                    session_id: r.session_id,
                    project_id: r.project_id,
                    tool: r.tool.map(str::to_string),
                });
            }
        }
        let i = *self.session_index.entry(r.session_id).or_insert_with(|| {
            self.sessions.push((
                r.session_id,
                SessionFacts {
                    device_id: r.device_id,
                    provider_session_id: r.provider_session_id.to_string(),
                    provider: r.provider.to_string(),
                    project_id: r.project_id,
                    captured: 0,
                    reconstructed: 0,
                    last_event_at: None,
                    last_kind: None,
                },
            ));
            self.sessions.len() - 1
        });
        let s = &mut self.sessions[i].1;
        if r.reconstructed {
            s.reconstructed += 1;
        } else {
            s.captured += 1;
        }
        if r.kind != EventKind::CaptureTest && s.last_event_at.is_none_or(|t| r.observed_at >= t) {
            s.last_event_at = Some(r.observed_at);
            s.last_kind = Some(r.kind);
        }
        let d = self
            .devices
            .entry((r.device_id, is_meta_kind(r.kind)))
            .or_default();
        d.events += 1;
        d.sessions.insert(r.session_id);
        d.providers.insert(r.provider.to_string());
        d.first_observed_at = min_ts(d.first_observed_at, Some(r.observed_at));
        d.last_observed_at = max_ts(d.last_observed_at, Some(r.observed_at));
        d.last_ingested_at = max_ts(d.last_ingested_at, r.ingested_at);
    }

    /// Add `other`, which follows `self` in stream order.
    pub fn absorb(&mut self, other: &StreamFacts) {
        self.events += other.events;
        self.reconstructed += other.reconstructed;
        for (pid, info) in &other.projects {
            let p = self.projects.entry(*pid).or_insert_with(|| ProjectFacts {
                events: 0,
                sessions: HashSet::new(),
                repo_remote: None,
                ..info.clone()
            });
            p.events += info.events;
            if p.repo_remote.is_none() {
                p.repo_remote = info.repo_remote.clone();
            }
            p.sessions.extend(info.sessions.iter().copied());
        }
        for (name, info) in &other.providers {
            let pr = self
                .providers
                .entry(name.clone())
                .or_insert_with(|| ProviderFacts {
                    provider: name.clone(),
                    ..Default::default()
                });
            pr.events += info.events;
            pr.last_event_at = max_ts(pr.last_event_at, info.last_event_at);
        }
        if other
            .last_event_at
            .is_some_and(|t| self.last_event_at.is_none_or(|mine| t >= mine))
        {
            self.last_event_at = other.last_event_at;
            self.last_event = other.last_event.clone();
        }
        for (sid, f) in &other.sessions {
            match self.session_index.get(sid) {
                Some(&i) => {
                    let mine = &mut self.sessions[i].1;
                    mine.captured += f.captured;
                    mine.reconstructed += f.reconstructed;
                    if f.last_event_at
                        .is_some_and(|t| mine.last_event_at.is_none_or(|m| t >= m))
                    {
                        mine.last_event_at = f.last_event_at;
                        mine.last_kind = f.last_kind;
                    }
                }
                None => {
                    self.session_index.insert(*sid, self.sessions.len());
                    self.sessions.push((*sid, f.clone()));
                }
            }
        }
        for (key, d) in &other.devices {
            let mine = self.devices.entry(*key).or_default();
            mine.events += d.events;
            mine.sessions.extend(d.sessions.iter().copied());
            mine.providers.extend(d.providers.iter().cloned());
            mine.first_observed_at = min_ts(mine.first_observed_at, d.first_observed_at);
            mine.last_observed_at = max_ts(mine.last_observed_at, d.last_observed_at);
            mine.last_ingested_at = max_ts(mine.last_ingested_at, d.last_ingested_at);
        }
    }

    pub fn session(&self, sid: &SessionId) -> Option<&SessionFacts> {
        self.session_index.get(sid).map(|&i| &self.sessions[i].1)
    }

    pub fn has_session(&self, sid: &SessionId) -> bool {
        self.session_index.contains_key(sid)
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Resolve a project argument: a `prj_` id (or bare uuid), a
    /// normalised remote (`host/owner/repo`, in any spelling
    /// `normalise_remote` accepts), a project name (exact, case-insensitive,
    /// or the last path components), or a logical root. `Err` carries the
    /// known projects for the message.
    pub fn resolve_project(
        &self,
        spec: &str,
    ) -> std::result::Result<ProjectId, Vec<&ProjectFacts>> {
        let spec = spec.trim();
        if let Ok(pid) = spec.parse::<ProjectId>()
            && self.projects.contains_key(&pid)
        {
            return Ok(pid);
        }
        let remote = attemptdb_core::event::normalise_remote(spec);
        if let Some(p) = self
            .projects
            .values()
            .find(|p| remote.is_some() && p.repo_remote == remote)
        {
            return Ok(p.project_id);
        }
        let spec_norm = attemptdb_core::PortablePath::from_raw(spec, None).logical;
        if let Some(p) = self.projects.values().find(|p| {
            p.name.eq_ignore_ascii_case(spec)
                || p.root == spec_norm
                || p.name.ends_with(&format!("/{spec}"))
        }) {
            return Ok(p.project_id);
        }
        Err(self.projects.values().collect())
    }

    /// The project of a repository: by normalised remote first, then by
    /// logical root.
    pub fn project_of(&self, root_logical: &str, remote: Option<&str>) -> Option<ProjectId> {
        if let Some(r) = remote
            && let Some(p) = self
                .projects
                .values()
                .find(|p| p.repo_remote.as_deref() == Some(r))
        {
            return Some(p.project_id);
        }
        self.projects
            .values()
            .find(|p| p.root == root_logical)
            .map(|p| p.project_id)
    }

    /// Resolve a session argument: a `ses_` id (full or short), or a
    /// provider session id (full or prefix).
    pub fn resolve_session(&self, spec: &str) -> Option<SessionId> {
        if let Ok(sid) = spec.parse::<SessionId>()
            && self.has_session(&sid)
        {
            return Some(sid);
        }
        let needle = spec.trim_start_matches("ses_");
        self.sessions
            .iter()
            .find(|(sid, f)| {
                f.provider_session_id == spec
                    || sid.short() == spec
                    || sid.to_string().starts_with(needle)
                    || sid.0.simple().to_string().starts_with(needle)
                    || f.provider_session_id.starts_with(spec)
            })
            .map(|(sid, _)| *sid)
    }
}
