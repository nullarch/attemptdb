# RFC 0002: Storage Engine

| | |
|---|---|
| **Status** | Draft |
| **Authors** | AttemptDB maintainers |
| **Created** | 2026-08-28 |
| **Related** | `docs/storage-format.md` (normative byte layouts), RFC 0001 (canonical event model), RFC 0005 (cross-platform runtime), RFC 0006 (privacy and sync), ADR 0001 (no SQLite core), ADR 0002 (Arrow + DataFusion) |
| **Implementation** | `crates/attemptdb-storage` (in progress; the crate is a stub at the time of writing and is being built to this document) |

## 1. Summary

AttemptDB owns its storage engine. It is a small hybrid of a **row-oriented
write-ahead log** for durability and recent writes, an **in-memory table** for
the events not yet flushed, **immutable columnar segments** (Arrow IPC files)
for history, and a **generation-numbered manifest** that names the current set
of files atomically. A read-only, single-file **snapshot container** (`.atdb`)
carries a manifest and its segments between machines.

The design goals, in priority order:

1. **Never lose an acknowledged event.** Acknowledgment depends only on the
   WAL fsync policy — never on indexing, projection, inference, sync, or UI.
2. **Never silently discard evidence.** Recovery truncates only a torn tail;
   corruption is quarantined, not deleted; repair is explicit.
3. **Byte-compatible across macOS, Windows, and Linux** on any CPU, with no
   dependency on writable mmap, mandatory locks, or POSIX-only rename
   semantics.
4. **Cheap ingest, fast recent-timeline reads, and vectorised historical
   scans** from the same files, through Apache Arrow and DataFusion.
5. **Honest about what it is.** The engine reuses Arrow IPC and CRC-32C rather
   than inventing encodings; the AttemptDB-specific parts are the event model,
   the ordering guarantees, the manifest protocol, and the agent semantics
   layered on top.

`docs/storage-format.md` is the byte-level specification. This RFC explains
the rationale, the lifecycle of an event through the engine, the durability
and recovery contracts, and the test matrix that proves them.

## 2. Why not an embedded database

ADR 0001 records the decision not to make SQLite (or RocksDB, or a pure Arrow
file library) the core. In short: a coding-agent event stream is append-only
with a single writer per device, is read mostly as time-ordered ranges and
column projections, must be exportable as a self-describing portable file, and
must have a durability story that a user can reason about from the file
listing alone. A row-store with B-trees and a page cache optimises for
different things, hides its durability behind an opaque file, and would make
"what exactly is on disk" a question about someone else's format. The pieces
we do need — a checksummed log, an immutable columnar file, and an atomic
manifest — are each small, well understood, and testable in isolation.

## 3. Directory layout and identity

```text
.attemptdb/
├── ATTEMPTDB      identity: format_version, schema_version, db_id, device_id, created_at
├── LOCK           advisory exclusive lock held by the single writer
├── wal/           NNNNNN.wal           framed append-only log
├── segments/      seg-<uuidv7>.arrow   immutable Arrow IPC files
├── manifest/      gen-NNNNNN.json      complete generations
├── spool/         *.spool              inbox from hook processes
└── blobs/         reserved             encrypted content-addressed blobs (planned)
```

Resolution of the database directory (identical in RFC 0005): `ATTEMPTDB_DIR`
if set; else a project-local `.attemptdb/` found by walking up from the
working directory; else `<data root>/db/.attemptdb` where the data root is
`--data-dir` > `ATTEMPTDB_DATA_DIR` > the per-OS data directory. A
project-local database keeps a repository's history next to the repository
(and `.gitignore` already excludes it); the per-user default is what
`attempt init` creates when no project database exists.

`db_id` identifies the logical database across copies; `device_id` identifies
the writer's device and is stamped into every ingested event (RFC 0001 §4).
The two are the same UUID for a freshly created database and diverge after a
snapshot is imported elsewhere.

## 4. Life of an event

