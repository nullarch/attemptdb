# Contributing to AttemptDB

AttemptDB is pre-1.0 but not a sketch: the storage engine, the four provider
adapters, the capture runtime, the projections, the query layer, the MCP
server, the local UI, and the sync client and server are implemented and
tested (`cargo test --workspace`). The RFCs under `docs/rfcs/` describe the
contracts those crates keep, and `docs/storage-format.md` is the byte-level
on-disk contract.

Good places to start: a new provider adapter (the contract is below), fixtures
and golden envelopes for payload shapes we do not cover yet, projection edge
cases with a failing test, platform work on Windows and Linux, and anything
labelled [`good first issue`](https://github.com/nullarch/attemptdb/labels/good%20first%20issue).
Design changes go through the RFC process below before code.

By contributing you agree that your contributions are licensed under the
[Apache License 2.0](LICENSE) that covers the project.

## Development setup

Requirements:

- Rust 1.94 or newer (the workspace sets `rust-version = "1.94"` and
  `edition = "2024"`). Install with [rustup](https://rustup.rs).
- A Tier 1 platform: macOS, Windows (MSVC), or Linux (glibc or musl).
- No Bash, Python, Node.js, Docker, or cloud account is required.

Commands:

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets
```

All four must pass before a pull request is reviewed. CI runs them on every
Tier 1 operating system; "compiles on my machine" is not sufficient.

## Crate layout

The workspace (`Cargo.toml` at the repository root) contains:

| Crate | Purpose |
| --- | --- |
| `crates/attemptdb-core` | Canonical data model: identifiers, hybrid logical clock, `Event`, schema constants, portable paths, capture modes, codecs. No I/O. |
| `crates/attemptdb-storage` | Storage engine: WAL, memtable, immutable columnar segments, manifest. |
| `crates/attemptdb-adapters` | Provider adapters that normalise coding-agent hook payloads into canonical events. |
| `crates/attemptdb-capture` | Capture runtime: spool, IPC protocol, daemon ingest, installer, doctor. |
| `crates/attemptdb-query` | Query layer: DataFusion-backed SQL and AttemptQL. |
| `crates/attemptdb-project` | Deterministic projections: sessions, turns, tool calls, attempts, work units, decisions, commits. |
| `crates/attemptdb-mcp` | Model Context Protocol server: the database as tools an agent can call. |
| `crates/attemptdb-ui` | Local AgentTimeline web UI and the static sanitised export. |
| `crates/attemptdb-server` | Sync server: authenticated per-tenant ingest and the read API. |
| `crates/attempt` | The `attempt` command-line binary (published as the `attemptdb` crate). |
| `crates/attempt-hook` | The small hook entrypoint agents call on every event. |
| `crates/attemptdb-bench` | Workload benchmarks. Not published. |

Dependency direction is strictly downward: `attemptdb-core` depends on nothing
in the workspace, and only `attempt` depends on everything. Do not add I/O,
an async runtime, or storage knowledge to `attemptdb-core`.

## Adapter contract

Every provider adapter converts one provider hook payload into one canonical
`Event` as defined in `crates/attemptdb-core/src/event.rs` and specified in
`docs/rfcs/0001-canonical-event-model.md`. An adapter must:

1. Set `provider` to the adapter's stable identifier (`claude_code`, `codex`,
   `cursor`, `gemini_cli`, or a new stable string for another provider).
2. Set `provider_event_name` to the provider's own event name, verbatim.
3. Set `kind` to the canonical `EventKind`. A provider event the adapter
   recognises but cannot map must be emitted with `kind = unknown`. Events are
   never dropped.
4. Set `adapter_version`, `capture_mode`, and, when known, `provider_version`
   and `hook_version`.
5. Keep content-bearing data out of `attrs`. `attrs` holds allowlisted,
   content-free metadata only. Prompts, commands, messages, file contents,
   tool input, tool output, error bodies, transcript paths, and similar data
   belong in `content` (which the capture mode may forbid) or `raw`.
   `Event::apply_capture_mode` must leave nothing content-bearing behind in
   `metadata_only` mode.
6. Preserve provider identifiers (`provider_session_id`, `provider_turn_id`,
   `tool.call_id`, `agent.provider_agent_id`) alongside canonical ids, never in
   place of them.
7. Ship fixtures under `fixtures/<provider>/` for every event name the adapter
   handles, plus golden normalised envelopes that tests compare against.

Adapters live in `crates/attemptdb-adapters`. New providers must be added
through the same contract and test suite; an adapter cannot bypass schema
validation or the privacy rules below.

## Fixtures rule: no real private payloads

Fixtures must be synthetic or scrubbed. They must not contain:

- real prompts, messages, or transcripts;
- real command lines, file contents, or tool output;
- email addresses, home directory paths, user names, or hostnames;
- tokens, API keys, session cookies, or other credentials;
- repository names, remotes, or paths other than `attemptdb` itself.

Payload *shape* matters; payload *content* does not. Replace values with
obviously synthetic placeholders and keep the structure the provider actually
emits. The same rule applies to screenshots, issue reports, benchmark data,
and demo datasets.

## Privacy canaries

Pull requests touching an adapter, the capture runtime, storage, export, or
sync must include or extend privacy canary tests: known marker strings placed
in prompts, commands, tool output, environment, and paths that must never
appear in `attrs`, in a `metadata_only` event, in an exported snapshot, or in
any synced payload. A canary leak is a blocking failure.

## RFC and ADR process

Design changes go through RFCs before code:

- RFCs live in `docs/rfcs/NNNN-title.md`, numbered sequentially.
- Each RFC starts with a header block (Title, Status, Authors, Created) and
  ends with a **Decisions** list and an **Open questions** list.
- Status flow: `Draft` → `Accepted` → `Implemented`, or `Superseded` (naming
  the successor).
- Architecture Decision Records live in `docs/adr/NNNN-title.md` and record
  context, decision, and consequences for choices that do not need a full RFC.

Changes to the canonical schema (`CANONICAL_SCHEMA_VERSION`, the field-id
table in `schema.rs`), the storage format, the WAL/spool frame layout, the
manifest, the `.atdb` container, or the IPC protocol require an RFC update in
the same pull request. Numeric field ids are never reused.

## Commit conventions

- One logical change per commit; keep refactors separate from behaviour
  changes.
- Subject line in the imperative mood, at most 72 characters, optionally
  prefixed with the crate or area (`storage: ...`, `adapters/codex: ...`,
  `rfc-0002: ...`).
- Body explains *why*, references the RFC or issue, and states any format or
  schema version bump.
- Code, comments, and documentation are written in English.

## Developer Certificate of Origin

Contributions must be signed off to certify the
[Developer Certificate of Origin 1.1](https://developercertificate.org/):

```text
Developer Certificate of Origin
Version 1.1

By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the best
    of my knowledge, is covered under an appropriate open source
    license and I have the right under that license to submit that
    work with modifications, whether created in whole or in part
    by me, under the same open source license (unless I am
    permitted to submit under a different license), as indicated
    in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including all
    personal information I submit with it, including my sign-off) is
    maintained indefinitely and may be redistributed consistent with
    this project or the open source license(s) involved.
```

Add the sign-off with `git commit -s`, which appends a line of the form:

```text
Signed-off-by: Your Name <you@example.com>
```

Use a real name and a reachable address. Unsigned commits are not merged.

## Reporting security issues

Do not open public issues for vulnerabilities. See [SECURITY.md](SECURITY.md).
