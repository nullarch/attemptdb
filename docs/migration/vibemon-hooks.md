# Migrating from VibeMon's `notify.sh` hooks

VibeMon's first collector (`vibemon-hooks`, "the thin client") installed one
shell hook per agent event that ran `bash ~/.vibemon/notify.sh <event>
<provider>` and posted a sanitised envelope straight to the hosted service.
AttemptDB replaces it: the same hooks now write to a local database first, and
`attempt sync` uploads from there under the device's policy (RFC 0006 §10).

What changes for a user:

| | `notify.sh` (legacy) | `attempt hook` |
|---|---|---|
| Where facts land | hosted service only | local `.attemptdb`, then the server |
| Offline | events lost | queued in the local WAL, uploaded later |
| Content | never left the machine | still never leaves unless `--send-content` |
| Event ids / times | none / seconds | UUIDv7 + HLC (dedupe, replay, ordering) |
| Inspect locally | — | `attempt timeline`, `attempt sql`, the UI |

## One command

```sh
curl -fsSL https://vibemon.dev/install.sh | sh -s -- --key atk_...
```

The draft of that script is [`vibemon-install.sh`](./vibemon-install.sh). It
installs `attempt` (and `attempt-hook`), then runs:

```sh
attempt init --capture-mode metadata_only --source vibemon
attempt hook install --remove-legacy vibemon     # ours in, notify.sh entries out
attempt sync connect vibemon --key atk_... --profile semantic
attempt daemon install                           # spool → database → uploads, no restart needed
attempt sync now
attempt doctor                                   # what the user sees: agents, hooks, database, sync
```

Two defaults carry promises made to existing VibeMon users:

- **`metadata_only` on disk.** AttemptDB's own default for a new install is
  `local_semantic` (prompts and tool output kept locally, encrypted). A
  migrated VibeMon user was told nothing content-bearing is collected, so
  the script keeps that true on their own disk too until they pass
  `--local-content` (or later run `attempt init --capture-mode
  local_semantic`). That flag is the consent step; nothing changes it
  silently.
- **`semantic` upload profile.** Metadata plus this device's inferences
  (attempts, handoffs, work units, decisions — each with evidence ids and
  confidence), never prompts, commands, or output. `--profile metadata_only`
  narrows it; `--send-content` (= `--profile full`) is the only way content
  leaves, and the hosted server's ceiling still applies.

`vibemon` is an alias `attempt sync` resolves to `https://sync.vibemon.dev`
(`VIBEMON_SYNC_URL` overrides it); `--server URL` points at another server.

`--remove-legacy vibemon` is the only new piece. It recognises the legacy
entries by their command (`~/.vibemon/notify.sh`, any home path) and, for
Gemini CLI, by their `vibemon-*` names; it never touches other hooks in the
same group, and it never deletes `~/.vibemon` itself. Without the flag, a
plain `attempt hook install` leaves the legacy entries alone (both collectors
would then run side by side — harmless, but double the hook cost). The same
flag works on `attempt hook uninstall`.

Each config file is backed up (`<file>.attemptdb.bak-<ts>`) before the edit,
so the previous state is one `mv` away. `attempt hook install --dry-run
--remove-legacy vibemon` shows what would change, with `legacy_removed`
counts per agent in `--json`.

## Keys

Legacy `~/.vibemon/api-key` values are not reused: the hosted server issues
AttemptDB device keys (`atk_…`) through `/v1/admin/keys`, one per device, and
the web app hands the key to the installer. The legacy key can be exchanged
server-side by the product when a signed-in user links the device; that is
outside this repository.

## Removing `~/.vibemon`

`vibemon-install.sh --purge-legacy` deletes the directory only after checking
that no agent config still references `notify.sh`. Until then the script
stays: a config the migration could not edit (unreadable JSON, a scope the
user did not migrate) would otherwise call a missing file on every event.

## Windows

