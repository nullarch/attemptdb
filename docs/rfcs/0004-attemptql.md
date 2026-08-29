# RFC 0004: AttemptQL

| | |
|---|---|
| **Status** | Draft |
| **Authors** | AttemptDB maintainers |
| **Created** | 2026-08-28 |
| **Related** | RFC 0001 (canonical event model), RFC 0002 (storage engine), RFC 0003 (fact/inference model), ADR 0002 (Arrow + DataFusion) |
| **Implementation** | `crates/attemptdb-query` (SQL over every table below; `SHOW`, `WHY`, `TRACE`, `STATE`, `DIFF`, `WHAT IS`, `EXPLAIN`; work units, decisions, corrections, retractions and `INCLUDING RETRACTED` implemented) |

## 1. Summary

AttemptQL is a small, statement-oriented query language for the questions
people actually ask about agent work: *what is this project doing now, why is
it blocked, what caused this, what did the project look like at 14:00, which
attempts failed, what was handed off between agents, what is the evidence for
this claim.* It is not a replacement for SQL. Every AttemptQL statement
compiles to the same DataFusion logical plan that a SQL query over the public
tables would produce, and plain SQL remains available for everything else.

Two properties are non-negotiable:

1. **Results are rows, never prose alone.** Every result set has an
   `evidence` column (event ids) and an `uncertainty` column (confidence and
   reason). A question with no supportable answer returns a row whose
   uncertainty reason is `insufficient_evidence`.
2. **Every derived value is traceable.** `SHOW EVIDENCE FOR <inference>`
   works on anything AttemptQL has ever shown.

## 2. Public tables

The logical schema AttemptQL and SQL share. Physical storage is RFC 0002;
inference tables are RFC 0003. `evidence` columns are `List<FixedSizeBinary(16)>`.

