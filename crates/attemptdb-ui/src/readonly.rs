//! The read-only gate in front of the query console.
//!
//! The engine cannot write to the database: it runs every statement with
//! options that refuse DDL, DML and statements. This pre-check says why in
//! plain words before a statement reaches the planner, and accepts one
//! statement per call. It lexes the statement (comments, string literals and
//! quoted identifiers are not keywords) and is the same check MCP uses; see
//! [`attemptdb_query::readonly`].

/// Accept only read statements.
pub fn check_read_only(statement: &str) -> Result<(), String> {
    attemptdb_query::check_read_only(statement, "the UI")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate() {
        assert!(check_read_only("SELECT 1").is_ok());
        assert!(check_read_only("  show failed attempts ;").is_ok());
        assert!(check_read_only("WHY project STATUS BLOCKED").is_ok());
        assert!(check_read_only("SELECT 'insert into' FROM events").is_ok());
        assert!(check_read_only("INSERT INTO events VALUES (1)").is_err());
        assert!(check_read_only("SELECT 1; DROP TABLE events").is_err());
        assert!(check_read_only("WITH x AS (SELECT 1) CREATE TABLE y AS SELECT * FROM x").is_err());
        assert!(check_read_only("").is_err());
        // Valid SQL the substring check used to refuse.
        assert!(check_read_only("-- recent\nSELECT 1").is_ok());
        assert!(check_read_only("SELECT 'a;b'").is_ok());
        assert!(check_read_only("SELECT 1 AS \"update\"").is_ok());
        assert!(
            check_read_only("DROP TABLE x")
                .unwrap_err()
                .contains("DROP")
        );
        assert!(
            check_read_only("SELECT 1 UPDATE")
                .unwrap_err()
                .contains("served by the UI")
        );
    }
}
