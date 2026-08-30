# AttemptDB

> **The database for what agents tried.**

Git records what changed. AttemptDB records what AI coding agents
*attempted*: the requests they received, the tools they used, the paths they
abandoned, the decisions they made, and the evidence behind the final result.

```text
$ attempt timeline
▌ Claude Code  nullarch/attemptdb  2026-08-28 17:41:02 → open  Full coverage  3 turns · 41 tool calls · 2 failures
  17:41:05 turn 1   completed    Make the WAL recover from a torn tail
    att_0191e3a2 ↻ superseded [string_mismatch] edit crates/attemptdb-storage/src/frame.rs  (1 path)  4.1s  conf 0.9 → att_0191e3b0
    att_0191e3b0 ✓ succeeded   edit crates/attemptdb-storage/src/frame.rs · shell ×2  (1 path)  38.2s  conf 0.9
```

> [!IMPORTANT]
> AttemptDB is pre-release. The local engine, capture, and query layers below
> are real and tested, but formats may still change before the first tagged
> release. There is no hosted service, signup, or telemetry in this repository.

## The missing history before the commit

A commit can tell you that `frame.rs` changed. It cannot tell you:

- which agent was asked to change it and what it believed the problem was;
- which approaches were attempted and discarded;
- whether a tool failed or a human denied permission;
- why the final approach won;
- which agent, subagent, test, or artifact produced the evidence;
- what the project looked like halfway through the work.

Coding agents already emit fragments of this history through hooks. AttemptDB
turns those fragments into a local, queryable temporal and causal record.

## AttemptDB Event v1

The canonical event is published as an open format: `spec/event-v1.schema.json`
is the JSON Schema of exactly what this implementation writes, and
`attempt conformance <events.jsonl>` checks a stream against it — identity
derivation, per-device ordering, span resolution, capture-mode discipline, and
the content-free `attrs` contract — then prints `COMPATIBLE` or the findings.
CI validates every golden fixture against the schema and round-trips a fully
populated event, so the schema cannot drift from the code. See
[`spec/README.md`](spec/README.md).

## Install

No release is tagged yet, so the source build is currently the only path that
works. The other two are wired up and will work from the first tag.

```sh
# From source — works today. Requires a Rust toolchain (1.94+).
cargo install --git https://github.com/nullarch/attemptdb attempt

# From a release — macOS and Linux. Verifies the published SHA256.
curl -fsSL https://raw.githubusercontent.com/nullarch/attemptdb/main/install.sh | sh

# Windows PowerShell.
irm https://raw.githubusercontent.com/nullarch/attemptdb/main/install.ps1 | iex
```

Neither installer touches a coding agent's configuration. Hooks are wired only
when you run `attempt hook install` yourself.

Release binaries are not code-signed yet, so macOS Gatekeeper will object to a
manually downloaded archive; `install.sh` clears the quarantine flag the same
way Homebrew does. See [`docs/releasing.md`](docs/releasing.md) for what each
release publishes.

## Quick start

```sh
attempt init                               # per-user database (or `--local` for ./.attemptdb)
attempt hook install                       # wires Claude Code, Codex, Cursor, Gemini CLI hooks
attempt doctor                             # configured / trusted / active per agent
attempt daemon install                     # optional: background writer (launchd / systemd --user)
attempt import claude-transcripts          # optional: reconstruct history from before the hooks
attempt mcp --print-config                 # optional: let your agents query AttemptDB over MCP
# ...work normally with your coding agent...
attempt timeline                           # sessions → turns → attempts, with evidence
attempt failures                           # SHOW FAILED ATTEMPTS
attempt why                                # WHY project STATUS BLOCKED (evidence-backed)
attempt query "SELECT kind, count(*) FROM events GROUP BY 1 ORDER BY 2 DESC"
attempt snapshot export history.atdb       # portable, checksummed, read anywhere
attempt --snapshot history.atdb timeline
attempt snapshot export public.atdb --sanitized   # no prompts/commands/output/raw payloads/home paths
attempt snapshot audit public.atdb                # privacy review before you publish it
```

Nothing leaves your machine. Prompts, commands, and tool output stay local
(`local_semantic` mode); `attempt init --capture-mode metadata_only` keeps
only content-free metadata.

## Architecture