```text
hook process                          daemon (single writer)
────────────                          ──────────────────────
adapter builds Event (source_seq=0)
  │
  ├─ IPC reachable ──► ingest_batch ──► assign source_seq, hlc, ingested_at
  │                                     append records to WAL
  │                                     fsync per policy
  │                    ◄──── ack ──────  insert into MemTable
  │                                     (later) flush MemTable → segment
  │                                     write manifest generation
  │                                     checkpoint + truncate WAL
  │
  └─ IPC unreachable ► spool/inbox.spool (locked append, exit)
                                        import on start / periodically
                                        → same path as ingest_batch
```

Ordering fields are assigned **once**, by the writer, in ingest order. The
writer is the only process that ever opens the WAL, MemTable, or manifest for
writing; everything else is a reader of immutable files or an IPC client.

## 5. Write-ahead log and durability

Frame format: `docs/storage-format.md` §5. Every record carries its length and
a CRC-32C over type, codec, flags, and payload; the payload is the JSON event
(ADR 0002 reserves a binary codec id for later).

**Group commit.** The writer drains its ingest queue into one WAL append
(many records, one `write` sequence) and then applies the fsync policy once
for the whole group. Clients are acknowledged together. This keeps the p95
acknowledgment latency near one `fsync` regardless of how many hooks fire at
once.

**Durability policy** (`durability` in the daemon config):

| Policy | Behaviour | Ack means |
|---|---|---|
| `strict` (default) | `fsync` (`fdatasync` where available, `FlushFileBuffers` on Windows) after every append group | The event survives process kill and power loss (subject to the disk honouring flush) |
| `relaxed` | `fdatasync` on a timer every *N* ms (default 200) | The event survives process kill; up to *N* ms may be lost on power loss |

Acknowledgment never depends on MemTable insertion, segment flush, indexes,
projections, inference, sync, or the UI. Those are all reconstructible from
the WAL and segments; the WAL is the only thing that must be durable at ack
time. Changing the policy cannot weaken the default: `strict` is what a fresh
install runs, and `relaxed` must be chosen explicitly.

**Recovery** scans each WAL file, validates records, and truncates the file at
the last good record (`storage-format.md` §5.4). Earlier valid records are
never discarded. Because the writer assigned `source_seq` before appending,
replay is idempotent: a record whose `source_seq ≤ manifest.last_source_seq`
is already in a segment and is skipped.

**Rotation** creates a new numbered file when the active file exceeds a size
or age threshold; the next manifest generation records the new active file.

## 6. MemTable and segments

The **MemTable** holds recent ingested events in memory in `source_seq` order,
with a hash index by `event_id` (for idempotent import) and by `session_id`
(for the "what is happening now" queries that dominate interactive use).
Reads combine the MemTable with segments; nothing in the MemTable is visible to
other processes except through the WAL — the daemon serves recent-timeline
queries over IPC, and a CLI opened without a daemon replays the WAL into its
own MemTable (read-only).

A **flush** writes the MemTable to a new immutable segment when any threshold
is crossed:

| Threshold | Default |
|---|---|
| Bytes (JSON size of buffered events) | 32 MiB |
| Event count | 50 000 |
| Elapsed time since the oldest buffered event | 5 minutes |
| Explicit | `attempt snapshot export`, daemon shutdown |

Flush sequence: write `segments/seg-<uuidv7>.arrow` to a temp name, `fsync`,
rename, compute SHA-256, write the manifest generation that adds the segment
and advances `wal.checkpoint_offset`, then append a checkpoint record to the
WAL. Only after the manifest is durable may the MemTable drop the flushed
events and may older WAL files be deleted. A crash between any two steps
leaves either the old manifest (segment file orphaned and ignored, then
garbage-collected) or the new one (WAL replay skips the flushed events).

