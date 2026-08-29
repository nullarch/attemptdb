# ADR 0002: Apache Arrow and DataFusion as the substrate

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-08-28 |
| **Related** | ADR 0001, RFC 0002 (storage engine), RFC 0004 (AttemptQL), `docs/storage-format.md` |

## Context

Owning the storage engine (ADR 0001) does not mean owning a columnar memory
format, a vectorised executor, or a SQL planner. Those are solved problems
with mature Rust implementations, and reinventing them would cost
correctness and interoperability without buying any product value. The
master plan's operating premise is to reuse mature open standards where they
improve correctness, interoperability, or trust, and to implement
AttemptDB-specific semantics ourselves.

The AttemptDB-specific semantics are: the canonical event model and ids,
device-local sequencing and the hybrid logical clock, the WAL and manifest
protocols, the fact/inference separation with bitemporal validity, causal
edges and traversal, deterministic projections, AttemptQL, and the privacy
model. None of those are provided by a query engine.

## Decision

- **Apache Arrow** is the in-memory representation for all query results and
  the physical encoding of immutable segments (Arrow IPC file format with
  dictionary encoding and zstd buffer compression). Segment columns carry
  stable `attemptdb.field_id` metadata so the Arrow schema is a projection of
  the canonical schema, not a second source of truth.
- **Apache DataFusion** is the SQL parser, logical planner, optimiser, and
  vectorised physical executor. AttemptDB implements custom `TableProvider`s
  over its segments and MemTable with statistics-based pruning, custom
  logical and physical nodes for causal traversal and temporal state, and
  table functions that expose them to SQL. AttemptQL compiles to DataFusion
  logical plans (RFC 0004).
- **JSON is the format-version-1 WAL and spool payload codec** (`codec = 1`).
  It is self-describing, preserves unknown fields, is trivially debuggable
  with standard tools, and its cost is bounded by the WAL being a short-lived
  buffer in front of columnar segments. A codec id is **reserved** for a
  compact binary encoding keyed by the numeric field ids in `schema.rs`; it
  will be introduced by a format-version bump when WAL write volume or hook
  latency measurements justify it, not before.

## Consequences

- AttemptDB inherits DataFusion's SQL surface, optimiser, streaming
  execution, memory limits, and Arrow Flight/IPC transport options; it does
  not maintain a planner.
- Segments are readable by any Arrow implementation (Python, Java, C++),
  which makes the "exportable, not trapped" promise real beyond the `attempt`
  binary.
- Dependency weight: DataFusion and Arrow are large crates and dominate
  binary size and compile time. Features are trimmed (`default-features =
  false` with only the expression families the query layer needs) and this
  is accepted for a native binary that ships as a single file.
- Version coupling: the segment format is tied to the Arrow IPC
  specification, which is stable; the Rust crate versions can move without a
  format change.
- JSON payloads mean the WAL is larger and slower to parse than a binary
  encoding; recovery time is bounded by WAL size, which rotation and
  checkpointing keep small. The reserved codec id keeps the door open without
  freezing a binary layout prematurely.
- The stable numeric field ids exist precisely so that the future binary
  codec, the Arrow segment columns, and any C ABI share one identifier space.
