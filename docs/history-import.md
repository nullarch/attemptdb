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
- **Not twice.** A session that hooks also captured is not stored twice; see
  [How the channels are merged](#how-the-channels-are-merged).
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

## `attempt import claude-transcripts`

```sh
attempt import claude-transcripts                  # this project's transcripts
attempt import claude-transcripts --all-projects   # every project
attempt import claude-transcripts ./some/dir       # a file or a directory
```

### What a transcript becomes

| In the transcript | Event |
|---|---|
| the first timestamped entry | `session_started` |
| a typed `user` entry | `prompt_submitted` |
| `attachment { type: queued_command }` | `prompt_submitted` with `provider.prompt_source = "queued_command"`: a message typed while the agent was busy (in one 135 MB session 54 of the 55 mid-session prompts exist only this way). A queued background-task notification (`commandMode: "task-notification"`, or text the hook adapter also treats as injected) is not a prompt |
| `tool_use` block, `tool_result` block | `tool_call_started`, `tool_call_finished` / `tool_call_failed` (a rejected call is `denied`, an interrupted one `cancelled`) |
| `server_tool_use` block, then its `<tool>_tool_result` block | the same pair, `provider.server_tool = true`: tools the API ran for the model, which no hook ever sees. Only the shape of the result is kept (how many, the error code), never the pages |
| assistant text | `agent_message`. Text followed by a tool call or by another message in the same turn is **narration** and carries `provider.interim = true`; the earlier importer dropped it (167 of 613 lines survived) |
| the end of a turn | `turn_stopped`, synthesised **only when the turn demonstrably ended**: a `turn_duration` entry follows, a real prompt follows, or the last message carries a terminal `stop_reason` (`end_turn`, `stop_sequence`, `max_tokens`, `refusal`). A transcript imported mid-turn ends in its last message, without a stop for a turn that has no end yet |
| `[Request interrupted by user]` | `turn_stopped`, outcome `cancelled` |
| `system/api_error` | `notification`, `notification_type = "api_error"`, with `http_status`, `retry_attempt`, `max_retries`, `retry_in_ms` under `attrs.provider` and the text as content |
| `system/compact_boundary`, `summary` | `compaction_finished` |
| `cost-state` | an `unknown` event carrying its numeric fields under `attrs.provider`; an entry with no number is skipped and counted |
| subagent files | `subagent_started` / `subagent_stopped` and the subagent's own events, `is_sidechain` |
| other attachments (`hook_success`, `edited_text_file`, `file`, ...), UI bookkeeping | skipped and counted |

Token usage is metadata only. The numbers of an API message's `usage`
(`input_tokens`, `output_tokens`, `cache_creation_input_tokens`,
`cache_read_input_tokens`, `web_search_requests`, `web_fetch_requests`) are
recorded under `attrs.provider` of the **first event the message produced**
(the newest line of a message wins: a message is written one block per line,
each with the usage at that moment), so summing them over events counts every
message once.

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
is the one place ids are made, and it is `derive_event_id` (below) for Codex.
The key is the Codex `call_id` for response-item tool calls (`call:<id>`), the
item id for item-style operations, otherwise the line's `ordinal` (or its line
number in rollouts that predate ordinals). Rollouts are append-only, so a
rollout that has grown adds only its new tail. The one exception is the
end-of-session marker, which moves with the end of the file: a later end is a
new event, and an active session (modified in the last five minutes) gets none
yet.

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

## How the channels are merged

The same real-world action can reach the database through hooks, a transcript
or rollout import, a re-import and OTel, each with its own event id. Storage
deduplicates by `event_id` only, so the merge has to happen in the ids or
before the write. Two mechanisms do it, chosen by whether the provider names
the action.

### 1. Tool calls: the same id in every channel

`attemptdb_adapters::common::derive_event_id(provider, session, kind,
native_key)` is a UUIDv5 under the AttemptDB namespace of
`("event-v1", provider, session, slot, key)`, where `key` is `call:<id>`
(`tool_call_key`) and `slot` is the event kind, except that
`tool_call_finished` and `tool_call_failed` share one slot (`tool_call_end`):
a call has one end, and if two channels disagree about whether it failed,
storage still keeps one.

