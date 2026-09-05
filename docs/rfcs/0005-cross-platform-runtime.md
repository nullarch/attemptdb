# RFC 0005: Cross-Platform Runtime

| | |
|---|---|
| **Status** | Draft |
| **Authors** | AttemptDB maintainers |
| **Created** | 2026-08-28 |
| **Related** | RFC 0001 (canonical event model), RFC 0002 (storage engine), RFC 0006 (privacy and sync), `docs/storage-format.md`, `docs/compatibility-matrix.md` |

This RFC specifies how the single `attempt` binary behaves as a CLI, a hook
entrypoint, a daemon, an MCP server, and a UI server on macOS, Windows, and
Linux; where it stores data on each operating system; how hook processes talk
to the daemon; what happens when the daemon is not running; and what an
installer may and may not do to a user's coding-agent configuration.

Implementation status: the `attemptdb-capture` crate that will implement this
RFC is currently a one-line stub. Everything below is the contract the crate is
being built to. Items that are not required for the first public release are
marked **planned**.

## 1. Motivation and the one-binary principle

Coding-agent hooks are executed on the hot path of the agent. A hook that is
slow, that depends on an interpreter which may be missing, or that fails
loudly, degrades the agent the user is actually trying to use. The runtime
therefore has one non-negotiable shape:

- `attempt` is the **only** executable AttemptDB requires. No Bash, Python,
  Node.js, Docker, or cloud account is needed to install, capture, query, or
  uninstall.
- The same binary provides every mode (CLI, hook, daemon, MCP, UI). Modes are
  selected by subcommand, not by separate installs.
- Hook startup overhead target: **p95 < 10 ms** from process start to the
  event being handed to the daemon or written to the spool, measured on each
  Tier 1 platform and excluding whatever the host agent itself spends on the
  hook mechanism. This target is enforced by a benchmark in CI (planned).
- The hook path never blocks on inference, indexing, sync, or UI work. The
  only thing an acknowledgment depends on is the WAL durability policy in
  RFC 0002.

## 2. Binary modes

| Mode | Invocation | Responsibility | Holds DB write lock | Reads DB |
|---|---|---|---|---|
| CLI client | `attempt <command>` (`status`, `timeline`, `query`, `why`, `trace`, `failures`, `handoffs`, `snapshot`, `doctor`, `hook install/status`, `init`, `update`, `uninstall`) | User-facing commands; talks to the daemon over IPC; falls back to read-only engine embedding when no daemon runs (**planned**) | No | Via daemon, or read-only from the latest durable manifest |
| Hook entrypoint | `attempt hook <provider> <event>` | Reads the provider's JSON payload from stdin, normalises it with the provider adapter, sends it to the daemon, spools on failure | No | No |
| Daemon | `attempt daemon [--foreground]` | The single database writer: IPC server, WAL, memtable, segment flush, manifest, spool import, projections | **Yes** (the `LOCK` file in `.attemptdb/`) | Yes |
| MCP server | `attempt mcp` | Model Context Protocol server over stdio for agents that want to query AttemptDB | No | Via daemon |
| UI server | `attempt ui` | Serves the local web UI from the daemon on an authenticated random loopback port and opens the system browser | No (the daemon serves it) | Via daemon |

Rules that apply to every mode:

- Exactly one process holds the write lock for a given `.attemptdb/`
  directory. That process is the daemon. If `attempt daemon` finds the lock
  held by a live process it exits with a clear message and non-zero status;
  if the lock holder is dead the stale lock is reclaimed (see RFC 0002).
- The hook entrypoint must exit **0** unless the provider's hook protocol
  assigns meaning to a non-zero exit (for example, blocking a tool call). An
  AttemptDB failure must never be turned into a provider-visible hook failure.
- The hook entrypoint has a bounded total budget (default 2 000 ms, always
  below the provider-side timeout the installer configures). Within that
  budget it tries IPC once; on any failure it appends to the spool and exits.
- All modes resolve paths with the rules in section 3 and honour
  `--data-dir` / `ATTEMPTDB_DATA_DIR` / `ATTEMPTDB_DIR` identically.
- The hook entrypoint never writes logs to stdout. The provider may treat
  stdout as hook output. Diagnostics go to the log directory (section 3) at
  `warn` level and above only.

### 2.1 Hook entrypoint contract

```text
attempt hook <provider> <provider-event-name>
    stdin : provider JSON payload (UTF-8, one document)
    stdout: empty, or the provider-specific response document if the provider
            protocol requires one (never AttemptDB diagnostics)
    exit  : 0, except where the provider protocol requires otherwise
```

Steps, in order:

1. Parse arguments. Unknown provider names are accepted and mapped to
   `Provider::Other(<name>)`; unknown event names are accepted and mapped to
   `EventKind::Unknown`. Nothing is dropped for being unrecognised.
