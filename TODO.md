# AttemptDB — Master TODO

> **The database for what agents tried.**

This document is the consolidated execution plan for AttemptDB, AgentTimeline,
and the VibeMon pivot discussed to date. It is the current source of truth for
scope, architecture, platform support, launch positioning, and business
conversion.

## 0. Operating premise

- [x] Treat implementation speed and engineering headcount as unconstrained.
- [x] Optimize for the best product and system structure, not the smallest
  amount of code.
- [x] Do not equate “more code” with “better architecture.” Reuse mature open
  standards where they improve correctness, interoperability, or trust.
- [x] Treat business coherence, credibility, privacy, adoption, and conversion
  as the actual constraints.
- [x] Build a real runnable project for HN, not a landing page or design-only
  announcement.
- [x] Preserve VibeMon's existing users, collection reliability, and privacy
  trust throughout the pivot.

## 1. Final strategic decisions

### Brand and product structure

- [x] Open-source database name: **AttemptDB**.
- [x] Tagline: **The database for what agents tried.**
- [x] Core contrast: **Git records what changed. AttemptDB records what agents
  attempted.**
- [x] Local folder and planned GitHub repository name: `attemptdb`.
- [x] Human-facing timeline experience: **AgentTimeline**.
- [x] Hosted sync, mobile, alerts, and team product: **VibeMon**.
- [x] Use AttemptDB as the technical acquisition object and AgentTimeline as
  the product narrative that introduces the new VibeMon direction.
- [x] Do not lead the HN launch with VibeMon's existing gamification or slime
  identity; introduce VibeMon later as the optional hosted/mobile companion.

```text
AttemptDB
├── AttemptQL       query language and temporal/causal operators
├── AgentTimeline   local human-facing explorer and shareable timeline
└── VibeMon         optional hosted sync, mobile, Needs You, and teams
```

### Technical direction

- [x] Do not make SQLite the core AttemptDB storage engine.
- [x] Use Rust for the cross-platform core, daemon, CLI, hook entrypoint, and
  embedded library.
- [x] Implement an AttemptDB-owned hybrid storage engine:
  row-oriented WAL and recent writes, immutable columnar segments for history.
- [x] Use Apache Arrow as the in-memory and interoperability format.
- [x] Use Apache DataFusion as the general SQL planning and vectorized execution
  substrate.
- [x] Implement AttemptDB-specific storage, AttemptQL, temporal projection,
  causal traversal, inference versioning, and agent semantics ourselves.
- [x] Support macOS, Windows, and Linux as Tier 1 platforms from the first public
  release.
- [x] Keep the local database authoritative; cloud sync must be optional and
  explicit.
- [x] Separate observed facts from inferred work state at every layer.
- [x] Make every user-visible inference traceable to immutable evidence.

### Launch direction

- [x] Recommended HN title:

  ```text
  Show HN: AttemptDB – a database for what AI coding agents tried
  ```

- [x] Build the recursive demo: AttemptDB must contain the real history of the
  agents that built AttemptDB.
- [x] Launch with a runnable local product requiring no signup or email.
- [x] Use the existing experience of handling more than 1.45 million
  metadata-only VibeMon events as the origin story, subject to final approval
  that this aggregate can be published.
- [x] Judge the launch by activated databases and VibeMon conversion, not page
  views alone.

## 2. North-star user experience

A user installs one native binary, enables the adapters for the agents present
on the machine, works normally, and can then ask:

```text
What is this project trying to finish?
Why is it blocked?
Which approaches already failed?
Why did the agent change this file?
What was the project state yesterday?
What did Claude hand off to Codex?
What needs me right now?
Can the next agent continue without a new explanation?
```

The ideal first-use flow is:

```text
install
  → attempt init
  → attempt hook install
  → work normally with an existing coding agent
  → attempt timeline
  → inspect one real Attempt and its evidence
  → optionally enable VibeMon sync/mobile
```

### Required command surface

- [x] `attempt init`
- [x] `attempt hook install`
- [x] `attempt hook status`
- [x] `attempt doctor`
- [x] `attempt daemon`
- [x] `attempt status`
- [x] `attempt timeline`
- [x] `attempt query`
- [x] `attempt why`
- [x] `attempt trace`
- [x] `attempt failures`
- [x] `attempt handoffs`
- [x] `attempt snapshot export`
- [x] `attempt snapshot open`
- [x] `attempt ui`
- [x] `attempt mcp`
- [x] `attempt update`
- [x] `attempt uninstall`

## 3. Product boundaries

### AttemptDB is

- [x] A local-first temporal and causal database for agent work history.
- [x] An append-only evidence store with versioned projections.
- [x] A cross-provider event model for coding agents.
- [x] A record of successful, failed, abandoned, and superseded attempts.
- [x] A queryable bridge between prompts, tool calls, files, tests, commits,
  artifacts, decisions, and human intervention.
- [x] A foundation for continuity between agents and sessions.

### AttemptDB is not

- [x] A generic vector database for preferences or chat memories.
- [x] A renamed LLM tracing dashboard.
- [x] A replacement for Git.
- [x] A manager surveillance product.
- [x] A claim that inferred intent is ground truth.
- [x] A reason to upload prompts, source code, or tool output by default.
- [x] A wrapper that hides SQLite while claiming a from-scratch database.
- [x] A second independent business competing with VibeMon for direction.

## 4. Canonical data model

### Identity and hierarchy

- [ ] Define stable IDs for:
  - [ ] tenant
  - [ ] user
  - [ ] device
  - [ ] project
  - [ ] repository
  - [ ] work thread
  - [ ] session
  - [ ] turn
  - [ ] span
  - [ ] event
  - [ ] agent
  - [ ] subagent
  - [ ] tool call
  - [ ] attempt
  - [ ] work unit
  - [ ] decision
  - [ ] artifact
  - [x] inference
  - [ ] correction
- [x] Choose the sortable ID representation, with UUIDv7 as the default
  candidate.
- [ ] Add device-local monotonically increasing source sequence numbers.
- [ ] Define a hybrid logical clock or equivalent causal ordering mechanism.
- [x] Store both `observed_at` and `ingested_at`.
- [x] Store original provider IDs alongside canonical IDs.

### Core primitives

- [ ] Specify `Event` as an immutable observed fact.
- [ ] Specify `Span` as a bounded execution range with causal parents and
  children.
- [ ] Specify `Attempt` as one approach toward an objective, regardless of
  outcome.
- [ ] Specify `WorkUnit` as a versioned inferred unit of project work.
- [ ] Specify `Decision` with selected option, alternatives, evidence, and
  outcome.
- [ ] Specify `Artifact` for file, commit, PR, test, document, image, URL, and
  other outputs.
- [ ] Specify `Inference` with confidence, evidence IDs, algorithm/model
  version, prompt hash, validity interval, and supersession.
- [x] Specify `Correction` as a first-class human event rather than an in-place
  edit.
- [ ] Specify causal edge types:
  - [ ] `parent_of`
  - [ ] `caused`
  - [ ] `triggered`
  - [ ] `blocked`
  - [ ] `resolved`
  - [ ] `superseded`
  - [ ] `produced`
  - [ ] `verified`
  - [ ] `contradicted`
  - [ ] `handed_off`
  - [ ] `evidence_for`

