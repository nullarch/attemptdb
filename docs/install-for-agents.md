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

## 2. Show the person what setup would change

```sh
"$ATTEMPT" setup --dry-run
```

This writes nothing. Summarise its output for the person:

- which coding agents it found and, for each, the configuration file it
  would edit (`would install` / `would update` / `already current`);
- where the database would be created, and its capture mode;
- whether a background daemon would be registered.

Mention the capture mode choice. The default, `local_semantic`, keeps
prompts and tool output in the local database. If they want a content-free
history, pass `--capture-mode metadata_only` in step 3 — it only applies to
a new database.

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
| `daemon.running` | The background daemon is up. `daemon.skipped` says why it was not registered (a headless machine is fine: hooks still spool to disk). |
| `binary_on_path` | `false` means the person should add the install directory to `PATH` for their own terminal. |

`setup` is idempotent. Running it again repairs wiring and creates nothing
twice, so it is the right answer to most failures once the cause is fixed.

## 4. Tell the person what happened

Keep it short:

1. what was wired (agents and files), and the database location;
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
  not a retry.
- Never edit an agent's hook configuration by hand. `attempt setup` does it
  structurally, with a backup, and knows each agent's format.
- Never write Codex's hook trust state. Trusting hooks is the person's
  decision, made in Codex.
- Do not run `attempt doctor` as part of the install. On a large existing
  database it reads every event; `setup` already ran the same checks
  without that scan.
