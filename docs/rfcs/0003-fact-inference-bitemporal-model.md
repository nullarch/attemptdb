# RFC 0003: Facts, Inferences, and the Bitemporal Model

| | |
|---|---|
| **Status** | Draft |
| **Authors** | AttemptDB maintainers |
| **Created** | 2026-08-28 |
| **Related** | RFC 0001 (canonical event model), RFC 0002 (storage engine), RFC 0004 (AttemptQL), RFC 0006 (privacy and sync) |
| **Implementation** | `crates/attemptdb-project` (Tier 1 `tier1-v5`: sessions, turns, tool calls, attempts, handoffs, work units, decisions, conflicts, corrections, retractions), `crates/attemptdb-query` (tables and AttemptQL), `attempt correct` / `attempt retract` (CLI). See "What is implemented today" below: the stored, versioned inference record and `AS KNOWN AT` are **planned**. |

## What is implemented today

This RFC describes the model and the road to it. So that nobody has to guess
which parts exist (`crates/attemptdb-project/tests/rfc_version.rs` fails when
the version above and `ALGORITHM_VERSION` disagree):

**Implemented.** Facts (`events`) are immutable and append-only. Every
derived row is *recomputed* from the fact stream by a deterministic
projection (Tier 1) and carries `evidence` (event ids), `confidence` and
`algorithm_version` (§5; the `corrections` and `retractions` rows are the
audit trail and carry evidence and confidence only). Human corrections and
retractions are events and are applied in stream order (§8). `STATE … AT t`
answers about **valid time**: what was true at *t*, using only what had been
observed and corrected by *t*. A session's `open` / `stale` / `closed` state
and a work unit's `status` are additionally a function of an explicit "as of"
instant (§5.1, §5.6): the wall clock for a live reader, a fixed instant for a
test or a replay. Nothing in the projection reads the clock.

**Planned, not implemented.** The stored `Inference` record of §3
(`inference_id`, `version`, `valid_to`, `inferred_at`, `superseded_by`,
`inputs_hash`, `contradicting`, `tier`) and the `inferences` table; the
inferred and superseded times of §4 and therefore `STATE … AS KNOWN AT`;
"replaying the same version is a no-op", incremental re-projection by
`inputs_hash` and `attempt project` (§9); Tier 2 and Tier 3 (§6, §7);
`provider_turn_id` grouping of turns (§5.2); `produced` edges to artifacts
and `continuation` edges (§5.4, §5.5); attempts that continue across turns,
cross-session supersession within 24 hours and abandonment after 30 minutes
of inactivity at the *attempt* level (§5.4; today the *work unit* is
abandoned after two idle hours, §5.6). Where a section below describes a
planned part it says so. Because derived rows are recomputed rather than
stored, "inferences are versioned and superseded, never edited" (§1, §2) is
the target; today the guarantee is the weaker "the same facts and the same
algorithm version give the same rows", and a changed algorithm changes the
rows (`algorithm_version` says which).

## 1. Summary

AttemptDB stores two different kinds of things and never lets them blur:

- **Facts** are canonical events (RFC 0001). They are observed, immutable,
  append-only, and carry no interpretation beyond what the provider said.
- **Inferences** are derived claims about facts — "these seven tool calls were
  one attempt", "this work unit is blocked", "Claude handed this off to
  Codex". Every inference records its confidence, the event ids it rests on,
  the algorithm or model and version that produced it, and the interval over
  which it claims to be valid. Inferences are versioned and superseded, never
  edited.

Higher-level primitives — sessions, turns, tool calls, attempts, work units,
decisions, handoffs, artifacts — are all inferences (or, for the simplest
ones, deterministic projections, which are inferences with confidence 1.0 and
a deterministic algorithm). A human **correction** is a first-class event that
supersedes an inference; it is evidence, not an overwrite.

The rule that follows from this: **every user-visible claim links to
evidence, and "insufficient evidence" is a valid answer.** A timeline that
confidently explains the work incorrectly is worse than one that says it
does not know.

## 2. Fact versus inference

| | Fact (`Event`) | Inference |
|---|---|---|
| Source | Provider hook payload, import, or a human action (correction) | An algorithm, a local model, or an optional cloud model |
| Mutability | Immutable once ingested | Target: immutable once written, superseded by a newer version. Today: recomputed from facts on every read (see "What is implemented today") |
| Identity | `event_id` (UUIDv7) | `inference_id` (UUIDv7); the *subject* (work unit, attempt, …) keeps a stable id across versions |
| Ordering | `source_seq`, `hlc` | `inferred_at`; versions of one subject are totally ordered |
| Confidence | not applicable | required, 0.0–1.0, from a fixed palette per algorithm (no false precision) |
| Storage | WAL + segments (RFC 0002) | `inferences` and `edges` tables (planned: a separate segment family under the same manifest) |
| Query | `events` | `attempts`, `work_units`, `handoffs`, `edges`, `inferences` (RFC 0004) |