### Work-unit state

- [x] Define phases:
  - [ ] `EXPLORE`
  - [ ] `PLAN`
  - [ ] `IMPLEMENT`
  - [ ] `DEBUG`
  - [ ] `VERIFY`
  - [ ] `REVIEW`
  - [ ] `DELIVER`
  - [ ] `BLOCKED`
- [x] Define status independently from phase.
- [x] Do not expose fake numerical progress percentages.
- [x] Record objective, phase, status, confidence, evidence, artifacts, actors,
  causal links, and updated time for each WorkUnit version.
- [ ] Define merge and split semantics for inferred WorkUnits.
- [x] Define how sessions from different agents join the same WorkUnit.

### Compatibility and standards

- [ ] Map canonical fields to OpenTelemetry GenAI semantic conventions where
  meaningful.
- [ ] Preserve provider-specific payloads or local references without forcing
  all provider details into the common schema.
- [ ] Define schema versioning independent of storage format versioning.
- [x] Publish JSON Schema or equivalent interchange specification
  (`spec/event-v1.schema.json`, drift-tested against the implementation and
  every golden fixture; `attempt conformance`).
- [x] Publish canonical event fixtures for every supported provider and version
  (`fixtures/providers/`: claude_code 70, codex 18, cursor 22, gemini_cli 16;
  version coverage tracked in `docs/compatibility-matrix.md`).
- [ ] Specify forward-compatible unknown-field preservation.

## 5. AttemptDB storage engine

### On-disk format

- [x] Write `docs/storage-format.md` before freezing binary layouts.
- [x] Define a platform-neutral little-endian encoding.
- [ ] Store text as UTF-8 with documented normalization rules.
- [ ] Never persist native Rust struct layouts, pointers, OS handles, or
  platform-sized integers.
- [x] Give every frame and segment a format version, schema version, length,
  and checksum.
- [ ] Choose checksums for frame corruption and content identity.
- [x] Add stable numeric field/type IDs.
- [x] Preserve unknown fields across read/export/import cycles.
- [ ] Define portable path fields:
  - [ ] original path
  - [ ] normalized logical path
  - [ ] repository-relative path
  - [ ] Windows drive/UNC metadata
- [x] Define `.attemptdb/` as the live database directory.
- [x] Define `.atdb` as the portable read-only snapshot/archive format.

### WAL and recovery

- [x] Design framed append-only WAL records.
- [ ] Make acknowledgment dependent only on durable WAL policy, not inference
  or indexing.
- [ ] Support configurable durability without weakening the default.
- [x] Implement group commit.
- [x] Implement checksummed recovery from partial tail records.
- [x] Implement WAL rotation.
- [ ] Implement safe log truncation after segment durability is proven.
- [x] Verify recovery after process kill, machine restart, and partial write.
- [x] Verify behavior when the disk is full.

### MemTable and immutable segments

- [x] Implement a write-optimized in-memory table for recent events.
- [ ] Define flush thresholds by bytes, event count, and elapsed time.
- [x] Encode immutable historical segments in an Arrow-compatible columnar
  layout.
- [x] Add dictionary encoding for provider, agent, event type, tool, path, and
  other repeated values.
- [ ] Add compression with independently readable blocks.
- [ ] Add min/max statistics and bloom filters where useful.
- [ ] Add predicate and projection pushdown metadata.
- [ ] Implement background compaction without blocking ingestion.
- [ ] Retain event IDs and evidence links through compaction.
- [ ] Ensure old readers can continue reading segments during compaction.

### Manifest and atomic state

- [ ] Do not depend solely on Unix-style overwrite-and-rename behavior.
- [ ] Implement an append-only manifest journal or double-buffered manifests.
- [x] Add generation numbers and checksums.
- [x] Select the newest fully valid generation during recovery.
- [ ] Tombstone obsolete files before deleting them.
- [ ] Delay physical deletion until active readers release references.
- [ ] Abstract file locking for macOS, Windows, and Linux.
- [ ] Avoid writable mmap as a correctness dependency.
- [ ] Test database open and recovery across all supported operating
  systems. **The crash (25), repair (18) and daemon (4) suites are
  `#![cfg(unix)]`, so a green Windows CI job proves the code compiles and
  the unit tests pass there — not that recovery works.**

### Indexes

- [ ] Temporal index by project, session, observed time, and source sequence.
- [ ] Identity index by event, span, turn, attempt, WorkUnit, and artifact ID.
- [ ] Causal adjacency index for parents and children.
- [ ] Reverse evidence index from event to all derived inferences.
- [ ] Session/turn/span containment indexes.
- [ ] Path and artifact indexes.
- [ ] Provider/tool/event-type indexes.
- [ ] Full-text or token index for locally permitted content.
- [ ] Redacted search index for metadata-only mode.
- [ ] Define index rebuild procedures from immutable events.

### Blob storage and encryption

- [x] Store large prompts, messages, tool outputs, diffs, and artifacts outside
  primary event segments.
- [x] Use content-addressed encrypted blobs.
- [x] Deduplicate identical content without leaking plaintext hashes externally.
- [x] Authenticate every encrypted blob.
- [x] Bind blob references to tenant/device/project scope.
- [x] Implement secure deletion limitations honestly.
- [x] Implement snapshot export with a portable encryption key.
- [x] Implement re-keying and key rotation.

### Snapshots, backup, and repair

- [x] Implement consistent point-in-time snapshots.
- [x] Implement snapshot verification before export succeeds.
- [x] Implement `attempt verify` for manifests, WAL, segments, indexes, and blobs.
- [x] Implement `attempt repair` without silently discarding evidence.
- [ ] Implement backup/restore documentation and tests.
- [ ] Verify `.atdb` snapshot exchange between macOS, Windows, and Linux.

## 6. Query engine and AttemptQL

### General query layer

- [x] Embed DataFusion as the physical/vectorized query engine.
- [x] Expose normal SQL for events, spans, attempts, work units, decisions,
  artifacts, inferences, corrections, and causal edges.
- [ ] Implement custom DataFusion table providers for AttemptDB segments.
- [ ] Implement predicate, time-range, project, and provider pushdown.
- [ ] Implement custom logical and physical nodes for temporal and causal
  operators.
- [ ] Add `EXPLAIN` support showing scanned segments and indexes. (plan output exists; segment/index listing pending)
- [ ] Add query cancellation, memory limits, and timeouts.
- [ ] Stream large results rather than loading them entirely into memory.

### AttemptQL commands

- [x] `WHAT IS <subject> DOING NOW`
- [x] `WHY <subject> STATUS <state>`
- [x] `TRACE <subject> CAUSES`
- [x] `STATE <subject> AT <timestamp>`
- [x] `SHOW ATTEMPTS`
- [x] `SHOW FAILED ATTEMPTS`
- [x] `SHOW SUPERSEDED ATTEMPTS`
- [x] `SHOW HANDOFFS`
- [x] `SHOW DECISIONS`
- [x] `SHOW EVIDENCE FOR <inference>`
- [x] `DIFF STATE <time-a> <time-b>`
- [ ] Define AttemptQL grammar and error messages.
- [ ] Compile AttemptQL into the shared logical plan representation.
- [x] Make every `WHY` answer return evidence and uncertainty, not prose alone.
- [ ] Publish query examples against the self-hosted AttemptDB build history.