| Table | Grain | Key columns |
|---|---|---|
| `events` | one canonical event | all RFC 0001 fields, flattened as in the segment schema (`storage-format.md` §8.2), ids as prefixed text, plus `retracted` (Boolean: the event or its session was retracted; correction/retraction events are never flagged) |
| `sessions` | one session | `session_id`, `provider`, `provider_session_id`, `project_id`, `project_name`, `state` (`open`/`closed`), `started_at`, `ended_at`, `end_reason`, `start_source`, `event_count`, `turn_count`, `prompt_count`, `tool_call_count`, `failure_count`, `agents`, `coverage`, `first_event_id`, `last_event_id`, `last_event_at`, `start_event_id`, `end_event_id`, `evidence`, `confidence`, `retracted` |
| `turns` | one turn | `turn_id`, `session_id`, `provider`, `project_id`, `project_name`, `turn_index`, `started_at`, `ended_at`, `status` (`completed`/`failed`/`in_progress`/`unknown`), `prompt_event_id`, `stop_event_id`, `tool_call_ids`, `tool_call_count`, `objective`, `prompt_chars`, `first_event_id`, `last_event_id`, `evidence`, `confidence`, `corrected_by`, `corrected_at`, `inferred_objective`, `retracted` |
| `tool_calls` | one paired tool call | `tool_call_id`, `session_id`, `provider`, `project_id`, `project_name`, `turn_id`, `agent_id`, `tool_name`, `tool_category`, `provider_call_id`, `started_at`, `finished_at`, `duration_ms`, `outcome_status`, `outcome_class`, `exit_code`, `path_relative`, `paths`, `command_category`, `git_subcommand`, `start_event_id`, `end_event_id`, `evidence`, `confidence`, `retracted` |
| `attempts` | one attempt | `attempt_id`, `session_id`, `provider`, `project_id`, `project_name`, `turn_id`, `turn_index`, `attempt_index`, `objective` (null without content), `approach` (content-free), `started_at`, `ended_at`, `outcome` (`succeeded`/`failed`/`abandoned`/`superseded`/`in_progress`/`unknown`), `failure_class`, `tool_call_ids`, `tool_call_count`, `paths`, `superseded_by`, `supersedes`, `evidence`, `confidence`, `algorithm_version`, `work_unit_id`, `corrected_by`, `corrected_at`, `correction_type`, `inferred_outcome`, `inferred_failure_class`, `note`, `retracted` |
| `work_units` | one work unit (`tier1-v0`, RFC 0003 §5.6) | `work_unit_id`, `version`, `project_id`, `project_name`, `objective_event_id`, `objective`, `phase`, `phase_reason`, `status`, `status_reason`, `started_at`, `updated_at`, `ended_at`, `sessions`, `session_count`, `turns`, `turn_count`, `attempts`, `attempt_count`, `failed_attempt_count`, `paths`, `actors`, `last_attempt`, `blocking_signal`, `evidence`, `confidence` (≤ 0.7), `algorithm_version` |
| `decisions` | one derived decision (RFC 0003 §5.7) | `decision_id`, `kind` (`approach_change`/`human_intervention`), `work_unit_id`, `session_id`, `provider`, `project_id`, `project_name`, `turn_id`, `selected`, `alternatives`, `rationale` (content-free), `rationale_source` (always `derived`), `decided_at`, `evidence`, `confidence` (≤ 0.7), `algorithm_version` |
| `handoffs` | one handoff edge | `from_session`, `to_session`, `from_provider`, `to_provider`, `project_id`, `handoff_at`, `gap_ms`, `shared_paths`, `evidence`, `confidence` |
| `edges` | one causal edge | `ordinal`, `edge_kind` (RFC 0001 §8.2; `parent_of` also links `work_unit → turn`), `from_type`, `from_id`, `to_type`, `to_id`, `evidence`, `confidence`, `edge_source` (`projection`/`derived`) |
| `signals` | one pending-input signal | `session_id`, `event_id`, `raised_at`, `kind`, `signal_type`, `cleared_at`, `cleared_by`, `pending`, `evidence`, `confidence` |
| `corrections` | one `Correction` event (RFC 0003 §8.1) | `event_id`, `corrected_at`, `session_id`, `project_id`, `correction_type`, `target_type`, `target`, `outcome`, `failure_class`, `note`, `note_chars`, `status` (`applied`/`target_not_found`/`target_retracted`/`invalid`), `evidence`, `confidence` |
| `retractions` | one `Retraction` event (RFC 0003 §8.2) | `event_id`, `retracted_at`, `project_id`, `target_type`, `target`, `reason`, `note`, `note_chars`, `matched`, `retracted_events`, `evidence`, `confidence` |
| `inferences` | every version (planned) | all RFC 0003 §3 fields |

Lists (`evidence`, `paths`, `sessions`, …) are `List<Utf8>` with prefixed
ids; the `_json` columns of earlier drafts were not built. `sessions`,
`turns`, `tool_calls` and `attempts` also hold the rows a retraction removed
from the projection, flagged `retracted = true` (`SELECT … WHERE NOT
retracted` is what `SHOW` does by default). `work_units_history` (planned)
exposes all versions for `AS KNOWN AT`.

## 3. Grammar

EBNF (ISO 14977 style; terminals are case-insensitive keywords unless
quoted; whitespace separates tokens; `--` starts a line comment):

