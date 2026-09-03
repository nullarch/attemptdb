# Changelog

All notable changes to AttemptDB are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) and is pre-1.0, so
minor versions may carry breaking changes until 1.0.

On-disk format versions are tracked separately in
[`docs/storage-format.md`](docs/storage-format.md) §13 and change only with an
RFC; a release that bumps one says so here.

## [Unreleased]

## [0.2.1] — 2026-09-03

### Added

- **Outbound webhook.** `--webhook-url` / `--webhook-secret`: after each
  accepted batch the server delivers the new events to the product's
  endpoint, HMAC-SHA256 signed, from a durable per-tenant cursor (pages of
  500, retries, a 60 s sweep, catch-up after a restart). `/v1/health`
  reports the counters. The product applies its own rules to the events;
  the server knows none of them.
- Keys record `issued_at`; `/v1/devices` and the webhook expose it as the
  device's pairing time.
- The VibeMon installers accept the older install command's `vbm_…` API
  key by exchanging it for a pairing token at the web first, so the
  canonical `/install.sh` can serve them without breaking commands already
  in people's hands.

### Fixed

- `attempt daemon install` on macOS retries `launchctl bootstrap` through
  launchd's asynchronous teardown of the previous registration ("Input/
  output error" on the first upgrade of a running daemon).

## [0.2.0] — 2026-09-02

The one-line install release: a product's web page mints a one-time
pairing token, and `attempt sync connect --pair` turns it into a device key
bound to this machine's own device id, proven before it is saved.

### Added

- **Pairing** (RFC 0006 §10). Server: `POST /v1/admin/pairings` mints a
  `pair_…` token (digest only on disk, 10-minute default TTL, one use);
  `GET /v1/pair/{token}` reports valid / expired / used / unknown; `POST
  /v1/pair` exchanges the token plus the local `device_id` for a device key
  bound to that id, retiring the same device's earlier keys. Device:
  `attempt sync connect --pair <token>` (or `--key`), with an authenticated
  handshake (an empty batch under the key: `401` unknown, `403` another
  device's) before the key is saved, and the previous connection restored
  on failure.
- **The operator's read.** The admin token plus `X-AttemptDB-Tenant`
  reads any tenant, so a product backend needs no reader key per tenant.
- **Rate limiting.** A token bucket per client address on the public
  pairing routes and per bearer key elsewhere (`--rate-limit`,
  `--pair-rate-limit`; `429` with `Retry-After`).
- `attempt doctor` shows the sync peer, masked key, profile, interval and
  last sync; `--remove-legacy vibemon` recognises the Windows client's
  `notify.py` / `notify.ps1` hook entries.
- The VibeMon installers (`docs/migration/vibemon-install.{sh,ps1}`) are
  token-first and sync-success-first, pin the AttemptDB release they were
  written against, and exit 0 without changing anything when run with no
  token on a machine that was never connected.
- CI runs a RustSec audit.
- **Work conflicts** (`conflict-v0`, RFC 0003 §5.8): two open work units of
  one project editing the same file in overlapping windows, neither committed
  since. Per shared path: each side's edit size and commit state; evidence is
  the edit events. Surfaced as the `conflicts` SQL table, `/v1/work`'s
  `conflicts`, and `/v1/attention` items with `reason = "work_conflict"`.
- **Countable test signals.** Adapters read a test runner's summary line
  (cargo, nextest, jest, vitest, pytest, mocha, rspec, phpunit, dotnet,
  `go test -v`) into `attrs.tests_passed` / `tests_failed` / `tests_skipped`;
  `/v1/work` carries a work unit's newest test run and build as `signal`.
- **Server read API for a console:** `GET /v1/live` (newest event and active
  sessions, answered from facts kept next to the writer), `GET
  /v1/events/{id}`, `POST /v1/corrections` (a reader or admin key records a
  correction or retraction, attributed to its user), and `user_id` /
  `users` on sessions and work units from the tenant's device keys.
- `tool_calls` gains `lines_added` / `lines_removed`; `attempt` CLI reads
  through the daemon's resident engine (IPC `QUERY`/`RESULT`) when one
  serves the database.

### Changed

- **Sync defaults:** upload profile `semantic`, interval 5 s. Each upload
  tick reads only past its cursor and never opens content blobs unless the
  profile sends content; the inference set is recomputed only after a tick
  that uploaded something.
- **`ALGORITHM_VERSION` is `tier1-v1`.** Work-unit rule 1 (shared path) no
  longer links turns of different sessions whose active spans overlap:
  concurrent sessions on one file are two units (and a conflict), sequential
  ones remain continuity. Every other Tier 1 entity is computed as before;
  device-uploaded `tier1-v0` items are superseded by the server's `tier1-v1`
  under the merge rule.
- The read path keeps segments as Arrow only, derives per-segment facts and
  id maps from the columns, builds the SQL layer and each projection table
  on first use, resolves content only for the rows and columns a reader
  asks for, and carries a per-session index on the projection. Measured at
  200 k events: first read after a change 432 → 44–117 ms, resident memory
  1,996 → ~800 MiB, `STATE … AT` from 188 ms to run noise.

## [0.1.2] — 2026-08-31

### Fixed

- **v0.1.1's binaries reported themselves as `0.1.0`.** The tag was cut without
  bumping `[workspace.package] version`, and every path in the release workflow
  derives the version from the tag name — so the archives were *named* 0.1.1
  while the binary inside was 0.1.0, and `attempt update` went on offering
  0.1.1 to someone who had just installed it. v0.1.1 should not be used; this
  release carries the same fixes with the version it claims.
- The release workflow now refuses a tag that disagrees with the workspace
  version, before it builds anything. Nothing checked that before, because
  every step took the version from the tag and none of them from the crates.

## [0.1.1] — 2026-08-31

**Superseded by 0.1.2 — do not use.** Its binaries report `0.1.0`, which puts
`attempt update` in a loop. The fixes below shipped correctly in 0.1.2.


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

[Unreleased]: https://github.com/nullarch/attemptdb/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/nullarch/attemptdb/releases/tag/v0.2.1
[0.2.0]: https://github.com/nullarch/attemptdb/releases/tag/v0.2.0
[0.1.2]: https://github.com/nullarch/attemptdb/releases/tag/v0.1.2
[0.1.1]: https://github.com/nullarch/attemptdb/releases/tag/v0.1.1
[0.1.0]: https://github.com/nullarch/attemptdb/releases/tag/v0.1.0
