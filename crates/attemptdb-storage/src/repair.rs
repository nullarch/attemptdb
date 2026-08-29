//! Repair of a damaged database directory (`attempt repair`).
//!
//! [`plan`] is a read-only analysis that works even when `Database::open`
//! fails (every manifest generation corrupt, identity file missing). It
//! yields a list of [`RepairAction`]s plus a list of problems repair cannot
//! fix. [`apply`] takes the writer lock, re-analyses the directory, and
//! executes exactly those actions of the given plan that still hold, so a
//! stale plan can never do something the user did not see.
//!
//! Rules (`docs/storage-format.md` §5.4, §8.3, §9.4):
//!
//! - A segment file is never deleted. Corrupt, overlapping, or unverifiable
//!   files move to `segments/quarantine/`; damaged files elsewhere are
//!   renamed to `<name>.corrupt` next to their original location.
//! - An unreferenced segment is adopted only after full verification (Arrow
//!   readable, format version, row count, SHA-256) and only when its
//!   `source_seq` range does not overlap a live segment; otherwise adopting
//!   it would duplicate events, so it is quarantined instead.
//! - Torn WAL and spool tails are cut at the end of the last good record,
//!   never earlier.
//! - The new manifest generation is written before any file is moved, so a
//!   crash in the middle of a repair leaves a database that still opens.
//! - `apply` is idempotent: a second `plan` after a successful `apply`
//!   proposes nothing.

use crate::format::{
    FILE_HEADER_LEN, IDENTITY_FILE, LOCK_FILE, MAGIC_SPOOL, MAGIC_WAL, MANIFEST_DIR,
    MANIFEST_FORMAT_VERSION, SEGMENTS_DIR, SPOOL_DIR, WAL_DIR, record_type,
};
use crate::frame::{FrameReader, Record};
use crate::identity::Identity;
use crate::manifest::{Manifest, SegmentMeta, WalState};
use crate::segment;
use crate::spool::{INBOX_COMMITTED_FILE, INBOX_FILE};
use crate::wal::sync_dir;
use crate::{IoAt, Result, StorageError};
use attemptdb_core::{DeviceId, Hlc, Timestamp};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Subdirectory of `segments/` that receives quarantined segment files.
pub const QUARANTINE_DIR: &str = "quarantine";
/// Suffix appended to quarantined files outside `segments/`.
pub const CORRUPT_SUFFIX: &str = "corrupt";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RepairAction {
    /// Reference a verified segment file the current generation does not list.
    AdoptSegment {
        file: String,
        rows: u64,
        sha256: String,
        min_seq: u64,
        max_seq: u64,
    },
    /// Move a file out of the way (see [`quarantine_target`]); nothing is deleted.
    QuarantineFile { path: PathBuf, reason: String },
    /// Delete the remains of an interrupted atomic write.
    RemoveStaleTmp { path: PathBuf },
    /// Delete a tombstoned segment file whose data lives in newer segments.
    RemoveUnreferencedTombstoned { file: String },
    /// Write a manifest generation from scratch because no valid one exists.
    RebuildManifest {
        from_generation: u64,
        segments: Vec<String>,
    },
    /// Cut a WAL or spool file at the end of its last good record.
    TruncateTornTail { path: PathBuf, at: u64 },
    /// Rewrite the `ATTEMPTDB` identity file from the manifest's identifiers.
    RecreateIdentity { db_id: Uuid, device_id: DeviceId },
}

impl RepairAction {
    /// Whether the action moves, cuts, or deletes user data and therefore
    /// deserves an explicit confirmation.
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            RepairAction::QuarantineFile { .. }
                | RepairAction::RemoveUnreferencedTombstoned { .. }
                | RepairAction::TruncateTornTail { .. }
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RepairPlan {
    pub actions: Vec<RepairAction>,
    /// Things repair cannot fix, with advice.
    pub problems: Vec<String>,
}

impl RepairPlan {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty() && self.problems.is_empty()
    }