2. Read stdin fully (bounded at 8 MiB; larger payloads are truncated with
   `attrs.payload_truncated = true` and the original size recorded).
3. Run the adapter to produce a canonical `Event` (RFC 0001). The adapter
   applies the capture mode (`Event::apply_capture_mode`) before the event
   leaves the hook process, so content-bearing fields never reach IPC or the
   spool in `metadata_only` mode.
4. Try IPC (section 4) with a connect + round-trip deadline that leaves at
   least 200 ms of the budget for the spool path.
5. On IPC failure, or on no acknowledgment within the deadline, append the
   event to a spool file (section 5) and exit.

## 3. Standard paths

### 3.1 Resolution rules

Two different things are resolved, and they are resolved independently:

- The **data root** is the per-user AttemptDB home. It is resolved as:
  `--data-dir <path>` > `ATTEMPTDB_DATA_DIR` > OS default (tables below).
- The **live database directory** (the `.attemptdb/` directory defined in
  RFC 0002) is resolved as: `ATTEMPTDB_DIR` (explicit path to a database
  directory) > a project-local `.attemptdb/` found by walking up from the
  current working directory to the filesystem root > `<data root>/db/.attemptdb`.

Config, cache, runtime (socket/pipe), and logs are separate directories with
OS-appropriate locations. In **portable mode** (`--data-dir` or
`ATTEMPTDB_DATA_DIR`) every one of those lives under the single data root:

```text
<data root>/
  config/           configuration files
  db/.attemptdb/    the default live database (RFC 0002 layout)
  cache/            rebuildable caches (indexes, UI assets)
  run/              socket / token files
  logs/             rotating daemon and hook logs
```

Portable mode is what makes a database on a USB drive or in a CI checkout
self-contained: nothing is written outside the root.

### 3.2 macOS

| Purpose | Path |
|---|---|
| Data root | `~/Library/Application Support/AttemptDB` |
| Config | `~/Library/Application Support/AttemptDB/config` |
| Default live DB | `~/Library/Application Support/AttemptDB/db/.attemptdb` |
| Cache | `~/Library/Caches/AttemptDB` |
| Logs | `~/Library/Logs/AttemptDB` |
| Runtime socket | `~/Library/Application Support/AttemptDB/run/attemptdb.sock` |

macOS limits `sun_path` to 104 bytes. If the resolved socket path exceeds
that (long user names, portable mode under a deep directory), the daemon
falls back to `$TMPDIR/attemptdb-<uid>/attemptdb.sock` and records the
effective path in `<data root>/run/endpoint.json` so hooks and clients find
it without recomputing the fallback.

### 3.3 Windows

| Purpose | Path |
|---|---|
| Data root | `%LOCALAPPDATA%\AttemptDB` |
| Config | `%APPDATA%\AttemptDB` |
| Default live DB | `%LOCALAPPDATA%\AttemptDB\db\.attemptdb` |
| Cache | `%LOCALAPPDATA%\AttemptDB\cache` |
| Logs | `%LOCALAPPDATA%\AttemptDB\logs` |
| Runtime endpoint | Named pipe `\\.\pipe\attemptdb-<user-sid-hash>` |

`<user-sid-hash>` is the lowercase hex of the first 8 bytes of SHA-256 over
the current user's SID string. It keeps pipe names per-user without leaking
the SID itself. Config lives in the roaming profile so hook configuration
follows the user; data, cache, and logs stay local because they can be large
and are device-specific.

### 3.4 Linux

| Purpose | Path |
|---|---|
| Data root | `$XDG_DATA_HOME/attemptdb` → `~/.local/share/attemptdb` |
| Config | `$XDG_CONFIG_HOME/attemptdb` → `~/.config/attemptdb` |
| Default live DB | `<data root>/db/.attemptdb` |
| Cache | `$XDG_CACHE_HOME/attemptdb` → `~/.cache/attemptdb` |
| Logs | `$XDG_STATE_HOME/attemptdb/logs` → `~/.local/state/attemptdb/logs` |
| Runtime socket | `$XDG_RUNTIME_DIR/attemptdb/attemptdb.sock` → `/tmp/attemptdb-<uid>/attemptdb.sock` |

The arrow means "if the variable is unset or empty, use the fallback". The
`/tmp` fallback directory is created with mode `0700` and its ownership is
verified before use; if it exists with different ownership the daemon refuses
to start rather than share a socket directory.

## 4. IPC

### 4.1 Transports

