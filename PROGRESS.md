# AttemptDB — Progress Log

Execution log for `TODO.md`. Newest session first. Read this before working.

## Current state (2026-08-28)

**What exists and runs**

| Area | Status | Where |
|---|---|---|
| Canonical model (ids, HLC, Event, paths, capture modes, field ids) | ✅ implemented, tested | `crates/attemptdb-core` |
| Storage engine: framed WAL + spool, torn-tail recovery, memtable, Arrow IPC segments (zstd, dictionary), manifest generations w/ checksums, single-writer lock, idempotent ingest, `.atdb` snapshot export/inspect/extract | ✅ implemented, tested | `crates/attemptdb-storage` |
| Provider adapters (Claude Code 28 events, Codex, Cursor, Gemini CLI) + 63 fixtures + golden + privacy canaries | ✅ implemented, tested | `crates/attemptdb-adapters`, `fixtures/providers` |
| Tier-1 projections: sessions, turns, tool-call pairing, attempts (failed/superseded), handoffs, causal edges, `state_at`, `why_blocked` | ✅ implemented, tested (`tier1-v0`) | `crates/attemptdb-project` |
| DataFusion SQL over `events`/`events_raw`/`sessions`/`turns`/`tool_calls`/`attempts`/`handoffs`/`edges`/`signals` + AttemptQL v0 (SHOW/WHY/TRACE/STATE/DIFF/WHAT IS/EXPLAIN) | ✅ implemented, tested | `crates/attemptdb-query` |
| Capture runtime: hook entrypoint, DB locator, config/device identity, installer for 4 agents, doctor (incl. Codex trust hashes), no-subprocess git info | ✅ implemented, tested | `crates/attemptdb-capture` |
| CLI `attempt` (init, hook install/uninstall/status/<provider>, doctor, status, verify, import, events, snapshot export/inspect/open, timeline, query, why, trace, failures, handoffs, tables; `--snapshot`, `--json`, `--data-dir`, `--db`) | ✅ end-to-end verified on macOS | `crates/attempt` |
| RFCs 0001–0006, storage-format.md, compatibility matrix, ADRs, LICENSE (Apache-2.0), CONTRIBUTING, CoC, SECURITY, issue/PR templates | ✅ drafted | `docs/`, root |
| Daemon / IPC, MCP server, local UI, encryption/blobs, sync, Tier-2/3 inference, release packaging, Windows/Linux runs | ⛔ not started | — |

**Self-hosting is live.** On 2026-08-28 18:38 KST `attempt hook install` (user scope) wired Claude Code (`~/.claude-acct2/settings.json`), Codex (`~/.codex/hooks.json`, awaiting `/hooks` trust), Cursor and Gemini on the owner's machine; the per-user database is `~/Library/Application Support/AttemptDB/db/.attemptdb` (`local_semantic`). Events from the bootstrap session itself started landing immediately (Claude Code hot-reloads settings). Everything before that moment is pre-capture history (TODO §12: import as *reconstructed*).

**Measured:** 165 tests green across the workspace; `cargo clippy --workspace --all-targets` clean; hook wall-clock (process spawn + run, release build, macOS ARM64) p50 8.9 ms / p95 11.0 ms over 60 runs, cold first run 1.4 s (69 MB binary page-in) — the TODO gate is p95 < 10 ms *excluding* host overhead, so in-process time (`attrs.hook_us`) is what to track; release binary is 69 MB because DataFusion is linked into the same executable.

**Design decisions taken this session (see RFCs for detail)**

- WAL/spool payload codec v1 = JSON with string keys (codec id 1); a binary codec id is reserved. Segments are Arrow IPC with `attemptdb.field_id` metadata → the columnar contract is stable even if names change.
- `source_seq` and `hlc` are assigned by the single writer at ingestion, not at capture; hooks write `captured_at`/`observed_at` only. Idempotency key = `event_id` (UUIDv7 minted in the hook process).
- Database location: `--db`/`ATTEMPTDB_DIR` > nearest `.attemptdb/` ancestor > `<data root>/db/.attemptdb`. Data root: `--data-dir`/`ATTEMPTDB_DATA_DIR` > OS dir.
- No daemon is required for v0: hooks append to `spool/inbox.spool` under a lock; every read command claims and imports the spool (single-writer lock, read-only fallback).
- Default capture mode for new installs = `local_semantic` (content stays local, inline in segments for format v1; encrypted blobs are a later format version). Existing VibeMon users stay `metadata_only` until explicit consent.
- Codex: hooks go to `~/.codex/hooks.json`; trust hashes in `config.toml [hooks.state]` are read (scheme verified against real values) but never written.

