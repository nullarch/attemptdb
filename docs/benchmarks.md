# AttemptDB benchmarks

> Workload benchmarks (TODO §15) on one machine, one commit, one day. Every
> number below was measured by `crates/attemptdb-bench`; the raw results are
> in [`benchmarks/2026-08-29-macos-arm64.json`](benchmarks/2026-08-29-macos-arm64.json)
> and the tables in this file were rendered from that file with
> `attemptdb-bench report`. Nothing here is a best case chosen by hand, and the
> section [Pathological and unflattering](#pathological-and-unflattering) is
> the part worth reading first.

## Scope

Ten questions, each answered by one or more benchmark steps:

| # | Question | Steps |
|---|---|---|
| 1 | How fast is sustained ingest, Strict versus Relaxed, and what does a concurrent reader cost? | `ingest_*` |
| 2 | How long until a single event is acknowledged (WAL) or spooled (hook path)? | `wal_latency` |
| 3 | What does one hook process invocation cost, with and without a daemon? | `hook` |
| 4 | How long does the recent timeline (last 24 h) take? | `recent_timeline` |
| 5 | How long does a full historical scan take, and how much memory? | `scan_project_full`, `engine_*` |
| 6 | How long does `TRACE … CAUSES DEPTH 10` take over a 200-attempt chain? | `trace_chain`, `engine_*` |
| 7 | How long does `STATE project AT <ts>` take? | `engine_*` |
| 8 | How does projection cost grow with event count? | `projection_*` |
| 9 | How large is the database per event and per kind, and how well does it compress? | `size_by_kind`, `ingest_strict_full` |
| 10 | What do many small segments cost readers, and what does compaction give back? | `segments_100k`, `compact_100k` (added 2026-08-30, see the last section) |

Not covered: Windows and Linux (TODO §15 asks for each independently; only
macOS was run), multi-device merge, encrypted blob segments (format 2; every
database here is format 1 with content inline), and the MCP server.

## Methodology

### Machine and build

| Item | Value |
|---|---|
| CPU | Apple M5 Pro |
| Logical CPUs | 18 |
| Memory | 48.00 GiB |
| Disk | File System Personality: APFS, Protocol: Apple Fabric, Solid State: Yes |
| OS | macOS 26.4 (aarch64) |
| rustc | rustc 1.94.1 (e408947bf 2026-03-25) |
| Commit | `6b97b5f (HEAD when the run started; binaries built from the working tree minutes before that commit landed, which still carried uncommitted changes)` |
| Build profile | release |
| `attempt` binary | `~/attemptdb/target/release/attempt` (74.66 MiB) |
| Run | 1,450,000 events, seed 20260829, time cap 600 s per step, RSS cap 36.00 GiB, started 2026-08-29T05:33:40.252484Z |

### How measurements were taken

- **One process per step.** `attemptdb-bench run` spawns
  `attemptdb-bench step <name>` for every step. Peak resident memory is the
  child's own `getrusage(RUSAGE_SELF).ru_maxrss` at exit (bytes on macOS;
  `VmHWM` from `/proc/self/status` on Linux), cross-checked by the parent
  polling `ps -o rss=` every 250 ms. Both are reported. A step is killed when
  its resident set exceeds the cap (36 GiB by default on this 48 GiB
  machine) or when it runs past twice the soft time cap; killed steps are
  listed as *not run* with the reason.
- **Timing** is `std::time::Instant` around the call under test only. Ingest
  numbers exclude event generation ("ingest events/s"); a second figure
  includes it ("wall events/s"). Latency tables give p50 / p95 / p99 over the
  stated number of samples; with 10 samples the p99 is the maximum.
- **Durability semantics.** `Database::ingest` under `Strict` calls
  `File::sync_data` on the WAL before acknowledging. On macOS the Rust
  standard library implements both `sync_data` and `sync_all` as
  `fcntl(F_FULLFSYNC)`, which flushes the drive's write cache, unlike
  `fsync(2)`, which on Apple platforms does not. The WAL latency section
  measures all three on the same file so the floor is explicit.
- **Disk numbers** are the sum of file sizes under `segments/` (Arrow IPC with
  per-buffer zstd), `wal/`, and `manifest/` after `Database::close`, which
  flushes the memtable. "WAL bytes" is the sum of the framed JSON records
  the writer appended, i.e. the raw JSON size of the events.
- **Soft time cap.** Ingest loops stop at the cap and report the count they
  reached (`capped`). No step in this run hit it unless the table says so.
- **The `attempt` binary** used by the hook and daemon steps is the release
  build from the same commit (`target/release/attempt`). The daemon is started
  by the benchmark as a child (`attempt daemon run --foreground [--relaxed]`)
  with `ATTEMPTDB_DATA_DIR` pointing at a temporary directory, so the user's
  database and the user's daemon are never touched.

### The synthetic workload

The generator (`crates/attemptdb-bench/src/{model,workload,text}.rs`) is
seeded and deterministic: the same seed yields byte-identical events,
including UUIDv7-shaped event ids whose time part is the synthetic
`observed_at`, so the writer's id-range dedup fast path behaves as it does
with real hooks.

**Where the distributions come from.** On 2026-08-29 the live database of
one developer (2,564 events, 7 sessions, 1 project, macOS, Claude Code with
heavy subagent use, 45% of events reconstructed from transcripts) was
queried with `attempt query` for aggregates only: counts per kind, tool,
outcome and notification type, key names of `attrs` and `content`, length
percentiles of `attrs_json`, `content_json` and each content field,
`duration_ms` percentiles, inter-event gaps, and path depth and extension
counts. No prompt, command, path, or output text was copied; every string
the generator emits comes from word lists written for the benchmark, mixed
with seeded numbers and hex so that zstd sees roughly the entropy of real
traffic. `model.rs` records every table with its provenance (*sampled* or
*assumed*); the sample is small and skewed (one session holds 96% of it), so
the per-session and per-turn shapes are coarse.

**Shape.** Sessions → turns → tool-call pairs:

