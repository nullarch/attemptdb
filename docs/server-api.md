# AttemptDB server API

The HTTP contract of `attemptdb-server` — the hosted deployment of the
AttemptDB engine (RFC 0006 §10). Written for the team that calls it: the
device-side uploader, the product backend that reads a tenant's work graph,
and the operator who issues keys.

Everything is JSON over plain HTTP/1.1 (terminate TLS in front). Ids are
readable and prefixed (`ses_…`, `att_…`, `wu_…`, `dec_…`, `ev_…`, `prj_…`,
`dev_…`, `trn_…`, `spn_…`); timestamps are RFC 3339 UTC with microseconds.
Errors are `{"error": "<message>"}` with the status codes listed per route.

## Principals and scopes

| Credential | Header | May |
|---|---|---|
| **device key** (`atk_…`, scope `device`) | `Authorization: Bearer <key>` | upload that one device's events and inferences; read back its own inferences |
| **reader key** (scope `reader`) | `Authorization: Bearer <key>` | read the whole tenant: every device's data and the projections |
| **admin key** (scope `admin`) | `Authorization: Bearer <key>` | everything a reader may (tenant management is reserved) |
| **admin token** (`--admin-token` / `ATTEMPTDB_ADMIN_TOKEN`) | `Authorization: Bearer <token>` | `/v1/admin/*`: issue, list, revoke and reload keys; remove devices |

A key binds `(tenant, device_id, scope, user_id?)`. The server stores only
the SHA-256 digest of a key. Status codes every authenticated route shares:
`401` missing or unknown key; `403` a key of the wrong scope (the message
names the scope: `a reader key cannot upload…`, `a device key cannot read
the tenant…`); `503` the tenant's storage failed — retry later.

One tenant is one `.attemptdb` database under `data/tenants/<tenant>/`;
nothing a request does can reach another tenant's directory.

## Facts and inferences

