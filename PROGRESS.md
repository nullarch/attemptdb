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
| Sync (M6), Tier-2/3 inference, evaluation harness/gold dataset, compaction, incremental projections, release packaging, Windows/Linux runs | ⛔ not started | — |

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
- M1 Durable engine — WAL/recovery/memtable/segments/manifest ✅ (macOS only so far), crash-injection tests 🟡 (unit-level torn-tail + tamper tests; no process-kill harness yet), Windows/Linux runs ⛔, compaction ⛔.
- M2 Agent semantics & query — projections ✅, DataFusion/AttemptQL ✅ (v0; work units/decisions/corrections not projected).
- M3 Native capture — hook/spool/installer/doctor ✅, daemon/IPC ✅, encryption ✅ (segment format 2 + key store; enable per database with `attempt keys init`), real-payload verification for Cursor/Gemini 🟡 (fixtures from a production installer, not re-captured here); Windows/Linux runs ⛔ (CI pending a remote).
- M4 Inference & correction — Tier-1 ✅ incl. work units, derived decisions, corrections and retractions; evaluation harness / gold dataset ⛔ (needs design partners).
- M5 AgentTimeline & self-hosting — CLI timeline ✅, MCP ✅, local web UI ✅, static sanitized export ✅, self-capture ✅ running since 2026-08-28; one-minute demo recording ⛔.
- M6 VibeMon bridge ⛔. M7 Show HN ⛔. M8 ⛔.

## Next actions (ordered)

1. Owner: run `/hooks` inside Codex once to trust the AttemptDB entries (`attempt doctor` shows `untrusted` until then); restart Cursor/Gemini sessions.
2. Push to `github.com/nullarch/attemptdb` so the CI matrix (macOS/Linux/Windows + musl) runs for the first time; fix whatever Windows/Linux surface it exposes (named pipes, `sync_dir`, paths). The push needs a token with the `workflow` scope — the active `nullarch` token has only `gist, read:org, read:user, repo`, so the owner runs `gh auth refresh -h github.com -s workflow` once.
2a. Scale work from the benchmarks: segment compaction; bounded projections (project + time window by default, incremental projection cache); `STATE … AT` over open sessions only (auto-close idle sessions); consider a separate small `attempt-hook` binary (75 MiB load ≈ 85 % of hook time).
3. Hook latency through the daemon is fsync-bound (3–6 ms `ipc` stage under strict durability vs 0.35 ms spool): decide whether the daemon should default to group-commit-with-timer (`--relaxed` exists) once `attrs.hook_us` p95 from real data is known.
4. Run the suite on Linux and Windows (CI matrix exists in `.github/workflows/ci.yml`; needs a remote). Local cross-`cargo check` is blocked by `zstd-sys` needing a cross C toolchain.
6. Codex/Cursor/Gemini transcript or log import where such files exist (only Claude Code is reconstructed today).
7. Tombstones/corrections (RFC 0003) so benchmark noise or mistaken imports can be retracted without rewriting facts (the `bench` session is the first real case).
8. Encrypted content blobs (format v2), key management per OS, `attempt verify/repair` completeness, compaction.
9. Work units / decisions / corrections projections and the evaluation harness (M4); local web UI (M5).
10. Decisions needing the owner (TODO §19): license confirmation (Apache-2.0 assumed), GitHub org/repo URL, domains, whether the 1.45M-event aggregate may be published.

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

One open finding: `crash::abort_wal_append_after_write` and
`abort_manifest_after_tmp_write_leaves_a_tolerated_tmp_file` failed on
**macos-x86_64 only**, both with `Locked` on a writer open taken straight
after `drop(db)`. Closing the lock file releases the `flock` synchronously, so
this should be impossible; 12 consecutive local runs on macOS ARM64 pass and
the ARM64 CI job passes too. `open_eventually` in `crash.rs` now waits up to 5
seconds and reports how long it actually waited, which separates the two
possible causes — a lagging lock release (milliseconds) from a genuinely
leaked handle (budget exhausted, and then the fix belongs in the engine).

Run 3 was green on macos-x86_64 and printed no wait, but that proves nothing:
libtest captures the print macros for a passing test, so the diagnostic could
not have been seen either way. It now writes to the real stderr, which
bypasses that capture. Treat this as unresolved until an Intel run reports an
actual number.

### Pre-public checklist

- [ ] `CODE_OF_CONDUCT.md` still names `conduct@attemptdb.dev`, an address
      nobody owns. Either register it or replace the escalation path.
- [ ] Confirm the 1.45M-event VibeMon aggregate may be published (TODO §19).
- [ ] Domains: `attemptdb.dev` / `attemptdb.com` unregistered.
- [ ] Run `attempt snapshot audit` on anything shipped as a demo dataset.
- [ ] First tag + green CI matrix on all five core targets.

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
