//! One long statement must not take the process down.
//!
//! DataFusion plans a statement by recursing through its expression tree, so
//! a chain of a few hundred `OR`s overflowed the 2 MiB stack of the private
//! statement runtime and aborted the process: an MCP server, the local UI or
//! the daemon, on one call. These tests run the shapes that did it. Before
//! the fix the test binary itself aborts ("has overflowed its stack").

mod common;

use attemptdb_query::{
    MAX_CHAINED_OPERATORS, MAX_STATEMENT_BYTES, MAX_SUBSELECTS, QueryEngine, QueryLimits,
};
use common::spec_scenario;

async fn engine() -> QueryEngine {
    QueryEngine::from_events(spec_scenario().events)
        .await
        .unwrap()
}

/// `n` terms of every shape that overflowed, as `(name, statement)`.
fn shapes(n: usize) -> Vec<(&'static str, String)> {
    let mut cte = String::from("WITH c0 AS (SELECT 1 AS x)");
    for i in 1..n {
        cte.push_str(&format!(", c{i} AS (SELECT x FROM c{})", i - 1));
    }
    cte.push_str(&format!(" SELECT * FROM c{}", n - 1));
    vec![
        ("plus", format!("SELECT 1{} AS x", " + 1".repeat(n))),
        (
            "or",
            format!(
                "SELECT count(*) FROM events WHERE kind = 'x0'{}",
                (1..n)
                    .map(|i| format!(" OR kind = 'x{i}'"))
                    .collect::<String>()
            ),
        ),
        (
            "and-like",
            format!(
                "SELECT count(*) FROM events WHERE kind LIKE 'a%'{}",
                (1..n)
                    .map(|i| format!(" AND kind LIKE '%b{i}%'"))
                    .collect::<String>()
            ),
        ),
        ("concat", format!("SELECT 'a'{} AS x", " || 'b'".repeat(n))),
        (
            "union-all",
            format!("SELECT 1 AS x{}", " UNION ALL SELECT 1".repeat(n)),
        ),
        ("cte-chain", cte),
    ]
}

#[tokio::test]
async fn the_shapes_that_aborted_the_process_are_refused_with_a_reason() {
    let e = engine().await;
    let limits = QueryLimits::new(10, 1 << 20);
    for (name, sql) in shapes(1000) {
        for (via, r) in [
            ("sql_limited", e.sql_limited(&sql, &limits).await),
            ("sql", e.sql(&sql).await),
            ("query_limited", e.query_limited(&sql, &limits).await),
            ("query", e.query(&sql).await),
            ("explain", e.explain(&sql).await),
        ] {
            let err = r
                .err()
                .unwrap_or_else(|| panic!("{name} via {via} ran"))
                .to_string();
            assert!(err.contains("too complex"), "{name} via {via}: {err}");
            assert!(
                err.contains("Split it") && err.len() < 1500,
                "{name} via {via}: the message says what to do and does not echo the statement: {err}"
            );
        }
    }
}

#[tokio::test]
async fn statements_at_the_limits_plan_and_run() {
    let e = engine().await;
    let limits = QueryLimits::new(10, 1 << 20);
    // The deepest statements the guard admits: they must plan on the
    // statement runtime's stack, not fall over just under the limit.
    let deepest = MAX_CHAINED_OPERATORS - 1;
    for (name, sql) in shapes(deepest.min(MAX_SUBSELECTS - 1)) {
        e.sql_limited(&sql, &limits)
            .await
            .unwrap_or_else(|err| panic!("{name} (limited): {err}"));
        e.sql(&sql)
            .await
            .unwrap_or_else(|err| panic!("{name} (unbounded): {err}"));
    }
    for (name, sql) in shapes(deepest)
        .into_iter()
        .filter(|(n, _)| matches!(*n, "plus" | "or" | "and-like" | "concat"))
    {
        e.sql_limited(&sql, &limits)
            .await
            .unwrap_or_else(|err| panic!("{name} at {deepest}: {err}"));
    }
}

#[tokio::test]
async fn an_attemptql_where_is_guarded_before_it_is_parsed() {
    let e = engine().await;
    let ors = (0..1000)
        .map(|i| format!("outcome = 'x{i}'"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let limits = QueryLimits::new(10, 1 << 20);
    for statement in [
        format!("SHOW ATTEMPTS WHERE {ors}"),
        format!("SHOW ATTEMPTS WHERE {ors} LIMIT 3"),
    ] {
        let err = e.query_limited(&statement, &limits).await.unwrap_err();
        assert!(err.to_string().contains("too complex"), "{err}");
        let err = e.query(&statement).await.unwrap_err();
        assert!(err.to_string().contains("too complex"), "{err}");
    }
    // A reasonable one still works.
    e.query("SHOW ATTEMPTS WHERE outcome = 'failed' OR outcome = 'succeeded'")
        .await
        .unwrap();
}

#[test]
fn an_unbounded_statement_plans_off_the_callers_small_stack() {
    // The CLI and the daemon run `QueryEngine::sql` on whatever thread they
    // have. The planner recursion must not run there: a 512 KiB stack cannot
    // plan a 300-term chain in any build.
    let sql = shapes(300)[1].1.clone();
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let e = engine().await;
                e.sql(&sql).await.unwrap().row_count()
            })
        })
        .unwrap();
    assert_eq!(handle.join().expect("the small-stack thread survived"), 1);
}

#[tokio::test]
async fn a_long_statement_is_refused_by_size() {
    let e = engine().await;
    let sql = format!("SELECT '{}' AS big", "x".repeat(MAX_STATEMENT_BYTES));
    let err = e.sql(&sql).await.unwrap_err().to_string();
    assert!(err.contains("KiB long"), "{err}");
}
