//! `attempt schema` end to end.
//!
//! The claim being tested is the one that makes the command worth having:
//! it answers with no database, no data directory and no capture, so an
//! agent that has just cloned the repository can learn how to query it
//! before there is anything to query.

use std::process::Command;

fn schema(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_attempt"))
        // A directory that does not exist: opening a database here would
        // fail, so a passing test proves nothing was opened.
        .arg("--data-dir")
        .arg("/nonexistent/attemptdb-schema-test")
        .arg("schema")
        .args(args)
        .env("ATTEMPTDB_KEYRING", "off")
        .env_remove("ATTEMPTDB_KEY_FILE")
        .output()
        .expect("attempt runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

#[test]
fn the_catalog_answers_with_no_database() {
    let (ok, text) = schema(&[]);
    assert!(ok, "attempt schema failed:\n{text}");
    for name in attemptdb_query::TABLE_NAMES {
        assert!(text.contains(name), "{name} missing:\n{text}");
    }
    assert!(text.contains("fact"), "{text}");
    assert!(text.contains("inference"), "{text}");
}

#[test]
fn one_table_lists_its_columns_and_their_allowed_values() {
    let (ok, text) = schema(&["signals"]);
    assert!(ok, "{text}");
    assert!(text.contains("pending"), "{text}");
    assert!(text.contains("permission_requested"), "{text}");
    let (ok, text) = schema(&["nope"]);
    assert!(!ok, "an unknown table must fail:\n{text}");
}

#[test]
fn the_markdown_form_is_the_checked_in_document() {
    let (ok, text) = schema(&["--format", "markdown"]);
    assert!(ok, "{text}");
    let checked_in = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/query-context.md"),
    )
    .expect("docs/query-context.md exists");
    assert_eq!(
        text, checked_in,
        "`attempt schema --format markdown` and docs/query-context.md have diverged"
    );
}

#[test]
fn the_json_form_is_machine_readable() {
    let (ok, text) = schema(&["--format", "json"]);
    assert!(ok, "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    assert_eq!(
        v["tables"].as_array().map(Vec::len),
        Some(attemptdb_query::TABLE_NAMES.len())
    );
    assert!(v["examples"].as_array().is_some_and(|e| !e.is_empty()));
}

#[test]
fn the_examples_are_listed_on_their_own() {
    let (ok, text) = schema(&["--examples"]);
    assert!(ok, "{text}");
    assert!(text.contains("SHOW FAILED ATTEMPTS"), "{text}");
}

#[test]
fn the_examples_do_not_print_text_that_fails_when_pasted() {
    let (ok, text) = schema(&["--examples"]);
    assert!(ok, "{text}");
    // Placeholders are angle-bracketed hints with a sentence about them, never
    // a `{session}` that a reader would paste into a statement.
    assert!(
        !text.contains("{session}") && !text.contains("{attempt}"),
        "{text}"
    );
    assert!(
        text.contains("<ses_id>") || text.contains("<att_id>"),
        "{text}"
    );
    assert!(text.contains("need a real id"), "{text}");
    // The JSON form keeps the raw placeholders for a program, and lists them.
    let (ok, json) = schema(&["--format", "json"]);
    assert!(ok, "{json}");
    let doc: serde_json::Value = serde_json::from_str(&json).expect("one JSON document");
    assert!(
        doc["placeholders"]
            .as_array()
            .is_some_and(|p| !p.is_empty())
    );
    assert!(doc["tables"].as_array().is_some_and(|t| t.len() == 15));
}

#[test]
fn the_json_help_says_it_is_one_document() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_attempt"))
        .args(["schema", "--help"])
        .env("ATTEMPTDB_KEYRING", "off")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("CURSOR_CONFIG_DIR")
        .env_remove("GEMINI_CONFIG_DIR")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(!help.contains("One object per table"), "{help}");
    assert!(help.contains("One JSON document"), "{help}");
}