Events are **facts**: what a hook observed, stored as uploaded (minus what
the server's capture-mode ceiling removes). Attempts, handoffs, work units,
decisions, "blocked" explanations and session states are **inferences**
(RFC 0003): every one carries `evidence` (event ids), `confidence` in
`[0, 1]`, `algorithm_version`, and — on this server — `computed_by`,
which is `"server"` or `"device"` (see [the merge rule](#inference-merge-rule)).
Never present an inference as a fact; the evidence ids resolve through
`/v1/events` and `/v1/query`.

The projection served here is the one the local `attempt ui` builds for the
same events (`attemptdb-project`, `attemptdb-query`): same algorithm, same
ids, same counts. `algorithm_version` in every response tells which build
produced them; a change of version means every inference should be
re-fetched, not migrated.

---

## Write side

### `POST /v1/sync` — upload events (device key)

RFC 0006 §10.3. The body is one batch of RFC 0001 canonical envelopes, the
JSON `attempt hook` spools; at most 5,000 events per batch, one batch in
flight per device, in `source_seq` order.

```json
{ "sync_version": 1, "device_id": "<uuid>", "batch_id": "<client-chosen>",
  "capture_mode": "local_semantic", "events": [ <RFC 0001 envelope>, … ] }
```

```json
200 { "sync_version": 1, "batch_id": "…", "accepted": 3, "duplicates": 0,
      "rejected": [ { "event_id": "…", "reason": "…" } ],
      "redactions": 0, "stripped_content": 3 }
```

- `accepted` stored for the first time; `duplicates` already stored
  (`event_id` is minted by the client, so a re-sent batch is safe);
  `rejected` not stored, do not retry (`event device_id does not match the
  batch`); `redactions` attrs dropped by the engine's content contract;
  `stripped_content` events whose `content`/`raw` the server's ceiling
  removed.
- `400` unsupported `sync_version` · `403` batch `device_id` ≠ key's device
  · `413` body over the limit (default 4 MiB) or over 5,000 events — split
  the batch · `503` storage failed — keep the batch, retry.
- The client's own `source_seq` survives as `attrs.device_seq`; the server
  assigns its database's `source_seq` and `hlc` at ingest.

### `POST /v1/sync/inferences` — upload device inferences (device key)

RFC 0006 §10.7, `spec/inference-v1.schema.json`. One document per `kind`
(`attempt`, `handoff`, `work_unit`, `decision`), at most 20,000 items,
replaced wholesale on every upload.

```json
{ "sync_version": 1, "schema": "attemptdb.inference/v1", "device_id": "<uuid>",
  "batch_id": "…", "kind": "attempt", "algorithm_version": "tier1-v0",
  "computed_at": 1756368000123456,
  "items": [ { "kind": "attempt", "id": "<uuid>", "session_id": "<uuid>",
               "evidence": ["<event uuid>", …], "confidence": 0.9,
               "algorithm_version": "tier1-v0", "fields": { …projection row… } } ] }
```

```json
200 { "sync_version": 1, "batch_id": "…", "kind": "attempt",
      "stored": 12, "rejected": [ { "id": "…", "reason": "missing evidence" } ], "stripped": 3 }
```

Every item must carry provenance (non-empty `evidence`, `confidence` in
`[0, 1]`, `algorithm_version`, an object `fields`); items without it are
rejected by id. Under a `metadata_only` ceiling the content-bearing fields
`objective` and `rationale` are nulled (`stripped` counts them). Inferences
are stored beside the event database
(`<tenant>/inferences/<device_id>/<kind>.json`), never ingested as events.
`400` wrong `sync_version`/`schema`/`kind`, empty `algorithm_version` ·
`403` device mismatch · `413` over 20,000 items.

### `GET /v1/inferences[?kind=<kind>]` — a device's own uploads (any key)

Device-scoped: answers for the key's own `device_id`, whatever the scope.
Without `kind`, a summary: `{ "device_id", "kinds": [ { "kind", "items",
"algorithm_version", "computed_at", "received_at" } ] }`. With `kind`, the
stored document. `404` no document of that kind for this device · `400`
unknown kind.

### `POST /v1/vibemon/hook` — legacy VibeMon envelope (device key)

One envelope v2 per request, exactly what `~/.vibemon/notify.sh` sends,
normalised through `attemptdb_adapters::vibemon` and ingested like
`/v1/sync`. The device is the key's; an event id is minted per request
(the legacy client never retries). `200 { "accepted", "duplicates",
"redactions" }` · `400` envelope does not parse.

---

## Admin surface (admin token)

Absent when no token is configured: every route below answers `404`.
`401` on a wrong or missing token.

### `POST /v1/admin/keys` — issue a key

```json
{ "tenant": "acme", "scope": "reader", "user_id": "usr_42", "label": "web backend",
  "device_id": "<uuid, optional>" }
```

```json
201 { "key": "atk_…", "sha256": "…", "tenant": "acme", "device_id": "<uuid>",
      "label": "…", "scope": "reader", "user_id": "usr_42",
      "note": "store the key now; the server keeps only its digest" }
```

`scope` is `device` (default), `reader` or `admin`; `user_id` is an opaque
single token (≤128 chars) echoed in listings; the server mints `device_id`
when absent. `400` invalid tenant, scope or user id.

### `GET /v1/admin/keys` — list keys

`{ "keys": [ { "sha256", "tenant", "device_id", "label", "scope", "user_id" } ] }`
— digests and bindings, never keys.

### `DELETE /v1/admin/keys/{sha256}` — revoke

`200 { "revoked": "<sha256>" }` · `404` no key with that digest. The next
request with the key gets `401`.

### `POST /v1/admin/keys/reload` — re-read the key file

`200 { "keys": <count> }`. SIGHUP does the same.

### `DELETE /v1/admin/devices/{device_id}[?tenant=<id>]` — a device leaves

Revokes every device key bound to the device, then writes one Retraction
(RFC 0003 §8, reason `revoked`) per session the device wrote, in every
tenant where it held a key (or only in `?tenant=`, which also works after
its keys are gone). Facts stay on disk; the sessions leave every
projection and every read route.

```json
200 { "device_id": "<uuid>", "keys_revoked": 1,
      "tenants": [ { "tenant": "acme", "keys_revoked": 1, "sessions_retracted": 2,
                     "sessions_already_retracted": 0, "events_affected": 57 } ],
      "note": "facts stay on disk; the retracted sessions leave every projection" }
```

`404` no device key bound to that device and no `?tenant=` given. A repeat
call is safe (`sessions_already_retracted`).

---

## Read side (reader or admin key)

Every read response carries, before its data:

```json
{ "tenant": "acme", "algorithm_version": "tier1-v0",
  "generated_at": "2026-08-30T10:00:00.000000Z", … }
```

Reads are served from a per-tenant engine cache: the first read after an
upload decodes only the newly flushed segments and re-projects only the
sessions the new events touched, then every read until the next upload is
served from the same view. `GET /v1/status` shows the cache's counters.

### Common parameters

| Parameter | Routes | Meaning |
|---|---|---|
| `project` | sessions, timeline, work, attention, state, events | one project: a `prj_` id (or bare uuid), a normalised remote (`github.com/acme/repo`, or any spelling such as `git@github.com:acme/repo.git`), a project name (`acme/repo`), or a logical root. `400` with the known projects when unknown. |
| `since`, `until` | sessions, timeline, work, attention, events | RFC 3339, `YYYY-MM-DD`, epoch seconds/millis, `now`, `today`, `yesterday`, or relative `-15m`, `-2h`, `-1d`, `-1w`. Inclusive. |
| `limit` | all lists | default 200 (attention: 20), max 2000; `400` unless a positive integer. |
| `cursor` | sessions, timeline, work | opaque; pass back the previous page's `next_cursor`. `400` when not one the server issued. |

`project` **filters** the tenant's projection — it does not re-project the
subset — so ids, counts and confidences are identical to the unfiltered
view. `since`/`until` keep the entities whose activity window (a session's
`started_at..last_event_at`, a work unit's `started_at..updated_at`, an
event's `observed_at`) overlaps the range. Lists are newest first; a page
is a keyset after the cursor, so new uploads do not shift the pages a
consumer has already read.

### `GET /v1/status`

The tenant in numbers, and the cache behind them.

```json
{ "capture_mode": "metadata_only", "events": 1204, "sessions": 31, "open_sessions": 2,
  "turns": 88, "tool_calls": 640, "attempts": 121, "handoffs": 3, "work_units": 19,
  "decisions": 7, "retracted_sessions": 0, "last_event_at": "…",
  "projects": [ { "project_id": "prj_…", "name": "acme/repo", "repo_remote": "github.com/acme/repo", "events": 1204, "sessions": 31 } ],
  "providers": [ { "provider": "claude_code", "events": 900, "last_event_at": "…" } ],
  "storage": { "generation": 4, "segments": 3, "segment_rows": 1100, "memtable_rows": 104, "wal_bytes": 22000, "last_source_seq": 1204 },
  "cache": { "view_built_at": "…", "decodes": 3, "refreshes": 5, "segments": 3, "projected_events": 1204, "sessions_reprojected": 1 },
  "device_inferences": { "documents": 2, "items": 140 },
  "projection_stats": { "events_seen": 1204, "out_of_order_events": 0, "unpaired_tool_starts": 1, "unpaired_tool_finishes": 0, "fifo_pairings": 0, "unknown_events": 0, "retracted_events": 0 } }
```

### `GET /v1/live[?window=<seconds>]`

"Is this user coding right now": the newest event and the sessions with
an event in the last `window` seconds (default 600), newest first. Facts
only — no projection, and after the first call for a tenant since the
server started, no engine: the answer comes from a few hundred bytes the
ingest path keeps up to date per tenant, so a client may poll this every
few seconds whatever the tenant's size. `idle_ms` is measured against the
server's clock.

```json
{ "events": 1204, "server_time": "…",
  "last_event": { "at": "…", "kind": "tool_call_finished", "provider": "claude_code",
                  "session_id": "ses_…", "project_id": "prj_…", "tool": "Bash", "idle_ms": 4210 },
  "window_secs": 600,
  "active_sessions": [ { "session_id": "ses_…", "provider": "claude_code", "project_id": "prj_…",
                         "last_event_at": "…", "last_kind": "prompt_submitted", "idle_ms": 4210 } ],
  "note": "facts only: the newest event and recently active sessions; no inference" }
```

Capture tests do not count as activity. `400` when `window` is not a
positive integer. This is the endpoint for a coding-state loop; the
session list with states and counts is `GET /v1/sessions`.

### `GET /v1/sessions`

The projection's sessions, newest first.

```json
{ "scope": { "project_id": null, "since": null, "until": null },
  "total": 31, "open": 2, "next_cursor": "…" | null,
  "sessions": [ {
    "session_id": "ses_…", "provider": "claude_code", "provider_name": "Claude Code",
    "provider_session_id": "…", "project_id": "prj_…", "project_name": "acme/repo",
    "device_id": "dev_…", "state": "open" | "closed",
    "started_at": "…", "ended_at": "…" | null, "last_event_at": "…",
    "end_reason": "…" | null, "start_source": "startup" | null,
    "event_count": 40, "turn_count": 3, "prompt_count": 3, "tool_call_count": 17,
    "attempt_count": 4, "failure_count": 1, "agents": ["agt_…"],
    "coverage": "full" | "partial" | "minimal" | "unknown",
    "captured_events": 40, "reconstructed_events": 0,
    "first_event_id": "ev_…", "last_event_id": "ev_…",
    "start_event_id": "ev_…" | null, "end_event_id": "ev_…" | null } ] }
```

Sessions are facts grouped by `session_id`; `coverage` says how much of
the lifecycle the stream shows.

### `GET /v1/timeline[?tools=1]`

Sessions → turns → attempts, the shape of the local UI's `/api/timeline`,
plus the tenant's handoffs, work units and decisions (each capped at
`limit`, newest first, with a `_total`). `tools=1` inlines each attempt's
tool calls.

```json
{ "scope": {…}, "events": 1204, "total_sessions": 31, "total_attempts": 121,
  "next_cursor": null,
  "sessions": [ { …session fields…, "turns": [ {
      "turn_id": "trn_…", "session_id": "ses_…", "index": 1,
      "status": "completed" | "failed" | "in_progress" | "unknown",
      "started_at": "…", "ended_at": "…" | null, "objective": null,
      "prompt_chars": 27, "prompt_event_id": "ev_…", "stop_event_id": "ev_…",
      "tool_call_ids": ["spn_…"],
      "attempts": [ <attempt> ] } ] } ],
  "handoffs": [ <handoff> ], "handoffs_total": 3,
  "work_units": [ <work unit> ], "work_units_total": 19,
  "decisions": [ <decision> ], "decisions_total": 7,
  "note": "attempts, blockers and handoffs are inferences with evidence; events are facts" }
```

A server-computed attempt:

```json
{ "kind": "attempt", "computed_by": "server", "attempt_id": "att_…",
  "session_id": "ses_…", "turn_id": "trn_…", "turn_index": 1, "index": 0,
  "objective": null, "approach": "edit src/parser.rs · shell ×1",
  "started_at": "…", "ended_at": "…" | null, "duration_ms": 11000,
  "outcome": "succeeded" | "failed" | "abandoned" | "superseded" | "in_progress" | "unknown",
  "failure_class": "string_mismatch" | null, "paths": ["src/parser.rs"],
  "tool_call_ids": ["spn_…"], "superseded_by": "att_…" | null, "supersedes": "att_…" | null,
  "evidence": ["ev_…"], "confidence": 0.9, "algorithm_version": "tier1-v0",
  "work_unit_id": "wu_…" | null, "corrected": null, "inferred_outcome": null,
  "inferred_failure_class": null, "note": null,
  "tool_calls": [ … ]   // with ?tools=1
}
```

Handoff: `{ "kind": "handoff", "computed_by", "handoff_id": "ses_a:ses_b",
"at", "from_session", "to_session", "from_provider", "to_provider",
"project_id", "gap_ms", "shared_paths", "evidence", "confidence",
"algorithm_version" }`. Decision: `{ "kind": "decision", "computed_by",
"decision_id": "dec_…", "work_unit_id", "session_id", "turn_id",
"decision_kind": "approach_change" | "human_intervention", "selected":
"att_…", "alternatives": ["att_…"], "rationale", "rationale_source":
"derived", "decided_at", "evidence", "confidence", "algorithm_version" }`.

### `GET /v1/work[?status=open|completed|abandoned|unknown][&phase=…]`

Work units with their member attempts and blocker, newest activity first.

```json
{ "scope": {…}, "total": 19, "next_cursor": null,
  "work_units": [ {
    "kind": "work_unit", "computed_by": "server", "work_unit_id": "wu_…",
    "project_id": "prj_…", "project_name": "acme/repo",
    "objective": null, "objective_event_id": "ev_…",
    "phase": "explore" | "plan" | "implement" | "debug" | "verify" | "review" | "deliver" | "blocked",
    "phase_reason": "…", "status": "open" | "completed" | "abandoned" | "unknown", "status_reason": "…",
    "started_at": "…", "updated_at": "…", "ended_at": "…" | null,
    "sessions": ["ses_…"], "turns": ["trn_…"], "attempts": ["att_…"],
    "paths": ["src/parser.rs"], "actors": ["claude_code", "codex"],
    "failure_count": 1, "last_attempt": "att_…", "blocking_signal": "ev_…" | null,
    "evidence": ["ev_…"], "confidence": 0.7, "algorithm_version": "tier1-v0", "version": 1,
    "member_attempts": [ <attempt> ],
    "blocked": { "computed_by": "server", "claim": "…", "evidence": ["ev_…"], "confidence": 0.85,
                 "uncertainty": "…", "algorithm_version": "tier1-v0" } | null } ],
  "note": "…" }
```

`attempts` is the unit's member ids; `member_attempts` the same attempts
as inference objects, each with its own `computed_by`; `blocked` is the
server's `why_blocked` answer for the unit (an uncleared pending-input
signal, or two consecutive failures of the same class).

### `GET /v1/attention`

"Needs You": every **open** session that looks blocked, highest confidence
first (then most recent). Default `limit` 20.

```json
{ "scope": {…}, "open_sessions": 2, "total": 1,
  "items": [ {
    "computed_by": "server", "session_id": "ses_…", "project_id": "prj_…",
    "reason": "pending_input" | "repeated_failure",
    "signal_type": "permission_request" | "permission_prompt" | "idle_prompt" | "agent_needs_input" | null,
    "failure_class": "string_mismatch" | null,
    "since": "…",
    "claim": "Session ses_… is waiting on a permission request raised at … with no later event observed.",
    "evidence": ["ev_…"], "confidence": 0.85,
    "uncertainty": "Coverage is full (…) A response given outside the hook surface would not be captured, so the wait may already be over.",
    "algorithm_version": "tier1-v0",
    "session": { …session fields… } } ],
  "note": "…" }
```

`since` is when the signal was raised (`pending_input`) or when the last
failed attempt ended (`repeated_failure`). Read `uncertainty`: a blocked
session is an inference over hook events, and a response given outside
the hook surface is invisible.

### `GET /v1/state[?at=<time>]`

Every session active at `at` (default now), as it stood then — turn,
in-flight tool calls, last attempt with its outcome as known at that time,
and whether it looked blocked. `400` when `at` does not parse.

```json
{ "scope": {…}, "at": "…", "total": 1, "blocked": 1,
  "sessions": [ {
    "computed_by": "server", "session_id": "ses_…", "provider": "claude_code", "project_id": "prj_…",
    "open": true, "coverage": "full", "current_turn": "trn_…", "turn_index": 1,
    "turn_status": "in_progress", "in_flight_tool_calls": ["spn_…"],
    "last_attempt": "att_…", "last_attempt_outcome": "failed", "last_failure_class": "string_mismatch",
    "last_activity_at": "…", "blocked": false, "block": null, "evidence": ["ev_…"],
    "algorithm_version": "tier1-v0" } ] }
```

### `GET /v1/events?after=<source_seq>&limit=<n>`

The tenant's events in `source_seq` order, strictly after `after` (default
0), as stored — RFC 0001 envelopes, already clamped to the server's
capture-mode ceiling. A consumer streams by passing `next` back as `after`;
an empty page returns `next == after` and `has_more: false`. `project`,
`since` and `until` filter; `next` still advances past filtered-out
events. `400` when `after` is not an integer.

```json
{ "scope": {…}, "after": 0, "count": 200, "next": 200, "has_more": true, "last_source_seq": 1204,
  "events": [ { "event_id": "<uuid>", "source_seq": 1, "hlc": …, "observed_at": …, "kind": "tool_call_finished",
                "provider": "claude_code", "session_id": "<uuid>", "project": { … }, "attrs": { … }, … } ] }
```

Ids and timestamps in this route are the envelope's own (bare uuids,
microseconds): this is the fact as it was recorded.