A projection that reads facts and writes inferences is **replayable**: given
the same fact stream and the same algorithm version it produces the same
output. This is a hard requirement (master plan §15 "deterministic projections
from identical fact streams") and is what makes re-projection after an
algorithm improvement safe.

## 3. The Inference record

```text
Inference
  inference_id      InferenceId   UUIDv7
  subject_type      enum          session | turn | tool_call | attempt | work_unit | decision | handoff | artifact | edge
  subject_id        UUID          stable across versions of the same subject
  version           u32           1, 2, … per subject
  claim             object        algorithm-specific payload (e.g. WorkUnit fields)
  confidence        f32           0.0–1.0
  evidence          [EventId]     non-empty; the facts this rests on
  contradicting     [EventId]     facts the algorithm saw that argue against the claim (may be empty)
  algorithm         string        "tier1" | "tier2-local" | "tier3-cloud" | "correction"
  algorithm_version string        e.g. "tier1-v0"
  model             string?       model id for Tier 2/3
  prompt_hash       string?       SHA-256 hex of the prompt template + parameters (Tier 2/3)
  inputs_hash       string        SHA-256 hex over the sorted evidence ids, to detect staleness
  valid_from        Timestamp     start of the interval the claim describes (observed time)
  valid_to          Timestamp?    end of that interval; absent = still valid
  inferred_at       Timestamp     when this version was computed
  superseded_by     InferenceId?  the version or correction that replaced this one
  superseded_at     Timestamp?
  tier              u8            1, 2, 3, or 0 for a correction
```

Rules:

- `evidence` is never empty. An inference with no evidence is not written; the
  query layer answers `insufficient_evidence` instead.
- `confidence` values come from the algorithm's documented palette (Tier 1
  below uses `1.0 / 0.9 / 0.7 / 0.6 / 0.5`). Algorithms do not emit
  `0.8734`.
- Supersession is a pointer, not a deletion. Old versions remain queryable
  (`STATE … AT`, audit, evaluation).
- A correction supersedes with `algorithm = "correction"` and its own
  `evidence` being the correction event's id plus whatever the human cited.

## 4. Four times

| Time | Field(s) | Meaning | Who sets it |
|---|---|---|---|
| **Observed** | `Event.observed_at` | When the fact happened in the world, per the provider | adapter |
| **Valid** | `valid_from`, `valid_to` | The interval of observed time over which the claim describes the world ("blocked from 14:02 until 14:31") | the algorithm, from evidence timestamps |
| **Inferred** | `inferred_at` | When the claim was computed (transaction time) | the projection worker |
| **Superseded** | `superseded_at` | When a newer version or a correction replaced the claim | the projection worker or the correction |

The model is bitemporal: valid time answers "what was true at *t*"; inferred
time answers "what did we believe at *t₂*". `STATE <subject> AT t` (RFC 0004)
uses the **latest** non-superseded inferences whose valid interval contains
*t* — the best current explanation of the past. `STATE … AT t AS KNOWN AT t₂`
(planned) restricts to inferences with `inferred_at ≤ t₂` and not superseded
before `t₂`, which is what evaluation and "why did the timeline say that
yesterday" need.

## 5. Tier 1: deterministic projection (`tier1-v5`)

Version history (derived rows from different versions are not comparable;
`algorithm_version` is on every inferred table):

- `tier1-v2` keeps pending human-input signals across background
  notifications, compaction and configuration observations. A subsequent
  progress event, permission decision, session/turn end, another input
  request, or elicitation result clears or replaces the signal.
- `tier1-v5` makes the projection honest about time and identity. Sessions
  are `open`, `stale` or `closed` as of an explicit instant (§5.1) with a
  confidence that follows coverage; a wait for a human is cleared only by the
  agent that raised it, and an `idle_prompt` after a finished turn is not a
  wait; a session used after its end is resumed; tool-call and edge
  confidences stop claiming `1.0` for guesses (§5.3, §5.4); attempts are
  split per agent, ignore failing exploration commands and take
  evidence-derived ids (§5.4); handoffs are detected per turn span and
  survive a round trip between agents (§5.5); work units link only through
  edited paths within six hours and never through hot files (§5.6); an
  interrupted turn ages into `abandoned` (§5.6). Prior projected caches must
  be rebuilt; immutable source events are unchanged.

Tier 1 needs no content. It runs in `metadata_only` mode with full fidelity
and is the baseline every provider must reach with less than ten percentage
points of accuracy difference (§8). Every rule below is a function of
canonical fields only.

