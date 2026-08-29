# AttemptDB Storage Format (format_version 1)

| | |
|---|---|
| **Status** | Draft — byte layouts are frozen only when RFC 0002 reaches Accepted |
| **Authors** | AttemptDB maintainers |
| **Created** | 2026-08-28 |
| **Related** | RFC 0002 (storage engine, rationale), RFC 0001 (canonical event model), `crates/attemptdb-core/src/codec.rs`, `crates/attemptdb-storage` |

This document is the normative byte-level specification of everything
AttemptDB writes to disk. RFC 0002 explains *why*; this document says *what*.
An implementation that follows this document can read and write a database
produced by any other conforming implementation on any operating system or CPU
architecture.

## 1. Conventions

- **All integers are little-endian.** `u16`, `u32`, `u64`, `i32`, `i64` are
  fixed-width. No platform-sized integers (`usize`) are ever persisted.
- **All text is UTF-8** without BOM. File names are UTF-8; on Windows they are
  converted to UTF-16 at the OS boundary only.
- **UUIDs are 16 raw bytes** in RFC 9562 order (the same bytes as
  `Uuid::as_bytes()`), never text, in binary structures. In JSON they are
  lowercase hyphenated text.
- **Timestamps are `i64` microseconds since the Unix epoch, UTC.**
- **Checksums:** CRC-32C (Castagnoli, polynomial `0x1EDC6F41`, reflected, the
  same function as `crc32c::crc32c` in Rust) for frame and manifest integrity;
  SHA-256 (hex, lowercase) for content identity of whole segment files and
  blobs.
- Structures are byte-packed; there is no alignment padding.
- Two version numbers are persisted everywhere: `format_version` (this
  document; currently `1`) and `schema_version` (the canonical event schema,
  RFC 0001; currently `1`). They change independently.
- Byte-layout diagrams use `offset  size  field` columns; sizes are in bytes.

## 2. Directory layout

A live database is a directory conventionally named `.attemptdb/`:

```text
.attemptdb/
├── ATTEMPTDB                 identity file (JSON)          required
├── LOCK                      advisory single-writer lock   created on open
├── wal/
│   ├── 000001.wal            framed append-only log
│   └── 000002.wal
├── segments/
│   └── seg-<uuidv7>.arrow    immutable Arrow IPC file
├── manifest/
│   ├── gen-000041.json       complete manifest generation
│   └── gen-000042.json
├── spool/
│   ├── inbox.spool           shared inbox appended by hook processes (under inbox.lock)
│   ├── inbox.spool.committed length of inbox.spool after the last successful append (u64 LE)
│   ├── inbox.lock            advisory lock taken for every append and every claim
│   └── claimed-<uuidv7>.spool an inbox taken over by the writer, deleted after import
└── blobs/                    reserved: encrypted content-addressed blobs (planned)
```

Resolution of *which* directory is the database (identical rule in RFC 0005):

1. `ATTEMPTDB_DIR` environment variable, if set — used verbatim.
2. Otherwise a project-local `.attemptdb/` found by walking up from the
   current working directory to the filesystem root.
3. Otherwise `<data root>/db/.attemptdb`, where the data root is
   `--data-dir` > `ATTEMPTDB_DATA_DIR` > the per-OS data directory
   (`~/Library/Application Support/AttemptDB`, `%LOCALAPPDATA%\AttemptDB`,
   `$XDG_DATA_HOME/attemptdb`).

File names inside the database are ASCII and fixed by this document. Numeric
file names (`000001.wal`, `gen-000041.json`) are zero-padded decimal, at least
six digits, growing when needed; readers parse them numerically.

## 3. `ATTEMPTDB` identity file

A JSON document, written once when the database is created, never rewritten
except by an explicit format upgrade:

```json
{
  "format_version": 1,
  "schema_version": 1,
  "db_id": "018f6a3c-5e0d-7b3a-9c4e-2f1d0a8b7c6d",
  "device_id": "018f6a3c-5e0d-7b3a-9c4e-2f1d0a8b7c6d",
  "created_at": 1787904000000000
}
```

