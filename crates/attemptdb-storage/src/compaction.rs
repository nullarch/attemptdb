//! Segment compaction: many small segments become one.
//!
//! A daemon that flushes on a timer, or a server tenant, accumulates
//! hundreds of small segments; every open lists them all and every scan
//! opens them all (`docs/benchmarks.md`: 200 segments instead of 2 cost
//! +6 % on a full scan and 2.3× on open at 100 k events). Compaction merges
//! runs of small segments into one, through the same writer a flush uses,
//! and publishes the result as a new manifest generation. Nothing about an
//! event changes: ids, `source_seq`, `hlc`, and every canonical column are
//! copied; `content`/`raw` travel exactly as stored (inline text or blob
//! ids), so blobs are never rewritten and no key is needed to compact
//! encrypted segments.
//!
//! # Input selection
//!
//! Segments are considered in manifest order, which is `min_hlc` /
//! `source_seq` order (flushes append, compaction puts its output where
//! its first input was). A segment is *small* when its encoded size is
//! below [`CompactionPolicy::small_segment_bytes`]. Maximal runs of
//! consecutive small segments are the candidates; a segment that is not
//! small ends a run and is never rewritten. Without a current encryption
//! key a change of segment format between neighbours also ends a run
//! (inline content cannot be written into a format 2 file without a key;
//! with a key the output is format 2 and inline content is encrypted,
//! exactly as a flush would). Runs shorter than
//! [`CompactionPolicy::min_inputs`] (never fewer than two) are skipped.
//! Nothing happens while the segment count is at most
//! [`CompactionPolicy::max_segments`]; above it, runs are merged whole,
//! oldest first, until the count is within the limit or no eligible run
//! remains. Each run becomes exactly one output segment, and
//! [`crate::Database::compact`] executes one run per call so that every
//! generation is one small, crash-safe step.
//!
//! # Lifecycle of the inputs
//!
//! The generation that introduces the merged segment lists every input in
//! `tombstones[]` with `since_generation` = that generation. A tombstoned
//! file is deleted by [`crate::Database::collect_garbage`] once a *later*
//! generation is durable (the next flush or compaction, or the next writer
//! open), never by the generation that dropped it: a reader that loaded the
//! previous generation keeps reading files that still exist, and a torn
//! newest generation can fall back to the previous one with all of its
//! files present. Deletion tolerates failure (a file held open on Windows)
//! and is retried on a later open.

use crate::format::SEGMENT_FORMAT_VERSION;
use crate::manifest::SegmentMeta;
use serde::Serialize;

/// When and what to compact. The defaults are conservative: a database
/// stays untouched until it holds more than 32 segments, only segments
/// under 8 MiB are ever rewritten, and a run needs at least four of them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CompactionPolicy {
    /// Compact only while the manifest lists more segments than this.
    pub max_segments: usize,
    /// A segment whose encoded size is below this is *small* and may be
    /// merged; larger segments are left as they are.
    pub small_segment_bytes: u64,
    /// Minimum length of a run of small segments worth merging (at least 2).
    pub min_inputs: usize,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            max_segments: 32,
            small_segment_bytes: 8 * 1024 * 1024,
            min_inputs: 4,
        }
    }
}

/// One run of segments that would be merged into one output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlannedRun {
    /// Position of the first input in the manifest's segment list; the
    /// output takes this position.
    pub first_index: usize,
    /// The inputs, in manifest order.
    pub inputs: Vec<SegmentMeta>,
    /// Rows across the inputs.
    pub rows: u64,
    /// Encoded bytes across the inputs.
    pub bytes: u64,
    /// Segment format the output will have (1 inline, 2 blob refs).
    pub format_version: u16,
}

impl PlannedRun {
    pub fn min_source_seq(&self) -> u64 {
        self.inputs
            .iter()
            .map(|s| s.min_source_seq)
            .min()
            .unwrap_or(0)
    }

    pub fn max_source_seq(&self) -> u64 {
        self.inputs
            .iter()
            .map(|s| s.max_source_seq)
            .max()
            .unwrap_or(0)
    }
}