    pub fn needs_confirmation(&self) -> bool {
        self.actions.iter().any(RepairAction::is_destructive)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RepairReport {
    pub applied: Vec<RepairAction>,
    pub skipped: Vec<(RepairAction, String)>,
    pub new_generation: Option<u64>,
}

/// Read-only analysis of `root`. Works without a valid manifest or identity
/// file; fails only when the directory does not look like a database at all.
pub fn plan(root: &Path) -> Result<RepairPlan> {
    Ok(analyze(root)?.plan)
}

/// Execute `plan` under the writer lock. Actions that no longer match the
/// on-disk state are skipped with a reason; nothing outside the plan is done.
pub fn apply(root: &Path, plan: &RepairPlan) -> Result<RepairReport> {
    let _lock = try_writer_lock(root)?;
    let analysis = analyze(root)?;
    let mut report = RepairReport::default();
    const STALE: &str =
        "no longer applicable: the database changed since the plan was made; run the plan again";

    // 1. The new manifest generation first: it references only files that
    //    exist and are verified, so a crash after this point leaves a
    //    database that opens with everything recovered so far.
    let manifest_written = match &analysis.new_manifest {
        Some(next)
            if analysis
                .manifest_actions
                .iter()
                .all(|a| plan.actions.contains(a)) =>
        {
            let mut next = next.clone();
            next.write(root)?;
            report.new_generation = Some(next.generation);
            true
        }
        Some(_) => false,
        None => true,
    };

    // 2. Everything else in plan order. Segment quarantines that the new
    //    generation depends on run only once that generation is durable.
    for action in &plan.actions {
        let in_manifest = analysis.manifest_actions.contains(action);
        let in_files = analysis.file_actions.contains(action);
        if !in_manifest && !in_files {
            report.skipped.push((action.clone(), STALE.into()));
            continue;
        }
        if in_manifest && !manifest_written {
            report.skipped.push((
                action.clone(),
                "the plan does not contain every action the new manifest generation depends on; run the plan again".into(),
            ));
            continue;
        }
        match action {
            RepairAction::AdoptSegment { .. } | RepairAction::RebuildManifest { .. } => {
                report.applied.push(action.clone())
            }
            _ => match execute(root, action) {
                Ok(()) => report.applied.push(action.clone()),
                Err(e) => report.skipped.push((action.clone(), e.to_string())),
            },
        }
    }
    Ok(report)
}

/// Where [`RepairAction::QuarantineFile`] moves `path`: segment files go to
/// `segments/quarantine/<file>`, anything else to `<path>.corrupt`. A numeric
/// suffix is added when the target already exists.
pub fn quarantine_target(root: &Path, path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let segments_dir = root.join(SEGMENTS_DIR);
    let base = if path.parent() == Some(segments_dir.as_path()) {
        segments_dir.join(QUARANTINE_DIR).join(&name)
    } else {
        path.with_file_name(format!("{name}.{CORRUPT_SUFFIX}"))
    };
    if !base.exists() {
        return base;
    }
    let stem = base
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    (1u32..)
        .map(|n| base.with_file_name(format!("{stem}.{n}")))
        .find(|p| !p.exists())
        .expect("unbounded range")
}

/// Take the writer lock without opening the database. `Locked` when another
/// writer (daemon, CLI) holds it.
pub(crate) fn try_writer_lock(root: &Path) -> Result<File> {
    let lock_path = root.join(LOCK_FILE);
    let f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .at(&lock_path)?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(std::fs::TryLockError::WouldBlock) => Err(StorageError::Locked(root.to_path_buf())),
        Err(std::fs::TryLockError::Error(e)) => Err(StorageError::io(&lock_path, e)),
    }
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

struct Analysis {
    plan: RepairPlan,
    /// Actions realised by writing `new_manifest`: adoptions, a rebuild, and
    /// the quarantine of segments the current generation references.
    manifest_actions: Vec<RepairAction>,
    /// Every other action, executed file by file.
    file_actions: Vec<RepairAction>,
    new_manifest: Option<Manifest>,
}

enum GenState {
    Valid(Manifest),
    /// Checksum fine, but a referenced segment file is gone (rejected on open).
    MissingSegments(Manifest, Vec<String>),
    Corrupt(String),
    Unsupported(u16),
}

struct GenFile {
    number: u64,
    path: PathBuf,
    state: GenState,
    /// Identifiers of a parseable document, even one whose checksum failed.
    ids: Option<(Uuid, DeviceId)>,
}

enum SegState {
    Verified(Box<SegmentMeta>, DeviceId),
    Corrupt(String),
    Unsupported(u16),
}

fn analyze(root: &Path) -> Result<Analysis> {
    let identity_path = root.join(IDENTITY_FILE);
    let manifest_dir = root.join(MANIFEST_DIR);
    let segments_dir = root.join(SEGMENTS_DIR);
    if !root.is_dir()
        || (!identity_path.exists() && !manifest_dir.is_dir() && !segments_dir.is_dir())
    {
        return Err(StorageError::NotADatabase(root.to_path_buf()));
    }
    let mut file_actions: Vec<RepairAction> = Vec::new();
    let mut manifest_actions: Vec<RepairAction> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();

    // --- identity -----------------------------------------------------------
    let (identity, recreate_identity) = match Identity::load(root) {
        Ok(id) => (Some(id), false),
        Err(StorageError::NotADatabase(_)) => (None, true),
        Err(StorageError::Corrupt { detail, .. }) => {
            file_actions.push(RepairAction::QuarantineFile {
                path: identity_path.clone(),
                reason: format!("identity file does not parse: {detail}"),
            });
            (None, true)
        }
        Err(StorageError::UnsupportedFormat {
            found, supported, ..
        }) => {
            problems.push(format!(
                "identity file declares format version {found}; this build supports {supported}. Upgrade attemptdb: repair refuses to touch a newer format"
            ));
            return Ok(Analysis {
                plan: RepairPlan {
                    actions: Vec::new(),
                    problems,
                },
                manifest_actions,
                file_actions,
                new_manifest: None,
            });
        }
        Err(e) => return Err(e),
    };

    // --- stale temp files ---------------------------------------------------
    let mut stale = Vec::new();
    for dir in [&segments_dir, &manifest_dir] {
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(dir).at(dir)? {
            let path = entry.at(dir)?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("tmp") && path.is_file() {
                stale.push(path);
            }
        }
    }
    let identity_tmp = root.join(format!("{IDENTITY_FILE}.tmp"));
    if identity_tmp.is_file() {
        stale.push(identity_tmp);
    }
    stale.sort();
    file_actions.extend(
        stale
            .into_iter()
            .map(|path| RepairAction::RemoveStaleTmp { path }),
    );

    // --- manifest generations ----------------------------------------------
    let gens = list_generations(root)?;
    let highest_number = gens.first().map(|g| g.number).unwrap_or(0);
    let selected_idx = gens
        .iter()
        .position(|g| matches!(g.state, GenState::Valid(_)));
    for (i, g) in gens.iter().enumerate() {
        match &g.state {
            GenState::Valid(_) => {}
            GenState::Corrupt(reason) => file_actions.push(RepairAction::QuarantineFile {
                path: g.path.clone(),
                reason: format!("manifest generation {} {reason}", g.number),
            }),
            GenState::MissingSegments(_, missing) => {
                // Above the selected generation it is rejected on every open;
                // below it, missing files are the normal remains of garbage
                // collection and the generation is just retained history.
                if selected_idx.is_none_or(|s| i < s) {
                    file_actions.push(RepairAction::QuarantineFile {
                        path: g.path.clone(),
                        reason: format!(
                            "manifest generation {} is rejected on open: it references missing segment(s) {}",
                            g.number,
                            missing.join(", ")
                        ),
                    });
                }
            }
            GenState::Unsupported(found) => problems.push(format!(
                "manifest generation {} uses format version {found} (this build supports {MANIFEST_FORMAT_VERSION}); upgrade attemptdb",
                g.number
            )),
        }
    }

    // --- segment files on disk ----------------------------------------------
    let mut on_disk: BTreeMap<String, SegState> = BTreeMap::new();
    if segments_dir.is_dir() {
        for entry in std::fs::read_dir(&segments_dir).at(&segments_dir)? {
            let entry = entry.at(&segments_dir)?;
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            if name.starts_with("seg-") && name.ends_with(".arrow") && path.is_file() {
                on_disk.insert(name.clone(), inspect_segment(&path, &name));
            }
        }
    }
    for (name, state) in &on_disk {
        if let SegState::Unsupported(found) = state {
            problems.push(format!(
                "segment {name} uses format version {found}, newer than this build supports; upgrade attemptdb (the file is left untouched)"
            ));
        }
    }

    // --- base manifest --------------------------------------------------------
    let selected: Option<&Manifest> = selected_idx.and_then(|i| match &gens[i].state {
        GenState::Valid(m) => Some(m),
        _ => None,
    });
    let rebuild_base: Option<(u64, &Manifest)> = if selected.is_none() {
        gens.iter().find_map(|g| match &g.state {
            GenState::MissingSegments(m, _) => Some((g.number, m)),
            _ => None,
        })
    } else {
        None
    };
    let rebuild = selected.is_none() && (!gens.is_empty() || !on_disk.is_empty());

    // Identifiers: identity file, else any parseable generation, else what
    // the segment rows say about the device (the db id is then invented).
    let ids_from_manifest = selected
        .map(|m| (m.db_id, m.device_id))
        .or_else(|| rebuild_base.map(|(_, m)| (m.db_id, m.device_id)))
        .or_else(|| gens.iter().find_map(|g| g.ids));
    let (db_id, device_id) = match (&identity, ids_from_manifest) {
        (Some(id), _) => (id.db_id, id.device_id),
        (None, Some(ids)) => ids,
        (None, None) => {
            let device = on_disk
                .values()
                .find_map(|s| match s {
                    SegState::Verified(_, dev) => Some(*dev),
                    _ => None,
                })
                .unwrap_or_default();
            let db_id = Uuid::now_v7();
            problems.push(format!(
                "no identity file and no parseable manifest: the db_id could not be recovered, a new one ({db_id}) is used; device_id {} comes from the segment rows",
                device
            ));
            (db_id, device)
        }
    };

    let base: Manifest = match (selected, rebuild_base) {
        (Some(m), _) => m.clone(),
        (None, Some((_, m))) => m.clone(),
        (None, None) => Manifest::initial(db_id, device_id),
    };

    // --- referenced segments --------------------------------------------------
    let mut live: Vec<SegmentMeta> = Vec::new();
    let mut lost: Vec<(SegmentMeta, String)> = Vec::new();
    for meta in &base.segments {
        match on_disk.get(&meta.file) {
            None => lost.push((meta.clone(), "the file is missing".into())),
            Some(SegState::Unsupported(_)) => live.push(meta.clone()),
            Some(SegState::Corrupt(reason)) => {
                manifest_actions.push(RepairAction::QuarantineFile {
                    path: segments_dir.join(&meta.file),
                    reason: reason.clone(),
                });
                lost.push((meta.clone(), reason.clone()));
            }
            Some(SegState::Verified(actual, _)) => {
                let reason = if actual.sha256 != meta.sha256 {
                    Some(format!(
                        "sha256 mismatch (manifest {}, file {})",
                        meta.sha256, actual.sha256
                    ))
                } else if actual.rows != meta.rows {
                    Some(format!(
                        "row count {} != manifest {}",
                        actual.rows, meta.rows
                    ))
                } else {
                    None
                };
                match reason {
                    Some(reason) => {
                        manifest_actions.push(RepairAction::QuarantineFile {
                            path: segments_dir.join(&meta.file),
                            reason: reason.clone(),
                        });
                        lost.push((meta.clone(), reason));
                    }
                    None => live.push(meta.clone()),
                }
            }
        }
    }

    // --- tombstones ------------------------------------------------------------
    let tombstoned: BTreeSet<String> = base.tombstones.iter().map(|t| basename(&t.file)).collect();
    for t in &base.tombstones {
        let name = basename(&t.file);
        if t.since_generation < base.generation && on_disk.contains_key(&name) {
            file_actions.push(RepairAction::RemoveUnreferencedTombstoned { file: name });
        }
    }

    // --- unreferenced segments -------------------------------------------------
    let mut candidates: Vec<SegmentMeta> = Vec::new();
    for (name, state) in &on_disk {
        if base.segments.iter().any(|s| &s.file == name) || tombstoned.contains(name) {
            continue;
        }
        match state {
            SegState::Verified(meta, _) => candidates.push((**meta).clone()),
            SegState::Corrupt(reason) => file_actions.push(RepairAction::QuarantineFile {
                path: segments_dir.join(name),
                reason: format!("unreferenced segment cannot be verified: {reason}"),
            }),
            SegState::Unsupported(_) => {}
        }
    }
    // Widest coverage first so that, among overlapping orphans, the one
    // holding the most of the sequence is the one adopted.
    candidates.sort_by(|a, b| {
        let span = |m: &SegmentMeta| m.max_source_seq - m.min_source_seq;
        span(b)
            .cmp(&span(a))
            .then(b.rows.cmp(&a.rows))
            .then(a.file.cmp(&b.file))
    });
    let mut adopted: Vec<SegmentMeta> = Vec::new();
    for c in candidates {
        if let Some(other) = live.iter().chain(adopted.iter()).find(|s| overlaps(s, &c)) {
            file_actions.push(RepairAction::QuarantineFile {
                path: segments_dir.join(&c.file),
                reason: format!(
                    "source_seq {}..{} overlaps segment {} ({}..{}); adopting it would duplicate events",
                    c.min_source_seq, c.max_source_seq, other.file, other.min_source_seq, other.max_source_seq
                ),
            });
        } else {
            if !rebuild {
                manifest_actions.push(RepairAction::AdoptSegment {
                    file: c.file.clone(),
                    rows: c.rows,
                    sha256: c.sha256.clone(),
                    min_seq: c.min_source_seq,
                    max_seq: c.max_source_seq,
                });
            }
            adopted.push(c);
        }
    }

    // --- what is lost ------------------------------------------------------------
    let live_ranges: Vec<(u64, u64)> = live
        .iter()
        .chain(adopted.iter())
        .map(|s| (s.min_source_seq, s.max_source_seq))
        .collect();
    for (meta, reason) in &lost {
        let missing = subtract_range((meta.min_source_seq, meta.max_source_seq), &live_ranges);
        if missing.is_empty() {
            continue;
        }
        problems.push(format!(
            "segment {} ({} rows, source_seq {}..{}) is dropped from the manifest: {}. Events with source_seq {} are no longer in any segment; they come back on the next open only if the WAL still holds them, otherwise they are lost. The file is kept under segments/{QUARANTINE_DIR}/ for manual inspection",
            meta.file,
            meta.rows,
            meta.min_source_seq,
            meta.max_source_seq,
            reason,
            format_ranges(&missing)
        ));
    }

    // --- WAL --------------------------------------------------------------------
    let wal_dir = root.join(WAL_DIR);
    let mut wal_numbers: Vec<u64> = Vec::new();
    let mut wal_max_seq = 0u64;
    let mut wal_max_hlc = Hlc::default();
    let mut undecodable = 0usize;
    if wal_dir.is_dir() {
        let mut files: Vec<(u64, PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(&wal_dir).at(&wal_dir)? {
            let entry = entry.at(&wal_dir)?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(n) = name
                .strip_suffix(".wal")
                .and_then(|s| s.parse::<u64>().ok())
            {
                files.push((n, entry.path()));
            }
        }
        files.sort();
        for (n, path) in files {
            wal_numbers.push(n);
            let records = analyze_framed(
                &path,
                &rel(&path),
                MAGIC_WAL,
                "WAL file",
                &mut file_actions,
                &mut problems,
            )?;
            for r in records
                .iter()
                .filter(|r| r.record_type == record_type::EVENT)
            {
                match r.decode_event() {
                    Ok(ev) => {
                        wal_max_seq = wal_max_seq.max(ev.source_seq);
                        wal_max_hlc = wal_max_hlc.max(ev.hlc);
                    }
                    Err(_) => undecodable += 1,
                }
            }
        }
    }
    if undecodable > 0 {
        problems.push(format!(
            "{undecodable} WAL record(s) pass their checksum but do not decode as events; the writer skips them on open (a newer build may be able to read them)"
        ));
    }

    // --- spool ------------------------------------------------------------------
    let spool_dir = root.join(SPOOL_DIR);
    if spool_dir.is_dir() {
        // Hooks append under this lock; holding it briefly keeps a record
        // that is being written from looking like a torn tail.
        let lock = spool_lock(root)?;
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&spool_dir).at(&spool_dir)? {
            let path = entry.at(&spool_dir)?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("spool") && path.is_file() {
                files.push(path);
            }
        }
        files.sort();
        let mut undecodable = 0usize;
        for path in files {
            let records = analyze_framed(
                &path,
                &rel(&path),
                MAGIC_SPOOL,
                "spool file",
                &mut file_actions,
                &mut problems,
            )?;
            undecodable += records
                .iter()
                .filter(|r| r.record_type == record_type::EVENT && r.decode_event().is_err())
                .count();
        }
        let _ = lock.unlock();
        if undecodable > 0 {
            problems.push(format!(
                "{undecodable} spool record(s) pass their checksum but do not decode as events; import skips them"
            ));
        }
    }

    // --- new manifest generation ---------------------------------------------------
    let new_manifest = if rebuild || !manifest_actions.is_empty() {
        let mut next = base.clone();
        next.generation = highest_number + 1;
        next.created_at = Timestamp::now();
        next.checksum = None;
        let mut segments: Vec<SegmentMeta> = live.iter().chain(adopted.iter()).cloned().collect();
        segments.sort_by_key(|a| (a.min_hlc, a.min_source_seq));
        next.last_source_seq = segments
            .iter()
            .map(|s| s.max_source_seq)
            .chain([base.last_source_seq, wal_max_seq])
            .max()
            .unwrap_or(0);
        next.last_hlc = segments
            .iter()
            .map(|s| s.max_hlc)
            .chain([base.last_hlc, wal_max_hlc])
            .max()
            .unwrap_or_default();
        if rebuild {
            next.wal = WalState {
                active_file: wal_numbers.last().copied().unwrap_or(0),
                checkpoint_offset: 0,
            };
        }
        // Dangling tombstones (file already gone) are dropped; the rest stay
        // until the file is actually removed.
        next.tombstones
            .retain(|t| on_disk.contains_key(&basename(&t.file)));
        if rebuild {
            manifest_actions.push(RepairAction::RebuildManifest {
                from_generation: rebuild_base.map(|(n, _)| n).unwrap_or(0),
                segments: segments.iter().map(|s| s.file.clone()).collect(),
            });
        }
        next.segments = segments;
        Some(next)
    } else {
        None
    };

    // --- identity (last: written after the manifest, like `create`) ------------------
    if recreate_identity {
        file_actions.push(RepairAction::RecreateIdentity { db_id, device_id });
    }

    let mut actions = manifest_actions.clone();
    actions.extend(file_actions.iter().cloned());
    Ok(Analysis {
        plan: RepairPlan { actions, problems },
        manifest_actions,
        file_actions,
        new_manifest,
    })
}

fn list_generations(root: &Path) -> Result<Vec<GenFile>> {
    let dir = root.join(MANIFEST_DIR);
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&dir).at(&dir)? {
        let entry = entry.at(&dir)?;
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(number) = name
            .strip_prefix("gen-")
            .and_then(|s| s.strip_suffix(".json"))
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        let path = entry.path();
        let bytes = std::fs::read(&path).at(&path)?;
        let (state, ids) = match serde_json::from_slice::<Manifest>(&bytes) {
            Err(e) => (GenState::Corrupt(format!("does not parse: {e}")), None),
            Ok(m) => {
                let ids = Some((m.db_id, m.device_id));
                let state = if m.format_version != MANIFEST_FORMAT_VERSION {
                    GenState::Unsupported(m.format_version)
                } else {
                    match manifest_checksum(&m) {
                        Ok(expected) if m.checksum == Some(expected) => {
                            let missing: Vec<String> = m
                                .segments
                                .iter()
                                .filter(|s| !root.join(SEGMENTS_DIR).join(&s.file).exists())
                                .map(|s| s.file.clone())
                                .collect();
                            if missing.is_empty() {
                                GenState::Valid(m)
                            } else {
                                GenState::MissingSegments(m, missing)
                            }
                        }
                        Ok(expected) => GenState::Corrupt(format!(
                            "fails its checksum (stored {:?}, computed {expected})",
                            m.checksum
                        )),
                        Err(e) => GenState::Corrupt(e.to_string()),
                    }
                };
                (state, ids)
            }
        };
        out.push(GenFile {
            number,
            path,
            state,
            ids,
        });
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.number));
    Ok(out)
}

