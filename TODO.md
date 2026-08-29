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
- [ ] `attempt daemon`
- [x] `attempt status`
- [x] `attempt timeline`
- [x] `attempt query`
- [x] `attempt why`
- [x] `attempt trace`
- [x] `attempt failures`
- [x] `attempt handoffs`
- [x] `attempt snapshot export`
- [x] `attempt snapshot open`
- [ ] `attempt ui`
- [ ] `attempt mcp`
- [ ] `attempt update`
- [ ] `attempt uninstall`

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
  - [ ] inference
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
- [ ] Specify `Correction` as a first-class human event rather than an in-place
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

- [ ] Define phases:
  - [ ] `EXPLORE`
  - [ ] `PLAN`
  - [ ] `IMPLEMENT`
  - [ ] `DEBUG`
  - [ ] `VERIFY`
  - [ ] `REVIEW`
  - [ ] `DELIVER`
  - [ ] `BLOCKED`
- [ ] Define status independently from phase.
- [ ] Do not expose fake numerical progress percentages.
- [ ] Record objective, phase, status, confidence, evidence, artifacts, actors,
  causal links, and updated time for each WorkUnit version.
- [ ] Define merge and split semantics for inferred WorkUnits.
- [ ] Define how sessions from different agents join the same WorkUnit.

### Compatibility and standards

- [ ] Map canonical fields to OpenTelemetry GenAI semantic conventions where
  meaningful.
- [ ] Preserve provider-specific payloads or local references without forcing
  all provider details into the common schema.
- [ ] Define schema versioning independent of storage format versioning.
- [ ] Publish JSON Schema or equivalent interchange specification.
- [ ] Publish canonical event fixtures for every supported provider and version.
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
- [ ] Implement group commit.
- [x] Implement checksummed recovery from partial tail records.
- [x] Implement WAL rotation.
- [ ] Implement safe log truncation after segment durability is proven.
- [ ] Verify recovery after process kill, machine restart, and partial write.
- [ ] Verify behavior when the disk is full.

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
- [ ] Test database open and recovery across all supported operating systems.

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

- [ ] Store large prompts, messages, tool outputs, diffs, and artifacts outside
  primary event segments.
- [ ] Use content-addressed encrypted blobs.
- [ ] Deduplicate identical content without leaking plaintext hashes externally.
- [ ] Authenticate every encrypted blob.
- [ ] Bind blob references to tenant/device/project scope.
- [ ] Implement secure deletion limitations honestly.
- [ ] Implement snapshot export with a portable encryption key.
- [ ] Implement re-keying and key rotation.

### Snapshots, backup, and repair

- [x] Implement consistent point-in-time snapshots.
- [x] Implement snapshot verification before export succeeds.
- [ ] Implement `attempt verify` for manifests, WAL, segments, indexes, and blobs.
- [ ] Implement `attempt repair` without silently discarding evidence.
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
- [ ] `SHOW DECISIONS`
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
- [ ] MCP server in the `attempt` binary.
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
- [ ] Support foreground, per-user daemon, CLI client, hook entrypoint, MCP, and
  UI server modes from the same binary.
- [ ] Keep hook startup overhead small and measurable.

### IPC and offline durability

- [ ] macOS/Linux default IPC: Unix domain socket.
- [ ] Windows default IPC: Named Pipe.
- [ ] Cross-platform fallback: authenticated loopback TCP.
- [ ] Use a framed, versioned local ingest protocol.
- [x] If the daemon is unavailable, append to a crash-safe per-process spool.
- [x] Import spool files when the daemon recovers.
- [x] Make ingestion idempotent by event ID and source sequence.
- [ ] Never block the coding agent on inference, sync, or UI work.
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
- [ ] Secret scanning again before sync/export.
- [ ] Allowlist canonical metadata fields.
- [x] Add privacy canaries for prompts, source code, command bodies, tokens,
  stdout/stderr, tool output, email, home paths, and provider-specific leaks.
- [ ] Never place real private payloads in fixtures, screenshots, issue reports,
  or public self-hosting demos.
- [ ] Generate sanitized demo datasets from explicit rules.
- [ ] Keep content-free mode fully functional, while representing its inference
  limits honestly.

### Key management

- [ ] macOS: Keychain.
- [ ] Windows: DPAPI/Credential Manager.
- [ ] Linux desktop: Secret Service.
- [ ] Linux headless: passphrase or explicit key file.
- [ ] Provide portable encrypted snapshot export independent of OS key stores.
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
- [ ] Store human corrections as training/evaluation evidence.

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

- [ ] macOS: `launchd` per-user agent.
- [ ] Windows: per-user background process first, optional Windows Service.
- [ ] Linux: `systemd --user` with non-systemd foreground/autostart fallback.
- [ ] Ensure the database works without installing a privileged system service.
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
- [ ] Implement rollback-safe auto-update.
- [ ] Provide offline/manual installation instructions.

### Cross-platform CI and failure testing

