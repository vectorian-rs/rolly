use std::time::SystemTime;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::Subscriber;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use std::sync::Arc;

use crate::constants::fields;
use crate::otlp_log::{encode_export_logs_request, LogData, SeverityNumber};
use crate::otlp_trace::{
    encode_export_trace_request, AnyValue, KeyValue, SpanData, SpanKind, SpanStatus, StatusCode,
};
use crate::trace_id::{generate_span_id, generate_trace_id};

// --- Span extensions ---

struct SpanTiming {
    start_nanos: u64,
}

/// Span context stored in tracing extensions. Public so PropagationLayer can read it.
pub struct SpanFields {
    pub(crate) attrs: Vec<KeyValue>,
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub(crate) parent_span_id: [u8; 8],
    /// Whether this span (and its descendants) should be exported.
    pub(crate) sampled: bool,
    pub(crate) span_kind: SpanKind,
    pub(crate) status_code: StatusCode,
    pub(crate) status_message: Option<String>,
}

// --- Shared visitor ---

/// Visitor that collects tracing fields into KeyValue pairs.
/// Used for both span attributes and event fields.
struct FieldCollector {
    attrs: Vec<KeyValue>,
    trace_id: Option<[u8; 16]>,
    message: Option<String>,
    span_kind: Option<SpanKind>,
    status_code: Option<StatusCode>,
    status_message: Option<String>,
}

impl FieldCollector {
    fn new() -> Self {
        Self {
            attrs: Vec::new(),
            trace_id: None,
            message: None,
            span_kind: None,
            status_code: None,
            status_message: None,
        }
    }

    /// Shared field-matching logic used by both record_str and record_debug.
    fn record_field(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => {
                self.message = Some(value.to_string());
            }
            // Kept as a span attribute so trace_id is visible in backends
            // that display raw attributes (e.g. Jaeger, Grafana Tempo).
            fields::TRACE_ID => {
                if let Ok(bytes) = hex_to_bytes_16(value) {
                    self.trace_id = Some(bytes);
                }
                self.attrs.push(KeyValue {
                    key: field.name().to_string(),
                    value: AnyValue::String(value.to_string()),
                });
            }
            // otel.* fields are semantic control signals consumed by OtlpLayer;
            // they map to OTLP Span fields, not attributes.
            fields::OTEL_KIND => {
                self.span_kind = parse_span_kind(value);
            }
            fields::OTEL_STATUS_CODE => {
                self.status_code = parse_status_code(value);
            }
            fields::OTEL_STATUS_MESSAGE => {
                self.status_message = Some(value.to_string());
            }
            _ => {
                self.attrs.push(KeyValue {
                    key: field.name().to_string(),
                    value: AnyValue::String(value.to_string()),
                });
            }
        }
    }
}

impl Visit for FieldCollector {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{:?}", value);
        // Strip surrounding quotes from Debug output (e.g. "\"server\"" → "server")
        // so that otel.* semantic fields work with %value and Display wrappers.
        let stripped = s
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(&s);
        self.record_field(field, stripped);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_field(field, value);
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.attrs.push(KeyValue {
            key: field.name().to_string(),
            value: AnyValue::Int(value),
        });
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.attrs.push(KeyValue {
            key: field.name().to_string(),
            value: AnyValue::Int(value as i64),
        });
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.attrs.push(KeyValue {
            key: field.name().to_string(),
            value: AnyValue::Bool(value),
        });
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.attrs.push(KeyValue {
            key: field.name().to_string(),
            value: AnyValue::Double(value),
        });
    }
}

// --- Helpers ---

pub(crate) fn hex_to_bytes_16(s: &str) -> Result<[u8; 16], ()> {
    if s.len() != 32 {
        return Err(());
    }
    let mut out = [0u8; 16];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0]).ok_or(())?;
        let lo = hex_nibble(chunk[1]).ok_or(())?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_span_kind(s: &str) -> Option<SpanKind> {
    match s {
        "server" | "SERVER" => Some(SpanKind::Server),
        "client" | "CLIENT" => Some(SpanKind::Client),
        "producer" | "PRODUCER" => Some(SpanKind::Producer),
        "consumer" | "CONSUMER" => Some(SpanKind::Consumer),
        "internal" | "INTERNAL" => Some(SpanKind::Internal),
        _ => None,
    }
}

