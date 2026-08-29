# RFC 0003: Facts, Inferences, and the Bitemporal Model

| | |
|---|---|
| **Status** | Draft |
| **Authors** | AttemptDB maintainers |
| **Created** | 2026-08-28 |
| **Related** | RFC 0001 (canonical event model), RFC 0002 (storage engine), RFC 0004 (AttemptQL), RFC 0006 (privacy and sync) |
| **Implementation** | `crates/attemptdb-project` (Tier 1, in progress; stub at the time of writing) |

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
| Mutability | Immutable once ingested | Immutable once written; superseded by a newer version |
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

## 5. Tier 1: deterministic projection (`tier1-v0`)

Tier 1 needs no content. It runs in `metadata_only` mode with full fidelity
and is the baseline every provider must reach with less than ten percentage
points of accuracy difference (§8). Every rule below is a function of
canonical fields only.

### 5.1 Sessions (confidence 1.0)

Group events by `session_id`. `started_at` = `observed_at` of the
`session_started` event, else the earliest event. `ended_at` = `observed_at`
of `session_ended` if present. A session with no end event is `open` if its
last event is within 30 minutes of now, else `stale` (closed by inactivity,
confidence 0.7, `valid_to` = last event time). Attributes: provider, project,
device, agents seen, event count, coverage grade (planned).

### 5.2 Turns (confidence 1.0; 0.9 when synthesised)

Within a session, order events by `source_seq`. If events carry
`provider_turn_id`, group by it. Otherwise a turn opens at each
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

Tool-call span id: `SpanId::derive([session_id, tool.call_id])` when a call id
exists, else `SpanId::derive([session_id, S.event_id])`. `parent_span_id` is
the turn span.

### 5.4 Attempts (confidence 0.7)

"Mutating" below means `tool.category.mutates_files()` (`file_write`,
`file_edit`, `notebook`) **or** `shell` — a shell command's effect is
unknown, so it is treated as potentially mutating.

- An attempt **opens** at the first mutating tool call of a turn. Read-only
  tool calls before it are attached as exploration evidence; read-only calls
  after it belong to the open attempt.
- An attempt **closes as `failed`** when a mutating tool call ends with
  `tool_call_failed` or `outcome.status ∈ {failure, denied}`. The next
  mutating call opens a new attempt.
- An attempt **closes as `succeeded`** at `turn_stopped` if it is open and its
  last mutating call succeeded.
- An attempt **closes as `abandoned`** at `turn_failed`, at `session_ended`
  while open, or after 30 minutes of session inactivity.
- An attempt may **continue across turns** in the same session when the
  next turn's first mutating call touches a path the open attempt already
  touched and no closing rule fired; otherwise the turn boundary closes it
  as `succeeded` (last mutating call succeeded) or `abandoned`.
- `paths` = union of `paths[].repo_relative` (else `logical`) over its tool
  calls. `agent_ids` = agents that made those calls. `objective` in Tier 1 is
  a **reference** to the opening turn's `prompt_submitted` event, never text.
- **Supersession:** attempt A is superseded by attempt B when both are in the
  same project, B opened after A closed, and `paths(A) ∩ paths(B) ≠ ∅`. Emit
  edge `superseded(A → B)` and set `A.superseded_by = B`. Only the most
  recent such B is recorded per A. Cross-session supersession is allowed
  within 24 hours; beyond that it is left to Tier 2.

Edges emitted: `triggered(prompt → attempt)`, `caused(failed tool call →
next attempt)`, `superseded`, `produced(attempt → artifact)` for each
mutated path (artifact id `ArtifactId::derive(["file", project_id, path])`).

### 5.5 Handoffs (confidence 0.6, up to 0.9)

Emit `handed_off(S1 → S2)` between sessions when all hold:

1. same `project_id`;
2. `provider(S1) ≠ provider(S2)`;
3. S2's first event is within **30 minutes** after S1's last event (or S1 is
   still open when S2 starts);
4. the paths touched by S1's attempts intersect the paths touched within
   S2's first turn.

