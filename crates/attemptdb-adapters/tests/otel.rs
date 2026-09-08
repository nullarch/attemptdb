//! Authored, synthetic OTLP fixtures. No private provider payloads.
use attemptdb_adapters::{
    CaptureContext,
    otel::{MAX_RECORDS, Signal, normalise},
};
use attemptdb_core::{
    CaptureMode, DeviceId, EventKind, ProjectRef, SessionId, Timestamp, event::Provider,
};
use serde_json::{Value, json};

fn context(mode: CaptureMode) -> CaptureContext {
    let device = DeviceId::new();
    CaptureContext {
        device_id: device,
        capture_mode: mode,
        project: ProjectRef::derive("/home/dev/example/project", None, &device),
        captured_at: Timestamp::from_micros(1_787_904_100_000_000),
        provider_version: None,
        hook_version: None,
    }
}
fn attr(key: &str, value: Value) -> Value {
    json!({"key":key,"value":value})
}
fn logs(provider: &str, name: &str, attrs: Vec<Value>) -> Value {
    json!({"_fixture_note":"Authored synthetic OTLP/HTTP JSON", "resourceLogs":[{"resource":{"attributes":[attr("service.name",json!({"stringValue":provider})),attr("user.email",json!({"stringValue":"CANARY_EMAIL@example.com"}))]},"scopeLogs":[{"scope":{"name":"fixture"},"logRecords":[{"timeUnixNano":"1787904000000000000","body":{"stringValue":name},"attributes":attrs}]}]}]})
}

#[test]
fn claude_usage_is_metadata_and_joins_the_hook_session_without_lifecycle() {
    let ctx = context(CaptureMode::MetadataOnly);
    let payload = logs(
        "claude-code",
        "claude_code.api_request",
        vec![
            attr("session.id", json!({"stringValue":"fixture-session"})),
            attr("event.name", json!({"stringValue":"api_request"})),
            attr("model", json!({"stringValue":"claude-sonnet-4-6"})),
            attr("input_tokens", json!({"intValue":"123"})),
            attr("output_tokens", json!({"intValue":"45"})),
            attr("cost_usd", json!({"doubleValue":0.0042})),
            attr("duration_ms", json!({"doubleValue":234.5})),
            attr("prompt", json!({"stringValue":"CANARY_PROMPT"})),
            attr("tool_parameters", json!({"stringValue":"CANARY_COMMAND"})),
            attr("error", json!({"stringValue":"CANARY_ERROR"})),
        ],
    );
    let batch = normalise(&ctx, Provider::ClaudeCode, Signal::Logs, &payload).unwrap();
    assert_eq!(batch.rejected, 0);
    let e = &batch.events[0];
    assert!(e.is_telemetry());
    assert_eq!(e.kind, EventKind::Unknown);
    assert_eq!(
        e.session_id,
        SessionId::derive(&["claude_code", "fixture-session"])
    );
    assert_eq!(e.attrs["x_otel_input_tokens"], 123);
    assert_eq!(e.attrs["x_otel_output_tokens"], 45);
    assert_eq!(e.attrs["x_otel_cost_usd"], 0.0042);
    assert_eq!(e.agent.model.as_deref(), Some("claude-sonnet-4-6"));
    assert!(e.raw.is_none() && e.content.is_none());
    assert!(!serde_json::to_string(e).unwrap().contains("CANARY"));
    let mut later = ctx.clone();
    later.captured_at = Timestamp::now();
    assert_eq!(
        e.event_id,
        normalise(&later, Provider::ClaudeCode, Signal::Logs, &payload)
            .unwrap()
            .events[0]
            .event_id
    );
    let other = context(CaptureMode::MetadataOnly);
    assert_ne!(
        e.event_id,
        normalise(&other, Provider::ClaudeCode, Signal::Logs, &payload)
            .unwrap()
            .events[0]
            .event_id
    );
}

