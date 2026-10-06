# Importing history: transcripts and rollouts

A fresh install has an empty database until the next agent session. The
history importers close that gap: they reconstruct what Claude Code and Codex
already did from the files those agents keep on disk, so the first
`attempt ui` or `attempt timeline` shows the person's own work.

Everything imported here is **reconstructed, not captured**. Every event
carries `attrs.reconstructed = true` and `attrs.reconstructed_from`
(`claude_code_transcript` or `codex_rollout`), has no `hook_version` and no
`raw` payload, and obeys the database's capture mode: under `metadata_only`
no text is stored, exactly as for a hook. Reconstruction is an approximation
of the live record; `attempt timeline --captured-only` leaves it out.

| Source | Where the agent keeps it | Command |
|---|---|---|
| Claude Code transcripts | `$CLAUDE_CONFIG_DIR/projects/<slug>/<session>.jsonl` and `~/.claude/projects/...` (plus `<session>/subagents/**`) | `attempt import claude-transcripts` |
| Codex rollouts | `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl` (default `~/.codex`), and `archived_sessions/` | `attempt import codex` |
| VibeMon legacy hook export | an NDJSON/JSON export, not an agent directory | `attempt import vibemon-export` (see [migration/vibemon-hooks.md](migration/vibemon-hooks.md)) |

## The first-run backfill (`attempt setup`)

`attempt setup` ends with a `history` step. It is on by default and bounded:

- **Window and budget.** Files modified in the last 30 days, at most 512 MiB
  of transcripts per agent, **newest first**; a file that does not fit the
  remaining budget is skipped and smaller, older ones are still considered.
- **Planned from metadata.** `attempt setup --dry-run` (and `--json`) report,
  per agent, how many files, sessions and bytes fall inside the window and
  what was left out (`skipped_old`, `skipped_over_budget`). Computing them
  reads file sizes and times, and for Codex the first line of each rollout;
  nothing is parsed.
- **Never in the way.** It runs after the daemon step. When the daemon holds
  the database's writer lock the events go through the spool (below), so it
  never waits for the daemon. A failure is reported in `history.error`; it
  does not make setup fail (`ok` stays true), because the history is optional.
- **Idempotent.** Event ids derive from the transcript entries, so running
  setup again, or importing a file that has grown, only adds what is new.
- **Private by default.** It reads only the agents' own directories, writes
  only to the local database, and records history under the database's
  capture mode (`history.capture_mode`).

| Flag | Effect |
|---|---|
| `--no-backfill` | Skip the step (`history.skipped` is `--no-backfill`). |
| `--backfill-days N` | Window in days. `0` means all history. Default 30. |
| `--backfill-max-mib N` | Transcript budget per agent in MiB. `0` means unlimited. Default 512. |
| `--provider ID` | The same filter as for hooks: only the named agents are backfilled. |

Anything the step skipped, or any longer history, is one command away:
`attempt import codex` and `attempt import claude-transcripts --all-projects`.

## `attempt import codex`

```sh
attempt import codex                          # everything under ~/.codex/sessions
attempt import codex --days 30                # modified in the last 30 days
attempt import codex --since 2026-09-01       # RFC 3339, YYYY-MM-DD, -2d, today
attempt import codex --max-bytes 512M         # newest first, until the budget is spent
attempt import codex --path ./rollouts        # a file or a directory, any *.jsonl
attempt import codex --dry-run                # the plan only; write nothing
attempt import codex --json                   # plan + summary as JSON
```

The summary lists files, sessions, events stored, duplicates skipped and
lines skipped (malformed, over the size limit, or a partial last line).
Rollouts are large: the owner's machine holds 456 files, 8.2 GB, the biggest
595 MB, with single lines up to 12 MB (generated images, screenshots). They
are **streamed** through a bounded buffer and handed to the database in
batches, so memory does not grow with the file.

### What a rollout becomes

