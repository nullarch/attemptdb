//! An inline SVG rendering of a `TRACE` result: nodes per endpoint, one
//! column per depth, edges with arrow heads. Cheap layered layout; capped
//! so a huge graph degrades into a note instead of a wall.

use crate::html::{esc, seg};
use crate::scope::ScopeQuery;
use serde_json::Value;
use std::collections::HashMap;

const MAX_NODES: usize = 60;
const NODE_W: f64 = 168.0;
const NODE_H: f64 = 34.0;
const COL_GAP: f64 = 70.0;
const ROW_GAP: f64 = 14.0;
const MARGIN: f64 = 12.0;

struct Node {
    kind: String,
    id: String,
    layer: usize,
}

fn short(id: &str) -> String {
    let (prefix, rest) = id.split_once('_').unwrap_or(("", id));
    let hex: String = rest.chars().filter(|c| *c != '-').take(8).collect();
    if prefix.is_empty() {
        hex
    } else {
        format!("{prefix}_{hex}")
    }
}

fn href(kind: &str, id: &str, scope: &ScopeQuery) -> Option<String> {
    let page = match kind {
        "attempt" => "attempt",
        "session" => "session",
        "event" => "evidence",
        _ => return None,
    };
    Some(format!("/{page}/{}{}", seg(id), scope.query_string(&[])))
}