### Public interfaces

- [ ] Stable Rust crate API.
- [ ] Stable C ABI for other language bindings.
- [ ] Local HTTP API.
- [x] MCP server in the `attempt` binary.
- [ ] Arrow IPC or Arrow Flight result transport.
- [ ] Node.js SDK.
- [ ] Python SDK.
- [ ] Go SDK.
- [ ] Event ingestion SDK and schema validators.
- [ ] Backward-compatible API versioning policy.

## 7. Capture daemon and provider adapters

### One native binary

- [x] Make `attempt` the only required executable.
- [x] Do not require Bash, Python, Node.js, Docker, or a cloud account.
- [x] Support foreground, per-user daemon, CLI client, hook entrypoint, MCP, and
  UI server modes from the same binary.
- [x] Keep hook startup overhead small and measurable.

### IPC and offline durability

- [x] macOS/Linux default IPC: Unix domain socket.
- [x] Windows default IPC: Named Pipe.
- [ ] Cross-platform fallback: authenticated loopback TCP.
- [x] Use a framed, versioned local ingest protocol.
- [x] If the daemon is unavailable, append to a crash-safe per-process spool.
- [x] Import spool files when the daemon recovers.
- [x] Make ingestion idempotent by event ID and source sequence.
- [x] Never block the coding agent on inference, sync, or UI work.
- [ ] Measure capture completeness and gaps, not just accepted requests.

### Adapter contract

- [ ] Define provider adapter input and normalized output interfaces.
- [x] Record provider name, version, hook version, capture mode, and adapter
  version on every event.
- [ ] Preserve the original payload locally when the privacy mode permits it.
- [ ] Assign a coverage grade to every session.
- [x] Represent unsupported and missed events explicitly.
- [x] Maintain real-payload fixtures and golden normalized envelopes.
- [x] Maintain a provider/version compatibility matrix in the repository.

### Claude Code

- [ ] Audit all current official lifecycle and tool events.
- [ ] Capture prompt, progress/final message, permission, tool, failure,
  compaction, session, and subagent events where supported.
- [ ] Capture task/plan events where available.
- [ ] Handle async hooks without delaying tool execution.
- [ ] Preserve existing user hook configuration transactionally.

### Codex

- [x] Use the actual supported hooks configuration path and shape.
- [x] Detect and explain required `/hooks` trust approval.
- [ ] Capture SessionStart/SessionEnd.
- [ ] Capture UserPromptSubmit with turn IDs.
- [ ] Capture PreToolUse/PostToolUse for shell, file changes, MCP, local function
  tools, plan updates, and agent spawning where supported.
- [ ] Capture PermissionRequest.
- [ ] Capture SubagentStart/SubagentStop.
- [ ] Capture Stop and final assistant message.
- [ ] Support structured `codex exec --json` import.
- [ ] Grade interactive sessions separately when progress-message coverage is
  incomplete.

### Cursor

- [ ] Use only verified event names and payload shapes.
- [ ] Test prompt, edit, shell, failure, stop, and session events with real
  fixtures.
- [ ] Detect ghost or unsupported configuration entries during upgrades.
- [ ] Verify production capture rather than treating successful config writes
  as successful integration.

### Gemini CLI

- [ ] Verify hook and MCP configuration fields against current real payloads.
- [ ] Test all supported lifecycle, prompt, tool, and failure events.
- [ ] Preserve Gemini-specific event details in the local raw layer.

### Additional agents

- [ ] Define a documented adapter SDK.
- [ ] Add GitHub Copilot, OpenCode, Pi, and other coding agents only through the
  same contract and test suite.
- [ ] Allow community adapters without allowing them to bypass privacy and
  schema validation.

### Installer safety

- [x] Detect installed agents before creating any agent directories.
- [x] Parse and update JSON/TOML structurally, never with blind text replacement.
- [x] Lock, back up, and atomically update existing config.
- [ ] Preserve all unrelated user content byte-for-byte when possible.
- [x] Verify each installed adapter with a test event.
- [x] Implement idempotent install, upgrade, repair, and uninstall.
- [x] `attempt doctor` must report configured, trusted, active, stale, and
  unverified states separately.

## 8. Privacy and security

### Capture modes

- [x] Define `metadata_only`.
- [x] Define `local_semantic`: detailed local evidence, redacted/no content sync.
- [x] Define `full_sync`: explicit user or organization opt-in.
- [x] Make cloud content sync disabled by default.
- [ ] Require explicit consent before changing existing VibeMon users from the
  current metadata-only behavior.
- [ ] Support capture policy per repository and organization.
- [ ] Display the active capture mode in CLI and UI.

### Content safety

- [ ] Secret scanning before persistence where possible.
- [x] Secret scanning again before sync/export (`attemptdb-core::secrets`,
  ruleset `secrets-v1`: issuer-format tokens, private keys, JWTs; attrs values
  containing one are dropped at ingest, content is redacted on the device
  before `--send-content` upload, sanitised exports strip content entirely).
- [ ] Allowlist canonical metadata fields.
- [x] Add privacy canaries for prompts, source code, command bodies, tokens,
  stdout/stderr, tool output, email, home paths, and provider-specific leaks.
- [ ] Never place real private payloads in fixtures, screenshots, issue reports,
  or public self-hosting demos.
- [x] Generate sanitized demo datasets from explicit rules.
- [ ] Keep content-free mode fully functional, while representing its inference
  limits honestly.

### Key management

- [x] macOS: Keychain.
- [x] Windows: DPAPI/Credential Manager.
- [x] Linux desktop: Secret Service.
- [x] Linux headless: passphrase or explicit key file.
- [x] Provide portable encrypted snapshot export independent of OS key stores.
- [ ] Add key-loss and recovery documentation.

### Threat model

- [x] Publish `SECURITY.md`.
- [ ] Document protection goals and non-goals.
- [ ] Threat-model malicious tool output and prompt injection entering the log.
- [ ] Treat displayed event content as untrusted.
- [x] Escape HTML, terminal sequences, Markdown, URLs, and paths on output.
- [ ] Bind local APIs to loopback only by default.
- [ ] Authenticate local HTTP and IPC clients.
- [ ] Prevent untrusted repositories from changing global capture policy.
- [ ] Define responsible disclosure and release signing procedures.

## 9. Inference and evaluation

### Inference architecture

- [x] Tier 1: deterministic grouping and state projection.
- [ ] Tier 2: local semantic extraction.
- [ ] Tier 3: optional cloud-model enrichment.
- [x] Version all inference algorithms, models, prompts, and schemas.
- [x] Store confidence and evidence IDs with every inference.
- [ ] Keep observed time, validity time, inference time, and supersession time.
- [ ] Permit full replay and re-projection after inference improvements.
- [x] Store human corrections as training/evaluation evidence.

### Gold dataset

- [ ] Recruit existing heavy VibeMon users as opt-in design partners.
- [ ] Label 200–500 real sessions with explicit consent.
- [ ] Label WorkUnit boundaries.
- [ ] Label objective, phase, blocker, decision, outcome, and handoff.
- [ ] Preserve provider balance in the evaluation set.
- [ ] Include ambiguous and incomplete sessions rather than only clean examples.
- [ ] Keep evaluation data separate from public demos.

