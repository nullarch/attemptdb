//! A document and the code it describes must not drift apart silently
//! (REPORT §6.15): RFC 0003 names the projection version it describes in its
//! header and in §5, and a test notices when they stop being the version the
//! code stamps on every inferred row.

use attemptdb_project::ALGORITHM_VERSION;
use std::path::PathBuf;

fn rfc() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/rfcs/0003-fact-inference-bitemporal-model.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every `tier1-v<n>` in `text` that is a *claim about the current version*:
/// the Implementation row of the header and the §5 heading.
fn current_version_claims(text: &str) -> Vec<String> {
    let mut claims = Vec::new();
    for line in text.lines() {
        let header_row = line.starts_with("| **Implementation**");
        let section = line.starts_with("## 5. Tier 1: deterministic projection");
        if !(header_row || section) {
            continue;
        }
        let mut rest = line;
        while let Some(i) = rest.find("tier1-v") {
            let tail = &rest[i..];
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .unwrap_or(tail.len());
            claims.push(tail[..end].to_string());
            rest = &tail[end..];
        }
    }
    claims
}

#[test]
fn rfc_0003_names_the_version_the_code_stamps() {
    let claims = current_version_claims(&rfc());
    assert!(
        claims.len() >= 2,
        "the header's Implementation row and the §5 heading both state the version: {claims:?}"
    );
    for c in &claims {
        assert_eq!(
            c, ALGORITHM_VERSION,
            "RFC 0003 says {c}, the code stamps {ALGORITHM_VERSION}: bump the RFC with the code"
        );
    }
}

#[test]
fn the_rfc_says_what_is_planned() {
    let text = rfc();
    assert!(text.contains("## What is implemented today"));
    for planned in ["inference_id", "AS KNOWN AT", "inputs_hash"] {
        assert!(
            text.contains(planned),
            "the RFC mentions {planned}, so it must say it is planned"
        );
    }
    assert!(text.contains("**Planned, not implemented.**"));
}
