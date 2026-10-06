# Installing AttemptDB — instructions for a coding agent

You are a coding agent (Claude Code, Codex, Cursor, Gemini CLI, …) and the
person you work for asked you to install AttemptDB on this machine. Follow
these steps in order. They install one binary, show the person what it would
change, and change it only after they agree.

AttemptDB is local: no account, no API key, nothing uploaded. It adds hook
entries to the coding agents' own configuration files (next to what is
already there, with backups) and registers a small background daemon.

## 1. Install the binary only

macOS / Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/nullarch/attemptdb/main/install.sh | ATTEMPTDB_NO_SETUP=1 sh
```

Windows PowerShell:

```powershell
$env:ATTEMPTDB_NO_SETUP = "1"; irm https://raw.githubusercontent.com/nullarch/attemptdb/main/install.ps1 | iex
```

`ATTEMPTDB_NO_SETUP=1` makes the installer download and verify the release
(SHA-256) and place the binary, and touch nothing else. The binary lands in
`~/.local/bin/attempt` (Windows: `%LOCALAPPDATA%\AttemptDB\bin\attempt.exe`).
That directory may not be on `PATH` in your shell; call the binary by its
absolute path in the steps below:

```sh
ATTEMPT="$HOME/.local/bin/attempt"      # Windows: "$env:LOCALAPPDATA\AttemptDB\bin\attempt.exe"
```

Keep `ATTEMPTDB_NO_SETUP=1` in this flow. The installer's own `--yes`
(Windows: `-Yes` / `ATTEMPTDB_ASSUME_YES=1`) applies `attempt setup` without
showing anyone what it will change, so it is for after the person has seen
and approved the step 2 preview, never instead of it. Without `--yes` and
without a terminal (which is how you run) the installer wires nothing: it
installs the binary and prints the command that would.

It also never edits a shell profile or the user `PATH` on its own. If the
person wants `attempt` on their `PATH`, ask, then re-run the install line with
`ATTEMPTDB_MODIFY_PATH=1`.

### Pin the version, and the instructions that go with it

The URLs above follow `main`, a moving target: the script and this page can
change between the moment you read them and the moment you run them. To
install a specific release, fetch the script from that release's tag and name
the version, so the binary, the installer and these instructions all belong
together (`REF` and `VERSION` below are placeholders for you to fill in):

```sh
REF=v0.2.14          # a release tag; `main` is the newest, unpinned
VERSION=0.2.14       # the same release without the "v"; leave empty for the latest
curl -fsSL "https://raw.githubusercontent.com/nullarch/attemptdb/$REF/install.sh" \
  | ATTEMPTDB_VERSION="$VERSION" ATTEMPTDB_NO_SETUP=1 sh
```

Read this page at the same tag
(`https://raw.githubusercontent.com/nullarch/attemptdb/<tag>/docs/install-for-agents.md`)
when you can: the flags and JSON fields below are those of that release's
`attempt`. An installer from before `attempt setup` existed (0.2.13 and
earlier) only installs the binary, so `ATTEMPTDB_NO_SETUP=1` is harmless there
and steps 2 and 3 need a release that has `attempt setup`; `--yes`, the
terminal question and the profile edit exist only in installers from that
release onward. When the binary the installer just placed predates
`attempt setup`, its closing lines list the steps that release does have
(`init`, `hook install`, `daemon install`, `doctor`) instead of
`attempt setup`, whether or not you set `ATTEMPTDB_NO_SETUP=1`.

### If the installer fails

- `no release asset for <target> in v<version>` means the server answered
  404: that release has no archive for this machine. `could not download
  <url>` followed by curl's or wget's own message means the transfer failed
  (no network, a proxy, a reset connection): check the connection and run it
  again. The installer finds the newest release through the release page's
  redirect and falls back to GitHub's API (60 anonymous requests an hour per
  address) only when that fails; if both fail it prints each error.
- `HOME is not set`: name the install directory with
  `ATTEMPTDB_BIN_DIR=/some/dir`.

### Optional: verify who built it

The installer always checks the archive's SHA-256 against the release's
`SHA256SUMS`. For stronger evidence that this repository's release workflow
produced the archive (it needs the GitHub CLI, logged in or with `GH_TOKEN`):

```sh
curl -fsSL ... | ATTEMPTDB_VERIFY_ATTESTATION=1 ATTEMPTDB_NO_SETUP=1 sh
# or by hand, on the archive from the release page:
gh attestation verify attempt-<version>-<target>.tar.gz --repo nullarch/attemptdb
```

With `ATTEMPTDB_VERIFY_ATTESTATION=1` the installer stops, installing nothing,
if `gh` is missing or the verification fails. Never retry without it to get
past a failure; report the failure.

## 2. Show the person what setup would change