| Member | Type | Meaning |
|---|---|---|
| `format_version` | integer | Highest format version any file in this directory may use. A reader whose supported version is lower must refuse to open the database for writing. |
| `schema_version` | integer | Canonical schema version in effect when the database was created; events may carry higher versions (RFC 0001 §10). |
| `db_id` | UUID text | UUIDv7 identity of this database. Survives copies; two directories with the same `db_id` are the same logical database. |
| `device_id` | UUID text | UUIDv7 identity of the device that owns the single writer. Written into every event ingested here. |
| `created_at` | integer | Microseconds. |

A directory without a valid `ATTEMPTDB` file is not a database and must not be
written to. Unknown members must be preserved if the file is ever rewritten.

The identity file is written **last** when a database is created (after the
directory skeleton and the first manifest generation), so a crash during
creation leaves a directory that `Database::exists` reports as "not a
database" and that `create` completes on the next attempt.

## 4. `LOCK`

An empty file on which the writer holds an **exclusive advisory lock** for as
long as it is open (`flock(LOCK_EX)` / `fcntl` on Unix, `LockFileEx` with
`LOCKFILE_EXCLUSIVE_LOCK` on Windows — abstracted by the `fs4` crate). Readers
do not lock it. A second writer must fail fast with a clear error, never queue.
The lock is advisory; the file's existence alone means nothing (stale files
after a crash are normal).

## 5. Frame format (WAL and spool files)

WAL files and spool files share one frame format. They differ only in the
magic, who writes them, and whether `source_seq`/`hlc` are assigned.

### 5.1 File header (32 bytes)

```text
offset  size  field
0       4     magic: "ATWL" (0x41 0x54 0x57 0x4C) for WAL, "ATSP" (0x41 0x54 0x53 0x50) for spool
4       2     format_version  u16 LE  = 1
6       2     schema_version  u16 LE  (canonical schema version the writer uses)
8       16    file_id         UUID bytes (UUIDv7, unique per file)
24      8     created_at      i64 LE  microseconds
```

### 5.2 Record (12-byte header + payload)

```text
offset  size  field
0       4     payload_len   u32 LE   length of payload in bytes
4       4     crc32c        u32 LE   CRC-32C over bytes [8, 12 + payload_len)
                                     i.e. record_type ‖ codec ‖ flags ‖ payload
8       1     record_type   u8       1 = event, 2 = checkpoint
9       1     codec         u8       1 = JSON UTF-8 (CodecId::Json)
10      2     flags         u16 LE   0 in format_version 1; readers reject non-zero
12      n     payload
```

Records follow each other with no padding. `payload_len` is bounded by an
implementation sanity limit of 64 MiB; a larger value is treated as corruption.
Other `record_type` or `codec` values are reserved; a reader that encounters
one in a file whose `format_version` it supports must treat the record as
corrupt (it cannot know the payload semantics).

### 5.3 Record payloads

**Event (`record_type = 1`)** — one canonical event encoded with the JSON codec
(RFC 0001 §11). In a WAL file the writer has already assigned `source_seq`,
`hlc`, and `ingested_at`; in a spool file `source_seq` and `hlc` are `0` and
`ingested_at` is absent.

**Checkpoint (`record_type = 2`)** — JSON, written by the writer only, marking
that everything before it in this WAL file is durable in segments:

```json
{ "last_source_seq": 41873, "last_hlc": 117172076552585216,
  "manifest_generation": 42, "written_at": 1787904000131877 }
```

Checkpoint records never appear in spool files.

### 5.4 Recovery scan

```text
open file; read 32-byte header; verify magic and format_version
good_end = 32
loop:
  if remaining < 12: stop (truncated header)
  read payload_len, crc, record_type, codec, flags
  if payload_len > 64 MiB or remaining < 12 + payload_len: stop (truncated payload)
  if flags != 0 or record_type ∉ {1,2} or codec ∉ {1}: stop (corrupt)
  if crc32c(bytes[good_end+8 .. good_end+12+payload_len]) != crc: stop (corrupt)
  deliver record; good_end += 12 + payload_len
truncate file to good_end (writer only; readers just ignore the tail)
```

The tail is truncated at the **last good record**; earlier valid records are
never discarded. The number of bytes dropped is logged and surfaced by
`attempt doctor`. A file **shorter than its 32-byte header** (a crash between
creating the file and writing the header) holds no records: the writer
re-initialises it, readers treat it as empty. A file whose header has the
wrong magic or version is reported as corrupt and left untouched (open
fails with `Corrupt`); quarantining to `<name>.corrupt` is planned for
`attempt repair`, not done automatically.