#[test]
fn codex_completion_preserves_reported_tokens_not_prompt_or_tool_text() {
    let payload = logs(
        "codex-cli",
        "codex.sse_event",
        vec![
            attr(
                "conversation.id",
                json!({"stringValue":"fixture-conversation"}),
            ),
            attr("event_kind", json!({"stringValue":"response.completed"})),
            attr("input_token_count", json!({"intValue":"800"})),
            attr("output_token_count", json!({"intValue":"120"})),
            attr("cached_token_count", json!({"intValue":"600"})),
            attr("reasoning_output_token_count", json!({"intValue":"90"})),
            attr("tool_input", json!({"stringValue":"CANARY_TOOL_INPUT"})),
        ],
    );
    let batch = normalise(
        &context(CaptureMode::MetadataOnly),
        Provider::Codex,
        Signal::Logs,
        &payload,
    )
    .unwrap();
    let e = &batch.events[0];
    assert_eq!(e.provider_event_name, "codex.sse_event");
    assert_eq!(
        e.session_id,
        SessionId::derive(&["codex", "fixture-conversation"])
    );
    assert_eq!(e.attrs["x_otel_cache_read_tokens"], 600);
    assert_eq!(e.attrs["x_otel_reasoning_tokens"], 90);
    assert_eq!(e.attrs["x_otel_event_kind"], "response.completed");
    assert!(!serde_json::to_string(e).unwrap().contains("CANARY"));
}

#[test]
fn codex_zero_log_timestamp_uses_its_observed_time_or_explicit_event_timestamp() {
    let mut payload = logs(
        "codex-cli",
        "",
        vec![
            attr("event.name", json!({"stringValue":"codex.sse_event"})),
            attr("event.kind", json!({"stringValue":"response.completed"})),
            attr(
                "conversation.id",
                json!({"stringValue":"fixture-conversation"}),
            ),
            attr("reasoning_token_count", json!({"intValue":"42"})),
            attr("cache_write_token_count", json!({"intValue":"12"})),
        ],
    );
    let row = &mut payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
    row["timeUnixNano"] = json!("0");
    row["observedTimeUnixNano"] = json!("1787904001000000000");
    let ctx = context(CaptureMode::MetadataOnly);
    let e = normalise(&ctx, Provider::Codex, Signal::Logs, &payload)
        .unwrap()
        .events
        .remove(0);
    assert_eq!(e.observed_at.as_micros(), 1_787_904_001_000_000);
    assert_eq!(e.attrs["x_otel_event_kind"], "response.completed");
    assert_eq!(e.attrs["x_otel_reasoning_tokens"], 42);
    assert_eq!(e.attrs["x_otel_cache_creation_tokens"], 12);
    assert_eq!(e.attrs["x_otel_session_attributed"], true);
    payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["attributes"]
        .as_array_mut()
        .unwrap()
        .push(attr(
            "event.timestamp",
            json!({"stringValue":"2026-08-28T08:00:00Z"}),
        ));
    let e = normalise(&ctx, Provider::Codex, Signal::Logs, &payload)
        .unwrap()
        .events
        .remove(0);
    assert_eq!(e.observed_at.as_micros(), 1_787_904_000_000_000);
}

#[test]
fn structured_span_events_keep_conversation_context_but_os_thread_ids_do_not() {
    let payload = json!({"resourceSpans":[{"scopeSpans":[{"spans":[{
        "name":"handle_responses", "startTimeUnixNano":"1787904000000000000",
        "endTimeUnixNano":"1787904001000000000", "traceId":"1234567890abcdef1234567890abcdef", "spanId":"1234567890abcdef",
        "attributes":[attr("thread.id",json!({"intValue":"20"}))],
        "events":[{"name":"codex.sse_event","timeUnixNano":"1787904000500000000","attributes":[
            attr("conversation.id",json!({"stringValue":"fixture-conversation"})),
            attr("model",json!({"stringValue":"fixture-model"})),
            attr("input_token_count",json!({"intValue":"100"})),
            attr("prompt",json!({"stringValue":"CANARY_PROMPT"}))
        ]}]
    }]}]}]});
    let rows = normalise(
        &context(CaptureMode::MetadataOnly),
        Provider::Codex,
        Signal::Traces,
        &payload,
    )
    .unwrap()
    .events;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].attrs["x_otel_session_attributed"], false);
    assert_eq!(rows[1].attrs["x_otel_record_type"], "span_event");
    assert_eq!(rows[1].attrs["x_otel_signal"], "traces");
    assert_eq!(rows[1].attrs["x_otel_session_attributed"], true);
    assert_eq!(rows[1].attrs["x_otel_input_tokens"], 100);
    assert_eq!(rows[1].agent.model.as_deref(), Some("fixture-model"));
    assert!(!serde_json::to_string(&rows).unwrap().contains("CANARY"));
}