fn parse_status_code(s: &str) -> Option<StatusCode> {
    match s {
        "ok" | "OK" => Some(StatusCode::Ok),
        "error" | "ERROR" => Some(StatusCode::Error),
        "unset" | "UNSET" => Some(StatusCode::Unset),
        _ => None,
    }
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn level_to_severity(level: &tracing::Level) -> SeverityNumber {
    match *level {
        tracing::Level::TRACE => SeverityNumber::Trace,
        tracing::Level::DEBUG => SeverityNumber::Debug,
        tracing::Level::INFO => SeverityNumber::Info,
        tracing::Level::WARN => SeverityNumber::Warn,
        tracing::Level::ERROR => SeverityNumber::Error,
    }
}

/// Deterministic sampling decision based on trace_id.
/// Uses the first 8 bytes of trace_id as a u64, maps to [0.0, 1.0) and compares
/// against the sampling rate. The same trace_id always produces the same decision.
pub fn should_sample(trace_id: [u8; 16], sampling_rate: f64) -> bool {
    if sampling_rate >= 1.0 {
        return true;
    }
    if sampling_rate <= 0.0 {
        return false;
    }
    let hash = u64::from_le_bytes(trace_id[..8].try_into().unwrap());
    let normalized = (hash as f64) / (u64::MAX as f64);
    normalized < sampling_rate
}

// --- Layer ---

/// Configuration for constructing an `OtlpLayer`.
pub struct OtlpLayerConfig<'a> {
    pub sink: Arc<dyn crate::TelemetrySink>,
    pub service_name: &'a str,
    pub service_version: &'a str,
    pub environment: &'a str,
    pub resource_attributes: &'a [(String, String)],
    pub export_traces: bool,
    pub export_logs: bool,
    pub sampling_rate: f64,
}

/// Custom tracing Layer that encodes spans/events as OTLP protobuf and sends via TelemetrySink.
pub struct OtlpLayer {
    sink: Arc<dyn crate::TelemetrySink>,
    resource_attrs: Vec<KeyValue>,
    scope_name: String,
    scope_version: String,
    export_traces: bool,
    export_logs: bool,
    /// Sampling rate: 1.0 = all, 0.0 = none. Deterministic based on trace_id.
    sampling_rate: f64,
}

impl OtlpLayer {
    pub fn new(config: OtlpLayerConfig<'_>) -> Self {
        let mut resource_attrs = vec![
            KeyValue {
                key: "service.name".to_string(),
                value: AnyValue::String(config.service_name.to_string()),
            },
            KeyValue {
                key: "service.version".to_string(),
                value: AnyValue::String(config.service_version.to_string()),
            },
            KeyValue {
                key: "deployment.environment".to_string(),
                value: AnyValue::String(config.environment.to_string()),
            },
        ];
        for (k, v) in config.resource_attributes {
            resource_attrs.push(KeyValue {
                key: k.clone(),
                value: AnyValue::String(v.clone()),
            });
        }
        Self {
            sink: config.sink,
            resource_attrs,
            scope_name: "rolly".to_string(),
            scope_version: env!("CARGO_PKG_VERSION").to_string(),
            export_traces: config.export_traces,
            export_logs: config.export_logs,
            sampling_rate: config.sampling_rate,
        }
    }
}