### `GET /v1/events/{id}`

One stored event by id (`ev_…` or a bare uuid), as stored — metadata,
never content. The segment whose id range covers the id is the only one
decoded. `400` malformed id · `404` not in this tenant.

```json
{ "event": { "event_id": "…", "kind": "tool_call_failed", "attrs": { … }, "content": null, … },
  "note": "as stored on the server: metadata only; the observing device holds the content" }
```

### `POST /v1/corrections` — a correction or retraction from the console (reader or admin key)

What `attempt correct` / `attempt retract` write on a device, written
here under the server's own writer identity and attributed to the key's
`user_id` (`attrs.x_attemptdb_corrected_by`). The observed facts are
unchanged; the projection re-reads them through the corrections table.

```json
{ "target": "att_…", "type": "attempt_outcome", "outcome": "succeeded", "failure_class": null, "note": "flaky CI" }
{ "target": "att_…", "type": "attempt_note", "note": "…" }
{ "target": "trn_…", "type": "turn_objective", "note": "the new objective" }
{ "target": "ses_…", "type": "retract_session", "reason": "benchmark", "note": "…" }
```

```json
200 { "event_id": "ev_…", "kind": "correction" | "retraction", "session_id": "ses_…", "accepted": 1 }
```

`outcome` is one of `succeeded`, `failed`, `abandoned`, `superseded`;
`reason` is the retraction vocabulary (`benchmark`, `test`, `duplicate`,
`mistaken_import`, `privacy`, `revoked`, `other`; anything else folds to
`other`). Under the `metadata_only` ceiling the note's text is dropped and
only `note_chars` survives. `400` bad target/type/outcome · `403` device
key · `404` unknown target.