| Platform | Transport | Endpoint | Security |
|---|---|---|---|
| macOS, Linux | Unix domain socket | `<runtime dir>/attemptdb.sock`; when that path exceeds the platform `sun_path` limit (portable data dirs under long temp paths), both sides deterministically fall back to `<temp dir>/attemptdb-<uid>/<hash(runtime dir)>.sock` | Containing directory `0700`, socket `0600`; the daemon reads peer credentials (`UnixStream::peer_cred()`) and rejects a different uid |
| Windows | Named pipe | `\\.\pipe\attemptdb-<hash(runtime dir)>` (the runtime dir lives under `%LOCALAPPDATA%`, so the name is per user) | Default pipe DACL (current user); an explicit SID-restricted DACL and `PIPE_REJECT_REMOTE_CLIENTS` are planned |
| Any (fallback) | Authenticated loopback TCP | **planned**, disabled; `Endpoint` is the extension point | per-install token, constant-time compare |

The daemon writes `<runtime dir>/endpoint.json` (`transport`, `path`,
`protocol_version`, `pid`) for humans and `attempt daemon status`. Hooks do
**not** read it: they compute the endpoint deterministically and check its
existence with a single `stat`, so the no-daemon fast path costs one
syscall. A stale socket (daemon crashed) yields `ECONNREFUSED` immediately and
the hook spools.

### 4.2 Framed, versioned protocol

Every connection begins with an 8-byte prelude from the client, then a
stream of frames in both directions. All integers are little-endian.

```text
Prelude (8 bytes, client → daemon, once per connection)
+----+----+----+----+---------------------+--------------+
| 'A'| 'T'| 'I'| 'P'| protocol_version u16| flags u16    |
+----+----+----+----+---------------------+--------------+
  magic "ATIP"          = 1                 = 0

Frame (12-byte header + payload) — identical to a WAL record header
+---------------+---------------+------+-------+-----------+
| len u32       | crc32c u32    | type | codec | flags u16 |
+---------------+---------------+------+-------+-----------+
| payload (len bytes)                                       |
+-----------------------------------------------------------+
```

- `len` is the payload length (header excluded); a receiver rejects
  `len > 16 MiB` *before* allocating.
- `crc32c` covers `type ‖ codec ‖ flags ‖ payload`, exactly like WAL and
  spool records (`docs/storage-format.md` §5.2); a mismatch is a
  `protocol_error`.
- `type` (u8): `1` HELLO, `2` INGEST, `3` ACK, `4` NACK, `5` PING, `6` PONG,
  `7` reserved (QUERY, planned), `8` HELLO_ACK, `9` SHUTDOWN.
- `codec` (u8): `1` JSON (UTF-8); the same id space as the WAL codec.
- `flags` (u16): must be `0`.

Payloads (JSON, codec 1):

| Type | Direction | Payload |
|---|---|---|
| HELLO (1) | client → daemon | `{ client: "hook" \| "cli" \| "mcp" \| "ui", client_version, protocol_version, device_id?, db_dir, spooled? }`. `db_dir` names the database the client resolved (RFC 0005 §3.1); a daemon serving a different database answers NACK `wrong_database` and the client spools into its own database — a project-local `.attemptdb/` and the per-user daemon therefore never mix. |
| HELLO_ACK (8) | daemon → client | `{ daemon_version, protocol_version, pid, db_id, device_id, schema_version, format_version, capture_mode, db_dir }` |
| INGEST (2) | client → daemon | JSON array of canonical `Event` documents with `source_seq = 0`, `hlc = 0`; the daemon assigns both. |
| ACK (3) | daemon → client | `{ accepted: [event_id…], duplicate: [event_id…], rejected: [{ event_id, reason }], durable_source_seq }`. Sent after `Database::ingest` returned, i.e. after the WAL fsync under `strict` durability; under `--relaxed` `durable_source_seq` is the last *assigned* sequence. A `duplicate` entry is success: the event is already stored. |
| NACK (4) | daemon → client | `{ code, message, retryable }`; codes: `hello_required`, `wrong_database`, `unknown_message_type` (connection stays open), `protocol_error` (bad magic, CRC, oversize; connection closed), `unsupported_protocol`, `ingest_failed`. |
| PING (5) / PONG (6) | either / daemon | PONG carries `DaemonStatus` (pid, version, endpoint, db_dir, data_dir, log_path, device_id, capture_mode, durability, started_at, ingest/spool/storage counters). |
| SHUTDOWN (9) | client → daemon | empty; the daemon answers ACK, waits for the client to hang up, flushes, removes socket and pid file, exits. |

A hook writes prelude + HELLO + INGEST in one `write` and reads HELLO_ACK +
ACK: one round trip. Default client deadlines are 25 ms to connect and
100 ms per round trip; on any failure the batch goes to the spool. The
writer task inside the daemon **group-commits**: INGEST batches that arrive
while a WAL sync is in progress share the next append and fsync.

Protocol versioning: an unsupported `protocol_version` in the prelude gets
NACK `unsupported_protocol` and the connection is closed; clients spool and
the next daemon upgrade imports the spool. New optional JSON keys do not
bump the version; new frame types or changed semantics do.

## 5. Spool fallback and idempotent ingestion

### 5.1 When the daemon is unreachable

