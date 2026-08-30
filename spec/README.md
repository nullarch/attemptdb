# AttemptDB Event v1

An open canonical event format for AI coding-agent activity.

`event-v1.schema.json` is the published form of RFC 0001's canonical event —
the structure the reference implementation (`attemptdb-core::Event`) has
serialised since the first commit, not a redesign for publication. The
implementation is the standard; this directory declares the stable part of it
and gives third parties a way to prove they speak it.

## Field groups

| Group | Fields | What it guarantees |
|---|---|---|
| Identity | `event_id`, `device_id`, `provider`, `session_id`, `provider_session_id`, `agent.*`, `project.*` | `session_id` is UUIDv5 of `(provider, provider_session_id)`; `project_id` is UUIDv5 of the normalised remote. Every device derives the same ids for the same thing. `correction` and `retraction` events carry the session they are *about*, so the derivation rule is not applied to them. |
| Temporal | `observed_at`, `captured_at`, `ingested_at`, `source_seq`, `hlc` | Three clocks kept apart: when it happened, when it was seen, when it was accepted. Per-device order is `source_seq`; cross-device order is `hlc`. |
| Relationships | `span_id`, `parent_span_id`, `tool.call_id`, `agent.parent_agent_id` | A `parent_span_id` refers to a `span_id` in the same stream. Causal edges beyond containment are inference (RFC 0003) and never live on an event. |
| Provenance | `adapter_version`, `hook_version`, `provider_version`, `provider_event_name`, `capture_mode` | Enough to re-derive or discount the canonical form later. The provider's own event name is always kept. |
| Extensions | `attrs` (allowlisted or `x_<provider>_*`), top-level `x_*`, `attrs.provider`, `raw` | Writers namespace what the schema does not define; readers preserve it. |
| Content | `content.*`, `raw` | The only content-bearing fields. Absent under `metadata_only`. |

## Conformance

A stream is conformant when every line parses against the schema **and**
the semantic rules hold across the stream:

```
attempt conformance events.jsonl

AttemptDB Event v1 · 1,204 events

Envelope            ✓   every line parses; schema_version 1; ids present and unique
Identity            ✓   session_id derives from (provider, provider_session_id)
Temporal            ✓   source_seq and hlc strictly increase per device
Causality           ✓   every parent_span_id resolves in the stream
Provenance          ✓   adapter_version present; no content under metadata_only
Extensions          ✓   attrs on the allowlist or x_-namespaced; values content-free

COMPATIBLE
```

Rules that a single-event stream cannot exercise (tool-call pairing,
sequence gaps) are reported as notes, not failures. Exit status is 0 when
compatible, 1 otherwise; `--json` prints the report as data.

The same checks run in CI over every golden fixture in
`fixtures/providers/`, and a test round-trips a fully populated `Event`
through the schema, so the schema cannot drift from the implementation in
either direction without a failing build.

## Versioning

- `schema_version` is the canonical model version (RFC 0001), independent
  of the on-disk storage format (RFC 0002).
- Adding an optional field or an `x_*` extension is not a version change.
- Changing the meaning or type of an existing field, or adding a required
  one, is a new `schema_version` and a new schema file.
- Unknown `kind` values are not permitted in v1; a provider event with no
  canonical mapping is emitted as `unknown` with `provider_event_name` kept.

## Not in this directory yet

Inference records (attempts, work units, decisions, corrections) have a
model (RFC 0003) but no published schema. They will be `inference-v1` when
their fields have stopped moving; they are deliberately not part of the
event schema, because an event is a fact and an inference is not.

## Inference v1

`inference-v1.schema.json` is the wire form of a device's own inferences —
attempts, handoffs, work units, decisions — as `attempt sync
--send-inferences` uploads them (RFC 0006 §10.7). It exists so the
fact/inference line (RFC 0003) survives the network: every item carries the
event ids it was derived from, a confidence in `[0, 1]`, and the algorithm
version, and a server that receives one stores it beside the event database,
never in it. Items without evidence are refused; the content-bearing fields
(`objective`, `rationale`) are null unless the device opted into content.
Sessions, turns, tool calls, and causal edges are not uploaded: they are
one-to-one with facts or derivable from them by anyone holding the events.