## Milestone map (from TODO §18)

- M0 Public contract — RFC drafts ✅, license ✅, naming assets ⛔ (needs owner: GitHub org/repo, domains), public claims ⛔ (README rewrite pending end-to-end verification).
- M1 Durable engine — WAL/recovery/memtable/segments/manifest ✅ (macOS only so far), crash-injection tests 🟡 (unit-level torn-tail + tamper tests; no process-kill harness yet), Windows/Linux runs ⛔, compaction ⛔.
- M2 Agent semantics & query — projections ✅, DataFusion/AttemptQL ✅ (v0; work units/decisions/corrections not projected).
- M3 Native capture — hook/spool/installer/doctor ✅, daemon/IPC ⛔, encryption ⛔, real-payload verification for Cursor/Gemini 🟡 (fixtures from a production installer, not re-captured here).
- M4 Inference & correction — Tier-1 ✅, corrections/eval harness ⛔.
- M5 AgentTimeline & self-hosting — CLI timeline ✅, UI ⛔, self-capture ✅ running since 2026-08-28.
- M6 VibeMon bridge ⛔. M7 Show HN ⛔. M8 ⛔.

## Next actions (ordered)

1. Owner: run `/hooks` inside Codex once to trust the AttemptDB entries (`attempt doctor` shows `untrusted` until then); restart Cursor/Gemini sessions.
2. Make the first git commit of the workspace (nothing is committed yet) so captured history can be linked to commits.
3. Import the bootstrap session's pre-capture history as *reconstructed* events (a `attempt import --reconstructed <transcript>` path does not exist yet — design it in RFC 0003 terms).
4. Shrink the hook path: either a separate tiny `attempt-hook` binary or lazy DataFusion linking, to get spawn+run p95 under 10 ms; then track `attrs.hook_us` p95 from real data.
5. Crash-injection harness (kill during WAL append / segment flush / manifest write) + disk-full test; run the suite on Linux and Windows (CI matrix, `docs/rfcs/0005`).
6. Daemon + IPC (Unix socket / named pipe) so hooks can hand off without the spool on the hot path; keep spool as fallback.
7. Encrypted content blobs (format v2), key management per OS, `attempt verify/repair` completeness, compaction.
8. Work units / decisions / corrections projections and the evaluation harness (M4).
9. Decisions needing the owner (TODO §19): license confirmation (Apache-2.0 assumed), GitHub org/repo URL, domains, whether the 1.45M-event aggregate may be published.

## Session log

### 2026-08-28 — bootstrap: workspace, engine, adapters, projections, capture, docs

- Read TODO.md; surveyed the VibeMon hook client (config paths, payload shapes, pitfalls, fixtures) and the official Claude Code hooks reference (28 events, settings precedence, exit-code semantics, hot reload of settings).
- Created the Cargo workspace (7 crates) and implemented core, storage, capture (locator/config/hook/ingest) and the CLI by hand; delegated adapters, projections, installer/doctor/platform/git, the query crate, and the RFC/OSS docs to parallel agents against written contracts.
- Final counts: core 16, storage 14, adapters 26, project 32, query 32, capture 45 — 165 tests green; clippy clean.
- End-to-end verified with simulated Claude + Codex sessions (failed → superseded attempt, WHY blocked on a Codex permission request, handoff Claude→Codex, `.atdb` export/inspect/query). Then installed hooks for real (see above); this session's own tool calls were the first captured events.
- Polish after e2e: handoffs require both sessions to have prompts/tool calls (capture tests and stray events no longer produce handoffs); timeline hides inactive sessions unless `--all`; `failures`/`handoffs` use compact column sets; explanations render as key/value records; doctor distinguishes `verified` (capture test only) from `active` (real events).
- Pre-capture history note (TODO §12): everything in this session happened before AttemptDB could capture itself. It must be imported/marked as *reconstructed*, never presented as captured fact.