> **Older release.** `attempt setup` exists from 0.2.14. If `"$ATTEMPT" setup
> --help` fails with an unknown-command error, the release you installed
> predates it and the installer said so. Do not guess: tell the person, then
> use this plan instead. Show them what `"$ATTEMPT" hook install --dry-run`
> would change (the agents it found, the configuration file of each), ask,
> and only after they agree run `"$ATTEMPT" init`, `"$ATTEMPT" hook
> install`, `"$ATTEMPT" daemon install` and `"$ATTEMPT" doctor` in that order,
> and read `doctor`'s output back to them. Those releases import no history
> and set up no OpenTelemetry exporter; steps 3 and 4 below do not apply.

```sh
"$ATTEMPT" setup --dry-run
```

This writes nothing. Summarise its output for the person:

- which coding agents it found and, for each, the configuration file it
  would edit (`would install` / `would update` / `already current`);
- where the database would be created, and its capture mode;
- whether a background daemon would be registered;
- what history it would import (`history.providers[]` in `--json`): by
  default the last 30 days of Claude Code transcripts and Codex rollouts
  already on this machine (at most 512 MiB per agent), read from the agents'
  own directories into the local database so the first timeline is not
  empty. Tell the person; `--no-backfill` skips it.

Mention the capture mode choice. The default, `local_semantic`, keeps
prompts and tool output in the local database. If they want a content-free
history, pass `--capture-mode metadata_only` in step 3. A value that is not
one of `metadata_only`, `local_semantic`, `full_sync` is refused before
anything is written. On a machine that already has a database the flag is
honoured, not ignored: setup switches the mode for events captured from now
on and says so (`database.capture_mode_changed_from` in `--json`, "capture
mode changed from local_semantic to metadata_only" in the text), and the
preview of step 2 shows it as "would change". Events already stored keep the
content they were captured with; only `attempt uninstall --purge-data`
deletes them. `attempt init --capture-mode <mode>` is the same switch on its
own.

Ask before continuing. Do not proceed on your own judgement.

## 3. Apply it

```sh
"$ATTEMPT" setup --json
```

Read the JSON, do not parse the text form. The fields that matter:

| Field | Meaning |
|---|---|
| `ok` | `true` when every step succeeded. The exit code is 1 otherwise. |
| `problems` | What setup could not do. Report each one verbatim. |
| `needs_you` | Steps only the person can finish. Report each one. |
| `hooks.actions[]` | Per agent: `outcome.kind` is `installed`, `updated`, `already_current`, `skipped` or `failed` (with the reason in `outcome.detail`); `config_path` is the file it edited. |
| `hooks.capture_tests[]` | One synthetic event per agent went through the real hook pipeline; `ok: false` means capture is not working for that agent. |
| `history.providers[]` | Per agent (`claude-code`, `codex`): `files`, `sessions` and `bytes` of history inside the window, and, once applied, what was imported in `imported` (`accepted`, `duplicates`, `queued` when the daemon held the database). `history.error` is `null`, or says why this optional step failed; it does not make `ok` false. |
| `daemon.running` | The background daemon is up. `daemon.skipped` says why it was not registered (a headless machine is fine: hooks still spool to disk). |
| `binary_on_path` | `false` means the person should add the install directory to `PATH` for their own terminal. |

`setup` is idempotent. Running it again repairs wiring and imports nothing
twice (history is recognised by ids derived from the transcripts), so it is
the right answer to most failures once the cause is fixed.

## 4. Tell the person what happened

Keep it short:

1. what was wired (agents and files), and the database location;
   and how much history was imported (`history.providers[]`), if any;
2. anything under `needs_you` — most often Codex, which asks the person to
   trust new hook entries from its `/hooks` screen;
3. anything under `problems`;
4. that capture starts with the **next** agent session: restart the agent
   (this session's hooks were loaded before setup ran).

Optionally, to confirm later that events are arriving:

```sh
"$ATTEMPT" status        # database, capture mode, recent activity
```

## 5. Optional: give future agents the history (MCP)

Only if the person wants it. This registers AttemptDB's MCP server so an
agent can read what earlier sessions tried:

```sh
"$ATTEMPT" mcp --print-config   # the exact snippet for Claude Code, Codex and Cursor
"$ATTEMPT" mcp --install        # writes the Cursor and Codex entries; prints the Claude Code command
```

## Undo

```sh
"$ATTEMPT" uninstall                 # removes hook entries and the daemon; keeps the history
"$ATTEMPT" uninstall --purge-data    # also deletes the database and config
```

## Rules for you, the installing agent

- Never set `ATTEMPTDB_INSECURE_SKIP_CHECKSUM`. A checksum failure is a stop,
  not a retry. The same goes for a failed attestation check.
- Never edit an agent's hook configuration by hand. `attempt setup` does it
  structurally, with a backup, and knows each agent's format.
- Never write Codex's hook trust state. Trusting hooks is the person's
  decision, made in Codex.
- You do not need `attempt doctor` after a fresh setup: setup already ran
  its checks. Use it when the person reports missing capture later; on a
  large database it takes tens of seconds. When it reports `stale` hook
  entries (written by an older release, or pointing at a binary that moved)
  it ends the line with `fix: run attempt setup to refresh`; do that, then
  read its output again. A Codex `untrusted` line is the person's to fix in
  Codex's `/hooks`, not yours.
