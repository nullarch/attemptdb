//! OTLP/HTTP JSON observations. Telemetry complements hooks; it must not
//! duplicate their lifecycle events or turn a periodic metric into work.
//!
//! The wire-compatible envelope uses `unknown` plus `source = otel` and
//! `x_otel_signal`. Only explicitly typed metadata is promoted. Original
//! records remain content, subject to the existing capture/privacy mode.

use crate::CaptureContext;
use attemptdb_core::event::Provider;
use attemptdb_core::{Event, EventId, EventKind, Timestamp};
use serde_json::{Map, Value, json};

pub const VERSION: &str = "otel-json-v1";
pub const MAX_RECORDS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Logs,
    Metrics,
    Traces,
}
impl Signal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Logs => "logs",
            Self::Metrics => "metrics",
            Self::Traces => "traces",
        }
    }
}

#[derive(Debug, Default)]
pub struct Batch {
    pub events: Vec<Event>,
    pub rejected: usize,
}

/// Decode a single OTLP request, preserving each sample's native timestamp,
/// temporality and identity. Replaying the same record is idempotent.
pub fn normalise(
    ctx: &CaptureContext,
    provider: Provider,
    signal: Signal,
    payload: &Value,
) -> Result<Batch, String> {
    let (resources, scopes, records) = match signal {
        Signal::Logs => ("resourceLogs", "scopeLogs", "logRecords"),
        Signal::Metrics => ("resourceMetrics", "scopeMetrics", "metrics"),
        Signal::Traces => ("resourceSpans", "scopeSpans", "spans"),
    };
    let resources = payload
        .get(resources)
        .and_then(Value::as_array)
        .ok_or("invalid OTLP resource array")?;
    let mut batch = Batch::default();
    for resource in resources {
        let ra = attributes(&resource["resource"]["attributes"]);
        let Some(groups) = resource.get(scopes).and_then(Value::as_array) else {
            return Err("invalid OTLP scope array".into());
        };
        for scope in groups {
            let Some(rows) = scope.get(records).and_then(Value::as_array) else {
                return Err("invalid OTLP record array".into());
            };
            for row in rows {
                if signal == Signal::Metrics {
                    let mut recognised = false;
                    for kind in [
                        "sum",
                        "gauge",
                        "histogram",
                        "exponentialHistogram",
                        "summary",
                    ] {
                        let Some(points) = row[kind]["dataPoints"].as_array() else {
                            continue;
                        };
                        recognised = true;
                        for point in points {
                            let identity = json!({"resource":resource["resource"],"scope":scope["scope"],"name":row["name"],"unit":row["unit"],"kind":kind,"temporality":row[kind]["aggregationTemporality"],"point":point});
                            append(
                                &mut batch,
                                make_event(
                                    ctx,
                                    &provider,
                                    signal,
                                    &ra,
                                    point,
                                    &identity,
                                    Some((row, kind)),
                                ),
                            )?;
                        }
                    }
                    if !recognised {
                        append(&mut batch, None)?;
                    }
                } else {
                    let identity = json!({"resource":resource["resource"],"scope":scope["scope"],"record":row});
                    append(
                        &mut batch,
                        make_event(ctx, &provider, signal, &ra, row, &identity, None),
                    )?;
                    if signal == Signal::Traces
                        && let Some(events) = row["events"].as_array()
                    {
                        // Codex embeds structured API observations as span
                        // events as well as exporting logs. Preserve their
                        // explicit context without guessing from a time window.
                        let mut parent_attrs = ra.clone();
                        parent_attrs.extend(attributes(&row["attributes"]));
                        for child in events {
                            let mut record = child.clone();
                            if let Some(object) = record.as_object_mut() {
                                object.insert(
                                    "startTimeUnixNano".into(),
                                    child["timeUnixNano"].clone(),
                                );
                                for key in ["traceId", "spanId", "parentSpanId"] {
                                    object.insert(key.into(), row[key].clone());
                                }
                            }
                            let identity = json!({"resource":resource["resource"],"scope":scope["scope"],"traceId":row["traceId"],"spanId":row["spanId"],"parent_attributes":row["attributes"],"span_event":child});
                            let event = make_event(
                                ctx,
                                &provider,
                                signal,
                                &parent_attrs,
                                &record,
                                &identity,
                                None,
                            )
                            .map(|mut e| {
                                e.attrs
                                    .insert("x_otel_record_type".into(), json!("span_event"));
                                e
                            });
                            append(&mut batch, event)?;
                        }
                    }
                }
            }
        }
    }
    Ok(batch)
}

fn append(batch: &mut Batch, event: Option<Event>) -> Result<(), String> {
    if batch.events.len() + batch.rejected >= MAX_RECORDS {
        return Err("too many OTLP records".into());
    }
    if let Some(event) = event {
        batch.events.push(event);
    } else {
        batch.rejected += 1;
    }
    Ok(())
}