```text
Claude Code / Codex / Cursor / Gemini CLI
        │  hooks (attempt hook <provider>, ~ms, exit 0 always)
        ▼
  spool/inbox.spool ──► single writer ──► wal/NNNNNN.wal ──► segments/seg-*.arrow
  (framed, CRC32C)      assigns seq+HLC   (durable ack)      (Arrow IPC, zstd, dictionary)
                                                                 │
                                       manifest/gen-NNNNNN.json  ◄┘  newest valid generation wins
                                                                 │
                 Tier-1 projections (sessions, turns, tool calls, attempts, handoffs, causal edges)
                                                                 │
                    DataFusion SQL  +  AttemptQL (SHOW / WHY / TRACE / STATE / DIFF)
                                                                 │
                              attempt CLI  ·  .atdb snapshots  ·  (planned) MCP, UI, daemon
```

- **Owned storage engine, not SQLite.** A framed write-ahead log with
  checksummed torn-tail recovery, an in-memory table for recent writes, and
  immutable Arrow IPC columnar segments for history, tied together by
  generation-numbered manifests. The byte-level contract is in
  [`docs/storage-format.md`](docs/storage-format.md).
- **Apache Arrow + DataFusion** as the in-memory format and SQL substrate.
  AttemptDB owns the data model, the temporal/causal semantics, the
  projections, and AttemptQL.
- **Facts and inferences are separate.** Events are immutable observed
  facts. Attempts, blockers, and handoffs are versioned inferences that carry
  evidence event ids, a confidence, and an algorithm version (`tier1-v0`).
- **Privacy is a storage property.** Content-free metadata lives in an
  allowlisted `attrs` map; anything content-bearing is gated by the capture
  mode and never synced by default. Adapter tests include privacy canaries.

## Primitives

| Primitive | Meaning | Status |
| --- | --- | --- |
| `Event` | An observed prompt, tool call, file effect, lifecycle event | implemented |
| `Session` / `Turn` / `ToolCall` | Deterministic grouping of events | implemented |
| `Attempt` | One approach toward an objective, whether it succeeded, failed, or was superseded | implemented (Tier 1) |
| `Handoff` | Work moving between agents | implemented (heuristic) |
| `CausalEdge` | `parent_of`, `caused`, `triggered`, `superseded`, `handed_off`, `evidence_for`, … | implemented |
| `WorkUnit` / `Decision` / `Artifact` / `Correction` | Higher-level inferred project work | specified in RFCs, not yet projected |

## Querying

```sql
SHOW FAILED ATTEMPTS FOR project = 'attemptdb';
WHY ses_0191e3a1 STATUS BLOCKED;
TRACE att_0191e3b0 CAUSES;
STATE project AT '2026-08-28T09:00:00Z';
SHOW HANDOFFS BETWEEN agent = 'claude_code' AND agent = 'codex';
SHOW EVIDENCE FOR att_0191e3a2;
SELECT tool_name, outcome_status, count(*) FROM events GROUP BY 1, 2;
```

Every `WHY`, `TRACE`, and `STATE` answer returns evidence ids and an
uncertainty note. "Insufficient evidence" is a valid answer. Grammar and
semantics: [`docs/rfcs/0004-attemptql.md`](docs/rfcs/0004-attemptql.md).

## What is implemented today

- `attemptdb-core` — ids (UUIDv7 / deterministic UUIDv5), hybrid logical
  clock, canonical event model with stable numeric field ids, portable paths.
- `attemptdb-storage` — WAL + spool framing, recovery, memtable, Arrow
  segments, manifests, single-writer lock, idempotent ingest, `.atdb`
  snapshot export/inspect/extract, `attempt verify`.
- `attemptdb-adapters` — Claude Code (all documented hook events), Codex,
  Cursor, Gemini CLI, with sanitised fixtures and golden envelopes.
- `attemptdb-project` — Tier-1 projections and `state_at` / `why_blocked`.
- `attemptdb-query` — DataFusion SQL over `events`, `sessions`, `turns`,
  `tool_calls`, `attempts`, `handoffs`, `edges`; AttemptQL v0.
- `attemptdb-capture` — hook entrypoint, database locator, structural
  installer with backup/lock/atomic replace for four agents, doctor (including
  Codex `/hooks` trust state), subprocess-free git info.
- `attempt` CLI — `init`, `hook install|uninstall|status`, `doctor`, `status`,
  `verify`, `events`, `timeline`, `query`, `why`, `trace`, `failures`,
  `handoffs`, `snapshot export|inspect|audit` (with `--sanitized` exports),
  `uninstall`. Hook overhead: ~0.6 ms in-process, ~5 ms wall including
  process spawn (macOS ARM64, release build).

- Capture daemon (`attempt daemon`) — Unix socket / named pipe IPC with
  CRC32C frames, group-commit writer, periodic spool import, launchd and
  `systemd --user` service files. Without it, hooks spool to disk and every
  read command imports the spool; with it, hooks hand events to the daemon and
  get an acknowledgment after the WAL fsync.