**Segments** are Arrow IPC files (`storage-format.md` §8): one row per event,
columns for every canonical field with stable `attemptdb.field_id` metadata,
dictionary encoding for the highly repetitive columns (provider, capture mode,
event names, kind, project root and name, tool name and category, outcome
status), and zstd-compressed buffers so column projection never decompresses
what it does not read. `content_json` and `raw_json` are stored inline in
format version 1 and are null in `metadata_only`; format version 2 is planned
to move them into `blobs/` as encrypted content-addressed objects referenced
by hash (RFC 0006), which is why their field ids are named `CONTENT_REF` and
`RAW_REF`.

Rows are sorted by `(hlc, device_id, source_seq)` so every segment's manifest
entry can carry tight `min/max` statistics for time, HLC, and sequence, plus
the set of providers and project ids. Query planning prunes segments from
those statistics without opening files.

## 7. Manifest and atomic state

The manifest is the only mutable state, and it is mutated by writing a new
complete document, never by editing. `storage-format.md` §9 defines the
document, the canonical serialisation for its CRC-32C, and the
temp-fsync-rename-fsync protocol.

Why complete documents rather than a journal: a generation is small (one
entry per segment), recovery is "pick the newest generation whose checksum
verifies and whose files exist", and a torn write can only produce a file
that fails the checksum and is skipped. Keeping the last eight generations on
disk means the engine never depends on a single rename being atomic *and*
durable at the same instant — which matters on Windows, where a directory
cannot be fsynced.

**Tombstones before deletion.** A file that stops being referenced (after a
flush supersedes a WAL file, after compaction merges segments) is first listed
in `tombstones[]` with the generation that dropped it. It is physically
deleted only after the *next* generation is durable and no reader holds it
(in-process reference count; cross-process, a non-blocking exclusive lock
attempt; on Windows, an open file simply cannot be deleted and the deletion
is retried later). Readers that opened an older generation keep working until
they close.

**WAL truncation** follows the same rule: a WAL file is deleted only when
every record in it has `source_seq ≤ last_source_seq` of a durable generation
whose segments contain those events.

## 8. Compaction

Implemented (2026-08-30; `crates/attemptdb-storage/src/compaction.rs`,
`Database::compact`, `attempt compact`, `storage-format.md` §9.6). Small
segments produced by time-based flushes are merged into larger ones sorted
by the same key. Rules as built:

- Compaction never changes an `event_id`, `source_seq`, `hlc`, or any
  canonical field; rows are copied, and `content`/`raw` travel exactly as
  stored (inline text or blob ids), so blobs are never rewritten and no key
  is needed to compact encrypted segments. Evidence links from inferences
  (RFC 0003) therefore survive unchanged.
- Inputs are runs of consecutive *small* segments (below
  `small_segment_bytes`, default 8 MiB) of at least `min_inputs` (default 4),
  merged whole and oldest first, only while the manifest lists more than
  `max_segments` (default 32); a large segment ends a run and is never
  rewritten. With a current key the output is format 2 (inline content of
  older format 1 inputs is encrypted, as a flush would); without one a
  format boundary ends a run and each run keeps its format.
- Input segments are tombstoned, not deleted, in the generation that
  introduces the output segment; they are deleted by the collection that
  follows a *later* generation (§7), so a reader on the previous generation
  and a fallback from a torn newest generation both keep every file.
- One run per call, one generation per run: a writer loop calls
  `compact` until it returns nothing, and every step is individually
  crash-safe (failpoints `compact.after_segment_write`,
  `compact.after_manifest_write`, `compact.before_delete_inputs`; §12).
- Compaction runs on the single writer, so ingest through that writer waits
  while a run is merged (hooks never block: they spool). The daemon's writer
  loop is where periodic compaction belongs; the CLI refuses with a clear
  message when the daemon holds the lock.
- A crash between publishing the output and publishing the manifest leaves
  an unreferenced output file whose events all live in the inputs; open
  reports it and leaves it, and `attempt repair` quarantines it as a
  leftover rather than adopting it.
- Still planned: deletion requests (RFC 0006) as compaction with a filter,
  recorded in the manifest so `attempt verify` can explain gaps in
  `source_seq`; time-partitioned compaction of old history.

## 9. Indexes (planned)