| In the rollout | Event |
|---|---|
| `session_meta` | `session_started` (project from `cwd` and `git`; `session_ended` follows the last line once the file has been quiet for 5 minutes) |
| `session_meta` of a subagent thread | `subagent_started` / `subagent_stopped` inside the parent's session; the history it inherits from the parent is skipped |
| `user_message`, or an `item_completed` `UserMessage` | `prompt_submitted` |
| `agent_message`, an `AgentMessage` item, or an assistant `message` | `agent_message` with `phase` (`commentary`, `final_answer`); the three encodings of one message are paired and stored once |
| `function_call` / `custom_tool_call` and their `*_output` | `tool_call_started` + `tool_call_finished` / `tool_call_failed`, paired by `call_id` (shell, `apply_patch`, `view_image`, `exec`, ...); exit codes from the output |
| `item_completed` `CommandExecution`, `FileChange`, `McpToolCall`, `DynamicToolCall`, `ImageView`, web search and image-generation extensions | a start/end pair each: Codex 0.148+ logs the real work of an `exec` call only here |
| `turn_aborted` | `turn_failed`, outcome `cancelled`, `error_class = interrupted` |
| `task_complete` | `turn_stopped` (`turn_failed` when it carries an `error`), with `duration_ms` and the turn's token counts as numbers |
| `compacted` | `compaction_finished` |
| `thread_rolled_back` | `notification` |
| anything else unrecognised | a content-free `unknown` event carrying only the type name |

Bookkeeping that holds no observable fact (`world_state`, `turn_context`,
`token_usage_record`, `thread_settings_applied`, reasoning, the duplicate
encodings above) is skipped and counted in the parser's statistics. Anything the
parser does not recognise is *not* in that list: it becomes an `unknown` event.
Reasoning is private and is never kept.

Token usage is metadata only: per turn, `attrs.output_tokens` and
`attrs.provider.{input,cached_input,reasoning_output,total}_tokens`,
computed from the deltas of Codex's running total.

### Ids and re-imports

`rollout_event_id(session, key, kind)` (`crates/attemptdb-adapters/src/transcript/codex.rs`)
is the one place ids are made: a UUIDv5 of the provider, the session, a key
and the event kind. The key is the Codex `call_id` for response-item tool
calls, the item id for item-style operations, otherwise the line's `ordinal`
(or its line number in rollouts that predate ordinals). Rollouts are
append-only, so a rollout that has grown adds only its new tail. The one
exception is the end-of-session marker, which moves with the end of the file:
a later end is a new event, and an active session (modified in the last five
minutes) gets none yet.

### Cost

Measured on a release build with a synthetic rollout shaped like the real
ones (tool outputs of a few KB to 24 KB, 3 MB screenshots on some calls,
compaction snapshots): parsing alone takes about 1 s for 200 MB and 2 s for
800 MB at a constant ~21 MB resident, whatever the file size. Importing the
same rollouts into a database takes 0.8 s / 3.3 s at ~210 MB / ~245 MB
resident: the database flushes a segment every 5 000 events instead of
holding them all. A re-import of the same file finds every event a duplicate
in about a third of that time.

## When the daemon holds the database

The database has a single writer. When the daemon is running it holds the
writer lock, and an importer that tried to open the database would fail. All
the history importers now write through one rule:

1. the writer lock is free: ingest directly, flush a segment, report
   `imported N new event(s)`;
2. it is held: append the events to the **spool**, the same transport hooks
   use, and report `queued N event(s)`. The daemon imports the spool every
   few seconds and counts accepted events and duplicates there. Importing
   twice queues twice and stores once.

The importer paces itself against the spool: once the inbox passes 32 MiB it
waits (up to 20 seconds, once) for the daemon to claim it before writing
more, because whoever imports a spool file reads it into memory whole.

## What is not merged yet

- **Hook events and transcript events of one session are not deduplicated.**
  A session that hooks captured live and that is later reconstructed from
  its transcript appears twice until the merge key
  `(provider, session, tool_use_id | entry id, kind)` lands. The ids are
  derived in one function so that change is one edit.
- **Project identity when a checkout is gone.** A rollout's `git` facts keep
  its project right (a remote is what identity is made of), but a repository
  without a remote whose directory no longer exists falls back to the
  working-directory text. For a session started in a subdirectory that is
  not the root a hook would have used. The Claude importer has the same gap.
- Cursor transcripts and Gemini chats are not imported.
