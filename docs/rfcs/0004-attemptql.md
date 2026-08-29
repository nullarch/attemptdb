# RFC 0004: AttemptQL

| | |
|---|---|
| **Status** | Draft |
| **Authors** | AttemptDB maintainers |
| **Created** | 2026-08-28 |
| **Related** | RFC 0001 (canonical event model), RFC 0002 (storage engine), RFC 0003 (fact/inference model), ADR 0002 (Arrow + DataFusion) |
| **Implementation** | `crates/attemptdb-query` (in progress; stub at the time of writing) |

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
| `events` | one canonical event | all RFC 0001 fields, flattened as in the segment schema (`storage-format.md` §8.2) |
| `sessions` | one session | `session_id`, `provider`, `project_id`, `project_name`, `device_id`, `started_at`, `ended_at`, `state` (`open`/`closed`/`stale`), `event_count`, `coverage_grade`, `evidence`, `confidence` |
| `turns` | one turn | `turn_id`, `session_id`, `agent_id`, `opened_at`, `closed_at`, `outcome` (`stopped`/`failed`/`open`), `prompt_event_id`, `evidence`, `confidence` |
| `tool_calls` | one paired tool call | `span_id`, `turn_id`, `session_id`, `agent_id`, `tool_name`, `tool_category`, `tool_call_id`, `started_at`, `finished_at`, `duration_ms`, `outcome_status`, `outcome_class`, `exit_code`, `path_relative`, `paths_json`, `start_event_id`, `end_event_id`, `pairing` (`call_id`/`fifo`/`single`/`unmatched`), `confidence` |
| `attempts` | one attempt | `attempt_id`, `work_unit_id`, `session_id`, `project_id`, `project_name`, `agent_ids`, `outcome` (`succeeded`/`failed`/`abandoned`/`in_progress`), `superseded_by`, `objective_event_id`, `objective` (text; null without content), `approach` (text; null without content), `paths_json`, `started_at`, `ended_at`, `tool_call_count`, `evidence`, `confidence`, `inference_id` |
| `work_units` | latest version per work unit | `work_unit_id`, `version`, `project_id`, `objective_event_id`, `objective`, `phase`, `status`, `attempt_count`, `failed_attempt_count`, `paths_json`, `actors_json`, `updated_at`, `valid_from`, `valid_to`, `evidence`, `confidence`, `inference_id` |
| `handoffs` | one handoff edge | `from_session_id`, `to_session_id`, `from_provider`, `to_provider`, `from_agent`, `to_agent`, `project_id`, `at`, `shared_paths_json`, `evidence`, `confidence`, `inference_id` |
| `edges` | one causal edge | `from_type`, `from_id`, `to_type`, `to_id`, `edge_type` (RFC 0001 §8.2), `at`, `evidence`, `confidence`, `inference_id` |
| `decisions` | one decision (planned) | `decision_id`, `selected`, `alternatives_json`, `outcome`, `made_by`, `at`, `evidence`, `confidence` |
| `inferences` | every version | all RFC 0003 §3 fields |
| `corrections` | correction events | `correction_id`, `target_inference_id`, `kind`, `author`, `at`, `event_id` |

`work_units_history` (planned) exposes all versions for `AS KNOWN AT`.

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
                [ "ORDER" "BY" column [ "ASC" | "DESC" ] ] [ "LIMIT" integer ] ;
target        = "ATTEMPTS"
              | "FAILED" "ATTEMPTS"
              | "SUPERSEDED" "ATTEMPTS"
              | "HANDOFFS" [ "BETWEEN" agent_filter "AND" agent_filter ]
              | "DECISIONS"
              | "EVIDENCE" "FOR" inference_ref
              | "SESSIONS" | "TURNS" | "TOOL" "CALLS" | "WORK" "UNITS" | "EDGES" ;
agent_filter  = "agent" "=" string ;
inference_ref = id | "attempt" id | "work_unit" id | "handoff" id ;

subject       = "project" [ string ]
              | "session" id | "work_unit" id | "attempt" id
              | "turn" id | "span" id | "event" id
              | "agent" string ;

filter_list   = filter { "AND" filter } ;
filter        = filter_key "=" value ;
filter_key    = "project" | "provider" | "agent" | "session" | "path"
              | "outcome" | "tool" | "phase" | "status" ;

predicate     = (* a DataFusion SQL boolean expression over the target table's columns *) ;

state_name    = identifier ;                 (* BLOCKED, ACTIVE, DONE, ABANDONED, WAITING_ON_HUMAN, or a phase *)
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

### `DIFF STATE <t₁> <t₂>`

Two `STATE` evaluations full-outer-joined on subject id. Rows:
`(subject_type, subject_id, change ∈ {added, removed, changed, unchanged},
before_json, after_json, evidence, uncertainty)`; `unchanged` rows are
omitted unless `WHERE change = 'unchanged'`.

### `SHOW …`

| Target | Equivalent |
|---|---|
| `ATTEMPTS` | `SELECT * FROM attempts` |
| `FAILED ATTEMPTS` | `… WHERE outcome = 'failed'` |
| `SUPERSEDED ATTEMPTS` | `… WHERE superseded_by IS NOT NULL` (joins the superseding attempt's outcome) |
| `HANDOFFS [BETWEEN agent = 'a' AND agent = 'b']` | `SELECT * FROM handoffs` with `from_agent`/`from_provider` and `to_agent`/`to_provider` matched in either order |
| `DECISIONS` | `SELECT * FROM decisions` |
| `EVIDENCE FOR <ref>` | `SELECT e.* FROM inferences i CROSS JOIN UNNEST(i.evidence) AS ev JOIN events e ON e.event_id = ev WHERE i.inference_id = ?` (a subject id resolves to its latest inference) |
| `SESSIONS` / `TURNS` / `TOOL CALLS` / `WORK UNITS` / `EDGES` | the corresponding table |

`FOR` filters map to columns: `project` → `project_name` or `project_id`;
`provider` → `provider`; `agent` → membership in `agent_ids`/`from_agent`/
`to_agent` (an agent value is a provider id, an agent type, or an `agt_` id);
`session` → `session_id`; `path` → `paths_json` contains (glob allowed);
`outcome`, `tool`, `phase`, `status` → same-named columns. `SINCE`/`UNTIL`
apply to the table's primary time column. Default `ORDER BY` is the primary
time column descending; default `LIMIT` is 100.

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