The legacy client shipped a signed GUI installer on Windows; its hook
entries are removed by the same `--remove-legacy vibemon`.
[`vibemon-install.ps1`](./vibemon-install.ps1) is the PowerShell counterpart
of the script above with one substitution: the background daemon is not
implemented on Windows yet (TODO.md §21.4), so the script registers a
per-user Scheduled Task that runs `attempt import` and `attempt sync now`
every five minutes. Hooks never wait on it — they append to the spool and
exit — so capture is complete either way; only the upload cadence differs.

## Backfill: the history VibeMon already holds

Hooks only see what happens after they are installed, but the hosted service
kept one row per legacy event in its `hook_events` table. Replaying that
table into the database means the timeline does not start on migration day:

```sh
attempt import vibemon-export hook_events.ndjson
```

### Exporting the table

Any export of `hook_events` rows works, as NDJSON (one JSON object per line)
or a JSON array of objects; the format is detected from the first byte.
Column names are the table's own (`id`, `user_id`, `created_at`,
`event_type`, `agent`, `session_id`, `payload`, `signals`, `project_id`,
`tool`, `file_path`, `lines_added`, `lines_removed`, `local_hour`,
`local_dow`, `envelope_version`); unknown columns are ignored. From `psql`:

```sh
\copy (select row_to_json(h) from hook_events h where user_id = '<uuid>' order by created_at) to 'hook_events.ndjson'
```

The Supabase dashboard's JSON export and a PostgREST `select=*` response are
the array form and import as-is. Note the table's retention policy
(`hook_events_retention_days`, 14 by default): export before the rows are
pruned.

### What the importer does

- **Maps rows back to the envelope.** The Edge Function stored columns, not
  the envelope: `tool_use` rows carry `tool`/`file_path`/line counts instead
  of a payload, and only `session_start`/`session_end` rows kept the working
  directory. The importer rebuilds the envelope v2 object and runs it
  through the same adapter the legacy endpoint uses; the working directory
  of a session's start row is applied to the rest of that session, and
  `project_id` links sessions that have no start row to the right directory.
  Nothing else is guessed. (`attemptdb_adapters::vibemon_export` documents
  the column mapping.)
- **Is idempotent.** Every event id is `EventId::derive(["vibemon-export",
  <row id>])`. A second run, or an overlapping later export, stores nothing
  and reports the rows as duplicates.
- **Orders by time.** Rows are sorted by `created_at`, then `id`, so
  `source_seq` is monotone in event time even if the export was not.
- **Rejects, never fails.** A malformed line, or a row without `id`,
  `created_at`, or a known `event_type`, is counted with its line number
  and reason (`--json` lists the first 50); the rest imports.
- **Stores facts, metadata only.** The legacy client never captured
  content, so the events are `metadata_only` (`commit.message`, the one
  content-bearing signal, does not survive). They are not marked
  `reconstructed` — they were captured live — and carry
  `attrs.x_vibemon_import = "hook_events"` plus `x_vibemon_row_id`,
  `x_vibemon_project_id`, `x_vibemon_envelope_version`, and
  `x_vibemon_client_version` for provenance.
- **Device.** `--device local` attributes the history to this database's
  own device, so legacy and live events of the same directory share a
  project; `--device <uuid>` picks any device. Without the flag each row
  goes to `DeviceId::derive(["vibemon-export", <user_id>])` (or the export's
  device/machine column when it has one — the real table has none). The
  device is not part of the event id, so rows already imported keep the
  device they were first given; choose `--device` before the first run.

`--dry-run` parses and prints the plan (rows read, parsed, rejected by
reason, sessions, time span, devices) without opening the database;
`--json` emits the same as JSON.

### Hosted tenants

The same command backfills a tenant of the sync server: on the server host,
point `--db` at the tenant directory while that tenant is not open by the
server —

```sh
attempt import vibemon-export --db data/tenants/<tenant>/ hook_events.ndjson
```

The database has one writer. If the server (or a local `attempt daemon`)
holds the tenant open, the import stops with `database is locked by another
writer: <path>`; close the tenant (or stop the daemon) and re-run. Nothing is
written before the lock is taken.