fn attributes(value: &Value) -> Map<String, Value> {
    let mut result = Map::new();
    if let Some(rows) = value.as_array() {
        for row in rows {
            if let Some(key) = row["key"].as_str().filter(|k| k.len() <= 128)
                && let Some(value) = any_value(&row["value"])
            {
                result.insert(key.to_owned(), value);
            }
        }
    }
    result
}

fn any_value(value: &Value) -> Option<Value> {
    for key in ["stringValue", "boolValue", "intValue", "doubleValue"] {
        if let Some(v) = value.get(key) {
            return Some(v.clone());
        }
    }
    None
}

fn number(value: &Value) -> Option<Value> {
    if value.is_number() {
        return value
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map(|_| value.clone());
    }
    let s = value.as_str()?;
    if let Ok(n) = s.parse::<u64>() {
        return Some(json!(n));
    }
    let n = s.parse::<f64>().ok()?;
    (n.is_finite() && n >= 0.0).then(|| json!(n))
}

fn timestamp(row: &Value, signal: Signal, attrs: &Map<String, Value>) -> Option<Timestamp> {
    let key = if signal == Signal::Traces {
        "startTimeUnixNano"
    } else {
        "timeUnixNano"
    };
    let nano = |v: &Value| {
        v.as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| v.as_u64())
            .filter(|n| *n > 0)
            .and_then(|n| i64::try_from(n / 1000).ok())
            .map(Timestamp::from_micros)
    };
    row.get(key)
        .and_then(nano)
        // OTLP uses zero for an unspecified timestamp. Codex 0.153 exports
        // that form with a real event.timestamp / observedTimeUnixNano.
        .or_else(|| {
            attrs
                .get("event.timestamp")
                .and_then(Value::as_str)
                .and_then(Timestamp::parse)
                .filter(|t| t.as_micros() > 0)
        })
        .or_else(|| row.get("observedTimeUnixNano").and_then(nano))
}

fn identifier(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| {
        !s.is_empty()
            && s.len() <= 128
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:/-[]".contains(&b))
    })
}

fn text_attr<'a>(attrs: &'a Map<String, Value>, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|k| attrs.get(*k).and_then(identifier))
}

fn put_number(event: &mut Event, attrs: &Map<String, Value>, dest: &str, names: &[&str]) {
    if let Some(v) = names.iter().find_map(|k| attrs.get(*k).and_then(number)) {
        event.attrs.insert(dest.into(), v);
    }
}

/// Claude Code truncates exported text at 60 KB; this is the ceiling on what
/// one record may carry into `content`, counted in characters so a multibyte
/// message is never cut inside a code point.
pub const MAX_MESSAGE_CHARS: usize = 65_536;

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn put_text(event: &mut Event, attrs: &Map<String, Value>, dest: &str, names: &[&str]) {
    if let Some(v) = text_attr(attrs, names) {
        event.attrs.insert(dest.into(), json!(v));
    }
}