A **partial append** (for example `ENOSPC` after some bytes were written)
leaves a torn record at the end of the file. The appender must truncate that
partial record before appending again; otherwise every later record would be
unreachable to the recovery scan. The reference engine does this in
`FrameWriter::append` and reuses the batch's `source_seq`s, because nothing
of the failed batch is on disk.

## 6. WAL files

- Named `wal/NNNNNN.wal`, numbered from `000001`, strictly increasing.
- Only the writer appends. Appends happen in **groups**: all records of one
  ingest batch are written with a single `write` call sequence and then made
  durable according to the fsync policy (RFC 0002 §5). Acknowledgment to the
  hook or client happens only after that.
- A record is never modified after it is written.
- **Rotation:** on every memtable flush the writer creates `NNNNNN+1.wal`
  (header only, fsynced), begins appending there, and records the new active
  file in the next manifest generation. Size/age-based rotation between
  flushes is planned; the format does not depend on the rotation policy.
- **Truncation:** a WAL file may be deleted only when every event record in it
  has `source_seq ≤ last_source_seq` of a **durable** manifest generation
  (RFC 0002 §7). The active file is never deleted.

## 7. Spool files

The spool is the inbox that hook processes write when they cannot hand an
event to the daemon (or when no daemon runs). It is a **transport**, not the
durability boundary: acknowledgment to the user is defined by the WAL policy
(RFC 0002), never by the spool.

- One shared file `spool/inbox.spool` (magic `ATSP`); payloads are
  un-ingested events (`source_seq = 0`, `hlc = 0`). Concurrent hook processes
  serialise their appends with the advisory lock `spool/inbox.lock`, so
  frames never interleave.
- **Trusted-tail open.** After a successful append the hook writes the new
  file length to `spool/inbox.spool.committed` (8 bytes, u64 LE, written via
  temp file + rename). The next appender validates only the records after
  that offset instead of scanning the whole inbox; if the hint is missing, out
  of range, or does not land on a valid record boundary, the whole file is
  scanned. A wrong hint can therefore only cost time, never data. A torn tail
  (crashed hook) is truncated before appending.
- **fsync is optional** (`config.spool_sync`, default off). Without it the
  appended records survive a hook-process crash (they are in the page cache)
  but not a power loss before the next import; with it every append pays a
  full fsync. The WAL always fsyncs according to its own policy.
- **Claim.** The writer takes the lock, renames `inbox.spool` to
  `claimed-<uuidv7>.spool`, removes the `.committed` sidecar, and releases the
  lock; a hook that arrives afterwards starts a fresh inbox. Claimed files are
  read in name order (UUIDv7, i.e. claim order), their events are ingested
  (ordering fields assigned, WAL append + sync), and each claimed file is
  deleted only after its events are durable in the WAL. A crash between
  ingest and delete re-imports the file; ingestion is idempotent by
  `event_id`, so nothing duplicates.
- A claimed file with a torn tail is imported up to the last valid record and
  reported in the writer's warnings.

## 8. Segments

A segment is an immutable **Arrow IPC file** (the "file" variant with footer,
readable by any Arrow implementation) named `segments/seg-<uuidv7>.arrow`,
where the UUID is the `segment_id`.

### 8.1 Schema-level metadata

Every segment's Arrow schema carries these key/value metadata entries:

| Key | Value |
|---|---|
| `attemptdb.format_version` | `"1"` |
| `attemptdb.schema_version` | `"1"` (the highest `schema_version` among rows) |
| `attemptdb.segment_id` | hyphenated UUID text, equal to the file name component |

Readers ignore additional keys; writers may add `attemptdb.created_at`
(microseconds, decimal text) and `attemptdb.writer` (binary version).

### 8.2 Columns

Every field carries metadata `attemptdb.field_id` = decimal text of the
canonical field id (RFC 0001 §6.1). Columns that are projections of a
structured field additionally carry `attemptdb.derivation`. Column order in the
file is the order below; readers must select columns by name, not position.