- Provider mix per session: Claude Code 70%, Codex 20%, Cursor 7%, Gemini
  CLI 3% (the requested public mix, not the sample's 99.9% Claude Code), plus
  1% single-event noise sessions (`capture_test` / `unknown`). Cursor reports
  only tool ends; Gemini and Cursor have no subagents; 45% of Claude Code
  sessions are transcript reconstructions (`transcript:*` names,
  `attrs.reconstructed`, no `raw`).
- Turns per session 1–12 (median 2); tool calls per turn 1–900 (median 60;
  the sampled five turns had p50 176 and max 857, all from one autonomous
  session, so the body of the table was lowered and the tail kept).
- 77% of tool calls run inside subagents, dispatched as an `Agent` tool
  call wrapping the subagent's calls and 1–20 `subagent_stopped` events.
- Tool mix over starts: shell 70.5%, edit 10.1%, read 8.9%, write 6.1%,
  subagent 2.2%, web 0.45%, other/search/MCP < 1%. Shell calls fail 2.0% of
  the time (`file_not_found` / `nonzero_exit`), edits 0.9%
  (`string_mismatch`), 0.1% of starts are denied.
- Content sizes per field are drawn from the sampled quantile tables
  (`SHELL_COMMAND`, `SHELL_OUTPUT`, `READ_OUTPUT`, `WRITE_INPUT`, …): for
  example shell output p50 1.2 KB / p90 13.6 KB / max 32 KB, file reads p50
  11.8 KB / p90 47 KB, prompts p50 32 B / p90 5 KB / max 11 KB. Hook-captured
  events also carry the provider payload in `raw` (the default
  `keep_raw_payload = true`), which is why a finished tool call is roughly
  twice its content size on the wire.
- Timing: tool durations by category (shell p50 40 ms / p90 6 s / p99 62 s),
  think time between calls from the sampled gap table (p50 0.7 s, p90 9.7 s,
  p99 80 s, capped at 10 min), session start gaps 20 s – 8 h (assumed), three
  sessions active at a time, synthetic time starting 2026-01-05T09:00:00Z.
- Paths: 12 projects with Zipf weights, 160 paths each, depth 4 for 91% of
  paths, extension mix rs 82% / txt 8.5% / jsonl 3.5% / py 2.5% / md 2%.
- One fixture session (`bench-chain-session`, placed at the midpoint of the
  stream) contains a turn with 200 sequential failing edits of one file, so
  the projection yields 200 attempts each superseded by the next — the
  `TRACE` subject.

**Calibration.** A 40,000-event sample of the generator averaged 11.7 KB of
JSON per event (sampled live database: 10.8 KB) and compressed 8.7:1 with
`zstd -3` as JSONL; the live database's segments compress about 5.7:1
against their JSON, so the synthetic text is somewhat more compressible than
real agent traffic. The generated kind mix is compared with the sampled mix
in the first results table.

### Reproduce

```sh
cargo build --release -p attemptdb-bench -p attempt
cargo run --release -p attemptdb-bench -- run --events 1450000 --out /tmp/attemptdb-bench --json
cargo run --release -p attemptdb-bench -- report /tmp/attemptdb-bench/results.json
```

`run` writes `results.json` after every step (so a killed run keeps its
partial results) and `results.md` at the end. Useful flags: `--events N`
scales everything (the curve sizes 10k / 100k / 500k are dropped when they
exceed N), `--only ingest,hook` / `--skip engine` select steps,
`--time-cap-secs`, `--rss-cap-gb`, `--seed`, `--attempt-bin`. `attemptdb-bench
sample --events 50` prints synthetic events as JSON lines, and
`attemptdb-bench step <name> …` runs one step in the current process.

The working directory needs roughly 3× the final database size in free
space (the Strict and Relaxed full ingests coexist briefly).

## Results

All tables are rendered from the raw results file by `attemptdb-bench report`; latencies are p50 / p95 / p99 over the stated sample counts.

### Generated kind mix versus the sampled mix

| Kind | Generated | Share | Sampled share |
|---|---|---|---|
| `tool_call_finished` | 673,966 | 46.5% | 46.6% |
| `tool_call_started` | 639,517 | 44.1% | 43.4% |
| `subagent_stopped` | 93,504 | 6.4% | 6.4% |
| `subagent_started` | 11,705 | 0.8% | 0.7% |
| `tool_call_failed` | 10,949 | 0.8% | 0.7% |
| `agent_message` | 6,585 | 0.5% | 0.7% |
| `turn_stopped` | 6,387 | 0.4% | 0.5% |
| `prompt_submitted` | 4,322 | 0.3% | 0.4% |
| `notification` | 1,769 | 0.1% | 0.4% |
| `permission_denied` | 681 | 0.0% | 0.0% |
| `session_started` | 446 | 0.0% | 0.0% |
| `session_ended` | 149 | 0.0% | — |
| `capture_test` | 10 | 0.0% | — |
| `unknown` | 10 | 0.0% | 0.2% |

> 1529 sessions were generated for the full run.

### Sustained ingest (batches of 100)

| Run | Durability | Events | Ingest events/s | Wall events/s | Batch p50 / p95 / p99 | Flushes | Segments on disk | Manifests on disk | Bytes/event (segments) | WAL→segment ratio | Peak RSS | Note |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `ingest_relaxed_10k` | relaxed | 10,000 | 11,455 | 8,620 | 922 µs / 58.41 ms / 60.56 ms | 14 | 22.79 MiB | 91.97 KiB | 2.33 KiB | 5.3× | 190.41 MiB |  |
| `ingest_strict_10k` | strict | 10,000 | 8,959 | 7,107 | 4.52 ms / 53.73 ms / 55.55 ms | 14 | 22.79 MiB | 91.97 KiB | 2.33 KiB | 5.3× | 192.66 MiB |  |
| `ingest_relaxed_100k` | relaxed | 100,000 | 12,644 | 9,544 | 897 µs / 56.96 ms / 61.08 ms | 127 | 200.59 MiB | 6.79 MiB | 2.05 KiB | 5.4× | 345.25 MiB |  |
| `ingest_strict_100k` | strict | 100,000 | 9,581 | 7,687 | 4.39 ms / 52.45 ms / 56.46 ms | 127 | 200.58 MiB | 6.79 MiB | 2.05 KiB | 5.4× | 338.52 MiB |  |
| `ingest_strict_200k` | strict | 200,000 | 8,841 | 7,154 | 4.93 ms / 56.52 ms / 60.65 ms | 259 | 414.30 MiB | 27.79 MiB | 2.12 KiB | 5.4× | 353.56 MiB |  |
| `ingest_relaxed_full` | relaxed | 1,450,000 | 11,881 | 9,101 | 983 µs / 58.79 ms / 68.58 ms | 1,853 | 2.91 GiB | 1.38 GiB | 2.11 KiB | 5.3× | 437.03 MiB |  |
| `ingest_strict_full` | strict | 1,450,000 | 8,592 | 7,014 | 4.94 ms / 58.55 ms / 66.83 ms | 1,853 | 2.91 GiB | 1.38 GiB | 2.11 KiB | 5.3× | 456.50 MiB |  |
| `ingest_strict_200k_reader` | strict | 200,000 | 9,241 | 7,266 | 4.50 ms / 53.75 ms / 57.58 ms | 259 | 414.29 MiB | 27.79 MiB | 2.12 KiB | 5.4× | 4.28 GiB | with concurrent reader |

### Concurrent reader during `ingest_strict_200k_reader`

| Reader metric | Value |
|---|---|
| Iterations (one per second when it keeps up) | 19 |
| Errors | 0 |
| Most events seen by one scan | 191,300 |
| Open (read-only, WAL replay) p50 / p95 / p99 | 10.18 ms / 16.17 ms / 16.17 ms |
| Scan all p50 / p95 / p99 | 855.04 ms / 2.45 s / 2.45 s |
| Project p50 / p95 / p99 | 144.11 ms / 737.92 ms / 737.92 ms |
| Open + scan + project p50 / p95 / p99 | 1.01 s / 3.20 s / 3.20 s |
| Ingest events/s without / with reader | 8,841 / 9,241 (+4.5%) |

### WAL acknowledgement latency

| Path | Samples | p50 / p95 / p99 | Max |
|---|---|---|---|
| `Database::ingest`, one event, Strict (F_FULLFSYNC per call) | 2000 | 3.01 ms / 3.97 ms / 4.11 ms | 6.95 ms |
| `Database::ingest`, one event, Relaxed (no sync) | 2000 | 9 µs / 45 µs / 76 µs | 472 µs |
| `SpoolWriter::append`, one event, sync off (hook default) | 2000 | 191 µs / 294 µs / 339 µs | 5.22 ms |
| `SpoolWriter::append`, one event, sync on | 2000 | 4.00 ms / 4.29 ms / 5.01 ms | 10.05 ms |
| 4 KiB append + `File::sync_data` (std → F_FULLFSYNC on macOS) | 500 | 3.09 ms / 4.06 ms / 4.88 ms | 8.36 ms |
| 4 KiB append + `fsync(2)` | 500 | 24 µs / 31 µs / 36 µs | 72 µs |
| 4 KiB append + `fcntl(F_FULLFSYNC)` | 500 | 3.00 ms / 3.89 ms / 4.05 ms | 26.28 ms |

> Single-event samples use synthetic `tool_call_finished` shell events averaging 15.79 KiB of JSON.

### Hook process wall clock

| Path | Spawns | Wall p50 / p95 / p99 | Wall max | In-process hook_us p50 / p95 / p99 | Events durable after |
|---|---|---|---|---|---|
| `/usr/bin/true` (fork+exec floor) | 200 | 778 µs / 922 µs / 979 µs | 1.51 ms | — | — |
| `attempt --version` (binary load floor) | 200 | 3.23 ms / 3.59 ms / 3.72 ms | 12.59 ms | — | — |
| `attempt hook claude-code`, no daemon (spool append) | 200 | 3.77 ms / 4.65 ms / 5.27 ms | 5.91 ms | 124 µs / 142 µs / 183 µs | 205 of 205 |
| `attempt hook claude-code`, daemon Strict (IPC + F_FULLFSYNC) | 200 | 7.03 ms / 8.00 ms / 8.07 ms | 8.26 ms | 131 µs / 157 µs / 163 µs | 205 of 205 |
| `attempt hook claude-code`, daemon `--relaxed` (IPC, no sync) | 200 | 3.62 ms / 4.03 ms / 4.15 ms | 4.68 ms | 126 µs / 146 µs / 153 µs | 205 of 205 |

> Binary: `~/attemptdb/target/release/attempt` (74.66 MiB); payload 2768 bytes.

### Recent timeline (last 24 h)

| Metric | Value |
|---|---|
| Events in the last 24 h of synthetic time | 22,608 |
| Segments in the database | 1854 |
| `QueryEngine::from_database` (scan + projection + tables) p50 / p95 / p99 | 561.48 ms / 587.80 ms / 587.80 ms |
| Projected sessions / turns / attempts in the window | 29 / 73 / 219 |
| `SHOW FAILED ATTEMPTS LIMIT 50` p50 / p95 / p99 (50 rows) | 490 µs / 659 µs / 5.83 ms |
| `SHOW ATTEMPTS LIMIT 50` p50 / p95 / p99 (50 rows) | 396 µs / 415 µs / 422 µs |
| `SHOW SESSIONS LIMIT 50` p50 / p95 / p99 (29 rows) | 277 µs / 332 µs / 397 µs |
| `SELECT count(*) FROM events` p50 / p95 / p99 (1 rows) | 98 µs / 119 µs / 1.16 ms |
| Peak RSS | 1.17 GiB |

### Full historical scan

| Step | Events | Wall | Rows/s | Peak RSS | Detail |
|---|---|---|---|---|---|
| `Database::scan` (all segments → `Vec<Event>`) | 1,450,000 | 20.47 s | 70,831 | shared below | 2.91 GiB of segments |
| `project()` over that `Vec<Event>` | 1,450,000 | 35.69 s | 40,627 | 20.82 GiB | 1529 sessions, 15342 attempts, 2059333 edges |
| `QueryEngine::from_database` (`engine_100k`) | 100,200 | 2.52 s | 39,816 | 2.72 GiB | prefix of the full database via `until` filter (re-encoded) |
| `QueryEngine::from_database` (`engine_200k`) | 200,300 | 5.50 s | 36,404 | 5.37 GiB | prefix of the full database via `until` filter (re-encoded) |
| `QueryEngine::from_database` (`engine_300k`) | 300,200 | 8.43 s | 35,615 | 8.04 GiB | prefix of the full database via `until` filter (re-encoded) |
| `QueryEngine::from_database` (`engine_400k`) | 400,000 | — | — | 9.78 GiB | did not complete (see note) |
| `QueryEngine::from_database` (`engine_500k`) | 500,000 | — | — | 11.43 GiB | did not complete (see note) |
| `QueryEngine::from_database` (`engine_full`) | 1,450,000 | 2 min 40 s | 9,090 | 21.12 GiB | whole database |

> `engine_400k` failed: exit Some(Some(101)): thread 'main' (70745573) panicked at ~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/arrow-array-59.2.0/src/builder/generic_bytes_builder.rs:87:57: | byte array offset overflow

> `engine_500k` failed: exit Some(Some(101)):  | thread 'main' (70698265) panicked at ~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/arrow-array-59.2.0/src/builder/generic_bytes_builder.rs:87:57: | byte array offset overflow | note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

### Queries over the loaded engine

| Engine | Statement | Rows | p50 / p95 / p99 |
|---|---|---|---|
| `engine_100k` | `SELECT provider, kind, count(*) AS n FROM events GROUP BY 1, 2 ORDER BY 3 DESC` | 32 | 1.45 ms / 3.11 ms / 3.11 ms |
| `engine_100k` | `SELECT count(*) FROM tool_calls` | 1 | 95 µs / 172 µs / 172 µs |
| `engine_100k` | `SELECT count(*) FROM events` | 1 | 104 µs / 112 µs / 112 µs |
| `engine_100k` | `SELECT count(*) FROM edges` | 1 | 84 µs / 89 µs / 89 µs |
| `engine_100k` | `SHOW FAILED ATTEMPTS LIMIT 50` | 50 | 842 µs / 1.81 ms / 1.81 ms |
| `engine_200k` | `SELECT provider, kind, count(*) AS n FROM events GROUP BY 1, 2 ORDER BY 3 DESC` | 38 | 2.80 ms / 4.09 ms / 4.09 ms |
| `engine_200k` | `SELECT count(*) FROM tool_calls` | 1 | 92 µs / 176 µs / 176 µs |
| `engine_200k` | `SELECT count(*) FROM events` | 1 | 97 µs / 136 µs / 136 µs |
| `engine_200k` | `SELECT count(*) FROM edges` | 1 | 78 µs / 83 µs / 83 µs |
| `engine_200k` | `SHOW FAILED ATTEMPTS LIMIT 50` | 50 | 1.23 ms / 3.44 ms / 3.44 ms |
| `engine_300k` | `SELECT provider, kind, count(*) AS n FROM events GROUP BY 1, 2 ORDER BY 3 DESC` | 39 | 3.92 ms / 5.31 ms / 5.31 ms |
| `engine_300k` | `SELECT count(*) FROM tool_calls` | 1 | 91 µs / 174 µs / 174 µs |
| `engine_300k` | `SELECT count(*) FROM events` | 1 | 103 µs / 153 µs / 153 µs |
| `engine_300k` | `SELECT count(*) FROM edges` | 1 | 81 µs / 91 µs / 91 µs |
| `engine_300k` | `SHOW FAILED ATTEMPTS LIMIT 50` | 50 | 1.60 ms / 4.37 ms / 4.37 ms |
| `engine_full` | `SELECT provider, kind, count(*) AS n FROM events GROUP BY 1, 2 ORDER BY 3 DESC` | 42 | 21.69 ms / 55.40 ms / 55.40 ms |
| `engine_full` | `SELECT count(*) FROM tool_calls` | 1 | 90 µs / 368 µs / 368 µs |
| `engine_full` | `SELECT count(*) FROM events` | 1 | 759 µs / 982 µs / 982 µs |
| `engine_full` | `SELECT count(*) FROM edges` | 1 | 85 µs / 135 µs / 135 µs |
| `engine_full` | `SHOW FAILED ATTEMPTS LIMIT 50` | 50 | 6.57 ms / 44.30 ms / 44.30 ms |

### Time travel and causal traversal

| Engine | Statement | Rows | p50 / p95 / p99 |
|---|---|---|---|
| `engine_100k` | `STATE project AT <ts>` × 10 points | [10,22,28,35,50,57,64,69,80,92] | 51.72 ms / 171.61 ms / 171.61 ms |
| `engine_200k` | `STATE project AT <ts>` × 10 points | [17,35,57,69,97,124,139,158,172,193] | 188.41 ms / 600.25 ms / 600.25 ms |
| `engine_300k` | `STATE project AT <ts>` × 10 points | [23,49,74,110,139,167,188,222,248,278] | 345.88 ms / 1.23 s / 1.23 s |
| `engine_full` | `STATE project AT <ts>` × 10 points | [64,186,328,476,621,772,920,1074,1207,1326] | 8.96 s / 36.02 s / 36.02 s |
| `engine_full` | `TRACE att_a9541a9e-8590-5d3c-9bed-a06a7214725a CAUSES DEPTH 10` | 30 | 187 µs / 322 µs / 322 µs |
| `trace_chain` (50,000 events, 200 chained attempts) | `TRACE … CAUSES DEPTH 1` | 3 | 8 µs / 10 µs / 58 µs |
| `trace_chain` (50,000 events, 200 chained attempts) | `TRACE … CAUSES DEPTH 10` | 30 | 20 µs / 20 µs / 31 µs |
| `trace_chain` (50,000 events, 200 chained attempts) | `TRACE … CAUSES DEPTH 50` | 150 | 70 µs / 73 µs / 90 µs |
| `trace_chain` (50,000 events, 200 chained attempts) | `TRACE … CAUSES DEPTH 200` | 599 | 271 µs / 274 µs / 316 µs |

### Projection cost versus event count

| Events | Mode | Generate | `project()` | Rows/s | Peak RSS | Sessions / attempts / edges |
|---|---|---|---|---|---|---|
| 10,000 | materialized | 0.30 s | 0.01 s | 846,770 | 241.61 MiB | 11 / 304 / 14875 |
| 10,000 | streaming | 0.29 s | 0.01 s | 920,131 | 133.11 MiB | 11 / 304 / 14875 |
| 100,000 | materialized | 2.50 s | 0.27 s | 367,173 | 1.82 GiB | 98 / 1199 / 141359 |
| 100,000 | streaming | 2.51 s | 0.27 s | 372,975 | 349.97 MiB | 98 / 1199 / 141359 |
| 500,000 | materialized | 12.37 s | 5.01 s | 99,758 | 7.82 GiB | 534 / 5394 / 713163 |
| 500,000 | streaming | 12.72 s | 4.38 s | 114,128 | 965.09 MiB | 534 / 5394 / 713163 |
| 1,450,000 | materialized | 36.14 s | 36.76 s | 39,441 | 18.49 GiB | 1529 / 15342 / 2066227 |
| 1,450,000 | streaming | 35.80 s | 34.49 s | 42,036 | 2.29 GiB | 1529 / 15342 / 2066227 |

### Size and compression by kind

| Profile | JSON B/event | JSONL+zstd(3) B/event | Arrow IPC plain B/event | Segment B/event | Segment ratio vs JSON |
|---|---|---|---|---|---|
| `tool_call_started/shell` | 7,104 | 655 | 6,616 | 1,162 | 6.1× |
| `tool_call_started/file_edit` | 3,723 | 419 | 3,310 | 697 | 5.3× |
| `tool_call_started/file_read` | 1,923 | 101 | 1,509 | 134 | 14.4× |
| `tool_call_started/file_write` | 35,142 | 4,958 | 34,726 | 9,456 | 3.7× |
| `tool_call_started/subagent` | 17,274 | 2,676 | 16,782 | 5,030 | 3.4× |
| `tool_call_started/web` | 1,804 | 129 | 1,314 | 175 | 10.3× |
| `tool_call_finished/shell` | 16,005 | 2,008 | 15,453 | 3,771 | 4.2× |
| `tool_call_finished/file_edit` | 16,295 | 2,156 | 15,832 | 4,006 | 4.1× |
| `tool_call_finished/file_read` | 39,117 | 5,511 | 38,653 | 10,513 | 3.7× |
| `tool_call_finished/file_write` | 45,061 | 6,382 | 44,596 | 12,181 | 3.7× |
| `tool_call_finished/subagent` | 22,482 | 3,521 | 21,941 | 6,631 | 3.4× |
| `tool_call_finished/web` | 6,991 | 1,002 | 6,449 | 1,795 | 3.9× |
| `tool_call_failed/shell` | 20,173 | 2,271 | 19,605 | 4,268 | 4.7× |
| `prompt_submitted` | 3,405 | 408 | 2,967 | 705 | 4.8× |
| `agent_message` | 6,460 | 1,751 | 6,040 | 1,735 | 3.7× |
| `subagent_stopped` | 2,009 | 135 | 1,479 | 204 | 9.8× |
| `turn_stopped` | 2,999 | 323 | 2,581 | 580 | 5.2× |
| `notification` | 1,521 | 49.05 | 1,091 | 82.28 | 18.5× |
| `session_started` | 1,331 | 51.86 | 909 | 61.56 | 21.6× |

> Weighted by the generated mix (98.3% coverage): 12.90 KiB of JSON and 2.99 KiB of segment per event, ratio 4.3×. The sampled live database: 10.55 KiB JSON → 1.84 KiB segment per event (5.7×).

### Where the bytes go

| Profile | Share of events | Share of segment bytes |
|---|---|---|
| `tool_call_finished/shell` | 31.9% | 39.9% |
| `tool_call_finished/file_read` | 4.1% | 14.2% |
| `tool_call_started/shell` | 32.2% | 12.4% |
| `tool_call_finished/file_write` | 2.7% | 11.1% |
| `tool_call_started/file_write` | 2.8% | 8.6% |
| `tool_call_finished/file_edit` | 4.6% | 6.1% |
| `tool_call_finished/subagent` | 1.1% | 2.5% |
| `tool_call_started/subagent` | 1.1% | 1.9% |
| `tool_call_started/file_edit` | 4.7% | 1.1% |
| `tool_call_failed/shell` | 0.7% | 1.0% |
| `subagent_stopped` | 6.7% | 0.5% |
| `agent_message` | 0.5% | 0.3% |
| `tool_call_started/file_read` | 4.1% | 0.2% |
| `tool_call_finished/web` | 0.2% | 0.1% |
| `turn_stopped` | 0.4% | 0.1% |
| `prompt_submitted` | 0.3% | 0.1% |
| `tool_call_started/web` | 0.2% | 0.0% |
| `notification` | 0.1% | 0.0% |
| `session_started` | 0.0% | 0.0% |

### Segment count versus read cost (no compaction)

| `flush_events` | Events | Segments | Segment bytes | Manifest bytes | Ingest events/s | Open p50 | Scan all p50 | Batches all p50 |
|---|---|---|---|---|---|---|---|---|
| 500 | 100,000 | 200 | 203.19 MiB | 16.25 MiB | 10,347 | 1.26 ms | 1.27 s | 518.35 ms |
| 5000 | 100,000 | 20 | 196.50 MiB | 212.57 KiB | 21,598 | 600 µs | 1.21 s | 456.86 ms |
| 50000 | 100,000 | 2 | 196.10 MiB | 4.99 KiB | 24,424 | 540 µs | 1.20 s | 453.61 ms |

### Step summary

| Step | Status | Wall | Peak RSS (getrusage) | Peak RSS (observed by parent) |
|---|---|---|---|---|
| `size_by_kind` | ok | 12.27 s | 1.44 GiB | 1.42 GiB |
| `wal_latency` | ok | 18.73 s | 63.72 MiB | 63.72 MiB |
| `hook` | ok | 4.16 s | 24.98 MiB | 24.84 MiB |
| `ingest_strict_10k` | ok | 1.56 s | 192.66 MiB | 187.02 MiB |
| `ingest_relaxed_10k` | ok | 1.31 s | 190.41 MiB | 189.44 MiB |
| `ingest_strict_100k` | ok | 13.25 s | 338.52 MiB | 335.53 MiB |
| `ingest_relaxed_100k` | ok | 10.71 s | 345.25 MiB | 342.28 MiB |
| `ingest_strict_full` | ok | 3 min 27 s | 456.50 MiB | 454.69 MiB |
| `ingest_relaxed_full` | ok | 2 min 40 s | 437.03 MiB | 437.03 MiB |
| `ingest_strict_200k` | ok | 28.15 s | 353.56 MiB | 353.56 MiB |
| `ingest_strict_200k_reader` | ok | 29.96 s | 4.28 GiB | 4.27 GiB |
| `segments_100k` | ok | 42.47 s | 4.57 GiB | 4.53 GiB |
| `projection_streaming_10k` | ok | 0.51 s | 133.11 MiB | 125.03 MiB |
| `projection_materialized_10k` | ok | 0.53 s | 241.61 MiB | 191.86 MiB |
| `projection_streaming_100k` | ok | 2.85 s | 349.97 MiB | 343.03 MiB |
| `projection_materialized_100k` | ok | 3.10 s | 1.82 GiB | 1.78 GiB |
| `projection_streaming_500k` | ok | 17.22 s | 965.09 MiB | 941.38 MiB |
| `projection_materialized_500k` | ok | 18.98 s | 7.82 GiB | 7.76 GiB |
| `projection_streaming_full` | ok | 1 min 10 s | 2.29 GiB | 2.29 GiB |
| `projection_materialized_full` | ok | 1 min 19 s | 18.49 GiB | 18.01 GiB |
| `recent_timeline` | ok | 3.12 s | 1.17 GiB | 1.17 GiB |
| `scan_project_full` | ok | 1 min 1 s | 20.82 GiB | 20.69 GiB |
| `engine_100k` | ok | 3.39 s | 2.72 GiB | 2.56 GiB |
| `engine_500k` | failed | 9.42 s | — | 11.43 GiB |
| `engine_full` | ok | 5 min 4 s | 21.12 GiB | 20.88 GiB |
| `engine_200k` | ok | 5.50 s | 5.37 GiB | 5.37 GiB |
| `engine_300k` | ok | 8.43 s | 8.04 GiB | 8.04 GiB |
| `engine_400k` | failed | 7.82 s | 9.78 GiB | 9.78 GiB |
| `trace_chain_2k` | ok | 0.51 s | 193.61 MiB | 153.69 MiB |
| `trace_chain` | ok | 2.13 s | 1.72 GiB | 1.72 GiB |

## Pathological and unflattering

> **2026-09-02.** Items 1, 3 and 7 below were the subject of a read-path
> rework (see `PROGRESS.md`, session 2026-09-02): the segment cache keeps
> Arrow only, the SQL layer and each of its tables are built on first use,
> content is read only for the rows and columns a reader asks for, the
> projection carries a per-session index, and the CLI reads through the
> daemon's resident engine. The numbers in this section are the 2026-08-29
> run and stand as the record of where the design started; the session
> entry has the after figures on the same shape of workload (200 k events:
> first read after a change 432 → 44–117 ms, resident memory 1,996 →
> ~800 MiB, `STATE … AT` from 188 ms to run noise).

The numbers above are what the current code does; this section says what
they mean. Nothing here is a projection or an estimate — each item points at
a row in the tables.

### 1. Every query over the whole history rebuilds the world in memory

`QueryEngine::from_database` over the 1.45 M-event database took **2 min 40 s
and 21.1 GiB** of resident memory before the first SQL statement ran, and the
CLI builds a fresh engine for every `attempt timeline`, `query`, `why`, and
`trace` invocation. The breakdown from the neighbouring steps: `Database::scan`
decodes every segment into a `Vec<Event>` in 20.5 s (70.8 k rows/s, ~11 KiB of
JSON parsed per event), `project()` over that vector takes 35.7 s, and the
pair peaks at 20.8 GiB; the engine then re-reads the segments as Arrow, builds
a "readable" copy of the events table, and registers thirteen tables. The
projection alone is not the memory problem — the streaming projector holds
1.45 M events in 2.3 GiB — the events are: the materialized projection runs
at 18.5 GiB because `Vec<Event>` is what the storage API returns; the engine
holds about 15 KiB of resident memory per event (21.1 GiB / 1.45 M).
Whole-history projection was expected to be the pathological case, and it
is: the fix (incremental projection, or a projector fed from segments without
materializing `Event`s) is design work, not tuning.

### 2. A scoped query over more than ~300 k events aborts the process

`engine_400k` and `engine_500k` did not fail gracefully; they panicked inside
Arrow with `byte array offset overflow`. Any filter — the default per-project
scope of the CLI, `--since`, `--session` — sends `QueryEngine::from_database`
down the path that re-encodes the filtered scan into **one** `RecordBatch`
(`events_to_batch`), whose `Utf8` columns use 32-bit offsets and therefore
cannot hold more than 2 GiB per column. At ~11 KiB of JSON per event the
`content_json` / `raw_json` column crosses that line somewhere between 300 k
and 400 k events (`engine_300k` built in 8.4 s at 8.0 GiB; `engine_400k`
aborted after 7.8 s at 9.8 GiB). The unfiltered path registers the segments'
own batches and does not overflow. In practice: a single project with more
than a few hundred thousand captured events cannot be queried from the CLI
at all today, and the failure mode is a crash rather than an error message.
This is a bug, not a limit; batches must be chunked (or `LargeUtf8` used) on
the filtered path.

### 3. `STATE project AT <ts>` grows with every session ever seen

Sessions rarely end — 10% of the synthetic sessions have a `session_ended`
(0 of 7 in the sample; hook `SessionEnd` is not reliably wired) — so the
snapshot treats almost every session as open and re-derives its state. At
100 k events the ten snapshots took 52 ms median; at 1.45 M events they took
**9.0 s median and 36 s at the latest timestamp**, returning 1,326 "open"
sessions. The cost is linear in sessions × their entities per query with no
index, and `DIFF STATE` runs it twice. Time travel is usable on a project, not
on a year of history.

### 4. The hook pays for a 74.7 MiB binary before it does any work

`attempt --version` — load the binary, parse arguments, exit — costs 3.2 ms
p50 on this machine against 0.78 ms for `/usr/bin/true`. The hook's own work
(parse the payload, resolve the repository, normalise, append to the spool)
is 124 µs p50 / 183 µs p99 in-process (`attrs.hook_us`), so roughly 85% of
the 3.8 ms p50 spool-path wall time is process start-up of a binary that
links DataFusion, tokio, and the MCP server the hook never uses. The "hook
path p95 below 10 ms" gate holds here (4.7 ms p95 without a daemon, 8.0 ms
with a Strict daemon, 4.0 ms with `--relaxed`), but on a slower disk or a
cold page cache the Strict daemon has little margin: every acknowledged
event costs one `F_FULLFSYNC` inside the daemon (see 5), which is why the
Strict daemon is *slower* for the hook than writing to the spool. The 205
hook invocations per mode never triggered a memtable flush; with a flush in
the way an acknowledgement waits for it (see 7).