If the hook cannot connect, cannot complete the handshake, or does not
receive an `ack` within its deadline, it appends the batch to a spool file:

```text
<live db dir>/spool/inbox.spool   (shared, appended under spool/inbox.lock)
```

- One spool file per hook process. Hook processes are short-lived, so in
  practice this is one file per event; the file name makes concurrent hooks
  collision-free without locking.
- The spool file format is byte-for-byte the WAL frame format defined in
  `docs/storage-format.md` with the file magic `ATSP` instead of `ATWL`.
  Records are complete canonical events with `source_seq = 0` and `hlc = 0`.
- The hook writes the header and the record, calls `fsync` on the file, and
  exits. It does not fsync the directory (the daemon's import tolerates a
  missing file after a crash; the event is then genuinely lost and counted as
  a capture gap, which is the honest outcome).
- If the live database directory cannot be resolved or created, the hook
  writes to `<data root>/spool-orphan/` with the same naming and the daemon
  imports from there too, attributing events by their embedded project data.

### 5.2 Import on recovery

The daemon imports spool files:

1. On start, after WAL recovery and before accepting IPC connections.
2. Periodically while running (default every 5 s, and immediately when a
   `hello` from a hook client mentions `"spooled": true`).

The writer claims the inbox by renaming it to `claimed-<uuidv7>.spool` under
`inbox.lock`; claimed files are imported in name order (UUIDv7, i.e. claim
order) and, within a file, in record order. Recovery of a spool file follows
the WAL rule: scan until EOF, truncation, or CRC mismatch; import every good
record before the first bad one; never discard earlier valid records.

A claimed file is deleted only after its events are durable in the WAL. A
claimed file found at start (crash between import and delete) is re-imported
— idempotency by `event_id` makes this safe — and then deleted on the same
rule. Hook processes never fsync the inbox by default (`config.spool_sync`);
the spool is a transport and the WAL is the durability boundary.

### 5.3 Idempotency

Ingestion is idempotent by `event_id`:

- The daemon keeps an index of event ids it has accepted (memtable + segment
  bloom filters, **planned**; a full scan of recent segments until then).
- A batch containing an already-accepted `event_id` is acknowledged with that
  id under `duplicate`; the event is not appended again and its original
  `source_seq` and `hlc` are unchanged.
- `source_seq` is assigned exactly once, by the single writer, at the moment
  the event is appended to the WAL. A spooled event that is imported later
  receives a `source_seq` later than events ingested live in the meantime;
  its `observed_at` and `captured_at` are unchanged. Readers that want
  capture order use `source_seq`; readers that want provider time use
  `observed_at` (RFC 0001).

### 5.4 Coverage and gaps

Capture completeness is measured, not assumed:

- Every session receives a **coverage grade** (**planned**; defined in
  RFC 0003) derived from the events actually observed versus the events the
  provider's hook set can emit.
- When the hook path itself knows it lost something (payload truncated,
  spool write failed, adapter parse error), it emits what it can with
  `attrs.capture_gap` set to a content-free reason (`payload_truncated`,
  `spool_write_failed`, `adapter_parse_error`, `provider_timeout`). A gap is
  a fact about capture and is stored as such, never silently omitted.
- `attempt doctor` and `attempt status` display the last import time, the
  count of pending spool files, and any `spool-orphan` content.

## 6. Background service

The daemon must run without a privileged system service. Each OS gets its
per-user mechanism; every mechanism can be replaced by running
`attempt daemon --foreground` in any process supervisor.

| OS | Mechanism | Unit / registration | Notes |
|---|---|---|---|
| macOS | launchd per-user LaunchAgent | `~/Library/LaunchAgents/dev.attemptdb.daemon.plist` with `RunAtLoad = true`, `KeepAlive = { SuccessfulExit = false }`, `ProcessType = Background`, stdout/stderr to the logs dir | Installed and loaded with `launchctl bootstrap gui/<uid>`; unloaded with `bootout`. No `/Library/LaunchDaemons`. |
| Windows | Per-user background process | Registered at logon via Task Scheduler (`\AttemptDB\Daemon`, run whether or not user is logged on = **no**, current user only) or `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`; an optional Windows Service is **planned** for shared machines | Runs with the user's token; no elevation prompt on install. |
| Linux | `systemd --user` | `~/.config/systemd/user/attemptdb.service` (`Type=simple`, `Restart=on-failure`, `ExecStart=<abs path>/attempt daemon --foreground`), enabled with `systemctl --user enable --now attemptdb` | Fallback when systemd is absent: an XDG autostart entry (`~/.config/autostart/attemptdb.desktop`) or the user's own supervisor running `--foreground`. |

Lifecycle commands (all implemented by the `attempt` binary, **planned**):

- `attempt daemon start | stop | restart | status` — talk to the OS
  mechanism, then verify via IPC `ping`.
