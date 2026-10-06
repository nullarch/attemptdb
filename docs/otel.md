# Local coding-agent telemetry

From 0.2.10, `attempt hook install` configures local OTLP collection for
detected Claude Code and Codex installations. No hosted account is required.

```sh
attempt hook install
# Restart Claude Code / Codex, then work normally.
attempt doctor
attempt doctor --json
```

Installation starts the runtime and checks receiver readiness. This proves
availability, **not an agent export**. Doctor lists persisted `claude_code:logs`,
`codex:logs`, metrics and traces separately. Current-process counters reset on
daemon restart; durable records do not. Metrics may take 60 seconds.

## Observations

| Source | Useful information | Limits |
|---|---|---|
| Hooks | Session, prompt, tool, permission, stop lifecycle | Provider hook coverage varies |
| Claude OTel | API model, input/output/cache tokens, reported cost, latency, metrics, enhanced traces | Traces need a supporting version; cost is an estimate |
| Codex OTel | API observations, latency, metrics, traces | Fields vary by event/version; absent cost is not zero. `codex.sse_event` stream chunks are discarded (below), so Codex completion tokens are not recorded |

Logs, data points and spans are immutable events with `kind='unknown'`,
`adapter_version='otel-json-v1'`, `attrs.source='otel'`, and `x_otel_signal`.
They do not create tasks or mark agents alive. `session.id` (Claude) and
`conversation.id` (Codex) identify the hook session. Missing identity stays
unattributed: inspect `x_otel_session_attributed` and
`x_otel_project_attributed`. Later hooks do not rewrite earlier facts.

A record's project is the project of its session's latest hook event (lifecycle
and tool events; telemetry rows never name one). The daemon's writer keeps that
as an in-memory session-to-project map: seeded once, on the first record, from
the project columns of every segment and the WAL (no content, raw or attrs
column is decoded and no encryption key is asked for; 0.24 s in all for the
first batch on a database of 1.5 million events in 75 segments), then fed with every hook event the writer
stores, whether it arrived over the socket or from the spool. A session the map
does not know has no hook event in the database, which is a final answer until
one arrives: its records are stored unattributed and it is never looked up
again. (Before, each unknown session was looked up by decoding every segment,
1.4 s for one session on a database of 4 million events, again every five
seconds, with the single writer blocked and every hook acknowledgement behind
it.)

## What is not kept (`otel-retention-v3`)

The intake keeps what a person or a query can use and drops what only
measures the agent's own plumbing or repeats a fact stored elsewhere. In the
first live database 3.71 million of 3.95 million events (94%) were telemetry
nothing read. One rule, `attemptdb_adapters::otel::retained`, is applied by
the local receiver and by the sync server's ingest (and by
`purge-telemetry`), so a client that predates a version is still held to it.
The receiver counts every dropped record as `dropped` in the OTLP receipt and
acknowledges the request as received; the sync server rejects them from older
clients. A request whose records are all dropped never wakes the writer.

| Dropped | Why it is safe |
|---|---|
| A **bare span** the exporter did not attribute to a session (Codex `receiving`, `handle_responses`, `append_items`, `persist_rollout_items`, …) | The agent process's own execution trace, tens of thousands an hour, none carrying a conversation id, none read by any projection or console. Spans that carry a session, and the structured span *events* Codex nests in those spans (`codex.tool_result`, `codex.api_request`), stay. |
| `codex.sse_event` and `codex.sse_event.*` | One record per streamed chunk, about nine in ten of them `custom_tool_call_input.delta` fragments; the largest single source of growth. Sessions, attempts, signals and work units are not derived from them; the completion (`codex.api_request`) and hooks carry lifecycle, tokens and latency. The same discard applies as log record, metric and span event; the rest of a span and its other span events are kept. |
| `codex.sqlite.*` (`logs.write.max_entry_bytes`, `.count`, `.duration_ms`, `.bytes`, `.entries`) | Five metric series about Codex's own log database, with no session: 300k samples in the first database. Storage plumbing of the exporter, not agent work. |
| `hook_execution_start`, `hook_execution_complete` (also as `claude_code.hook_execution_*`) | Claude's report that it ran hooks (121k each). Compared with the hook events captured here: the occurrence, session and time are the hook event's own, with the tool, call id and outcome besides. What the exporter adds (hook name, hook counts, blocking / error / cancelled counts, total duration) is not promoted into metadata, so nothing reads it. Use `attempt doctor` to see whether hooks are firing. |

Kept, because they carry what people read: `api_request` /
`claude_code.api_request` and `claude_code.llm_request` (model, tokens, cost,
latency), `claude_code.token.usage` and `claude_code.cost.usage`,
`codex.api_request`, `codex.turn.token_usage`, `codex.tool_result` and
`tool_result`, `tool_decision`, `claude_code.tool` and
`claude_code.tool.blocked_on_user` (how long a person took to approve), user
prompts and assistant replies (under the capture mode). Nothing in the
projection reads these records today; `attempt doctor` counts what is stored
per signal, and SQL reads the typed `x_otel_*` fields. If a family above is
wanted back, the list is `DISCARDED` in `crates/attemptdb-adapters/src/otel.rs`;
bump `RETENTION_VERSION` with any change. Rows stored before a version are not
removed by the receiver: the sync client never uploads a discarded family it
finds in an older database, and `POST /v1/admin/tenants/{tenant}/purge-telemetry`
rewrites a server tenant without them.

