# Query context

*Generated from the code by `attempt schema --format markdown`. Do not edit by hand: `cargo test -p attemptdb-query --test catalog` fails when this file and the schema disagree, and `UPDATE_GOLDEN=1` regenerates it.*

AttemptDB records what coding agents tried. A hook on each agent (Claude Code, Codex, Cursor, Gemini CLI) appends immutable events; a deterministic projector derives sessions, turns, tool calls, attempts, work units, decisions, handoffs and a causal graph from them. Queries run over both layers at once: the facts in `events`, and the inferences everywhere else, each carrying the event ids it was built from.

## Rules

1. Two languages share one surface. AttemptQL verbs (SHOW, WHY, TRACE, STATE, DIFF, WHAT IS, EXPLAIN) answer the common questions in one line; plain SQL in the DataFusion dialect answers everything else over the same tables. Prefer the verb when one fits: it applies the retraction and scope rules for you.

2. The engine is read-only. Only SELECT, WITH, VALUES, EXPLAIN, DESCRIBE, SHOW and the AttemptQL verbs are accepted, one statement per call. There is no INSERT, no CREATE, no GRANT: history is appended by capture, never by a query.

3. `events` is fact; every other table is inference. An event was observed and is immutable. A session, turn, tool call, attempt, work unit, decision, handoff, edge or conflict was derived by the projector, and each carries `evidence` (the event ids it was built from), `confidence` (0.0-1.0) and usually `algorithm_version`. Never present an inferred row as something the agent did; say what it was inferred from.

4. Ids are readable prefixed strings, not UUIDs: `ev_` event, `ses_` session, `trn_` turn, `tc_` tool call, `att_` attempt, `wu_` work unit, `dec_` decision, `cmt_` commit, `prj_` project, `dev_` device. Compare them as text. `events_raw` is the same stream with the storage types instead (16-byte UUIDs, dictionary-encoded strings); read `events` unless you need the raw layout.

5. Times are `timestamp(microsecond, UTC)`. `observed_at` is when the agent did it, `captured_at` when the hook recorded it, `ingested_at` when the database accepted it. Order history by `observed_at`; measure capture lag with the other two.

6. Retracted rows are hidden by AttemptQL and visible to SQL. `SHOW` drops them unless the statement says `INCLUDING RETRACTED`; a bare `SELECT` does not, so filter `retracted = false` yourself on `events`, `sessions`, `turns`, `tool_calls` and `attempts`.

7. Content may be absent by design. Under `capture_mode = 'metadata_only'` the columns that carry text — `objective`, `rationale`, `note`, `content_json`, `raw_json` — are null for every row, and that is a privacy setting, not missing data. Check `events.capture_mode` before concluding an agent had no objective.

8. Counts belong to the projection, not to SQL aggregates you re-derive. `sessions.turn_count`, `attempts.tool_call_count` and the rest are computed with the retraction rules applied; recomputing them with COUNT(*) over the child table gives a different (and usually wrong) number.

9. Scope is a filter, not a mode. There is one database per install holding every project; a question about one repository is `WHERE project_name = '…'` or the `--project` flag, never a different connection.

## Tables

### `events`

**fact** · one row per observed event

