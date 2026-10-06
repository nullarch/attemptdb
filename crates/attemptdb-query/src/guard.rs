//! A statement that is too big to plan is refused before it is planned.
//!
//! DataFusion parses and plans recursively: a chain of 900 `OR`s is a tree
//! 900 levels deep, and planning, optimising and even dropping it descend that
//! tree on the stack of whichever thread runs the statement. A stack overflow
//! is not an error a caller can catch, it aborts the process, and the process
//! may be an MCP server an agent depends on, the local UI, or the daemon.
//!
//! Two defences, neither of which is enough alone:
//!
//! - every statement runs on a thread with a large stack (see
//!   [`crate::limits`]), so a statement near the limit still plans;
//! - this module counts what makes the tree deep, from the token stream (no
//!   recursion, microseconds) and refuses a statement beyond the limits with a
//!   message that says what to change.
//!
//! The limits are far above anything a person or an agent writes by hand
//! (`kind IN (…)` is one token run, not one operator per value) and far below
//! what the stack takes: the deepest statement allowed here plans on a
//! fraction of the stack even in an unoptimised build.

use crate::error::{QueryError, Result};
use datafusion::sql::sqlparser::dialect::GenericDialect;
use datafusion::sql::sqlparser::keywords::Keyword;
use datafusion::sql::sqlparser::tokenizer::{Token, Tokenizer};

/// Longest statement, in bytes.
pub const MAX_STATEMENT_BYTES: usize = 512 * 1024;
/// Most tokens in one statement (flat lists such as `IN (…)` are the reason
/// this is generous).
pub const MAX_STATEMENT_TOKENS: usize = 100_000;
/// Most binary operators (`+ - * / % ||`, comparisons, `AND`, `OR`) plus set
/// operators (`UNION`, `INTERSECT`, `EXCEPT`) and `JOIN`s in one statement:
/// each can add a level to the expression or plan tree.
pub const MAX_CHAINED_OPERATORS: usize = 400;
/// Most `SELECT`/`VALUES` blocks in one statement: subqueries, CTEs and the
/// arms of a `UNION` each add a level to the plan.
pub const MAX_SUBSELECTS: usize = 100;

/// Refuse `sql` when it is too large to plan safely. `Ok` for anything the
/// tokenizer cannot read: the parser reports that with a better message.
pub(crate) fn check_statement(sql: &str) -> Result<()> {
    if sql.len() > MAX_STATEMENT_BYTES {
        return Err(too_complex(format!(
            "it is {} KiB long and the limit is {} KiB",
            sql.len().div_ceil(1024),
            MAX_STATEMENT_BYTES / 1024
        )));
    }
    let dialect = GenericDialect {};
    let Ok(tokens) = Tokenizer::new(&dialect, sql).tokenize() else {
        return Ok(());
    };
    if tokens.len() > MAX_STATEMENT_TOKENS {
        return Err(too_complex(format!(
            "it has {} tokens and the limit is {MAX_STATEMENT_TOKENS}",
            tokens.len()
        )));
    }
    let mut operators = 0usize;
    let mut selects = 0usize;
    for t in &tokens {
        match t {
            Token::Plus
            | Token::Minus
            | Token::Mul
            | Token::Div
            | Token::Mod
            | Token::StringConcat
            | Token::Ampersand
            | Token::Pipe
            | Token::Caret => operators += 1,
            Token::Word(w) => match w.keyword {
                Keyword::AND
                | Keyword::OR
                | Keyword::UNION
                | Keyword::INTERSECT
                | Keyword::EXCEPT
                | Keyword::JOIN => operators += 1,
                Keyword::SELECT | Keyword::VALUES => selects += 1,
                _ => {}
            },
            _ => {}
        }
    }
    if operators > MAX_CHAINED_OPERATORS {
        return Err(too_complex(format!(
            "it chains {operators} operators (AND, OR, +, ||, UNION, JOIN, comparisons …) and the limit is {MAX_CHAINED_OPERATORS}; \
             use `col IN ('a', 'b', …)` instead of a chain of ORs, one regular expression (`col ~ 'a|b|c'`) instead of many LIKEs, \
             and GROUP BY instead of a UNION of many arms"
        )));
    }
    if selects > MAX_SUBSELECTS {
        return Err(too_complex(format!(
            "it has {selects} SELECT blocks (subqueries, CTEs, UNION arms) and the limit is {MAX_SUBSELECTS}; \
             fold the repeated blocks into one query with GROUP BY or a join"
        )));
    }
    Ok(())
}