| Column | Arrow type | Null | field_id | Notes |
|---|---|---|---|---|
| `event_id` | FixedSizeBinary(16) | no | 1 | |
| `schema_version` | UInt16 | no | 2 | |
| `device_id` | FixedSizeBinary(16) | no | 3 | |
| `source_seq` | UInt64 | no | 4 | |
| `hlc` | UInt64 | no | 5 | |
| `observed_at` | Timestamp(Microsecond, "UTC") | no | 6 | |
| `captured_at` | Timestamp(Microsecond, "UTC") | no | 7 | |
| `ingested_at` | Timestamp(Microsecond, "UTC") | no | 8 | always assigned in segments |
| `provider` | Dictionary(Int32, Utf8) | no | 20 | |
| `provider_version` | Utf8 | yes | 21 | |
| `adapter_version` | Utf8 | no | 22 | |
| `hook_version` | Utf8 | yes | 23 | |
| `capture_mode` | Dictionary(Int32, Utf8) | no | 24 | |
| `provider_event_name` | Dictionary(Int32, Utf8) | no | 25 | |
| `kind` | Dictionary(Int32, Utf8) | no | 40 | |
| `project_id` | FixedSizeBinary(16) | no | 41 | |
| `project_root` | Dictionary(Int32, Utf8) | no | 42 | |
| `project_name` | Dictionary(Int32, Utf8) | no | 43 | |
| `repo_remote` | Utf8 | yes | 44 | |
| `branch` | Utf8 | yes | 45 | |
| `head` | Utf8 | yes | 46 | |
| `session_id` | FixedSizeBinary(16) | no | 60 | |
| `provider_session_id` | Utf8 | no | 61 | |
| `provider_turn_id` | Utf8 | yes | 62 | |
| `span_id` | FixedSizeBinary(16) | yes | 63 | |
| `parent_span_id` | FixedSizeBinary(16) | yes | 64 | |
| `agent_id` | FixedSizeBinary(16) | yes | 80 | null when the nil UUID |
| `provider_agent_id` | Utf8 | yes | — | no field id assigned yet (RFC 0001 open question) |
| `agent_type` | Utf8 | yes | 81 | |
| `parent_agent_id` | FixedSizeBinary(16) | yes | 82 | |
| `model` | Utf8 | yes | 83 | |
| `tool_name` | Dictionary(Int32, Utf8) | yes | 100 | |
| `tool_category` | Dictionary(Int32, Utf8) | yes | 101 | |
| `tool_call_id` | Utf8 | yes | 102 | |
| `path_logical` | Utf8 | yes | 120 | derivation `first.logical` |
| `path_relative` | Utf8 | yes | 120 | derivation `first.repo_relative` |
| `paths_json` | Utf8 | no | 120 | derivation `all`; JSON array of `PortablePath`, `[]` when empty |
| `outcome_status` | Dictionary(Int32, Utf8) | yes | 130 | |
| `outcome_class` | Utf8 | yes | 131 | |
| `exit_code` | Int32 | yes | — | no field id assigned yet |
| `duration_ms` | UInt64 | yes | 140 | |
| `attrs_json` | Utf8 | no | 200 | JSON object, `{}` when empty |
| `content_json` | Utf8 | yes | 210 | JSON object; null in `metadata_only`; moves to `blobs/` in a later format version |
| `raw_json` | Utf8 | yes | 211 | JSON value; null in `metadata_only`; moves to `blobs/` in a later format version |
| `unknown_json` | Utf8 | yes | 250 | JSON object of preserved unknown top-level fields; null when empty |

Row order within a segment: `hlc` ascending, then `device_id` bytes ascending,
then `source_seq` ascending (the reader ordering rule of RFC 0001 §5.5).

### 8.3 Encoding

- Record batches of at most 8 192 rows (writer default; readers accept any).
- Dictionary-encoded columns use Int32 indices with one dictionary batch per
  column per file (dictionaries are not replaced mid-file). Dictionary
  *values* are emitted in first-seen order; a value's index is not stable
  across segments.
- IPC body buffer compression is **zstd** (Arrow `CompressionType::ZSTD`,
  LZ4-frame is not used). Each buffer is independently decompressible, which
  is what makes column projection and partial reads possible.
- The Arrow IPC footer contains the schema and block offsets; a segment whose
  footer is missing or whose magic is wrong is corrupt and is quarantined,
  never partially read.
- The whole file's SHA-256 is recorded in the manifest entry that references
  it. `attempt verify` recomputes it; ordinary opens do not.

## 9. Manifest