```ebnf
statement     = [ "EXPLAIN" ] command [ ";" ] ;

command       = what_is | why | trace | state | show | diff ;

what_is       = "WHAT" "IS" subject "DOING" "NOW" ;
why           = "WHY" subject "STATUS" state_name ;
trace         = "TRACE" subject "CAUSES" [ "DEPTH" integer ] [ "DIRECTION" ( "UP" | "DOWN" | "BOTH" ) ] ;
state         = "STATE" subject "AT" timestamp [ "AS" "KNOWN" "AT" timestamp ] ;
diff          = "DIFF" "STATE" [ subject ] timestamp timestamp ;

show          = "SHOW" target [ "FOR" filter_list ] [ "WHERE" predicate ]
                [ "SINCE" timestamp ] [ "UNTIL" timestamp ]
                [ "ORDER" "BY" column [ "ASC" | "DESC" ] ] [ "LIMIT" integer ]
                [ "INCLUDING" "RETRACTED" ] ;
target        = "ATTEMPTS"
              | "FAILED" "ATTEMPTS"
              | "SUPERSEDED" "ATTEMPTS"
              | "HANDOFFS" [ "BETWEEN" agent_filter "AND" agent_filter ]
              | "WORK" "UNITS" | "DECISIONS"
              | "EVIDENCE" "FOR" inference_ref
              | "SESSIONS" | "TURNS" | "TOOL" "CALLS" | "EDGES" | "SIGNALS"
              | "CORRECTIONS" | "RETRACTIONS" ;
agent_filter  = "agent" "=" string ;
inference_ref = id | "attempt" id | "work_unit" id | "session" id | "turn" id | "event" id ;

subject       = "project" [ string ]
              | "session" id | "work_unit" id | "attempt" id
              | "turn" id | "span" id | "event" id
              | "agent" string ;

filter_list   = filter { "AND" filter } ;
filter        = filter_key "=" value ;
filter_key    = "project" | "provider" | "agent" | "session" | "turn" | "path"
              | "outcome" | "tool" | "phase" | "status" | "since" | "until" ;

predicate     = (* a DataFusion SQL boolean expression over the target table's columns *) ;

state_name    = identifier ;                 (* BLOCKED (session, project, work unit) or FAILED (attempt) today *)
id            = prefixed_uuid | uuid | short_id ;
prefixed_uuid = prefix uuid ;                (* ev_ ses_ trn_ spn_ att_ wu_ agt_ dec_ art_ inf_ cor_ prj_ dev_ *)
short_id      = prefix hex { hex } ;         (* prefix + ≥ 8 hex digits; must be unambiguous *)
timestamp     = string | "NOW" | relative ;  (* '2026-08-20T14:00:00Z', '2026-08-20', 'now', '-2h', '-3d' *)
relative      = "-" integer ( "m" | "h" | "d" | "w" ) ;
value         = string | integer | identifier ;
string        = "'" { character } "'" ;
```

Subject `project` without a name means "the project of the current working
directory" (resolved by the CLI, RFC 0005), or all projects when not
resolvable. Short ids (`att_01a04762`) are expanded against the identity
index; an ambiguous prefix is an error, never a guess.

## 4. Statement semantics

Each statement is defined by the SQL it is equivalent to. `evidence` and
`uncertainty` are always projected; `uncertainty` is a struct
`{ confidence: Float32, reason: Utf8, note: Utf8 }`.

### `WHAT IS <subject> DOING NOW`

`STATE <subject> AT NOW` restricted to `status IN ('active', 'waiting_on_human')`,
ordered by `updated_at DESC`. Rows: work units (or, for a session/attempt
subject, the single unit containing it) with `phase`, `status`, latest
attempt, and `last_event_at`. If no unit is active, one row with
`reason = 'insufficient_evidence'` and `note` naming the last observed event
time.

### `WHY <subject> STATUS <state>`

Finds the latest inference asserting `<state>` for the subject, then joins
the edges that justify it: for `BLOCKED`, the `blocked` edges and their
source events (permission requests, denials, failed turns); for `ACTIVE`, the
most recent triggering prompt and tool calls; for `ABANDONED`, the last
attempt and the idle gap. Rows: one per justifying edge or event, with the
inference id, so `SHOW EVIDENCE FOR` can follow. If the subject does not
currently hold `<state>`, the result says which state it does hold (one row,
`reason = 'state_mismatch'`), and does not invent a justification.

Implemented today: `BLOCKED` for a session or project (uncleared
pending-input signal, or the last two attempts failed the same way) and for
a **work unit** (`WHY wu_… STATUS BLOCKED`: the unit's `blocking_signal` —
an uncleared pending-input signal in a member session, the same fact that
makes its phase `blocked` — else its last two attempts failing with the
same class; a unit that is not blocked answers `state_mismatch` with its
actual phase and status); `FAILED` for an attempt (names the failing event,
the superseding attempt, and any human correction that set the outcome).

### `TRACE <subject> CAUSES`

Recursive traversal of `edges` from the subject over the causal types
(`caused`, `triggered`, `blocked`, `resolved`, `superseded`, `contradicted`,
`handed_off`), default direction `UP` (toward causes), default `DEPTH 5`.
Rows: `(depth, edge_type, from_type, from_id, to_type, to_id, at, evidence,
uncertainty)` in traversal order. Cycles are cut; a row with
`reason = 'depth_limit'` marks where traversal stopped.