- **Stop**: send the daemon a shutdown request over IPC; it stops accepting
  connections, flushes the memtable to a segment, writes a manifest
  generation, releases `LOCK`, exits 0. A SIGTERM / console control event
  triggers the same path. Hooks arriving during shutdown spool.
- **Restart / upgrade**: the new binary is placed next to the old one and
  swapped with an atomic rename (Windows: rename the running file aside, put
  the new file in place, delete the old on next start). The service is then
  restarted. Events emitted during the gap spool and are imported by the new
  daemon. Old-format databases are opened read-only until an explicit
  `attempt migrate` runs (RFC 0002).
- **Uninstall**: `attempt uninstall` stops the daemon, removes the service
  registration, removes AttemptDB's hook entries from each provider config
  (section 8) and, only with `--purge`, deletes the data root. Without
  `--purge` the database is left in place and its location printed.

## 7. Release targets and distribution

All items in this section are **planned**; none exist yet.

Release targets (exactly these eight; a release is not cut unless all eight
build and pass their native smoke tests):

| Target | Triple |
|---|---|
| macOS ARM64 | `aarch64-apple-darwin` |
| macOS x86_64 | `x86_64-apple-darwin` |
| Windows 10/11 x86_64 MSVC | `x86_64-pc-windows-msvc` |
| Windows 11 ARM64 MSVC | `aarch64-pc-windows-msvc` |
| Linux glibc x86_64 | `x86_64-unknown-linux-gnu` |
| Linux glibc ARM64 | `aarch64-unknown-linux-gnu` |
| Linux musl x86_64 static | `x86_64-unknown-linux-musl` |
| Linux musl ARM64 static | `aarch64-unknown-linux-musl` |

Distribution channels:

| Channel | Platform | Notes |
|---|---|---|
| Homebrew formula | macOS (and Linuxbrew) | Bottles for both macOS targets |
| Signed and notarized DMG / PKG | macOS | Developer ID signature, notarization stapled |
| Signed MSI / EXE | Windows | Authenticode signature; SmartScreen reputation is earned over time and documented honestly |
| Winget package | Windows | Manifest submitted per release |
| Scoop manifest | Windows | Optional |
| Signed tarballs | Linux | `.tar.zst` per target with detached signature |
| `.deb` / `.rpm` | Linux | Per architecture; installs the binary and the user unit template only |
| AppImage | Linux | Only for the optional desktop shell, not for the core binary |
| Checksums + provenance | All | `SHA256SUMS` plus a signed provenance statement per release |
| Auto-update | All | **Implemented** (`attempt update`, and since 0.2.8 the daemon on its own): the release policy `update.json` is read once a day; a required release is installed at once, an optional one at a quiet moment; SHA-256 verified against `SHA256SUMS`, health-checked before and after the swap, previous binary kept as `attempt.prev` and restored automatically on failure or with `--rollback`; `auto_update` on / required / off |
| Offline / manual install | All | Documented steps: copy binary, `attempt init`, `attempt hook install` |

## 8. Installer safety

`attempt hook install` edits files owned by other programs. The rules below
are mandatory for every provider adapter and every platform.

1. **Detect before mkdir.** The installer decides that a provider is present
   by finding its existing configuration directory or binary. It never
   creates `~/.claude`, `~/.codex`, `~/.cursor`, or `~/.gemini` to have a
   place to write a hook. A provider that is not detected is reported as
   `not installed` and skipped unless `--force-provider <name>` is given.
2. **Structural edits only.** JSON is parsed with `serde_json` using
   `preserve_order` and written back through the same document; TOML is
   edited with `toml_edit` so comments, ordering, and formatting survive.
   Blind text replacement is forbidden. The installer touches only the keys
   it owns (`hooks.<Event>[...]` entries whose command starts with the
   AttemptDB binary path).
3. **Lock, back up, atomic replace.** For each config file: take an advisory
   lock (RFC 0002 file-locking abstraction), copy the file to
   `<file>.attemptdb-backup-<UTC timestamp>` (keep the last 5), write the new
   content to a temp file in the same directory, `fsync` it, rename over the
   original, and `fsync` the directory where the platform allows. On Windows
   the rename uses `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`, and a
   failure leaves the original untouched.
4. **Byte-for-byte preservation** of unrelated content where the format
   allows it (TOML always; JSON as far as `preserve_order` and the original
   indentation style can be detected). Where exact preservation is impossible
   the installer says so in its output.
5. **Verify with a real event.** After writing configuration, the installer
   emits a `capture_test` event through the hook path for that provider and
   waits for it to become durable. The provider state is `unverified` until
   this succeeds. Where the provider must run the hook itself (Codex trust,
   Cursor), the installer explains what the user must do and how to re-run
   the verification (`attempt hook verify <provider>`).
6. **Idempotent.** Running install, upgrade, repair, or uninstall twice
   produces the same configuration as running it once. Upgrades replace only
   AttemptDB-owned entries. Uninstall removes only AttemptDB-owned entries and
   leaves every other hook in place.
