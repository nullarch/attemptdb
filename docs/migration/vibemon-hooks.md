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
installs `attempt`, runs `attempt init`, then:

```sh
attempt hook install --remove-legacy vibemon   # ours in, notify.sh entries out
attempt sync connect https://sync.vibemon.dev --key atk_...
attempt sync now
```

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

The legacy client was POSIX-only, so there is nothing to remove there;
`install.ps1` plus `attempt hook install` and `attempt sync connect` is the
whole path.