impl<S> Layer<S> for OtlpLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let span = ctx.span(id).expect("span not found");

        let mut visitor = FieldCollector::new();
        attrs.record(&mut visitor);

        let span_id = generate_span_id();

        let parent = span.parent();
        let parent_fields = parent.as_ref().and_then(|p| {
            p.extensions()
                .get::<SpanFields>()
                .map(|f| (f.trace_id, f.span_id, f.sampled))
        });

        // Trace ID resolution order:
        // 1. Inherit from parent span (child spans always share the parent's trace)
        // 2. Use explicit trace_id attribute from span fields
        // 3. Generate a new random trace ID for root spans
        let trace_id = match parent_fields {
            Some((parent_trace_id, _, _)) => parent_trace_id,
            None => visitor.trace_id.unwrap_or_else(|| generate_trace_id(None)),
        };

        let parent_span_id = parent_fields.map(|(_, id, _)| id).unwrap_or([0u8; 8]);

        // Inherit sampling decision from parent, or make a new one for root spans.
        let sampled = match parent_fields {
            Some((_, _, parent_sampled)) => parent_sampled,
            None => should_sample(trace_id, self.sampling_rate),
        };

        let mut ext = span.extensions_mut();
        ext.insert(SpanTiming {
            start_nanos: now_nanos(),
        });
        ext.insert(SpanFields {
            attrs: visitor.attrs,
            trace_id,
            span_id,
            parent_span_id,
            sampled,
            span_kind: visitor.span_kind.unwrap_or(SpanKind::Internal),
            status_code: visitor.status_code.unwrap_or(StatusCode::Unset),
            status_message: visitor.status_message,
        });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let span = ctx.span(id).expect("span not found");
        let mut ext = span.extensions_mut();
        if let Some(fields) = ext.get_mut::<SpanFields>() {
            let mut visitor = FieldCollector::new();
            values.record(&mut visitor);
            fields.attrs.extend(visitor.attrs);
            if let Some(kind) = visitor.span_kind {
                fields.span_kind = kind;
            }
            if let Some(code) = visitor.status_code {
                fields.status_code = code;
            }
            if let Some(msg) = visitor.status_message {
                fields.status_message = Some(msg);
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        if !self.export_logs {
            return;
        }

        let mut visitor = FieldCollector::new();
        event.record(&mut visitor);

        let (trace_id, span_id, sampled) = ctx
            .current_span()
            .id()
            .and_then(|id| {
                ctx.span(id).and_then(|s| {
                    s.extensions()
                        .get::<SpanFields>()
                        .map(|f| (f.trace_id, f.span_id, f.sampled))
                })
            })
            .unwrap_or(([0u8; 16], [0u8; 8], true));

        // Suppress log events for sampled-out traces
        if !sampled {
            return;
        }

        let severity = level_to_severity(event.metadata().level());
        let log = LogData {
            time_unix_nano: now_nanos(),
            severity_number: severity,
            severity_text: event.metadata().level().to_string(),
            body: AnyValue::String(visitor.message.unwrap_or_default()),
            attributes: visitor.attrs,
            trace_id,
            span_id,
        };

        let data = encode_export_logs_request(
            &self.resource_attrs,
            &self.scope_name,
            &self.scope_version,
            &[log],
        );
        self.sink.send_logs(data);
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        if !self.export_traces {
            return;
        }

        let span = ctx.span(&id).expect("span not found");
        let ext = span.extensions();

        let (start_nanos, attrs, trace_id, span_id, parent_span_id, span_kind, status) = {
            let timing = match ext.get::<SpanTiming>() {
                Some(t) => t,
                None => return,
            };
            let fields = match ext.get::<SpanFields>() {
                Some(f) => f,
                None => return,
            };

            // Sampled-out spans are not exported
            if !fields.sampled {
                return;
            }

            let status = match fields.status_code {
                StatusCode::Unset => None,
                code => Some(SpanStatus {
                    message: fields.status_message.clone().unwrap_or_default(),
                    code,
                }),
            };

            (
                timing.start_nanos,
                fields.attrs.clone(),
                fields.trace_id,
                fields.span_id,
                fields.parent_span_id,
                fields.span_kind,
                status,
            )
        };

        let end_nanos = now_nanos();

        let span_data = SpanData {
            trace_id,
            span_id,
            parent_span_id,
            name: span.name().to_string(),
            kind: span_kind,
            start_time_unix_nano: start_nanos,
            end_time_unix_nano: end_nanos,
            attributes: attrs,
            status,
        };

        let data = encode_export_trace_request(
            &self.resource_attrs,
            &self.scope_name,
            &self.scope_version,
            &[span_data],
        );
        self.sink.send_traces(data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    // ─── Mock TelemetrySink backed by std::sync::mpsc ───

    #[derive(Debug)]
    #[allow(dead_code)]
    enum MockMessage {
        Traces(Vec<u8>),
        Logs(Vec<u8>),
        Metrics(Vec<u8>),
    }

    struct MockSink {
        tx: std::sync::mpsc::Sender<MockMessage>,
    }

    impl crate::TelemetrySink for MockSink {
        fn send_traces(&self, data: Vec<u8>) {
            let _ = self.tx.send(MockMessage::Traces(data));
        }
        fn send_logs(&self, data: Vec<u8>) {
            let _ = self.tx.send(MockMessage::Logs(data));
        }
        fn send_metrics(&self, data: Vec<u8>) {
            let _ = self.tx.send(MockMessage::Metrics(data));
        }
    }

    fn mock_sink() -> (
        Arc<dyn crate::TelemetrySink>,
        std::sync::mpsc::Receiver<MockMessage>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Arc::new(MockSink { tx }), rx)
    }

    // ─── Tests ───

    #[test]
    fn otlp_layer_constructs_without_panic() {
        let (sink, _rx) = mock_sink();
        let _layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: true,
            sampling_rate: 1.0,
        });
    }

    #[test]
    fn custom_resource_attributes_appear_in_trace() {
        let (sink, rx) = mock_sink();
        let custom_attrs = vec![
            ("team".to_string(), "platform".to_string()),
            ("region".to_string(), "us-east-1".to_string()),
        ];
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &custom_attrs,
            export_traces: true,
            export_logs: true,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        {
            let span = tracing::info_span!("attr-span");
            let _enter = span.enter();
        }

        let msg = rx.recv().expect("should receive trace");
        match msg {
            MockMessage::Traces(data) => {
                assert!(
                    data.windows(4).any(|w| w == b"team"),
                    "custom attribute key 'team' not found in protobuf"
                );
                assert!(
                    data.windows(8).any(|w| w == b"platform"),
                    "custom attribute value 'platform' not found in protobuf"
                );
                assert!(
                    data.windows(6).any(|w| w == b"region"),
                    "custom attribute key 'region' not found in protobuf"
                );
                assert!(
                    data.windows(9).any(|w| w == b"us-east-1"),
                    "custom attribute value 'us-east-1' not found in protobuf"
                );
            }
            other => panic!("expected Traces, got {:?}", other),
        }
    }

    #[test]
    fn hex_to_bytes_16_valid() {
        let result = hex_to_bytes_16("0102030405060708090a0b0c0d0e0f10");
        assert_eq!(
            result,
            Ok([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
        );
    }

    #[test]
    fn hex_to_bytes_16_wrong_length() {
        assert!(hex_to_bytes_16("0102").is_err());
    }

    #[test]
    fn hex_to_bytes_16_invalid_chars() {
        assert!(hex_to_bytes_16("zz02030405060708090a0b0c0d0e0f10").is_err());
    }

    #[test]
    fn layer_captures_span_and_sends_trace_on_close() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: true,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let trace_id_hex = "0102030405060708090a0b0c0d0e0f10";
        let trace_id_bytes: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

        {
            let span = tracing::info_span!("test-span", trace_id = trace_id_hex);
            let _enter = span.enter();
        }

        let msg = rx.recv().expect("should receive trace message");
        match msg {
            MockMessage::Traces(data) => {
                assert!(
                    data.windows(16).any(|w| w == trace_id_bytes),
                    "trace_id not found in protobuf"
                );
                assert!(
                    data.windows(9).any(|w| w == b"test-span"),
                    "span name not found in protobuf"
                );
            }
            other => panic!("expected Traces, got {:?}", other),
        }
    }

    #[test]
    fn layer_captures_event_and_sends_log() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: true,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!("hello integration");

        let msg = rx.recv().expect("should receive log message");
        match msg {
            MockMessage::Logs(data) => {
                assert!(
                    data.windows(17).any(|w| w == b"hello integration"),
                    "log body not found in protobuf"
                );
            }
            other => panic!("expected Logs, got {:?}", other),
        }
    }

    #[test]
    fn layer_event_inside_span_carries_trace_context() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: true,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let trace_id_hex = "aabbccdd11223344aabbccdd11223344";
        let trace_id_bytes: [u8; 16] = [
            0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22, 0x33, 0x44, 0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22,
            0x33, 0x44,
        ];

        {
            let span = tracing::info_span!("outer", trace_id = trace_id_hex);
            let _enter = span.enter();
            tracing::info!("inner event");
        }

        // First message: the log event
        let msg = rx.recv().expect("should receive log");
        match msg {
            MockMessage::Logs(data) => {
                assert!(
                    data.windows(16).any(|w| w == trace_id_bytes),
                    "trace_id not propagated to log"
                );
            }
            other => panic!("expected Logs, got {:?}", other),
        }

        // Second message: the span trace
        let msg = rx.recv().expect("should receive trace");
        assert!(matches!(msg, MockMessage::Traces(_)));
    }

    #[test]
    fn field_collector_handles_all_types() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: true,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        {
            let span = tracing::info_span!(
                "typed-span",
                str_field = "hello",
                i64_field = 42i64,
                u64_field = 100u64,
                bool_field = true,
                f64_field = 1.5f64,
            );
            let _enter = span.enter();
        }

        let msg = rx.recv().expect("should receive trace");
        match msg {
            MockMessage::Traces(data) => {
                for name in &[
                    "str_field",
                    "i64_field",
                    "u64_field",
                    "bool_field",
                    "f64_field",
                ] {
                    assert!(
                        data.windows(name.len()).any(|w| w == name.as_bytes()),
                        "field '{}' not found in protobuf",
                        name
                    );
                }
                assert!(
                    data.windows(5).any(|w| w == b"hello"),
                    "string value 'hello' not found"
                );
            }
            other => panic!("expected Traces, got {:?}", other),
        }
    }

    #[test]
    fn parent_span_id_propagated_to_child() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: true,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        {
            let parent = tracing::info_span!("parent-span");
            let _parent_enter = parent.enter();
            {
                let child = tracing::info_span!("child-span");
                let _child_enter = child.enter();
            }
        }

        let msg1 = rx.recv().expect("should receive child trace");
        match &msg1 {
            MockMessage::Traces(data) => {
                assert!(
                    data.windows(10).any(|w| w == b"child-span"),
                    "child span name not found"
                );
            }
            other => panic!("expected Traces for child, got {:?}", other),
        }

        let msg2 = rx.recv().expect("should receive parent trace");
        match &msg2 {
            MockMessage::Traces(data) => {
                assert!(
                    data.windows(11).any(|w| w == b"parent-span"),
                    "parent span name not found"
                );
            }
            other => panic!("expected Traces for parent, got {:?}", other),
        }
    }

    #[test]
    fn layer_skips_log_export_when_disabled() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: false,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!("should not export");

        {
            let span = tracing::info_span!("traced-span");
            let _enter = span.enter();
        }

        let msg = rx.recv().expect("should receive trace");
        assert!(
            matches!(msg, MockMessage::Traces(_)),
            "expected Traces, got {:?}",
            msg
        );

        assert!(rx.try_recv().is_err(), "expected no more messages");
    }

    #[test]
    fn layer_skips_trace_export_when_disabled() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: false,
            export_logs: true,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        {
            let span = tracing::info_span!("suppressed-span");
            let _enter = span.enter();
            tracing::info!("logged event");
        }

        let msg = rx.recv().expect("should receive log");
        assert!(
            matches!(msg, MockMessage::Logs(_)),
            "expected Logs, got {:?}",
            msg
        );

        let extra = rx.recv_timeout(std::time::Duration::from_millis(100));
        assert!(extra.is_err(), "expected no trace message");
    }

    // --- Sampling tests ---

    #[test]
    fn should_sample_rate_1_always_samples() {
        for i in 0u8..=255 {
            let mut trace_id = [0u8; 16];
            trace_id[0] = i;
            assert!(should_sample(trace_id, 1.0));
        }
    }

    #[test]
    fn should_sample_rate_0_never_samples() {
        for i in 0u8..=255 {
            let mut trace_id = [0u8; 16];
            trace_id[0] = i;
            assert!(!should_sample(trace_id, 0.0));
        }
    }

    #[test]
    fn should_sample_is_deterministic() {
        let trace_id = [
            0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22, 0x33, 0x44, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let result1 = should_sample(trace_id, 0.5);
        let result2 = should_sample(trace_id, 0.5);
        assert_eq!(result1, result2, "same trace_id must produce same decision");
    }

    #[test]
    fn should_sample_respects_rate_approximately() {
        let mut sampled = 0u64;
        let total = 10_000u64;
        for i in 0..total {
            let hash = blake3::hash(&i.to_le_bytes());
            let mut trace_id = [0u8; 16];
            trace_id.copy_from_slice(&hash.as_bytes()[..16]);
            if should_sample(trace_id, 0.5) {
                sampled += 1;
            }
        }
        let ratio = sampled as f64 / total as f64;
        assert!(
            (0.45..=0.55).contains(&ratio),
            "expected ~50% sampled, got {:.1}%",
            ratio * 100.0
        );
    }

    #[test]
    fn sampling_rate_zero_drops_all_traces_and_logs() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: true,
            sampling_rate: 0.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        {
            let span = tracing::info_span!("sampled-out-span");
            let _enter = span.enter();
            tracing::info!("sampled-out-log");
        }

        assert!(
            rx.try_recv().is_err(),
            "expected no messages when sampling_rate=0.0"
        );
    }

    #[test]
    fn sampling_rate_one_exports_all() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: true,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let trace_id_hex = "0102030405060708090a0b0c0d0e0f10";
        {
            let span = tracing::info_span!("sampled-in-span", trace_id = trace_id_hex);
            let _enter = span.enter();
            tracing::info!("sampled-in-log");
        }

        let msg1 = rx.recv().expect("should receive log");
        assert!(matches!(msg1, MockMessage::Logs(_)));

        let msg2 = rx.recv().expect("should receive trace");
        assert!(matches!(msg2, MockMessage::Traces(_)));
    }

    #[test]
    fn child_spans_inherit_parent_sampling_decision() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: true,
            sampling_rate: 0.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        {
            let parent = tracing::info_span!("parent");
            let _p = parent.enter();
            {
                let child = tracing::info_span!("child");
                let _c = child.enter();
                tracing::info!("child-event");
            }
        }

        assert!(
            rx.try_recv().is_err(),
            "child spans and events should inherit parent's sampled-out decision"
        );
    }

    #[test]
    fn root_span_without_explicit_trace_id_gets_generated_nonzero_id() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: false,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        {
            let span = tracing::info_span!("root-no-trace-id");
            let _enter = span.enter();
        }

        let msg = rx.recv().expect("should receive trace");
        match msg {
            MockMessage::Traces(data) => {
                // The trace must not contain all-zero trace_id.
                // Protobuf field 1 (trace_id) is at the start of the Span message.
                // We check that the encoded output does NOT contain 16 consecutive zero bytes
                // at any trace_id position. A simpler check: the span name is present and
                // the data is non-trivially sized (generated ID is in there).
                assert!(
                    data.windows(16).any(|w| w == b"root-no-trace-id"),
                    "span name not found"
                );
                // All-zero trace_id [0u8;16] should NOT appear — the generated ID is random.
                let zero_trace = [0u8; 16];
                assert!(
                    !data.windows(16).any(|w| w == zero_trace),
                    "trace_id should not be all zeros for a root span"
                );
            }
            other => panic!("expected Traces, got {:?}", other),
        }
    }

    #[test]
    fn child_span_inherits_trace_id_from_parent() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: false,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let trace_id_hex = "aabbccdd11223344aabbccdd11223344";
        let trace_id_bytes: [u8; 16] = [
            0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22, 0x33, 0x44, 0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22,
            0x33, 0x44,
        ];

        {
            let parent = tracing::info_span!("parent", trace_id = trace_id_hex);
            let _p = parent.enter();
            {
                // Child has NO explicit trace_id — must inherit from parent
                let child = tracing::info_span!("child");
                let _c = child.enter();
            }
        }

        // First message: child span (closes first)
        let child_msg = rx.recv().expect("should receive child trace");
        match &child_msg {
            MockMessage::Traces(data) => {
                assert!(
                    data.windows(5).any(|w| w == b"child"),
                    "child span name not found"
                );
                assert!(
                    data.windows(16).any(|w| w == trace_id_bytes),
                    "child span must carry parent's trace_id"
                );
            }
            other => panic!("expected Traces for child, got {:?}", other),
        }

        // Second message: parent span
        let parent_msg = rx.recv().expect("should receive parent trace");
        match &parent_msg {
            MockMessage::Traces(data) => {
                assert!(
                    data.windows(6).any(|w| w == b"parent"),
                    "parent span name not found"
                );
                assert!(
                    data.windows(16).any(|w| w == trace_id_bytes),
                    "parent span must carry its own trace_id"
                );
            }
            other => panic!("expected Traces for parent, got {:?}", other),
        }
    }

    #[test]
    fn grandchild_inherits_trace_id_through_chain() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: false,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let trace_id_hex = "1111111122222222aaaaaaaa33333333";
        let trace_id_bytes: [u8; 16] = [
            0x11, 0x11, 0x11, 0x11, 0x22, 0x22, 0x22, 0x22, 0xaa, 0xaa, 0xaa, 0xaa, 0x33, 0x33,
            0x33, 0x33,
        ];

        {
            let root = tracing::info_span!("root", trace_id = trace_id_hex);
            let _r = root.enter();
            {
                let mid = tracing::info_span!("mid");
                let _m = mid.enter();
                {
                    let leaf = tracing::info_span!("leaf");
                    let _l = leaf.enter();
                }
            }
        }

        // All three spans must contain the same trace_id
        for expected_name in &[b"leaf" as &[u8], b"mid", b"root"] {
            let msg = rx.recv().expect("should receive trace");
            match &msg {
                MockMessage::Traces(data) => {
                    assert!(
                        data.windows(expected_name.len())
                            .any(|w| w == *expected_name),
                        "span name {:?} not found",
                        std::str::from_utf8(expected_name)
                    );
                    assert!(
                        data.windows(16).any(|w| w == trace_id_bytes),
                        "span {:?} must carry root's trace_id",
                        std::str::from_utf8(expected_name)
                    );
                }
                other => panic!("expected Traces, got {:?}", other),
            }
        }
    }

    #[test]
    fn span_kind_from_otel_kind_attr() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: false,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        {
            let span = tracing::info_span!("server-span", otel.kind = "server");
            let _enter = span.enter();
        }

        let msg = rx.recv().expect("should receive trace");
        match msg {
            MockMessage::Traces(data) => {
                // SpanKind::Server = 2, encoded as varint field 6: tag=0x30, value=0x02
                assert!(
                    data.windows(2).any(|w| w == [0x30, 0x02]),
                    "SpanKind::Server not found in protobuf"
                );
                // otel.kind must NOT appear in attributes
                assert!(
                    !data.windows(9).any(|w| w == b"otel.kind"),
                    "otel.kind should not leak into span attributes"
                );
            }
            other => panic!("expected Traces, got {:?}", other),
        }
    }

    #[test]
    fn span_status_from_otel_status_attrs() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: false,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        {
            let span = tracing::info_span!(
                "error-span",
                otel.status_code = "error",
                otel.status_message = "something broke"
            );
            let _enter = span.enter();
        }

        let msg = rx.recv().expect("should receive trace");
        match msg {
            MockMessage::Traces(data) => {
                // StatusCode::Error = 2, in Status message field 3: tag=0x18, value=0x02
                assert!(
                    data.windows(2).any(|w| w == [0x18, 0x02]),
                    "StatusCode::Error not found in protobuf"
                );
                assert!(
                    data.windows(15).any(|w| w == b"something broke"),
                    "status message not found in protobuf"
                );
                // otel.* must NOT appear in attributes
                assert!(
                    !data.windows(16).any(|w| w == b"otel.status_code"),
                    "otel.status_code should not leak into attributes"
                );
                assert!(
                    !data.windows(19).any(|w| w == b"otel.status_message"),
                    "otel.status_message should not leak into attributes"
                );
            }
            other => panic!("expected Traces, got {:?}", other),
        }
    }

    #[test]
    fn deferred_status_recording() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: false,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        {
            let span = tracing::info_span!(
                "deferred-span",
                otel.status_code = tracing::field::Empty,
                otel.status_message = tracing::field::Empty,
            );
            let _enter = span.enter();
            // Simulate setting status after work completes
            span.record("otel.status_code", "ok");
            span.record("otel.status_message", "all good");
        }

        let msg = rx.recv().expect("should receive trace");
        match msg {
            MockMessage::Traces(data) => {
                // StatusCode::Ok = 1, in Status message field 3: tag=0x18, value=0x01
                assert!(
                    data.windows(2).any(|w| w == [0x18, 0x01]),
                    "StatusCode::Ok not found in protobuf"
                );
                assert!(
                    data.windows(8).any(|w| w == b"all good"),
                    "status message 'all good' not found in protobuf"
                );
            }
            other => panic!("expected Traces, got {:?}", other),
        }
    }

    #[test]
    fn default_span_kind_is_internal_and_no_status() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: false,
            sampling_rate: 1.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        {
            let span = tracing::info_span!("plain-span");
            let _enter = span.enter();
        }

        let msg = rx.recv().expect("should receive trace");
        match msg {
            MockMessage::Traces(data) => {
                // SpanKind::Internal = 1, field 6: tag=0x30, value=0x01
                assert!(
                    data.windows(2).any(|w| w == [0x30, 0x01]),
                    "SpanKind::Internal should be the default"
                );
                // Status message text should not appear when status_code is Unset.
                // We can't reliably detect field-tag absence in raw protobuf
                // (0x7A appears in random data), but verifying no status-related
                // strings leak is sufficient since encode_span skips None status.
                assert!(
                    !data.windows(16).any(|w| w == b"otel.status_code"),
                    "otel.status_code should not appear in attributes"
                );
            }
            other => panic!("expected Traces, got {:?}", other),
        }
    }

    #[test]
    fn all_span_kind_variants_parse() {
        for (input, expected_tag) in [
            ("server", 0x02u8),
            ("client", 0x03),
            ("producer", 0x04),
            ("consumer", 0x05),
            ("internal", 0x01),
            ("SERVER", 0x02),
            ("CLIENT", 0x03),
        ] {
            let (sink, rx) = mock_sink();
            let layer = OtlpLayer::new(OtlpLayerConfig {
                sink,
                service_name: "test-svc",
                service_version: "0.0.1",
                environment: "test",
                resource_attributes: &[],
                export_traces: true,
                export_logs: false,
                sampling_rate: 1.0,
            });
            let subscriber = tracing_subscriber::registry().with(layer);
            let _guard = tracing::subscriber::set_default(subscriber);

            {
                let span = tracing::info_span!("kind-test", otel.kind = input);
                let _enter = span.enter();
            }

            let msg = rx.recv().expect("should receive trace");
            match msg {
                MockMessage::Traces(data) => {
                    assert!(
                        data.windows(2).any(|w| w == [0x30, expected_tag]),
                        "SpanKind for '{}' (expected tag byte 0x{:02x}) not found",
                        input,
                        expected_tag
                    );
                }
                other => panic!("expected Traces for '{}', got {:?}", input, other),
            }
        }
    }

    #[test]
    fn events_outside_spans_are_exported_regardless_of_sampling() {
        let (sink, rx) = mock_sink();
        let layer = OtlpLayer::new(OtlpLayerConfig {
            sink,
            service_name: "test-svc",
            service_version: "0.0.1",
            environment: "test",
            resource_attributes: &[],
            export_traces: true,
            export_logs: true,
            sampling_rate: 0.0,
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!("standalone-log");

        let msg = rx.recv().expect("standalone event should be exported");
        assert!(matches!(msg, MockMessage::Logs(_)));
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    #[kani::proof]
    fn should_sample_boundary() {
        let trace_id: [u8; 16] = kani::any();
        // rate >= 1.0 → always true
        assert!(should_sample(trace_id, 1.0));
        // rate <= 0.0 → always false
        assert!(!should_sample(trace_id, 0.0));
        // rate 0.5 → exercises the hash path; no panics
        let _ = should_sample(trace_id, 0.5);
    }

    #[kani::proof]
    fn hex_nibble_all_bytes() {
        let b: u8 = kani::any();
        let result = hex_nibble(b);
        if (b >= b'0' && b <= b'9') || (b >= b'a' && b <= b'f') || (b >= b'A' && b <= b'F') {
            assert!(result.is_some());
            assert!(result.unwrap() < 16);
        } else {
            assert!(result.is_none());
        }
    }

    #[kani::proof]
    #[kani::unwind(2)]
    fn hex_to_bytes_16_length_check() {
        let len: usize = kani::any();
        kani::assume(len <= 34);
        kani::assume(len != 32);
        let buf: [u8; 34] = [b'0'; 34];
        // SAFETY: all bytes are 0x30 ('0'), valid UTF-8
        let s = unsafe { core::str::from_utf8_unchecked(&buf[..len]) };
        assert!(hex_to_bytes_16(s).is_err());
    }
}