### 5. Durable means `F_FULLFSYNC`, and on macOS that is 3 ms, not 30 µs

`fsync(2)` on APFS returned in 24 µs p50 and does not flush the drive's
cache; `fcntl(F_FULLFSYNC)` took 3.0 ms p50 / 4.0 ms p99 / 26 ms max. Rust's
`File::sync_data` and `sync_all` are `F_FULLFSYNC` on Apple platforms, so
Strict durability really is durable — and really costs 3 ms per commit.
Single-event Strict `ingest` is 3.0 ms p50 versus 9 µs Relaxed; batching 100
events narrows the gap to 8.6 k versus 11.9 k events/s, and the spool with
`sync = true` (not the default) would add 4 ms to every hook. The default
`spool_sync = false` is the right call for latency and the wrong one for a
power loss before the next import: events sit in the page cache for up to the
daemon's 5 s import interval.

### 6. Ingest is flush-bound, and there are ~1,850 segments after 1.45 M events

With ~11 KiB per event the 8 MiB byte threshold flushes the memtable every
~750 events, so `flush_events = 5000` never fires and the full database ends
up with **1,854 segments of ~1.6 MiB**. Each flush is a foreground stall on
the writer: encode 750 events to Arrow, zstd-compress, write and fsync the
segment, rewrite and fsync the manifest, rotate and fsync the WAL. That is
the p95 in every ingest row — **~55–60 ms at p95/p99 and 205 ms at worst for
a 100-event batch whose p50 is 4.9 ms** (Strict) or 0.98 ms (Relaxed). It also
caps throughput: at 100 k events Relaxed ingest ran at 12.6 k events/s with
the default thresholds, 21.6 k with 5,000-event flushes, and 24.4 k with
50,000-event flushes (byte threshold disabled). There is no compaction, so
segment count only grows: reading 200 small segments instead of 2 at 100 k
events costs 6% more on a full scan (1.27 s vs 1.20 s) and 2.3× on open
(1.26 ms vs 0.54 ms). (Compaction landed on 2026-08-30; the last section of
this document measures it on the same shape of database.)