### Metrics

- [ ] WorkUnit boundary F1.
- [ ] Objective/title reviewer acceptance.
- [ ] Phase accuracy.
- [ ] Needs You precision and recall, prioritizing precision.
- [ ] Evidence faithfulness.
- [ ] Human correction rate.
- [ ] Provider parity.
- [ ] Time from event to projected state.
- [ ] Rate of “insufficient evidence” answers.
- [ ] Rate of false causal claims.

### Required quality gates

- [ ] At least 80% reviewer acceptance for WorkUnit/objective summaries.
- [ ] At least 95% precision for Needs You notifications.
- [ ] 100% evidence linkage for user-visible derived claims.
- [ ] Less than 10 percentage points of core-accuracy difference between Tier 1
  providers.
- [ ] Explicit uncertainty instead of unsupported precise state.

## 10. macOS, Windows, and Linux support

### Shared platform contract

- [ ] A Tier 1 platform must pass the same durability, privacy, adapter, query,
  snapshot, upgrade, and uninstall test suites.
- [ ] “Compiles” is not sufficient for Tier 1 status.
- [ ] Keep the database format byte-compatible across operating systems and CPU
  architectures.
- [ ] Support non-ASCII paths, Korean text, emoji, spaces, long paths, and
  unusual repository names.
- [ ] Test sleep/wake, abrupt shutdown, concurrent sessions, and clock changes.

### Release targets

- [ ] macOS ARM64.
- [ ] macOS x86_64.
- [ ] Windows 10/11 x86_64 MSVC.
- [ ] Windows 11 ARM64 MSVC.
- [ ] Linux glibc x86_64.
- [ ] Linux glibc ARM64.
- [ ] Linux musl x86_64 static build.
- [ ] Linux musl ARM64 static build.

### Standard paths

- [ ] macOS data: `~/Library/Application Support/AttemptDB`.
- [ ] Windows data: `%LOCALAPPDATA%\AttemptDB`.
- [ ] Linux data: `$XDG_DATA_HOME/attemptdb`, falling back to
  `~/.local/share/attemptdb`.
- [ ] Keep config, data, cache, runtime socket/pipe, and logs in separate
  OS-appropriate locations.
- [ ] Support `--data-dir` portable mode.

### Background service

- [x] macOS: `launchd` per-user agent.
- [ ] Windows: per-user background process first, optional Windows Service.
- [x] Linux: `systemd --user` with non-systemd foreground/autostart fallback.
- [x] Ensure the database works without installing a privileged system service.
- [ ] Implement clean stop, restart, upgrade, and uninstall behavior.

### Distribution

- [ ] macOS Homebrew formula.
- [ ] macOS signed and notarized DMG/PKG.
- [ ] Windows signed MSI/EXE.
- [ ] Windows Winget package.
- [ ] Optional Windows Scoop manifest.
- [ ] Linux signed tarballs.
- [ ] Linux `.deb` packages.
- [ ] Linux `.rpm` packages.
- [ ] Linux AppImage for the optional desktop shell.
- [ ] Publish checksums and signed release provenance.
- [x] Implement rollback-safe auto-update.
- [ ] Provide offline/manual installation instructions.

### Cross-platform CI and failure testing

- [x] Native CI runners for every Tier 1 OS family.
- [x] Architecture-specific builds and smoke tests.
- [x] Kill during WAL append.
- [x] Kill during segment flush.
- [x] Kill during manifest update.
- [x] Disk full and quota exhaustion.
- [ ] Permission denied and read-only filesystem.
- [x] Corrupted WAL, segment, manifest, index, and blob.
- [x] Concurrent readers and writer.
- [x] Daemon unavailable with spool recovery.
- [x] Old schema and old binary compatibility (`tests/compat.rs` against the
  committed `fixtures/db/format-v2` database and snapshot).
- [ ] macOS-created snapshot opened on Windows and Linux, and every reverse
  combination.
- [ ] Windows long path and UNC path.
- [ ] Linux glibc and musl behavior.
- [ ] macOS Gatekeeper/notarization.
- [ ] Windows Defender and SmartScreen checks.

## 11. Local UI and AgentTimeline

### Shared web UI

- [x] Serve the primary UI from the AttemptDB daemon on an authenticated random
  loopback port.
- [x] `attempt ui` opens the system browser.
- [x] Never bind publicly without an explicit option and warning.
- [ ] Reuse the same frontend for local browser, optional Tauri shell, and
  hosted VibeMon where practical.
- [x] Keep DB access behind stable APIs rather than embedding storage logic in
  the UI.

### AgentTimeline views

- [x] Project current state.
- [x] WorkUnit list with evidence-backed status.
- [x] Attempt history including failed and superseded approaches.
- [x] Session/turn/span waterfall.
- [x] Causal graph.
- [x] Decision log with alternatives and evidence.
- [x] Artifact and Git linkage (`commits` table; attempts and work units
  carry the shas they produced).
- [x] Handoffs across agents and subagents.
- [x] Time-travel state viewer.
- [x] Human correction UI.
- [x] Capture coverage and missing-evidence display.
- [x] Local/cloud privacy mode indicator.
- [ ] Needs You queue containing only high-precision intervention items.

### Optional native shell

- [ ] Keep Tauri as a thin shell over the daemon and shared web frontend.
- [ ] Add tray status and background controls without moving the database into
  the UI process.
- [ ] Support deep links from VibeMon mobile to local/hosted timeline items.

### Shareable artifacts and viral loop

- [x] Export a sanitized timeline as static HTML.
- [ ] Generate a GitHub README status badge.
- [ ] Generate PR agent-work summaries.
- [ ] Generate a shareable build replay.
- [ ] Generate a cross-agent handoff graph.
- [x] Add optional “Built with AttemptDB” attribution.
- [x] Require an explicit privacy review before publishing any timeline.

## 12. Self-hosting and proof

- [x] Install AttemptDB capture before meaningful AttemptDB implementation work
  continues beyond the bootstrap stage.
- [x] Import the bootstrap history without representing hand-written seed data
  as captured fact.
- [x] Mark pre-capture history as imported or reconstructed.
- [ ] Record the agents, prompts, tools, tests, failures, decisions, and
  corrections that build AttemptDB.
- [ ] Produce at least one real failed-and-superseded Attempt suitable for the
  public demo.
- [ ] Ensure the public version contains no private paths, secrets, prompts, or
  unrelated repository information.
- [ ] Make the self-hosted dataset downloadable as a sanitized `.atdb` snapshot.
- [ ] Make every HN demo query runnable against that snapshot.

## 13. VibeMon integration and pivot

### Protect the existing product

- [ ] Keep the current VibeMon metadata-only promise unchanged for existing
  users until they explicitly opt in to new local capture.
- [x] Do not break the existing hook ingestion path during shadow deployment.
- [x] Run AttemptDB alongside existing VibeMon collection before migration.
- [x] Compare event counts and coverage between the two pipelines.
- [ ] Preserve XP/activity behavior during the transition.
- [ ] Keep unrelated existing user changes and data intact.

### Shared collector and schema

