//! Authored, synthetic OTLP fixtures. No private provider payloads.
use attemptdb_adapters::{
    CaptureContext,
    otel::{MAX_RECORDS, Signal, normalise, retained},
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
        "codex.api_request",
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
    assert_eq!(e.provider_event_name, "codex.api_request");
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
            attr("event.name", json!({"stringValue":"codex.api_request"})),
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
        "events":[{"name":"codex.api_request","timeUnixNano":"1787904000500000000","attributes":[
            attr("conversation.id",json!({"stringValue":"fixture-conversation"})),
            attr("model",json!({"stringValue":"fixture-model"})),
            attr("input_token_count",json!({"intValue":"100"})),
            attr("prompt",json!({"stringValue":"CANARY_PROMPT"}))
        ]}]
    }]}]}]});
    let batch = normalise(
        &context(CaptureMode::MetadataOnly),
        Provider::Codex,
        Signal::Traces,
        &payload,
    )
    .unwrap();
    // The bare span carries no conversation: it is Codex's own execution
    // trace and is not kept. Its structured event is the observation.
    assert_eq!(batch.dropped, 1);
    assert_eq!(batch.rejected, 0);
    let rows = batch.events;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].attrs["x_otel_record_type"], "span_event");
    assert_eq!(rows[0].attrs["x_otel_signal"], "traces");
    assert_eq!(rows[0].attrs["x_otel_session_attributed"], true);
    assert_eq!(rows[0].attrs["x_otel_input_tokens"], 100);
    assert_eq!(rows[0].agent.model.as_deref(), Some("fixture-model"));
    assert!(!serde_json::to_string(&rows).unwrap().contains("CANARY"));
}