- [ ] Native CI runners for every Tier 1 OS family.
- [ ] Architecture-specific builds and smoke tests.
- [ ] Kill during WAL append.
- [ ] Kill during segment flush.
- [ ] Kill during manifest update.
- [ ] Disk full and quota exhaustion.
- [ ] Permission denied and read-only filesystem.
- [ ] Corrupted WAL, segment, manifest, index, and blob.
- [ ] Concurrent readers and writer.
- [ ] Daemon unavailable with spool recovery.
- [ ] Old schema and old binary compatibility.
- [ ] macOS-created snapshot opened on Windows and Linux, and every reverse
  combination.
- [ ] Windows long path and UNC path.
- [ ] Linux glibc and musl behavior.
- [ ] macOS Gatekeeper/notarization.
- [ ] Windows Defender and SmartScreen checks.

## 11. Local UI and AgentTimeline

### Shared web UI

- [ ] Serve the primary UI from the AttemptDB daemon on an authenticated random
  loopback port.
- [ ] `attempt ui` opens the system browser.
- [ ] Never bind publicly without an explicit option and warning.
- [ ] Reuse the same frontend for local browser, optional Tauri shell, and
  hosted VibeMon where practical.
- [ ] Keep DB access behind stable APIs rather than embedding storage logic in
  the UI.

### AgentTimeline views

- [ ] Project current state.
- [ ] WorkUnit list with evidence-backed status.
- [ ] Attempt history including failed and superseded approaches.
- [ ] Session/turn/span waterfall.
- [ ] Causal graph.
- [ ] Decision log with alternatives and evidence.
- [ ] Artifact and Git linkage.
- [ ] Handoffs across agents and subagents.
- [ ] Time-travel state viewer.
- [ ] Human correction UI.
- [ ] Capture coverage and missing-evidence display.
- [ ] Local/cloud privacy mode indicator.
- [ ] Needs You queue containing only high-precision intervention items.

### Optional native shell

- [ ] Keep Tauri as a thin shell over the daemon and shared web frontend.
- [ ] Add tray status and background controls without moving the database into
  the UI process.
- [ ] Support deep links from VibeMon mobile to local/hosted timeline items.

### Shareable artifacts and viral loop

- [ ] Export a sanitized timeline as static HTML.
- [ ] Generate a GitHub README status badge.
- [ ] Generate PR agent-work summaries.
- [ ] Generate a shareable build replay.
- [ ] Generate a cross-agent handoff graph.
- [ ] Add optional “Built with AttemptDB” attribution.
- [ ] Require an explicit privacy review before publishing any timeline.

## 12. Self-hosting and proof

- [x] Install AttemptDB capture before meaningful AttemptDB implementation work
  continues beyond the bootstrap stage.
- [ ] Import the bootstrap history without representing hand-written seed data
  as captured fact.
- [ ] Mark pre-capture history as imported or reconstructed.
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
- [ ] Do not break the existing hook ingestion path during shadow deployment.
- [ ] Run AttemptDB alongside existing VibeMon collection before migration.
- [ ] Compare event counts and coverage between the two pipelines.
- [ ] Preserve XP/activity behavior during the transition.
- [ ] Keep unrelated existing user changes and data intact.

### Shared collector and schema

- [ ] Move provider adapters toward a shared AttemptDB adapter contract.
- [ ] Separate event ingestion from VibeMon XP/profile/gamification work.
- [ ] Replace per-event synchronous cloud work with local durability and batch
  export.
- [ ] Produce real semantic AttemptDB signals before treating VibeMon Mission UI
  labels as facts.
- [ ] Keep conservative metadata fallbacks labeled as derived/limited.

### Sync protocol

- [ ] Define immutable event sync with device ID, event ID, source sequence, and
  idempotency.
- [ ] Sync redacted event fields independently from encrypted content blobs.
- [ ] Support repository-specific sync policy.
- [ ] Support device offline operation and eventual upload.
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
- [ ] CLI/MCP: primary experience for terminal-only users.
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
- [ ] Publish benchmark methodology and raw reproducible results.
- [ ] Publish limitations and unsupported claims.
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
- [ ] Model crash consistency with deterministic fault injection.

### Workload benchmarks

- [ ] Define a public synthetic workload modeled on real coding-agent event
  distributions.
- [ ] Replay at least 1.45 million sanitized/synthetic-equivalent events.
- [ ] Benchmark sustained ingest with concurrent queries.
- [ ] Benchmark WAL acknowledgment latency.
- [ ] Benchmark recent timeline queries.
- [ ] Benchmark full-project historical scans.
- [ ] Benchmark causal traversal depth and fan-out.
- [ ] Benchmark time-travel projection reconstruction.
- [ ] Benchmark compaction impact on foreground ingest.
- [ ] Benchmark database size and compression by event type.
- [ ] Benchmark macOS, Windows, and Linux independently.
- [ ] Publish failures and pathological cases, not only best-case numbers.

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
- [ ] Include reproducible tests and benchmarks.
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
