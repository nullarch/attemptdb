//! AttemptQL: the statement language of RFC 0004.
//!
//! ```text
//! SHOW ATTEMPTS [FOR project = 'name' AND since '-7d'] [WHERE <sql>] [LIMIT n] [INCLUDING RETRACTED]
//! SHOW FAILED ATTEMPTS | SHOW SUPERSEDED ATTEMPTS | SHOW SESSIONS | SHOW TURNS
//! SHOW TOOL CALLS | SHOW HANDOFFS [BETWEEN agent = 'a' AND agent = 'b']
//! SHOW WORK UNITS [FOR phase = 'blocked' AND status = 'open'] | SHOW DECISIONS
//! SHOW EVIDENCE FOR <att_ | trn_ | ses_ | spn_ | wu_ | ev_ id> | SHOW EDGES | SHOW SIGNALS
//! SHOW CORRECTIONS | SHOW RETRACTIONS
//! WHY session '<ses_id>' STATUS BLOCKED | WHY project STATUS BLOCKED | WHY <att_id> FAILED
//! WHY work_unit '<wu_id>' STATUS BLOCKED
//! TRACE <id> CAUSES [DEPTH n] [DIRECTION UP|DOWN|BOTH]
//! STATE project AT '<ts>' | STATE session '<ses_id>' AT now | STATE work_unit '<wu_id>' AT now
//! DIFF STATE '<ts-a>' '<ts-b>'
//! WHAT IS project DOING NOW
//! EXPLAIN <statement>
//! ```

pub mod ast;
mod lexer;
mod parser;

pub use ast::*;
pub use lexer::{TokKind, Token, lex};
pub use parser::parse;

/// Whether `text` should be handed to the SQL engine rather than the
/// AttemptQL parser: `SELECT`, `WITH`, `VALUES`, `DESCRIBE`, `EXPLAIN <sql>`
/// and DataFusion's `SHOW TABLES` / `SHOW COLUMNS`.
pub fn is_sql(text: &str) -> bool {
    let mut words = skip_leading_comments(text)
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_uppercase);
    let Some(first) = words.next() else {
        return false;
    };
    let second = words.next().unwrap_or_default();
    match first.as_str() {
        "SELECT" | "WITH" | "VALUES" | "DESCRIBE" | "CREATE" | "INSERT" | "DROP" | "SET" => true,
        "EXPLAIN" => matches!(
            second.as_str(),
            "SELECT" | "WITH" | "VALUES" | "ANALYZE" | "VERBOSE"
        ),
        "SHOW" => matches!(second.as_str(), "TABLES" | "COLUMNS" | "FUNCTIONS" | "ALL"),
        _ => false,
    }
}

/// The statement keyword a mistyped first word most likely meant: within two
/// edits of one of them (`SELEC` is `SELECT`, `SHOWW` is `SHOW`), and not one
/// of them already. Both AttemptQL verbs and the SQL ones count, since the
/// text reaches this parser because it did not start with a SQL keyword.
pub(crate) fn closest_keyword(word: &str) -> Option<&'static str> {
    const KEYWORDS: &[&str] = &[
        "SELECT", "WITH", "EXPLAIN", "DESCRIBE", "VALUES", "SHOW", "WHY", "TRACE", "STATE", "DIFF",
        "WHAT",
    ];
    let w = word.to_ascii_uppercase();
    KEYWORDS
        .iter()
        .map(|k| (edit_distance(&w, k), *k))
        .filter(|(d, _)| (1..=2).contains(d))
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
}

/// Levenshtein distance between two short ASCII words.
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != cb))
                .min(row[j] + 1)
                .min(row[j + 1] + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// `text` without the whitespace and comments (`-- …` to the end of the
/// line, `/* … */`) that precede the first word.
fn skip_leading_comments(text: &str) -> &str {
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r);
        } else {
            return rest;
        }
    }
}

/// The text of an AttemptQL `WHERE` clause as exactly one SQL expression,
/// printed back from its parse tree.
///
/// The clause is spliced into the statement the executor compiles, next to
/// the filter that hides retracted rows. Splicing the user's text would let
/// `true) OR (retracted` rewrite that filter, so the text must parse as a
/// single expression with nothing left over, and what is spliced is the
/// printed tree, which cannot carry a comment, a stray parenthesis or a
/// second clause with it. Subqueries are allowed (the SQL surface allows
/// them); the retraction filter is applied to the statement's own table
/// outside the expression either way.
pub fn normalise_predicate(text: &str) -> std::result::Result<String, String> {
    use datafusion::sql::sqlparser::dialect::GenericDialect;
    use datafusion::sql::sqlparser::parser::Parser;
    use datafusion::sql::sqlparser::tokenizer::Token;
    // Parsing, printing and dropping the tree recurse on this thread's stack.
    crate::guard::check_statement(text).map_err(|e| e.to_string())?;
    let dialect = GenericDialect {};
    let mut parser = Parser::new(&dialect)
        .try_with_sql(text)
        .map_err(|e| format!("WHERE needs one SQL expression: {e}"))?;
    let expr = parser
        .parse_expr()
        .map_err(|e| format!("WHERE needs one SQL expression: {e}"))?;
    let next = parser.peek_token();
    if next.token != Token::EOF {
        return Err(format!(
            "WHERE needs one SQL expression, but found {} after it (unbalanced parenthesis or a second clause?)",
            next.token
        ));
    }
    Ok(expr.to_string())
}

#[cfg(test)]
mod tests {
    use super::{is_sql, normalise_predicate};

    #[test]
    fn detects_sql() {
        assert!(is_sql("SELECT count(*) FROM events"));
        assert!(is_sql("  with x as (select 1) select * from x"));
        assert!(is_sql("EXPLAIN SELECT 1"));
        assert!(is_sql("show tables"));
        assert!(!is_sql("SHOW ATTEMPTS"));
        assert!(!is_sql("EXPLAIN SHOW ATTEMPTS"));
        assert!(!is_sql("WHY project STATUS BLOCKED"));
        assert!(!is_sql(""));
    }

    #[test]
    fn leading_comments_do_not_hide_sql() {
        assert!(is_sql("-- recent failures\nSELECT 1"));
        assert!(is_sql("/* why */ SELECT 1"));
        assert!(is_sql(
            "  -- a\n  -- b\n\n/* c */ -- d\nwith x as (select 1) select * from x"
        ));
        assert!(!is_sql("-- note\nSHOW SESSIONS"));
        assert!(!is_sql("-- only a comment"));
        assert!(!is_sql("/* unterminated SELECT"));
    }

    #[test]
    fn a_predicate_is_exactly_one_expression() {
        assert_eq!(
            normalise_predicate("outcome = 'failed'").unwrap(),
            "outcome = 'failed'"
        );
        assert_eq!(
            normalise_predicate("a = 1 -- trailing\n AND /* c */ b > 2").unwrap(),
            "a = 1 AND b > 2"
        );
        assert_eq!(
            normalise_predicate("x IN (SELECT 1) OR (y = 'a;b')").unwrap(),
            "x IN (SELECT 1) OR (y = 'a;b')"
        );
        for bad in [
            "true) OR (retracted",
            "true) OR retracted OR (false",
            "a = 1; DROP TABLE events",
            "a = 1, b = 2",
            "a = 1 b = 2",
            ") OR (1=1",
            "",
            "a = (1",
        ] {
            assert!(normalise_predicate(bad).is_err(), "{bad:?}");
        }
    }
}