### `STATE <subject> AT <t>`

Work units and attempts whose `valid_from ≤ t` and (`valid_to IS NULL OR
valid_to > t`), using the latest non-superseded inferences (RFC 0003 §4).
`AS KNOWN AT t₂` (planned) uses `work_units_history` filtered by
`inferred_at ≤ t₂`. Rows: one per subject with its state at `t`.

Implemented: one row per **session** active at `t` (`subject_type =
'session'`: open/closed, current turn, in-flight tool calls, last attempt
and its outcome as known at `t`, blocked flag) followed by one row per
**work unit** open at `t` (`subject_type = 'work_unit'`: `phase`, `status`,
`attempt_count`, `failed_attempt_count`, `sessions`, last attempt outcome
as known at `t`). Units are recomputed at `t` by
`Projection::work_units_at` (only entities observed by `t`, outcomes
masked, corrections after `t` ignored, idleness judged against `t`); a unit
that was already `completed` or `abandoned` by `t` is not listed (its
`valid_to` has passed) and the note reports how many were. The subject may
be a project, a session (units containing it) or a work unit. Retracted
entities never appear.

### `DIFF STATE <t₁> <t₂>`

Two `STATE` evaluations full-outer-joined on subject id. Rows:
`(subject_type, subject_id, session_id, provider, change ∈ {added, removed,
changed}, field, before, after, confidence, uncertainty, evidence)` — one
row per changed field, for sessions (open, turn, in-flight calls, last
attempt and outcome, blocked) and for work units (phase, status, attempt
and failure counts, session count, last attempt, blocked). A unit that
completed between the two times shows as `removed` with its final state in
`after`. `unchanged` rows are omitted.

### `SHOW …`

