//! The read-only gate in front of the query console.
//!
//! The engine cannot write to the database, but DataFusion would happily
//! `CREATE` an in-memory table or `COPY` rows to a file, so anything that is
//! not a read verb is refused up front, and only one statement per call is
//! accepted. Same rules as the MCP server.

const READ_VERBS: &[&str] = &[
    "SELECT", "WITH", "VALUES", "EXPLAIN", "DESCRIBE", "SHOW", "WHY", "TRACE", "STATE", "DIFF",
    "WHAT",
];

const WRITE_WORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "TRUNCATE", "COPY", "SET", "RESET",
    "GRANT", "REVOKE", "MERGE", "UNLOAD", "INSTALL", "LOAD", "ATTACH", "DETACH",
];

/// Words of a statement outside single-quoted strings, upper-cased.
fn bare_words(statement: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    for c in statement.chars() {
        if c == '\'' {
            in_string = !in_string;
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        if in_string {
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            current.push(c.to_ascii_uppercase());
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Accept only read statements.
pub fn check_read_only(statement: &str) -> Result<(), String> {
    let trimmed = statement.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err("empty statement".to_string());
    }
    if trimmed.contains(';') {
        return Err("one statement per call (found ';' inside the statement)".to_string());
    }
    let words = bare_words(trimmed);
    let Some(first) = words.first() else {
        return Err("statement has no keyword".to_string());
    };
    if !READ_VERBS.contains(&first.as_str()) {
        return Err(format!(
            "read-only: {first} statements are not accepted; use SELECT/WITH/EXPLAIN/DESCRIBE (SQL) or SHOW/WHY/TRACE/STATE/DIFF/WHAT IS (AttemptQL)"
        ));
    }
    if let Some(w) = words.iter().find(|w| WRITE_WORDS.contains(&w.as_str())) {
        return Err(format!(
            "read-only: {w} is not allowed inside a statement served by the UI"
        ));
    }
    Ok(())
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
    }
}
