# ADR 0003: Receiving OpenTelemetry from coding agents

| | |
|---|---|
| **Status** | Proposed — owner decision (TODO.md §21.5) |
| **Date** | 2026-08-30 |
| **Related** | RFC 0001 §9 (AttemptDB → OTel mapping), RFC 0005 (runtime), TODO.md §21.5 |

## Context

Hooks give AttemptDB the execution lifecycle: sessions, prompts, tool
calls, permissions, stops, subagents — every fact the projections are built
from. They do not carry what the agents' OpenTelemetry exporters carry:
model name per response, input/output/cache token counts, cost estimates,
API latency and errors, and (for some providers) the assistant's own
message events. Today "the complete Agent Timeline" is therefore hooks +
git context. The product direction (VibeMon's timeline and cost views)
wants the telemetry too, and the proposal on the table is that AttemptDB
receives it: the daemon listens for OTLP on the loopback, the installer
points each agent's exporter at it, and the events land in the same
canonical log as the hook events.

Constraints that shape any answer:

- **Single static binary.** No OpenTelemetry SDK or gRPC stack; the
  existing HTTP server (axum, already linked for `attempt ui`) plus
  `serde_json` are the budget. OTLP/HTTP with JSON encoding is a documented
  wire form; protobuf is not on the table without a dependency.
- **Metadata vs content.** Token counts, model ids, latencies and costs are
  metadata and belong in `attrs` (allowlisted, `x_otel_*` for anything not
  promoted). Assistant message bodies are content and follow the capture
  mode like prompts do.
- **Facts vs inference.** A span is a fact observed by the exporter; joining
  it to a hook event (same session, same tool call) is an inference with
  evidence ids and a confidence, like every other projection.
- **The hook path must never wait on this.** The receiver is a daemon
  feature; hooks keep appending to the spool.

## Decision (recommended)

Receive OTLP/HTTP **JSON** on the daemon, loopback only, and map it into
canonical events. Concretely:

1. `attempt daemon` listens on `127.0.0.1:4318` (configurable, off by
   default until `attempt hook install --otel` or the installer enables
   it) for `POST /v1/traces`, `/v1/metrics`, `/v1/logs` with
   `Content-Type: application/json`. Protobuf bodies are answered `415`
   with a message naming the JSON encoding; gRPC is not offered.
2. Resource and scope attributes identify the provider (Claude Code sets
   `service.name`; Codex its own). Each span/log record becomes one Event of
   a new kind family (`ModelRequest`, `ModelResponse`, `TelemetryMetric`,
   or `Unknown` with the OTel name preserved) with `provider` set from the
   resource, `provider_session_id` from the session attribute the provider
   emits, and the numeric attributes promoted to allowlisted attrs
   (`model`, `input_tokens`, `output_tokens`, `cache_read_tokens`,
   `cost_usd`, `latency_ms`, `status_code`). Everything else goes to
   `attrs.x_otel_*` under the existing 256-character, single-line, no-PII
   rules; message bodies go to `content`.
3. Events carry the OTel trace/span ids in `attrs.x_otel_trace_id` /
   `x_otel_span_id` so the projector can correlate a response with the tool
   call in flight (same session, overlapping time window) — an inference
   with the two event ids as evidence.
4. The installer writes the provider's exporter settings with the same
   structural-edit safety as hooks (Claude Code: `CLAUDE_CODE_ENABLE_TELEMETRY`,
   `OTEL_*_EXPORTER=otlp`, `OTEL_EXPORTER_OTLP_PROTOCOL=http/json`,
   `OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318` in the settings
   `env` block; Codex: its `[otel]` table — per each provider's current
   documentation, verified against fixtures like the hook payloads are).
   `attempt doctor` reports whether the receiver is up and whether each
   agent is configured; `uninstall` removes the settings.
5. Privacy: the receiver honours the database's capture mode; under
   `metadata_only` no message body is kept. Nothing the receiver stores is
   uploaded unless the sync profile allows it, exactly like hook events.

## Consequences

- One more listener in the daemon (loopback only, token-less because OTLP
  clients cannot present one; the loopback binding and the per-user daemon
  are the boundary, as for the local UI's port).
- Adapter surface grows: a fixture set of real (sanitised) OTLP/JSON
  payloads per provider, golden envelopes, and canary tests, the same
  discipline as `fixtures/providers`.
- RFC 0001 gains the reverse mapping (OTel → AttemptDB) and the new event
  kinds; `spec/event-v1.schema.json` gains the kinds — additive, no format
  version bump.
- Cost: a few days. Value: token/cost/model per turn on the timeline, which
  hooks can never provide.

## Alternatives considered

- **Not receiving OTel; document the gap.** Cheapest; leaves cost and
  model attribution to the hosted product's own integrations, which see
  nothing local. Rejected by the product direction, kept as the fallback if
  the providers' OTel output proves too unstable to fixture.
- **A separate `attempt otel` process.** Cleaner isolation, one more
  service to install and supervise on three platforms. The daemon already
  owns the writer and the service registration; a second process buys
  little.
- **Protobuf OTLP.** The default encoding for most SDKs, but it needs a
  protobuf decoder (`prost`) — a dependency the single-binary rule allows
  only with a reason; JSON is enough for the two providers that matter and
  can be revisited if a provider stops offering it.

## Decision needed

Whether to proceed with the recommendation, and whether the receiver ships
on by default (installer enables it) or opt-in. Until decided, TODO.md
§21.5 stays open and no code is written for it.