Confidence: 0.6 base; +0.2 if S1 has a `session_ended` event; +0.1 if the
shared path count is at least 3; capped at 0.9. Same-provider successors are
`continuation` edges (`parent_of`-like, planned) rather than handoffs in v0.

### 5.6 Work units (confidence 0.5 for phase; structure 0.7)

`tier1-v0` produces a WorkUnit per connected component of attempts joined by
`superseded` and `handed_off` edges within a project. Fields:

- `objective`: reference to the earliest opening prompt event (no text).
- `phase`, derived from the most recent attempt's state:
  `EXPLORE` (only read-only calls so far), `PLAN` (a `plan`-category tool
  call is the latest mutating-or-plan call), `IMPLEMENT` (mutating calls
  succeeding), `DEBUG` (a failed attempt followed by a new attempt on the
  same paths), `VERIFY` (a `shell` call after mutating calls succeeded —
  weak; test detection needs content), `REVIEW` and `DELIVER` (not produced
  by Tier 1; require artifact/commit/PR facts, planned), `BLOCKED` (an
  unresolved `permission_requested`, a `permission_denied`, or a
  `turn_failed` as the latest terminal event).
- `status`, independent of phase: `active` (events within 30 minutes),
  `waiting_on_human` (`permission_requested` without a later tool call or
  denial), `done` (never set by Tier 1), `abandoned` (30+ minutes idle with a
  failed or abandoned last attempt), `unknown` otherwise.
- No numeric progress. Counts of attempts, failed attempts, and touched paths
  are reported as counts.

Merge and split of work units across sessions and providers beyond the edge
rules above is a Tier 2 concern.

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

## 8. Corrections

A correction is a **canonical event** — it goes through the WAL like any
fact — with `provider = "attemptdb"`, `provider_event_name = "Correction"`,
`attrs.correction_target = <inference_id>`, `attrs.correction_kind`
(`replace_claim`, `merge_subjects`, `split_subject`, `reject`, `confirm`),
and the replacement claim in `content` (content-gated: a correction that
supplies free text is content) or in `attrs` when it is enum-valued (a phase,
a status). Adding a dedicated `EventKind::Correction` is a planned schema
change; until then `kind = unknown` with `attrs.correction = true`.

The projection worker applies corrections in `source_seq` order after each
tier: the corrected inference is superseded by a new version with
`algorithm = "correction"`, `confidence = 1.0`, `evidence = [correction event]
∪ cited events`. Later algorithm runs must not silently override a
correction: a re-projection that would produce a claim conflicting with a
`confirm`/`replace_claim` correction writes its result with
`superseded_by = the correction version` immediately, so the correction stays
current and the disagreement is visible. Corrections are retained as
evaluation data (§9) with consent.

## 9. Replay and re-projection

`attempt project --algorithm tier1 --version v1` (planned) recomputes every
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
- Tier 1 (`tier1-v0`) is deterministic, content-free, and replayable; its
  rules for sessions, turns, tool-call pairing, attempts, supersession,
  handoffs, and work units are those in §5.
- Attempt boundaries split at failed mutating or shell tool calls;
  supersession is by shared paths; handoffs are cross-provider within 30
  minutes with shared paths.
- Corrections are first-class events applied in order and survive
  re-projection.
- "Insufficient evidence" is a first-class answer and its rate is a tracked
  metric.
- Work-unit phase and status are independent; no numeric progress exists.

## Open questions

- Whether corrections should get a dedicated `EventKind` in schema version 2
  (recommended) or remain `unknown` + `attrs.correction`.
- The 30-minute windows (session staleness, attempt abandonment, handoff)
  are guesses; calibrate on the gold set.
- Whether `shell` should always be treated as mutating, or whether a
  content-free heuristic on `tool.name` (`Bash` vs `Read`) plus
  `attrs.git_dirty` can do better.
- Whether Tier 1 should attempt `VERIFY` at all without content, or leave the
  phase at `IMPLEMENT` with lower confidence.
- Storage of inferences: same segment format with a different column set
  under the same manifest, or a separate manifest family.
- How to expose `AS KNOWN AT` in AttemptQL without making the common query
  verbose.
