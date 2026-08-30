# attemptdb

**The database for what AI coding agents tried.**

Git records what changed. AttemptDB records what your agents **attempted** —
every prompt, tool call, failed approach, and handoff — in a local,
queryable, temporal and causal database. One binary. No account. Nothing
leaves your machine.

This crate installs the `attempt` command-line interface. The package is
named `attemptdb` because the crates.io name `attempt` belongs to an
unrelated crate; the binary is still `attempt`.

```sh
cargo install attemptdb
attempt init            # create the local database
attempt hook install    # wire up Claude Code, Codex, Cursor, Gemini CLI
```

Then work normally with your coding agent and ask questions about it:

```sh
attempt timeline                       # sessions, turns, attempts
attempt why att_a9c319da               # evidence-backed answer, with its uncertainty
attempt trace att_a9c319da             # walk causal edges backwards
attempt query "SELECT failure_class, count(*) FROM attempts
               WHERE outcome = 'failed' GROUP BY 1"
attempt ui                             # local AgentTimeline in your browser
attempt mcp                            # serve the database to your agent over MCP
```

## What it is

- **Facts and inference are separate.** Captured events are immutable
  observations; sessions, attempts, work units and blockers are projections
  that carry evidence ids, a confidence, and an algorithm version. Nothing
  is presented as fact that was inferred.
- **Owned storage.** A framed write-ahead log, Arrow IPC segments, and
  atomic manifest generations — not SQLite. DataFusion is the query
  substrate; `AttemptQL` sits above it for the questions SQL is clumsy at.
- **Local-first and content-free by default.** Metadata is an allowlist;
  prompts and tool output stay on your machine (encrypted), and sync is an
  explicit opt-in with per-repository policy and secret redaction.
- **The hook never blocks your agent.** It appends to a spool and exits 0,
  in about four milliseconds.

## Crates

| Crate | What it is |
|---|---|
| [`attemptdb`](https://crates.io/crates/attemptdb) | the `attempt` CLI (this crate) |
| [`attempt-hook`](https://crates.io/crates/attempt-hook) | the small hook entrypoint agents call |
| [`attemptdb-core`](https://crates.io/crates/attemptdb-core) | canonical event model, ids, clocks |
| [`attemptdb-storage`](https://crates.io/crates/attemptdb-storage) | WAL, memtable, segments, snapshots |
| [`attemptdb-adapters`](https://crates.io/crates/attemptdb-adapters) | provider payload → canonical event |
| [`attemptdb-project`](https://crates.io/crates/attemptdb-project) | deterministic Tier-1 projections |
| [`attemptdb-query`](https://crates.io/crates/attemptdb-query) | SQL and AttemptQL |

Full documentation, the on-disk format, the RFCs, and the published event
schema are in the repository: <https://github.com/nullarch/attemptdb>

## License

Apache-2.0
