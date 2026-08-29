# ADR 0001: SQLite is not the core storage engine

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-08-28 |
| **Related** | RFC 0002 (storage engine), `docs/storage-format.md`, ADR 0002 |

## Context

The fastest way to ship a local event store is to put events in SQLite (or
another embedded key-value or row store) and build the product on top. The
master plan rejects this for the core engine while explicitly allowing
"proven embedded storage" to be used *and documented* where it fits. The
reasons need to be recorded so that the question does not get re-litigated
every time the engine looks like more work than a schema migration.

What the workload actually is:

- Append-only, single writer per device, many short-lived hook processes that
  must not block on the database.
- Reads are time-ordered range scans and column projections (timeline,
  filters by project/provider/kind/path), plus causal traversal over derived
  edges.
- The on-disk format must be portable as a self-describing file that another
  implementation can read, byte-compatible across macOS, Windows, and Linux,
  and exportable as a single snapshot file.
- Durability must be explainable from the file listing: "which events are
  guaranteed to be on disk after this acknowledgment" has to be answerable
  without reading someone else's journal format.
- The database should be honest about what it is. A product called AttemptDB
  that is a SQLite schema would draw the correct criticism that it is "traces
  plus SQLite", and the plan lists "a wrapper that hides SQLite while claiming
  a from-scratch database" as a thing AttemptDB is not.

## Decision

AttemptDB owns its storage engine: a framed, checksummed write-ahead log; an
in-memory table for recent events; immutable columnar segments in Apache
Arrow IPC format; and a generation-numbered manifest with checksums and
tombstones. The byte layouts are specified in `docs/storage-format.md` and
the rationale in RFC 0002.

SQLite (or any other embedded database) may still be used for **auxiliary**,
rebuildable state where it is the best tool — for example a full-text index
over locally permitted content, or the hosted control plane's transactional
data (which the plan already assigns to Postgres). Any such use is documented,
lives outside the manifest and snapshots, and is never required to read the
facts.

## Consequences

- More code to own: WAL, recovery, segment writer, manifest protocol, and
  their fault-injection tests on three operating systems. This is accepted;
  each piece is small and independently testable, and the plan treats
  engineering capacity as unconstrained relative to correctness and
  credibility.
- Query execution is delegated to DataFusion over Arrow (ADR 0002) so the
  engine does not need its own planner or executor.
- The storage format is public and versioned; changing it requires an RFC
  and a format-version bump, not a migration script.
- Portability and durability claims are testable against the specification
  rather than against an opaque file.
- The HN-facing answer to "why not traces plus SQLite" is a concrete file
  format, a durability contract, and evidence-linked queries — not a naming
  argument.