7. **Never write provider trust state.** In particular, AttemptDB never
   writes `[hooks.state]` in `~/.codex/config.toml`.

### 8.1 `attempt doctor` states

`attempt doctor` reports, per provider, exactly one of the following states,
defined precisely so that the output can be tested:

| State | Definition |
|---|---|
| `not installed` | The provider was not detected (no config directory and no binary). |
| `configured` | The provider is detected and every expected AttemptDB hook entry is present in the provider's config with the correct command path and event names. No claim is made about whether the provider runs it. |
| `trusted` | `configured`, and the provider's own approval mechanism has accepted the hook where such a mechanism exists (Codex `/hooks` approval; Cursor hook enablement). For providers without an approval step this state is skipped and `configured` proceeds directly to `unverified` / `active`. |
| `unverified` | `configured` (and `trusted` where applicable), but no `capture_test` event and no real event has ever been received from this provider. |
| `active` | At least one real (non-`capture_test`) event from this provider has been ingested within the last 7 days (configurable `doctor.active_window_days`). |
| `stale` | Verified at some point (a `capture_test` or real event exists), but no real event within the active window. Typical causes: provider upgraded and dropped the hook, user stopped using the provider, config file replaced. |

`doctor` also reports: daemon reachability and transport, data root and live
database paths, capture mode, pending spool files, last successful import,
last manifest generation, and any backup files it left behind.

## 9. Provider configuration facts

The facts below are what the installer writes and what the adapter expects.
Verification levels use the vocabulary of `docs/compatibility-matrix.md`.

### 9.1 Claude Code — `documented` (official docs, Aug 2026)

Files: `~/.claude/settings.json` (user), `.claude/settings.json` and
`.claude/settings.local.json` (project). AttemptDB writes to the user file by
default and to a project file only with `--scope project`.

Shape:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "",
        "hooks": [
          { "type": "command", "command": "/usr/local/bin/attempt hook claude_code PostToolUse", "timeout": 5 }
        ]
      }
    ]
  }
}
```

`timeout` is in **seconds**. The installer writes one matcher group per event
with an empty matcher (all tools). Payload arrives on stdin as JSON.

### 9.2 Codex — `observed` (real `~/.codex/hooks.json`)

File: `~/.codex/hooks.json`. **Not** `~/.codex/settings.json`: hooks written
there are silently ignored by Codex. This was a real failed attempt in
AttemptDB's own history and is the canonical example in the README.

Shape is the same as Claude Code's: a top-level `hooks` object keyed by event
name, each an array of matcher groups with `hooks: [{ "type": "command",
"command": "...", "timeout": <seconds> }]`. `type` is **required**. Codex
**rejects unknown fields**, so the installer must not add AttemptDB-specific
keys anywhere in the document.

Trust: the user must approve the hook inside Codex via `/hooks`. Approval is
recorded as trust hashes in `~/.codex/config.toml` under `[hooks.state]`.
AttemptDB **never writes** that table; `attempt doctor` reads it only to
report `trusted` vs `configured`, and if it cannot parse it, reports
`configured` with a note.

Events observed in a real config: `SessionStart`, `SessionEnd`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PermissionRequest`,
`SubagentStart`, `SubagentStop`, `Stop`.

### 9.3 Cursor — `observed` (production installer; payloads partially verified)

File: `~/.cursor/hooks.json`. Flat shape, no matcher groups:

```json
{
  "version": 1,
  "hooks": {
    "afterFileEdit": [
      { "command": "/usr/local/bin/attempt hook cursor afterFileEdit", "timeout": 5 }
    ]
  }
}
```

`timeout` is in **seconds**. Events: `sessionStart`, `sessionEnd`,
`beforeSubmitPrompt`, `stop`, `afterFileEdit`, `afterShellExecution`,
`postToolUseFailure`. The installer must detect ghost entries (event names
Cursor no longer supports) during upgrade and report them rather than
rewriting them.

### 9.4 Gemini CLI — `observed` (production installer; payloads partially verified)

File: `~/.gemini/settings.json`. Hook entries carry a `name` and a `timeout`
in **milliseconds** (not seconds — the installer converts).

```json
{
  "hooks": {
    "AfterTool": [
      { "name": "attemptdb", "type": "command", "command": "/usr/local/bin/attempt hook gemini_cli AfterTool", "timeout": 5000 }
    ]
  }
}
```

Events: `SessionStart`, `SessionEnd`, `BeforeAgent`, `AfterAgent`,
`BeforeTool`, `AfterTool`. The exact set of accepted keys per entry is still
being verified against real payloads; the installer preserves any key it did
not write.

### 9.5 Windows command-line caveat

Provider hosts run hook commands through a shell (`cmd.exe` or PowerShell
depending on the host). To avoid quoting failures:

- Write the **absolute path** to `attempt.exe` and pass the provider and
  event name as plain arguments: `C:\Users\x\AppData\Local\AttemptDB\bin\attempt.exe hook claude_code PostToolUse`.
- Never embed JSON, quotes, or `&`, `|`, `<`, `>`, `^`, `%` in the command.
  A `%` in a path (`%LOCALAPPDATA%`) is expanded by `cmd.exe` but not by
  every host; write the expanded path.
- Do not wrap in `cmd /c` or `powershell -Command`; both add a quoting layer
  and measurable startup latency.
- If the install path contains spaces, the installer quotes the executable
  path only (`"C:\Program Files\AttemptDB\attempt.exe" hook ...`) and verifies
  with a `capture_test` event that the host actually launched it.

### 9.6 Provider event → canonical kind

RFC 0001 is the normative home of this table; it is repeated here so the
installer and the adapters are read against the same list.

| Provider | Provider event | Canonical kind |
|---|---|---|
| claude_code, codex | `SessionStart` | `session_started` |
| claude_code, codex | `SessionEnd` | `session_ended` |
| claude_code, codex | `UserPromptSubmit` | `prompt_submitted` |
| claude_code, codex | `PreToolUse` | `tool_call_started` |
| claude_code, codex | `PostToolUse` | `tool_call_finished` |
| claude_code, codex | `PostToolUseFailure` | `tool_call_failed` |
| claude_code, codex | `PermissionRequest` | `permission_requested` |
| claude_code, codex | `PermissionDenied` | `permission_denied` |
| claude_code, codex | `Notification` | `notification` |
| claude_code, codex | `Stop` | `turn_stopped` |
| claude_code, codex | `StopFailure` | `turn_failed` |
| claude_code, codex | `SubagentStart` | `subagent_started` |
| claude_code, codex | `SubagentStop` | `subagent_stopped` |
| claude_code, codex | `TaskCreated` | `task_created` |
| claude_code, codex | `TaskCompleted` | `task_completed` |
| claude_code, codex | `PreCompact` | `compaction_started` |
| claude_code, codex | `PostCompact` | `compaction_finished` |
| claude_code, codex | `ConfigChange` | `config_changed` |
| claude_code, codex | `CwdChanged` | `cwd_changed` |
| claude_code, codex | `FileChanged` | `file_changed` |
| claude_code, codex | `WorktreeCreate` | `worktree_created` |
| claude_code, codex | `WorktreeRemove` | `worktree_removed` |
| cursor | `sessionStart` | `session_started` |
| cursor | `sessionEnd` | `session_ended` |
| cursor | `beforeSubmitPrompt` | `prompt_submitted` |
| cursor | `stop` | `turn_stopped` |
| cursor | `afterFileEdit` | `tool_call_finished` |
| cursor | `afterShellExecution` | `tool_call_finished` (exit code 0) or `tool_call_failed` (non-zero) |
| cursor | `postToolUseFailure` | `tool_call_failed` |
| gemini_cli | `SessionStart` | `session_started` |
| gemini_cli | `SessionEnd` | `session_ended` |
| gemini_cli | `BeforeAgent` | `prompt_submitted` |
| gemini_cli | `AfterAgent` | `turn_stopped` |
| gemini_cli | `BeforeTool` | `tool_call_started` |
| gemini_cli | `AfterTool` | `tool_call_finished` |
| any | anything else | `unknown` — never dropped; `provider_event_name` keeps the original |

## 10. Cross-platform CI and failure-test matrix

A Tier 1 platform passes every row below natively (not under emulation).
"Compiles" is not a pass.

