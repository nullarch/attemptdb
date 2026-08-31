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
| MCP server (`attempt mcp`): 9 tools incl. `attempt_handoff_brief`, resources, `--print-config`/`--install`, project `.mcp.json` | ✅ implemented, tested (21), real-data smoke | `crates/attemptdb-mcp`, `crates/attempt/src/cmd_mcp.rs` |
| `attempt repair` (adopt/rebuild/quarantine/tmp/identity) and `snapshot restore` | ✅ implemented, 18 scenario tests | `crates/attemptdb-storage/src/repair.rs`, `crates/attempt/src/cmd_repair.rs` |
| Encrypted content blobs (XChaCha20-Poly1305, keyed-hash ids, segment format 2) + key management (keyring / key file / passphrase, `attempt keys`), key-aware snapshots | ✅ implemented, tested (storage 79, capture 72) | `crates/attemptdb-storage/src/blobs.rs`, `crates/attemptdb-capture/src/keys.rs`, `crates/attempt/src/cmd_keys.rs` |
| Local web UI `attempt ui` (token-authed loopback, now/timeline/session/attempt/failures/handoffs/why/state/query, JSON API, SVG trace) + `attempt ui export` static sanitized HTML | ✅ implemented, tested (18), smoke-tested on the live DB | `crates/attemptdb-ui`, `crates/attempt/src/cmd_ui.rs` |
| Work units, derived decisions, corrections, retractions (`attempt correct`, `attempt retract`), new tables + AttemptQL statements | ✅ implemented, tested (project 44, query 38) | `crates/attemptdb-project/src/{workunit,decision,meta}.rs`, `crates/attemptdb-query`, `crates/attempt/src/cmd_correct.rs` |
| Benchmark program (`attemptdb-bench`, 1.45 M-event synthetic replay) + `docs/benchmarks.md` | ✅ run on macOS ARM64; pathologies documented | `crates/attemptdb-bench`, `docs/benchmarks.md`, `docs/benchmarks/2026-08-29-macos-arm64.json` |
| Sync client (peers, profiles, cursors, secret scanning), `attemptdb-server` (per-tenant databases, key scopes, admin keys, device removal, read API, legacy envelope, backfill importer), deployment files | ✅ implemented, tested (waves 5–13); not deployed | `crates/attemptdb-capture/src/sync.rs`, `crates/attemptdb-server`, `deploy/`, `docs/server-api.md`, `docs/deploy.md` |
| `attempt-hook` (0.8 MB hook entrypoint), incremental projection, on-disk compatibility fixture, commit linkage, rollback-safe `attempt update` | ✅ implemented, tested | `crates/attempt-hook`, `crates/attemptdb-query/src/cache.rs`, `fixtures/db`, `crates/attemptdb-project` |
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

One step remains. Four that were on this list are done.

| Step | State |
|---|---|
| ~~Repository public~~ | **done 2026-08-31 (attemptdb-d7).** Reachable from the CLI after all: `gh repo edit --visibility public --accept-visibility-change-consequences` |
| ~~Social preview~~ | **done.** `docs/media/social-preview.png` uploaded through Settings → General; the section only exists once the repository is public, and there is no REST API for it. Verified by fetching the live `og:image` and comparing bytes — identical to the file in the repository |
| ~~Profile~~ | **done.** `nullarch/nullarch` created and pushed; six repositories pinned, `attemptdb` second, top row |
| ~~First tag~~ | **v0.1.0 shipped 2026-08-31**, tagged at `302b963` — the tree CI actually verified, not the tip. Held through three red matrices until run 33352579921 came back green on all seven jobs; the release run built all eight targets, published `SHA256SUMS`, and the homebrew job took its "tap token not configured" branch as expected. Verified by installing from the published release into a scratch directory: `attempt 0.1.0`, `attempt-hook 0.1.0` |
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
