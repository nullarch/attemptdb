//! The device's inference source for `attempt sync --send-inferences`:
//! Tier-1 projections (`attemptdb-project`) turned into
//! `attemptdb.inference/v1` items, each carrying its evidence ids,
//! confidence, and algorithm version. Nothing here decides *whether* they
//! leave the device — that is the sync policy in `attemptdb-capture`.

use anyhow::Result;
use attemptdb_capture::sync::{EventFeed, InferenceItem, InferenceSet, InferenceSource};
use attemptdb_core::{Event, EventId};
use attemptdb_project::{ALGORITHM_VERSION, Projection, Projector};
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

fn item<T: Serialize>(
    kind: &str,
    id: String,
    session_id: Option<String>,
    project_id: Option<String>,
    evidence: &[EventId],
    confidence: f32,
    row: &T,
) -> Result<InferenceItem> {
    let mut fields = serde_json::to_value(row)?;
    if let Some(obj) = fields.as_object_mut() {
        // Provenance travels at the top level of the item, once.
        for key in ["evidence", "confidence", "algorithm_version"] {
            obj.remove(key);
        }
    } else {
        fields = Value::Object(Default::default());
    }
    Ok(InferenceItem {
        kind: kind.to_string(),
        id,
        session_id,
        project_id,
        evidence: evidence.to_vec(),
        confidence,
        algorithm_version: ALGORITHM_VERSION.to_string(),
        fields,
    })
}

/// Project the events as the uploader feeds them (one at a time, never
/// held: the projector keeps the few fields a projection needs of each) and
/// convert the four user-facing tables.
pub fn compute_streaming(feed: EventFeed<'_>) -> Result<InferenceSet> {
    let mut projector = Projector::new();
    feed(&mut |e: &Event| {
        projector.push(e);
    })?;
    convert(projector.finish())
}

fn convert(p: Projection) -> Result<InferenceSet> {
    let mut items = Vec::new();
    for a in &p.attempts {
        items.push(item(
            "attempt",
            a.attempt_id.to_string(),
            Some(a.session_id.to_string()),
            None,
            &a.evidence,
            a.confidence,
            a,
        )?);
    }
    for h in &p.handoffs {
        items.push(item(
            "handoff",
            format!("{}:{}", h.from_session, h.to_session),
            Some(h.to_session.to_string()),
            Some(h.project_id.to_string()),
            &h.evidence,
            h.confidence,
            h,
        )?);
    }
    for w in &p.work_units {
        items.push(item(
            "work_unit",
            w.work_unit_id.to_string(),
            None,
            Some(w.project_id.to_string()),
            &w.evidence,
            w.confidence,
            w,
        )?);
    }
    for d in &p.decisions {
        items.push(item(
            "decision",
            d.decision_id.to_string(),
            Some(d.session_id.to_string()),
            None,
            &d.evidence,
            d.confidence,
            d,
        )?);
    }
    Ok(InferenceSet {
        algorithm_version: ALGORITHM_VERSION.to_string(),
        computed_at: p.reference_time,
        items,
    })
}

pub fn source() -> InferenceSource {
    InferenceSource(Arc::new(compute_streaming))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_stream_yields_an_empty_versioned_set() {
        let set = compute_streaming(&mut |_visit| Ok(())).unwrap();
        assert_eq!(set.algorithm_version, ALGORITHM_VERSION);
        assert!(set.items.is_empty());
    }
}