### 5.1 Sessions (confidence by coverage)

Group events by `session_id` (events of a provider that sent no session id,
`unknown`, get one session per project instead of sharing one). `started_at`
= `observed_at` of the `session_started` event, else the earliest event.
`ended_at` = `observed_at` of `session_ended` if present; a session used
again afterwards (a new `session_started`, a human prompt, a tool call or a
permission request, not a trailing completion hook) has been **resumed** and
is not ended any more.

`state` is judged against an explicit `as_of` instant (the projection's
`reference_time`): `closed` when an end was observed and not resumed;
otherwise `open` while the last non-end event is within **30 minutes** of
`as_of`, else `stale` (closed by inactivity). A session that is waiting on a
human (an uncleared blocking signal) stays `open` for **12 hours** (a
documented choice: nothing happens in a session that waits for an approval).
`stale` is an inference from silence, never a fact, so it is capped at
confidence `0.7`; `state` is the one column that depends on *when* the
projection is judged, not only on the events. A live reader passes the wall
clock; tests and replays pass a fixed instant; with none given the stream's
latest timestamp is used, which is what makes a replay reproducible.

Confidence follows the coverage grade: `full` (start, end, prompts and tool
calls all observed) `1.0`, `partial` `0.8`, `minimal` (activity only) `0.6`,
`unknown` `0.4`. An `open` session is therefore at most `0.8`: its end has
not been observed. Attributes: provider, project, agents seen, event count,
coverage grade; device is on the events.

### 5.2 Turns (confidence 1.0; 0.9 when synthesised)

Within a session, order events by `source_seq`. If events carry
`provider_turn_id`, group by it (*planned*: today turns are always
synthesised). A turn opens at each
`prompt_submitted` for the top-level agent and closes at the next
`turn_stopped` or `turn_failed` for that agent, or at the next
`prompt_submitted`, whichever comes first (confidence 0.9 for the synthesised
boundary). Events before the first prompt form a `setup` pseudo-turn. Turn id:
`TurnId::derive([session_id, provider_turn_id])` or
`TurnId::derive([session_id, opening_event_id])`.

### 5.3 Tool-call pairing

For every `tool_call_started` (S) find its terminal event (T:
`tool_call_finished` or `tool_call_failed`):

1. **By call id (1.0):** same `session_id`, same `agent.agent_id`, equal
   `tool.call_id`.
2. **FIFO (0.9):** otherwise the earliest unmatched S with the same
   `session_id`, `agent.agent_id`, and `tool.name` that precedes T in
   `source_seq` within the same turn.
3. **Unmatched S (0.7):** a start with no terminal by turn end is a tool call
   with `outcome.status = unknown` and `attrs.capture_gap = "no_terminal"`.
4. **Unmatched T (1.0):** providers with post-only hooks (Cursor
   `afterFileEdit`) yield single-event tool calls; `duration_ms` comes from
   the payload or is absent.

Implemented as the `pairing` column (`call_id`, `fifo`, `end_only`,
`in_flight`) and the confidences above. The unmatched start is not given
`outcome.status = unknown` / `capture_gap` yet: it is simply a call with no
`finished_at`.

Tool-call span id: `SpanId::derive([session_id, "call", tool.call_id])` when
a call id exists and is unused in the session, else
`SpanId::derive([session_id, "seq", ordinal])`. `parent_span_id` is the turn
span.

### 5.4 Attempts (confidence 0.4 / 0.6 / 0.9)

"Mutating" below means `tool.category.mutates_files()` (`file_write`,
`file_edit`, `notebook`); a `shell` command's effect is unknown.

What is implemented (`tier1-v5`):

- A turn's tool calls are split **by agent**: a main agent and the subagents
  it runs in parallel interleave their calls, and one agent's failure must not
  end another's attempt. Each agent's calls form attempts; `agent_id` is a
  column.
- An attempt **ends as `failed`** when a call fails (`outcome.status ∈
  {failure, denied}`) in a way that says the attempt did not work: a failing
  file edit, write or notebook call; a failing shell command after the
  attempt has edited something, or classified as a `test` or `build` run, or
  denied; or a test run that exits `0` but reports failing tests
  (`attrs.tests_failed > 0`). A shell command that fails during plain
  exploration (a `grep` finding nothing) is a failed *call*, not a failed
  attempt. The next call that started after the failing call ended opens a
  new attempt.
- An agent's last attempt takes its outcome from the turn: `succeeded` at
  `turn_stopped` (it means the turn stopped, nothing more: the
  `verification` column says whether a test or build ran and how it went),
  `failed` at `turn_failed`, `abandoned` when the turn was cut by the next
  prompt or the end of the session, `in_progress` while the turn is open.