- [x] Move provider adapters toward a shared AttemptDB adapter contract
  (`spec/event-v1.schema.json` + `attempt conformance`; the legacy VibeMon
  envelope is one more input to the same contract).
- [ ] Separate event ingestion from VibeMon XP/profile/gamification work.
- [x] Replace per-event synchronous cloud work with local durability and batch
  export (`attempt hook` → spool → local WAL; `attempt sync` uploads batches
  after the fact, one in flight, cursor advanced only on acknowledgement).
- [ ] Produce real semantic AttemptDB signals before treating VibeMon Mission UI
  labels as facts.
- [ ] Keep conservative metadata fallbacks labeled as derived/limited.

### Sync protocol

- [x] Define immutable event sync with device ID, event ID, source sequence, and
  idempotency (RFC 0006 §10 v1: `POST /v1/sync`, client-minted event ids,
  server dedupe, `attrs.device_seq`).
- [ ] Sync redacted event fields independently from encrypted content blobs.
- [x] Support repository-specific sync policy (`attempt sync policy
  exclude|include <remote|prj_…>`, `connect --exclude`; evaluated on the
  device, excluded projects never leave it and the server never learns of
  them).
- [x] Support device offline operation and eventual upload (per-database
  `sync_state` cursor; a failed batch is re-sent next run and deduplicated).
- [ ] Resolve mutable user preferences and correction pointers separately from
  immutable event facts.
- [ ] Avoid requiring a full CRDT where immutable IDs and ordered corrections
  are sufficient.
- [ ] Make deletion and retention behavior visible and auditable.

### Cloud architecture

- [ ] Use the same AttemptDB segment and query semantics locally and in cloud.
- [ ] Store immutable cloud segments and large blobs in object storage.
- [ ] Implement a tenant/project/device catalog and access-control layer.
- [ ] Add distributed query nodes over shared segments.
- [ ] Add projection workers for team/project state.
- [ ] Use Postgres only for VibeMon transactional concerns such as identity,
  billing, permissions, and product configuration—not as the primary raw
  AttemptDB history engine.
- [ ] Define retention separately for local facts, cloud metadata, synced
  content, and derived projections.

### VibeMon product roles

- [ ] Web: hosted AgentTimeline and project memory.
- [ ] Mobile: away-from-keyboard monitoring and high-precision Needs You.
- [x] CLI/MCP: primary experience for terminal-only users.
- [ ] Pro: multi-device sync, long history, mobile notifications, advanced
  search, and hosted explorer.
- [ ] Team: handoff, overlap/risk, RBAC, SSO, retention, audit, and policy.
- [ ] Avoid manager-surveillance positioning; sell continuity, handoff,
  evidence, and governance.
- [ ] Preserve VibeMon's emotional/game layer as differentiation without making
  it the AttemptDB technical story.

### VibeMon business validation

- [ ] Recruit at least 20 existing heavy users into the design-partner cohort.
- [ ] Reduce current install/hook non-activation from roughly 54% to below 35%.
- [ ] Measure source attribution from HN → GitHub → install → first event → first
  query → VibeMon sync.
- [ ] Achieve at least five paid individual users or three committed team design
  partners before expanding team/control-plane scope.

## 14. Documentation and open-source foundation

- [x] Select and add a permissive license; Apache-2.0 is the leading candidate.
- [ ] Reserve GitHub repository, package names, and domains before public
  announcement.
- [x] Create `CONTRIBUTING.md`.
- [x] Create `CODE_OF_CONDUCT.md`.
- [x] Create `SECURITY.md`.
- [ ] Create `GOVERNANCE.md` if external maintainers emerge.
- [x] Create ADR/RFC directories.
- [ ] Publish the canonical event schema RFC.
- [ ] Publish the storage format RFC.
- [ ] Publish the privacy and capture-mode RFC.
- [ ] Publish the AttemptQL RFC.
- [ ] Publish compatibility and support policy.
- [x] Publish benchmark methodology and raw reproducible results.
- [x] Publish limitations and unsupported claims.
- [x] Add issue and pull-request templates.
- [ ] Add a roadmap without promising dates that are not meaningful.

## 15. Reliability and benchmark program

### Correctness suites

- [ ] Unit tests for codecs, IDs, clocks, projections, and indexes.
- [ ] Property tests for arbitrary event sequences and projection replay.
- [ ] Golden storage-format fixtures retained across releases.
- [ ] SQL logic tests.
- [ ] AttemptQL parser/planner/result tests.
- [ ] Adapter real-payload golden tests.
- [ ] Privacy canary tests.
- [ ] Cross-version open, upgrade, and downgrade tests.
- [ ] Fuzz WAL, segment, manifest, query parser, IPC, and importer inputs.
- [x] Model crash consistency with deterministic fault injection.

### Workload benchmarks

- [x] Define a public synthetic workload modeled on real coding-agent event
  distributions.
- [x] Replay at least 1.45 million sanitized/synthetic-equivalent events.
- [x] Benchmark sustained ingest with concurrent queries.
- [x] Benchmark WAL acknowledgment latency.
- [x] Benchmark recent timeline queries.
- [x] Benchmark full-project historical scans.
- [x] Benchmark causal traversal depth and fan-out.
- [x] Benchmark time-travel projection reconstruction.
- [x] Benchmark compaction impact on foreground ingest.
- [x] Benchmark database size and compression by event type.
- [ ] Benchmark macOS, Windows, and Linux independently.
- [x] Publish failures and pathological cases, not only best-case numbers.

### Initial technical gates

- [ ] At least 99.5% local durable-write success in monitored real sessions.
- [ ] Hook path p95 overhead below 10 ms, excluding host-agent overhead.
- [ ] No event loss in tested supported crash points.
- [ ] No privacy-canary leak.
- [ ] Deterministic projections from identical fact streams.
- [ ] Portable snapshots produce identical logical query results on every Tier
  1 OS.
- [ ] Indexes can be fully rebuilt from immutable facts.

## 16. HN launch package

### Repository experience

- [ ] README explains the thesis above the fold.
- [x] README says exactly which portions are implemented.
- [ ] One install command works on a clean macOS, Windows, and Linux machine.
- [ ] No signup, email, API key, or hosted service required for the demo.
- [ ] Include a small downloadable self-hosted `.atdb` database.
- [ ] Include copy-paste queries whose output matches the README.
- [ ] Include architecture and storage diagrams.
- [x] Include reproducible tests and benchmarks.
- [ ] Include license and tagged release.
- [ ] Keep the VibeMon upsell optional and below the OSS value proposition.

### Demo narrative

- [ ] Start with the current AttemptDB project state.
- [ ] Ask why one WorkUnit was blocked.
- [ ] Follow evidence to a real failed attempt.
- [ ] Open the successful superseding attempt.
- [ ] Time-travel to the project before the fix.
- [ ] Show a handoff between two different coding agents.
- [ ] Reveal that the database contains its own build history.
- [ ] End with local installation, not a signup page.
- [ ] Keep the complete demo under one minute.

### HN text