/// CRC-32C of the canonical serialisation (`docs/storage-format.md` §9.2).
fn manifest_checksum(m: &Manifest) -> Result<u32> {
    let mut clone = m.clone();
    clone.checksum = None;
    Ok(crc32c::crc32c(&serde_json::to_vec(&clone)?))
}

/// Verify one segment file and compute the metadata a manifest entry needs.
fn inspect_segment(path: &Path, file: &str) -> SegState {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return SegState::Corrupt(format!("cannot read: {e}")),
    };
    let sha256 = sha256_hex(&bytes);
    let batches = match segment::read_segment_batches(path) {
        Ok(b) => b,
        Err(StorageError::UnsupportedFormat { found, .. }) => return SegState::Unsupported(found),
        Err(StorageError::Corrupt { detail, .. }) => {
            return SegState::Corrupt(format!("not a readable segment: {detail}"));
        }
        Err(e) => return SegState::Corrupt(format!("not a readable segment: {e}")),
    };
    // Decoding trusts the column types; refuse foreign Arrow files up front.
    let canonical = segment::events_schema();
    for b in &batches {
        for f in b.schema().fields() {
            if let Ok(expected) = canonical.field_with_name(f.name())
                && expected.data_type() != f.data_type()
            {
                return SegState::Corrupt(format!(
                    "column {} has type {} (expected {})",
                    f.name(),
                    f.data_type(),
                    expected.data_type()
                ));
            }
        }
    }
    let mut events = Vec::new();
    for b in &batches {
        match segment::batch_to_events(b) {
            Ok(ev) => events.extend(ev),
            Err(e) => return SegState::Corrupt(format!("rows do not decode: {e}")),
        }
    }
    if events.is_empty() {
        return SegState::Corrupt("holds no rows".into());
    }
    let segment_id = file
        .strip_prefix("seg-")
        .and_then(|s| s.strip_suffix(".arrow"))
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(Uuid::now_v7);
    let mut providers = BTreeSet::new();
    let mut projects = BTreeSet::new();
    let mut sessions = BTreeSet::new();
    let mut min_obs = i64::MAX;
    let mut max_obs = i64::MIN;
    let mut min_hlc = u64::MAX;
    let mut max_hlc = 0u64;
    let mut min_seq = u64::MAX;
    let mut max_seq = 0u64;
    let mut min_id = events[0].event_id;
    let mut max_id = events[0].event_id;
    for ev in &events {
        providers.insert(ev.provider.as_str().to_string());
        projects.insert(ev.project.project_id);
        sessions.insert(ev.session_id);
        min_obs = min_obs.min(ev.observed_at.as_micros());
        max_obs = max_obs.max(ev.observed_at.as_micros());
        min_hlc = min_hlc.min(ev.hlc.as_u64());
        max_hlc = max_hlc.max(ev.hlc.as_u64());
        min_seq = min_seq.min(ev.source_seq);
        max_seq = max_seq.max(ev.source_seq);
        min_id = min_id.min(ev.event_id);
        max_id = max_id.max(ev.event_id);
    }
    let meta = SegmentMeta {
        segment_id,
        file: file.to_string(),
        rows: events.len() as u64,
        bytes: bytes.len() as u64,
        min_observed_at: Timestamp::from_micros(min_obs),
        max_observed_at: Timestamp::from_micros(max_obs),
        min_hlc: Hlc(min_hlc),
        max_hlc: Hlc(max_hlc),
        min_source_seq: min_seq,
        max_source_seq: max_seq,
        min_event_id: min_id,
        max_event_id: max_id,
        providers: providers.into_iter().collect(),
        project_ids: projects.into_iter().collect(),
        session_count: sessions.len() as u64,
        sha256,
    };
    SegState::Verified(Box::new(meta), events[0].device_id)
}

