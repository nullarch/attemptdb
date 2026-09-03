# AGENTS.md — AttemptDB

Instructions for coding agents working in this repository. This is the
canonical file; `CLAUDE.md` and any other vendor file point here rather than
repeating it.

Read `PROGRESS.md` first in every session: it is the execution log and the
"what is next" list. `TODO.md` is the master plan (scope, architecture,
launch); do not rewrite it, tick items there only when they are truly done.

## What this is

AttemptDB — *the database for what agents tried*. A local-first temporal and
causal database of coding-agent work (Claude Code, Codex, Cursor, Gemini CLI),
written in Rust. One binary `attempt` acts as CLI, hook entrypoint, daemon,
MCP server and UI server. VibeMon is the optional hosted companion; it is
never required and never mentioned before the local value in docs.

## Commands

```sh
cargo test --workspace              # everything (first build is slow: DataFusion)
cargo test -p <crate>               # one crate
cargo clippy --workspace --all-targets   # must stay clean
cargo fmt --all                     # before every commit
cargo run -p attemptdb -- <cmd>     # run the CLI from source
cargo install --path crates/attempt # install `attempt` into ~/.cargo/bin
```

Regenerating checked-in artefacts after an intentional change (review the diff):

```sh
UPDATE_GOLDEN=1 cargo test -p attemptdb-adapters   # golden normalised envelopes
UPDATE_GOLDEN=1 cargo test -p attemptdb-query --test catalog   # docs/query-context.md
```

## Querying this database

Do not guess at table or column names, and do not read `tables.rs` to find
them. The catalog is a command:

```sh
attempt schema                 # the rules, and every table with its grain
attempt schema attempts        # one table: every column, its meaning, its allowed values
attempt schema --examples      # questions people ask, and the statement that answers each
attempt schema --format json   # the same, for a program
```

Over MCP the same catalog is the `attempt_schema` tool, and `attempt_query`
runs one statement. `docs/query-context.md` is the identical document,
generated from `crates/attemptdb-query/src/catalog.rs`; edit the catalog
module, never the markdown.

Four rules decide whether a statement is *right* rather than merely valid.
`attempt schema` prints all nine; these are the ones most often got wrong:

- **`events` is fact; every other table is inference.** Inferred rows carry
  `evidence` (event ids), `confidence` and `algorithm_version`. Never report
  one as something the agent did — say what it was inferred from.
- **Retracted rows are hidden by AttemptQL and visible to SQL.** A bare
  `SELECT` must filter `retracted = false` itself.
- **Content may be absent by design.** Under `capture_mode = 'metadata_only'`
  every text column is null. That is a privacy setting, not missing data.
- **Do not re-derive the projection's counts** with `COUNT(*)`: the stored
  counts apply the retraction rules and yours will not.

## Workspace layout

```
crates/
  attemptdb-core      canonical model: ids (UUIDv7/v5), HLC, Event, paths, capture modes, schema field ids
  attemptdb-storage   engine: framed WAL/spool, memtable, Arrow IPC segments, manifest generations, .atdb snapshots, encrypted blobs
  attemptdb-adapters  provider payload → canonical Event (claude_code, codex, cursor, gemini_cli) + fixtures/golden tests
  attemptdb-project   Tier-1 deterministic projections: sessions, turns, tool calls, attempts, work units, handoffs, causal edges, attention, state_at/why_blocked
  attemptdb-query     DataFusion SQL over events + projection tables; AttemptQL parser/executor; the query catalog
  attemptdb-capture   hook entrypoint (spool append), DB locator, config/device identity, key store, daemon, installer, doctor, git info
  attemptdb-mcp       MCP server over stdio: the tools a coding agent calls
  attemptdb-ui        local AgentTimeline web UI (axum, server-rendered), static HTML export, summary card
  attemptdb-server    sync server: authenticated per-tenant ingest and read over HTTP (RFC 0006 §10)
  attemptdb-bench     public synthetic workload and the benchmark runner
  attempt-hook        the smallest executable on the hook path
  attempt             the CLI binary
docs/rfcs/0001..0006  canonical model, storage engine, fact/inference bitemporal model, AttemptQL, cross-platform runtime, privacy & sync
docs/adr/             decisions with consequences: no SQLite core, Arrow/DataFusion, OTel intake
docs/storage-format.md   byte-level on-disk contract (WAL/spool frames, segment schema, manifest, .atdb container)
docs/query-context.md    generated table/column catalog — regenerate, never hand-edit
docs/server-api.md       the sync server's HTTP contract
fixtures/providers/      sanitised provider payloads + golden normalised envelopes (never real private content)
spec/                    Event v1 and Inference v1 JSON Schemas
```

Dependency direction: core ← storage ← capture ← attempt; core ← adapters ←
capture; core ← project ← query ← {mcp, ui, server, attempt}.

## Invariants (do not break)

- **Facts vs inference.** `Event` is immutable observed fact; everything in
  `attemptdb-project` is inference with evidence ids, confidence, and
  `ALGORITHM_VERSION`. Never present inference as fact in UI or docs.
- **Metadata vs content.** `Event.attrs` is an allowlist of content-free
  metadata; anything content-bearing lives in `Event.content`/`raw`, which are
  stripped in `metadata_only` mode. Privacy canary tests in
  `attemptdb-adapters` enforce this — keep them green.
- **On-disk format = `docs/storage-format.md`.** Little-endian, UTF-8, CRC32C
  on every frame, stable numeric field ids
  (`attemptdb-core::schema::field_id`). Any layout change needs a format
  version bump and an RFC update.
- **Hook path must never hurt the agent.** `attempt hook <provider>` always
  exits 0, prints nothing (except Gemini's `{"decision":"allow"}`), never
  opens the database (spool only), and stays in the low milliseconds.
- **Installer safety.** Structural JSON edits with backup + lock + atomic
  rename; detect agents before creating directories; never write Codex's
  `[hooks.state]` trust table.
- **No SQLite in the core.** Storage is owned (WAL + Arrow segments);
  DataFusion is the query substrate. Say so plainly in docs.
- **The query surface is read-only.** Only `SELECT`/`WITH`/`EXPLAIN`/
  `DESCRIBE`/`SHOW` and the AttemptQL verbs, one statement per call. History
  is appended by capture, never by a query.
- **Anything published uses demo data.** Screenshots, recordings, the summary
  card and the artifact all run against `attempt ui --demo`; the live database
  holds the owner's real prompts and paths.
- **Self-hosting.** This repo's own build history is the demo dataset. Keep
  hooks installed while working here; never commit `.attemptdb/` or `.atdb`
  files (gitignored).

## Conventions

- Rust 2024 edition, `rust-version` 1.94. All code, comments, docs and commit
  messages in English.
- Prefer small pure functions with unit tests; integration tests under
  `crates/*/tests/`.
- Do not add dependencies casually — the binary must stay a single
  self-contained executable on macOS/Windows/Linux.
- Fixtures: sanitise paths to `/home/dev/...` and repos to `example/project`;
  mark authored fixtures with `_fixture_note`.
- A generated document says so in its first lines and names the command that
  regenerates it. If a document and the code disagree, a test should be the
  thing that notices.