Each generation is one **complete** JSON document at
`manifest/gen-NNNNNN.json`. Generations are never edited; a new one is
written for every state change (segment flushed, WAL rotated, file
tombstoned, compaction).

### 9.1 Document

```json
{
  "format_version": 1,
  "generation": 42,
  "db_id": "018f6a3c-5e0d-7b3a-9c4e-2f1d0a8b7c6d",
  "device_id": "018f6a3c-5e0d-7b3a-9c4e-2f1d0a8b7c6d",
  "created_at": 1787904000131877,
  "last_hlc": 117172076552585216,
  "last_source_seq": 41873,
  "wal": { "active_file": "000002.wal", "checkpoint_offset": 1048608 },
  "segments": [
    {
      "segment_id": "01a04700-0000-7000-8000-000000000001",
      "file": "seg-01a04700-0000-7000-8000-000000000001.arrow",
      "rows": 8192,
      "bytes": 1310720,
      "min_observed_at": 1787900000000000,
      "max_observed_at": 1787903999999999,
      "min_hlc": 117171808117129216,
      "max_hlc": 117172076552519680,
      "min_source_seq": 33682,
      "max_source_seq": 41873,
      "providers": ["claude_code", "codex"],
      "project_ids": ["b2c07819-b40b-5dc6-9012-b8d1dc193b15"],
      "sha256": "9b2f0c1e6a4d3b8f7e5c2a1d0f9e8b7c6a5d4e3f2b1a0c9d8e7f6a5b4c3d2e1f"
    }
  ],
  "tombstones": [
    { "file": "segments/seg-01a04600-0000-7000-8000-000000000009.arrow", "since_generation": 41 }
  ],
  "checksum": 2891740412
}
```

| Member | Meaning |
|---|---|
| `generation` | Strictly increasing integer; equals the number in the file name. |
| `last_hlc`, `last_source_seq` | Highest values the writer has issued to any event that is durable in the WAL at the time of writing. The writer resumes its generators from these. |
| `wal.active_file` | WAL file the writer was appending to. |
| `wal.checkpoint_offset` | Byte offset in `active_file` before which all events are contained in `segments`. Replay starts here. |
| `segments[]` | Every live segment, in `min_hlc` order. Statistics enable pruning by time, HLC, sequence, provider, and project without opening the file. |
| `tombstones[]` | Files no longer referenced that may still be held open by readers. |
| `checksum` | CRC-32C of the canonical serialisation of this document with the `checksum` member absent. |

### 9.2 Canonical serialisation for the checksum