Indexes are derived, rebuildable, and never required for correctness. The
planned set, from the master plan §5:

| Index | Purpose |
|---|---|
| Temporal | `(project_id, session_id, observed_at, source_seq)` for timeline range scans |
| Identity | event, span, turn, attempt, work unit, artifact id → location |
| Causal adjacency | parents and children of spans and edges |
| Reverse evidence | event id → every inference that cites it |
| Containment | session → turns → spans |
| Path and artifact | `path_logical`, `path_relative`, artifact locator → events |
| Provider / tool / kind | dictionary-backed bitmaps for filter pushdown |
| Full-text / token | over locally permitted content (`local_semantic` and above) |
| Redacted search | token index over metadata only, for `metadata_only` |

**Rebuild from immutable facts.** Every index is a pure function of the
segments and the WAL. `attempt verify` checks each index against a full scan;
`attempt repair --rebuild-indexes` deletes and recomputes them. Indexes live
under a separate `index/` directory (planned) that is not part of the manifest
and not part of snapshots, so a snapshot is always "facts only".

## 10. Snapshots and repair

`attempt snapshot export <file>.atdb` flushes the MemTable, writes a manifest
generation, and packs that manifest plus its segments into the single-file
container of `storage-format.md` §10. Each entry carries its own CRC-32C and
the footer carries a CRC-32C over the entry table, so a truncated or corrupted
download is detected before any query runs. `attempt snapshot open` mounts the
container read-only; opening is identical to opening a database whose WAL is
empty.

`attempt verify` checks: identity file, every manifest generation's checksum,
every segment's SHA-256 against its manifest entry, WAL record CRCs, spool
files, and (planned) indexes and blobs. `attempt repair` may truncate a WAL
tail, quarantine an unreadable file, or rebuild indexes; it never deletes a
readable event and always writes a report of what it changed.

## 11. Cross-platform rules