fn too_complex(detail: String) -> QueryError {
    QueryError::Plan(format!(
        "the statement is too complex to run safely: {detail}. Split it into several statements, or narrow what it asks for"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(sql: &str) -> String {
        check_statement(sql).unwrap_err().to_string()
    }

    #[test]
    fn ordinary_statements_pass() {
        for sql in [
            "SELECT 1",
            "SELECT kind, count(*) FROM events WHERE kind IN ('a', 'b') GROUP BY 1 ORDER BY 2 DESC LIMIT 10",
            "WITH a AS (SELECT 1 AS x), b AS (SELECT x FROM a) SELECT * FROM b",
            "SELECT 'it''s a (SELECT trap' AS s",
        ] {
            check_statement(sql).unwrap();
        }
        // Text inside a string literal is not an operator.
        let literal = format!("SELECT '{}'", " OR ".repeat(2000));
        check_statement(&literal).unwrap();
        // Garbage the tokenizer rejects is the parser's to report.
        check_statement("SELECT 'unterminated").unwrap();
    }

    #[test]
    fn the_limits_are_inclusive() {
        let ok = format!("SELECT 1{}", " + 1".repeat(MAX_CHAINED_OPERATORS));
        check_statement(&ok).unwrap();
        let too_many = format!("SELECT 1{}", " + 1".repeat(MAX_CHAINED_OPERATORS + 1));
        assert!(
            refused(&too_many).contains(&format!("chains {} operators", MAX_CHAINED_OPERATORS + 1))
        );
        let unions = format!("SELECT 1{}", " UNION ALL SELECT 1".repeat(MAX_SUBSELECTS));
        assert!(refused(&unions).contains("SELECT blocks"));
    }

    #[test]
    fn the_shapes_that_overflowed_the_stack_are_refused() {
        let n = 1000;
        let or = format!(
            "SELECT count(*) FROM events WHERE kind = 'x0'{}",
            (1..n)
                .map(|i| format!(" OR kind = 'x{i}'"))
                .collect::<String>()
        );
        let and_like = format!(
            "SELECT count(*) FROM events WHERE kind LIKE 'a%'{}",
            (1..n)
                .map(|i| format!(" AND kind LIKE '%b{i}%'"))
                .collect::<String>()
        );
        let mut cte = String::from("WITH c0 AS (SELECT 1 AS x)");
        for i in 1..400 {
            cte.push_str(&format!(", c{i} AS (SELECT x FROM c{})", i - 1));
        }
        cte.push_str(" SELECT * FROM c399");
        for sql in [
            format!("SELECT 1{}", " + 1".repeat(n)),
            or,
            and_like,
            format!("SELECT 'a'{}", " || 'b'".repeat(n)),
            format!("SELECT 1{}", " UNION ALL SELECT 1".repeat(n)),
            cte,
        ] {
            let m = refused(&sql);
            assert!(m.contains("too complex"), "{m}");
            assert!(m.len() < 1200, "the message must not echo the statement");
        }
    }

    #[test]
    fn size_and_token_limits_apply() {
        let long = format!("SELECT '{}'", "x".repeat(MAX_STATEMENT_BYTES));
        assert!(refused(&long).contains("KiB long"));
        let many = format!("SELECT {}", "1,".repeat(MAX_STATEMENT_TOKENS));
        assert!(refused(&many).contains("tokens"));
    }
}
