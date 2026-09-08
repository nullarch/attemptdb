# ADR 0003: Receiving OpenTelemetry from coding agents

| | |
|---|---|
| **Status** | Accepted — owner requested default collection on 2026-09-08 |
| **Date** | 2026-08-30; decision updated 2026-09-08 |
| **Related** | RFC 0001 §9, RFC 0005, RFC 0006, TODO.md §21.5 |

## Decision

`attempt hook install` configures Claude Code and Codex telemetry by default,
alongside hooks for detected agents. Hooks record execution lifecycle; OTel
adds model, token, reported cost, API latency and trace observations. Neither
pairing nor installed configuration proves an actual telemetry receipt.

The existing daemon accepts authenticated OTLP/HTTP JSON at
`127.0.0.1:<port>/<provider>/v1/{logs,metrics,traces}`. Port 4318 is preferred;
a free alternative is persisted when occupied. The local bearer secret lives
in user-private configuration. Browser Origin requests, protobuf, compression
and unsupported providers are refused. The axum/serde stack is already linked;
no gRPC stack, external collector or new storage format is needed.

The receiver acknowledges after the existing single writer's durable WAL
append. Original resource/scope/record, timestamp, provider, signal and device
derive a stable event id; identical retries are idempotent. Limits: 4 MiB,
4,096 records, 16 concurrent admitted batches.

Telemetry uses `kind=unknown`, `adapter_version=otel-json-v1`, `attrs.source=otel`
and typed `attrs.x_otel_*`. It does not synthesize lifecycle facts or advance
work. Provider session ids derive the same SessionId as hooks. A known hook
session can supply its project identity; missing identity stays explicitly
unattributed. Temporal overlap alone is not an assignment rule. Trace/span
ids and metric temporality/start time remain available. Tool-level causal
links and per-turn usage projections are separate, unfinished work.

Installation structurally edits Claude's user `env` and Codex's user `[otel]`,
never Codex's `[hooks.state]`. Each signal uses the provider's documented JSON
exporter. Claude traces additionally require enhanced telemetry and a
supporting version. Prompt/tool-content logging defaults off. Foreign
collectors are preserved and reported as a conflict. Locked, backed-up atomic
edits have an ownership ledger; uninstall restores only unchanged owned values.
Project-hook removal does not disable shared user telemetry settings.

Windows replaces minute-only maintenance with a persistent scheduled daemon:
immediate start, IgnoreNew, no execution/battery cutoff, one-minute recovery
triggers. macOS/Linux use existing daemons. Direct hook installation starts a
scoped runtime when a service is unavailable. The migration installer's Linux
session supervisor remains the crash-recovering option without systemd.

## Privacy and limits

Only a fixed set of typed metadata fields is promoted. Prompt text, commands,
output, user identity, error text and unknown attributes do not become
metadata. Raw records obey capture mode and `keep_raw_payload`; metadata-only
capture discards them. Optional sync uses existing profile/redaction rules.
Local collection does not require VibeMon.

Doctor distinguishes configured/running, current-process receipts and durable
local observations per provider/signal. Already-running agents must restart.
SDK buffering can lose unexported data on abrupt exit. Missing fields are not
assumed zero; cumulative metric samples must not be summed, and logs, metrics
and traces must not be counted as independent token usage.

## Alternatives

- A separate collector adds an executable and service to manage.
- Protobuf/gRPC adds dependencies where both providers offer JSON.
- Mapping periodic metrics to lifecycle facts duplicates hooks and makes
  finished sessions appear active. Projection and live-work signals exclude it.

Configuration, queries and provider references: [local telemetry](../otel.md).