The manifest makes it worse than the segment count suggests. Every flush
writes a new generation file listing **every** segment, and old generations
are never removed, so manifest bytes grow quadratically with flushes: the
full database has **1,855 generation files totalling 1.38 GiB** — the
latest alone is 1.6 MB — next to 2.91 GiB of segments, and the database is
4.30 GiB on disk rather than 2.91. The 100 k-event variants show the curve:
5 KB of manifests with 2 flushes, 218 KB with 20, 17 MB with 200. Garbage
collection of superseded generations (and a compaction that merges segments)
is the missing piece; until then a year of daemon uptime is a manifest
directory the size of the data.

### 7. Readers re-read everything, every time

The concurrent reader opened the database read-only once a second while
200 k events were ingested: each open replays the WAL (10 ms p50), each scan
decompresses and decodes every segment again (855 ms p50, 2.45 s max), then
projects (144 ms p50). There is no segment cache and no incremental
projection, so a "refresh every second" client costs O(database) per refresh
and 4.3 GiB of resident memory at 200 k events. Ingest throughput was not
hurt (+4.5%, within noise) because the reader only competes for CPU and page
cache on an 18-core machine.

### 8. The recent timeline is fast only because the window is small

The last 24 h of synthetic time held 22.6 k events; the engine over that
window built in 561 ms p50 and `SHOW FAILED ATTEMPTS LIMIT 50` answered in
0.5 ms. Segment pruning by `observed_at` does its job (1,854 segments, a
handful read). But the window is a filter, so it takes the re-encoding path
of item 2 — a busy day with 300 k+ events would abort.