### Countable signals

A work unit (`/v1/work`) carries `signal`: the newest test run and build
its sessions reported since the unit started, or `null` when nothing was
counted — a console shows a phase badge then, never a made-up percentage.

```json
"signal": { "tests": { "passed": 18, "failed": 2, "skipped": 0, "total": 20, "at": "…" },
            "build": { "ok": true, "at": "…" } }
```

The counts are the adapters' `tests_passed` / `tests_failed` /
`tests_skipped` attrs, read from a runner's summary line on the device
(cargo, nextest, jest, vitest, pytest, mocha, rspec, phpunit, dotnet, go
test -v); the line itself never leaves. `build` is a `build`-category
command's exit code.

### People

Sessions (`/v1/sessions`, `/v1/timeline`, `/v1/attention`, `/v1/live`)
carry `device_id` and `user_id`; work units (`/v1/work`) carry `devices`
and `users`. The mapping is the tenant's device keys' `user_id`, read per
request, so a key issued or relabelled a moment ago shows at once.

### `POST /v1/query`

```json
{ "statement": "SHOW FAILED ATTEMPTS SINCE -1d", "limit": 200 }
```

AttemptQL (RFC 0004) or SQL over the tenant's tables (`events`,
`events_raw`, `sessions`, `turns`, `tool_calls`, `attempts`, `handoffs`,
`edges`, `signals`, `work_units`, `decisions`, `corrections`,
`retractions`). Read-only at the engine layer: DDL, DML, `SET`, `COPY`
and transactions are refused by DataFusion itself (`400`). Rows are capped
at `limit` (query string or body; default 200, max 2000); the whole
statement has a 5 s budget (`408`). Parse errors come back as `400` with a
caret rendering of the position; other engine errors as `400` with the
engine's message. `project` is not applied here — filter on `project_id`
in the statement.

