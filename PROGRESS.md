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
| Capture daemon + IPC (Unix socket / named pipe, `ATIP` frames, group commit, spool import loop, launchd/systemd service) | ✅ implemented, tested, running on the owner's machine | `crates/attemptdb-capture/src/{ipc,daemon,service}.rs` |
| Claude Code transcript import (reconstructed history, deterministic ids, idempotent) | ✅ implemented, tested; bootstrap session imported | `crates/attemptdb-adapters/src/transcript`, `crates/attemptdb-capture/src/import.rs` |
| Crash-injection harness (failpoints, SIGKILL rounds, ENOSPC, concurrent spool writers) | ✅ 25 tests, ~3.5 s | `crates/attemptdb-storage/tests/crash.rs` |
| MCP server (`attempt mcp`): 10 tools incl. `attempt_schema` and `attempt_handoff_brief`, resources, `--print-config`/`--install`, project `.mcp.json` | ✅ implemented, tested (21), real-data smoke | `crates/attemptdb-mcp`, `crates/attempt/src/cmd_mcp.rs` |
| `attempt repair` (adopt/rebuild/quarantine/tmp/identity) and `snapshot restore` | ✅ implemented, 18 scenario tests | `crates/attemptdb-storage/src/repair.rs`, `crates/attempt/src/cmd_repair.rs` |
| Encrypted content blobs (XChaCha20-Poly1305, keyed-hash ids, segment format 2) + key management (keyring / key file / passphrase, `attempt keys`), key-aware snapshots | ✅ implemented, tested (storage 79, capture 72) | `crates/attemptdb-storage/src/blobs.rs`, `crates/attemptdb-capture/src/keys.rs`, `crates/attempt/src/cmd_keys.rs` |
| Local web UI `attempt ui` (token-authed loopback, overview/timeline/work/attention/session/attempt/failures/handoffs/why/state/query, JSON API, SVG trace) + `attempt ui export` static sanitized HTML | ✅ implemented, tested (16 e2e), smoke-tested on the live DB | `crates/attemptdb-ui`, `crates/attempt/src/cmd_ui.rs` |
| Agent Timeline product surfaces (2026-09-03): Overview redesign, Work board + inspector, high-precision Needs You queue, live invalidation over SSE, bundled demo mode, sanitized SVG summary card | ✅ implemented, tested — see the 2026-09-03 session entry | `crates/attemptdb-project/src/attention.rs`, `crates/attemptdb-ui/src/{pages,api,demo,card}.rs` |
| Query catalog (2026-09-03): every table's grain/joins and every column's meaning and vocabulary, from one source that cannot drift; `attempt schema`, the `attempt_schema` MCP tool, generated `docs/query-context.md`, 23 worked examples executed as tests | ✅ implemented, tested (query 7 + CLI 5 + MCP 1) | `crates/attemptdb-query/src/catalog.rs`, `crates/attempt/src/cmd_schema.rs`, `docs/query-context.md` |
| Work units, derived decisions, corrections, retractions (`attempt correct`, `attempt retract`), new tables + AttemptQL statements | ✅ implemented, tested (project 44, query 38) | `crates/attemptdb-project/src/{workunit,decision,meta}.rs`, `crates/attemptdb-query`, `crates/attempt/src/cmd_correct.rs` |
| Benchmark program (`attemptdb-bench`, 1.45 M-event synthetic replay) + `docs/benchmarks.md` | ✅ run on macOS ARM64; pathologies documented | `crates/attemptdb-bench`, `docs/benchmarks.md`, `docs/benchmarks/2026-08-29-macos-arm64.json` |
| Sync client (peers, profiles, cursors, secret scanning), `attemptdb-server` (per-tenant databases, key scopes, admin keys, device removal, read API, legacy envelope, backfill importer), deployment files | ✅ implemented, tested (waves 5–13); not deployed | `crates/attemptdb-capture/src/sync.rs`, `crates/attemptdb-server`, `deploy/`, `docs/server-api.md`, `docs/deploy.md` |
| `attempt-hook` (0.8 MB hook entrypoint), incremental projection, on-disk compatibility fixture, commit linkage, rollback-safe `attempt update` | ✅ implemented, tested | `crates/attempt-hook`, `crates/attemptdb-query/src/cache.rs`, `fixtures/db`, `crates/attemptdb-project` |
| Read-path engine audit and rework (2026-09-02): Arrow-only segment cache, per-segment derived parts and facts, lazy SQL layer and per-table materialisation, content read only on demand, per-session projection index, CLI reads through the daemon (`QUERY`/`RESULT` frames) | ✅ implemented, tested (522), measured — see the 2026-09-02 session entry | `crates/attemptdb-query/src/{cache,parts,facts,lazy}.rs`, `crates/attemptdb-storage/src/cache.rs`, `crates/attempt/src/read_service.rs`, `crates/attemptdb-project/src/model.rs` (`SessionIndex`) |
| Tier-2/3 inference, evaluation harness/gold dataset, OTel intake (ADR 0003), Windows daemon, Windows/Linux runs of the durability suites, release (first tag) | ⛔ not started / owner | — |

**Self-hosting is live.** On 2026-08-28 18:38 KST `attempt hook install` (user scope) wired Claude Code (`~/.claude-acct2/settings.json`), Codex (`~/.codex/hooks.json`, awaiting `/hooks` trust), Cursor and Gemini on the owner's machine; the per-user database is `~/Library/Application Support/AttemptDB/db/.attemptdb` (`local_semantic`). Events from the bootstrap session itself started landing immediately (Claude Code hot-reloads settings). Everything before that moment is pre-capture history (TODO §12: import as *reconstructed*).

**Measured (2026-08-29, wave 3):** 347 tests green, `cargo clippy --workspace --all-targets` and `cargo fmt --all --check` clean across the workspace; `cargo clippy --workspace --all-targets` clean; hook wall-clock (process spawn + run, release build, macOS ARM64) p50 8.9 ms / p95 11.0 ms over 60 runs, cold first run 1.4 s (69 MB binary page-in) — the TODO gate is p95 < 10 ms *excluding* host overhead, so in-process time (`attrs.hook_us`) is what to track; release binary is 69 MB because DataFusion is linked into the same executable.

**Design decisions taken this session (see RFCs for detail)**

- WAL/spool payload codec v1 = JSON with string keys (codec id 1); a binary codec id is reserved. Segments are Arrow IPC with `attemptdb.field_id` metadata → the columnar contract is stable even if names change.
- `source_seq` and `hlc` are assigned by the single writer at ingestion, not at capture; hooks write `captured_at`/`observed_at` only. Idempotency key = `event_id` (UUIDv7 minted in the hook process).
- Database location: `--db`/`ATTEMPTDB_DIR` > nearest `.attemptdb/` ancestor > `<data root>/db/.attemptdb`. Data root: `--data-dir`/`ATTEMPTDB_DATA_DIR` > OS dir.
- No daemon is required for v0: hooks append to `spool/inbox.spool` under a lock; every read command claims and imports the spool (single-writer lock, read-only fallback).
- Default capture mode for new installs = `local_semantic` (content stays local, inline in segments for format v1; encrypted blobs are a later format version). Existing VibeMon users stay `metadata_only` until explicit consent.
- Codex: hooks go to `~/.codex/hooks.json`; trust hashes in `config.toml [hooks.state]` are read (scheme verified against real values) but never written.

## Milestone map (from TODO §18)

- M0 Public contract — RFC drafts ✅, license ✅, GitHub repo ✅ (`github.com/nullarch/attemptdb`, private), release + installer pipeline ✅ (never executed — no tag, no CI run yet), domains ⛔, Homebrew tap repo ⛔ (owner), signing ⛔.
- M1 Durable engine — WAL/recovery/memtable/segments/manifest ✅, crash-injection harness ✅ (28 SIGKILL/SIGABRT scenarios incl. compaction), compaction ✅ (wave 13), on-disk compatibility fixture ✅, Windows/Linux runs ⛔ (CI blocked on billing → public repo).
- M2 Agent semantics & query — projections ✅, DataFusion/AttemptQL ✅ (v0; work units/decisions/corrections not projected).
- M3 Native capture — hook/spool/installer/doctor ✅, daemon/IPC ✅, encryption ✅ (segment format 2 + key store; enable per database with `attempt keys init`), real-payload verification for Cursor/Gemini 🟡 (fixtures from a production installer, not re-captured here); Windows/Linux runs ⛔ (CI pending a remote).
- M4 Inference & correction — Tier-1 ✅ incl. work units, derived decisions, corrections and retractions; evaluation harness / gold dataset ⛔ (needs design partners).
- M5 AgentTimeline & self-hosting — CLI timeline ✅, MCP ✅, local web UI ✅, static sanitized export ✅, self-capture ✅ running since 2026-08-28; one-minute demo recording ⛔.
- M6 VibeMon bridge ⛔. M7 Show HN ⛔. M8 ⛔.

## Next actions (ordered, after wave 13 — 2026-08-30)