| Rule | Why |
|---|---|
| No writable mmap as a correctness dependency | Windows and macOS mmap semantics differ from Linux; a crash with dirty mapped pages is unrecoverable in general. Read-only mmap of segments is an optional optimisation. |
| File locking abstraction (`fs4`) | `flock`/`fcntl` on Unix and `LockFileEx` on Windows behave differently on close and across forks; the engine only ever asks for "exclusive, non-blocking, advisory". |
| Windows rename caveat | `rename` onto an existing name needs `MOVEFILE_REPLACE_EXISTING`; directory fsync does not exist; open files cannot be renamed or deleted. The retained-generation manifest and deferred deletion exist for this. |
| UTF-8 paths | Internal names are ASCII; user paths are `PortablePath`s; Windows long paths use `\\?\` at the OS boundary only. |
| Byte-compatible across OS and CPU | Everything is little-endian, fixed-width, and self-describing; an Arrow IPC file written on ARM64 macOS opens on x86_64 Windows. |
| No platform time types | Only `i64` microseconds are ever persisted. |
| Clock changes | `source_seq` and the HLC (RFC 0001 §5) make ordering immune to wall-clock regression; `observed_at` may be non-monotonic and that is recorded, not corrected. |

## 12. Failure-testing matrix

Each cell must be exercised by deterministic fault injection (a
`FailPoint`-style hook in the writer) and by real process kills, on every
Tier 1 OS, before RFC 0002 leaves Draft.

| Fault | Injected at | Expected outcome |
|---|---|---|
| Kill during WAL append | after header write, mid-payload, after payload before fsync | Torn tail truncated; all acknowledged (`strict`) events present; no earlier record lost |
| Kill during segment flush | after temp write, after rename before manifest, after manifest before checkpoint record | Either old manifest + orphan segment (GC'd) or new manifest; WAL replay yields identical results either way |
| Kill during manifest update | after temp write, after rename before directory fsync | Newest valid generation selected; a torn newest file is skipped and reported |
| Kill during compaction | after the merged segment is published, after the new generation is durable, before tombstoned inputs are deleted | Either the old generation (output unreferenced, left for `repair`) or the new one (inputs tombstoned, deleted after the next generation); identical events and ids either way; `verify` clean |
| Disk full | WAL append, segment write, manifest write | Append fails and is **not** acknowledged; hook falls back to spool; no partial manifest becomes current; daemon reports `ENOSPC` in `attempt status` |
| Quota / permission denied / read-only FS | open for write | Clear error; database opens read-only for queries |
| Corrupted WAL record | flipped byte in payload, in CRC, in length | Scan stops at the record; earlier records delivered; file truncated by the writer; `doctor` reports bytes dropped |
| Corrupted segment | flipped byte in body, truncated footer | SHA-256 mismatch in `verify`; reader quarantines file; queries report the gap rather than returning partial rows silently |
| Corrupted manifest | flipped byte, truncated file | Checksum fails; previous generation used; reported |
| Corrupted index / blob (planned) | any | Rebuilt from facts / reported as unavailable content |
| Concurrent readers + writer | readers open old generation during flush, compaction, tombstone GC | Readers complete; files they hold are not deleted; writer never blocks on readers |
| Second writer | open while `LOCK` held | Fails fast with a clear message |
| Daemon unavailable | hook cannot connect | Spool written and fsynced; imported on recovery; no duplicates by `event_id` |
| Old binary / old schema | newer `format_version` in identity file; newer `schema_version` in events | Refuses to write; reads what it can; unknown fields preserved |
| Snapshot exchange | `.atdb` created on each OS opened on each other OS | Identical logical query results |
| Sleep/wake, clock jump backwards | during ingest | HLC monotonic; `source_seq` contiguous; nothing reordered |

## Decisions

- AttemptDB owns a WAL + MemTable + immutable Arrow IPC segment + manifest
  engine; no embedded database is the core (ADR 0001).
- Acknowledgment depends only on the WAL fsync policy; `strict` is the
  default and `relaxed` is opt-in.
- WAL and spool files share one framed, CRC-32C-checked, little-endian record
  format with JSON payloads in format version 1.
- Recovery truncates only the torn tail of a log and never discards earlier
  valid records; corruption is quarantined, not deleted.
- Segments are Arrow IPC files with stable `attemptdb.field_id` column
  metadata, dictionary encoding for repetitive columns, and zstd buffer
  compression; rows are sorted by `(hlc, device_id, source_seq)`.
- The manifest is a sequence of complete, checksummed, generation-numbered
  JSON documents; recovery selects the newest valid one; the last eight are
  retained.
- Files are tombstoned before deletion and deleted only after the next
  durable generation and after readers release them.
- Compaction merges runs of small segments oldest first, one run per
  generation, copying rows and blob references verbatim; large segments are
  never rewritten and the policy (`max_segments`, `small_segment_bytes`,
  `min_inputs`) is the caller's.
- `content` and `raw` are inline columns in format version 1 and move to
  encrypted blobs in a later format version.
- Indexes are derived, live outside the manifest and snapshots, and are
  always rebuildable from facts.
- The `.atdb` snapshot is a single-file container of one manifest plus its
  segments, with per-entry and footer CRC-32C.

## Open questions

- Should `relaxed` mode also batch across IPC clients with a maximum group
  size, or only by time?
- Flush thresholds: are 32 MiB / 50 000 events / 5 minutes right for a
  typical interactive session, or should the time threshold be shorter so
  `attempt timeline` after a crash sees less WAL replay?
- Whether the CLI should replay the WAL itself when no daemon runs (fast, but
  duplicates MemTable logic) or refuse and start a daemon.
- Manifest retention count (8) and whether it should be size-based.
- Whether segment SHA-256 should be verified on every open of a snapshot
  (safer, slower) or only in `verify`.
- Compaction policy: the shipped one is size-tiered (small runs only);
  whether old history should also compact time-partitioned, into one segment
  per project per month, is open.
- Exact index file formats and whether DataFusion's own statistics can replace
  the provider/tool/kind bitmaps.