The log. Every row was written by a hook (or reconstructed from an agent's own transcript) and is immutable: nothing in AttemptDB ever updates an event. Start here when a derived row looks wrong, and finish here when a claim needs proof — every inference cites these ids in its `evidence`.

Joins: `session_id` → `sessions.session_id` · `span_id` → `tool_calls.tool_call_id`

| column | type | null | meaning |
|---|---|---|---|
| `event_id` | text |  | UUIDv7 of the event (`ev_…`). Immutable, unique, and the only thing an inference ever cites. |
| `schema_version` | uint16 |  | Event schema version this row was written under (`spec/event-v1.schema.json`). |
| `device_id` | text |  | The machine that captured it (`dev_…`). One database can hold several devices after a sync or an import. |
| `source_seq` | uint64 |  | Per-device monotonic sequence. Together with `device_id` it makes the event's arrival order total. |
| `hlc` | uint64 |  | Hybrid logical clock: orders events across devices when wall clocks disagree. |
| `observed_at` | timestamp |  | When the agent did the thing. Order history by this. |
| `captured_at` | timestamp |  | When the hook recorded it. `captured_at - observed_at` is capture lag. |
| `ingested_at` | timestamp | yes | When the database accepted it. Null for events read straight from a segment. |
| `provider` | text |  | Which coding agent. `attemptdb` marks events AttemptDB wrote itself (corrections, retractions, capture tests); any other adapter contributes its own identifier, so match the values you know rather than assuming the list is closed. Common values: `claude_code`, `codex`, `cursor`, `gemini_cli`, `attemptdb` (open vocabulary — others appear). |
| `provider_version` | text | yes | The agent's own version string, when it reported one. |
| `adapter_version` | text |  | The AttemptDB adapter that normalised the payload. Changes here can change every derived row. |
| `hook_version` | text | yes | The hook binary that captured it. Null for events reconstructed from a transcript. |
| `capture_mode` | text |  | The privacy mode in force when the event was written. Under `metadata_only` every content column below is null by design. Values: `metadata_only`, `local_semantic`, `full_sync`. |
| `provider_event_name` | text |  | The provider's own name for the hook that fired, before normalisation. |
| `kind` | text |  | The canonical event kind. This is the column to filter on; `provider_event_name` is provider-specific. Values: `session_started`, `session_ended`, `prompt_submitted`, `tool_call_started`, `tool_call_finished`, `tool_call_failed`, `permission_requested`, `permission_denied`, `notification`, `agent_message`, `turn_stopped`, `turn_failed`, `subagent_started`, `subagent_stopped`, `task_created`, `task_completed`, `compaction_started`, `compaction_finished`, `config_changed`, `cwd_changed`, `file_changed`, `worktree_created`, `worktree_removed`, `correction`, `retraction`, `capture_test`, `unknown`. |
| `project_id` | text |  | Stable id of the repository (`prj_…`), derived from its root path and remote. |
| `project_root` | text |  | Absolute path of the repository root on the capturing machine. |
| `project_name` | text |  | `owner/repo` when a git remote is known, otherwise the directory name. This is what a human filters on. |
| `repo_remote` | text | yes | Git remote URL, when the repository has one. |
| `branch` | text | yes | Git branch at the time, when the provider reported one. |
| `head` | text | yes | The commit `HEAD` pointed at when the event was observed. |
| `session_id` | text |  | The agent session (`ses_…`), derived from the provider's own session id. |
| `provider_session_id` | text |  | The provider's own session identifier, as it wrote it. |
| `provider_turn_id` | text | yes | The provider's own turn identifier, when it has one. |
| `span_id` | text | yes | The tool call this event belongs to (`tc_…`): started and finished events of one call share it. |
| `parent_span_id` | text | yes | The enclosing span, for nested calls. |
| `agent_id` | text |  | Which agent instance acted (`agt_…`): a subagent has its own. |
| `agent_type` | text | yes | The agent's role as the provider named it — the main loop, or a named subagent. |
| `parent_agent_id` | text | yes | The agent that spawned this one. |
| `model` | text | yes | Model name the provider reported for the turn. |
| `provider_agent_id` | text | yes | The provider's own agent identifier. |
| `tool_name` | text | yes | The tool as the provider named it (`Bash`, `Edit`, `shell`, …). |
| `tool_category` | text | yes | The normalised category. Filter on this to compare providers. Values: `shell`, `file_read`, `file_write`, `file_edit`, `search`, `web`, `mcp`, `subagent`, `plan`, `notebook`, `other`. |
| `tool_call_id` | text | yes | The provider's own call id, when it issues one. |
| `path_logical` | text | yes | The path as the agent wrote it, absolute or not. |
| `path_relative` | text | yes | The same path relative to the repository root. Filter on this: it is stable across machines. |
| `paths_json` | text | yes | Every path the call touched, as a JSON array of repository-relative strings. |
| `outcome_status` | text | yes | How the call ended. Values: `success`, `failure`, `denied`, `cancelled`, `unknown`. |
| `outcome_class` | text | yes | A coarser reason, when the provider gave one. Common values: `exit_code`, `denied`, `timeout`, `cancelled` (open vocabulary — others appear). |
| `exit_code` | int32 | yes | Process exit status, when the provider reported one. |
| `duration_ms` | uint64 | yes | Wall-clock milliseconds. |
| `attrs_json` | text |  | The metadata allowlist as JSON (RFC 0006 §4). Content-free by construction: anything that could carry text is rejected before it is written. |
| `content_json` | text | yes | Prompt, message and command text. Content: null under `metadata_only`, and moved to an encrypted blob when a key exists. |
| `raw_json` | text | yes | The provider's original payload. Content, same rules as `content_json`. |
| `content_ref` | text | yes | Blob id holding `content_json` when it was written out of line and encrypted. |
| `raw_ref` | text | yes | Blob id holding `raw_json`, same. |
| `unknown_json` | text | yes | Fields the adapter did not recognise, kept verbatim so an upgrade can read them. Never silently dropped. |
| `retracted` | bool |  | True when a Retraction covers this event or its session. |

### `events_raw`

**fact** · one row per observed event, in storage types

The same stream as `events` with the on-disk types instead of readable ones: 16-byte UUIDs rather than `ev_…` strings, dictionary-encoded providers and kinds. Read it when you are checking the storage layer or comparing against a segment; read `events` for everything else.

| column | type | null | meaning |
|---|---|---|---|
| `event_id` | uuid |  | UUIDv7 of the event (`ev_…`). Immutable, unique, and the only thing an inference ever cites. |
| `schema_version` | uint16 |  | Event schema version this row was written under (`spec/event-v1.schema.json`). |
| `device_id` | uuid |  | The machine that captured it (`dev_…`). One database can hold several devices after a sync or an import. |
| `source_seq` | uint64 |  | Per-device monotonic sequence. Together with `device_id` it makes the event's arrival order total. |
| `hlc` | uint64 |  | Hybrid logical clock: orders events across devices when wall clocks disagree. |
| `observed_at` | timestamp |  | When the agent did the thing. Order history by this. |
| `captured_at` | timestamp |  | When the hook recorded it. `captured_at - observed_at` is capture lag. |
| `ingested_at` | timestamp | yes | When the database accepted it. Null for events read straight from a segment. |
| `provider` | dict<text> |  | Which coding agent. `attemptdb` marks events AttemptDB wrote itself (corrections, retractions, capture tests); any other adapter contributes its own identifier, so match the values you know rather than assuming the list is closed. Common values: `claude_code`, `codex`, `cursor`, `gemini_cli`, `attemptdb` (open vocabulary — others appear). |
| `provider_version` | text | yes | The agent's own version string, when it reported one. |
| `adapter_version` | text |  | The AttemptDB adapter that normalised the payload. Changes here can change every derived row. |
| `hook_version` | text | yes | The hook binary that captured it. Null for events reconstructed from a transcript. |
| `capture_mode` | dict<text> |  | The privacy mode in force when the event was written. Under `metadata_only` every content column below is null by design. Values: `metadata_only`, `local_semantic`, `full_sync`. |
| `provider_event_name` | dict<text> |  | The provider's own name for the hook that fired, before normalisation. |
| `kind` | dict<text> |  | The canonical event kind. This is the column to filter on; `provider_event_name` is provider-specific. Values: `session_started`, `session_ended`, `prompt_submitted`, `tool_call_started`, `tool_call_finished`, `tool_call_failed`, `permission_requested`, `permission_denied`, `notification`, `agent_message`, `turn_stopped`, `turn_failed`, `subagent_started`, `subagent_stopped`, `task_created`, `task_completed`, `compaction_started`, `compaction_finished`, `config_changed`, `cwd_changed`, `file_changed`, `worktree_created`, `worktree_removed`, `correction`, `retraction`, `capture_test`, `unknown`. |
| `project_id` | uuid |  | Stable id of the repository (`prj_…`), derived from its root path and remote. |
| `project_root` | dict<text> |  | Absolute path of the repository root on the capturing machine. |
| `project_name` | dict<text> |  | `owner/repo` when a git remote is known, otherwise the directory name. This is what a human filters on. |
| `repo_remote` | text | yes | Git remote URL, when the repository has one. |
| `branch` | text | yes | Git branch at the time, when the provider reported one. |
| `head` | text | yes | The commit `HEAD` pointed at when the event was observed. |
| `session_id` | uuid |  | The agent session (`ses_…`), derived from the provider's own session id. |
| `provider_session_id` | text |  | The provider's own session identifier, as it wrote it. |
| `provider_turn_id` | text | yes | The provider's own turn identifier, when it has one. |
| `span_id` | uuid | yes | The tool call this event belongs to (`tc_…`): started and finished events of one call share it. |
| `parent_span_id` | uuid | yes | The enclosing span, for nested calls. |
| `agent_id` | uuid |  | Which agent instance acted (`agt_…`): a subagent has its own. |
| `agent_type` | text | yes | The agent's role as the provider named it — the main loop, or a named subagent. |
| `parent_agent_id` | uuid | yes | The agent that spawned this one. |
| `model` | text | yes | Model name the provider reported for the turn. |
| `provider_agent_id` | text | yes | The provider's own agent identifier. |
| `tool_name` | dict<text> | yes | The tool as the provider named it (`Bash`, `Edit`, `shell`, …). |
| `tool_category` | dict<text> | yes | The normalised category. Filter on this to compare providers. Values: `shell`, `file_read`, `file_write`, `file_edit`, `search`, `web`, `mcp`, `subagent`, `plan`, `notebook`, `other`. |
| `tool_call_id` | text | yes | The provider's own call id, when it issues one. |
| `path_logical` | text | yes | The path as the agent wrote it, absolute or not. |
| `path_relative` | text | yes | The same path relative to the repository root. Filter on this: it is stable across machines. |
| `paths_json` | text | yes | Every path the call touched, as a JSON array of repository-relative strings. |
| `outcome_status` | dict<text> | yes | How the call ended. Values: `success`, `failure`, `denied`, `cancelled`, `unknown`. |
| `outcome_class` | text | yes | A coarser reason, when the provider gave one. Common values: `exit_code`, `denied`, `timeout`, `cancelled` (open vocabulary — others appear). |
| `exit_code` | int32 | yes | Process exit status, when the provider reported one. |
| `duration_ms` | uint64 | yes | Wall-clock milliseconds. |
| `attrs_json` | text |  | The metadata allowlist as JSON (RFC 0006 §4). Content-free by construction: anything that could carry text is rejected before it is written. |
| `content_json` | text | yes | Prompt, message and command text. Content: null under `metadata_only`, and moved to an encrypted blob when a key exists. |
| `raw_json` | text | yes | The provider's original payload. Content, same rules as `content_json`. |
| `content_ref` | text | yes | Blob id holding `content_json` when it was written out of line and encrypted. |
| `raw_ref` | text | yes | Blob id holding `raw_json`, same. |
| `unknown_json` | text | yes | Fields the adapter did not recognise, kept verbatim so an upgrade can read them. Never silently dropped. |

### `sessions`

**inference** · one row per agent session

One run of a coding agent, from the first event that named a session id to the last. Whether it is still open is `state`; whether it is still alive is `last_event_at`, because agents are killed far more often than they exit.

| column | type | null | meaning |
|---|---|---|---|
| `session_id` | text |  | The session (`ses_…`). |
| `provider` | text |  | Which coding agent produced the underlying events. Common values: `claude_code`, `codex`, `cursor`, `gemini_cli`, `attemptdb` (open vocabulary — others appear). |
| `provider_session_id` | text |  | The provider's own session id, for cross-checking against its logs. |
| `project_id` | text |  | Stable id of the repository (`prj_…`), derived from its root path and remote. |
| `project_name` | text |  | `owner/repo` when a git remote is known, otherwise the directory name. This is what a human filters on. |
| `state` | text |  | Whether an end event was observed. `open` also covers a session that was killed without one. Values: `open`, `closed`. |
| `started_at` | timestamp |  | When the row's first evidence was observed. |
| `ended_at` | timestamp | yes | When the row's last evidence was observed. Null while it is still open. |
| `end_reason` | text | yes | Why it ended, as the provider reported it. |
| `start_source` | text | yes | How the session began, as the provider reported it — a fresh start, a resume, a compaction. |
| `event_count` | int64 |  | Events in the session. |
| `turn_count` | int64 |  | Turns in the session. |
| `prompt_count` | int64 |  | Prompts the human submitted. |
| `tool_call_count` | int64 |  | How many tool calls this row contains. Computed with the retraction rules applied — do not re-derive it with COUNT(*). |
| `failure_count` | int64 |  | Tool calls that ended in failure. |
| `agents` | list<text> |  | Every agent instance that acted in the session, main loop and subagents. |
| `coverage` | text |  | How complete the capture is. `full` means hooks recorded everything; `partial` and `minimal` mean some of this session was reconstructed from a transcript, so absence of a row is not evidence of absence. Values: `full`, `partial`, `minimal`, `unknown`. |
| `first_event_id` | text |  | First event of the row, in observation order. |
| `last_event_id` | text |  | Last event of the row, in observation order. |
| `last_event_at` | timestamp |  | When the newest event of the session was observed. This, not `ended_at`, is what tells you a session is still live. |
| `start_event_id` | text | yes | The event that opened the row, when one was observed. |
| `end_event_id` | text | yes | The event that closed the row, when one was observed. |
| `evidence` | list<text> |  | The event ids this row was inferred from. The whole point of an inference: follow these to check the claim. |
| `confidence` | float32 |  | 0.0-1.0. How strongly the evidence supports the row, not how important the row is. |
| `retracted` | bool |  | True when a Retraction removed the row. `SHOW` hides these; SQL does not. |

### `turns`

**inference** · one row per human prompt and the agent's response to it

The unit a human recognises: what was asked, and everything the agent did before it stopped. `objective` is the ask in the human's words when content was captured; `prompt_chars` is there when it was not.

Joins: `session_id` → `sessions.session_id`

| column | type | null | meaning |
|---|---|---|---|
| `turn_id` | text |  | The turn (`trn_…`). |
| `session_id` | text |  | The session this row belongs to (`ses_…`). |
| `provider` | text |  | Which coding agent produced the underlying events. Common values: `claude_code`, `codex`, `cursor`, `gemini_cli`, `attemptdb` (open vocabulary — others appear). |
| `project_id` | text |  | Stable id of the repository (`prj_…`), derived from its root path and remote. |
| `project_name` | text |  | `owner/repo` when a git remote is known, otherwise the directory name. This is what a human filters on. |
| `turn_index` | int64 |  | Position of the turn in its session, from 0. |
| `started_at` | timestamp |  | When the row's first evidence was observed. |
| `ended_at` | timestamp | yes | When the row's last evidence was observed. Null while it is still open. |
| `status` | text |  | How the turn ended. Values: `completed`, `failed`, `in_progress`, `unknown`. |
| `prompt_event_id` | text | yes | The prompt that opened the turn. |
| `stop_event_id` | text | yes | The event that closed it. |
| `tool_call_ids` | list<text> |  | The tool calls making up this row, in order. |
| `tool_call_count` | int64 |  | How many tool calls this row contains. Computed with the retraction rules applied — do not re-derive it with COUNT(*). |
| `objective` | text | yes | What the work was for, in the human's own words. Content: null under `metadata_only`. |
| `prompt_chars` | int64 | yes | Length of the prompt in characters. Metadata: present even under `metadata_only`, where `objective` is null. |
| `first_event_id` | text |  | First event of the row, in observation order. |
| `last_event_id` | text |  | Last event of the row, in observation order. |
| `evidence` | list<text> |  | The event ids this row was inferred from. The whole point of an inference: follow these to check the claim. |
| `confidence` | float32 |  | 0.0-1.0. How strongly the evidence supports the row, not how important the row is. |
| `corrected_by` | text | yes | The Correction event (`ev_…`) that overrode this row's inference, if any. |
| `corrected_at` | timestamp | yes | When that correction was written. |
| `inferred_objective` | text | yes | The objective as the projector read it, kept when a human correction replaced `objective`. The two together are the audit trail. |
| `retracted` | bool |  | True when a Retraction removed the row. `SHOW` hides these; SQL does not. |

### `tool_calls`

**inference** · one row per tool invocation

A started and a finished event paired into one call, with its path, its duration and how it ended. A call with `finished_at IS NULL` is still running — or its completion was never captured, which `sessions.coverage` tells you.

Joins: `session_id` → `sessions.session_id` · `turn_id` → `turns.turn_id`

| column | type | null | meaning |
|---|---|---|---|
| `tool_call_id` | text |  | The tool call (`tc_…`). |
| `session_id` | text |  | The session this row belongs to (`ses_…`). |
| `provider` | text |  | Which coding agent produced the underlying events. Common values: `claude_code`, `codex`, `cursor`, `gemini_cli`, `attemptdb` (open vocabulary — others appear). |
| `project_id` | text |  | Stable id of the repository (`prj_…`), derived from its root path and remote. |
| `project_name` | text |  | `owner/repo` when a git remote is known, otherwise the directory name. This is what a human filters on. |
| `turn_id` | text | yes | The turn (`trn_…`) this row belongs to: one human prompt and everything the agent did in response. |
| `agent_id` | text |  | Which agent instance made the call. |
| `tool_name` | text |  | The tool as the provider named it. |
| `tool_category` | text |  | The normalised category — compare providers on this, not on `tool_name`. Values: `shell`, `file_read`, `file_write`, `file_edit`, `search`, `web`, `mcp`, `subagent`, `plan`, `notebook`, `other`. |
| `provider_call_id` | text | yes | The provider's own call id. |
| `started_at` | timestamp | yes | When the call started. Null when only its completion was observed. |
| `finished_at` | timestamp | yes | When it returned. Null while it is still running. |
| `duration_ms` | int64 | yes | Wall-clock duration. Null unless both ends were observed. |
| `outcome_status` | text | yes | How it ended. Null while it is still running. Values: `success`, `failure`, `denied`, `cancelled`, `unknown`. |
| `outcome_class` | text | yes | A coarser reason for the outcome. Common values: `exit_code`, `denied`, `timeout`, `cancelled` (open vocabulary — others appear). |
| `exit_code` | int32 | yes | Process exit status, when the provider reported one. |
| `path_relative` | text | yes | The primary path, repository-relative. |
| `paths` | list<text> |  | Repository-relative paths this row touched, deduplicated. |
| `command_category` | text | yes | What a shell command was doing, classified from the command line. Common values: `git`, `test`, `build`, `install`, `network`, `fs`, `run`, `other` (open vocabulary — others appear). |
| `git_subcommand` | text | yes | For a git call, the subcommand (`commit`, `push`, …). |
| `lines_added` | int64 | yes | Lines added, when the provider reported a diff. |
| `lines_removed` | int64 | yes | Lines removed, same. |
| `start_event_id` | text | yes | The event that opened the row, when one was observed. |
| `end_event_id` | text | yes | The event that closed the row, when one was observed. |
| `evidence` | list<text> |  | The event ids this row was inferred from. The whole point of an inference: follow these to check the claim. |
| `confidence` | float32 |  | 0.0-1.0. How strongly the evidence supports the row, not how important the row is. |
| `retracted` | bool |  | True when a Retraction removed the row. `SHOW` hides these; SQL does not. |

### `attempts`

**inference** · one row per contiguous run of tool calls pursuing one objective

The table this database is named for: what the agent tried. Several attempts in one turn mean it tried, failed and tried again — `attempt_index`, `supersedes` and `superseded_by` are the retry chain. `outcome = 'superseded'` is a retry, not an independent failure; counting it as one double-counts.

Joins: `session_id` → `sessions.session_id` · `turn_id` → `turns.turn_id` · `work_unit_id` → `work_units.work_unit_id` · `superseded_by` → `attempts.attempt_id`

| column | type | null | meaning |
|---|---|---|---|
| `attempt_id` | text |  | The attempt (`att_…`). |
| `session_id` | text |  | The session this row belongs to (`ses_…`). |
| `provider` | text |  | Which coding agent produced the underlying events. Common values: `claude_code`, `codex`, `cursor`, `gemini_cli`, `attemptdb` (open vocabulary — others appear). |
| `project_id` | text |  | Stable id of the repository (`prj_…`), derived from its root path and remote. |
| `project_name` | text |  | `owner/repo` when a git remote is known, otherwise the directory name. This is what a human filters on. |
| `turn_id` | text |  | The turn (`trn_…`) this row belongs to: one human prompt and everything the agent did in response. |
| `turn_index` | int64 |  | Position of the enclosing turn in its session. |
| `attempt_index` | int64 |  | Position of this attempt within its turn, from 0. Attempt 1 after a failed attempt 0 is a retry. |
| `objective` | text | yes | What the work was for, in the human's own words. Content: null under `metadata_only`. |
| `approach` | text |  | How the attempt went about it, classified from the tool calls it used. Common values: `edit`, `shell`, `search`, `read`, `mixed` (open vocabulary — others appear). |
| `started_at` | timestamp |  | When the row's first evidence was observed. |
| `ended_at` | timestamp | yes | When the row's last evidence was observed. Null while it is still open. |
| `outcome` | text |  | How the attempt ended. `superseded` means a later attempt in the same turn replaced it — that is a retry, not an independent failure. Values: `succeeded`, `failed`, `abandoned`, `superseded`, `in_progress`, `unknown`. |
| `failure_class` | text | yes | What kind of failure, when it failed. Open vocabulary: two failures of the same class are the signal that something is stuck. Common values: `test_failure`, `compile_error`, `permission_denied`, `timeout`, `not_found`, `conflict`, `other` (open vocabulary — others appear). |
| `tool_call_ids` | list<text> |  | The tool calls making up this row, in order. |
| `tool_call_count` | int64 |  | How many tool calls this row contains. Computed with the retraction rules applied — do not re-derive it with COUNT(*). |
| `paths` | list<text> |  | Repository-relative paths this row touched, deduplicated. |
| `commit_shas` | list<text> |  | Commit shas produced under this row. |
| `superseded_by` | text | yes | The attempt that replaced this one. |
| `supersedes` | text | yes | The attempt this one replaced. |
| `evidence` | list<text> |  | The event ids this row was inferred from. The whole point of an inference: follow these to check the claim. |
| `confidence` | float32 |  | 0.0-1.0. How strongly the evidence supports the row, not how important the row is. |
| `algorithm_version` | text |  | The projector version that produced the row (`tier1-v1`). Rows from different versions are not comparable. |
| `work_unit_id` | text | yes | The work unit (`wu_…`) this row was folded into, if any. |
| `corrected_by` | text | yes | The Correction event (`ev_…`) that overrode this row's inference, if any. |
| `corrected_at` | timestamp | yes | When that correction was written. |
| `correction_type` | text | yes | What the correction changed. Values: `attempt_outcome`, `attempt_note`, `turn_objective`. |
| `inferred_outcome` | text | yes | The outcome the projector derived, kept when a human correction replaced `outcome`. Values: `succeeded`, `failed`, `abandoned`, `superseded`, `in_progress`, `unknown`. |
| `inferred_failure_class` | text | yes | The failure class the projector derived, kept for the same reason. Common values: `test_failure`, `compile_error`, `permission_denied`, `timeout`, `not_found`, `conflict`, `other` (open vocabulary — others appear). |
| `note` | text | yes | A human's note from a Correction. Content: null under `metadata_only`. |
| `retracted` | bool |  | True when a Retraction removed the row. `SHOW` hides these; SQL does not. |

### `handoffs`

**inference** · one row per session picking up where another stopped

Two sessions, usually two different agents, touching the same files across a gap. `gap_ms` and `shared_paths` are the whole basis of the inference: a long gap with one shared file is weak evidence and the `confidence` says so.

Joins: `from_session` → `sessions.session_id` · `to_session` → `sessions.session_id`

| column | type | null | meaning |
|---|---|---|---|
| `from_session` | text |  | The session that stopped (`ses_…`). |
| `to_session` | text |  | The session that picked the work up. |
| `from_provider` | text |  | Agent that stopped. Common values: `claude_code`, `codex`, `cursor`, `gemini_cli`, `attemptdb` (open vocabulary — others appear). |
| `to_provider` | text |  | Agent that continued. Common values: `claude_code`, `codex`, `cursor`, `gemini_cli`, `attemptdb` (open vocabulary — others appear). |
| `project_id` | text |  | Stable id of the repository (`prj_…`), derived from its root path and remote. |
| `handoff_at` | timestamp |  | When the second session started. |
| `gap_ms` | int64 |  | Milliseconds between the last event of the first session and the first of the second. A large gap weakens the inference. |
| `shared_paths` | list<text> |  | Paths both sessions touched. This overlap is why the handoff was inferred at all. |
| `evidence` | list<text> |  | The event ids this row was inferred from. The whole point of an inference: follow these to check the claim. |
| `confidence` | float32 |  | 0.0-1.0. How strongly the evidence supports the row, not how important the row is. |

### `edges`

**inference** · one row per causal or structural link between two entities

The graph `WHY` and `TRACE` walk. Endpoints are polymorphic: `from_type`/`to_type` name the table and `from_id`/`to_id` its prefixed id, so join by writing the type into the condition. `edge_source` separates edges the projector asserted from edges the causal layer derived on top of them.

| column | type | null | meaning |
|---|---|---|---|
| `ordinal` | int64 |  | Position in the edge list. A stable handle, not a meaning. |
| `edge_kind` | text |  | What the edge asserts. Values: `parent_of`, `caused`, `triggered`, `blocked`, `resolved`, `superseded`, `produced`, `verified`, `contradicted`, `handed_off`, `evidence_for`. |
| `from_type` | text |  | Kind of entity the edge starts at. Values: `event`, `tool_call`, `turn`, `attempt`, `session`, `work_unit`. |
| `from_id` | text |  | Prefixed id of that entity. |
| `to_type` | text |  | Kind of entity the edge ends at. Values: `event`, `tool_call`, `turn`, `attempt`, `session`, `work_unit`. |
| `to_id` | text |  | Prefixed id of that entity. |
| `evidence` | list<text> |  | The event ids this row was inferred from. The whole point of an inference: follow these to check the claim. |
| `confidence` | float32 |  | 0.0-1.0. How strongly the evidence supports the row, not how important the row is. |
| `edge_source` | text |  | `projection` for edges the projector wrote; `derived` for edges the causal graph added on top of them. Values: `projection`, `derived`. |

### `signals`

**inference** · one row per moment the agent needed a human

Permission requests, denials and notifications, each with the event that cleared it. A row with `pending = true` in an open session is an agent waiting right now — this is the fact behind Needs You.

Joins: `session_id` → `sessions.session_id` · `event_id` → `events.event_id`

| column | type | null | meaning |
|---|---|---|---|
| `session_id` | text |  | The session this row belongs to (`ses_…`). |
| `event_id` | text |  | The event that raised the signal. |
| `raised_at` | timestamp |  | When it was raised. |
| `kind` | text |  | What kind of signal. A permission request is the agent waiting on a human. Values: `permission_requested`, `permission_denied`, `notification`. |
| `signal_type` | text | yes | The provider's own label for a notification. Free text: there is no fixed set to match against. |
| `cleared_at` | timestamp | yes | When the next event in the session arrived, which is what ends the wait. Null while it is still pending. |
| `cleared_by` | text | yes | The event that cleared it. |
| `pending` | bool |  | True while nothing has cleared it. A pending signal in an open session is a human being waited on. |
| `evidence` | list<text> |  | The event ids this row was inferred from. The whole point of an inference: follow these to check the claim. |
| `confidence` | float32 |  | 0.0-1.0. How strongly the evidence supports the row, not how important the row is. |

### `work_units`

**inference** · one row per thread of work, across sessions and agents

What a human would call a task: an objective, the sessions and attempts spent on it, where it stands. It survives session boundaries, agent switches and days, which is what makes it the right grain for "what is going on in this repository". `phase` and `status` are inferences with reasons attached — quote the reason, not just the label.

| column | type | null | meaning |
|---|---|---|---|
| `work_unit_id` | text |  | The work unit (`wu_…`): one thread of work, which may span sessions, agents and days. |
| `version` | int64 |  | How many times the unit has been revised. It grows as evidence arrives. |
| `project_id` | text |  | Stable id of the repository (`prj_…`), derived from its root path and remote. |
| `project_name` | text |  | `owner/repo` when a git remote is known, otherwise the directory name. This is what a human filters on. |
| `objective_event_id` | text | yes | The prompt the objective was read from. |
| `objective` | text | yes | What the unit is for. Content: null under `metadata_only`. |
| `phase` | text |  | Where the work stands. Inferred from the recent tool mix and outcomes, so read `phase_reason` with it. Values: `explore`, `plan`, `implement`, `debug`, `verify`, `review`, `deliver`, `blocked`. |
| `phase_reason` | text |  | Why that phase was chosen, in one sentence. |
| `status` | text |  | Whether the unit is still open. Values: `open`, `completed`, `abandoned`, `unknown`. |
| `status_reason` | text |  | Why that status was chosen. |
| `started_at` | timestamp |  | When the row's first evidence was observed. |
| `updated_at` | timestamp |  | When the newest evidence for this row was observed. |
| `ended_at` | timestamp | yes | When the row's last evidence was observed. Null while it is still open. |
| `sessions` | list<text> |  | Every session that contributed. |
| `session_count` | int64 |  | How many. |
| `turns` | list<text> |  | Every turn that contributed. |
| `turn_count` | int64 |  | How many. |
| `attempts` | list<text> |  | Every attempt in the unit. |
| `attempt_count` | int64 |  | How many. |
| `failed_attempt_count` | int64 |  | How many of them failed. Two failures of the same class with no success after is the repeated-failure signal. |
| `paths` | list<text> |  | Repository-relative paths this row touched, deduplicated. |
| `commit_shas` | list<text> |  | Commit shas produced under this row. |
| `actors` | list<text> |  | The agents that worked on it — more than one means the work was handed off. |
| `last_attempt` | text | yes | The most recent attempt (`att_…`). |
| `blocking_signal` | text | yes | The event id of the signal holding the unit up, when one is pending. |
| `evidence` | list<text> |  | The event ids this row was inferred from. The whole point of an inference: follow these to check the claim. |
| `confidence` | float32 |  | 0.0-1.0. How strongly the evidence supports the row, not how important the row is. |
| `algorithm_version` | text |  | The projector version that produced the row (`tier1-v1`). Rows from different versions are not comparable. |

### `decisions`

**inference** · one row per point where the direction changed

An agent abandoning one approach for another, or a human stepping in. `rationale` is derived from what happened around the change, never typed by anyone, and `rationale_source` says so.

Joins: `session_id` → `sessions.session_id` · `turn_id` → `turns.turn_id` · `work_unit_id` → `work_units.work_unit_id`

| column | type | null | meaning |
|---|---|---|---|
| `decision_id` | text |  | The decision (`dec_…`). |
| `kind` | text |  | What kind of decision. `human_intervention` is a human changing the direction; `approach_change` is the agent abandoning one approach for another. Values: `approach_change`, `human_intervention`. |
| `work_unit_id` | text | yes | The work unit (`wu_…`) this row was folded into, if any. |
| `session_id` | text |  | The session this row belongs to (`ses_…`). |
| `provider` | text |  | Which coding agent produced the underlying events. Common values: `claude_code`, `codex`, `cursor`, `gemini_cli`, `attemptdb` (open vocabulary — others appear). |
| `project_id` | text |  | Stable id of the repository (`prj_…`), derived from its root path and remote. |
| `project_name` | text |  | `owner/repo` when a git remote is known, otherwise the directory name. This is what a human filters on. |
| `turn_id` | text |  | The turn (`trn_…`) this row belongs to: one human prompt and everything the agent did in response. |
| `selected` | text |  | What was chosen, as a prefixed id or a short label. |
| `alternatives` | list<text> |  | What was not chosen, and had evidence behind it. |
| `rationale` | text |  | Why, in one sentence, derived from what happened around it. |
| `rationale_source` | text |  | How the rationale was produced. Always `derived`: nobody typed it. Values: `derived`. |
| `decided_at` | timestamp |  | When the decision was observed. |
| `evidence` | list<text> |  | The event ids this row was inferred from. The whole point of an inference: follow these to check the claim. |
| `confidence` | float32 |  | 0.0-1.0. How strongly the evidence supports the row, not how important the row is. |
| `algorithm_version` | text |  | The projector version that produced the row (`tier1-v1`). Rows from different versions are not comparable. |

### `commits`

**inference** · one row per observed `git commit` call

Where the work landed. One row per commit call, resolved to a sha or not: `linkage` says how confident the tie is, and `sha IS NULL` means the call was seen but the sha never was.

Joins: `session_id` → `sessions.session_id` · `turn_id` → `turns.turn_id` · `attempt_id` → `attempts.attempt_id` · `tool_call_id` → `tool_calls.tool_call_id`

| column | type | null | meaning |
|---|---|---|---|
| `commit_id` | text |  | The commit row (`cmt_…`). Not the sha: one row per observed `git commit` call, resolved or not. |
| `session_id` | text |  | The session this row belongs to (`ses_…`). |
| `provider` | text |  | Which coding agent produced the underlying events. Common values: `claude_code`, `codex`, `cursor`, `gemini_cli`, `attemptdb` (open vocabulary — others appear). |
| `project_id` | text |  | Stable id of the repository (`prj_…`), derived from its root path and remote. |
| `project_name` | text |  | `owner/repo` when a git remote is known, otherwise the directory name. This is what a human filters on. |
| `turn_id` | text | yes | The turn (`trn_…`) this row belongs to: one human prompt and everything the agent did in response. |
| `attempt_id` | text | yes | The attempt (`att_…`) this row belongs to. |
| `tool_call_id` | text |  | The tool call (`tc_…`) this row belongs to. |
| `sha` | text | yes | The commit sha. Null when the commit could not be resolved to one. |
| `previous_sha` | text | yes | What `HEAD` pointed at before the commit. |
| `branch` | text | yes | Git branch at the time, when the provider reported one. |
| `committed_at` | timestamp |  | When the commit call finished. |
| `linkage` | text |  | How the sha was tied to the call. `end_event` means the call itself reported it; `next_head` means the sha was read from the next observed HEAD change, which is weaker; `unresolved` means no sha was found and `sha` is null. Values: `end_event`, `next_head`, `unresolved`. |
| `evidence` | list<text> |  | The event ids this row was inferred from. The whole point of an inference: follow these to check the claim. |
| `confidence` | float32 |  | 0.0-1.0. How strongly the evidence supports the row, not how important the row is. |
| `algorithm_version` | text |  | The projector version that produced the row (`tier1-v1`). Rows from different versions are not comparable. |

### `corrections`

**inference** · one row per human correction of an inference

A human saying the projector got it wrong. The correction is itself an immutable event; it never edits the row it corrects, which keeps both readings — see `attempts.outcome` against `attempts.inferred_outcome`. `status` says whether it found its target.

Joins: `event_id` → `events.event_id` · `session_id` → `sessions.session_id`

| column | type | null | meaning |
|---|---|---|---|
| `event_id` | text |  | The Correction event (`ev_…`). A correction is itself an immutable fact, never an edit of the row it corrects. |
| `corrected_at` | timestamp |  | When the human wrote it. |
| `session_id` | text |  | The session the correction was written into. |
| `project_id` | text |  | Stable id of the repository (`prj_…`), derived from its root path and remote. |
| `correction_type` | text | yes | What the correction changed. Values: `attempt_outcome`, `attempt_note`, `turn_objective`. |
| `target_type` | text | yes | Which kind of entity `target` names. Values: `attempt`, `turn`, `session`. |
| `target` | text |  | The projected entity the row points at, as a prefixed id. |
| `outcome` | text | yes | The outcome the human asserted, for an `attempt_outcome` correction. Values: `succeeded`, `failed`, `abandoned`, `superseded`, `in_progress`, `unknown`. |
| `failure_class` | text | yes | The failure class the human asserted. Common values: `test_failure`, `compile_error`, `permission_denied`, `timeout`, `not_found`, `conflict`, `other` (open vocabulary — others appear). |
| `note` | text | yes | Free text a human wrote. Content: null under `metadata_only`. |
| `note_chars` | int64 | yes | Length of `note` in characters. Metadata, so it survives `metadata_only` even when `note` does not. |
| `status` | text |  | Whether the correction found its target and took effect. Values: `applied`, `target_not_found`, `target_retracted`, `invalid`. |
| `evidence` | list<text> |  | The event ids this row was inferred from. The whole point of an inference: follow these to check the claim. |
| `confidence` | float32 |  | 0.0-1.0. How strongly the evidence supports the row, not how important the row is. |

### `retractions`

**inference** · one row per human retraction

A session, attempt or event removed from every projection — benchmarks, tests, mistaken imports, privacy. The facts stay in the log; the projections behave as if they never happened. `retracted_events` counts what left.

Joins: `event_id` → `events.event_id`

| column | type | null | meaning |
|---|---|---|---|
| `event_id` | text |  | The Retraction event (`ev_…`). |
| `retracted_at` | timestamp |  | When it was written. |
| `project_id` | text |  | Stable id of the repository (`prj_…`), derived from its root path and remote. |
| `target_type` | text | yes | Which kind of entity `target` names. Values: `session`, `event`, `attempt`. |
| `target` | text |  | The projected entity the row points at, as a prefixed id. |
| `reason` | text |  | Why the data was retracted. Values: `benchmark`, `test`, `duplicate`, `mistaken_import`, `privacy`, `revoked`, `other`. |
| `note` | text | yes | Free text a human wrote. Content: null under `metadata_only`. |
| `note_chars` | int64 | yes | Length of `note` in characters. Metadata, so it survives `metadata_only` even when `note` does not. |
| `matched` | bool |  | Whether the target was found. |
| `retracted_events` | int64 |  | How many events left the projections as a result. The facts stay in the log. |
| `evidence` | list<text> |  | The event ids this row was inferred from. The whole point of an inference: follow these to check the claim. |
| `confidence` | float32 |  | 0.0-1.0. How strongly the evidence supports the row, not how important the row is. |

### `conflicts`

**inference** · one row per pair of work units touching the same files

Two threads of work over the same paths. `overlapping = true` means both are still open: two agents editing the same files right now, which is worth interrupting someone over. The committed flags and line counts say how far each has gone.

Joins: `first_work_unit` → `work_units.work_unit_id` · `second_work_unit` → `work_units.work_unit_id`

| column | type | null | meaning |
|---|---|---|---|
| `conflict_id` | text |  | The conflict row. |
| `project_id` | text |  | Stable id of the repository (`prj_…`), derived from its root path and remote. |
| `first_work_unit` | text |  | The work unit that started first (`wu_…`). |
| `second_work_unit` | text |  | The one that started later. |
| `first_started_at` | timestamp |  | When the first unit started. |
| `second_started_at` | timestamp |  | When the second started. |
| `started_at` | timestamp |  | When the overlap began. |
| `updated_at` | timestamp |  | When the newest evidence for the overlap arrived. |
| `paths` | list<text> |  | The files both units touched. This overlap is the conflict. |
| `path_count` | int64 |  | How many. |
| `overlapping` | bool |  | True while both units are still open: two agents editing the same files right now. |
| `first_committed` | bool |  | Whether the first unit has committed the shared paths. |
| `second_committed` | bool |  | Whether the second has. |
| `first_lines_added` | int64 |  | Lines the first unit added to the shared paths. |
| `first_lines_removed` | int64 |  | Lines it removed. |
| `second_lines_added` | int64 |  | Lines the second unit added. |
| `second_lines_removed` | int64 |  | Lines it removed. |
| `evidence` | list<text> |  | The event ids this row was inferred from. The whole point of an inference: follow these to check the claim. |
| `confidence` | float32 |  | 0.0-1.0. How strongly the evidence supports the row, not how important the row is. |
| `algorithm_version` | text |  | The projector version that produced the row (`tier1-v1`). Rows from different versions are not comparable. |

## Example questions

Placeholders (`{session}`, `{attempt}`) stand for a real id; substitute one before running.

**What is going on in this repository right now?**

```
WHAT IS project DOING NOW
```

Open work units with their phase and latest attempt. Answers with `insufficient_evidence` rather than guessing when nothing is active.

**What did the agents try?**

```
SHOW ATTEMPTS ORDER BY started_at DESC LIMIT 20
```

The default view of the database. Add `INCLUDING RETRACTED` to see what a retraction removed.

**What failed?**

```
SHOW FAILED ATTEMPTS
```

`outcome = 'failed'` only. Superseded attempts are retries and are excluded here on purpose.

**What failed in one file?**

```
SHOW FAILED ATTEMPTS FOR path = 'crates/*/src/*.rs'
```

`path` matches against the attempt's `paths` list; `*` is a glob.

**Which attempts were retried?**

```
SHOW SUPERSEDED ATTEMPTS
```

Each row is an attempt a later one replaced, with the successor's outcome — the retry chain, not a list of failures.

**Where did work pass between agents?**

```
SHOW HANDOFFS
```

Read `gap_ms` and `shared_paths` before believing a handoff: they are the evidence.

**Why is this session stuck?**

```
WHY session {session} STATUS BLOCKED
```

Answers from pending signals and repeated same-class failures, and says `state_mismatch` when the session is not blocked at all.

**What caused this attempt?**

```
TRACE attempt {attempt} CAUSES DEPTH 3
```

Walks the causal edges upward. `DIRECTION DOWN` walks to consequences instead.

**What is this claim based on?**

```
SHOW EVIDENCE FOR attempt {attempt}
```

The events the inference was built from, in observation order. Every derived row can be opened this way.

**What did the repository look like yesterday?**

```
STATE project AT '-1d'
```

Sessions and work units as they stood at that moment, with outcomes known only up to then.

**What changed since yesterday?**

```
DIFF STATE '-1d' NOW
```

One row per changed field. Units that completed in between show as `removed` with their final state.

**Which failures repeat?**

```
SELECT failure_class, count(*) AS failures FROM attempts WHERE outcome = 'failed' AND retracted = false GROUP BY failure_class ORDER BY failures DESC
```

`failure_class` is an open vocabulary: two attempts sharing one is the signal that something is genuinely stuck.

**Which work is stuck?**

```
SELECT work_unit_id, phase, phase_reason, failed_attempt_count FROM work_units WHERE status = 'open' AND failed_attempt_count >= 2 ORDER BY updated_at DESC
```

Quote `phase_reason` with the phase: the label alone is an inference presented as fact.

**Is anyone waiting on me?**

```
SELECT s.session_id, s.provider, g.kind, g.raised_at FROM signals g JOIN sessions s ON s.session_id = g.session_id WHERE g.pending = true AND s.state = 'open' ORDER BY g.raised_at
```

A pending signal in an open session is an agent waiting on a human right now.

**How do the agents differ in what they run?**

```
SELECT provider, tool_category, count(*) AS calls FROM tool_calls WHERE retracted = false GROUP BY provider, tool_category ORDER BY calls DESC
```

Compare on `tool_category`, never on `tool_name`: every provider names its tools differently.

**What is slow?**

```
SELECT tool_name, path_relative, duration_ms FROM tool_calls WHERE duration_ms IS NOT NULL ORDER BY duration_ms DESC LIMIT 10
```

Null `duration_ms` means one end of the call was never observed, not that it was instant.

**How much of this history is actually captured?**

```
SELECT provider, sum(CASE WHEN hook_version IS NULL THEN 1 ELSE 0 END) AS reconstructed, count(*) AS events FROM events WHERE retracted = false GROUP BY provider
```

Rows with no `hook_version` were reconstructed from a transcript after the fact. Their absence of a detail is not evidence of absence.

**What did the agents commit?**

```
SELECT committed_at, sha, branch, linkage FROM commits WHERE sha IS NOT NULL ORDER BY committed_at DESC LIMIT 20
```

`linkage = 'next_head'` is a weaker tie than `end_event`; `sha IS NULL` means the commit call was seen but its sha never was.

**Are two agents editing the same files?**

```
SELECT conflict_id, path_count, overlapping, first_committed, second_committed FROM conflicts WHERE overlapping = true
```

`overlapping = true` means both work units are still open — the case worth interrupting someone over.

**What have humans corrected?**

```
SELECT corrected_at, correction_type, target, status FROM corrections ORDER BY corrected_at DESC
```

The audit trail of where the projector was wrong. `status` says whether the correction found its target.

**What kinds of events are in here at all?**

```
SELECT kind, count(*) AS events FROM events WHERE retracted = false GROUP BY kind ORDER BY events DESC
```

The first query to run against an unfamiliar database: it shows what the hooks actually captured.

**What columns does this table have?**

```
DESCRIBE attempts
```

Types straight from the schema. `attempt schema` adds what they mean.

**How will this query run?**

```
EXPLAIN SELECT count(*) FROM events WHERE kind = 'tool_call_failed'
```

The DataFusion plan, including which filters were pushed into the segment scan.