/// What a compaction would do, and why it would do nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CompactionPlan {
    /// The policy the plan was made under.
    pub policy: CompactionPolicy,
    /// Segments listed by the manifest now.
    pub segments_before: usize,
    /// Segments the manifest would list after every planned run.
    pub segments_after: usize,
    /// Runs to merge, oldest first. [`crate::Database::compact`] executes
    /// the first one.
    pub runs: Vec<PlannedRun>,
    /// Why runs were skipped or nothing was planned (human readable).
    pub notes: Vec<String>,
}

impl CompactionPlan {
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }
}

/// What one [`crate::Database::compact`] call did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CompactionReport {
    /// The merged inputs, in manifest order; tombstoned in `generation`.
    pub inputs: Vec<SegmentMeta>,
    /// Encoded bytes of the inputs.
    pub input_bytes: u64,
    /// The merged segment as the new generation lists it.
    pub output_segment: SegmentMeta,
    /// Encoded bytes of the output.
    pub output_bytes: u64,
    /// Events (rows) in the output — equal to the sum over the inputs.
    pub events: u64,
    /// The manifest generation that references the output.
    pub generation: u64,
    /// Tombstoned files still on disk after this call (this run's inputs
    /// plus anything a reader kept open); deleted by a later collection.
    pub pending_deletions: usize,
}