/// `rows` are the `TRACE` result rows (`depth`, `edge_kind`, `from_type`,
/// `from_id`, `to_type`, `to_id`, `edge_source`). `subject_type` /
/// `subject_id` name the start node (depth 0). Returns `None` when there is
/// nothing to draw.
pub fn trace_dag(
    rows: &[Value],
    subject_type: &str,
    subject_id: &str,
    scope: &ScopeQuery,
) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let mut nodes: Vec<Node> = vec![Node {
        kind: subject_type.to_string(),
        id: subject_id.to_string(),
        layer: 0,
    }];
    let mut index: HashMap<String, usize> = HashMap::from([(subject_id.to_string(), 0)]);
    let mut edges: Vec<(usize, usize, String, bool)> = Vec::new();
    let mut truncated = false;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    for row in rows {
        let depth = row.get("depth").and_then(Value::as_u64).unwrap_or(1) as usize;
        let from_id = s(row, "from_id");
        let to_id = s(row, "to_id");
        if from_id.is_empty() || to_id.is_empty() {
            continue;
        }
        // The endpoint already known sits one layer nearer the subject; the
        // new one takes `depth`.
        let from_known = index.contains_key(&from_id);
        let to_known = index.contains_key(&to_id);
        let (from_layer, to_layer) = match (from_known, to_known) {
            (true, false) => (0, depth),
            _ => (depth, 0),
        };
        let mut ensure = |id: &str, kind: &str, layer: usize| -> Option<usize> {
            if let Some(&i) = index.get(id) {
                return Some(i);
            }
            if nodes.len() >= MAX_NODES {
                return None;
            }
            nodes.push(Node {
                kind: kind.to_string(),
                id: id.to_string(),
                layer,
            });
            index.insert(id.to_string(), nodes.len() - 1);
            Some(nodes.len() - 1)
        };
        let a = ensure(&from_id, &s(row, "from_type"), from_layer);
        let b = ensure(&to_id, &s(row, "to_type"), to_layer);
        match (a, b) {
            (Some(a), Some(b)) => edges.push((
                a,
                b,
                s(row, "edge_kind"),
                s(row, "edge_source") == "derived",
            )),
            _ => truncated = true,
        }
    }
    if edges.is_empty() {
        return None;
    }
    let max_layer = nodes.iter().map(|n| n.layer).max().unwrap_or(0);
    // Positions: causes (deeper layers) on the left, the subject on the right.
    let mut per_layer: Vec<usize> = vec![0; max_layer + 1];
    let mut pos: Vec<(f64, f64)> = Vec::with_capacity(nodes.len());
    for n in &nodes {
        let col = max_layer - n.layer;
        let row = per_layer[n.layer];
        per_layer[n.layer] += 1;
        let x = MARGIN + col as f64 * (NODE_W + COL_GAP);
        let y = MARGIN + row as f64 * (NODE_H + ROW_GAP);
        pos.push((x, y));
    }
    let rows_max = per_layer.iter().copied().max().unwrap_or(1).max(1);
    let width = MARGIN * 2.0 + (max_layer as f64 + 1.0) * NODE_W + max_layer as f64 * COL_GAP;
    let height = MARGIN * 2.0 + rows_max as f64 * NODE_H + (rows_max as f64 - 1.0) * ROW_GAP;
    let mut svg = format!(
        "<svg class=\"dag\" viewBox=\"0 0 {width:.0} {height:.0}\" width=\"{width:.0}\" height=\"{height:.0}\" role=\"img\" aria-label=\"causal trace\">\
         <defs><marker id=\"arrow\" viewBox=\"0 0 10 10\" refX=\"10\" refY=\"5\" markerWidth=\"7\" markerHeight=\"7\" orient=\"auto-start-reverse\"><path d=\"M 0 0 L 10 5 L 0 10 z\" fill=\"currentColor\"/></marker></defs>"
    );
    for (a, b, kind, derived) in &edges {
        let (ax, ay) = pos[*a];
        let (bx, by) = pos[*b];
        // From the right edge of the cause to the left edge of the effect.
        let (x1, y1) = (ax + NODE_W, ay + NODE_H / 2.0);
        let (x2, y2) = (bx, by + NODE_H / 2.0);
        let mx = (x1 + x2) / 2.0;
        svg.push_str(&format!(
            "<path class=\"edge{}\" d=\"M {x1:.1} {y1:.1} C {mx:.1} {y1:.1}, {mx:.1} {y2:.1}, {x2:.1} {y2:.1}\" marker-end=\"url(#arrow)\"/>\
             <text class=\"edge-label\" x=\"{mx:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
            if *derived { " derived" } else { "" },
            (y1 + y2) / 2.0 - 4.0,
            esc(kind)
        ));
    }
    for (i, n) in nodes.iter().enumerate() {
        let (x, y) = pos[i];
        let label = format!("{} {}", n.kind, short(&n.id));
        let body = format!(
            "<rect class=\"node {}\" x=\"{x:.1}\" y=\"{y:.1}\" width=\"{NODE_W}\" height=\"{NODE_H}\" rx=\"6\"/>\
             <text x=\"{:.1}\" y=\"{:.1}\">{}</text>",
            esc(&n.kind),
            x + 8.0,
            y + NODE_H / 2.0 + 4.0,
            esc(&label)
        );
        match href(&n.kind, &n.id, scope) {
            Some(h) => svg.push_str(&format!(
                "<a href=\"{}\"><title>{}</title>{body}</a>",
                esc(&h),
                esc(&n.id)
            )),
            None => svg.push_str(&format!("<g><title>{}</title>{body}</g>", esc(&n.id))),
        }
    }
    svg.push_str("</svg>");
    if truncated {
        svg.push_str(&format!(
            "<p class=\"muted small\">graph truncated at {MAX_NODES} nodes; the table below lists every edge</p>"
        ));
    }
    Some(svg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn draws_and_escapes() {
        let rows = vec![
            json!({"depth": 1, "edge_kind": "caused", "from_type": "event", "from_id": "ev_0000-1", "to_type": "attempt", "to_id": "att_abcd", "edge_source": "derived"}),
            json!({"depth": 1, "edge_kind": "triggered", "from_type": "event", "from_id": "ev_<b>", "to_type": "attempt", "to_id": "att_abcd", "edge_source": "projection"}),
        ];
        let svg = trace_dag(&rows, "attempt", "att_abcd", &ScopeQuery::default()).unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains("caused"));
        assert!(!svg.contains("<b>"));
        assert!(svg.contains("ev_&lt;b&gt;"));
        assert!(trace_dag(&[], "attempt", "x", &ScopeQuery::default()).is_none());
    }
}
