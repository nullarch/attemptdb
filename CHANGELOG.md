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

## [0.1.1] — 2026-08-31

Two Linux-only defects in `attempt update`, both found by CI within hours of
v0.1.0 and neither reachable on macOS, which is why every manual check of the
release missed them.

### Fixed

- `attempt update` could fail on Linux with "Text file busy". Linux refuses to
  `execve` a file any process still holds open for writing; spawning forks, and
  a child forked by one thread inherits a write handle another thread is about
  to close, which keeps the freshly staged binary unexecutable for a few
  milliseconds. `update::spawn_executable` retries on `ETXTBSY` for up to two
  seconds and the health check goes through it.
- The daemon respawn had the same race with a worse outcome. On the fallback
  branch — no launchd or systemd unit, so nothing else restarts the daemon —
  the spawn result was discarded, so `attempt update` could report **success**
  while leaving the capture daemon stopped. It now retries, and a failure is
  reported in the output and in `--json` instead of being swallowed.
- Both installers refused nothing when verification was impossible: a missing
  `SHA256SUMS`, or (in `install.sh`) no `sha256sum` and no `shasum`, warned and
  installed anyway. For a script run as `curl … | sh` that turns a hard failure
  into a silent unverified install, so both now refuse.
  `ATTEMPTDB_INSECURE_SKIP_CHECKSUM=1` is the deliberate override.
- `attempt update` could report the pid of a daemon it had not restarted. The
  service manager's unit is registered per user and `restart_service` ignores
  the locator, so a restart through it always bounces the user's daemon — while
  the status query afterwards is scoped to `--data-dir`/`--db`. With a scoped
  locator those are two different processes, and one success line named the
  wrong one. No pid is reported in that case now, with a line saying why.
- Both installers advertised `cargo install attemptdb` on the path where
  release resolution had already failed — a second failing command, since the
  crates are not published. They now print the clone-and-build commands, which
  work.

### Changed

- Build provenance attestation is required rather than best-effort. It was
  `continue-on-error` because attestation is an Enterprise feature on private
  repositories; this one is public and the step succeeded for every v0.1.0
  archive, so a release that cannot attest its artifacts now fails instead of
  shipping them unattested.

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

[Unreleased]: https://github.com/nullarch/attemptdb/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/nullarch/attemptdb/releases/tag/v0.1.1
[0.1.0]: https://github.com/nullarch/attemptdb/releases/tag/v0.1.0