| Area | Test | macOS | Windows | Linux glibc | Linux musl |
|---|---|---|---|---|---|
| Build | Native runner, both architectures, release profile | required | required | required | required |
| Durability | Kill (SIGKILL / `TerminateProcess`) during WAL append; reopen; all acknowledged events present, tail truncated at last good record | required | required | required | required |
| Durability | Kill during segment flush; reopen; no duplicate or missing events; partial segment file ignored and tombstoned | required | required | required | required |
| Durability | Kill during manifest update; reopen; highest verifiable generation selected; temp manifest ignored | required | required | required | required |
| Resources | Disk full and quota exhaustion during append / flush / manifest; daemon reports, hooks spool, no corruption | required | required | required | required |
| Resources | Permission denied and read-only filesystem for data root and provider config | required | required | required | required |
| Corruption | Corrupted WAL, segment, manifest, index, and blob (bit flips, truncation, zero fill); `attempt verify` detects, `attempt repair` discards nothing valid | required | required | required | required |
| Concurrency | Concurrent readers (CLI, UI, snapshot export) with a writer; readers see a consistent manifest generation | required | required | required | required |
| Capture | Daemon unavailable: hooks spool, daemon start imports in order, no duplicates, `.done` cleanup after manifest durability | required | required | required | required |
| Compatibility | Old schema version read by new binary; new schema version read by old binary (unknown fields preserved); old binary refuses newer format version cleanly | required | required | required | required |
| Portability | `.atdb` snapshot created on each OS opened on each other OS with identical logical query results | required | required | required | required |
| Paths | Windows long paths (> 260 chars, `\\?\` prefix) and UNC paths | — | required | — | — |
| Paths | Non-ASCII (Korean), emoji, spaces, and very long repository names in project roots and file paths | required | required | required | required |
| libc | glibc vs musl behaviour for locking, `fsync`, and `renameat` | — | — | required | required |
| Signing | Gatekeeper / notarization: signed binary launches on a clean machine without override | required | — | — | — |
| Signing | Defender / SmartScreen: signed installer runs; hook `attempt.exe` not quarantined during a capture test | — | required | — | — |
| Power | Sleep / wake with the daemon running: socket still valid, hooks reconnect, HLC monotonic across the gap | required | required | required | — |
| Clock | Wall clock stepped backwards and forwards while ingesting: `source_seq` and HLC monotonic; `observed_at` unchanged | required | required | required | required |
| Service | Install, stop, restart, upgrade, uninstall of the per-user service leaves no orphan process or registration | required | required | required | required |
| Hooks | Each provider's config is written, verified with `capture_test`, upgraded, and uninstalled without altering unrelated content (byte comparison) | required | required | required | required |
| Performance | Hook path p95 < 10 ms (excluding host agent) on the reference workload | required | required | required | required |

Fault injection is deterministic where possible (a filesystem shim that fails
the N-th write), with a smaller set of real-kill tests to catch what the shim
cannot model.

## Decisions

- `attempt` is the only required executable; every mode is a subcommand of
  the same binary.
- Only the daemon holds the database write lock. Hooks and CLI clients never
  write to `.attemptdb/` directly except into `spool/`.
- The hook entrypoint exits 0 on any AttemptDB-side failure and spools; it
  never turns an AttemptDB failure into a provider-visible hook failure.
- Path resolution is two-level: data root (`--data-dir` > `ATTEMPTDB_DATA_DIR`
  > OS default) and live database directory (`ATTEMPTDB_DIR` > project-local
  `.attemptdb/` by walking up from cwd > `<data root>/db/.attemptdb`).
- Config, data, cache, runtime, and logs are separate directories except in
  portable mode, where everything lives under the data root.
- IPC uses Unix domain sockets on macOS/Linux and named pipes on Windows;
  loopback TCP is an authenticated, off-by-default fallback and is never
  bound to a non-loopback address.
- The IPC protocol is framed and versioned (`ATIP`, protocol version 1) and
  shares the codec-id space with the WAL/spool frames.
- Spool files use the WAL frame format with magic `ATSP`: one shared
  `inbox.spool` appended under `inbox.lock`, claimed by the writer through a
  rename to `claimed-<uuidv7>.spool` and deleted once its events are durable
  in the WAL (`docs/storage-format.md` §7).
- Ingestion is idempotent by `event_id`; `source_seq` is assigned once by the
  single writer at WAL append.
- Background execution is per-user only (launchd agent, per-user Windows
  process, `systemd --user`); no privileged service is required.
- Eight release targets, all required for a release.
- Installer rules: detect before mkdir, structural edits only, lock + backup
  + atomic replace, verify with `capture_test`, idempotent, never write
  provider trust state.
- `attempt doctor` reports exactly one of `not installed`, `configured`,
  `trusted`, `unverified`, `active`, `stale` per provider, with the
  definitions in section 8.1.
- Codex hooks live in `~/.codex/hooks.json`; Gemini timeouts are milliseconds;
  Claude Code and Cursor timeouts are seconds.

## Open questions

- Should the CLI embed the storage engine read-only when no daemon is
  running (fast `attempt timeline` without a service), or always require the
  daemon? Embedding is simpler for users but means two code paths for reads.
- Windows: is a per-user background process sufficient for the first
  release, or do shared-machine and enterprise users need a Windows Service
  from day one?
- Should the loopback TCP fallback be enabled automatically when the primary
  transport fails to bind, or remain strictly opt-in?
- Hook budget: is 2 000 ms total (with the provider-side timeout set to 5 s)
  the right default, or should the budget be derived from the provider's
  configured timeout?
- Should spool files be written under the live database directory (current
  design) or always under the data root, given that project-local
  `.attemptdb/` directories may be on slow or network filesystems?
- Gemini CLI: which keys beyond `name`, `type`, `command`, `timeout` are
  accepted per hook entry, and does the host reject unknown keys like Codex
  does?
- Cursor: is `version` required to be exactly `1`, and how does the host
  behave when it sees a higher value?
- Auto-update: in-place binary swap versus package-manager-only updates on
  platforms that have a package manager.