- **Supersession:** a failed attempt is superseded by the first later attempt
  **of the same agent**, in the same turn or the next, that **edited** a path
  it edited (a path that was only read is not a retry) or, when it failed a
  test or build run, passed a run of the same kind. Emit `superseded(A → B)`
  and set `A.superseded_by = B`; the `caused(failure → B's first action)`
  edge is capped at confidence `0.6` (adjacency plus a shared edit is a
  guess).
- `paths` = union of `paths[].repo_relative` (else `logical`) over its tool
  calls (reads included, for display; only edited paths make a retry).
  `objective` in Tier 1 is the opening turn's prompt text when content was
  captured.
- **Identity.** `attempt_id` is derived from the attempt's own evidence, never
  from its position (`tier1-v4` and earlier used `session, turn index,
  position`, so a late-arriving earlier event renumbered attempts and a
  correction landed on the wrong one): `AttemptId::derive([session_id,
  anchor kind, anchor event id])`, where the anchor is the end of the call
  that failed the attempt, else its first tool-call event, else the turn's
  opening event. An id survives late earlier events, events appended to a
  running attempt, and a scoped view of the log. Positional ids still
  written in corrections and retractions are resolved by position under
  today's rules and flagged `legacy_position`.

*Planned, not implemented:* an attempt that continues across turns when the
next turn's first mutating call touches a path the open attempt touched;
closing an attempt as `abandoned` after 30 minutes of session inactivity (the
work unit is abandoned after two idle hours instead, §5.6); cross-session
supersession within 24 hours; `produced(attempt → artifact)` edges.

Edges emitted: `triggered(prompt → attempt)`, `caused(failed tool call end →
attempt it ended)`, `caused(failed call → next attempt)`, `superseded`.

### 5.5 Handoffs (confidence 0.6, up to 0.9)

The unit of detection is a **turn span** (a turn's first to last event). A
handoff is a *receiving* span whose most recent predecessor in the project is
a span of a different session and a different provider, when:

1. the project was quiet when the receiving span began (no other span covers
   that instant: two agents working at once are concurrent, not a handoff);
2. the receiving span begins within **30 minutes** after the giving span's
   last event (or the giving session's `session_ended`, whichever is later
   but still before it), and the receiving session touched a path the giving
   session had **edited** by then. A path both sides only read is not shared
   evidence;
3. or, with no shared path, the receiving span begins within **5 minutes**
   (confidence `0.5`).

Confidence with a shared edited path: `0.6` base; `+0.2` if the giving
session had a `session_ended` before the receiver began; `+0.1` if three or
more paths are shared; capped at `0.9`.

For a session's first span the receiving instant is the session's start.
Because the receiver is a span, a session that resumes after another agent
worked is a receiving session again: Claude, then a Codex review, then Claude
yields two handoffs (`from_turn` / `to_turn` name the turns). Detection sorts
each project's spans, so it is `O(spans log spans)`. Same-provider successors
are continuations, not handoffs; `continuation` edges are *planned*.

### 5.6 Work units (`tier1-v1`, implemented; confidence capped at 0.7)

A work unit is a **connected component of turns** within one project. Turns
are the nodes; two turns are linked when any of these hold:

1. **Shared edited path.** Both **edited** at least one common
   repository-relative path (a `file_write`, `file_edit` or `notebook` call;
   reads, searches, web calls and shell commands never link), and the later
   turn's first edit is within **six hours** of the earlier turn's last. A
   turn links to **one** earlier turn per path: the most recent of its own
   session, else the most recent of any session, so a turn on a file two
   concurrent units both edited does not fuse them. *Since `tier1-v1`:*
   turns of **different sessions whose active spans overlap** do not link on
   a shared path — two agents changing one file at the same time are two
   pieces of work (and a conflict, §5.8). *Since `tier1-v5`:* a path edited
   by at least five sessions of the project and at least a quarter of its
   sessions (a changelog, a lockfile) is a **hot path** and links nothing: an
   unbounded transitive closure over such files had fused twenty days of
   unrelated work into one unit.
2. **Adjacency.** They are consecutive turns of the same session and the
   later one starts within **10 minutes** of the earlier one's end.
3. **Handoff.** A `handed_off` edge (§5.5) links the turn that gave to the
   turn that received.

The component's fields are the versioned inference record (`version = 1`
until units are stored and superseded individually; the struct carries
`algorithm_version`, `evidence` and `confidence`):

- `work_unit_id = WorkUnitId::derive([project_id, first_event_id of the
  earliest member turn])`, so the id survives later turns joining the unit
  and only changes when the earliest turn changes.
- `objective`: prompt text of the earliest prompted turn when content was
  captured (`None` in `metadata_only`); `objective_event_id` always.
