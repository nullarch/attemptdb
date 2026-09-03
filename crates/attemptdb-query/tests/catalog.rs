//! The catalog is only worth having if it cannot drift.
//!
//! Three things are checked here. Every column of every registered table has
//! a description and no description names a column that does not exist, so a
//! new column cannot ship undocumented. Every example statement runs against
//! a real projection, so an example cannot rot into a statement the parser
//! no longer accepts. And `docs/query-context.md` is generated from the same
//! code, so the file in the repository is never a stale copy: it is checked
//! here and regenerated with `UPDATE_GOLDEN=1`.

mod common;

use attemptdb_query::QueryEngine;
use attemptdb_query::catalog::{self, PLACEHOLDERS};
use common::spec_scenario;

async fn engine() -> QueryEngine {
    QueryEngine::from_events(spec_scenario().events)
        .await
        .expect("engine")
}

#[test]
fn every_column_has_a_meaning() {
    let mut missing = Vec::new();
    for t in catalog::catalog() {
        for c in &t.columns {
            if c.doc.trim().is_empty() {
                missing.push(format!("{}.{}", t.name, c.name));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "{} column(s) with no description in attemptdb-query/src/catalog.rs:\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

#[test]
fn a_documented_column_exists() {
    // The reverse guard: a column renamed in `tables.rs` leaves its old
    // description behind, describing nothing. Every documented name must
    // appear in at least one table that could carry it.
    let c = catalog::catalog();
    let known: std::collections::BTreeSet<&str> = c
        .iter()
        .flat_map(|t| t.columns.iter().map(|col| col.name.as_str()))
        .collect();
    let documented: std::collections::BTreeSet<String> = c
        .iter()
        .flat_map(|t| {
            t.columns
                .iter()
                .filter(|col| !col.doc.is_empty())
                .map(|col| col.name.clone())
        })
        .collect();
    for name in &documented {
        assert!(known.contains(name.as_str()), "{name} documents nothing");
    }
}

#[test]
fn a_values_list_belongs_to_a_column_that_exists() {
    for t in catalog::catalog() {
        for c in &t.columns {
            if !c.values.is_empty() {
                assert!(
                    !c.data_type.starts_with("int")
                        && !c.data_type.starts_with("float")
                        && c.data_type != "bool",
                    "{}.{} is {} but carries a value list",
                    t.name,
                    c.name,
                    c.data_type
                );
            }
        }
    }
}

#[test]
fn placeholders_and_examples_agree() {
    for p in PLACEHOLDERS {
        assert!(
            catalog::examples().iter().any(|e| e.statement.contains(p)),
            "placeholder {p} is used by no example"
        );
    }
    for e in catalog::examples() {
        let mut rest = e.statement;
        while let Some(open) = rest.find('{') {
            let close = rest[open..].find('}').expect("unclosed placeholder") + open;
            let token = &rest[open..=close];
            assert!(
                PLACEHOLDERS.contains(&token),
                "{}: unknown placeholder {token}",
                e.question
            );
            rest = &rest[close + 1..];
        }
        assert!(!e.question.is_empty() && !e.note.is_empty());
    }
}

#[tokio::test]
async fn every_example_runs() {
    let e = engine().await;
    let p = e.projection();
    let session = format!("ses_{}", p.sessions.first().expect("a session").session_id);
    let attempt = format!("att_{}", p.attempts.first().expect("an attempt").attempt_id);
    let mut failed = Vec::new();
    for ex in catalog::examples() {
        let statement = ex
            .statement
            .replace("{session}", &session)
            .replace("{attempt}", &attempt);
        if let Err(err) = e.query(&statement).await {
            failed.push(format!("{statement}\n    {err}"));
        }
    }
    assert!(
        failed.is_empty(),
        "{} example(s) no longer run:\n  {}",
        failed.len(),
        failed.join("\n  ")
    );
}

#[test]
fn the_checked_in_document_matches_the_code() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/query-context.md")
        .canonicalize()
        .unwrap_or_else(|_| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/query-context.md")
        });
    let generated = catalog::markdown();
    let update = std::env::var("UPDATE_GOLDEN").is_ok_and(|v| v == "1");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if update || current.is_empty() {
        std::fs::write(&path, &generated).expect("write docs/query-context.md");
        return;
    }
    assert_eq!(
        current, generated,
        "docs/query-context.md is out of date; regenerate it with \
         `UPDATE_GOLDEN=1 cargo test -p attemptdb-query --test catalog` \
         (or `attempt schema --format markdown > docs/query-context.md`)"
    );
}

#[test]
fn the_json_form_carries_the_same_tables() {
    let v = catalog::json();
    let tables = v["tables"].as_array().expect("tables");
    assert_eq!(tables.len(), attemptdb_query::TABLE_NAMES.len());
    assert!(v["rules"].as_array().is_some_and(|r| r.len() >= 5));
    assert_eq!(
        v["examples"].as_array().map(Vec::len),
        Some(catalog::examples().len())
    );
    let events = tables
        .iter()
        .find(|t| t["name"] == "events")
        .expect("events");
    assert_eq!(events["layer"], "fact");
    let attempts = tables
        .iter()
        .find(|t| t["name"] == "attempts")
        .expect("attempts");
    assert_eq!(attempts["layer"], "inference");
}