Compact JSON (no whitespace), members in exactly the order shown above,
`checksum` omitted, integers only (no floats, no exponents), strings escaped
per RFC 8259 minimal rules (`"`, `\`, control characters; non-ASCII kept as
UTF-8), arrays in the order stored. The writer computes the CRC-32C over those
bytes, then appends `checksum` as the last member and writes the document
(pretty-printed or compact — readers re-canonicalise by parsing, removing
`checksum`, and re-serialising). Unknown members are preserved in place and
included in the checksum input.

### 9.3 Write protocol

```text
1. serialise generation G+1 to manifest/gen-<G+1>.json.tmp
2. fsync the temp file
3. rename to manifest/gen-<G+1>.json          (atomic on all Tier 1 filesystems)
4. fsync the manifest directory               (POSIX; on Windows, skip — see §11)
5. only now: delete WAL files and tombstoned files whose retention rules allow it
```

The previous `K` generations (default 8) are retained so a torn or
half-written newest file never leaves the database without a valid manifest.

### 9.4 Recovery selection

```text
candidates = manifest/gen-*.json sorted by generation descending
for each candidate:
  parse; if parse fails → skip
  recompute checksum; if mismatch → skip
  for each segment: if file missing → skip candidate
  accept candidate as current
if none accepted: the database is unrecoverable without `attempt repair`
```

Skipped candidates are reported, not deleted. After selection the writer
replays **every** WAL file in number order from offset 32 and skips any event
whose `event_id` is already present in the selected generation's segments
(id-based deduplication, which also makes a WAL that was not yet truncated
after a flush harmless). `wal.checkpoint_offset` is reserved for a later
optimisation and is written as `0` by format version 1.

**Recovery after a rejected newest generation is not lossless by itself.**
If generation *N* was accepted, the WAL truncated, and generation *N* is
later found corrupt, the events that only *N*'s newest segment contained are
no longer in the WAL. The segment file still exists: open reports it as an
*unreferenced segment* (a warning naming the file), leaves it in place, and
`attempt repair` (planned) re-adopts such files into a new generation after
verifying them. Stale `*.tmp` files in `segments/` and `manifest/` are removed
by the writer on open, with a warning.

### 9.5 Tombstone deletion

A tombstoned file is physically deleted only when (a) the current durable
generation is greater than `since_generation`, and (b) no reader holds it: in
process, a reference count; across processes, the writer takes a
non-blocking exclusive lock on the file and skips deletion if that fails
(Windows additionally refuses to delete open files, which is treated as
"skip and retry later").

## 10. Portable snapshot container (`.atdb`, container version 1)

A single-file, read-only, point-in-time copy of a database: the manifest plus
every segment it references. No WAL, no spool, no lock.

```text
File header (32 bytes)
offset  size  field
0       4     magic "ATDB"
4       2     format_version   u16 LE  = 1
6       2     schema_version   u16 LE
8       16    snapshot_id      UUID bytes (UUIDv7)
24      8     created_at       i64 LE microseconds

Entry (repeated, entry_count times)
offset      size  field
0           2     name_len   u16 LE    (1 ≤ name_len ≤ 1024)
2           n     name       UTF-8, forward slashes, no leading slash
2+n         8     len        u64 LE
10+n        4     crc32c     u32 LE over the entry's bytes
14+n        len   bytes

Footer (12 bytes)
offset  size  field
0       4     entry_count  u32 LE
4       4     crc32c       u32 LE over the entry table
8       4     magic "ATDB"
```

The **entry table** is the concatenation, in file order, of every entry's
header fields (`name_len ‖ name ‖ len ‖ crc32c`) without the bytes. A reader
opens a snapshot by checking the trailing magic, reading `entry_count`,
scanning entries from offset 32 while accumulating the entry table, and
comparing the footer CRC.

Entries, in order:

1. `manifest.json` — a manifest document as in §9 with `wal` omitted and
   `tombstones` empty. The writer flushes the memtable to a final segment
   before exporting so the snapshot is complete.
2. `segments/<file>` for each segment in `manifest.json`, in manifest order.

Opening a snapshot (`attempt snapshot open`) is identical to opening a database
whose WAL is empty. Snapshots are byte-identical when created from the same
manifest generation on any OS. Encryption of snapshots is defined in RFC 0006
and adds a container version, not a new entry type.

## 11. Cross-platform rules

| Rule | Detail |
|---|---|
| No writable mmap dependency | Segments may be read through read-only mmap as an optimisation; correctness never depends on it. WAL and manifests use ordinary buffered I/O plus `fsync`/`FlushFileBuffers`. |
| File locking abstraction | `LOCK` and per-segment reader locks use `fs4` (advisory `flock`/`LockFileEx`). Never rely on mandatory locking. |
| Windows rename | `rename` onto an existing name uses `MoveFileExW(MOVEFILE_REPLACE_EXISTING \| MOVEFILE_WRITE_THROUGH)`; directory fsync is unavailable, so the retained-generations rule (§9.3) is what makes manifest recovery safe there. Open files cannot be renamed or deleted; all such operations are retried later rather than failed. |
| Paths | Directory and file names are ASCII; user paths inside events are UTF-8 `PortablePath`s (RFC 0001 §6.3). Long paths on Windows use the `\\?\` prefix at the OS boundary only. |
| Byte compatibility | Every structure here is little-endian and fixed-width; a database or snapshot written on any OS/CPU opens on any other with identical logical query results. |
| Time | The database never stores local time or time zones. |
| Case | File names are lowercase; the database is not expected to survive being placed on a case-folding filesystem with a differently-cased duplicate. |

## 12. Version table

| Artifact | Field | Value in this document |
|---|---|---|
| Identity file | `format_version` | 1 |
| WAL / spool header | `format_version` | 1 |
| WAL / spool record | `codec` | 1 (JSON) — a binary codec id is reserved for a later version (ADR 0002) |
| Segment metadata | `attemptdb.format_version` | 1 |
| Manifest | `format_version` | 1 |
| Snapshot header | `format_version` | 1 |

A reader supports format version *N* if it can read every artifact at
version ≤ *N*. Upgrading a database rewrites the identity file last, after
every other artifact has been migrated.