fn make_event(
    ctx: &CaptureContext,
    provider: &Provider,
    signal: Signal,
    resource: &Map<String, Value>,
    row: &Value,
    identity: &Value,
    metric: Option<(&Value, &str)>,
) -> Option<Event> {
    let mut attrs = resource.clone();
    attrs.extend(attributes(&row["attributes"]));
    let observed = timestamp(row, signal, &attrs)?;
    let session = text_attr(
        &attrs,
        &[
            "session.id",
            "conversation.id",
            "session_id",
            "conversation_id",
            "thread_id",
        ],
    );
    // Metrics without a session are deliberately not assigned to whichever
    // agent happened to be active. They remain provider-level observations.
    let missing_session = format!("otel-unattributed-{}", provider.as_str());
    let session = session.unwrap_or(&missing_session);
    let name = metric
        .and_then(|(m, _)| identifier(&m["name"]))
        .or_else(|| identifier(&row["eventName"]))
        .or_else(|| text_attr(&attrs, &["event.name", "event_name", "name"]))
        .or_else(|| identifier(&row["name"]))
        .or_else(|| identifier(&row["body"]["stringValue"]))
        .unwrap_or("otel.observation");
    let mut event = Event::new(
        ctx.device_id,
        provider.clone(),
        name,
        EventKind::Unknown,
        ctx.project.clone(),
        session,
        ctx.capture_mode,
        VERSION,
    );
    // Hash the complete original record, including its source timestamp. No
    // raw content appears in the derived id or in metadata.
    event.event_id = EventId::derive(&[
        "otel-json-v1",
        &ctx.device_id.to_string(),
        provider.as_str(),
        signal.as_str(),
        &identity.to_string(),
    ]);
    event.observed_at = observed;
    event.captured_at = ctx.captured_at;
    event.hook_version = None;
    event.provider_version =
        text_attr(&attrs, &["service.version", "app.version"]).map(str::to_owned);
    event.agent.model = text_attr(
        &attrs,
        &["model", "gen_ai.request.model", "gen_ai.response.model"],
    )
    .map(str::to_owned);
    event.attrs.insert("source".into(), json!("otel"));
    event
        .attrs
        .insert("x_otel_signal".into(), json!(signal.as_str()));
    event.attrs.insert(
        "x_otel_record_type".into(),
        json!(match signal {
            Signal::Logs => "log_record",
            Signal::Metrics => "metric_sample",
            Signal::Traces => "span",
        }),
    );
    event.attrs.insert(
        "x_otel_session_attributed".into(),
        json!(!session.starts_with("otel-unattributed-")),
    );
    for (dest, names) in [
        (
            "x_otel_input_tokens",
            &[
                "input_tokens",
                "input_token_count",
                "gen_ai.usage.input_tokens",
                "codex.turn.token_usage.input_tokens",
            ][..],
        ),
        (
            "x_otel_output_tokens",
            &[
                "output_tokens",
                "output_token_count",
                "gen_ai.usage.output_tokens",
                "codex.turn.token_usage.output_tokens",
            ][..],
        ),
        (
            "x_otel_cache_read_tokens",
            &[
                "cache_read_tokens",
                "cached_input_tokens",
                "cached_token_count",
                "gen_ai.usage.cache_read.input_tokens",
                "codex.turn.token_usage.cached_input_tokens",
            ][..],
        ),
        (
            "x_otel_cache_creation_tokens",
            &[
                "cache_creation_tokens",
                "cache_creation_input_tokens",
                "cache_write_token_count",
                "gen_ai.usage.cache_write.input_tokens",
                "codex.turn.token_usage.cache_write_input_tokens",
            ][..],
        ),
        (
            "x_otel_reasoning_tokens",
            &[
                "reasoning_tokens",
                "reasoning_output_tokens",
                "reasoning_output_token_count",
                "reasoning_token_count",
                "codex.usage.reasoning_output_tokens",
                "codex.turn.token_usage.reasoning_output_tokens",
            ][..],
        ),
        (
            "x_otel_total_tokens",
            &[
                "total_tokens",
                "total_token_count",
                "codex.usage.total_tokens",
                "codex.turn.token_usage.total_tokens",
            ][..],
        ),
        ("x_otel_tool_tokens", &["tool_token_count"][..]),
        ("x_otel_ttft_ms", &["ttft_ms"][..]),
        ("x_otel_cost_usd", &["cost_usd"][..]),
        ("x_otel_cost_usd_micros", &["cost_usd_micros"][..]),
        (
            "x_otel_duration_ms",
            &["duration_ms", "duration", "latency_ms"][..],
        ),
        (
            "x_otel_status_code",
            &[
                "status_code",
                "http.status_code",
                "http.response.status_code",
            ][..],
        ),
        ("x_otel_attempt", &["attempt", "retry_count"][..]),
        ("x_otel_event_sequence", &["event.sequence"][..]),
        // The size of what was said, whether or not the text itself was
        // exported: a prompt or reply's length is metadata.
        ("x_otel_prompt_chars", &["prompt_length"][..]),
        ("x_otel_response_chars", &["response_length"][..]),
    ] {
        put_number(&mut event, &attrs, dest, names);
    }
    // What was said. Claude Code exports the user's prompt on `user_prompt`
    // and its own reply on `assistant_response` (Codex: `codex.user_prompt`)
    // only when the provider is configured to (`OTEL_LOG_USER_PROMPTS`,
    // `OTEL_LOG_ASSISTANT_RESPONSES`, `log_user_prompt`); otherwise the field
    // reads `<REDACTED>`. The text is content, never metadata: it lives in
    // `content` under the capture mode like a hook's prompt, and the
    // `messages` sync profile is what lets it leave the device.
    if signal == Signal::Logs && ctx.capture_mode.persists_content_locally() {
        let spoken = |key: &str| {
            attrs
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "<REDACTED>")
                .map(|s| truncate_chars(s, MAX_MESSAGE_CHARS))
        };
        let mut content = attemptdb_core::event::EventContent::default();
        match name {
            "user_prompt" | "claude_code.user_prompt" | "codex.user_prompt" => {
                content.prompt = spoken("prompt");
            }
            "assistant_response" | "claude_code.assistant_response" | "codex.assistant_response" => {
                content.message = spoken("response");
            }
            _ => {}
        }
        if !content.is_empty() {
            event.content = Some(content);
        }
    }
    for (dest, names) in [
        ("x_otel_request_id", &["request_id", "response_id"][..]),
        ("x_otel_client_request_id", &["client_request_id"][..]),
        ("x_otel_tool_call_id", &["tool_call_id", "call_id"][..]),
        (
            "x_otel_event_kind",
            &["event.kind", "event_kind", "kind"][..],
        ),
        (
            "x_otel_turn_id",
            &["turn.id", "turn_id", "submission.id"][..],
        ),
        (
            "x_otel_reasoning_effort",
            &[
                "reasoning_effort",
                "model_reasoning_effort",
                "codex.request.reasoning_effort",
            ][..],
        ),
        ("x_otel_query_source", &["query_source"][..]),
        ("x_otel_tool_name", &["tool_name"][..]),
    ] {
        put_text(&mut event, &attrs, dest, names);
    }
    if let Some(success) = attrs.get("success").and_then(|v| {
        v.as_bool()
            .or_else(|| v.as_str().and_then(|s| s.parse::<bool>().ok()))
    }) {
        event.attrs.insert("x_otel_success".into(), json!(success));
    }
    for (src, dest, len) in [
        ("traceId", "x_otel_trace_id", 32),
        ("spanId", "x_otel_span_id", 16),
        ("parentSpanId", "x_otel_parent_span_id", 16),
    ] {
        if let Some(s) = row[src]
            .as_str()
            .filter(|s| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            event
                .attrs
                .insert(dest.into(), json!(s.to_ascii_lowercase()));
        }
    }
    if signal == Signal::Traces {
        let end = row["endTimeUnixNano"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| row["endTimeUnixNano"].as_u64());
        if let Some(end) = end.filter(|end| *end / 1000 >= observed.as_micros() as u64) {
            event.attrs.insert(
                "x_otel_duration_ms".into(),
                json!((end / 1000 - observed.as_micros() as u64) / 1000),
            );
        }
        if let Some(code) =
            row["status"]["code"]
                .as_u64()
                .or_else(|| match row["status"]["code"].as_str() {
                    Some("STATUS_CODE_OK") => Some(1),
                    Some("STATUS_CODE_ERROR") => Some(2),
                    Some("STATUS_CODE_UNSET") => Some(0),
                    _ => None,
                })
        {
            event
                .attrs
                .insert("x_otel_span_status_code".into(), json!(code));
        }
    }
    if let Some((metric, kind)) = metric {
        event.attrs.insert("x_otel_metric_type".into(), json!(kind));
        if let Some(unit) = metric["unit"].as_str().filter(|s| {
            s.len() <= 32
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"{}./_%".contains(&b))
        }) {
            event.attrs.insert("x_otel_unit".into(), json!(unit));
        }
        if let Some(v) = metric[kind].get("aggregationTemporality")
            && let Some(n) = number(v).or_else(|| match v.as_str() {
                Some("AGGREGATION_TEMPORALITY_DELTA") => Some(json!(1)),
                Some("AGGREGATION_TEMPORALITY_CUMULATIVE") => Some(json!(2)),
                Some("AGGREGATION_TEMPORALITY_UNSPECIFIED") => Some(json!(0)),
                _ => None,
            })
        {
            event.attrs.insert("x_otel_temporality".into(), n);
        }
        if let Some(v) = metric[kind]["isMonotonic"].as_bool() {
            event.attrs.insert("x_otel_monotonic".into(), json!(v));
        }
        for (source, dest) in [
            ("asInt", "x_otel_value"),
            ("asDouble", "x_otel_value"),
            ("count", "x_otel_count"),
            ("sum", "x_otel_sum"),
            ("min", "x_otel_min"),
            ("max", "x_otel_max"),
            ("startTimeUnixNano", "x_otel_start_time_unix_nano"),
        ] {
            if let Some(n) = row.get(source).and_then(number) {
                event.attrs.insert(dest.into(), n);
            }
        }
        for (src, dest) in [
            ("type", "x_otel_token_type"),
            ("token_type", "x_otel_token_type"),
        ] {
            put_text(&mut event, &attrs, dest, &[src]);
        }
        for (src, dest) in [
            ("explicitBounds", "x_otel_histogram_bounds"),
            ("bucketCounts", "x_otel_histogram_counts"),
        ] {
            if let Some(values) = row[src].as_array().filter(|v| v.len() <= 128) {
                let nums: Option<Vec<_>> = values.iter().map(number).collect();
                if let Some(nums) = nums {
                    event.attrs.insert(dest.into(), json!(nums));
                }
            }
        }
    }
    if ctx.capture_mode != attemptdb_core::CaptureMode::MetadataOnly {
        event.raw = Some(identity.clone());
    }
    attemptdb_core::attrs::sanitise(&mut event.attrs);
    Some(event)
}