| Channel | Where the id comes from |
|---|---|
| Claude hook (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`, `PermissionDenied`) | `tool_use_id` |
| Claude transcript | `tool_use.id` of the block, `tool_result.tool_use_id` |
| Codex hook | `tool_use_id` |
| Codex rollout | `call_id` of `function_call` / `custom_tool_call` and their outputs |

It is a hash of a few strings, so the hook stays stateless, never opens the
database and costs nothing measurable. Because both sides derive the same id,
storage's by-id duplicate check merges the channels **whichever arrives
first**, and a hook registered twice (user and project scope, different
command strings) delivers byte-identical payloads that now store one event per
call event. A tool event with no call id in its payload, or with no session id
(a call id is only unique within a session), keeps a random id.

Codex's hook `tool_use_id` is the model's tool call id, which is the rollout's
`call_id`; the fixtures of this repository show both as `call_*` strings but
cannot prove they are the same string for every Codex version. If they are
not, nothing merges by id (the ids simply do not collide) and importer rule 1
below, which joins on the call id the stored hook event carries, still stops
the double count.

**Compatibility.** The Claude transcript importer shipped before this
(v0.1.0). Its tool events used to be named `(session, entry uuid, block)`.
The parser still computes that old id for each tool event and reports it
(`TranscriptImport::legacy_ids`); the importer skips a tool event whose old id
is already stored, so a database imported by an earlier release does not get
its tool calls a second time. Every other event keeps the id it always had.
The Codex importer is new in this tree and changed freely.

### 2. Everything else: the importer reconciles before it writes

A prompt, a turn end, a session start, a subagent start or a compaction has no
id the provider gives it, and the hook cannot know the transcript's. A hash of
the payload would merge two different prompts that read the same ("continue"
twice in one turn), so those events keep their ids and the **importers**
reconcile instead (`attemptdb-capture`, `import_common::{StoredIndex,
Reconciler}`).

Before writing anything, an importer reads what the database already holds
about the sessions of the run: segments pruned by provider and time from their
metadata, a few columns of the rows of the wanted sessions, then the memtable.
No event is decoded and no content is read. It then skips a reconstructed
event when:

1. **It is a tool call** and the session already holds a hook-captured event
   for the same call id in the same slot, whatever its id (hook events from
   before the natural ids have random ones), or an earlier import stored it
   under the id an older version derived.
2. **It is a prompt, turn end, session start or end, subagent start or stop,
   or compaction finished**, and a hook-captured event of the same kind in the
   same session, not yet matched, lies within **ten seconds**
   (`MATCH_TOLERANCE_MICROS`): the nearest one is consumed. Matching is by
   order, never by text, because under `metadata_only` there is none. Two hook
   prompts consume the two nearest transcript prompts and a third, which hooks
   missed, is imported. A session start matches at any distance (the
   transcript has no start of its own, only its first entry). A turn end that
   is a user interruption never matches: Claude fires no `Stop` hook for one.

The skip is counted: `skipped_captured` in the import summary (`--json`).

**Project identity.** A session's own hook events say which project it was in.
When any are stored, their project (id, root, name, remote) replaces the one
derived from the transcript's `cwd`, which for a deleted worktree or a moved
repository without a remote is a hash of the path and differs from the
remote-based one the hooks computed. The branch stays the transcript's.

**Under the daemon.** When the daemon holds the writer lock the import writes
through the spool (above), and the lookup uses a read-only open of the
database, which replays the WAL and so sees everything the daemon has
acknowledged. Hook events still in the spool inbox are not seen yet; the
daemon drains it within seconds. If the lookup cannot read the database at
all, the summary carries a warning and the run writes everything.

### What is still counted twice

- **Import first, hooks later.** Tool calls still merge (same id). A prompt or
  turn end the importer wrote before the hook event existed is a second one:
  the hook cannot know the transcript's id, and the importer has already run.
  In practice an import drains the hook spool first and a hook fires before
  the agent writes the entry, so the overlap is limited to an action in
  flight (hooks installed mid-turn).
- **A hook registered twice, for events with no call id.** Two entries deliver
  two prompts, stops or notifications with identical payloads and random ids.
  There is no timestamp or id in those payloads to tell a double delivery from
  two real events, so they are kept apart. Registering a hook once is the fix;
  tool calls, the bulk of the volume, are already merged.
- **Hook events that never landed.** A hook event that has not reached the
  database by the time of the lookup cannot be reconciled with.
- **Project identity of a session hooks never saw.** A repository without a
  remote whose directory no longer exists falls back to the working-directory
  text, which is the repository root only when the session started at the
  root. (A rollout's own `git` facts keep a Codex project right whenever a
  remote exists.)
- Other providers: Cursor transcripts and Gemini chats are not imported, so
  nothing overlaps; their hook adapters name no call id yet.
- **A queued prompt typed mid-turn** is a prompt in the transcript and, when
  the agent fires `UserPromptSubmit` for it, a hook prompt too; both are
  reconciled by order like any other prompt.