```json
{ "statement": "…", "kind": "rows" | "explanation" | "empty",
  "columns": ["attempt_id", …], "row_count": 3, "truncated": false,
  "rows": [ { "attempt_id": "att_…", … } ], "notes": [] }
```

`WHY`, `TRACE` and `STATE` results carry an `evidence` column plus a
confidence and an uncertainty note, never prose alone.

---

## Inference merge rule

A tenant may hold device-uploaded inference documents next to the events
(`POST /v1/sync/inferences`). Wherever the read side returns an attempt,
handoff, work unit or decision (`/v1/timeline`, `/v1/work`), it compares
the device's item for the same `(kind, id)` with the one it computed:

1. **The device item is returned** when its `algorithm_version` is the
   same as or newer than the server's `algorithm_version`.
2. **The server item is returned** otherwise — including whenever the
   device's version cannot be compared.

"Newer" is defined on versions of the form `<family>-v<n>` (`tier1-v0`,
`tier1-v3`): two versions compare only within the same family, by `n`.
`tier1-v1 ≥ tier1-v0` wins; `tier2-v0`, `v2`, or an empty version never
win against `tier1-v0`. When two devices hold the same `(kind, id)`, the
higher version is kept, then the later `received_at`.

The returned object is one computation, whole:

- `computed_by: "server"` — the shapes above.
- `computed_by: "device"` — the row the device uploaded (its `fields`),
  with the device's `evidence`, `confidence`, `algorithm_version`, plus
  `device_id`, `computed_at` and `received_at`. Only the *encoding* of
  known id and timestamp fields is normalised to the server's (prefixed
  ids, RFC 3339); no value is recomputed, no server field (`duration_ms`,
  `superseded_by`, …) is added, and a field the server does not know
  (from a newer algorithm) passes through untouched. Content fields the
  ceiling removed at upload are `null`.