- `sessions`, `turns`, `attempts`, `paths` (first-touch order), `actors`
  (providers of the member sessions), `failure_count` (attempts whose
  possibly corrected outcome is `failed` or `superseded`), `last_attempt`,
  `started_at`, `updated_at` (latest member activity), `ended_at` (set only
  when the status is `completed` or `abandoned`).
- `evidence`: the union of member attempts' evidence, the evidence of
  handoffs between member sessions, and the blocking signal.
- `confidence`: the minimum over member attempts, **capped at 0.7** — the
  grouping is a heuristic and never claims more.

**Phase** is judged from the unit's last **5** tool calls, in chronological
order. A call is *decisive* when its category is `shell`, `file_write`,
`file_edit`, `notebook` or `plan`; reads, searches, web, MCP, subagent and
other calls are *neutral*. `phase_reason` states which rule fired.

| Rule (first match wins) | Phase |
|---|---|
| An uncleared *blocking* pending-input signal (`permission_requested`, or a `permission_prompt` / `agent_needs_input` notification, or an `idle_prompt` raised while a turn was running) in a member session that has not been silent for 12 hours | `blocked` (`blocking_signal` names it) |
| The last decisive call is a mutating/shell call that ended `failure` or `denied` | `debug` |
| The last decisive call is a shell command with `attrs.git_subcommand ∈ {commit, push}` | `deliver` |
| The last decisive call is a shell command with `attrs.command_category = "test"` and a file-mutating call precedes it in the unit | `verify` (with no prior edit: `explore`) |
| The last decisive call is a `plan` call | `plan` |
| The last decisive call is a file-mutating call followed by neutral calls | `review` |
| The last decisive call is a file-mutating call or other shell command and is the last call | `implement` |
| No decisive call in the window, but the unit edited something earlier | `review` |
| No decisive call in the window and no earlier edit (or no tool calls at all) | `explore` |

`command_category` and `git_subcommand` are the adapters' content-free
classification of the command line (RFC 0001); they are metadata, so
`verify` and `deliver` work in `metadata_only` mode.

**Status** is independent of phase and judged against a reference time —
the latest observed timestamp of the stream by default (so the projection
stays a pure function of the event set), or the time passed to
`Projector::finish_at` / `IncrementalProjector::snapshot_at` / `project_at` /
`Projection::work_units_at` (a live reader passes the wall clock, so a turn
that was interrupted days ago does not read "in progress" forever).
`status_reason` states which rule fired.

| Status | Rule |
|---|---|
| `completed` | The last turn completed (`turn_stopped`), its last attempt is `succeeded`, no tool call is in flight, and the session ended or the unit has been idle for **more than 30 minutes** |
| `abandoned` | The last attempt is `failed`, `superseded` or `abandoned` and the unit has been idle for **more than 2 hours** |
| `unknown` | Every member session has unknown coverage |
| `open` | Otherwise (including an in-flight tool call however long ago it started) |

There is **no numeric progress** anywhere: attempts, failed attempts,
sessions, turns and paths are reported as counts.

`Projection::work_units_at(t)` recomputes the units as they stood at `t`:
only turns, calls, attempts, handoffs and signals observed at or before `t`
take part, outcomes are masked to what was known then (an attempt that had
not ended is `in_progress`; a failed attempt whose retry had not started is
`failed`, not `superseded`; corrections written after `t` are ignored), and
idleness is judged against `t`. This is what `STATE … AT t` uses.

Merging and splitting beyond these three rules is a Tier 2 concern.

### 5.7 Decisions (`tier1-v1`, implemented; derived, confidence capped at 0.7)

Tier 1 derives decisions from the attempt structure. Nothing in them is
stated by a human: `rationale_source = "derived"` and every `rationale` is
assembled from failure classes, tool names/categories and
repository-relative paths.

- **`approach_change`** — one per superseded → superseding attempt pair
  (§5.4). `selected` is the retry, `alternatives = [the failed attempt]`,
  `decided_at` the retry's start, evidence the failing event and the retry's
  first action. Rationale shape: `abandoned approach after string_mismatch on
  src/x.rs; retried with a different edit (edit src/x.rs · shell)` (or `the
  same kind of change (…)` when the approach summaries match).
  `decision_id = DecisionId::derive(["approach_change", failed, retry])`.
- **`human_intervention`** — a permission denial (a `permission_denied`
  event, or a tool call that ended `denied`) followed in the same session by
  the next tool call using a **different** tool. `selected` is the attempt
  holding the retry; the attempt holding the denied call is the alternative
  when it is a different attempt. Rationale shape: `permission denied for
  Bash; continued with Edit (file_edit)`. Retrying with the same tool is not
  a decision.

Confidence is the minimum of the involved attempts' confidence, capped at
0.7. Decisions are listed in `(decided_at, decision_id)` order and carry
the `work_unit_id` of the selected attempt.