/// Scan one framed file (WAL or spool). Bad magic → quarantine; a torn tail
/// → `TruncateTornTail` at the end of the last good record. Returns the valid
/// records so callers can inspect them.
fn analyze_framed(
    path: &Path,
    rel: &str,
    magic: [u8; 4],
    what: &str,
    actions: &mut Vec<RepairAction>,
    problems: &mut Vec<String>,
) -> Result<Vec<Record>> {
    let len = std::fs::metadata(path).at(path)?.len();
    if len < FILE_HEADER_LEN as u64 {
        // A crash between creating the file and writing its header; the
        // writer (or the next hook) starts it over. Nothing to repair.
        return Ok(Vec::new());
    }
    match FrameReader::scan(path, magic) {
        Ok(scan) => {
            if scan.truncated_at.is_some() {
                actions.push(RepairAction::TruncateTornTail {
                    path: path.to_path_buf(),
                    at: scan.valid_len,
                });
                problems.push(format!(
                    "{what} {rel}: {} byte(s) after offset {} are not valid records and cannot be recovered; TruncateTornTail cuts the file there (the writer does the same on open)",
                    scan.total_len.saturating_sub(scan.valid_len),
                    scan.valid_len
                ));
            }
            Ok(scan.records)
        }
        Err(StorageError::Corrupt { detail, .. }) => {
            actions.push(RepairAction::QuarantineFile {
                path: path.to_path_buf(),
                reason: format!("{what} header: {detail}"),
            });
            Ok(Vec::new())
        }
        Err(StorageError::UnsupportedFormat {
            found, supported, ..
        }) => {
            problems.push(format!(
                "{what} {rel} uses format version {found} (this build supports {supported}); upgrade attemptdb"
            ));
            Ok(Vec::new())
        }
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

fn execute(root: &Path, action: &RepairAction) -> Result<()> {
    match action {
        RepairAction::RemoveStaleTmp { path } => {
            std::fs::remove_file(path).at(path)?;
            sync_parent(path)
        }
        RepairAction::QuarantineFile { path, .. } => {
            let lock = if is_spool_file(root, path) {
                Some(spool_lock(root)?)
            } else {
                None
            };
            let target = quarantine_target(root, path);
            if let Some(dir) = target.parent() {
                std::fs::create_dir_all(dir).at(dir)?;
            }
            std::fs::rename(path, &target).at(path)?;
            if is_spool_inbox(root, path) {
                let _ = std::fs::remove_file(root.join(SPOOL_DIR).join(INBOX_COMMITTED_FILE));
            }
            sync_parent(path)?;
            sync_parent(&target)?;
            if let Some(l) = lock {
                let _ = l.unlock();
            }
            Ok(())
        }
        RepairAction::TruncateTornTail { path, at } => {
            let lock = if is_spool_file(root, path) {
                Some(spool_lock(root)?)
            } else {
                None
            };
            let magic = if is_spool_file(root, path) {
                MAGIC_SPOOL
            } else {
                MAGIC_WAL
            };
            // Never cut before the last good record: re-scan and insist that
            // the cut point is still exactly where the plan put it.
            let scan = FrameReader::scan(path, magic)?;
            if scan.truncated_at.is_none() || scan.valid_len != *at {
                return Err(StorageError::Other(format!(
                    "{} changed since the plan was made (valid length {} vs planned {at}); run the plan again",
                    path.display(),
                    scan.valid_len
                )));
            }
            let f = OpenOptions::new().write(true).open(path).at(path)?;
            f.set_len(*at).at(path)?;
            f.sync_all().at(path)?;
            if is_spool_inbox(root, path) {
                let _ = std::fs::remove_file(root.join(SPOOL_DIR).join(INBOX_COMMITTED_FILE));
            }
            if let Some(l) = lock {
                let _ = l.unlock();
            }
            Ok(())
        }
        RepairAction::RemoveUnreferencedTombstoned { file } => {
            let path = root.join(SEGMENTS_DIR).join(file);
            let f = File::open(&path).at(&path)?;
            match f.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(StorageError::Other(format!(
                        "{} is held open by a reader; retry later",
                        path.display()
                    )));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(StorageError::io(&path, e)),
            }
            std::fs::remove_file(&path).at(&path)?;
            drop(f);
            sync_parent(&path)
        }
        RepairAction::RecreateIdentity { db_id, device_id } => {
            let mut id = Identity::new(*device_id);
            id.db_id = *db_id;
            id.created_by = format!("attempt repair {}", env!("CARGO_PKG_VERSION"));
            id.extra.insert(
                "recreated_by_repair_at".into(),
                serde_json::Value::from(Timestamp::now().as_micros()),
            );
            id.write(root)
        }
        RepairAction::AdoptSegment { .. } | RepairAction::RebuildManifest { .. } => {
            Err(StorageError::Other("realised by the manifest write".into()))
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn spool_lock(root: &Path) -> Result<File> {
    let dir = root.join(SPOOL_DIR);
    std::fs::create_dir_all(&dir).at(&dir)?;
    let lock_path = dir.join("inbox.lock");
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .at(&lock_path)?;
    lock.lock().at(&lock_path)?;
    Ok(lock)
}

fn is_spool_file(root: &Path, path: &Path) -> bool {
    path.parent() == Some(root.join(SPOOL_DIR).as_path())
}

fn is_spool_inbox(root: &Path, path: &Path) -> bool {
    is_spool_file(root, path) && path.file_name().and_then(|n| n.to_str()) == Some(INBOX_FILE)
}

fn sync_parent(path: &Path) -> Result<()> {
    match path.parent() {
        Some(dir) if dir.is_dir() => sync_dir(dir),
        _ => Ok(()),
    }
}

fn basename(file: &str) -> String {
    file.rsplit(['/', '\\']).next().unwrap_or(file).to_string()
}

fn overlaps(a: &SegmentMeta, b: &SegmentMeta) -> bool {
    a.min_source_seq <= b.max_source_seq && b.min_source_seq <= a.max_source_seq
}

/// `range` minus every range in `covers`, as sorted disjoint ranges.
fn subtract_range(range: (u64, u64), covers: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out = vec![range];
    for &(a, b) in covers {
        out = out
            .into_iter()
            .flat_map(|(x, y)| {
                if b < x || a > y {
                    vec![(x, y)]
                } else {
                    let mut v = Vec::new();
                    if a > x {
                        v.push((x, a - 1));
                    }
                    if b < y {
                        v.push((b + 1, y));
                    }
                    v
                }
            })
            .collect();
    }
    out.sort_unstable();
    out
}

fn format_ranges(ranges: &[(u64, u64)]) -> String {
    ranges
        .iter()
        .map(|(a, b)| {
            if a == b {
                a.to_string()
            } else {
                format!("{a}..{b}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtract_ranges_reports_uncovered_parts() {
        assert_eq!(subtract_range((21, 40), &[]), vec![(21, 40)]);
        assert_eq!(
            subtract_range((21, 40), &[(1, 20), (41, 60)]),
            vec![(21, 40)]
        );
        assert_eq!(
            subtract_range((21, 40), &[(21, 40)]),
            Vec::<(u64, u64)>::new()
        );
        assert_eq!(
            subtract_range((21, 40), &[(25, 30)]),
            vec![(21, 24), (31, 40)]
        );
        assert_eq!(
            subtract_range((21, 40), &[(10, 25), (35, 50)]),
            vec![(26, 34)]
        );
        assert_eq!(format_ranges(&[(1, 1), (3, 9)]), "1, 3..9");
    }

    #[test]
    fn quarantine_targets_are_unique_and_placed_by_kind() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let seg = root.join(SEGMENTS_DIR).join("seg-a.arrow");
        assert_eq!(
            quarantine_target(root, &seg),
            root.join(SEGMENTS_DIR)
                .join(QUARANTINE_DIR)
                .join("seg-a.arrow")
        );
        let wal = root.join(WAL_DIR).join("000001.wal");
        assert_eq!(
            quarantine_target(root, &wal),
            root.join(WAL_DIR).join("000001.wal.corrupt")
        );
        std::fs::create_dir_all(root.join(WAL_DIR)).unwrap();
        std::fs::write(root.join(WAL_DIR).join("000001.wal.corrupt"), b"x").unwrap();
        assert_eq!(
            quarantine_target(root, &wal),
            root.join(WAL_DIR).join("000001.wal.corrupt.1")
        );
    }
}