| Target | Equivalent |
|---|---|
| `ATTEMPTS` | `SELECT * FROM attempts` |
| `FAILED ATTEMPTS` | `… WHERE outcome = 'failed'` |
| `SUPERSEDED ATTEMPTS` | `… WHERE superseded_by IS NOT NULL` (joins the superseding attempt's outcome) |
| `HANDOFFS [BETWEEN agent = 'a' AND agent = 'b']` | `SELECT * FROM handoffs` with `from_agent`/`from_provider` and `to_agent`/`to_provider` matched in either order |
| `WORK UNITS` | `SELECT * FROM work_units` |
| `DECISIONS` | `SELECT * FROM decisions` |
| `CORRECTIONS` / `RETRACTIONS` | the corresponding table |
| `EVIDENCE FOR <ref>` | the events named by the entity's `evidence` (attempt, turn, session, tool call, work unit, or a single event), as `events` rows in observed order |
| `SESSIONS` / `TURNS` / `TOOL CALLS` / `EDGES` / `SIGNALS` | the corresponding table |

`FOR` filters map to columns: `project` → `project_name` or `project_id`;
`provider` → `provider` (`actors` contains, for work units; either end of a
handoff); `agent` → an `agt_` id (sessions, tool calls) or a provider;
`session` → `session_id` (`sessions` contains, for work units; either end of
a handoff); `turn` → `turn_id` (`turns` contains, for work units); `path` →
`paths` contains (`*` glob allowed); `outcome`, `tool`, `status` →
same-named columns (`status` is the work-unit status, `outcome`/`status`
of `sessions` is `state`); `phase` → work-unit `phase` (validated against
the phase vocabulary). `SINCE`/`UNTIL` apply to the table's primary time
column (`started_at`, `handoff_at`, `raised_at`, `decided_at`,
`corrected_at`, `retracted_at`). Default `ORDER BY` is the primary time
column descending; default `LIMIT` is 100.

**Retracted rows.** `SHOW SESSIONS` / `TURNS` / `TOOL CALLS` / `ATTEMPTS`
(and the `FAILED` / `SUPERSEDED` variants) add `AND NOT retracted` and note
how many rows were hidden; `… INCLUDING RETRACTED` returns them with
`retracted = true`. `SHOW EVIDENCE FOR … INCLUDING RETRACTED` likewise
includes retracted events. Tables without retracted rows accept the clause
and say it had no effect. Retracted sessions and attempts are not subjects:
`WHY`, `TRACE`, `STATE` and `SHOW EVIDENCE FOR` resolve ids against the
live projection only.

## 5. Compilation

```text
AttemptQL text ─► parser (hand-written recursive descent) ─► AST
      ─► planner: AST → DataFusion LogicalPlan over the public tables
      ─► DataFusion optimiser (pushdown of time range, project, provider into the segment table provider)
      ─► physical plan ─► Arrow RecordBatch stream
SQL text      ─► DataFusion SQL parser ─► the same LogicalPlan type ─► same path
```

- Public tables are DataFusion `TableProvider`s over segments (RFC 0002) plus
  the MemTable, with `min/max` pruning from manifest statistics.
- `TRACE` uses a custom logical node (`CausalTraverse`) with a physical
  implementation that walks the causal adjacency index; in SQL it is exposed
  as the table function `attempt_trace(id, depth, direction)`.
- `STATE AT` compiles to ordinary filters; `state_at(ts)` is the SQL table
  function equivalent. `evidence(inference_id)` is the SQL equivalent of
  `SHOW EVIDENCE FOR`.
- Result transport is Arrow (in-process, IPC over the local API, or Flight —
  planned); the CLI renders tables with `evidence` collapsed to counts unless
  `--evidence` is passed.
- Limits: every query runs with a memory budget, a timeout (default 30 s),
  and cancellation; large results stream.

## 6. `EXPLAIN`

`EXPLAIN <command>` returns rows describing: the AttemptQL AST, the logical
plan, the physical plan, the segments scanned versus pruned (with the manifest
statistic that pruned each), the indexes used, the inference algorithm
versions consulted, and whether the MemTable was included. `EXPLAIN ANALYZE`
(planned) adds actual row counts and timings.

## 7. Error conventions

Errors are structured, positional, and never echo content-bearing data.

```text
error[AQL0102]: expected DOING after subject
  --> query:1:23
   |
 1 | WHAT IS project 'attemptdb' NOW
   |                             ^^^ expected 'DOING'
```

| Code range | Category | Examples |
|---|---|---|
| `AQL01xx` | Lexical | unterminated string, bad relative timestamp |
| `AQL02xx` | Syntax | unexpected token, missing keyword (with the expected token) |
| `AQL03xx` | Resolution | unknown id, ambiguous short id (lists candidate ids), unknown project name (suggests `SHOW SESSIONS`) |
| `AQL04xx` | Semantics | unknown state name (lists valid names), `BETWEEN` with identical agents, `DIFF` with `t₁ ≥ t₂` |
| `AQL05xx` | Execution | timeout, memory limit, cancelled, segment unreadable (names the file) |
| `AQL06xx` | Evidence | inference without evidence (internal invariant violation; reported, never rendered) |

Messages say what was expected, where, and what to do; they name ids and
column names but never render `content`, `raw`, prompts, paths beyond
`repo_relative`, or values from `attrs`.

## 8. Examples against the self-hosted build history

The dataset is AttemptDB's own development history (planned; ids below are
illustrative and follow the README's Codex hooks example).

```sql
SHOW FAILED ATTEMPTS FOR project = 'attemptdb' SINCE '-7d';
```

| attempt_id | outcome | superseded_by | paths | started_at | evidence | uncertainty |
|---|---|---|---|---|---|---|
| att_01a03f10 | failed | att_01a03f9c | crates/attemptdb-capture/src/install/codex.rs | 2026-08-27T09:12:04Z | 14 events | 0.7 heuristic |

```sql
WHY work_unit wu_01a03e00 STATUS BLOCKED;
```

| inference_id | edge_type | from_type | from_id | at | evidence | uncertainty |
|---|---|---|---|---|---|---|
| inf_01a03fa0 | blocked | event | ev_01a03f8e (permission_denied, tool Bash) | 2026-08-27T09:31:40Z | 1 event | 1.0 deterministic |

```sql
TRACE attempt att_01a03f9c CAUSES;
```

| depth | edge_type | from | to | at | evidence | uncertainty |
|---|---|---|---|---|---|---|
| 1 | superseded | att_01a03f10 | att_01a03f9c | 2026-08-27T09:40:11Z | 3 events | 0.7 heuristic |
| 2 | caused | ev_01a03f77 (tool_call_failed, Edit) | att_01a03f10 | 2026-08-27T09:18:52Z | 1 event | 1.0 deterministic |
| 3 | triggered | ev_01a03f01 (prompt_submitted) | att_01a03f10 | 2026-08-27T09:12:04Z | 1 event | 1.0 deterministic |

```sql
STATE project AT '2026-08-27T09:30:00Z';
```

| subject_type | subject_id | phase | status | attempts | failed | evidence | uncertainty |
|---|---|---|---|---|---|---|---|
| work_unit | wu_01a03e00 | DEBUG | active | 2 | 1 | 41 events | 0.5 heuristic |

```sql
SHOW HANDOFFS BETWEEN agent = 'claude_code' AND agent = 'codex';
```

| from_session | to_session | from_provider | to_provider | at | shared_paths | evidence | uncertainty |
|---|---|---|---|---|---|---|---|
| ses_f35dc30f | ses_7a1e09b2 | claude_code | codex | 2026-08-27T10:02:15Z | 2 | 9 events | 0.8 heuristic |

```sql
SHOW EVIDENCE FOR att_01a03f10;
```

Returns the 14 events (prompt, tool calls, the failed edit, the session
end) as `events` rows.

```sql
DIFF STATE '2026-08-27T09:00:00Z' '2026-08-27T11:00:00Z';
```

| subject_type | subject_id | change | before | after | evidence | uncertainty |
|---|---|---|---|---|---|---|
| work_unit | wu_01a03e00 | changed | `{phase: IMPLEMENT, status: active}` | `{phase: VERIFY, status: active}` | 63 events | 0.5 heuristic |

When the capture mode hid content, `objective` and `approach` are null and
`uncertainty.reason = 'content_unavailable'`; the structural columns are
unaffected.

## 9. Boundary between AttemptQL and SQL (open)

The current position: AttemptQL owns the *verbs* (`WHAT IS`, `WHY`, `TRACE`,
`STATE`, `DIFF`, `SHOW`) and the evidence/uncertainty contract; SQL owns
arbitrary joins, aggregation, and ad-hoc analysis, with table functions
(`attempt_trace`, `state_at`, `evidence`) so nothing is reachable only from
AttemptQL. Whether to grow AttemptQL toward SQL (adding `GROUP BY`, joins) or
to keep it deliberately small and push power users to SQL is the main open
question of this RFC.

## Decisions

- AttemptQL is a small statement language, not a SQL dialect; the statements
  are those in §3.
- Every result is rows with `evidence` and `uncertainty` columns;
  `insufficient_evidence` is a valid, first-class answer.
- AttemptQL and SQL compile to the same DataFusion logical plan over the
  public tables of §2; DataFusion executes both.
- Subjects are `project`, `session`, `work_unit`, `attempt`, `turn`, `span`,
  `event`, `agent`; ids accept display prefixes and unambiguous short forms.
- `SHOW` hides retracted rows by default; `INCLUDING RETRACTED` shows them
  flagged. The `events` view carries a `retracted` column for SQL.
- `work_units`, `decisions`, `corrections` and `retractions` are ordinary
  tables; decisions are derived (`rationale_source = 'derived'`) and, like
  work units, capped at confidence 0.7.
- `EXPLAIN` reports segments scanned versus pruned and inference versions
  consulted.
- Errors are coded, positional, and never echo content.

## Open questions

- The AttemptQL/SQL boundary (§9).
- Whether `WHERE` should accept full DataFusion SQL expressions or a
  restricted safe subset.
- `AS KNOWN AT` syntax and whether `work_units_history` should be a separate
  table or a `VERSIONS` modifier.
- How `agent = 'claude-code'` (display name) versus `'claude_code'`
  (provider id) versus `agt_…` should resolve; current plan: accept all,
  normalise to ids.
- Whether `SHOW ATTEMPTS FOR path = 'src/**'` needs a proper glob semantics
  document.
- Result transport for large evidence lists (inline list vs lazy handle).
