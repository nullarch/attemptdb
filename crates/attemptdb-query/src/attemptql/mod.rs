//! AttemptQL: the statement language of RFC 0004.
//!
//! ```text
//! SHOW ATTEMPTS [FOR project = 'name' AND since '-7d'] [WHERE <sql>] [LIMIT n]
//! SHOW FAILED ATTEMPTS | SHOW SUPERSEDED ATTEMPTS | SHOW SESSIONS | SHOW TURNS
//! SHOW TOOL CALLS | SHOW HANDOFFS [BETWEEN agent = 'a' AND agent = 'b']
//! SHOW EVIDENCE FOR <att_ | trn_ | ses_ | spn_ | ev_ id> | SHOW EDGES | SHOW SIGNALS
//! WHY session '<ses_id>' STATUS BLOCKED | WHY project STATUS BLOCKED | WHY <att_id> FAILED
//! TRACE <id> CAUSES [DEPTH n] [DIRECTION UP|DOWN|BOTH]
//! STATE project AT '<ts>' | STATE session '<ses_id>' AT now
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
    let mut words = text
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

#[cfg(test)]
mod tests {
    use super::is_sql;

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
}