1. **Owner, today (TODO §21.1):** release `vibemon-hooks` v30 (production collection has been broken since 2026-08-26); make `nullarch/attemptdb` public (Actions billing, first tag, `attempt update`, Homebrew tap); take the three decisions — tenant = organisation or user, default sync profile, realtime path — and the OTel intake decision (ADR 0003).
2. First tag → release workflow runs for real → `install.sh`/`install.ps1` and `attempt update` verified against a published release; the Linux/Windows CI runs of the wave-13 code.
3. vibemon repositories (TODO §21.8): `vibemon.dev/install.sh` serves `docs/migration/vibemon-install.sh`; the `/hook` Edge Function forwards to `POST /v1/vibemon/hook`; "connect a device" calls `POST /v1/admin/keys {tenant, user_id, scope: device}`; the web reads `/v1/timeline`, `/v1/attention`, `/v1/sessions`, `/v1/devices` instead of `hook_events`; backfill with `attempt import vibemon-export`.
4. Deploy `attemptdb-server` (`deploy/`, `docs/deploy.md`): one VM, one volume, Caddy; then run `attempt hook install --remove-legacy vibemon` live on this machine.
5. Engine: segment compaction wired into the daemon's flush loop and the server's idle sweep (the engine half is wave 13's last agent); the 0.45 s reload floor (cache readable batches per segment, typed projection builders).
6. Windows: the per-user daemon (service registration + the `cfg(unix)` durability suites), then the Scheduled Task stopgap in `vibemon-install.ps1` goes away.
7. Open engineering findings: Tier-2 inference records (RFC 0003 `Inference` store + provider trait), evaluation harness / gold dataset (design partners).

## Wave 3 plan (2026-08-29 afternoon) — close the milestone gaps

| Gap | Milestone | Owner | Definition of done |
|---|---|---|---|
| CI on Linux/Windows | M1/M3 | main session | private repo pushed, matrix green (or failures triaged into issues) |
| Encrypted content blobs + key management | M3 | agent A | segment format v2 with blob refs, XChaCha20-Poly1305 blobs, OS key store via `keyring` + key-file fallback, `attempt keys`, snapshot export with portable key, v1 segments still readable |
| Local web UI | M5 | agent B | `attempt ui` on an authenticated loopback port: state, timeline, attempt/evidence, why, trace, query console, time travel, coverage/privacy indicator; static sanitized HTML export |
| Work units, decisions, corrections, retractions | M4 | agent C | `work_units`/`decisions` tables + AttemptQL, `attempt correct`/`attempt retract` as first-class correction events honoured by projections and sanitized exports |
| Benchmark program | §15 | agent D | synthetic workload from real distributions, 1.45M-event replay, ingest/ack/query/traversal/size numbers in `docs/benchmarks.md` with pathological cases |
| VibeMon shadow validation | M6 | main session | per-session event counts VibeMon vs AttemptDB for this device, documented |

## Wave 4 (2026-08-29 evening) — distribution and open-source surface

Local-first means no server, but it does not mean no distribution. Before this
wave the project had no git remote at all, no release workflow, and an install
path that required cloning the repo and having a Rust toolchain.

| Item | State | Note |
|---|---|---|
| GitHub repository | done | `nullarch/attemptdb`, **private**; flip to public when the pre-public checklist below is clear |
| `.github/workflows/release.yml` | written, never run | tag-driven; 5 core targets gate the release, 3 ARM targets are optional and reported honestly in the notes |
| `install.sh` / `install.ps1` | written, never run against a real release | resolve latest tag, verify `SHA256SUMS`, install to `~/.local/bin` / `%LOCALAPPDATA%\AttemptDB\bin`; neither touches agent config |
| `docs/releasing.md` | done | per-platform signing status, what is and is not automated |
| Repository URL placeholders | done | `streamize/attemptdb` -> `nullarch/attemptdb` in `Cargo.toml`, README, RFC 0001 |
| `SECURITY.md` | done | reporting now points at GitHub private vulnerability reporting instead of an unowned mailbox; signing section states reality |
| Homebrew tap | blocked | creating a **public** repo is blocked for the agent; owner runs `gh repo create nullarch/homebrew-attemptdb --public` |
| Code signing | not started | Apple Developer membership + Windows certificate are purchases, not code |

No `TODO.md` item under *Distribution* or *Release targets* is ticked yet:
every one of them requires a workflow run that has not happened. The pipeline
existing is not the same as the pipeline working, and the milestone map says
so.

### What the first CI runs found

Four platform defects, none of which could have been found on this machine.

| Finding | Platform | Cause |
|---|---|---|
| 23 clippy lints | all | local 1.94.1 vs CI `stable` = 1.98.0; fixed, and the toolchain is now pinned |
| `attemptdb-bench` does not compile | Windows | `std::os::fd`, `libc::{rusage,getrusage,fsync}` used unconditionally; now `cfg(unix)` and `peak_rss_bytes()` returns `Option` so Windows reports a blank rather than a fake zero |
| smoke step never had a binary | macOS/Linux ARM | the step assumed `cargo test` leaves `target/<triple>/debug/attempt`; it does not. The step had never run before, so the assumption had never been tested |
| `test` exceeded 60 min | linux-x86_64 | clippy is `check`-shaped and produces no codegen, so `test` pays for a full debug codegen of DataFusion on a cold cache; budget raised to 90 min |

Run 3 found two more, both real:

| Finding | Platform | Cause |
|---|---|---|
| `cmd_mcp::tests::snippets` fails | Windows | `quote_for_shell` correctly emits `"..."` on Windows and `'...'` on POSIX; the test hardcoded the POSIX form. The test now builds its expectation with the same helper, so it asserts composition instead of one platform's quoting |
| `ld terminated with signal 7 [Bus error]` | linux-x86_64 | lld mmaps the output file and takes SIGBUS when the filesystem fills under it. Full debug info for a DataFusion-sized tree fills a standard runner. CI now builds with `line-tables-only` (backtraces keep file:line) and frees the ~20 GB of preinstalled toolchains first |

The Bus error also explains run 2's 90-minute timeout on the same platform:
the runner was already out of headroom.

Run 4 was green everywhere except Windows, where the last finding was a
**test** defect with a real lesson: `doctor::tests::codex_untrusted_until_hashes_match`
synthesises a Codex `[hooks.state]` table whose key is a filesystem path, and
wrote it into a TOML **basic** string. On Windows the path's backslashes are
escape sequences there, so the key silently changed and the state came back
`Untrusted` instead of `Active`. The product only ever reads that table (it
must never write it), so the escaping belongs in the test.

One open finding: `crash::abort_wal_append_after_write` and
`abort_manifest_after_tmp_write_leaves_a_tolerated_tmp_file` failed on
**macos-x86_64 only**, both with `Locked` on a writer open taken straight
after `drop(db)`. Closing the lock file releases the `flock` synchronously, so
this should be impossible; 12 consecutive local runs on macOS ARM64 pass and
the ARM64 CI job passes too. `open_eventually` in `crash.rs` now waits up to 5
seconds and reports how long it actually waited, which separates the two
possible causes — a lagging lock release (milliseconds) from a genuinely
leaked handle (budget exhausted, and then the fix belongs in the engine).

Run 3 was green on macos-x86_64 and printed no wait, but that proved nothing:
libtest captures the print macros for a passing test. Writing to the real
stderr fixed the visibility — and then the numbers it printed in run 4 turned
out to be measuring the wrong thing. It timed the whole open, not the retries,
and duly reported a "lock wait" for read-only opens, which take no lock at
all. The helper counts retries now. Nothing about the original macos-x86_64
failure is explained yet; it stays open until an Intel run reports a non-zero
retry count.

**Run 5 (2026-08-31, the first Intel run since the repository went public):
green.** `abort_wal_append_after_write`, `abort_wal_append_after_sync` and
`abort_manifest_after_tmp_write_leaves_a_tolerated_tmp_file` are all `ok` in
the macos-x86_64 log; 518 tests passed, zero failures, and `tests/crash.rs`
genuinely ran rather than being skipped. The retry line appeared zero times,
and since `open_eventually` prints only when `retries > 0` that is a hard zero:
every writer reopen succeeded first try. So the diagnostic is now proven wired
and silent for the right reason, rather than silent because libtest swallowed
it — which is what run 3 wrongly looked like.

Be precise about what that does and does not settle. It does **not** explain
the original two failures; it says they did not recur, for a third consecutive
Intel run, with an instrument that had a real chance to speak and had nothing
to record. The original criterion — "stays open until an Intel run reports a
non-zero retry" — cannot close on evidence like this, because it can only be
met by a reproduction. That criterion was wrong, and replacing it is a
judgement, so here it is stated as one: **three clean runs against working
instrumentation are taken as evidence that the original failures were
environmental, and the finding moves from open to not reproducing.** It is not
fixed, and nothing in the engine changed. What would reopen it: any Intel
failure, or any run that prints a non-zero retry count. The instrumentation
stays in place for exactly that.

**That judgement was wrong, and it lasted about two hours.** The very next
run reproduced it — `corrupt_segment_is_reported_by_verify_never_panics`,
`Locked` on a writer reopen straight after `drop(db)`, on **linux-x86_64**.
So it was never macOS-x86_64-specific and it was never environmental in the
sense of "someone else's machine"; it is timing, and the platform in the
original report was a coincidence of which job happened to lose the race.

The mechanism is almost certainly the one the `ETXTBSY` bug taught us the same
afternoon, in its other form. `flock` belongs to the open file description,
and a child forked by one thread inherits every descriptor open at that
instant — so it inherits the lock. This suite spawns real child processes
(`crash_writer`, `spool_writer`) from tests running in parallel threads, and
between another thread's `fork` and its `exec` that child holds a duplicate of
the lock descriptor. `drop(db)` releases our reference; the lock survives until
the child execs. Microseconds, which is why it is rare, unreproducible in
isolation, and indifferent to platform except through timing.

The harness already had the answer and applied it inconsistently:
`open_eventually` retries exactly this, and seven call sites used it while
**ten writer opens called `Database::open(…).unwrap()` directly**. Every one of
those was a latent flake. All ten now go through the helper.

Two conclusions worth keeping. The engine is not implicated — retrying is right
in the harness and would be wrong in the product, where `Locked` is a real signal
that another writer holds the database. And a closing criterion that can only
be met by a reproduction should have been replaced by *investigation*, not by a
judgement that three green runs meant absence; the investigation was one grep
away.

### Run 5: green on all five Tier 1 targets

macOS ARM64 · macOS x86_64 · Linux x86_64 · Linux ARM64 · Windows x86_64, plus
the musl static build and the MSRV job. Seven defects, all found by CI and none
findable on this workstation.

**What green does not mean.** `crash.rs` (25 tests), `repair.rs` (18) and
`daemon.rs` (4) carry `#![cfg(unix)]`, so on Windows those binaries run zero
tests and report `ok`. Windows is proven to compile, pass unit tests, and get
through the CLI smoke path; **its durability, recovery and daemon behaviour are
untested**. TODO §Cross-platform CI now says so at the item that is easiest to
misread as done.

The macos-x86_64 lock failure did not recur, and the corrected diagnostic
reported **zero retries** — so the retry helper contributed nothing to run 5's
green. Two clean Intel runs, no explanation, no reproduction. It stays open.

### CI is now blocked on Actions billing, not on code

The second release dry run reported all eight targets as failures. They were
not: the jobs never started. The annotation reads "The job was not started
because recent account payments have failed or your spending limit needs to be
increased." A billing stop and a code failure look identical in the run list
and differ in the annotations — check those before debugging code that never
ran.

Eight runs cost roughly 4,200 billable minutes. macOS bills at 10x and was
about 87% of the total; GitHub Free allows 2,000 a month and Pro 3,000.
**Actions is free on public repositories**, so going public clears the block
and removes the recurring cost — a cost argument on top of the launch one.
If it stays private, the levers in order of effect are: drop one of the two
macOS jobs, restrict the macOS matrix to tags and pull requests instead of
every push, or raise the spending limit.

Waste already removed without touching coverage: superseded runs are cancelled
(`concurrency`, never for a tag), and `cargo fmt --check` runs on one runner
rather than five.

Still unverified because of the block: the corrected musl staticness check
(`file(1)` rather than matching ldd phrasing). The first dry run proved the old
check wrong; the replacement has not run.

### Hosted-backend audit (2026-08-30)

AttemptDB exists to become VibeMon's collection backend as well as a local
tool. Audited against that role — every item below was verified in code or by
running it, not inferred.

**Blockers before it can serve a dashboard**

1. *Every read rebuilds the world, and every write invalidates it.* The UI and
   MCP stores cache one view per scope keyed on WAL+manifest mtime
   (`attemptdb-ui/src/store.rs:377`), so any ingest evicts it; a reload is a
   full `scan` → `Vec<Event>` → Arrow batches → `project()` → causal graph →
   `MemTable`, five copies of the data. Benchmarks: 100 k events = 3.1 s /
   1.8 GiB, 500 k = 19 s / 7.8 GiB. There is no incremental projection and no
   segment cache (TODO 2a, undone). A per-user DB under continuous ingest pays
   this on every refresh.
2. *Ingest cannot be idempotent with envelope v2.* `EventId` is a random
   UUIDv7 minted in `Event::new` (`core/src/event.rs:559`); ingest dedupes by
   that id (`storage/src/db.rs::is_known`). The VibeMon envelope carries no
   event id or nonce (required: `v event agent cwd timestamp payload signals`).
   A retry therefore stores a duplicate — and `notify.sh` does not retry at
   all today, so delivery is at-most-once and silently lossy. Fix belongs in
   the hooks contract: envelope v3 with a client-minted event id.
3. *Arrival order is not event order.* `VIBEMON_TS` is second-precision
   (`notify.sh:126`) and each event leaves in its own detached `nohup curl`
   (`notify.sh:1020`). AttemptDB orders by `observed_at`, then HLC (server
   receipt). Within one second the order is arbitrary — PostToolUse before
   PreToolUse — and the projector's turns/attempts are wrong. Envelope v3
   needs millisecond timestamps and a per-session monotonic sequence.

**High — before any public SQL endpoint**

4. *The engine has no SQL policy.* The DataFusion context is built with a
   default `SessionConfig` (`query/src/lib.rs:127`), no `SQLOptions`;
   `attemptql::is_sql` accepts `CREATE`/`INSERT`/`DROP`/`SET`. Verified:
   `attempt query "CREATE EXTERNAL TABLE hosts STORED AS CSV LOCATION
   '/etc/hosts'"` exits 0 after reading the file; a nonexistent path fails
   with `No files found at file:///…`. UI `/api/query` and the MCP tool
   refuse it via `check_read_only` (a keyword-prefix check on `READ_VERBS`),
   which is an app-layer string test in front of an engine that will do
   anything. **Fixed the same day:** `QueryEngine::sql`/`explain` now go
   through `sql_with_options` with DDL, DML and statements disabled, and
   `engine_is_read_only_at_the_engine_layer` proves CREATE EXTERNAL TABLE,
   CREATE TABLE/VIEW, INSERT, SET and COPY are refused by the engine at every
   entry point while SELECT/EXPLAIN still work. The same CLI command now
   fails with `DDL not supported: CreateExternalTable`.
5. *Content policy is enforced by tests, not by the engine.*
   `apply_capture_mode` strips only `content`/`raw` (`event.rs:602`); `attrs`
   is a free `Map<String, Value>` and `unknown` is preserved. Safe while the
   server authors every Event through its own adapter; not safe the day it
   accepts pre-normalised Events from clients.
6. *`commit.message` is content.* VibeMon signals carry commit titles
   (≤200 chars, on by default); AttemptDB's canary lists `message`/`title` as
   content keys. A faithful adapter puts it in `content`, which
   `metadata_only` strips — VibeMon's `commit_recent` loses commit titles.
   Product decision, not a bug.

**Medium — capacity and operations**

7. Per-open-DB memory: memtable up to `flush_events 20 000` / 64 MiB plus WAL
   and lock handles, multiplied by open DBs; needs an open-DB LRU. The dedupe
   id index is bounded in practice: segments are filtered by `min/max
   event_id`, and UUIDv7 ids are time-ordered, so live ingest only loads the
   newest segment's ids.
8. No compaction; per-user DBs with the daemon's 15-minute periodic flush
   accumulate small segments. Measured cost is mild (200 vs 2 segments: +6 %
   scan, 2.3× open) and manifest growth is already pruned, so this is a
   next-year problem, not a launch one.
9. Sync engine inside async handlers (`store.rs::load` scans on the runtime
   thread); the daemon's writer-thread-plus-channel is the right shape for a
   server. Old-binary/old-schema compatibility is untested (TODO), which
   matters for rolling upgrades across thousands of DBs.

**Fits hosting well**: Linux is the tested platform (crash/repair suites run
there); `ProjectId` derives from the remote, so the same repo gets the same id
in every user's DB and team roll-ups can join; the exclusive flock forces
per-user DBs, which is physical tenancy; read-only open takes no lock and
replays the WAL in ~10 ms; `.atdb` is already a backup format; account
deletion is `rm -r`; metadata-only events cost 134 B each.

### Wave 5 (2026-08-30) — the hosted path, minus anything that bends the concept

Goal: let a user's machine write to an AttemptDB we operate, without turning
the OSS engine into a service component. Three lines were drawn first and
kept: server code lives outside core; `commit.message` stays content (source
commit titles from the GitHub integration instead); inferences that leave a
device carry their provenance.

| Item | State | Evidence |
|---|---|---|
| SQL read-only at the engine layer | done | `engine_is_read_only_at_the_engine_layer` |
| `attrs` contract enforced by the engine (RFC 0006 §4.3) | done | `attemptdb-core::attrs` — allowlist ∪ `x_<provider>_*`, ≤256 chars, single-line, no email, no home path; applied in `Database::ingest`; `IngestReport.redactions`; adapters re-export the same list |
| `attemptdb-server` crate | done | `POST /v1/sync` (RFC 0006 §10.3, events as RFC 0001 envelopes), bearer keys as SHA-256 digests, one `.attemptdb` per tenant under `data/tenants/<tenant>/`, LRU of open databases that never evicts an in-flight handle, capture-mode ceiling (`metadata_only` strips `content`/`raw` before the WAL), idle sweeper, flush on shutdown; `attemptdb-server digest <key>` for operators |
| Server e2e tests | done, 4 + 6 unit | raw HTTP over TCP: 401/403/400/413, idempotent re-send (`duplicates`), ceiling strips content on disk, forbidden attr dropped with `redactions = 1`, `device_seq` preserved, mixed-device batch partially rejected, two tenants isolated with `max_open = 1`, WAL drained by the shutdown flush |
| RFC 0006 §10 | updated | records the two deviations from the sketch (envelopes, ceiling) and the implemented status codes |
| Test-only attrs | moved | crash/repair harness keys now `x_test_*`, the extension namespace, instead of privileged names |

Not built, on purpose or not yet:

- **Client uploader** (`attempt daemon --sync <url>`): cursor, one batch in
  flight per device, retry on 503/network, drop on 4xx, split on 413. Needs
  an HTTP client dependency (rustls; no OpenSSL) — a single-binary decision
  to make deliberately.
- **Legacy endpoint** `POST /v1/vibemon/hook` accepting envelope v2 through a
  VibeMon adapter, so existing installs work before any client upgrade.
- **Envelope v3** (client event id, millisecond timestamp, per-session seq):
  a `vibemon-hooks` contract change; the agent cannot edit that repository.
- **Incremental projection + segment cache**: the largest remaining item and
  the one that matters most for serving; independent of everything above.
- **Deployment**: VM with a persistent disk (not Cloud Run — flock/fsync),
  TLS in front, `.atdb` snapshots to object storage, key issuance in
  vibemon-web, key-file reload, rate limiting, dual-write and cutover.
- **Read side**: projection sync to Supabase with `inference_id`,
  `algorithm_version`, `confidence` and evidence ids.

### Decision (2026-08-30): `vibemon-hooks` and the collector

Proposal considered: evolve `vibemon-hooks` into `attemptdb-collectors`,
move from "privacy by discard" to "full fidelity local + selective sync",
put an `attemptdb.event/v1` protocol above envelope v2, make local install
account-free, migrate in phases.

Judgement: the direction is right; the premise is not. The provider-neutral
collector already exists in this repository as `attempt hook` +
`attemptdb-adapters` (4 adapters, 126 fixtures, canary tests, structural
installer, spool that never blocks the agent, UUIDv7 ids, local-first
`local_semantic` default, encrypted content). What the proposal describes as
the target state is largely the current state of `attempt`; what broke
VibeMon collection in production (v28's shell/python env-prefix bug) is a
property of the Python/bash client the proposal would build on.

So the migration is **asset transfer, not client evolution**:

| From `vibemon-hooks` | Into `attemptdb` | Why |
|---|---|---|
| `classify.py` taxonomy (32 dotted categories) | `classify_command` (8 coarse + git subcommand) | real coverage gap; port rules and their tests, keep coarse as prefix |
| `installer/windows` (signed EXE) | `install.ps1` only | Windows path AttemptDB lacks |
| self-update (`install.sh?v` polling) | none | TODO "rollback-safe auto-update" |
| `vibemon.dev/install.sh` URL + installed base | new installers | Phase 3 flips it to install `attempt` |
| `contract/golden` (19, real payloads) | `fixtures/providers` | add as extra cases; both share the production-installer origin |

Not adopted:

- A new nested event protocol. RFC 0001's canonical `Event` already carries
  identity, temporal (`observed_at`/`hlc`/`source_seq`), relationships
  (`span_id`/`parent_span_id`, causal edges), provenance
  (`adapter_version`/`hook_version`/`capture_mode`) and extensions
  (`x_<provider>_*`, `attrs.provider`, `unknown`), with stable field ids.
  Restructuring it is a format break for a naming gain. Do the unticked TODO
  instead: publish the JSON Schema of what exists as `attemptdb.event/v1`.
- A separate `attemptdb-collectors` repository now. The collector is the
  `attempt` binary; core ← adapters ← capture ← attempt is one workspace and
  one release. The identity can be a crate and a README section today and a
  repository when a second consumer exists.
- Envelope v2 as a projection target. It survives as the **legacy input**
  format (`POST /v1/vibemon/hook`, v2 → Event on the server) for installs
  that have not upgraded; nothing needs to produce v2.
- "Derived inference" to the cloud without policy. A work title synthesised
  from a prompt is content by another route; it syncs under the user's policy
  with `inference_id`, `algorithm_version`, `confidence` and evidence ids.

Phases, mapped onto the waves already planned: (1) coexist — both hooks
installed, as on this machine since 2026-08-28; owner ships the v30 hotfix.
(2) `attempt sync connect vibemon` (Wave B uploader + key issuance) and the
legacy v2 endpoint; port the four assets above. (3) `vibemon.dev/install.sh`
installs `attempt`, runs `attempt hook install`, optionally connects.
(4) `vibemon-hooks` archived as a compatibility repository. Steps touching
the `vibemon-hooks` repository or `vibemon.dev` are the owner's.

### Wave 6 (2026-08-30) — the standard, declared; vibemon-hooks assets, ported

Decision carried out: AttemptDB is the collector; `vibemon-hooks` hands over
its assets and retires. Round 1 of Phase 2.

| Item | State | Evidence |
|---|---|---|
| `spec/event-v1.schema.json` + `spec/README.md` | done | JSON Schema of the canonical event as serialised today — field groups identity / temporal / relationships / provenance / extensions mapped onto existing fields, no restructuring |
| Drift tests | done | `attemptdb-core/tests/spec.rs`: a fully populated `Event` and every golden fixture validate against the schema; nine forbidden shapes are rejected; goldens also pass the conformance rules (`jsonschema` is a dev-dependency only) |
| `attemptdb-core::conformance` + `attempt conformance` | done | six sections, failures vs notes, `--json`, exit status; unit tests for a clean stream, one violation per section, and short streams that get notes not failures |
| Classifier taxonomy port | done | `CommandFacts.subcategory` — the 32 vibemon-hooks categories (`git.commit`, `pkg.test`, `infra.docker`, …) plus chain priority; emitted as `attrs.command_subcategory` next to the coarse `command_category` projections key on; 40-case table test |
| Home elision in path attrs | done, and a real finding | `attrs.cwd` / `previous_cwd` / `worktree_path` carried absolute home paths (`/Users/<name>/…`) in production — RFC 0006 §4.2 forbids exactly that. `elide_home` (deterministic, no environment) now produces `~/…`; 53 goldens regenerated, diff limited to those keys |

First real run: `attempt events --all-projects --json -n 400` piped into
`attempt conformance` reports every section clean except Extensions — 400
failures, all `attrs.cwd` with an absolute `/Users/<name>/…` path, written by
the adapters before today's fix. Facts are immutable, so those events keep
that value in their segments; new events carry `~/…`, and the sanitised
export already elides home paths. The tool found the defect it was built to
find, on its own project, on day one.

Round 2, same day — the loop is closed end to end:

| Item | State | Evidence |
|---|---|---|
| `attempt sync` client (`attemptdb-capture::sync`) | done | `connect` / `now` / `status` / `disconnect`; `sync.json` 0600; per-database cursor under `data/sync/`; batches in `source_seq` order, one in flight, cursor advanced on ack only; 413 → halve and retry; 5xx/transport → keep cursor, record error; 4xx → stop and say why; `metadata_only` clamp on the device by default, `--send-content` opt-in |
| Daemon integration | done | uploads on the configured interval when `sync.json` exists; read-only opens, never contends with the writer |
| Dependency | deliberate | `ureq` 2 with rustls (`tls` feature): no OpenSSL, still one static binary |
| Legacy envelope v2 | done | `attemptdb_adapters::vibemon::normalise_envelope` + `POST /v1/vibemon/hook`; seven sanitised fixtures; kinds, attrs, `commit.message` → content (gone under the ceiling), home-elided `cwd`; conformance-clean |
| Tests | done | capture e2e against an in-process server: order, cursor, idempotent re-run, strip-by-default, 401 / unreachable keep the cursor, oversized batch splits, `--send-content` still clamped by the server; adapters vibemon 3 + unit 3; server legacy route 1 |
| **This machine, end to end** | done | local server on 127.0.0.1:8797 → `attempt sync connect` → `attempt sync now`: **4,473 events, 5 batches, 1.2 s, cursor 4473**, second run "nothing to upload"; server tenant holds 4,473 events with `content_json` count 0; `attempt conformance` over the server copy: clean except one Identity finding that turned out to be a rule bug (below); 4,472 `redactions` reported — the pre-fix absolute `cwd` values, dropped by the engine at server ingest exactly as RFC 0006 §4.3 says |
| Conformance rule fix | done | `retraction`/`correction` carry the *target's* session id by design (RFC 0003 §8); the derivation rule no longer applies to them; test added; spec/README notes it |

One limitation recorded in the adapter: VibeMon strips the host from every
remote (`owner/repo`), so legacy events get a root-and-device project id
rather than the remote-derived one; the identifier is kept as the display
name and ids converge once an install moves to `attempt hook`.

Not in this round: Windows installer and rollback-safe self-update (assets to
port from `vibemon-hooks`), repository sync policy (§10.5), key issuance in
vibemon-web, deployment.

### Wave 7 (2026-08-30) — incremental projection, and what it flushed out

The serving blocker from the hosted-backend audit: every read rebuilt the
world and every write invalidated it.

| Item | State | Evidence |
|---|---|---|
| `attemptdb-storage::ScanCache` | done | decoded segments (events + Arrow batches) kept across opens, keyed by segment id; refresh decodes only what the manifest newly lists, drops what it no longer lists, replays the WAL; `Refreshed::scan` reproduces `Database::scan` semantics from the cache; `tests/cache.rs` |
| `attemptdb-project::IncrementalProjector` | done | per-session observation buffers, cached `SessionBuild`s, dirty tracking; corrections/retractions and an ordering-mode change invalidate everything (they are the only cross-session influences); `finish_at` split so both paths share `assemble`; four equivalence tests against `project()` — every prefix, reversed delivery, duplicates, corrections + retractions |
| `QueryEngine::from_parts` | done | engine from cached batches + a ready projection; no decode, no re-project |
| UI and MCP stores | done | `EngineCache` per store; unfiltered scope uses the incremental path, scoped views project the scoped events from the cache (exact, no decode); `cache_stats()`; UI test asserts decodes/refreshes/projector size across WAL append, flush, and a scoped view |
| `attemptdb-bench step refresh` | done | 200 k events: reload **0.5 s vs 5.6 s** from scratch (11×); decode 3.8 s → 0.002 s; numbers in `docs/benchmarks.md` |
| Two quadratic loops in the batch projector | fixed | `workunit::build` evidence and path dedupe via `Vec::contains`; 700 ms → 19 ms on 200 k; every earlier projection number was paying for it |
| Engine defect: multi-batch segments | fixed | "Dictionary replacement detected" on any flush > 4,096 rows whose dictionary columns vary across chunks; never hit before because no flush exceeded one batch; a server tenant at the default 20,000-row memtable would have failed its first big flush. Segments now share one dictionary per column across chunks; `tests/large_flush.rs` |

Next in this line: the remaining 0.45 s per reload is engine construction —
projection tables (292 ms) and readable event batches (124 ms) are rebuilt
from scratch; cache readable batches per segment, build projection tables
from typed column builders.

### Wave 8 (2026-08-30) — repository policy and secret scanning

| Item | State | Evidence |
|---|---|---|
| Repository sync policy (RFC 0006 §10.5) | done | `SyncConfig.include/exclude` (normalised remote or `prj_…`), `SyncConfig::allows`, `attempt sync policy exclude|include|remove|clear`, `connect --exclude/--include`; excluded events never upload and the cursor still passes them |
| Secret scanning (RFC 0006 §5) | done | `attemptdb-core::secrets` ruleset `secrets-v1` (issuer prefixes, PEM blocks, JWTs; no regex dependency); attrs values with a secret are dropped at ingest (`value_allowed`), `Event::redact_secrets` redacts content spans; the sync client applies it before `--send-content` upload and reports `secrets_redacted` |
| Tests | done | core: formats detected / prose not / PEM whole / redaction keeps context; capture e2e: excluded repo never reaches the server, three tokens redacted on the wire copy, local copy untouched, cursor covers excluded events |

Deliberately not in v1: generic `password=`/`token=` heuristics (recall at
the price of precision), a value-level scan of `paths`, and scanning of
transcript imports beyond what ingest already applies.

### Wave 9 (2026-08-30) — key issuance and reload

| Item | State | Evidence |
|---|---|---|
| `/v1/admin/keys` (issue · list · revoke · reload) | done | admin bearer token gates it; no token → 404; keys minted server-side (`atk_` + 32 random bytes), returned once, digest-only on disk, atomic rewrite + immediate reload; device id minted unless supplied |
| SIGHUP reload | done (unix) | operator edits the file by hand, sends SIGHUP; same code path as `POST /reload` |
| Tests | done | absent surface, wrong token, issue → upload works without restart, file holds digest not key, list never shows keys, revoke → 401, hand-edit + reload → works |

What vibemon-web needs to call: `POST /v1/admin/keys {tenant: <user_id>,
label}` on "link this device", hand the returned key to the installer, and
`DELETE` on unlink.

### Wave 10 (2026-08-30) — legacy hook migration

| Item | State | Evidence |
|---|---|---|
| `attempt hook install --remove-legacy vibemon` | done | recognises `~/.vibemon/notify.sh` commands (any home path) and Gemini `vibemon-*` names; other hooks in the same group untouched; emptied events dropped, events we install refilled in place; `~/.vibemon` never touched; same flag on `uninstall`; `legacy_removed` per agent in `--json` |
| Dry run on this machine | done | claude-code 9 · codex 6 · cursor 7 · gemini-cli 6 legacy entries found, ours re-inserted in place, nothing written |
| `docs/migration/vibemon-hooks.md` + `vibemon-install.sh` | done | draft of the next `vibemon.dev/install.sh`: install → `init` → `hook install --remove-legacy vibemon` → `sync connect <server> --key` → `sync now`; `--purge-legacy` deletes `~/.vibemon` only when no agent config still references it; `--dry-run` prints the plan |

Owner: the live run on this machine (`attempt hook install --remove-legacy
vibemon`) is deliberately not done here — it stops the legacy client's
uploads to the hosted service on this device; do it when the server side is
ready.

### Wave 11 (2026-08-30) — rollback-safe `attempt update`

| Item | State | Evidence |
|---|---|---|
| `attemptdb_capture::update` | done | resolve (latest or `--to`) → download asset + `SHA256SUMS` next to the binary → digest must match → `tar` extract → stage `attempt.new` → health check → swap (old kept as `attempt.prev`) → health check again → restore on failure; `--rollback`; package-managed paths (Homebrew/cargo/Scoop/Nix) refused with the manager's command; target triple from `build.rs` |
| Health check | done | `--version` must print, and when a database exists `status --json` must succeed — the "runs but cannot read our files" case is what triggers the rollback |
| Daemon restart | done | `service::restart_service` (`launchctl kickstart -k` / `systemctl --user restart`) when installed, else stop + respawn `daemon run` in its own process group; `--no-restart` |
| Tests | done | 7 unit (parsing, semver incl. pre-releases, managed paths, slots, swap/rollback matrix) + 4 e2e against a local fake release server (happy path + rollback of the *replacing* binary, checksum mismatch and missing sums leave everything untouched, staged failure, pinned/up-to-date and Homebrew refusal) |
| Live | blocked on owner | `attempt update --check` → "no release found … (is the repository public and a release published?)" until the first public tag |

### Wave 12 (2026-08-30) — inference sync with provenance

| Item | State | Evidence |
|---|---|---|
| `spec/inference-v1.schema.json` | done | wire form of one upload: kind ∈ {attempt, handoff, work_unit, decision}, every item with evidence (≥1), confidence ∈ [0,1], algorithm_version, fields; drift test validates the uploader's own body against it |
| Client (`attempt sync connect --send-inferences`, default off) | done | projector supplied by the binary (`attempt::inferences`, so `attemptdb-capture` stays free of inference code); computed over policy-allowed events after the fact upload; no-evidence/unknown-kind items dropped; `objective`/`rationale` removed unless `--send-content` (then secret-redacted); sorted + digested, unchanged sets not re-sent; 20k/kind cap reported as `truncated`; daemon uploader uses the same path |
| Server (`POST /v1/sync/inferences`, `GET /v1/inferences`) | done | provenance validated per item (rejected by id with reason), capture-mode ceiling strips content fields, one document per (device, kind) replaced wholesale under `<tenant>/inferences/`, never ingested as events |
| Tests | done | capture unit (policy filter, redaction, digest stability, describe) + e2e against the in-process server (facts first, one stored item with 3 evidence ids and a null objective, summary, 404 for absent kind, unchanged second run, off by default) + server e2e (stored/rejected/stripped counts, wholesale replace, 403/400 paths, tenant isolation, no event DB created) |

### Wave 13 (2026-08-30) — the unified collector, TODO §21

The audit against the "one collector, two user experiences" structure
(TODO §21) found the local half ~90 % built, the cloud half write-only, and
the product half untouched. This wave closed what can be closed from this
repository; four agents worked in git worktrees on independent crates and
were merged back one by one. Every number below was measured here.

| Item | State | Evidence |
|---|---|---|
| Server key scopes + user binding | done | `Scope {device, reader, admin}` and `user_id` on `KeyEntry`/`Principal`; files without a scope stay device keys; upload routes 403 non-device keys; `Scope::parse`/`validate_user_id`; e2e `reader_keys_read_but_never_write` |
| `DELETE /v1/admin/devices/{id}[?tenant=]` | done | revokes the device keys, writes one Retraction per session (`RetractionReason::Revoked`, server-authored, no content); facts stay, projections drop the sessions; repeat calls report `sessions_already_retracted`; e2e `deleting_a_device_revokes_its_keys_and_retracts_its_sessions` |
| Sync peers + profiles + daemon config reload + `vibemon` alias (agent) | done | `sync.json` → `{peers}` (old shape read as `default`), per-peer cursor files, `metadata_only`/`semantic`/`full` mapped onto the stored flags, `connect|add|list|remove|now|status|disconnect|policy --peer`; the daemon re-reads the config every tick so `connect` needs no restart; e2e `two_peers_with_different_profiles_keep_independent_cursors` |
| Cursor bound to its server | done | `SyncState.url`; a peer pointed at another server restarts from 0 and the server deduplicates; `connect`/`add` say so; unit `a_cursor_follows_its_server_not_its_peer_name` |
| Server read API (agent) | done | `EngineCache` moved from the UI into `attemptdb-query` (UI, MCP, server share it); per-tenant view keyed on manifest generation/segments/memtable; `GET /v1/sessions|timeline|work|attention|state|events|status`, `POST /v1/query` (engine-level read-only, 5 s timeout); merge rule: device inference wins only with the same family and `n ≥ server`; every inference object carries `computed_by`; parity test prints `events=23 sessions=3 turns=3 attempts=4 handoffs=2 work_units=1 decisions=1 (server == local projection)`; `docs/server-api.md` |
| `GET /v1/devices` | done | key bindings + `connected` + counts + `last_sync_at` (server receipt time): the "Connected · last sync N s ago" row |
| Backfill importer (agent) | done | `attempt import vibemon-export <file>`: NDJSON or JSON array of `hook_events` rows (schema read from `vibemon-app/supabase/migrations` — the Edge Function stored columns, not the envelope), rebuilt into envelope v2 for the existing adapter; `EventId::derive(["vibemon-export", row.id])` so re-runs store nothing; cwd recovered from sibling rows, never invented; rejected rows counted by reason; two sanitised fixtures; conformance-clean |
| On-disk compatibility fixture | done | `fixtures/db/format-v2` (directory with an unflushed WAL tail) + `.snapshot` + `expected.json`; `storage/tests/compat.rs` reads, continues, restores, and refuses `format_version: 99` with `unsupported format version 99`; `docs/storage-format.md` §14 |
| Commit linkage (TODO §21.6, §11) | done | `Projection.commits`: a successful `git commit` call tied to the `HEAD` the hook recorded on its own end event (0.9), on the next head-bearing event whose previous head matches (0.7), or left unresolved (0.4); `Attempt.commit_shas`, `WorkUnit.commit_shas`, `CommitId` (`cmt_`); `commits` query table, `SHOW COMMITS`, JSON APIs and `attempt timeline` show the shas; no command output is read |
| `attempt-hook` binary | done | `crates/attempt-hook` links only the capture crate: **0.8 MB vs 76.5 MB**; `attempt hook install` prefers it when it sits next to `attempt`, `doctor` recognises `attempt-hook <provider>`, `attempt update` installs and rolls back the pair; release workflow, Homebrew formula, `install.sh`/`install.ps1` ship both. Measured (40 runs incl. spawn, macOS ARM64): wall p50 6.6 → 4.2 ms, p95 7.1 → 4.6 ms — the remainder is process spawn. This machine's hooks now reference it |
| Group-commit decision (§21.4) | closed | live data: `attrs.hook_us` over 1,544 events p50 258 µs / p95 415 µs / p99 778 µs; the gate is met under strict durability, which stays the default |
| Deployment files | written, not run | `deploy/Dockerfile` (Alpine/musl static, both binaries), `entrypoint.sh`, `docker-compose.yml` with Caddy, `docs/deploy.md` (VM + one volume, keys, backups by volume snapshot, backfill, upgrade). Docker was not running on this machine |
| Migration installers | updated | `vibemon-install.sh`: `init --capture-mode metadata_only` (existing VibeMon users keep their promise on disk; `--local-content` is the consent step), `--profile semantic`, `connect vibemon`, `daemon install`, ends with `doctor`; `vibemon-install.ps1` drafted with a Scheduled Task standing in for the Windows daemon; `docs/migration/vibemon-hooks.md` matches |
| OTel intake | decision pending | `docs/adr/0003-otel-intake.md` proposes OTLP/HTTP JSON on the daemon, loopback only, mapped into canonical events under the capture mode; no code until the owner decides (TODO §21.5) |
| RFC 0006 | updated | §10.8 peers and profiles, §10.9 read side, §10.10 key scopes and device removal |
| Segment compaction (agent) | done | `Database::compaction_plan` / `compact`: contiguous runs of small segments (below 8 MiB, ≥ 4 in a run, only while more than 32 segments are listed) rewritten through the flush writer into one segment, one durable manifest generation per step, inputs tombstoned and removed after the *next* generation; failpoints `compact.after_segment_write` / `after_manifest_write` / `before_delete_inputs` under real SIGABRT; `attempt compact [--dry-run]`; bench: 100 k events in 200 segments → open p50 974 → 342 µs (2.85×), scan +4.7 %, compaction 3.9 s; wired into the daemon (≤ 4 steps after each periodic flush) and the server (≤ 4 steps when a tenant is flushed and closed, `--no-compaction` opts out); storage 109 tests |

Self-hosting: `attempt` and `attempt-hook` reinstalled from the final tree;
the daemon restarted on it (its first start hit the writer lock held by a
CLI command and launchd's retry took over — expected); hooks rewired to
`attempt-hook`; the bootstrap session's events kept flowing throughout.
`doctor` first reported the new wiring as `stale` because it expected
`attempt` itself — fixed (it now expects the preferred hook binary). Owner:
Codex needs the changed `SessionStart` command re-approved in `/hooks`.
Final workspace run: 518 tests, 0 failures, clippy and fmt clean.

Not done, and why: `vibemon.dev/install.sh` still serves `vibemon-hooks`,
the `/hook` forwarder, the web "connect a device" flow, and every screen
migration are in the vibemon repositories (§21.8); the first tag, the
public repository, the Homebrew tap and code signing are the owner's; the
Windows daemon is unimplemented and untestable here; `--remove-legacy
vibemon` stays a dry run on this machine until the hosted server exists.

### Wave 14 (2026-08-31) — discoverability: the repository as a front door

The code was ready to be found and the repository was not. An audit of the
GitHub surface (metadata, community files, package names, launch channels)
found the exposure machinery almost entirely empty, and one hard blocker.

| Item | State | Evidence |
|---|---|---|
| **crates.io name** | fixed | `attempt` on crates.io is an unrelated crate, so `cargo install attempt` would have installed the wrong software. The CLI package is now `attemptdb`; the binary stays `attempt`. Every internal path dependency carries a version, so the workspace is actually publishable — verified with `cargo package --list` |
| crates.io search surface | done | `homepage`, `keywords` (5) and `categories` on every crate, inherited from `[workspace.package]`; `crates/attempt/README.md` is the crate landing page; `attemptdb-bench` stays `publish = false` |
| Repository topics | done | 20 topics (`rust`, `ai-agents`, `claude-code`, `mcp`, `local-first`, `datafusion`, …). The repository had **none**, which is the main way GitHub search and topic pages surface a project |
| Social preview | done | `docs/media/social-preview.png`, 1280×640, generated with layout assertions so nothing collides. Link unfurls previously showed the owner's avatar |
| Discussions | enabled | plus `.github/ISSUE_TEMPLATE/config.yml` routing questions there and vulnerabilities to private reporting |
| Contributor issues | done | ten issues opened from real gaps, four labelled `good first issue` (shell completions, man page, adapter fixtures, architecture diagrams). The `good first issue` label existed with nothing to attach it to |
| **CONTRIBUTING was false** | fixed | it told newcomers the storage, adapter, capture, query and projection crates were "stubs while the RFCs are finalised" — they have been implemented and tested for weeks. Intro and crate table now describe what exists |
| **CODE_OF_CONDUCT was a dead end** | fixed | it directed reports to `conduct@attemptdb.dev`, a domain nobody owns. Reports go through GitHub private reporting, the only private channel this project operates |
| README install | fixed | it now separates what works today (from source) from what arrives with the first tag, instead of promising a crates.io install that does not exist yet |
| CHANGELOG, CITATION.cff, dependabot, CODEOWNERS | done | |

Left for the owner, in order: make the repository public (which also clears
the Actions billing block), cut the first tag, publish the crates deepest
dependency first, then the targeted channels — the awesome-claude-code list,
the official MCP registry, Show HN, Lobsters, r/rust, This Week in Rust.
GitHub Trending is not a channel to aim at; it is what happens when several
of those land on the same day.

### Pre-public checklist

- [x] `CODE_OF_CONDUCT.md` no longer names an address nobody owns; conduct
      reports go through GitHub private reporting.
- [x] `CONTRIBUTING.md` describes the implemented workspace, not stubs.
- [x] Repository topics, social preview, Discussions, contributor issues.
- [x] Package names resolved (`attemptdb` on crates.io; `attempt` was taken).
- [x] Home paths scrubbed: `paths.rs` / `attrs.rs` test cases carry an
      anonymous home, `docs/benchmarks*` carry home-elided paths.
- [x] History decision: **kept, not squashed.** Seventy-three commits of real
      engineering are the most credible thing this repository has, and the
      hashes the log cites (`5d8c033`, `391e69a`, `4bbeede`) still resolve.
      One document was purged instead of the whole history —
      `docs/migration/cutover-plan.md`, an operational runbook for a live
      service that named another repository's files and described a daily
      `curl | bash` self-update path as an attack surface. It had never been
      pushed; it now lives in the vibemon workspace. The VibeMon outage
      post-mortem stays: it is the owner's own service, the bug is understood
      and fixed locally, and honest failure reporting is what this project is
      about.
- [x] The 1.45 M-event figure in the README is the **synthetic** benchmark
      workload (`attemptdb-bench`), labelled as such in `docs/benchmarks.md`.
      No production aggregate is claimed anywhere public. The launch draft did
      conflate the two and has been corrected; it also moved out of the
      repository (`.launch/`, gitignored) — a `SHOW_HN_DRAFT.md` in the root
      of a public repository is the wrong first impression.
- [x] Secret scan over every tracked file: the only hits are the detector's own
      patterns (`attemptdb-core::secrets`) and the canary fixtures. No
      credential, token or private address is tracked anywhere.
- [x] `CHANGELOG.md` promoted to `[0.1.0] — 2026-08-31` with compare links.
- [x] crates.io names checked against the registry: all eleven are free.
      `attempt` is taken by an unrelated crate, which is why the CLI package is
      `attemptdb`. `attemptdb-core` dry-run publishes cleanly (17 files,
      37.4 KiB compressed); the rest cannot be rehearsed, because a crate only
      packages once its dependencies are already on the registry.
- [ ] Domains: `attemptdb.dev` / `attemptdb.com` unregistered. `nullarch.dev`
      is owned and could host the docs instead of a new purchase.
- [ ] Run `attempt snapshot audit` on anything shipped as a demo dataset.
- [ ] First tag + green CI matrix on all five core targets.

### What is blocked, and on what

One step remains: crates.io, which stays with the owner. Everything else on
this list is done, and the release took three tags to get right — the record of
why is in the rows below and in `CHANGELOG.md`.

| Step | State |
|---|---|
| ~~Repository public~~ | **done 2026-08-31 (attemptdb-d7).** Reachable from the CLI after all: `gh repo edit --visibility public --accept-visibility-change-consequences` |
| ~~Social preview~~ | **done.** `docs/media/social-preview.png` uploaded through Settings → General; the section only exists once the repository is public, and there is no REST API for it. Verified by fetching the live `og:image` and comparing bytes — identical to the file in the repository |
| ~~Profile~~ | **done.** `nullarch/nullarch` created and pushed; six repositories pinned, `attemptdb` second, top row |
| ~~First tag~~ | **v0.1.0 shipped 2026-08-31**, tagged at `302b963` — the tree CI actually verified, not the tip. Held through three red matrices until run 33352579921 came back green on all seven jobs; the release run built all eight targets, published `SHA256SUMS`, and the homebrew job took its "tap token not configured" branch as expected. Verified by installing from the published release into a scratch directory: `attempt 0.1.0`, `attempt-hook 0.1.0` |
| ~~Released~~ | **v0.1.2 is current.** v0.1.0 shipped, then two Linux defects in `attempt update` forced v0.1.1, which I tagged without bumping the workspace version — its binaries report `0.1.0`, so `attempt update` offered it forever to anyone who installed it. v0.1.1 is marked prerelease (not deleted) so `releases/latest` skipped it, and v0.1.2 carries the same fixes with the version it claims. `release.yml` now has a `version-guard` job that compares the tag against the workspace version before the eight builds run; it passed its first real tag. End-to-end verified after each release by installing from the published artifact: `attempt 0.1.2`, `attempt-hook 0.1.2`, `update --check` → "up to date" |
| crates.io | order computed from `cargo metadata`: core → adapters → project → storage → query → server → capture → attempt-hook → mcp → ui → attemptdb. Needs a token, then eleven sequential publishes — the chain cannot be rehearsed, because a crate only packages once its dependencies are on the registry. Verified unpublished the authoritative way (sparse index `index.crates.io/at/te/attemptdb` → 404; the API answers 403 without a User-Agent, which is not an answer). Nothing in the repository advertises `cargo install attemptdb` as working today; the one sentence to flip afterwards is the README's future-tense line |

The profile was the weakest link in the launch and is now the part with the
most headroom left. Of 74 public repositories, **55 are forks** and 19 are the
owner's own work; there was no bio, no profile README, and three pins, none of
them AttemptDB. The README and the pins are fixed. The bio is still empty, and
that one is the owner's to write — it is a sentence about himself, not a
sentence about the software.

One thing the profile work turned up: `nullarch/kakaocli` is a fork of
`silver-flight-group/kakaocli`, which is not one of the owner's organisations.
The first draft of the profile README listed it among his own projects. It is
out of both the README and the pins. Anything that generates a "my projects"
list from the repository list has to check `fork`; among the plausible
candidates only kakaocli fails that check.

### A Linux-only bug the release shipped with (2026-08-31)

The first CI run after the installer hardening went red on `test
(linux-x86_64)` only, and it was not the commit's fault — that commit touched
no Rust. `update_downloads_verifies_swaps_and_keeps_the_previous_binary` failed
with `Os { code: 26, kind: ExecutableFileBusy }` executing the binary it had
just staged.

Linux refuses to `execve` a file any process still holds open for writing.
`fs::copy` closes both ends before returning, so the update path does not hold
it — but spawning a process forks, and a child forked by one thread inherits
every descriptor open at that instant, including a write handle another thread
is about to close. That inherited handle keeps the file "being written" until
the child execs. A health check landing in that window fails with "Text file
busy". macOS does not enforce this, which is why the path looked clean on
every darwin run, including two manual ones against the published release.

It is a race, so it is intermittent, and it is not only a test artifact: the
production health check in `cmd_update.rs` spawns the freshly written binary
the same way. `attempt update` on Linux could fail with "Text file busy" and
leave the user with no idea what to do — a shipped bug in v0.1.0, found by CI
two hours after the release.

`update::spawn_executable` now retries on `ETXTBSY` for up to two seconds,
and both the CLI's health check and the test helper go through it. The window
is milliseconds and closes on its own; failing an update over it is the wrong
answer, because the binary is fine.

Worth noting how it was found: not by review, and not by the two independent
manual installs on macOS, but by the only Linux execution any of this has ever
had — a CI job on a commit that could not have caused it.

An audit for the same shape then found a second site, one function below the
first: `restart_daemon` respawns `attempt daemon run` from the binary the swap
has just written, and it used a raw `spawn()` whose failure was discarded
(`if cmd.spawn().is_ok()`). That one is worse than the health check. It is on
the fallback branch — no launchd or systemd unit, so nothing else restarts the
daemon — and on `ETXTBSY` the update would report **success** while the user's
capture daemon stayed stopped. Silent loss of collection is precisely what the
hook-path invariant exists to prevent. It now goes through `spawn_executable`
too, and the failure is carried in `DaemonNote.error` and printed, instead of
the user being told only that the daemon "did not come back".

Every other process-spawn site in shipped code was checked against "can this
exec a file this process just wrote", and the rest are clean: the UI's
`open`/`xdg-open`, `tar` and `xattr`, `launchctl`/`systemctl`, and the agent
version probes, which run third-party binaries the installer never writes. The
only `fs::copy` onto an executable is the update staging itself. That is a
static audit, so an exec reached through a dependency would not show up in it.

### Decisions taken 2026-08-31 (TODO §21.1c)

The three the server could not proceed without. Keys bind to a tenant string
and a tenant cannot be renamed without stopping the server, so these had to be
settled before any real key is issued.

| Decision | Taken | Why |
|---|---|---|
| Tenant granularity | **organisation**; solo users get a personal org | One tenant is one database, and `/v1/work`, `/v1/sessions` and `/v1/attention` are tenant-scoped. The organisation work graph — the team view the product sells — would otherwise have to cross a tenant boundary the server deliberately does not cross. The mapping lives in the server as `--tenant-rule`, never baked into the Edge Function, so it can change without redeploying VibeMon. |
| Default sync profile | **`semantic`** | Metadata plus this device's inferences with evidence ids, confidence and algorithm version; `objective`/`rationale` text is stripped before upload, so nothing content-bearing leaves the machine. That keeps the metadata-only promise made to existing VibeMon users while letting `/v1/attention` cite *why* something is blocked instead of only counting. The server's capture-mode ceiling stays `metadata_only`; `full` stays an explicit opt-in. |
| Realtime path | **`/v1/sessions` polling at 5 s first**, presence channel later | Polling reuses the read API that already exists and is already parity-tested against the local projection, and the daemon's `semantic` upload interval is also 5 s, so end-to-end lag stays inside ten seconds. A presence channel needs websocket infrastructure on a one-VM deployment with no horizontal story yet; it is an optimisation, not a cutover blocker. |

Consequences: `POST /v1/admin/keys` issues against an org tenant from the
start; `attempt sync connect vibemon` defaults to `semantic`; 21.4b's daemon
interval is settled at 5 s; and `useCodingState` (21.8b) targets polling.

## Session log

### 2026-09-05 (later) — clients that update themselves

Nothing moved an installed `attempt` forward: `attempt update` existed, and
only a person ran it. The legacy client's daily poll was the only update
channel the product had, and migrating off it removed it — so the users who
had moved were the ones who could never be fixed. The owner's steer was
"pull from GitHub periodically, with required and optional updates", and
that is what shipped:

- **The policy is the release's.** `RELEASE.toml` (two scalars) becomes
  `update.json` beside the assets; `required_below` is the floor under which
  a client updates at once. Clients read it from the `releases/latest`
  redirect — a plain download — so a fleet behind one address never meets
  the API's per-address limit. Older releases without the file resolve
  through the API as before.
- **The client decides, then acts.** `update::decide` → `Decision`,
  `CheckState` in the cache dir (doctor reads it, no request), `auto_tick`
  (fetch ≤ once a day; required → now; optional → at a quiet moment when
  `auto_update = on`; nothing when nothing here can restart the daemon),
  `health_check_for` (the same check `attempt update` runs). The daemon runs
  a tick ten minutes after start and hourly; a swap ends the daemon with an
  error so launchd/systemd bring it back on the new binary. `attempt
  maintenance` does upload + tick as one command and is what the Windows
  task now runs every minute.
- Tested: decision matrix, policy parsing, state round-trip, the tick's
  mode/environment/moment gates without a request; then for real —
  `attempt update --check` against GitHub (0.2.7 has no policy yet → API
  fallback → up to date), `attempt maintenance` and `attempt doctor` on the
  portable demo database, and the environment switch.


### 2026-09-05 — making the migration's failures visible

The question was how to move the ~107 users still on the legacy
vibemon-hooks client to AttemptDB. The mechanism has existed since 0.2.2:
the legacy client polls `vibemon.dev/install.sh?v` daily and, when the
string changes, runs the install URL unattended — and that URL already
serves the AttemptDB migration installer. What did not exist was any way to
know whether an unattended install *worked*. Three things were found while
checking, all visible in data nobody was reading:

- **@elo_tt had paired 13 devices in two days, all labelled `cursor`, all
  with zero events** — a throwaway environment (Cursor's cloud sandbox has
  that hostname) running the install command on every session. Their real
  machine is still on the legacy hooks and fine.
- **The reconcile cron had failed every hour since 2026-09-03**:
  `20260903170000` used `current_setting('app.settings.supabase_url')`, a
  GUC this project never had (`cron.job_run_details` had the error on every
  run). The webhook kept up, so nothing was lost; the catch-up path was
  simply never exercised on schedule.
- **/setup's install command was broken for anyone who unticked "collect
  commit messages"**: it appended `--no-commit-msg`, which the AttemptDB
  installer refused with exit 2.

What was built (vibemon-app, vibemon-web, this repo):

- `api/cron/attemptdb-watch`, hourly: asks the sync server for every tenant
  with devices and says in Discord what crossed a threshold *in that hour*
  (paired 2–3 h ago with no events; had events, last one 24–25 h ago; ≥3
  zero-event devices with the newest under an hour old), plus install
  failures reported in the last hour, plus — once a day — migrated users
  whose legacy hooks are still writing rows. Stateless, so each finding is
  said once. The reconcile cron is rescheduled with the Vault pattern that
  works and the ops secret it expects (`cron_ops_secret` added to the Vault).
- `attemptdb_install_reports` + `POST /api/attemptdb/install-report`: the
  installers send one line on exit (see the 0.2.6 changelog).
- `src/lib/install-pins.ts` (web): one pin for every install route, and the
  rollout canary — `ATTEMPTDB_ROLLOUT_PERCENT` decides which share of
  pollers sees the next legacy version. **It is 0. The lever has not been
  pulled**; the plan is a real Windows verification and a voluntary wave
  first, then a small percentage watched in the console, then more.
- **The 0.2.6 script was then run for real**: unattended (no TTY), a
  sandbox HOME holding the older client's stored key, against the production
  web and sync server, with the real 0.2.6 binary behind a shim that skipped
  only launchd registration (its label would have replaced this machine's
  daemon). Every step went through — key exchanged, paired, hooks installed,
  three capture-test events uploaded, the legacy hook entry removed — and the
  script still exited 1: `attempt doctor` returned 1 for an untrusted Codex
  hook and `set -e` took that as the verdict, so the report said `failed at
  done`. Fixed in 0.2.7 (`|| true`). The sandbox device was removed from the
  tenant afterwards; this machine's database, settings and daemon were
  checked untouched. One quirk noted: `attempt doctor` in the sandbox said
  "daemon running" because the socket path is per user, not per HOME —
  harmless for real machines, which have one HOME.
- `/install.ps1` serves the AttemptDB installer (it was the last route on
  the legacy client; with `?v` it would have looped Windows machines through
  a legacy reinstall daily), and the ps1 installer gained the stored-key
  path it lacked.


### 2026-09-04 (later) — the mark, and the Windows upload that had stopped

*The icon work is below; it turned up something worse on the way.*

**Windows uploads stop a minute after the install.** The console showed
@bala's tenant at 31 events, last one `2026-09-03T12:26` — the install's own
`sync now`, and nothing since. Windows has no daemon, so the installer
registered a Scheduled Task running

    powershell -NoProfile -WindowStyle Hidden -Command "\"…attempt.exe\" import; \"…attempt.exe\" sync now"

and whether that works depends on how `-Command` strips the quotes: what
PowerShell ends up parsing is a quoted string followed by a bare word, which
is a parse error, unless the stripping leaves the path bare (and then it
breaks the moment a username has a space in it). Either way the task fails
silently every minute, forever, and the hooks keep spooling to a disk nobody
uploads.

The task now runs the executable directly with its arguments — no shell, so
no quoting to get wrong — and one command does the job because opening the
database imports the spool first. `attempt daemon install|uninstall` owns the
task now (`service.rs`, alongside launchd and systemd), the installer calls
the CLI instead of running `schtasks` itself, and `/Create /F` replaces the
broken task when a user re-runs the install. **Existing Windows installs stay
broken until they re-run it** — the Windows update path still serves the
legacy installer, which is the Windows-parity item on the plan.

While there: `attempt uninstall` removed hooks and left the background
registration behind — on every platform. It unregisters it now, which on
Windows means it stops a task pointing at a binary the user just deleted.

### The mark

Windows installs `attempt.exe` and `attempt-hook.exe` and that was all there
was to see: no icon resource, so Explorer drew the generic console glyph, and
no version information, so the properties dialog and SmartScreen had nothing
to name. Both binaries now carry an icon and a `VERSIONINFO` block
(`crates/attempt/build.rs`, `crates/attempt-hook/build.rs`, `winresource`,
host-gated so nothing changes off Windows; a missing resource compiler is a
warning, never a failed build).

- **The mark is the record it keeps**: a session marker, the stem running
  down from it, two attempts branching off — one short and muted because it
  stopped, one that landed — and the stem carrying on past the last branch,
  because the log is append-only. Violet is the accent the console and the
  Event v1 badge already use.
- **`assets/icon/render.py` is the master.** It draws at 8× and resamples, so
  the antialiasing is free, and it emits a *different, simpler drawing* below
  48 px (one branch, heavier strokes) — a shrunk full drawing is mush at a
  taskbar's size. It writes the `.ico`, the PNGs, the SVG, and the base64 the
  single-file export inlines, so none of them can drift.
- **The `.ico` is hand-assembled.** Pillow writes PNG frames at every size;
  the Windows shell reads PNG reliably only at 256, so frames below it go out
  as bottom-up 32-bit DIBs with the AND mask, and 256 stays PNG.
- The console, the local UI and the static export now show the mark in the
  tab (`/favicon.svg`, public — the sign-in page needs it before anyone is
  signed in). The export's self-containment test used to forbid every
  `<link>`; it now forbids every *fetching* link and requires the icon to
  carry its own bytes, which is what it always meant.


### 2026-09-04 — the console, dressed as the instrument it is (and a redaction bug it flushed out)

The console shipped in `0.2.3` worked but looked like a test page: browser
defaults, one blue accent, boxed cards, prose-sized rows. It is now designed
against the product it reads — the CLI's ledger, in a browser.

- **`crates/attemptdb-server/assets/admin.html`.** A token set that covers
  three viewer states (system, and an explicit light or dark the operator
  picks with the `◐` control, kept in `localStorage`): slate neutrals, the
  spec's blueviolet as the single accent, semantic colours (ok / warn / fail /
  live) kept separate from it. Everything countable is monospace with
  `tabular-nums`; every label is a 10 px uppercase micro-label. Chrome: a
  46 px bar with a segmented nav and a health pill whose dot carries the
  status, a sticky facts strip, a 286 px tenant rail with a filter (`/`
  focuses it, `Esc` clears) that sorts resident tenants first and shows a
  live dot, devices, last-seen and size per row. Readouts are one hairline
  grid, not seven boxes; tables are hairline rows with hover, dimmed zeros
  and right-aligned numbers (query results too — a column of numbers is
  detected and aligned). `alert()`/`confirm()` are gone: destructive actions
  open a `<dialog>` that says what the action does, and results come back as
  toasts. Ids carry a copy control on hover. Empty states say what would fill
  them.
- **Truth in the readout.** The webhook cell used to compute a lag against a
  cursor of 0 and report "7,911 behind" on a server with no webhook
  configured. It now reads `off — no forwarding configured` unless
  `/v1/health` says a webhook exists.
- **The login page** (in `admin_ui.rs`) matches: same tokens, the `▌` mark,
  the operator token in a labelled field, and one line on where the token
  lives and how long the session lasts.
- **Verified against real data, not fixtures.** A sanitized snapshot of this
  repository's own history (7,911 events, 19 sessions) was restored into a
  portable database and synced to a local server as tenant `org_demo`; every
  tab was walked in Chrome in both themes.
- **The same panic was live, and it was bricking a tenant.** `fly logs` had
  `panicked at crates/attemptdb-core/src/secrets.rs:105` on the deployed
  server, on the same character (`'브'`): the server redacts on ingest, the
  panic happened while the tenant's lock was held, and the poisoned mutex made
  every later request for that tenant answer *"cannot load the tenant: tenant
  database poisoned"* — which is exactly what the owner hit in the console.
  Three things were wrong and all three are fixed: the panic (below), the
  registry handing out poisoned tenants (`tenants.rs` now drops the handle and
  reopens the directory — the recovery a restart performs), and the fact that
  the machine had to be restarted by hand to clear it.
- **`deploy.yml` had never run.** It triggers on `release: published`, and a
  release created by a workflow's own `GITHUB_TOKEN` does not fire that event.
  0.2.3 built and published eleven assets and deployed nothing; the live
  server was still the source build. The Release workflow now dispatches the
  deploy (`workflow_dispatch` is exempt from the recursion rule).
- **`crates/attemptdb-core/src/secrets.rs`: a panic on any non-ASCII prose.**
  Producing that snapshot crashed: `snapshot export --sanitized` panicked with
  *"start byte index 1 is not a char boundary"*. The secret scanner walks
  **bytes** and sliced `text[i..]` at every one, so the first Korean (or
  accented, or emoji) character in a prompt or a command aborted the export —
  and `redact`/`contains_secret` sit on the same path, so this could reach any
  redaction of non-ASCII text. Fixed by refusing indices that are not char
  boundaries (no rule can start there: every prefix is ASCII), with a
  regression test in Korean, French and emoji.


### 2026-09-03 (later) — the query catalog: `attempt schema`, `attempt_schema`, `docs/query-context.md`, `AGENTS.md`

*Correcting the record: this work is in commit `b238314`, whose message
("PROGRESS: the final-architecture plan, Phases 0-2 on the product side")
describes something else. A concurrent session in this repository ran
`git add -A` and swept the working tree into its own commit while this one was
staging. Same failure as the earlier `3b4005e` / `7b07a0f` pair. The content is
correct and tested; only the message is wrong, and it was already pushed, so
the history is left alone. Two agents committing in one worktree needs a rule,
not another apology.*

The database had a query surface and no way to learn it. Table names were in
the `attempt_query` tool description, column names only in `tables.rs`, and
the meanings nowhere — so anything writing a statement (a person on day one,
or an LLM on any day) had to guess or read the projector. Three surfaces now
answer that, all from one source.

- **`crates/attemptdb-query/src/catalog.rs` (new, ~1 300 lines, 3 unit
  tests).** Prose only: each table's layer (fact or inference), its grain, a
  summary, its joins, and one line per column. The column *list* is
  deliberately not in it — columns, types and nullability are read from the
  real Arrow schema at call time, so the catalog cannot describe a column that
  does not exist. Closed vocabularies are built from the Rust enums that
  produce them, with exhaustiveness guards that stop compiling when a variant
  is added. That guard immediately earned itself: it caught that
  `Provider::Other(String)` exists, so `provider` is an **open** vocabulary,
  not the closed list of four the docs would otherwise have asserted.
- **Nine rules** ship with the catalog, written for a reader with no other
  context. The ones that decide correctness rather than validity: `events` is
  fact and everything else is inference with evidence ids; retracted rows are
  hidden by AttemptQL and *visible to SQL*; content is null by design under
  `metadata_only` and that is a privacy setting, not missing data; the
  projection's counts already apply the retraction rules, so re-deriving them
  with `COUNT(*)` gives a different and usually wrong number.
- **`attempt schema` (new command, `crates/attempt/src/cmd_schema.rs`).** The
  only command that never opens the database, which is the point: an agent
  that has just cloned the repository can learn how to query it before there
  is anything to query. `attempt schema` (rules + table list), `attempt schema
  <table>` (every column, meaning, allowed values), `--examples`, and
  `--format markdown|json`. Five CLI tests, one of which points `--data-dir`
  at a non-existent path so a pass proves no database was opened.
- **`attempt_schema` MCP tool (10th tool).** Same catalog, shaped for a tool
  loop: no arguments returns the rules and the table list (small), `table=`
  returns one table in full, `examples=true` returns the worked questions. It
  is the tool to call before `attempt_query`.
- **`docs/query-context.md` (709 lines, generated).** Identical content,
  produced by `attempt schema --format markdown`.
  `crates/attemptdb-query/tests/catalog.rs` (7 tests) fails when the file and
  the code disagree; `UPDATE_GOLDEN=1` regenerates it, following the adapter
  fixtures' convention.
- **Every example is executed.** The 23 worked questions run against the
  reference scenario in `every_example_runs`, with `{session}` / `{attempt}`
  placeholders substituted from the fixture. An example that stops parsing is
  a failing test, not a stale document.
- **`AGENTS.md` is now the canonical instruction file**, `CLAUDE.md` a
  20-line pointer plus what is genuinely Claude-Code-specific. Anyone arriving
  with Codex or Cursor previously got no instructions at all. Moving it also
  surfaced a stale crate list: `attemptdb-mcp`, `-ui`, `-server`, `-bench` and
  `attempt-hook` had never been added to it.

Not done, on purpose: `llms.txt` waits for a documentation site to exist
(TODO §14), and the catalog describes only what the engine registers — a
Tier-2 projection will need its own entry when one lands.

### 2026-09-03 — the local product: Overview, Work, Needs You, live updates, demo mode, the summary card

Six items from TODO §11 (`docs/agent-timeline-ui.md` §8.1, §8.3, §8.4, §9.1,
§8.11, §11.4). Everything is still the server-rendered Rust UI: the React
package of §11.1 is a separate item, and answering the product questions first
is cheaper than answering them inside a rewrite.

- **`attemptdb-project::attention` (new module, 15 tests).** The Needs You
  queue is a shared inference, not a UI trick: `Projection::attention_at(now,
  min_confidence)` returns ranked `AttentionItem`s carrying evidence ids,
  confidence, an uncertainty sentence and `ALGORITHM_VERSION`. Four rules and
  no others — an uncleared permission request (rank 1), an uncleared
  `idle_prompt` / `agent_needs_input` notification (2), an open work unit whose
  last two attempts failed with the same class and were not superseded by a
  successful attempt (3), two open work units editing shared paths (4). The
  tests that matter are the negative ones: a normal completed turn, an idle
  open session, a single failure, two failures of different classes, a cleared
  signal, a successful retry after the loop, and a signal in an ended session
  all produce an empty queue. On this repository's own 8 000-event database the
  queue is empty with ten sessions open — the precision is real, not a
  coincidence of a small fixture.
- **Overview** is now current work → Needs You strip → live execution →
  attempt path, with work units, decisions, artifacts, handoffs, sharing and
  the capture/storage tables below the fold. *Live* means activity within 30
  minutes (`attemptdb_ui::LIVE_WINDOW_MS`); the eight sessions here that are
  open only because their provider never sent an end event are counted in one
  honest line instead of being drawn as running agents. The empty-database
  screen is the three-step first-run status with a link into the demo.
- **Work board** (`/work`): Active / Blocked / Recently finished over inferred
  work units, each card carrying objective-or-the-reason-it-is-missing, phase,
  status, actors, attempts, failures, paths, span, confidence, evidence and the
  attempt chain; `/work/{id}` is the inspector (attempts, decisions, handoffs,
  commits, the Needs You items that name it). No `Next` column: AttemptDB does
  not know about planned work.
- **Live updates** (`GET /api/live`): a server-sent stream of a 16-hex-digit
  revision hashed from the WAL/manifest/spool file sizes and mtimes. It opens
  no database, decodes no segment and projects nothing — the probe is a `stat`.
  The client refetches only the region that changed (`/api/overview`,
  `/api/attention`), can be paused so an inspected item does not move, and asks
  for a reload rather than re-rendering evidence links from JSON.
- **Demo mode** (`attempt ui --demo`, `?demo=1`): a *separate* database
  generated into the cache directory from a story written in `demo.rs` — the
  framed WAL, a test that fails and the retry that works, a handoff to Codex, a
  commit each, and one permission request nobody answered. Ids are derived from
  the story position so every machine gets the same demo; timestamps are
  anchored to now and rebuilt after six hours so "live execution" means
  something. Every event is `reconstructed` / `reconstructed_from:
  attemptdb-demo`, every page carries a banner, and the flag rides on every
  link and form so a click cannot silently leave it. A demo event can never
  reach the user's own database.
- **Summary card** (`GET /card.svg`, `attempt ui export card.svg`): a 1200×630
  SVG — the attempt chain with outcomes and failure classes, the counts, the
  providers, the tagline, the attribution. Sanitized by construction rather
  than by flag, because an image is shared before anyone reads it: no prompt,
  command or tool-output text, and only repository-relative paths. The UI test
  asserts the reference story's prompt, its `<script>` payload, its command and
  its `/home/alice` path are all absent.
- **Not done, deliberately.** PNG rasterisation of the card needs a font
  rasteriser; the single self-contained binary is worth more, so the TODO line
  was split and the PNG half left open. The nav still carries
  Failures/Handoffs/Why as top-level entries (a separate TODO item), and
  correction authoring is still the CLI (`attempt correct …`), linked from each
  Needs You item rather than written from the page.

**README media (same session).** `docs/media/agent-timeline.png` and the
25-second `docs/media/ui-demo.gif` are real `attempt ui --demo` screens —
Overview, Needs You with its evidence, the Work board, the superseded attempt,
and the exported card — cropped, captioned and sequenced by
`docs/media/ui/render.py` the way `docs/media/demo/render.py` already does for
the terminal demo. Nothing in either is drawn by hand, and the demo database is
deterministic, so the capture recipe in that script reproduces them. Two more
product bugs came out of taking the screenshots: `.live-wrap`'s `display` beat
the `[hidden]` attribute, so the live indicator showed `connecting…` before the
stream existed; and the demo rebuilt only every six hours, so an hour-old demo
opened on an empty Live execution card — the one thing the Overview exists to
show. The rebuild window is now 20 minutes, inside `LIVE_WINDOW_MS`, asserted
by a test.

577 tests green, `cargo clippy --workspace --all-targets` and `cargo fmt --all
--check` clean. Smoke-tested against this machine's live database and in demo
mode in a real browser: the Overview showed this very session — work unit
`implement`, the prompt that started it, `att_db0d0b5e ✗ failed file_not_found
→ att_0013cecf ▶ in progress`.

### 2026-09-03 (night) — the final-architecture plan, Phases 0–2 on the product side

The owner set the goal to the plan (AttemptDB the only home of raw agent
events; Supabase keeps users, XP, notifications, billing, teams and
rollups). Done in vibemon-app / vibemon-web, nothing in this repository:
Phase 0 (effects ledger + one transaction per page + hourly reconcile;
found `upsert_coding_stats` broken since March), Phase 1 core (app and
web read `/live`, `/sessions`, `/events` through JWT-authenticated
proxies when a device is paired; app OTA shipped), Phase 2 foundation
(`user_activity_daily` fed by both paths, backfilled, audited daily —
96/96 users exact against the batch rule). Phase 3's lever is the
existing app update modal (`install.sh?v`), untouched pending the
owner's timing. Record: `vibemon/ATTEMPTDB_FINAL_ARCHITECTURE_PLAN.md`.

### 2026-09-03 — VibeMon runs on AttemptDB collection: the webhook bridge, the owner's machine, the canonical switch

The owner's direction: "NOTE.md 방식대로 — `/hook`은 필요없다". So the
legacy client and `/hook` were left alone entirely; AttemptDB is the
collector and the server pushes to the product.

- **Outbound webhook** (`webhook.rs`): after an accepted batch the server
  delivers the tenant's events past a durable per-tenant cursor, HMAC-
  signed, in pages of 500; 2xx advances the cursor; three retries, then a
  60-second sweep; catch-up after a restart. Keys gained `issued_at`,
  exposed as `paired_at` per device in the page. Integration test with an
  in-process receiver that fails first and restarts.
- **`attemptdb-events`** (vibemon-app Edge Function): verifies the
  signature, maps canonical events to the legacy `hook_events` row shape
  (edits → `tool_use` xp 1, shell → `bash`, failures, prompts, stops,
  permissions, session start/end, capture test → install status; reads,
  MCP calls and subagents are not mirrored, as the legacy client never
  recorded them), inserts ON CONFLICT DO NOTHING on `attemptdb_event_id`,
  and applies `/hook`'s side effects to new rows only (XP + slime, 2 s
  rate-limit parity, projects, coding_sessions, work_links, streak, line
  stats, throttled achievements, permission notifications, collaboration
  subscriptions, squad gate). Events observed before the device's
  pairing time are skipped, so a machine that ran both collectors is not
  counted twice. Three bugs surfaced on the first real delivery — a
  partial unique index does not satisfy ON CONFLICT, NOT NULL line
  counts, a session whose newest row was a stop losing its project — and
  the server's sweep re-delivered the page after each fix.
- **Verified with the owner's real VibeMon account** from a sandbox
  device: rows with `envelope_version 3`, local hour in Asia/Seoul, XP
  fed, session upserted; test artifacts removed.
- **The owner's machine is connected for real**: attempt 0.1.0 → 0.2.0
  in `~/.cargo/bin` (hook commands unchanged, Codex trust kept), daemon
  replaced (launchd's asynchronous teardown made the first bootstrap fail
  with EIO — retried by hand, now retried by `daemon install`), 12,722
  events uploaded, legacy hooks removed from all four agents after the
  upload was accepted. From that minute the machine's VibeMon rows arrive
  only through AttemptDB.
- **0.2.1** released and the **canonical `/install.sh` switched** to the
  AttemptDB installer (vibemon-web `1f25f6d`): the installers accept the
  older `vbm_…` command by exchanging the key at `POST /api/attemptdb/pair`
  for a pairing token, `?v` is pinned to the legacy VERSION 30 forever,
  and a run with no argument changes nothing. Verified live from a
  sandbox: the `/setup` command installed 0.2.1, paired, uploaded, done.
  `/install.ps1` stays legacy until Windows parity. A volume snapshot was
  taken and the live tenant verified on the server (6 segments, WAL
  clean). Left: `sync.vibemon.dev` at Squarespace (owner login), a
  restore rehearsal onto a fresh volume, an external health monitor,
  the Windows track, code signing.

### 2026-09-02 (evening) — the one-line install, end to end: pairing, installers, `/devices`, 0.2.0, Fly bootstrap

NOTE.md (vibemon) listed what stood between "AttemptDB exists" and "a
user copies one command"; this session implemented it and pushed the two
repositories. What landed, with the evidence behind each tick in NOTE.md:

- **Pairing** (`attemptdb-server/src/pairing.rs`): `POST /v1/admin/pairings`
  mints a `pair_…` token (digest only in `pairings.json`, 10-minute
  default TTL, single use); `GET /v1/pair/{token}` for the installer's
  pre-flight; `POST /v1/pair` exchanges token + the local `device_id` for a
  device key bound to that id and retires the same device's earlier keys.
  A token-bucket limiter (`limiter.rs`) per client address on the pairing
  routes, per bearer key elsewhere. `crates/attempt/tests/pairing_e2e.rs`
  runs the whole exchange against an in-process server.
- **`attempt sync connect --pair | --key`** proves a key with an empty
  batch (401 unknown / 403 another device's) *before* saving it and
  restores the previous peer on failure. Defaults are now semantic / 5 s;
  each tick reads only past its cursor; inference recompute is gated on an
  upload that stored something. `doctor` shows the sync section;
  `--remove-legacy vibemon` knows `notify.py`/`notify.ps1`.
- **The operator's read**: admin token + `X-AttemptDB-Tenant` reads any
  tenant, so the product's backend needs no reader key stored per user;
  operator reads skip the per-key bucket. `/v1/devices` gained
  `last_seen_at` (set by the handshake, so "Connected" appears before the
  first event).
- **Installers** (`docs/migration/vibemon-install.{sh,ps1}`): token check →
  binary (pinned `ATTEMPTDB_VERSION=0.2.0`, fetched from that tag; the old
  default pointed at a release asset that does not exist) → init without
  touching an existing database's mode → pair → hooks → daemon → one
  upload the server must accept → legacy hook removal → doctor. No token
  on a never-connected machine: exit 0, nothing changed (the legacy
  auto-updater's path). `vbm_…` argument: exit 2, nothing changed.
- **0.2.0 tagged and pushed**; the Release workflow builds the eight
  targets. CI gained a `cargo audit --deny warnings` job (clean today).
  `docs/server-api.md`: pairing, the handshake, rate limits, the
  operator's read; `docs/deploy.md`: the Fly section, backups (volume
  snapshots), the one-line install; `deploy/fly-up.sh` is the idempotent
  bootstrap (app → volume → secret → deploy → cert → DNS line → health).
- **vibemon-web** (`8473d8c`, pushed to main → Vercel): `/devices`
  (server action mints the token, two OS tabs, copy, expiry countdown,
  3-second polling while the token lives, Unlink with a two-step confirm),
  `/api/devices`, `/install-attemptdb.sh|.ps1` (302 to the v0.2.0 tag),
  `src/lib/attemptdb.server.ts` (tenant = `org_<user id>`, a personal
  organisation; a team tenant later means a re-pair, which the exchange
  already handles). Verified against a local server with a script:
  mint → `sync connect --pair` → `sync now` → listed with
  `last_seen_at`/`last_sync_at` → unlink → next upload 401. `next build`
  passes. Not yet seen in a browser (needs a signed-in session).

**2026-09-03, deployed.** The owner ran `fly auth login`; `deploy/fly-up.sh`
(FLY_ORG=streamize) created `attemptdb-sync` (iad, shared-cpu-1x 1 GB,
volume `attemptdb_data` 1 GB, admin-token secret), built the image on
Fly's builder (two Dockerfile bugs found on the way: `-p attempt` named
a package that does not exist — the CLI package is `attemptdb` — and
`[build].dockerfile` resolves relative to `deploy/`), and the Machine
came up with the health check passing. The first deploy could not
allocate addresses for an org-owned app, so the script now allocates a
shared IPv4 and a dedicated IPv6 itself. `https://attemptdb-sync.fly.dev/v1/health`
answers; the admin token (same value as Vercel's) is accepted. The two
April experiments on the account (`axonize`, `axonize-db`) were destroyed
at the owner's request. Vercel production points `ATTEMPTDB_SYNC_URL` at
the fly.dev host until `sync.vibemon.dev` exists (Squarespace Domains;
certificate already requested), and `/devices` appends `--server` to the
command whenever the sync URL is not the product hostname. A sandboxed
end-to-end run against the live installer and the live server — mint
through the web's server-side client, pair, handshake, hooks, daemon
registration (launchctl shimmed), one accepted upload, listed as
connected with `last_sync_at`, unlink — passed, this time with
`CLAUDE_CONFIG_DIR` unset so the owner's real settings stayed untouched.

Earlier that night: the admin token lives in `~/.attemptdb-admin-token` (0600,
what `deploy/fly-up.sh` reads by default) and Vercel production carries
`ATTEMPTDB_ADMIN_TOKEN` + `ATTEMPTDB_SYNC_URL` (redeployed). Left for the
owner: `fly auth login` (flyctl's stored token is expired; the browser is
not signed in to fly.io either) then `deploy/fly-up.sh`; the
`sync.vibemon.dev` record at Squarespace Domains (vibemon.dev's registrar
and DNS host — no GCP project has a Cloud DNS zone for it); a real
pairing from this machine; backup/restore rehearsal; then the canonical
`/install.sh` switch. Windows daemon and code signing remain separate
tracks.

### 2026-09-02 — engine audit: the read path was the scaling ceiling, and it was rebuilt

The owner asked whether the DBMS is properly built, fast for its purpose,
and scalable. The audit split the answer by layer and measured instead of
reciting: the **storage engine is a DBMS** (tmp→fsync→rename→dir-fsync,
CRC32C frames, idempotent WAL replay, 28 failpoint/SIGKILL crash tests, zone
maps, compaction, manifest pruning — nothing to fix), the **write path is
fast** (hook 3.8 ms, 28 k events/s over HTTP, 43 B/event on disk in
`metadata_only`), and the **read path was not a database at all**: every
event was resident in five or six representations at once (a `Vec<Event>`
next to its Arrow batches, projector observations, a "readable" events
table with prefixed-string ids rebuilt per view, twelve projection tables,
the id maps and the WAL clone), and a fingerprint of `(generation,
segments, memtable_rows)` invalidated all of it on every new event.
Measured with `attemptdb-server` over one tenant (release build, Apple
M5 Pro, `metadata_only`, 200 events/session; "1st read" is the first
read after one new event):

| events | 1st read before → after | first SQL on a fresh view | RSS before → after |
|---|---|---|---|
| 10 k | 33 ms → 26 ms | — → 64 ms (cold SQL layer) | 213 → 191 MiB |
| 100 k | 257 ms → 102 ms | — → 44 ms | 1,051 → 564 MiB |
| 200 k | 432 ms → 117 ms (44 ms when the WAL is small) | 195 ms → 26 ms | 1,996 → 792–984 MiB |

Both "before" columns were exactly linear (2.1 µs and 9.6 KiB per event),
which is what made the 1-year projection (1 M events: 2 s per read,
9.6 GB) credible. Where the memory went, from `examples/memory_profile`
over 200 k events: decoded events + Arrow 4.3 KiB/event → Arrow alone
0.7 KiB; projector observations 1.3 KiB; projection 0.8 KiB; SQL tables
1.2 KiB → 0.6 KiB (only the tables a statement scanned).

What changed, in commit order:

1. **Lazy SQL layer, per-segment derived parts** (`287cd91`). Ten of the
   server's eleven read endpoints, every UI page and every MCP tool read
   the projection; only SQL needs DataFusion. The context, the readable
   `events` table and the graph are built on first use. What a segment
   contributes (readable columns, id maps) is derived once
   (`SegmentParts`) and kept by `EngineCache`; the server's facts likewise.
2. **Arrow-only segment cache; facts and ids from the columns**
   (`6adfc90`). `CachedSegment` holds the manifest entry and the batches;
   events are decoded on demand one segment at a time. `StreamFacts`
   (projects, providers, sessions with first device and capture counts,
   per-device upload facts) is derived per segment from the columns and
   merged in stream order — server `/v1/status`, `/v1/devices`, project
   resolution, UI/MCP status, scope bar and capture counts all read it;
   `/v1/events` skips segments whose `source_seq` range ends at the cursor.
3. **Content only when asked** (`4d53318`). Encrypted content is one blob
   file per row; every reader opened all of them (15,108 files for this
   repository's 8,806-event database) to answer questions that never look
   at content. The projector now resolves content for three kinds
   (`needs_content`), the `events`/`events_raw` tables are a
   `TableProvider` that fills `content_json`/`raw_json` only for a
   statement projecting them, scoped views filter rows as Arrow
   (`ScanFilter::filter_batch`), and the CLI does one refresh per
   invocation and resolves `--project`/`--session` from facts. This
   repository's `attempt timeline`: 0.99 s / 234 MB → 0.10 s / 90 MB.
4. **CLI reads through the daemon** (`d8d2e8b`). IPC types `QUERY` (7,
   the reserved slot) and `RESULT` (10). A daemon started by `attempt`
   installs a `ReadService` (`EngineService`) that keeps an `EngineCache`
   next to the writer; a query refreshes it on the writer thread, then
   builds/reuses a per-scope view off that thread. Results travel as
   Arrow IPC (base64 in the JSON frame; no new dependency); a timeline is
   the projection trimmed to the sessions shown plus totals. The engine is
   dropped after 10 min idle. The CLI pings first and falls back to its
   own engine whenever the daemon cannot answer (older daemon, other
   database, result over the 16 MiB frame, `ATTEMPTDB_NO_DAEMON=1`).
   200 k events, warm daemon: `query` 9 ms, `timeline` 20 ms (825 ms
   locally). `crates/attempt/tests/daemon_read.rs` checks daemon and
   local answers byte for byte.
5. **Per-session projection index** (`5aa5ed9`). `turns_of`/`tool_calls_of`
   /`attempts_of`/`session`/`work_unit*` scanned whole tables; `STATE … AT`
   did that per open session (188 ms at 200 k, 9–36 s at 1.45 M in the
   benchmark doc). `SessionIndex` is built on first use, skipped by serde,
   ignored by equality, reset by clone and at the end of `assemble`.
   `STATE … AT` over 1,002 open sessions is now within run noise.
6. **Per-table lazy projection tables** (`2329f96`). Each of the twelve
   is a `TableProvider` built by the first statement that scans it;
   schemas come from the tables of an empty projection.

Also: `deploy/entrypoint.sh` exposes `ATTEMPTDB_MAX_OPEN` /
`ATTEMPTDB_IDLE_FLUSH_SECS` (the only memory dial the server has),
`deploy/fly.toml` for the planned Fly.io deployment, `docs/deploy.md`
documents both.

**VibeMon-shaped load, and `/v1/live`** (`880ba56`). The read numbers
above are one tenant. VibeMon's app will have every user poll every 5 s,
and its users are 88 weekly actives with a heavy tail (median 872, mean
4,987, max 137,007 events; `ACTIVITY_PERF_AUDIT.md`). Replayed as 88
tenants drawn from that distribution (324 k events) on the planned Fly
settings (`ATTEMPTDB_MAX_OPEN=3`), polling `/v1/sessions` every 5 s
while a writer pushed one event per second: **p50 9 ms, p90 43, p99 236,
max 323 ms, RSS peak 562 MiB, no errors** — and every slow poll was the
largest tenant being reopened (database open + decode + projection) after
eviction, 250–320 ms each time; the 137 k user would pay ~700 ms every 5
s for a question that needs no projection. The app's loop
(`useCodingState`) asks only for the newest event's time and kind, so
`GET /v1/live` answers it from a few hundred bytes per tenant that ingest
keeps current and that eviction never touches (seeded once per tenant
from the stream facts after a start). Same load on `/v1/live`: **p50 1
ms, p90 1 ms, p99 49 ms (the one-time seeds), RSS peak 340 MiB.** The
realtime decision of 2026-08-31 ("`/v1/sessions` polling at 5 s") is
refined: the 5-s loop is `/v1/live`; `/v1/sessions` is for screens.
Still missing for the activity screen: a facts-level daily endpoint
(counts, running time, hourly bins in the user's time zone) — 30-minute
UTC bins per segment, merged, would give every whole- and half-hour zone
without a projection; not built.

**The agent console's server gaps, closed** (analysis artifact
`6c464bf8`, commits `68e6b58` `fffc5c6` `3a94eb6` `9702394`). The
Vibemon Agent Console (8 artboards) was read screen by screen against
the read API; six gaps stood between the two, all on the server side and
none touching the on-disk format:

| gap | what | where |
|---|---|---|
| G5 people | `user_id` on sessions and `/v1/live`, `devices`/`users` on work units, from the tenant's device keys (read per request) | `read.rs::People` |
| G4 evidence | `GET /v1/events/{id}` decodes only the segment whose id range covers the id; metadata only | `corrections.rs` |
| G3 corrections | `POST /v1/corrections` — attempt outcome/note, turn objective, session retraction — written under the server's writer identity, attributed to the key's user (`attrs.x_attemptdb_corrected_by`); the note's text falls to its length under the ceiling | `corrections.rs`, `tests/console.rs` |
| G2 signals | adapters read a runner's summary line into `tests_passed/failed/skipped` (cargo, nextest, jest, vitest, pytest, mocha, rspec, phpunit, dotnet, go -v); `/v1/work` carries `signal.tests` / `signal.build` per unit, `null` when nothing was counted | `adapters/signals.rs`, `facts.rs`, `attrs.rs` |
| G1 conflicts | `conflict-v0` (RFC 0003 §5.8): two open units of one project, no shared session, a path both edited, windows overlapping or within two hours; per path each side's lines and commit state; 0.7/0.5. Needed **`tier1-v1`**: rule 1 no longer links turns of different sessions whose spans overlap, so concurrent actors are two units. `conflicts` table, `/v1/work.conflicts`, `/v1/attention` `reason = work_conflict` | `project/conflict.rs`, `workunit.rs` |
| G6 window | `--view-window-days N`: segments before the window are never decoded (zone map), the projector is rebuilt when the window moves by a day, `/v1/events` reads the manifest so backfill sees everything. Measured on a 300 k-event tenant spread over 30 days: window 14 → 140 k resident, 7 of 15 segments decoded, RSS 1,164 → 628 MiB, first read 91 → 32 ms | `storage/cache.rs`, `query/cache.rs`, `engine.rs` |

Still the owner's: **D1**, whether the `semantic` profile carries the
device-inferred `objective` sentence (the console shows it on team
screens; the 2026-08-31 decision strips it). The server works either way
— `objective` is `null` when it did not travel.

**What is still O(n) per view, and known.** `IncrementalProjector::snapshot`
clones every session build and re-assembles the whole `Projection`
(~50 ms at 200 k; cross-session work units and handoffs need the whole
set), and the WAL's ≤ 20 k events are re-encoded per view. Making the
assembled projection persistent with per-session replacement is the next
structural step, not tuning. Memory per resident event is now ~3.5 KiB
(Arrow 0.7, observations 1.3, projection 0.8, tables ≤ 0.6): a 1 M-event
tenant is ~3.5 GB resident, down from ~9.6 GB — the server's
`ATTEMPTDB_MAX_OPEN` remains the bound. Blob packing (one file per
segment instead of per row) would change the on-disk layout and needs a
format bump and an RFC; not started. Measurements are macOS arm64; the
Alpine/musl allocator may hold more.

### 2026-08-29 — hardening and capture depth (in progress)

- First commits: `5d8c033` bootstrap, then hook path (`391e69a`), storage crash harness (`4bbeede`), docs.
- Hook path: trusted-tail spool open (`inbox.spool.committed` sidecar), spool fsync off by default (WAL stays the durability boundary), `ATTEMPTDB_HOOK_TRACE` stage timings → in-process ~0.6 ms, wall p50 4.7 ms / p95 5.3 ms including spawn (gate p95 < 10 ms met; binary spawn alone is ~3.6 ms).
- Crash-injection harness (`crates/attemptdb-storage/tests/crash.rs`, failpoints, 25 tests, ~3.5 s) found and fixed 7 engine bugs: torn record after partial append, memtable drained before the generation was durable, source_seq gap after a failed WAL append, read-only open mutating the WAL, sub-header WAL/spool files wedging open, opaque Arrow errors on damaged segments, stale `.tmp`/unreferenced-segment hygiene. `docs/storage-format.md` updated with the observed semantics.
- Sanitized, project-scoped `.atdb` export (`snapshot export --sanitized --anonymize-sessions --drop-remote`) and `snapshot audit` privacy review; the real self-capture data exports with zero findings.
- `attempt uninstall [--purge-data]`; GitHub Actions matrix (macOS arm64/x86_64, Linux x86_64/arm64, Windows, musl static) with a CLI smoke test — unverified until the repo has a remote.
- Cross-target `cargo check` for Windows/Linux cannot run locally: `zstd-sys` needs a cross C toolchain; CI covers it.
- Open engineering note from the harness: after a *rejected* newest manifest generation the segment only it referenced becomes unreferenced (warned, left in place) → `attempt repair` must re-adopt it (not implemented yet).
- Transcript import (`attempt import claude-transcripts`): parser verified against ~400 real transcripts (entry types, tool_use/tool_result pairing, subagent files, compaction boundaries, interruptions); 5 synthetic fixtures + goldens; the bootstrap session and its 13 subagent transcripts were imported as **reconstructed** events (1152 events, `attrs.reconstructed = true`) and merge with the hook-captured tail of the same session. Projections now order by `observed_at` first so reconstructed and captured events interleave correctly.
- Injected prompts: Claude Code fires `UserPromptSubmit` for subagent task notifications; the hook adapter, transcript parser, and projector now treat `<task-notification>`/`<system-reminder>`/local-command text as notifications, never as turns (the two already-captured ones stay in the log as facts and are skipped by the projector).
- Daemon: `attempt daemon run|status|stop|install|uninstall`; `ATIP` prelude + CRC32C frames over a Unix socket (fallback path under `$TMPDIR` when the runtime dir exceeds sun_path); group-commit writer; spool imported at start and every 5 s; installed as `~/Library/LaunchAgents/dev.attemptdb.daemon.plist` on the owner's machine at 12:50 KST. With the daemon, hooks deliver via IPC (ack after WAL fsync, 3–6 ms) and read commands open read-only while it holds the writer lock.
- MCP: `attempt mcp` serves tools over stdio; `attempt_handoff_brief` produced a 15 KB continuation brief of AttemptDB's own history (latest session, last turns, failures with evidence ids, files touched, open calls, uncertainty) — the first concrete answer to "can the next agent continue without a new explanation?". `.mcp.json` in the repo registers it for Claude Code (`attempt` must be on PATH); Cursor/Codex via `attempt mcp --install`.
- `attempt repair` / `snapshot restore` landed (see the repair tests for the exact scenarios); the live database reports "nothing to repair".
- **Benchmarks (1.45 M events, Apple M5 Pro):** ingest 8.6 k ev/s strict / 11.9 k relaxed; WAL ack 3.0 ms strict (= macOS `F_FULLFSYNC` floor) / 9 µs relaxed; hook process 3.8 ms p50 spool path, 7.0 ms via strict daemon, 124 µs in-process; TRACE 187 µs on a 2 M-edge graph; GROUP BY over 1.45 M rows 22 ms. **Pathological:** whole-history `QueryEngine` = 160 s and 21 GiB at 1.45 M (≈15 KiB RSS/event); `STATE … AT` is O(open sessions) → 9–36 s; 75 MiB binary load ≈ 85 % of hook wall time. Bugs found and fixed the same day: single-RecordBatch encoding overflowed Arrow's i32 Utf8 offsets above ~300 k events (now ≤ 4,096 rows per batch everywhere); manifest generations were never pruned (O(n²): 1,855 files / 1.38 GiB) — now the newest 9 are kept; default flush thresholds raised to 20 k events / 64 MiB (was 5 k / 8 MiB → 1,854 tiny segments). Still open: compaction, incremental/bounded projections (default scope + `--since` for big databases), session auto-close for `STATE`.
- **VibeMon shadow validation (M6) found a production outage in VibeMon itself.** Since hook install (2026-08-28 09:38Z) AttemptDB captured 1,230 Claude Code events (581 tool finishes, 9 failures, 6 prompts) from this machine; VibeMon's own `catch_up` for the same window reported 0 sessions and 0 failures, and `session_list` shows nothing after 2026-08-27. Root cause (verified with a local HTTP sink): `vibemon-hooks` v28 (commit `f632657`, released 2026-08-26 10:19Z, still present in v29) inserted a comment block *inside* the backslash-continued `VIBEMON_*=… \` env-prefix before `python3`, so the assignments run as plain shell variables and the extractor starts without them — every envelope is `{"event":"unknown","cwd":"","timestamp":"","payload":{}}` and the server answers 400. Fix = move the comment above the block (verified: envelope correct, server 200). Applied to the installed `~/.vibemon/notify.sh` on this machine (backup `notify.sh.bak-v29-attemptdb`); the `vibemon-hooks` repository fix and a v30 release are the owner's call.
- Benchmark noise: an early latency benchmark ran hooks against the real database (provider session `bench`, 103 shell events). Append-only means they stay; exclude with `--session`/`--captured-only` when demoing, or add a tombstone/correction mechanism (RFC 0003) later.

### 2026-08-28 — bootstrap: workspace, engine, adapters, projections, capture, docs

- Read TODO.md; surveyed the VibeMon hook client (config paths, payload shapes, pitfalls, fixtures) and the official Claude Code hooks reference (28 events, settings precedence, exit-code semantics, hot reload of settings).
- Created the Cargo workspace (7 crates) and implemented core, storage, capture (locator/config/hook/ingest) and the CLI by hand; delegated adapters, projections, installer/doctor/platform/git, the query crate, and the RFC/OSS docs to parallel agents against written contracts.
- Final counts: core 16, storage 14, adapters 26, project 32, query 32, capture 45 — 165 tests green; clippy clean.
- End-to-end verified with simulated Claude + Codex sessions (failed → superseded attempt, WHY blocked on a Codex permission request, handoff Claude→Codex, `.atdb` export/inspect/query). Then installed hooks for real (see above); this session's own tool calls were the first captured events.
- Polish after e2e: handoffs require both sessions to have prompts/tool calls (capture tests and stray events no longer produce handoffs); timeline hides inactive sessions unless `--all`; `failures`/`handoffs` use compact column sets; explanations render as key/value records; doctor distinguishes `verified` (capture test only) from `active` (real events).
- Pre-capture history note (TODO §12): everything in this session happened before AttemptDB could capture itself. It must be imported/marked as *reconstructed*, never presented as captured fact.