Fields of the two are never mixed. Per-unit `member_attempts` and
`blocked` are separate objects with their own `computed_by`.

## Health

`GET /v1/health` (no key): `{ "status": "ok", "sync_version": 1,
"capture_mode": "metadata_only", "open_tenants": 3 }`.

## `GET /v1/devices` — devices and their last sync

Scope: `reader` or `admin`. Facts only, no projection: every device that
holds a key in this tenant or has uploaded events to it, newest upload
first.

```json
{ "tenant": "org_acme", "algorithm_version": "tier1-v0", "generated_at": "…",
  "count": 2,
  "devices": [
    { "device_id": "dev_…",
      "keys": [ { "sha256": "…", "label": "kevin laptop", "scope": "device", "user_id": "usr_42" } ],
      "connected": true,
      "events": 4473, "sessions": 12, "retracted_sessions": 0,
      "providers": ["claude_code", "codex"],
      "first_observed_at": "…", "last_observed_at": "…",
      "last_sync_at": "…" } ] }
```

`connected` is "a device key still exists" (false after
`DELETE /v1/admin/devices/{id}`); `last_sync_at` is the server receipt time
(`ingested_at`) of the device's newest event — what a "Connected · last
sync 3 s ago" row shows; `last_observed_at` is the newest event time on the
device's own clock. The server's own retraction events are not a device.