- Transcript import (`attempt import claude-transcripts`) — reconstructs
  sessions from Claude Code's own transcript files, marks every event
  `reconstructed`, and merges with hook-captured events of the same session.
- MCP server (`attempt mcp`) — stdio JSON-RPC with tools `attempt_status`,
  `attempt_timeline`, `attempt_failures`, `attempt_why`, `attempt_trace`,
  `attempt_state_at`, `attempt_evidence`, `attempt_query`, and
  `attempt_handoff_brief` (a continuation brief with evidence ids and an
  explicit uncertainty section). This repository's `.mcp.json` registers it.
- `attempt repair` / `attempt snapshot restore` — adopt unreferenced segments,
  rebuild a manifest from verifiable segments, quarantine corrupt files,
  restore a snapshot with a backup of the current database.
- Crash-injection test harness — failpoints for kill-during-WAL/segment/manifest
  writes, simulated disk-full, concurrent spool writers, corrupted files.
- Encrypted content blobs (`attempt keys`) — XChaCha20-Poly1305, keyed-hash
  content ids, OS key store / key file / passphrase, portable-key snapshots.
- Local web UI (`attempt ui`) — token-authenticated loopback explorer with
  timeline, session waterfall, attempt evidence and causal trace, why/state/
  query pages, and `attempt ui export` for a self-contained sanitized HTML page.
- Work units, derived decisions, corrections and retractions
  (`attempt correct`, `attempt retract`) as first-class events honoured by
  every projection and by sanitized exports.

Not yet: segment compaction, incremental projections for very large
databases, sync to a hosted service, Tier-2/3 semantic inference, an
evaluation dataset, signed releases, Windows and Linux test runs (CI matrix
exists, unverified). See [`PROGRESS.md`](PROGRESS.md)
and [`TODO.md`](TODO.md).

## Benchmarks

Measured on one machine (Apple M5 Pro, macOS 26.4) with a 1.45 M-event
synthetic workload modelled on real distributions: ingest 8.6 k events/s with
a fsync per batch, WAL acknowledgment 3.0 ms (the macOS `F_FULLFSYNC` floor),
hook process 3.8 ms wall / 124 µs in-process, causal trace 187 µs on a
2 M-edge graph. The unflattering numbers are there too: a whole-history query
engine over 1.45 M events costs 160 s and 21 GiB, and `STATE … AT` scales with
open sessions. Methodology, raw JSON, and the pathological cases:
[`docs/benchmarks.md`](docs/benchmarks.md).

## What AttemptDB is not

- another vector database for extracted user preferences;
- an LLM tracing dashboard with a new skin;
- a replacement for Git;
- a claim that inferred intent is ground truth;
- a reason to upload private prompts or source code by default;
- a wrapper that hides SQLite while claiming a from-scratch database.

## Why this project exists

AttemptDB grows out of operating [VibeMon](https://vibemon.dev), which has
handled more than 1.45 million metadata-only coding-agent events. That stream
was enough to measure activity, but not enough to answer the questions we kept
wanting to ask: what is the agent actually trying to finish, why is it blocked,
which approaches already failed, and can the next agent continue without the
human explaining everything again.

AttemptDB is the open-source data layer for those questions. AgentTimeline is
the human-facing view of that data. VibeMon remains the optional hosted and
mobile companion for sync, remote status, and moments that need attention.

## Self-hosting

AttemptDB records the agents that build AttemptDB. History from before the
first hook install is imported and marked as reconstructed, never presented as
captured fact. A sanitised `.atdb` snapshot of the build history will ship
with the first release so every query above can be run against it.

## Documentation

- [`docs/rfcs/0001-canonical-event-model.md`](docs/rfcs/0001-canonical-event-model.md)
- [`docs/rfcs/0002-storage-engine.md`](docs/rfcs/0002-storage-engine.md) · [`docs/storage-format.md`](docs/storage-format.md)
- [`docs/rfcs/0003-fact-inference-bitemporal-model.md`](docs/rfcs/0003-fact-inference-bitemporal-model.md)
- [`docs/rfcs/0004-attemptql.md`](docs/rfcs/0004-attemptql.md)
- [`docs/rfcs/0005-cross-platform-runtime.md`](docs/rfcs/0005-cross-platform-runtime.md)
- [`docs/rfcs/0006-privacy-and-sync.md`](docs/rfcs/0006-privacy-and-sync.md)
- [`docs/compatibility-matrix.md`](docs/compatibility-matrix.md) · [`SECURITY.md`](SECURITY.md) · [`CONTRIBUTING.md`](CONTRIBUTING.md)

## License

Apache-2.0. See [`LICENSE`](LICENSE).