#[test]
fn cumulative_samples_keep_temporality_start_and_dimensions_and_are_not_summed() {
    let payload = json!({"resourceMetrics":[{"scopeMetrics":[{"metrics":[{"name":"claude_code.token.usage","unit":"{token}","sum":{"aggregationTemporality":"AGGREGATION_TEMPORALITY_CUMULATIVE","isMonotonic":true,"dataPoints":[
        {"timeUnixNano":"1787904000000000000","startTimeUnixNano":"1787903900000000000","asInt":"20","attributes":[attr("type",json!({"stringValue":"input"}))]},
        {"timeUnixNano":"1787904060000000000","startTimeUnixNano":"1787903900000000000","asInt":"25","attributes":[attr("type",json!({"stringValue":"input"}))]}
    ]}}]}]}]});
    let batch = normalise(
        &context(CaptureMode::MetadataOnly),
        Provider::ClaudeCode,
        Signal::Metrics,
        &payload,
    )
    .unwrap();
    assert_eq!(batch.events.len(), 2);
    let a = &batch.events[0].attrs;
    assert_eq!(a["x_otel_temporality"], 2);
    assert_eq!(
        a["x_otel_start_time_unix_nano"],
        1_787_903_900_000_000_000_u64
    );
    assert_eq!(a["x_otel_unit"], "{token}");
    assert_eq!(a["x_otel_token_type"], "input");
    assert_eq!(a["x_otel_session_attributed"], false);
    assert_eq!(batch.events[1].attrs["x_otel_value"], 25);
    assert_ne!(batch.events[0].event_id, batch.events[1].event_id);
}

#[test]
fn trace_identity_status_and_timing_survive_without_span_content() {
    let payload = json!({"resourceSpans":[{"scopeSpans":[{"spans":[{"name":"claude_code.api_request","traceId":"1234567890abcdef1234567890abcdef","spanId":"1234567890abcdef","parentSpanId":"1111111111111111","startTimeUnixNano":"1787904000000000000","endTimeUnixNano":"1787904001234000000","status":{"code":"STATUS_CODE_ERROR","message":"CANARY_STDERR"},"attributes":[attr("session.id",json!({"stringValue":"fixture-session"})),attr("gen_ai.input.messages",json!({"stringValue":"CANARY_PROMPT"}))]}]}]}]});
    let e = normalise(
        &context(CaptureMode::MetadataOnly),
        Provider::ClaudeCode,
        Signal::Traces,
        &payload,
    )
    .unwrap()
    .events
    .remove(0);
    assert_eq!(e.attrs["x_otel_duration_ms"], 1234);
    assert_eq!(e.attrs["x_otel_span_status_code"], 2);
    assert_eq!(e.attrs["x_otel_parent_span_id"], "1111111111111111");
    assert!(!serde_json::to_string(&e).unwrap().contains("CANARY"));
    let raw = normalise(
        &context(CaptureMode::LocalSemantic),
        Provider::ClaudeCode,
        Signal::Traces,
        &payload,
    )
    .unwrap()
    .events
    .remove(0);
    assert!(raw.raw.is_some());
    assert!(
        !serde_json::to_string(&raw.attrs)
            .unwrap()
            .contains("CANARY")
    );
}

#[test]
fn invalid_records_are_reported_and_oversized_batches_fail() {
    let ctx = context(CaptureMode::MetadataOnly);
    assert!(normalise(&ctx, Provider::Codex, Signal::Logs, &json!({})).is_err());
    let mut payload = logs("codex", "codex.api_request", vec![]);
    payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["timeUnixNano"] = json!("0");
    let batch = normalise(&ctx, Provider::Codex, Signal::Logs, &payload).unwrap();
    assert_eq!(batch.rejected, 1);
    assert!(batch.events.is_empty());
    payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"] =
        json!(vec![json!({"timeUnixNano":"1"}); MAX_RECORDS + 1]);
    assert!(normalise(&ctx, Provider::Codex, Signal::Logs, &payload).is_err());
}