### 9. Two copies of every string, 5.3× compression, 15.6 GiB written for 2.9 GiB kept (4.3 GiB with manifests)

Hook-captured events carry the provider payload in `raw` *and* the normalised
fields in `content`, so the same tool output is stored twice per event (the
default `keep_raw_payload = true`). On disk the full database is 2.11 KiB per
event for 11.5 KiB of JSON (5.3×; the sampled live database is 5.7×). File
reads and writes are 14% of events and 34% of bytes. Arrow's per-buffer zstd
leaves compression on the table: whole-file `zstd -3` of the same events as
JSONL is 2–3× smaller than the segment for every content-bearing kind
(shell results: 2.0 KiB vs 3.7 KiB per event). Write amplification: 15.6 GiB
of framed JSON went through the WAL to produce 2.9 GiB of segments, plus each
segment is written once more as a temp file before its rename, plus 1.4 GiB
of manifest generations (item 6).

### 10. Causal traversal is the one thing that is cheap

`TRACE … CAUSES DEPTH 10` over the 200-attempt supersession chain took 187 µs
p50 on the 2.08 M-edge graph of the full database and 20 µs on a 50 k-event
database; depth 200 (599 edges) took 271 µs. The graph is an in-memory
adjacency map built during engine construction — which is where the cost
went (items 1 and 2).