- [x] Store the current draft in `SHOW_HN_DRAFT.md`.
- [ ] Rewrite the final first comment in the founder's natural voice.
- [ ] Replace every placeholder with a verified command or URL.
- [ ] Remove every claim not supported by public evidence.
- [ ] Prepare concise answers to:
  - [ ] Why not Git?
  - [ ] Why not traces plus SQLite?
  - [ ] Why is this a DBMS?
  - [ ] Why not OpenTelemetry alone?
  - [ ] How is inference prevented from becoming hallucinated history?
  - [ ] What is collected locally and uploaded?
  - [ ] How does this differ from agent memory/vector databases?
  - [ ] How does it compare with Grafana, Langfuse, LangSmith, and Phoenix?
- [ ] Do not ask friends or existing users to upvote or seed comments.
- [ ] Be present for technical questions and criticism after posting.

### Launch blockers

- [x] Runnable code exists.
- [ ] Self-hosted build-history demo exists.
- [ ] One-command path works without signup.
- [ ] Cross-platform release artifacts are signed and verified.
- [ ] Public data contains no private user content.
- [ ] Claims match tests and benchmarks.
- [ ] Attribution is working before traffic arrives.
- [ ] VibeMon onboarding and sync path can absorb the traffic.
- [ ] GitHub issues, security reporting, and contribution paths are ready.

## 17. Viral and conversion loop

```text
HN title
  → AttemptDB GitHub
  → local install
  → first captured Attempt
  → first WHY/TRACE answer
  → sanitized shareable timeline
  → GitHub/PR/README distribution
  → optional VibeMon sync/mobile
  → recurring use and paid conversion
```

- [ ] Add source attribution to every installer and release link.
- [ ] Measure repository view → download.
- [ ] Measure download → successful install.
- [ ] Measure install → verified hook.
- [ ] Measure verified hook → first durable event.
- [ ] Measure first event → first useful Attempt/WHY query.
- [ ] Measure first query → second-day and seventh-day reuse.
- [ ] Measure local use → VibeMon sync opt-in.
- [ ] Measure sync → mobile activation and paid conversion.
- [ ] Generate privacy-reviewed shareable artifacts that link back to
  AttemptDB.
- [ ] Make attribution optional and removable in exported artifacts.

### Provisional launch scorecard

- [ ] Reach the Show HN page and target the main HN front page.
- [ ] Reach at least 300 GitHub stars within 72 hours as a distribution signal.
- [ ] Achieve at least 100 verified local installations from launch traffic.
- [ ] Achieve at least 50 users who capture a real first Attempt.
- [ ] Achieve at least 20 users active on three separate days within 14 days.
- [ ] Achieve at least 20 VibeMon sync activations from AttemptDB.
- [ ] Collect qualitative answers to whether failed-attempt history changes
  real coding-agent workflows.

These are launch-learning thresholds, not vanity goals or permanent product
KPIs. If traffic is high but activation is low, fix the local value and
onboarding before expanding scope.

## 18. Milestone order and definitions of done

Engineering capacity is not the limiting factor, but dependency order still
matters. Parallelize work inside each milestone without violating the contracts
established by earlier milestones.

### M0 — Public contract

- [ ] Finalize license, naming assets, repository structure, and public claims.
- [ ] Merge event-schema, storage-format, privacy, and AttemptQL draft RFCs.
- [ ] Define Tier 1 platform and compatibility policy.

**Done when:** another engineer can implement an interoperable event writer and
understand what the database promises without private context.

### M1 — Durable cross-platform engine

- [ ] WAL, recovery, MemTable, segment flush, manifest, and basic compaction.
- [ ] Portable event format and snapshot exchange.
- [ ] Native builds and crash tests on macOS, Windows, and Linux.

**Done when:** identical event streams survive fault injection and return
identical results on every Tier 1 OS.

### M2 — Agent semantics and query

- [ ] Event, Span, Attempt, WorkUnit, Decision, Artifact, Inference, and
  Correction tables/projections.
- [ ] DataFusion-backed SQL.
- [ ] First `SHOW ATTEMPTS`, `WHY`, `TRACE`, and `STATE AT` operators.

**Done when:** a deterministic fixture explains a failed and superseded attempt
using evidence-linked queries.

### M3 — Native capture

- [ ] Daemon, IPC, spool, installer, doctor, and provider adapters.
- [ ] Verified Claude, Codex, Cursor, and Gemini capture.
- [ ] Privacy modes and encryption.

**Done when:** clean machines on all Tier 1 OSes can install, capture, recover,
and uninstall without damaging existing agent configuration.

### M4 — Inference and correction

- [ ] Deterministic WorkUnit projection.
- [ ] Optional local/cloud semantic enrichment.
- [ ] Human corrections and evaluation harness.

**Done when:** the inference quality gates in this document are met against the
opt-in gold dataset.

### M5 — AgentTimeline and self-hosting

- [ ] Local timeline, causal graph, time travel, and evidence navigation.
- [ ] AttemptDB captures its own continued development.
- [ ] Sanitized self-hosted snapshot and one-minute demo.

**Done when:** a new user can answer why a real AttemptDB development approach
failed without reading raw log files.

### M6 — VibeMon bridge

- [ ] Optional sync, hosted AgentTimeline, mobile Needs You, and attribution.
- [ ] Shadow validation against existing VibeMon event collection.

**Done when:** an AttemptDB user can opt into VibeMon without changing local
ownership or the default content privacy boundary.

### M7 — Show HN

- [ ] Public repository, signed releases, benchmarks, docs, demo, and support
  readiness.
- [ ] Final founder-written HN submission and first comment.

**Done when:** every launch blocker in this document is checked and the product
is directly usable without signup.

### M8 — Post-launch product decision

- [ ] Analyze activation, repeat use, sync, paid conversion, and qualitative
  feedback.
- [ ] Prioritize based on demonstrated user value rather than HN comments alone.
- [ ] Expand team/control features only after continuity and project-memory
  value is proven.

**Done when:** the next product direction is supported by real AttemptDB and
VibeMon behavior data.

## 19. Decisions still requiring explicit closure

- [ ] Confirm Apache-2.0 versus another permissive license.
- [ ] Confirm public GitHub organization and repository URL.
- [ ] Register or reserve `attemptdb.com`, `attemptdb.dev`, and package names if
  still available.
- [ ] Confirm whether the 1.45M-event aggregate can be used publicly.
- [ ] Finalize local content capture consent UX.
- [ ] Finalize the exact boundary between AttemptQL syntax and SQL functions.
- [ ] Finalize `.atdb` snapshot encryption and key portability UX.
- [ ] Finalize VibeMon pricing attached to sync/mobile/team features.
- [ ] Decide whether the first public UI is browser-only or also ships with the
  optional Tauri shell.

## 20. Immediate next actions

- [x] Update `README.md` to replace the generic embedded-storage language with
  the agreed AttemptDB-owned WAL + columnar segment + DataFusion architecture.
- [x] Create `docs/rfcs/0001-canonical-event-model.md`.
- [x] Create `docs/rfcs/0002-storage-engine.md`.
- [x] Create `docs/rfcs/0003-fact-inference-bitemporal-model.md`.
- [x] Create `docs/rfcs/0004-attemptql.md`.
- [x] Create `docs/rfcs/0005-cross-platform-runtime.md`.
- [x] Create `docs/rfcs/0006-privacy-and-sync.md`.
- [ ] Select the license.
- [ ] Reserve the public naming assets.
- [ ] Define the self-hosting bootstrap boundary and begin capture before the
  implementation history that matters is created.

