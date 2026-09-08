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
| Codex OTel | API/stream observations, completion tokens, latency, metrics, traces | Fields vary by event/version; absent cost is not zero |

Logs, data points and spans are immutable events with `kind='unknown'`,
`adapter_version='otel-json-v1'`, `attrs.source='otel'`, and `x_otel_signal`.
They do not create tasks or mark agents alive. `session.id` (Claude) and
`conversation.id` (Codex) identify the hook session. Missing identity stays
unattributed: inspect `x_otel_session_attributed` and
`x_otel_project_attributed`. Later hooks do not rewrite earlier facts.

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
settings carry its local bearer secret. It is not an API key; never copy it
into shared project files. Endpoints are `/claude_code/v1/{logs,metrics,traces}`
and `/codex/v1/{logs,metrics,traces}`. Only uncompressed OTLP/HTTP JSON is accepted.

Claude uses per-signal endpoint/protocol/header environment settings. Codex
uses `[otel]` exporter/metrics_exporter/trace_exporter with `otlp-http` and
`protocol="json"`. Prompt/tool-content logging defaults off. Managed, project
or shell settings can override configuration; actual receipts are the final check.

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