Metadata retains emitted model, numeric usage/cost/duration/status fields,
request ids and trace/span/parent ids. Structured span events retain their
explicit conversation context; x_otel_record_type distinguishes them from
spans and log records. Codex zero timestamps use its separate event or observed
time. OS thread.id is not a conversation identity. Metrics preserve native value,
temporality, monotonicity, start time and supported histogram counts/bounds.
Unknown attributes are omitted from metadata. Do not sum cumulative snapshots
or add logs, metrics and traces representing the same usage. Turn-level cost
inference is separate work.

## Query actual receipts

Use `attempt schema events` for the full catalog. These count facts; continue
reading stored counts for projection tables.

```sql
SELECT provider, COUNT(*) AS observations, MAX(observed_at) AS latest
FROM events
WHERE retracted = false AND kind = 'unknown'
  AND attrs_json LIKE '%"source":"otel"%'
GROUP BY provider
```

```sql
SELECT observed_at, provider, provider_event_name, session_id, model, attrs_json
FROM events
WHERE retracted = false AND kind = 'unknown'
  AND attrs_json LIKE '%"source":"otel"%'
ORDER BY observed_at DESC LIMIT 20
```

The same statements work on an optionally connected server's `POST /v1/query`.
Local receipts do not prove upload: compare server event ids and ingestion times.

## Ownership, privacy and upgrades

The receiver binds IPv4 loopback, prefers port 4318, and persists an available
alternative in the configuration directory's private `otel.json`. Provider
settings carry its local bearer secret. It is not an API key: it only lets
a process on this machine post OTLP records to the loopback receiver and ask
it whether it is healthy (it gives no access to the stored history and opens
nothing to the network), so it is stored in plain text
in the agent's settings, which are made private (mode 0600) while the token is
there and given their old mode back on uninstall. Never copy it into shared
project files. Endpoints are `/claude_code/v1/{logs,metrics,traces}`
and `/codex/v1/{logs,metrics,traces}`. Only uncompressed OTLP/HTTP JSON is accepted.

Claude uses per-signal endpoint/protocol/header environment settings. Codex
uses `[otel]` exporter/metrics_exporter/trace_exporter with `otlp-http` and
`protocol="json"`. The conversation is exported by default (`OTEL_LOG_USER_PROMPTS=1`,
`OTEL_LOG_ASSISTANT_RESPONSES=1`; Codex `log_user_prompt = true`): the
prompt of a `user_prompt` record and the reply of an `assistant_response`
record land in `content` under the database's capture mode (never in
metadata; `x_otel_prompt_chars` / `x_otel_response_chars` carry only the
size) and leave the device only under the `messages` or `full` sync profile.
Tool arguments and tool content stay off (`OTEL_LOG_TOOL_DETAILS=0`,
`OTEL_LOG_TOOL_CONTENT=0`). Managed, project
or shell settings can override configuration; actual receipts are the final check.

A setting you wrote to switch something off is kept, and setup says so: an
explicit `OTEL_LOG_USER_PROMPTS=0` (or `false`) and `OTEL_LOG_ASSISTANT_RESPONSES=0`,
Codex `log_user_prompt = false`, and an exporter set to `none`
(`OTEL_METRICS_EXPORTER=none`, Codex `metrics_exporter = "none"`) are never
turned back on; a signal whose exporter is `none` gets none of our endpoint
settings either. (An earlier release overwrote these; the next setup puts the
value it recorded back.) Hooks capture normally either way.

Foreign exporters are preserved with a visible installation error. Configure
collector forwarding when both destinations are needed. Reinstall preserves
unrelated settings. An adjacent private ledger stores previous values.
`attempt hook uninstall --scope user` restores unchanged owned values;
project-hook removal leaves shared telemetry configuration. Codex trust is
never modified. Raw records obey capture mode and `keep_raw_payload`;
metadata-only capture discards content and user identity. Sync keeps its
existing profile rules. This is local agent telemetry, not product analytics.

Existing clients must upgrade and run `attempt hook install`. Running provider
processes must restart. No installation can recover previously missing OTel.
Windows runs a persistent scheduled daemon. Linux without systemd can use the
migration installer's session supervisor. Explicit data directories use scoped
processes whose lifetime is the host session. A disposed environment takes
its receiver with it. SDK buffering can lose unexported packets on abrupt exit.
ACK means local durable acceptance, not every SDK event or server receipt.

Provider contracts:
[Claude monitoring](https://code.claude.com/docs/en/monitoring-usage),
[Codex advanced configuration](https://learn.chatgpt.com/docs/config-file/config-advanced),
[Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).
