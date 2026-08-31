# Changelog

All notable changes to AttemptDB are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) and is pre-1.0, so
minor versions may carry breaking changes until 1.0.

On-disk format versions are tracked separately in
[`docs/storage-format.md`](docs/storage-format.md) §13 and change only with an
RFC; a release that bumps one says so here.

## [Unreleased]

Nothing yet.

## [0.1.0] — 2026-08-31

The first tagged release: the whole local pipeline — capture, storage, query,
projections, MCP, UI, sync — in one binary, plus the sync server.

### Added

- Segment compaction: contiguous runs of small segments merge into one through
  a new manifest generation, crash-safe at three injection points; `attempt
  compact [--dry-run]`. The daemon compacts after each periodic flush and the
  sync server when a tenant is flushed and closed.
- `attempt-hook`: a dedicated hook entrypoint that links only the capture
  crate — 0.8 MB against the CLI's 76 MB, which takes hook wall time from
  6.6 ms to 4.2 ms (macOS ARM64, 40 runs including process spawn).
  `attempt hook install` prefers it when it sits next to `attempt`, and
  `attempt update` installs and rolls back the pair.
- Commit linkage: a successful `git commit` tool call is tied to the sha
  `HEAD` moved to, using only the repository head the hook already records —
  no command output is read. New `commits` projection and query table,
  `SHOW COMMITS`, and `commit_shas` on attempts and work units.
- Sync peers and profiles: several servers per device, each with its own
  cursor, interval, repository policy, and profile (`metadata_only`,
  `semantic`, `full`). The daemon re-reads its configuration every tick, so
  connecting or changing a peer needs no restart.
- Sync server read API: `GET /v1/sessions`, `/v1/timeline`, `/v1/work`,
  `/v1/attention`, `/v1/state`, `/v1/events`, `/v1/devices`, `/v1/status` and
  `POST /v1/query`, served from a per-tenant engine cache shared with the
  local UI and MCP server. Documented in [`docs/server-api.md`](docs/server-api.md).
- Key scopes (`device`, `reader`, `admin`) with an optional user binding, and
  `DELETE /v1/admin/devices/{id}`, which revokes a device's keys and retracts
  its sessions from every projection while leaving the facts on disk.
- `attempt import vibemon-export`: deterministic, idempotent backfill of a
  legacy `hook_events` export.
- On-disk compatibility fixture and test suite: a database and snapshot
  written by an earlier build are read, continued, and restored by the current
  one, and an unknown format version is refused rather than misread.
- Deployment files (`deploy/`) and [`docs/deploy.md`](docs/deploy.md).

### Changed

- The command-line crate is published as `attemptdb` (the crates.io name
  `attempt` belongs to an unrelated crate). The installed binary is still
  `attempt`: `cargo install attemptdb`.

### Security

- Secret scanning (`secrets-v1`) drops attribute values containing a
  credential at ingest and redacts content before any upload.

[Unreleased]: https://github.com/nullarch/attemptdb/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/nullarch/attemptdb/releases/tag/v0.1.0