## Not run, capped, or deviating from the plan

- **Windows and Linux**: not run. Only macOS ARM64 numbers exist.
- **Compaction**: not implemented; item 6 reports segment counts and read
  cost instead of compaction impact on foreground ingest.
- **Encrypted content blobs (segment format 2)**: not exercised; every
  database here is format 1 with content inline.
- **`engine_400k` / `engine_500k`**: crashed (item 2). `engine_200k`,
  `engine_300k`, `engine_400k`, and `trace_chain` (50 k events) were run as
  separate `attemptdb-bench step` invocations minutes after the main run,
  same binary and database, and merged into the results file; the main run's
  own trace step used a 2 k-event database (`trace_chain_2k`). The plan in
  `main.rs` now includes those sizes.
- **`TRACE` on `engine_100k`–`engine_300k`**: not run — the chained fixture
  session sits at the midpoint of the stream, outside those prefixes.
- **Time cap**: no step reached the 600 s soft cap; `engine_full` was the
  longest at 5 min 4 s including its queries. No step reached the 36 GiB RSS
  cap.
- **Provider mix** is applied per session, so event shares differ from the
  70/20/7/3 target (Claude Code sessions are larger and include subagent
  stops; Cursor sessions have no start events): in the 40 k-event sample the
  event shares were 83 / 12 / 3 / 2.
