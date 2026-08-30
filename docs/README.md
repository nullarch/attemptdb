# AttemptDB documentation

> **The database for what agents tried.**

This directory is the public contract of AttemptDB: what the database
promises, how its files are laid out, how facts and inferences are kept
apart, how it is queried, how it runs on each operating system, and what it
does with your data. The project is pre-release; every document states what
is implemented and what is planned. Where a document and the code disagree,
the document wins and the code is a bug — unless the document says
"planned".

## Start here

| If you want to… | Read |
|---|---|
| Write an interoperable event producer | [RFC 0001](rfcs/0001-canonical-event-model.md), then [storage-format.md](storage-format.md) §5 |
| Understand what is on disk, byte by byte | [storage-format.md](storage-format.md) |
| Understand why the engine looks the way it does | [RFC 0002](rfcs/0002-storage-engine.md), [ADR 0001](adr/0001-no-sqlite-core.md), [ADR 0002](adr/0002-arrow-datafusion.md) |
| Know how "blocked" or "handed off" is decided | [RFC 0003](rfcs/0003-fact-inference-bitemporal-model.md) |
| Query the database | [RFC 0004](rfcs/0004-attemptql.md) |
| Install, run, or package it on macOS, Windows, or Linux | [RFC 0005](rfcs/0005-cross-platform-runtime.md) |
| Know what is captured, stored, and synced | [RFC 0006](rfcs/0006-privacy-and-sync.md), [SECURITY.md](../SECURITY.md) |
| Check whether your agent and version are supported | [compatibility-matrix.md](compatibility-matrix.md) |
| Contribute | [CONTRIBUTING.md](../CONTRIBUTING.md), [CODE_OF_CONDUCT.md](../CODE_OF_CONDUCT.md) |
| Replace VibeMon's legacy `notify.sh` hooks | [migration/vibemon-hooks.md](migration/vibemon-hooks.md) |

## RFCs

RFCs define behaviour that other implementations may depend on. Each has a
header block (status, authors, created date), a body, a **Decisions** list,
and an **Open questions** list. Status flow: Draft → Accepted → Implemented,
or Superseded.

| RFC | Title | Status | Normative code |
|---|---|---|---|
| [0001](rfcs/0001-canonical-event-model.md) | Canonical Event Model | Draft | `crates/attemptdb-core/src/{ids,clock,time,event,schema,paths,privacy,codec}.rs` |
| [0002](rfcs/0002-storage-engine.md) | Storage Engine | Draft | `crates/attemptdb-storage` (in progress) |
| [0003](rfcs/0003-fact-inference-bitemporal-model.md) | Facts, Inferences, and the Bitemporal Model | Draft | `crates/attemptdb-project` (in progress) |
| [0004](rfcs/0004-attemptql.md) | AttemptQL | Draft | `crates/attemptdb-query` (in progress) |
| [0005](rfcs/0005-cross-platform-runtime.md) | Cross-Platform Runtime | Draft | `crates/attemptdb-capture`, `crates/attempt` (in progress) |
| [0006](rfcs/0006-privacy-and-sync.md) | Privacy, Capture Modes, and Sync | Draft | `crates/attemptdb-core/src/privacy.rs` |

## Specifications

| Document | Purpose |
|---|---|
| [storage-format.md](storage-format.md) | Byte-level layout of the identity file, lock, WAL and spool frames, Arrow segments, manifest generations, and the `.atdb` snapshot container (format version 1) |
| [compatibility-matrix.md](compatibility-matrix.md) | Provider × event × verification level, and platform tiers |

## Architecture decision records

| ADR | Decision |
|---|---|
| [0001](adr/0001-no-sqlite-core.md) | SQLite is not the core storage engine |
| [0002](adr/0002-arrow-datafusion.md) | Apache Arrow and DataFusion are the substrate; JSON is the v1 WAL codec with a binary codec id reserved |

## Repository-level documents

| Document | Purpose |
|---|---|
| [../README.md](../README.md) | Thesis and project status |
| [../TODO.md](../TODO.md) | Master plan and milestone definitions |
| [../CONTRIBUTING.md](../CONTRIBUTING.md) | Development setup, adapter contract, fixtures rule, RFC process, sign-off |
| [../CODE_OF_CONDUCT.md](../CODE_OF_CONDUCT.md) | Contributor Covenant 2.1 |
| [../SECURITY.md](../SECURITY.md) | Reporting, protection goals and non-goals, disclosure timeline |
| [../LICENSE](../LICENSE) | Apache License 2.0 |
| `../.github/ISSUE_TEMPLATE/` | Bug report, feature request, adapter request |
| `../.github/PULL_REQUEST_TEMPLATE.md` | Pull request checklist |

## Vocabulary

| Term | Meaning | Defined in |
|---|---|---|
| Event | An immutable observed fact | RFC 0001 |
| Provider | A coding-agent product (`claude_code`, `codex`, `cursor`, `gemini_cli`) | RFC 0001 |
| `source_seq`, HLC | Per-device sequence and hybrid logical clock assigned by the single writer | RFC 0001 §5 |
| Capture mode | `metadata_only` / `local_semantic` / `full_sync` | RFC 0006 |
| Segment, manifest, WAL, spool | Storage engine components | RFC 0002, storage-format.md |
| `.attemptdb/`, `.atdb` | Live database directory; portable snapshot file | storage-format.md |
| Projection, inference, correction | Derived claims and their human amendments | RFC 0003 |
| Attempt, WorkUnit, handoff | Inferred units of agent work | RFC 0001 §8, RFC 0003 §5 |
| AttemptQL | The statement language over the public tables | RFC 0004 |
| Tier 1 platform | An OS/arch that passes every suite natively | compatibility-matrix.md |

## Conventions for editing these documents

- English; precise; implementation-oriented; no marketing language.
- Mark anything the code does not do yet as **planned**.
- Field names, enum values, constants, and derivation rules must match
  `crates/attemptdb-core` exactly; if they cannot, file the discrepancy as an
  open question in the RFC and an issue against the code.
- Byte layouts change only through a format-version bump and an RFC update.
- Never include real prompts, tool output, private paths, tokens, or email
  addresses in examples; use the synthetic values already in these documents.
