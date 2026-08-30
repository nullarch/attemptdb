# CLAUDE.md — AttemptDB

Guidance for Claude Code when working in this repository. Read `PROGRESS.md`
first in every session: it is the execution log and the "what is next" list.
`TODO.md` is the master plan (scope, architecture, launch); do not rewrite it,
tick items there only when they are truly done.

## What this is

AttemptDB — *the database for what agents tried*. A local-first temporal and
causal database of coding-agent work (Claude Code, Codex, Cursor, Gemini CLI),
written in Rust. One binary `attempt` acts as CLI, hook entrypoint, and
(planned) daemon/MCP/UI server. VibeMon is the optional hosted companion; it is
never required and never mentioned before the local value in docs.

## Workspace layout

```
crates/
  attemptdb-core      canonical model: ids (UUIDv7/v5), HLC, Event, paths, capture modes, schema field ids
  attemptdb-storage   engine: framed WAL/spool, memtable, Arrow IPC segments, manifest generations, .atdb snapshots
  attemptdb-adapters  provider payload → canonical Event (claude_code, codex, cursor, gemini_cli) + fixtures/golden tests
  attemptdb-project   Tier-1 deterministic projections: sessions, turns, tool calls, attempts, handoffs, causal edges, state_at/why_blocked
  attemptdb-query     DataFusion SQL over events + projection tables; AttemptQL parser/executor
  attemptdb-capture   hook entrypoint (spool append), DB locator, config/device identity, installer, doctor, git info
  attempt             the CLI binary
docs/rfcs/0001..0006  canonical model, storage engine, fact/inference bitemporal model, AttemptQL, cross-platform runtime, privacy & sync
docs/storage-format.md  byte-level on-disk contract (WAL/spool frames, segment schema, manifest, .atdb container)
fixtures/providers/   sanitised provider payloads + golden normalised envelopes (never real private content)
```

Dependency direction: core ← storage ← capture ← attempt; core ← adapters ← capture; core ← project ← query ← attempt.

## Commands

- `cargo test --workspace` — everything (DataFusion makes the first build slow; later builds are incremental).
- `cargo test -p <crate>` — one crate. `cargo clippy --workspace --all-targets` must stay clean.
- `cargo run -p attemptdb -- <cmd>` — run the CLI from source; `cargo install --path crates/attempt` installs `attempt` into `~/.cargo/bin`.
- `UPDATE_GOLDEN=1 cargo test -p attemptdb-adapters` regenerates golden envelopes after an intentional adapter change (review the diff).

## Invariants (do not break)

- **Facts vs inference.** `Event` is immutable observed fact; everything in `attemptdb-project` is inference with evidence ids, confidence, and `ALGORITHM_VERSION`. Never present inference as fact in UI or docs.
- **Metadata vs content.** `Event.attrs` is an allowlist of content-free metadata; anything content-bearing lives in `Event.content`/`raw`, which are stripped in `metadata_only` mode. Privacy canary tests in `attemptdb-adapters` enforce this — keep them green.
- **On-disk format = `docs/storage-format.md`.** Little-endian, UTF-8, CRC32C on every frame, stable numeric field ids (`attemptdb-core::schema::field_id`). Any layout change needs a format version bump and an RFC update.
- **Hook path must never hurt the agent.** `attempt hook <provider>` always exits 0, prints nothing (except Gemini's `{"decision":"allow"}`), never opens the database (spool only), and stays in the low milliseconds.
- **Installer safety.** Structural JSON edits with backup + lock + atomic rename; detect agents before creating directories; never write Codex's `[hooks.state]` trust table.
- **No SQLite in the core.** Storage is owned (WAL + Arrow segments); DataFusion is the query substrate. Say so plainly in docs.
- **Self-hosting.** This repo's own build history is the demo dataset. Keep hooks installed while working here; never commit `.attemptdb/` or `.atdb` files (gitignored).

## Conventions

- Rust 2024 edition, `rust-version` 1.94. All code, comments, docs in English. Commit messages in English.
- Prefer small pure functions with unit tests; integration tests under `crates/*/tests/`.
- Do not add dependencies casually — the binary must stay a single self-contained executable on macOS/Windows/Linux.
- Fixtures: sanitise paths to `/home/dev/...` and repos to `example/project`; mark authored fixtures with `_fixture_note`.