- **Reconstructed events** are 45% of *Claude Code sessions*, i.e. about 29%
  of all events; the sample's 45% was of all events because it was all
  Claude Code.
- **Projection-curve caveat**: the `projection_*` steps project the
  generator's stream before ingestion (`hlc = 0`), which the projector orders
  by `(observed_at, captured_at, event_id)`; tool calls with 0 ms synthetic
  duration then sometimes sort their end before their start, so those runs
  report ~1% more tool calls (692,443) than the same events projected from
  the database (685,549). Cost is unaffected; correctness of the stored
  path is the one that matters.
- **Commit provenance**: the binaries were built from the working tree a few
  minutes before commit `6b97b5f` landed while other work was in flight; the
  run's own git lookup ran outside the repository and recorded nothing, so
  the hash was filled in from the git log afterwards.
- **Sample size**: the workload's per-session and per-turn shapes rest on
  seven sessions from one developer; the content-size, tool-mix, duration,
  and gap tables rest on 1–2 thousand events. The generator is public and
  seeded so that a better sample can replace `model.rs` without changing the
  benchmark.

## Refresh path (2026-08-30)

Item 7 above measured a polling reader re-decoding every segment and
re-projecting the whole history on every refresh. Two changes since then:
`ScanCache` keeps decoded segments across opens (a segment is immutable), and
`IncrementalProjector` re-finalises only the sessions new events touched, then
re-runs the cross-session stage (handoffs, work units, decisions, edges),
which is O(sessions), not O(events). The result is asserted equal to the
batch projector at every prefix, in any delivery order, with duplicates,
corrections and retractions.