### 5.8 Work conflicts (`conflict-v0`, implemented; confidence 0.5–0.7)

The one Tier 1 inference that only exists where sessions of different
agents — on a server, different devices — meet in one projection: the
"work conflict" a team console raises before git sees a merge conflict.
A conflict is a pair of **open** work units of one project with **no
session in common** and at least one path both edited (file-mutating
calls), where the two units' edit windows on that path — first to last
edit — overlap or lie within **two hours** of each other. Per shared path
the record carries each side's `lines_added`/`lines_removed` and whether
that side committed (a `git commit` call in one of its sessions) since its
last edit. Confidence is **0.7** when the windows overlap and neither side
has committed, **0.5** otherwise; evidence is the edit events on each side
(up to three per side per path). Its `algorithm_version` is `conflict-v0`,
separate from the projection's, since adding it changed no other entity.

It cannot see edits outside the hook surface or commits the hooks did not
classify, and across devices the windows are compared on the devices' own
clocks (`observed_at`).

## 6. Tier 2: local semantic extraction (planned)

Runs only where `capture_mode` permits content locally (`local_semantic`,
`full_sync`). Uses local heuristics and, optionally, a local model to produce
objective titles, attempt approach summaries, decision records with
alternatives, test/verification detection from command lines and output,
blocker descriptions, and work-unit merges across sessions. Every output is
an Inference with `algorithm = "tier2-local"`, the model id, and a
`prompt_hash`. Tier 2 never overrides a Tier 1 structural inference; it
annotates it (a new version of the same subject with the Tier 1 claim
preserved in `claim.structure`).

## 7. Tier 3: optional cloud enrichment (planned)

Same contract as Tier 2 with `algorithm = "tier3-cloud"`, only ever on data the
capture mode allows to leave the device (RFC 0006), only when the user or
organisation enabled it, and recorded so the UI can show which claims came
from a remote model. Tier 3 outputs are the lowest-trust layer in the UI and
are shown with their evidence expanded by default.

## 8. Corrections and retractions (implemented)

Both are **canonical events** — they go through the WAL like any fact —
written by AttemptDB itself with `provider = "attemptdb"`. They carry their
own `EventKind` (`correction`, `retraction`; schema change shipped in
`attemptdb-core`) and land in the session of the entity they describe: the
CLI copies the target's `provider_session_id` and overrides the canonical
`session_id`, so `--session` scoping and `SHOW EVIDENCE FOR ses_…` include
them. The projector splits them off before grouping sessions (they never
count as session activity, never open turns, never extend idle time) and
applies them afterwards in stream order. The adapters' attribute allowlist
documents their keys: `correction_type`, `target`, `target_type`,
`outcome`, `reason`, `note_chars`.

### 8.1 Corrections

```text
provider_event_name  "Correction"
attrs.correction_type  attempt_outcome | attempt_note | turn_objective
attrs.target           att_… | trn_…  (prefixed canonical id; an attempt id is the evidence-derived one, §5.4)
attrs.outcome          succeeded | failed | abandoned | superseded   (attempt_outcome)
attrs.failure_class    content-free class                            (optional)
attrs.note_chars       length of the note
content.note           free text — content, dropped at ingest in metadata_only
```

Application rules (`Projection::corrections` records every correction with
a `status`: `applied`, `target_not_found`, `target_retracted`,
`content_unavailable`, `invalid`):

- `attempt_outcome` replaces the attempt's `outcome` and `failure_class`
  (a class given explicitly wins; otherwise the inferred class is kept only
  when the new outcome is still a failure). The projection's own values are
  preserved in `inferred_outcome` / `inferred_failure_class` (set once, by
  the first correction), `corrected` points at the correction event, and a
  note, when present, is attached.
- `attempt_note` attaches `note` and sets `corrected`; the outcome is
  untouched.
- `turn_objective` replaces the turn's `objective` and that of its
  attempts; the prompt text the projection derived is kept in
  `inferred_objective`. In `metadata_only` mode the text is not stored, so
  there is nothing to apply: the correction is recorded with status
  `content_unavailable` (not `applied`) and the turn is left untouched. An
  `attempt_note` with no stored note is `content_unavailable` the same way.
- A correction naming a positional attempt id from before `tier1-v5` is
  resolved by position under today's rules, applied, and flagged
  `legacy_position`.
- **Latest correction wins.** Corrections are applied in stream order; the
  `inferred_*` fields always hold the original projection, never an earlier
  correction.
- Corrections do not re-run structural rules: correcting a superseded
  attempt to `succeeded` leaves its `superseded_by` pointer; correcting an
  attempt to `failed` creates no supersession edge. Attempt `confidence` is
  about the boundaries and is not changed; the query layer surfaces
  `corrected_by` so a reader can see the value is human-stated.