## 21. Unified collector — remaining work (audit 2026-08-30)

Audit of the "one collector, two user experiences" structure against `main`
`69e26e7`: AttemptDB is the single collection engine for every new install;
VibeMon users get it through an installer and never need the CLI;
`vibemon-hooks` becomes a compatibility layer and retires. What the audit
found done is already ticked in the sections above; this section lists only
what is still open, grouped by the layer of that structure, with pointers
where an item also lives elsewhere in this file. The VibeMon-side execution
plan (phases, decisions, screen list) is
`streamize/vibemon/ATTEMPTDB_ARCHITECTURE_PLAN.md`.

Measured baseline: 432 tests green on macOS ARM64; daemon running on the
owner's machine; server write path (`/v1/sync`, `/v1/sync/inferences`,
`/v1/vibemon/hook`, `/v1/admin/keys`) implemented and tested; no read API, no
deployment, no release, no OTel intake, zero AttemptDB references in
`vibemon-web` / `vibemon-app`.

### 21.1 Stop the bleeding (owner, before anything else)

- [x] Release `vibemon-hooks` v30 with the env-prefix comment fix
  *(2026-08-31: v28 put the comment between the backslash-continued `VIBEMON_*` prefix and the `python3` that consumes it — a comment ends the logical line, so the assignments became unexported shell variables and every Unix hook posted the empty fallback envelope. Comment moved above the command; three regression tests in `tests/test_static.py` (continuation-comment check over src and over the built `NOTIFY_SCRIPT`, plus an execution of the real block asserting all ten variables arrive) all fail against the v29 build. Tag pushed before `main` so `?v` never pointed at 30 while `releases/latest` still served v29. Verified: `?v` = 30, served `install.sh` sha256 = local reproducible build. The gate — fleet `hook_events` insert resuming — needs a 24 h check.)*
  (`src/notify.sh` 129–131): production collection has been broken since
  2026-08-26 and every installed client is still on v28/v29.