`attemptdb-bench step refresh --events 200000 --relaxed`, same machine:

| | Old path (`from_database`) | Cached path |
|---|---|---|
| First load, 200 k events, 35 segments | 4.60 s | 4.41 s (decode 3.83 · project 0.14 · engine 0.44) |
| Reload after 1,000 events landed in the WAL | 5.59 s (from scratch) | **0.51 s** (refresh 0.002 · project 0.06 · engine 0.45; 0 segments decoded, 6 sessions rebuilt) |
| Reload after those events became a segment | 5.59 s | **0.50 s** (1 segment decoded, 0 sessions rebuilt) |

Eleven times faster on a reload, and the remaining half second is engine
construction, not decoding or projecting: building the projection's Arrow
tables (292 ms) and the readable `events` batches (124 ms), both of which are
still rebuilt from scratch per refresh. That is the next target.

Profiling the refresh path also found two quadratic loops in the **batch**
projector that every earlier number above paid for: `workunit::build`
deduplicated a unit's evidence with `Vec::contains` (a busy unit has tens of
thousands of evidence ids), and a turn's touched paths the same way. With
insertion-ordered sets, `workunit::build` on this database went from 700 ms
to 19 ms and the whole cold projection from 0.75 s to 0.14 s. The 1.45 M-event
projection figures in this document predate that fix and are now pessimistic.

The step also caught a latent engine defect: a flush of more than 4,096 rows
writes several Arrow batches into one IPC file, and a chunk whose
dictionary-encoded column (tool name, kind, …) saw values the first chunk did
not made the writer fail with "Dictionary replacement detected". No flush in
the earlier runs exceeded one batch (the 8 MiB byte threshold flushed every
~750 events), so it never surfaced; a busy server tenant with the default
20,000-row memtable would have hit it on its first big flush. Every chunk of a
segment now shares one dictionary per column; `tests/large_flush.rs` covers
it.

## Compaction (2026-08-30)

Item 6 above left a database of ~1,850 small segments with nothing to merge
them. `Database::compact` (`attempt compact`, `crates/attemptdb-storage/src/compaction.rs`)
now merges runs of small segments — below 8 MiB by default, at least four in
a row, only while the manifest lists more than 32 segments — into one
segment per run and one manifest generation per step. Rows and blob
references are copied verbatim (no key needed for encrypted segments, blobs
never rewritten), large segments are never touched, and the inputs are
tombstoned and deleted only after the next generation is durable.

`attemptdb-bench step compact --events 100000 --relaxed`, same machine, same
100 k-event workload flushed every 500 events as the 200-segment row of
"Segment count versus read cost" (`docs/benchmarks/2026-08-30-compact-macos-arm64.json`;
three opens per measurement, p50):

| | Segments | Segment bytes | Manifest bytes | Open p50 | Scan all p50 | Batches all p50 |
|---|---|---|---|---|---|---|
| Before | 200 | 203.29 MiB | 1.42 MiB | 974 µs | 1.35 s | 548 ms |
| After | 2 | 196.36 MiB | 1.15 MiB | **342 µs** | 1.29 s | 486 ms |

`Database::compact` took **3.91 s** for the one run: 200 inputs (203.29 MiB)
into one 196.34 MiB segment of 100,000 events — 25.6 k events/s, i.e. about
the speed of a relaxed ingest of the same events, because the merge decodes
every column, rebuilds the shared dictionaries, and zstd-compresses the
output once. The "after" row has two segments because the benchmark flushes
one more event to trigger the collection that deletes the 200 inputs (they
are gone: 2 files on disk).

What it buys: open is 2.85× faster (the manifest lists 2 entries instead of
200 and the tail of retained generations shrinks with every write), the full
scan 4.7 % and the Arrow batch path 12.7 % — the same ~6 % the 200-vs-2 row
above predicted, since scanning is dominated by decompressing and decoding
the same 100 k rows either way. Disk use drops 3.4 % (one zstd frame per
column per 4,096-row batch instead of per 500-row file). The larger effect
is on what grows with time rather than with events: every generation is one
entry instead of 200, and a reader's `ScanCache` decodes one new segment and
drops 200.

What it costs: the writer is busy for the duration of a run (one run per
`compact` call, so a daemon can interleave runs with flushes), and the inputs
occupy disk twice until the generation after the compaction lands — for a
daemon flushing every few minutes that is minutes; `attempt compact` says
so in its summary.