- Work units, decisions and `why_blocked` read the corrected values.
  Time-travel (`STATE … AT t`, `work_units_at(t)`, the blocked state of a
  session at `t`) applies only the corrections written at or before `t`.

### 8.2 Retractions

```text
provider_event_name  "Retraction"
attrs.target_type    session | event | attempt
attrs.target         ses_… | ev_… | att_…
attrs.reason         benchmark | test | duplicate | mistaken_import | privacy | revoked | other
attrs.note_chars     length of the note
content.note         free text — content, dropped at ingest in metadata_only
```

A retracted fact stays in the log (facts are immutable) but leaves **every
projection**: sessions, turns, tool calls, attempts, handoffs, edges,
signals, work units, decisions, `state_at` and `why_blocked`.
`Projection::retractions` lists each retraction with whether it `matched`
anything loaded and how many fact events it removed;
`Projection::retracted_ids` (also `attemptdb_project::retracted_ids(events)`)
holds the retracted session ids, attempt ids and event ids so that the CLI
and the sanitized export can drop them (`RetractedSet::is_retracted(event)`
is true for a retracted event or any fact event of a retracted session;
correction and retraction events themselves are never retracted, so the
audit trail survives filtering). `stats.retracted_events` counts the
removed facts; they still count in `stats.events_seen`.

- **Session**: every fact event of the session is removed *before*
  projecting. Handoffs to or from it disappear; work units lose it.
- **Event**: the event is removed before projecting, so the remaining
  stream is projected as if it never happened. Consequences follow from the
  ordinary rules: retracting a failed edit's events merges the attempts it
  split; retracting a turn's prompt merges its tool calls into the previous
  turn and renumbers later turns; **retracting the only evidence of an
  attempt removes that attempt**.
- **Attempt**: removed *after* projecting, together with its tool calls
  (whose start/end events join `retracted_ids.events`). Re-splitting the turn
  would merge the siblings around the hole; instead sibling attempts keep
  their ids, a `superseded_by` pointer to the retracted attempt is cleared
  (the pointing attempt reverts to `failed`), session tool-call and failure
  counts are adjusted, and edges, handoffs and turn boundaries that cite the
  attempt's events are dropped or repointed. Shared evidence (the prompt, the
  stop) stays with the siblings. With reason `privacy` the prompt text of the
  attempt's turn is also hidden from the turn, its attempts, the work units
  built from it and the retracted rows, and the text of corrections aimed at
  it is dropped; a `privacy` retraction of a session hides the text of its
  retracted rows too. A snapshot exported without those events re-projects
  with the retry's id unchanged (ids come from evidence, §5.4); the attempt
  index may shift.
- The removed entities are kept on the side (`Projection::retracted`:
  sessions, turns, tool calls, attempts) so the query layer can show them
  on request (`INCLUDING RETRACTED`, RFC 0004). Handoffs, edges, signals,
  units and decisions have no retracted view.
- Retractions are honoured whatever their position in the stream (a
  retraction observed before its target still applies) and even when the
  target is not loaded (a filtered scan): the ids stay in `retracted_ids`
  and `matched = false` says nothing was found.
- A correction aimed at a retracted attempt is reported
  `target_retracted`; a correction or retraction can itself not be
  retracted (the CLI refuses). A wrong retraction is documented, not
  undone: retractions are facts too, and `attempt correct` cannot target one;
  the retraction event stays on record as the audit trail.
- A retraction naming a positional attempt id from before `tier1-v5` is
  resolved by position and flagged `legacy_position`.

The CLI (`attempt correct <att_|trn_> --outcome … [--failure-class …]
[--note …]`, `attempt retract --session|--attempt|--event ID --reason R
[--note …] [--yes]`) re-projects the stream with and without the new event
and prints the changed fields before writing (`--dry-run` previews only).
Corrections are retained as evaluation data (§10) with consent.

## 9. Replay and re-projection

*Planned; not implemented.* Today every read re-projects (incrementally, per
session) and the result is never stored. `attempt project --algorithm tier1 --version v1` (planned) recomputes every
Tier 1 inference from facts, writing new versions and superseding the old
ones with `superseded_at = now`. Because facts are immutable and the algorithm
is deterministic, replaying the same version twice is a no-op (detected by
`inputs_hash` + `algorithm_version`). Re-projection is incremental in the
common case: only subjects whose evidence set changed since the last run are
recomputed. Corrections survive re-projection per §8.

## 10. Evaluation, metrics, and quality gates

Gold data comes from opt-in design partners (200–500 labelled real sessions,
provider-balanced, including ambiguous and incomplete sessions, kept separate
from public demos). Metrics computed per algorithm version:

| Metric | Definition |
|---|---|
| WorkUnit boundary F1 | Precision/recall of inferred work-unit boundaries against labels |
| Objective/title acceptance | Fraction of Tier 2/3 titles a reviewer accepts unchanged |
| Phase accuracy | Exact-match accuracy of `phase` at labelled points in time |
| Needs You precision / recall | For `waiting_on_human` and `BLOCKED` notifications; precision is prioritised |
| Evidence faithfulness | Fraction of claims whose cited evidence actually supports them (reviewer-judged) |
| Human correction rate | Corrections per 100 inferences shown |
| Provider parity | Max pairwise difference of core metrics across Tier 1 providers |
| Time to projected state | `inferred_at − max(evidence.observed_at)` |
| Insufficient-evidence rate | Fraction of queries answered `insufficient_evidence` (tracked, not minimised blindly) |
| False causal claim rate | Fraction of `caused`/`blocked`/`resolved` edges judged wrong |

Required gates before an algorithm version is the default:

- ≥ 80% reviewer acceptance for WorkUnit/objective summaries (Tier 2/3).
- ≥ 95% precision for Needs You notifications.
- 100% evidence linkage for user-visible derived claims (enforced
  structurally: the query layer refuses to render an inference without
  evidence).
- < 10 percentage points of core-accuracy difference between Tier 1
  providers.
- Explicit uncertainty instead of unsupported precise state: no claim is
  rendered without its confidence and reason, and the answer
  `insufficient_evidence` must be reachable from every query.

## 11. The evidence rule

Every row returned by AttemptQL (RFC 0004) carries an `evidence` column with
the event ids behind it and an `uncertainty` column with the confidence and a
reason (`deterministic`, `heuristic`, `model`, `corrected`,
`insufficient_evidence`, `content_unavailable`). The UI renders nothing
derived without a way to open its evidence. When the evidence for a question
is missing — the capture mode hid it, a hook was not installed, a session was
only partially captured — the answer says so and names the gap
(`coverage_grade`, `capture_gap` markers) rather than guessing.

## Decisions

- Facts and inferences are stored and queried separately; inferences are
  versioned, superseded, and never edited.
- Every inference has non-empty evidence, a confidence from a fixed palette,
  an algorithm/model version, and (for model tiers) a prompt hash.
- Four times: observed, valid (`valid_from`/`valid_to`), inferred, superseded.
- Tier 1 (currently `tier1-v5`; see §5 for what each version changed) is deterministic, content-free, and replayable; its
  rules for sessions, turns, tool-call pairing, attempts, supersession,
  handoffs, work units, and decisions are those in §5. Work-unit and
  decision confidence is capped at 0.7.
- Attempt boundaries split, per agent, at failed edits and at failed shell
  commands that follow an edit or are test/build runs; supersession is by
  shared *edited* paths (or a passed check); attempt ids come from evidence;
  handoffs are cross-provider within 30 minutes with a shared edited path.
- Corrections (`EventKind::Correction`) and retractions
  (`EventKind::Retraction`) are first-class events written by AttemptDB
  itself, applied in stream order, and survive re-projection: the latest
  correction wins and the inferred value is kept alongside; a retracted
  session, event or attempt leaves every projection and the sanitized export
  but stays in the log.
- "Insufficient evidence" is a first-class answer and its rate is a tracked
  metric.
- Work units are connected components of turns (shared edited path within
  six hours and never a hot path, ten-minute adjacency, handoff); phase comes
  from the last five tool calls and status from the last attempt plus idle
  time (30 min / 2 h) judged against an explicit instant; the two are
  independent; no numeric progress exists.
- A session is `open`, `stale` or `closed` as of an explicit instant; `stale`
  is an inference from silence and is never presented as a fact.
- This RFC states plainly what is implemented and what is planned (see "What
  is implemented today"); a test fails when its version and the code's
  disagree.

## Open questions

- The 30-minute windows (session staleness, attempt abandonment, handoff,
  work-unit completion), the 10-minute turn adjacency and the 2-hour
  abandonment threshold are guesses; calibrate on the gold set.
- Whether an attempt retraction should re-split the turn (accepting id
  churn) instead of removing the attempt in place.
- Whether `shell` should always be treated as mutating, or whether a
  content-free heuristic on `tool.name` (`Bash` vs `Read`) plus
  `attrs.git_dirty` can do better.
- Whether `verify` / `deliver` from the adapters' command classification
  (`command_category`, `git_subcommand`) is reliable enough, or should lower
  the unit's confidence further.
- Storage of inferences: same segment format with a different column set
  under the same manifest, or a separate manifest family.
- How to expose `AS KNOWN AT` in AttemptQL without making the common query
  verbose.