- [x] Make `nullarch/attemptdb` public: unblocks Actions billing, the first
  *(2026-08-31: done from the CLI with `gh repo edit --visibility public --accept-visibility-change-consequences`, after an independent pre-public audit — no `/Users/chung` in the working tree, no credentials, history-wide path scan clean, the only secret-shaped hits are the privacy canaries' placeholders. Billing block confirmed lifted: re-running the last failed CI run starts jobs instead of the "job was not started … billing" annotation. `test (windows-x86_64)` now fails for a real reason — a release blocker, tracked by the other session.)*
  tag, `attempt update`, and the Homebrew tap.
- [x] Take the three decisions the server cannot proceed without: tenant =
  *(2026-08-31, recorded in PROGRESS "Decisions taken 2026-08-31": **tenant = organisation**, personal org for solo users, mapping in the server as `--tenant-rule`, never baked into the Edge Function; **default sync profile = `semantic`**, server capture-mode ceiling stays `metadata_only`, `full` stays explicit opt-in; **realtime = `/v1/sessions` 5 s polling first**, presence channel later.)*
  organisation (personal org for solo users) or user; default sync profile
  (`metadata_only` / `semantic` / `full`); realtime path for the VibeMon
  coding-state UI (daemon interval 5 s first, presence channel later).

### 21.2 Single collector: retire the legacy client

- [ ] Run `attempt hook install --remove-legacy vibemon` for real on the
  owner's machine once the hosted server receives uploads (dry run done; the
  live run stops the legacy client's uploads on that device).
- [ ] Supabase Edge Function `/hook` forwards envelope v2 to
  `POST /v1/vibemon/hook` so legacy installs land in the new server without a
  client update; XP accrual stays in `/hook` until 21.8.
- [x] Explicit consent step before any existing VibeMon user moves off
  *(2026-08-30: the migration installers run `attempt init --capture-mode metadata_only`; `--local-content` / `-LocalContent` is the consent step, nothing changes the mode silently)*
  `metadata_only` (§8 Capture modes).
- [ ] Archive `vibemon-hooks`: README "Superseded by AttemptDB" + migration
  link; keep the repository for the compatibility contract only.

### 21.3 One-line install for VibeMon users

- [ ] Cut the first tag and run `release.yml` for real; `install.sh` /
  `install.ps1` have never installed a published release (§10 Distribution).
- [x] `docs/migration/vibemon-install.sh` installs and starts the daemon
  (`attempt daemon install`); today it stops at `sync now`, so nothing
  uploads after the first run.
- [ ] Serve that script at `vibemon.dev/install.sh` (currently serves the
  *(`vibemon-install.ps1` drafted 2026-08-30 with a Scheduled Task standing in for the Windows daemon; serving both is the owner's)*
  `vibemon-hooks` release) and a Windows counterpart at `install.ps1`.
- [ ] Windows: port the signed GUI installer from
  `vibemon-hooks/installer/windows` (`VibemonSetup.exe`); Winget / Scoop
  after signing (§10).
- [ ] Homebrew tap `nullarch/homebrew-attemptdb` (owner creates the public
  repository; formula generated from the release).
- [x] `attempt sync connect` default endpoint / `vibemon` alias so the
  *(2026-08-30: `attempt sync connect vibemon` / `add <name> vibemon`, `VIBEMON_SYNC_URL` overrides)*
  installer and docs do not carry the URL.
- [ ] Device key hand-off: vibemon-web "Connect your coding agents" →
  *(server side done: `POST /v1/admin/keys` takes `scope` and `user_id`; the web call is 21.8)*
  `POST /v1/admin/keys` → key embedded in the install command; `DELETE` on
  unlink.
- [x] Post-install summary the way the product describes it: agents
  *(2026-08-30: both installers end with `attempt doctor`)*
  detected, hooks installed, database started, sync connected — one screen,
  no `attempt` commands required afterwards.

### 21.4 Daemon and background sync

- [ ] Windows per-user background process (§10 Background service) and the
  `cfg(unix)` daemon / crash / repair suites running there — Windows
  currently proves compile + unit tests only.
- [ ] Daemon default upload interval per profile (`semantic` 5 s) once the
  realtime decision is taken.
- [x] Sync status for non-CLI users: `attempt sync status --json` exists; the
  *(2026-08-30: server side `GET /v1/devices` — keys, connected, counts, `last_sync_at`; the web row is 21.8)*
  web needs "Connected · last sync N s ago" per device from the server
  (21.7 read API).
- [x] Decide group-commit-with-timer as the daemon default (`--relaxed`
  *(2026-08-30: measured on 1,544 live events — `attrs.hook_us` p50 258 µs, p95 415 µs, p99 778 µs; the gate is met under strict durability, which stays the default)*
  exists) from real `attrs.hook_us` p95.
- [x] Separate small `attempt-hook` binary: the 75 MiB executable load is
  *(2026-08-30: `crates/attempt-hook`, 0.8 MB vs 76.5 MB; hook wall p50 6.6 → 4.2 ms, p95 7.1 → 4.6 ms on this machine — the rest is process spawn; installers, updater and Homebrew ship the pair)*
  about 85 % of hook wall time.

### 21.5 OTel intake (new scope — in no section above; decide first)

- [ ] Decide whether AttemptDB receives OTLP at all. Hooks carry the
  *(ADR 0003 (`docs/adr/0003-otel-intake.md`) proposes OTLP/HTTP JSON on the daemon, loopback only; owner decision pending)*
  execution lifecycle; token / model / cost / API telemetry only exist in the
  agents' OTel exporters. Without this, "complete Agent Timeline" means
  hooks + git only.
- [ ] If yes: local OTLP/HTTP receiver in the daemon on `127.0.0.1:4318`
  (JSON encoding, no heavy new dependency — the binary stays single and
  static), mapped into canonical Events under `attrs.x_otel_*` /
  `content`, obeying the capture mode.
- [ ] Installer registers `OTEL_EXPORTER_OTLP_ENDPOINT` and each provider's
  telemetry switch (per provider docs) with the same structural-edit safety
  as hooks; `doctor` reports it; `uninstall` removes it.
- [ ] Correlate OTel spans with hook events (session id, tool id) in the
  projector; RFC 0001 §9 maps one direction only today.

### 21.6 Git and filesystem effects

- [x] Artifact and Git linkage in the timeline (§11): commits joined to the
  *(2026-08-30: `commits` projection + query table, `SHOW COMMITS`, `commit_shas` on attempts and work units, shown by `attempt timeline`, the UI JSON and the read API; linkage from the `HEAD` the hook records, no output read)*
  attempts that produced them via `commit.sha`.
- [ ] Filesystem change capture beyond what tool calls report (watcher or
  post-tool diff stat), metadata-only by default.

### 21.7 Server: tenancy, read side, deployment

- [x] Sync peers and named profiles: `attempt sync add <name> <url> --key …
  *(2026-08-30: peers in `sync.json`, per-peer cursors bound to the server URL, `metadata_only|semantic|full`, daemon re-reads the config every tick)*
  --profile metadata_only|semantic|full`, `sync list|remove`, per-peer
  cursor and digest; existing `sync.json` becomes the `default` peer. Do
  this before external users exist (format migration otherwise).
- [x] Key entries carry `user_id` and `scope ∈ {device, reader, admin}`;
  `/v1/sync*` accepts `device` only; the read API needs `reader`.
- [x] `DELETE /v1/devices/{id}`: revoke the key and record a server-side
  *(2026-08-30: `DELETE /v1/admin/devices/{id}[?tenant=]`, reason `revoked`, repeat calls report already-retracted sessions)*
  Retraction for that device's events (facts kept, projections exclude).
- [x] Read API over a per-tenant `EngineCache` (server depends on
  *(2026-08-30: `EngineCache` moved into `attemptdb-query` (UI, MCP and server share it); parity test: server == local projection; `docs/server-api.md`)*
  `attemptdb-project` / `attemptdb-query`): `GET /v1/work`, `/v1/sessions`,
  `/v1/attention` (`why_blocked` top N = Needs You), `/v1/timeline`,
  `/v1/state?at=`, `GET /v1/events?after=<seq>`, `POST /v1/query`
  (read-only, tenant-scoped). Done when `attempt ui` and `/v1/timeline`
  agree on session and attempt counts for this device's tenant.
- [x] Merge rule for device-uploaded vs server-computed inferences: same
  `(kind, id)` → device wins only when its `algorithm_version` ≥ the
  server's; every response carries `computed_by`, `algorithm_version`,
  `evidence`.
- [x] Organisation work graph: per-tenant projections across devices
  *(2026-08-30: a tenant database holds every device's events, so sessions, handoffs and work units across devices come out of the same projection; served by `/v1/work`, `/v1/sessions`, `/v1/attention`)*
  (overlap, handoff, blocked) — the team view the product sells.
- [ ] Deployment: Dockerfile from the musl static binary, one persistent
  *(2026-08-30: `deploy/` (Dockerfile, entrypoint, compose with Caddy) and `docs/deploy.md` written, not built here — Docker was not running; rate limiting and the actual deployment remain)*
  volume (VM, not Cloud Run — flock / fsync), TLS in front, `.atdb`
  snapshot backup to object storage, health check, rate limiting, open-DB
  LRU sizing.
- [x] Backfill: `attempt import vibemon-export <file> --tenant <t>` over the
  *(2026-08-30: `attempt import vibemon-export <file> [--db <tenant dir>]` — NDJSON or array, ids derived from the row PK so re-runs store nothing, rejected rows counted by reason)*
  Supabase `hook_events` export (envelope v2 adapter exists; the batch
  importer does not); verify per-period session / event counts match.
- [x] Old-binary / old-schema compatibility test (rolling upgrades across
  *(2026-08-30: `fixtures/db/format-v2` + `tests/compat.rs`: read, continue, restore, refuse an unknown version)*
  thousands of tenant DBs) (§10 CI).
- [x] Segment compaction for long-lived tenant DBs (§5).
  *(2026-08-30: `Database::compact` — contiguous runs of small segments merged into one, new manifest generation, inputs tombstoned and deleted after the next generation, three SIGKILL crash points, `attempt compact`; 200 segments → 1: open 2.85× faster; the daemon compacts after each periodic flush and the server when a tenant is flushed and closed)*

### 21.8 VibeMon web and app (vibemon repositories)

- [ ] Web reads move to the read API: `src/lib/timeline.ts`,
  `src/lib/mcp/tools.ts` (`needs_you`, `catch_up`, `session_list`, …),
  admin user detail.
- [ ] App screens off `hook_events`: activity, project, index (coding
  state), share / daily-card / yard / debug / setup; `useCodingState`
  follows the realtime decision.
- [ ] XP engine consumes `GET /v1/events?after=` instead of running inside
  `/hook`; `/hook` becomes a pure forwarder.
- [ ] Device list page: agents connected, last sync, capture profile, unlink.
- [ ] Capture-profile picker at connect time (recommended = `semantic`) and
  organisation-level policy (`metadata_only` / `semantic` / `full` /
  custom) for team admins.
- [ ] Drop `hook_events` / `coding_sessions` / `work_links` only after read
  migration and backfill verification.

### 21.9 Desktop shell (optional, after 21.7)

- [ ] Tray app limited to collector status, local event / disk counts, sync
  state, privacy mode, "Open VibeMon"; embeds `attempt ui` (§11 Optional
  native shell). No second timeline product.

### 21.10 Self-hosting housekeeping (this machine)

- [x] Reinstall the binary and restart the daemon: `~/.cargo/bin/attempt` is
  *(2026-08-30: reinstalled twice during the wave; hooks now reference `attempt-hook`)*
  the 2026-08-29 build and lacks `sync`, `--remove-legacy`, `update`, and
  inference sync; the running daemon therefore has no uploader.
- [ ] Run `attempt hook install --remove-legacy vibemon` live (see 21.2).

### 21.11 Open engineering findings carried over

- [ ] macOS x86_64 `Locked` on writer reopen in `crash.rs` — two clean runs,
  no explanation; stays open until an Intel run reports a non-zero retry.
- [ ] Windows durability, recovery and daemon behaviour untested (suites are
  `cfg(unix)`).
- [ ] Engine reload floor (0.45 s per refresh): cache readable batches per
  segment, build projection tables from typed column builders.
