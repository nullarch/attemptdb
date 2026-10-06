//! The read-only pre-check in front of MCP and the web UI's query console.
//!
//! This is a courtesy, not the gate: the engine runs every statement with
//! `SQLOptions` that refuse DDL, DML and statements, and that plan
//! inspection is what keeps the query surface read-only. The pre-check
//! exists to say *why* in plain words before a statement reaches the planner,
//! so it is deliberately thin and lexes the statement properly instead of
//! searching it for substrings: comments, string literals and quoted
//! identifiers are what they are, not keywords.
//!
//! - the statement is one statement (a `;` token ends it; a `;` inside a
//!   string literal is not one);
//! - its first keyword is a read verb;
//! - no *unquoted* word in it is a write verb (`"update"` as a quoted
//!   identifier and `'insert into'` as a string are fine).

use datafusion::sql::sqlparser::dialect::GenericDialect;
use datafusion::sql::sqlparser::tokenizer::{Token, Tokenizer, Whitespace};

const READ_VERBS: &[&str] = &[
    "SELECT", "WITH", "VALUES", "DESCRIBE", "EXPLAIN", "SHOW", "WHY", "TRACE", "STATE", "DIFF",
    "WHAT",
];
const WRITE_WORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "TRUNCATE", "COPY", "SET", "RESET",
    "GRANT", "REVOKE", "MERGE", "UNLOAD", "INSTALL", "LOAD", "ATTACH", "DETACH",
];

/// Accept only read statements. `surface` names the caller in the message
/// ("MCP", "the UI").
pub fn check_read_only(statement: &str, surface: &str) -> Result<(), String> {
    let dialect = GenericDialect {};
    let tokens = Tokenizer::new(&dialect, statement)
        .tokenize()
        .map_err(|e| format!("cannot read the statement: {e}"))?;
    // Comments and whitespace carry no meaning.
    let mut tokens: Vec<Token> = tokens
        .into_iter()
        .filter(|t| !matches!(t, Token::Whitespace(w) if is_trivia(w)))
        .collect();
    while matches!(tokens.last(), Some(Token::SemiColon)) {
        tokens.pop();
    }
    if tokens.is_empty() {
        return Err("empty statement".to_string());
    }
    if tokens.iter().any(|t| matches!(t, Token::SemiColon)) {
        return Err("one statement per call (found ';' inside the statement)".to_string());
    }
    let first = tokens.iter().find_map(|t| match t {
        Token::Word(w) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
        _ => None,
    });
    let Some(first) = first else {
        return Err("statement has no keyword".to_string());
    };
    if !READ_VERBS.contains(&first.as_str()) {
        return Err(format!(
            "read-only: {first} statements are not accepted; use SELECT/WITH/EXPLAIN/DESCRIBE (SQL) or SHOW/WHY/TRACE/STATE/DIFF/WHAT IS (AttemptQL)"
        ));
    }
    let write = tokens.iter().find_map(|t| match t {
        Token::Word(w) if w.quote_style.is_none() => {
            let up = w.value.to_ascii_uppercase();
            WRITE_WORDS.contains(&up.as_str()).then_some(up)
        }
        _ => None,
    });
    if let Some(w) = write {
        return Err(format!(
            "read-only: {w} is not allowed inside a statement served by {surface}"
        ));
    }
    Ok(())
}

fn is_trivia(w: &Whitespace) -> bool {
    matches!(
        w,
        Whitespace::Space
            | Whitespace::Newline
            | Whitespace::Tab
            | Whitespace::SingleLineComment { .. }
            | Whitespace::MultiLineComment(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) {
        assert_eq!(check_read_only(s, "MCP"), Ok(()), "{s}");
    }

    fn err(s: &str) -> String {
        check_read_only(s, "MCP").expect_err(s)
    }

    #[test]
    fn plain_reads_pass() {
        ok("SELECT count(*) FROM events");
        ok("  with x as (select 1) select * from x;");
        ok("SHOW FAILED ATTEMPTS FOR path = 'drop table'");
        ok("WHY project STATUS BLOCKED");
        ok("EXPLAIN SELECT 1");
        ok("DESCRIBE events");
        ok("(SELECT 1) UNION ALL (SELECT 2)");
        ok("SELECT 1;;");
    }

    #[test]
    fn comments_strings_and_quoted_identifiers_are_not_keywords() {
        ok("-- why this?\nSELECT 1");
        ok("/* a note; with a semicolon */ SELECT 1");
        ok("SELECT 'a;b' AS x");
        ok("SELECT 'insert into events' FROM events");
        ok("SELECT 1 AS \"update\"");
        ok("SELECT \"delete\" FROM (SELECT 1 AS \"delete\") t");
        // A comment cannot hide a verb: the first keyword is the first
        // word after the comments.
        assert!(err("-- harmless\nDROP TABLE events").contains("DROP"));
        assert!(err("/* SELECT */ INSERT INTO events VALUES (1)").contains("INSERT"));
    }

    #[test]
    fn writes_and_extra_statements_are_refused() {
        assert!(err("INSERT INTO events VALUES (1)").contains("read-only"));
        assert!(err("CREATE TABLE t AS SELECT 1").contains("CREATE"));
        assert!(err("SELECT 1; DROP TABLE events").contains("one statement"));
        assert!(err("COPY (SELECT 1) TO '/tmp/x.csv'").contains("COPY"));
        assert!(err("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x").contains("INSERT"));
        assert!(err("SET datafusion.execution.batch_size = 1").contains("SET"));
        assert!(err("").contains("empty"));
        assert!(err("   ;  ").contains("empty"));
        assert!(err("-- only a comment").contains("empty"));
        assert!(
            check_read_only("DROP TABLE x", "the UI")
                .unwrap_err()
                .contains("DROP")
        );
        assert!(
            check_read_only("SELECT 1 /* x */ UPDATE", "the UI")
                .unwrap_err()
                .contains("served by the UI")
        );
    }

    #[test]
    fn an_unterminated_literal_is_a_readable_error() {
        assert!(err("SELECT 'oops").contains("cannot read the statement"));
    }
}