#[test]
fn spans_without_a_session_are_dropped_and_spans_with_one_are_kept() {
    let span = |name: &str, attrs: Vec<Value>| {
        json!({"name":name, "startTimeUnixNano":"1787904000000000000", "endTimeUnixNano":"1787904000100000000",
            "traceId":"1234567890abcdef1234567890abcdef", "spanId":"1234567890abcdef", "attributes":attrs})
    };
    let payload = json!({"resourceSpans":[{"scopeSpans":[{"spans":[
        span("receiving", vec![attr("thread.id", json!({"intValue":"20"}))]),
        span("handle_responses", vec![]),
        span("codex.tool_result", vec![attr("conversation.id", json!({"stringValue":"fixture-conversation"}))]),
    ]}]}]});
    let batch = normalise(
        &context(CaptureMode::MetadataOnly),
        Provider::Codex,
        Signal::Traces,
        &payload,
    )
    .unwrap();
    assert_eq!(
        (batch.events.len(), batch.dropped, batch.rejected),
        (1, 2, 0)
    );
    let kept = &batch.events[0];
    assert_eq!(kept.provider_event_name, "codex.tool_result");
    assert_eq!(kept.attrs["x_otel_record_type"], "span");
    assert_eq!(kept.attrs["x_otel_session_attributed"], true);
    assert!(retained(kept));
    // The rule is the same function the sync server applies to uploads:
    // a hook event is always kept, a log record is always kept.
    let mut log = kept.clone();
    log.attrs
        .insert("x_otel_record_type".into(), json!("log_record"));
    log.attrs
        .insert("x_otel_session_attributed".into(), json!(false));
    assert!(retained(&log));
    let mut bare = kept.clone();
    bare.attrs
        .insert("x_otel_session_attributed".into(), json!(false));
    assert!(!retained(&bare));
    bare.attrs.remove("source");
    assert!(retained(&bare), "not from OTel: not the rule's business");
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

#[test]
fn exported_prompt_and_reply_become_content_under_the_capture_mode_never_metadata() {
    let prompt = logs(
        "claude-code",
        "claude_code.user_prompt",
        vec![
            attr("session.id", json!({"stringValue":"fixture-session"})),
            attr("event.name", json!({"stringValue":"user_prompt"})),
            attr("prompt_length", json!({"intValue":"27"})),
            attr(
                "prompt",
                json!({"stringValue":"make the retries idempotent"}),
            ),
        ],
    );
    let reply = logs(
        "claude-code",
        "claude_code.assistant_response",
        vec![
            attr("session.id", json!({"stringValue":"fixture-session"})),
            attr("event.name", json!({"stringValue":"assistant_response"})),
            attr("response_length", json!({"intValue":"38"})),
            attr(
                "response",
                json!({"stringValue":"I will read the webhook handler first."}),
            ),
            attr("model", json!({"stringValue":"claude-sonnet-4-6"})),
        ],
    );
    let redacted = logs(
        "claude-code",
        "claude_code.assistant_response",
        vec![
            attr("session.id", json!({"stringValue":"fixture-session"})),
            attr("event.name", json!({"stringValue":"assistant_response"})),
            attr("response_length", json!({"intValue":"38"})),
            attr("response", json!({"stringValue":"<REDACTED>"})),
        ],
    );
    // Local content mode: the text is content, its size is metadata.
    let ctx = context(CaptureMode::LocalSemantic);
    let e = &normalise(&ctx, Provider::ClaudeCode, Signal::Logs, &prompt)
        .unwrap()
        .events[0];
    assert_eq!(
        e.content.as_ref().unwrap().prompt.as_deref(),
        Some("make the retries idempotent")
    );
    assert_eq!(e.attrs["x_otel_prompt_chars"], 27);
    assert!(
        !serde_json::to_string(&e.attrs)
            .unwrap()
            .contains("idempotent")
    );
    assert!(e.is_telemetry());
    let e = &normalise(&ctx, Provider::ClaudeCode, Signal::Logs, &reply)
        .unwrap()
        .events[0];
    assert_eq!(
        e.content.as_ref().unwrap().message.as_deref(),
        Some("I will read the webhook handler first.")
    );
    assert_eq!(e.attrs["x_otel_response_chars"], 38);
    assert!(!serde_json::to_string(&e.attrs).unwrap().contains("webhook"));
    // A provider-side redaction leaves no content, only the size.
    let e = &normalise(&ctx, Provider::ClaudeCode, Signal::Logs, &redacted)
        .unwrap()
        .events[0];
    assert!(e.content.is_none());
    assert_eq!(e.attrs["x_otel_response_chars"], 38);
    // Metadata-only capture: the text never lands anywhere.
    let ctx = context(CaptureMode::MetadataOnly);
    for payload in [&prompt, &reply] {
        let e = &normalise(&ctx, Provider::ClaudeCode, Signal::Logs, payload)
            .unwrap()
            .events[0];
        assert!(e.content.is_none() && e.raw.is_none());
        let text = serde_json::to_string(e).unwrap();
        assert!(!text.contains("idempotent") && !text.contains("webhook"));
    }
}

#[test]
fn codex_sse_events_are_discarded_not_stored_and_not_reported_rejected() {
    // Named by the record body.
    let by_body = logs(
        "codex-cli",
        "codex.sse_event",
        vec![attr(
            "conversation.id",
            json!({"stringValue":"fixture-conversation"}),
        )],
    );
    // Named by the `event.name` attribute, as Codex exports it.
    let by_attribute = logs(
        "codex-cli",
        "",
        vec![
            attr("event.name", json!({"stringValue":"codex.sse_event"})),
            attr(
                "conversation.id",
                json!({"stringValue":"fixture-conversation"}),
            ),
        ],
    );
    for payload in [by_body, by_attribute] {
        let batch = normalise(
            &context(CaptureMode::MetadataOnly),
            Provider::Codex,
            Signal::Logs,
            &payload,
        )
        .unwrap();
        assert!(batch.events.is_empty());
        assert_eq!(batch.dropped, 1);
        assert_eq!(batch.rejected, 0);
    }
}

#[test]
fn codex_sse_span_events_are_discarded_but_their_span_and_siblings_are_kept() {
    let payload = json!({"resourceSpans":[{"scopeSpans":[{"spans":[{
        "name":"handle_responses", "startTimeUnixNano":"1787904000000000000",
        "endTimeUnixNano":"1787904001000000000", "traceId":"1234567890abcdef1234567890abcdef", "spanId":"1234567890abcdef",
        // A span that carries a session is kept (a bare one is not: see
        // `otel::retained`), so only the SSE span event is discarded here.
        "attributes":[
            attr("thread.id",json!({"intValue":"20"})),
            attr("conversation.id",json!({"stringValue":"fixture-conversation"}))
        ],
        "events":[
            {"name":"codex.sse_event","timeUnixNano":"1787904000500000000","attributes":[
                attr("conversation.id",json!({"stringValue":"fixture-conversation"}))
            ]},
            {"name":"codex.api_request","timeUnixNano":"1787904000600000000","attributes":[
                attr("conversation.id",json!({"stringValue":"fixture-conversation"}))
            ]}
        ]
    }]}]}]});
    let batch = normalise(
        &context(CaptureMode::MetadataOnly),
        Provider::Codex,
        Signal::Traces,
        &payload,
    )
    .unwrap();
    assert_eq!(batch.dropped, 1);
    assert_eq!(batch.rejected, 0);
    // The span itself and the other span event survive.
    assert_eq!(batch.events.len(), 2);
    assert!(
        batch
            .events
            .iter()
            .all(|e| e.provider_event_name != "codex.sse_event")
    );
}

// ---------------------------------------------------------------------------
// otel-retention-v3: what is discarded, what is kept
// ---------------------------------------------------------------------------

fn session_attr() -> Vec<Value> {
    vec![
        attr("session.id", json!({"stringValue":"fixture-session"})),
        attr("conversation.id", json!({"stringValue":"fixture-session"})),
    ]
}

fn metric_payload(name: &str, attrs: Vec<Value>) -> Value {
    json!({"resourceMetrics":[{"scopeMetrics":[{"metrics":[{"name":name,"unit":"1","sum":{
    "aggregationTemporality":"AGGREGATION_TEMPORALITY_DELTA","isMonotonic":true,"dataPoints":[
        {"timeUnixNano":"1787904000000000000","asInt":"3","attributes":attrs}
    ]}}]}]}]})
}

fn span_payload(name: &str, attrs: Vec<Value>) -> Value {
    json!({"resourceSpans":[{"scopeSpans":[{"spans":[{
        "name":name, "startTimeUnixNano":"1787904000000000000",
        "endTimeUnixNano":"1787904000100000000",
        "traceId":"1234567890abcdef1234567890abcdef", "spanId":"1234567890abcdef",
        "attributes":attrs}]}]}]})
}

fn span_event_payload(name: &str, attrs: Vec<Value>) -> Value {
    json!({"resourceSpans":[{"scopeSpans":[{"spans":[{
        "name":"parent", "startTimeUnixNano":"1787904000000000000",
        "endTimeUnixNano":"1787904000100000000",
        "traceId":"1234567890abcdef1234567890abcdef", "spanId":"1234567890abcdef",
        "attributes":session_attr(),
        "events":[{"name":name,"timeUnixNano":"1787904000050000000","attributes":attrs}]}]}]}]})
}

/// Every payload shape one family can arrive in, each with a session so only
/// the family rule (not the bare-span rule) can decide.
fn family_payloads(name: &str) -> Vec<(Signal, Value)> {
    vec![
        (Signal::Logs, logs("x", name, session_attr())),
        (
            Signal::Logs,
            logs(
                "x",
                "",
                [
                    session_attr(),
                    vec![attr("event.name", json!({"stringValue":name}))],
                ]
                .concat(),
            ),
        ),
        (Signal::Metrics, metric_payload(name, session_attr())),
        (Signal::Metrics, metric_payload(name, vec![])),
        (Signal::Traces, span_event_payload(name, session_attr())),
    ]
}

const DISCARDED_FAMILIES: &[(&str, Provider)] = &[
    ("codex.sse_event", Provider::Codex),
    ("codex.sse_event.duration_ms", Provider::Codex),
    ("codex.sqlite.logs.write.max_entry_bytes", Provider::Codex),
    ("codex.sqlite.logs.write.count", Provider::Codex),
    ("codex.sqlite.logs.write.duration_ms", Provider::Codex),
    ("codex.sqlite.logs.write.bytes", Provider::Codex),
    ("codex.sqlite.logs.write.entries", Provider::Codex),
    ("hook_execution_start", Provider::ClaudeCode),
    ("hook_execution_complete", Provider::ClaudeCode),
    ("claude_code.hook_execution_start", Provider::ClaudeCode),
    ("claude_code.hook_execution_complete", Provider::ClaudeCode),
];

const KEPT_FAMILIES: &[(&str, Provider)] = &[
    ("api_request", Provider::ClaudeCode),
    ("claude_code.api_request", Provider::ClaudeCode),
    ("claude_code.llm_request", Provider::ClaudeCode),
    ("gen_ai.request.attempt", Provider::ClaudeCode),
    ("claude_code.token.usage", Provider::ClaudeCode),
    ("claude_code.cost.usage", Provider::ClaudeCode),
    ("claude_code.tool", Provider::ClaudeCode),
    ("claude_code.tool.blocked_on_user", Provider::ClaudeCode),
    ("tool_result", Provider::ClaudeCode),
    ("tool_decision", Provider::ClaudeCode),
    ("user_prompt", Provider::ClaudeCode),
    ("assistant_response", Provider::ClaudeCode),
    // Near misses of the discard prefixes: a different family, kept.
    ("claude_code.hook", Provider::ClaudeCode),
    ("hooks_installed", Provider::ClaudeCode),
    ("codex.sse", Provider::Codex),
    ("codex.api_request", Provider::Codex),
    ("codex.api_request.duration_ms", Provider::Codex),
    ("codex.tool_result", Provider::Codex),
    ("codex.tool_decision", Provider::Codex),
    ("codex.user_prompt", Provider::Codex),
    ("codex.conversation_starts", Provider::Codex),
    ("codex.turn.token_usage", Provider::Codex),
    ("codex.tool.call", Provider::Codex),
    ("codex.sqlite", Provider::Codex),
];

#[test]
fn every_discarded_family_is_dropped_not_stored_and_not_rejected() {
    for (name, provider) in DISCARDED_FAMILIES {
        assert!(
            attemptdb_adapters::otel::is_discarded(name),
            "{name} is on the discard list"
        );
        for (signal, payload) in family_payloads(name) {
            let batch = normalise(
                &context(CaptureMode::MetadataOnly),
                provider.clone(),
                signal,
                &payload,
            )
            .unwrap();
            let shape = format!("{name} as {}", signal.as_str());
            // A span event is dropped while its (attributed) parent span is
            // kept: that parent is the only thing a trace payload leaves.
            let left = if signal == Signal::Traces { 1 } else { 0 };
            assert_eq!(batch.events.len(), left, "{shape}: stored");
            assert!(
                batch.events.iter().all(|e| e.provider_event_name != *name),
                "{shape}: {name} was stored"
            );
            assert_eq!(batch.dropped, 1, "{shape}: counted as dropped");
            assert_eq!(batch.rejected, 0, "{shape}: never reported as rejected");
        }
    }
}

#[test]
fn every_kept_family_is_stored_in_every_shape() {
    for (name, provider) in KEPT_FAMILIES {
        assert!(
            !attemptdb_adapters::otel::is_discarded(name),
            "{name} must not be on the discard list"
        );
        for (signal, payload) in family_payloads(name) {
            let batch = normalise(
                &context(CaptureMode::MetadataOnly),
                provider.clone(),
                signal,
                &payload,
            )
            .unwrap();
            let shape = format!("{name} as {}", signal.as_str());
            // A span event arrives with its (attributed) parent span: one
            // more kept record, never a dropped one.
            let expected = if signal == Signal::Traces { 2 } else { 1 };
            assert_eq!(batch.events.len(), expected, "{shape}: stored");
            assert_eq!(batch.dropped, 0, "{shape}: nothing dropped");
            assert_eq!(batch.rejected, 0, "{shape}");
            assert!(batch.events.iter().all(retained), "{shape}: retained()");
        }
    }
}

#[test]
fn a_kept_span_family_is_stored_when_attributed_and_dropped_when_bare() {
    for name in [
        "claude_code.llm_request",
        "claude_code.tool.blocked_on_user",
    ] {
        let attributed = normalise(
            &context(CaptureMode::MetadataOnly),
            Provider::ClaudeCode,
            Signal::Traces,
            &span_payload(name, session_attr()),
        )
        .unwrap();
        assert_eq!((attributed.events.len(), attributed.dropped), (1, 0));
        let bare = normalise(
            &context(CaptureMode::MetadataOnly),
            Provider::ClaudeCode,
            Signal::Traces,
            &span_payload(name, vec![]),
        )
        .unwrap();
        assert_eq!((bare.events.len(), bare.dropped), (0, 1));
    }
}

#[test]
fn a_batch_with_nothing_left_has_no_events_and_counts_every_record_as_dropped() {
    // Several discarded records and one that cannot be read (no timestamp):
    // the receiver acknowledges the request without waking the writer, and
    // the unreadable one is the only rejection.
    let mut payload = logs("x", "codex.sse_event", session_attr());
    let rows = payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
        .as_array_mut()
        .unwrap();
    let template = rows[0].clone();
    rows.clear();
    for name in [
        "codex.sse_event",
        "codex.sqlite.logs.write.count",
        "hook_execution_start",
        "hook_execution_complete",
    ] {
        let mut row = template.clone();
        row["body"] = json!({"stringValue":name});
        rows.push(row);
    }
    let mut unreadable = template.clone();
    unreadable["timeUnixNano"] = json!("0");
    rows.push(unreadable);
    let batch = normalise(
        &context(CaptureMode::MetadataOnly),
        Provider::Codex,
        Signal::Logs,
        &payload,
    )
    .unwrap();
    assert!(batch.events.is_empty());
    assert_eq!((batch.dropped, batch.rejected), (4, 1));
}

#[test]
fn a_mixed_batch_keeps_what_is_kept_and_counts_the_rest() {
    let mut payload = logs("x", "codex.api_request", session_attr());
    let rows = payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
        .as_array_mut()
        .unwrap();
    let template = rows[0].clone();
    for name in [
        "codex.tool_result",
        "codex.sse_event",
        "hook_execution_complete",
    ] {
        let mut row = template.clone();
        row["body"] = json!({"stringValue":name});
        rows.push(row);
    }
    let batch = normalise(
        &context(CaptureMode::MetadataOnly),
        Provider::Codex,
        Signal::Logs,
        &payload,
    )
    .unwrap();
    let names: Vec<&str> = batch
        .events
        .iter()
        .map(|e| e.provider_event_name.as_str())
        .collect();
    assert_eq!(names, ["codex.api_request", "codex.tool_result"]);
    assert_eq!((batch.dropped, batch.rejected), (2, 0));
}

#[test]
fn hook_execution_records_add_nothing_the_hook_events_do_not_already_carry() {
    // The comparison the discard rule rests on: promoted into metadata, a
    // `hook_execution_complete` record carries its name, session and time
    // (and a sequence number). The exporter's own attributes (hook name,
    // counts, total duration) are not promoted, so nothing reads them.
    let record = logs(
        "claude-code",
        "",
        vec![
            attr(
                "event.name",
                json!({"stringValue":"hook_execution_complete"}),
            ),
            attr("session.id", json!({"stringValue":"fixture-session"})),
            attr("event.sequence", json!({"intValue":"7"})),
            attr("hook_event", json!({"stringValue":"PostToolUse"})),
            attr("hook_name", json!({"stringValue":"PostToolUse:Write"})),
            attr("num_hooks", json!({"intValue":"1"})),
            attr("num_success", json!({"intValue":"1"})),
            attr("num_blocking", json!({"intValue":"0"})),
            attr("total_duration_ms", json!({"intValue":"12"})),
        ],
    );
    // Observe what the intake would store if the family were not discarded,
    // by comparing against a kept log record built the same way.
    let mut as_kept = record.clone();
    as_kept["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["attributes"][0] =
        attr("event.name", json!({"stringValue":"tool_result"}));
    let stored = &normalise(
        &context(CaptureMode::MetadataOnly),
        Provider::ClaudeCode,
        Signal::Logs,
        &as_kept,
    )
    .unwrap()
    .events[0];
    let promoted: std::collections::BTreeSet<&str> = stored
        .attrs
        .keys()
        .map(String::as_str)
        .filter(|k| k.starts_with("x_otel_"))
        .collect();
    assert_eq!(
        promoted,
        [
            "x_otel_event_sequence",
            "x_otel_record_type",
            "x_otel_session_attributed",
            "x_otel_signal",
        ]
        .into_iter()
        .collect(),
        "hook counts and durations are not promoted: {:?}",
        stored.attrs
    );
    let batch = normalise(
        &context(CaptureMode::MetadataOnly),
        Provider::ClaudeCode,
        Signal::Logs,
        &record,
    )
    .unwrap();
    assert_eq!((batch.events.len(), batch.dropped), (0, 1));
}