/// Build a plan over `segments` (manifest order). `formats[i]` is the
/// segment format version of `segments[i]` and is consulted only when
/// `encryption_active` is false (with a key every run is writable as
/// format 2); entries of segments that are not small may be anything.
pub(crate) fn plan(
    segments: &[SegmentMeta],
    formats: &[u16],
    policy: &CompactionPolicy,
    encryption_active: bool,
) -> CompactionPlan {
    debug_assert_eq!(segments.len(), formats.len());
    let n = segments.len();
    let min_inputs = policy.min_inputs.max(2);
    let mut plan = CompactionPlan {
        policy: policy.clone(),
        segments_before: n,
        segments_after: n,
        runs: Vec::new(),
        notes: Vec::new(),
    };
    if n <= policy.max_segments {
        plan.notes.push(format!(
            "{n} segment(s), within the limit of {}: nothing to do",
            policy.max_segments
        ));
        return plan;
    }
    let small = |s: &SegmentMeta| s.bytes < policy.small_segment_bytes;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut large = 0usize;
    let mut short_runs = 0usize;
    let mut i = 0;
    while i < n {
        if !small(&segments[i]) {
            large += 1;
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < n && small(&segments[i]) && (encryption_active || formats[i] == formats[start]) {
            i += 1;
        }
        if i - start >= min_inputs {
            runs.push((start, i));
        } else {
            short_runs += 1;
        }
    }
    let mut count = n;
    for (start, end) in runs {
        if count <= policy.max_segments {
            plan.notes.push(format!(
                "run of {} small segment(s) at position {start} left alone: the count is within the limit",
                end - start
            ));
            continue;
        }
        let inputs: Vec<SegmentMeta> = segments[start..end].to_vec();
        plan.runs.push(PlannedRun {
            first_index: start,
            rows: inputs.iter().map(|s| s.rows).sum(),
            bytes: inputs.iter().map(|s| s.bytes).sum(),
            format_version: if encryption_active {
                SEGMENT_FORMAT_VERSION
            } else {
                formats[start]
            },
            inputs,
        });
        count -= (end - start) - 1;
    }
    plan.segments_after = count;
    if plan.runs.is_empty() {
        plan.notes.push(format!(
            "{n} segment(s) exceed the limit of {}, but no run of at least {min_inputs} small segments (< {} bytes) exists: {large} large, {short_runs} run(s) too short{}",
            policy.max_segments,
            policy.small_segment_bytes,
            if encryption_active {
                ""
            } else {
                "; without a key a change of segment format ends a run"
            }
        ));
    } else if count > policy.max_segments {
        plan.notes.push(format!(
            "{count} segment(s) remain after every eligible run, above the limit of {}: the rest are large or in runs shorter than {min_inputs}",
            policy.max_segments
        ));
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::{DeviceId, EventId, Hlc, Timestamp};
    use uuid::Uuid;

    fn seg(i: u64, bytes: u64) -> SegmentMeta {
        SegmentMeta {
            segment_id: Uuid::now_v7(),
            file: format!("seg-{i}.arrow"),
            rows: 10,
            bytes,
            min_observed_at: Timestamp::from_micros(i as i64),
            max_observed_at: Timestamp::from_micros(i as i64),
            min_hlc: Hlc(i),
            max_hlc: Hlc(i),
            min_source_seq: i * 10 + 1,
            max_source_seq: i * 10 + 10,
            min_event_id: EventId::new(),
            max_event_id: EventId::new(),
            providers: vec![],
            project_ids: vec![],
            session_count: 1,
            sha256: String::new(),
        }
    }

    fn policy(max_segments: usize, min_inputs: usize) -> CompactionPolicy {
        CompactionPolicy {
            max_segments,
            small_segment_bytes: 1_000,
            min_inputs,
        }
    }

    fn run_bounds(p: &CompactionPlan) -> Vec<(usize, usize)> {
        p.runs
            .iter()
            .map(|r| (r.first_index, r.first_index + r.inputs.len()))
            .collect()
    }

    #[test]
    fn nothing_within_the_limit() {
        let segs: Vec<SegmentMeta> = (0..5).map(|i| seg(i, 10)).collect();
        let p = plan(&segs, &[1; 5], &policy(5, 2), false);
        assert!(p.is_empty());
        assert_eq!(p.segments_after, 5);
        assert!(p.notes[0].contains("within the limit"));
        let _ = DeviceId::new();
    }

    #[test]
    fn large_segments_split_runs_and_are_never_inputs() {
        // s s s L s s L s s s s  (L = large)
        let sizes = [10, 10, 10, 5_000, 10, 10, 5_000, 10, 10, 10, 10];
        let segs: Vec<SegmentMeta> = sizes
            .iter()
            .enumerate()
            .map(|(i, b)| seg(i as u64, *b))
            .collect();
        let p = plan(&segs, &[1; 11], &policy(2, 3), false);
        assert_eq!(run_bounds(&p), vec![(0, 3), (7, 11)]);
        assert_eq!(p.segments_after, 11 - 2 - 3);
        assert!(p.notes.iter().any(|n| n.contains("remain")));
    }

    #[test]
    fn merges_oldest_first_and_stops_at_the_limit() {
        // 3 small, large, 3 small: limit 5 → only the first run is needed.
        let sizes = [10, 10, 10, 5_000, 10, 10, 10];
        let segs: Vec<SegmentMeta> = sizes
            .iter()
            .enumerate()
            .map(|(i, b)| seg(i as u64, *b))
            .collect();
        let p = plan(&segs, &[1; 7], &policy(5, 2), false);
        assert_eq!(run_bounds(&p), vec![(0, 3)]);
        assert_eq!(p.segments_after, 5);
        assert!(p.notes.iter().any(|n| n.contains("left alone")));
    }

    #[test]
    fn format_boundary_is_a_barrier_without_a_key_only() {
        let segs: Vec<SegmentMeta> = (0..6).map(|i| seg(i, 10)).collect();
        let formats = [1, 1, 1, 2, 2, 2];
        let p = plan(&segs, &formats, &policy(1, 2), false);
        assert_eq!(run_bounds(&p), vec![(0, 3), (3, 6)]);
        assert_eq!(p.runs[0].format_version, 1);
        assert_eq!(p.runs[1].format_version, 2);
        let p = plan(&segs, &formats, &policy(1, 2), true);
        assert_eq!(run_bounds(&p), vec![(0, 6)]);
        assert_eq!(p.runs[0].format_version, 2);
    }

    #[test]
    fn min_inputs_is_at_least_two_and_short_runs_are_reported() {
        let sizes = [10, 5_000, 10, 5_000];
        let segs: Vec<SegmentMeta> = sizes
            .iter()
            .enumerate()
            .map(|(i, b)| seg(i as u64, *b))
            .collect();
        let p = plan(&segs, &[1; 4], &policy(1, 1), false);
        assert!(p.is_empty());
        assert!(p.notes[0].contains("2 run(s) too short"), "{:?}", p.notes);
        assert!(p.notes[0].contains("2 large"));
    }

    #[test]
    fn default_policy_is_conservative() {
        let d = CompactionPolicy::default();
        assert_eq!(d.max_segments, 32);
        assert_eq!(d.small_segment_bytes, 8 * 1024 * 1024);
        assert_eq!(d.min_inputs, 4);
    }
}
