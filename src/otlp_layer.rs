use std::time::SystemTime;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::Subscriber;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use crate::exporter::Exporter;
use crate::otlp_log::{encode_export_logs_request, LogData, SeverityNumber};
use crate::otlp_trace::{
    encode_export_trace_request, AnyValue, KeyValue, SpanData, SpanKind,
};
use crate::trace_id::generate_span_id;

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
}

// --- Shared visitor ---

/// Visitor that collects tracing fields into KeyValue pairs.
/// Used for both span attributes and event fields.
struct FieldCollector {
    attrs: Vec<KeyValue>,
    trace_id: Option<[u8; 16]>,
    message: Option<String>,
}

impl FieldCollector {
    fn new() -> Self {
        Self {
            attrs: Vec::new(),
            trace_id: None,
            message: None,
        }
    }
}

impl Visit for FieldCollector {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{:?}", value);
        if field.name() == "message" {
            self.message = Some(s);
        } else {
            self.attrs.push(KeyValue {
                key: field.name().to_string(),
                value: AnyValue::String(s),
            });
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            if field.name() == "trace_id" {
                if let Ok(bytes) = hex_to_bytes_16(value) {
                    self.trace_id = Some(bytes);
                }
            }
            self.attrs.push(KeyValue {
                key: field.name().to_string(),
                value: AnyValue::String(value.to_string()),
            });
        }
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

fn hex_to_bytes_16(s: &str) -> Result<[u8; 16], ()> {
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

// --- Layer ---

/// Custom tracing Layer that encodes spans/events as OTLP protobuf and sends to Exporter.
pub(crate) struct OtlpLayer {
    exporter: Exporter,
    resource_attrs: Vec<KeyValue>,
    scope_name: String,
    scope_version: String,
    export_traces: bool,
    export_logs: bool,
}

impl OtlpLayer {
    pub fn new(
        exporter: Exporter,
        service_name: &str,
        service_version: &str,
        environment: &str,
        export_traces: bool,
        export_logs: bool,
    ) -> Self {
        let resource_attrs = vec![
            KeyValue {
                key: "service.name".to_string(),
                value: AnyValue::String(service_name.to_string()),
            },
            KeyValue {
                key: "service.version".to_string(),
                value: AnyValue::String(service_version.to_string()),
            },
            KeyValue {
                key: "deployment.environment".to_string(),
                value: AnyValue::String(environment.to_string()),
            },
        ];
        Self {
            exporter,
            resource_attrs,
            scope_name: "pz-o11y".to_string(),
            scope_version: env!("CARGO_PKG_VERSION").to_string(),
            export_traces,
            export_logs,
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
        let trace_id = visitor.trace_id.unwrap_or([0u8; 16]);

        let parent_span_id = span
            .parent()
            .and_then(|p| p.extensions().get::<SpanFields>().map(|f| f.span_id))
            .unwrap_or([0u8; 8]);

        let mut ext = span.extensions_mut();
        ext.insert(SpanTiming {
            start_nanos: now_nanos(),
        });
        ext.insert(SpanFields {
            attrs: visitor.attrs,
            trace_id,
            span_id,
            parent_span_id,
        });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let span = ctx.span(id).expect("span not found");
        let mut ext = span.extensions_mut();
        if let Some(fields) = ext.get_mut::<SpanFields>() {
            let mut visitor = FieldCollector::new();
            values.record(&mut visitor);
            fields.attrs.extend(visitor.attrs);
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        if !self.export_logs {
            return;
        }

        let mut visitor = FieldCollector::new();
        event.record(&mut visitor);

        let (trace_id, span_id) = ctx
            .current_span()
            .id()
            .and_then(|id| {
                ctx.span(id)
                    .and_then(|s| {
                        s.extensions()
                            .get::<SpanFields>()
                            .map(|f| (f.trace_id, f.span_id))
                    })
            })
            .unwrap_or(([0u8; 16], [0u8; 8]));

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
        self.exporter.send_logs(data);
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        if !self.export_traces {
            return;
        }

        let span = ctx.span(&id).expect("span not found");
        let ext = span.extensions();

        let (start_nanos, attrs, trace_id, span_id, parent_span_id) = {
            let timing = match ext.get::<SpanTiming>() {
                Some(t) => t,
                None => return,
            };
            let fields = match ext.get::<SpanFields>() {
                Some(f) => f,
                None => return,
            };
            (
                timing.start_nanos,
                fields.attrs.clone(),
                fields.trace_id,
                fields.span_id,
                fields.parent_span_id,
            )
        };

        let end_nanos = now_nanos();

        let span_data = SpanData {
            trace_id,
            span_id,
            parent_span_id,
            name: span.name().to_string(),
            kind: SpanKind::Internal,
            start_time_unix_nano: start_nanos,
            end_time_unix_nano: end_nanos,
            attributes: attrs,
            status: None,
        };

        let data = encode_export_trace_request(
            &self.resource_attrs,
            &self.scope_name,
            &self.scope_version,
            &[span_data],
        );
        self.exporter.send_traces(data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exporter::{ExportMessage, ExporterConfig};
    use tracing_subscriber::layer::SubscriberExt;

    #[tokio::test]
    async fn otlp_layer_constructs_without_panic() {
        let exporter = Exporter::start(ExporterConfig {
            traces_url: Some("http://127.0.0.1:1/v1/traces".to_string()),
            logs_url: Some("http://127.0.0.1:1/v1/logs".to_string()),
            channel_capacity: 64,
        });
        let _layer = OtlpLayer::new(exporter, "test-svc", "0.0.1", "test", true, true);
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

    #[tokio::test]
    async fn layer_captures_span_and_sends_trace_on_close() {
        let (exporter, mut rx) = Exporter::start_test();
        let layer = OtlpLayer::new(exporter, "test-svc", "0.0.1", "test", true, true);
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let trace_id_hex = "0102030405060708090a0b0c0d0e0f10";
        let trace_id_bytes: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

        {
            let span = tracing::info_span!("test-span", trace_id = trace_id_hex);
            let _enter = span.enter();
        }

        let msg = rx.recv().await.expect("should receive trace message");
        match msg {
            ExportMessage::Traces(data) => {
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

    #[tokio::test]
    async fn layer_captures_event_and_sends_log() {
        let (exporter, mut rx) = Exporter::start_test();
        let layer = OtlpLayer::new(exporter, "test-svc", "0.0.1", "test", true, true);
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!("hello integration");

        let msg = rx.recv().await.expect("should receive log message");
        match msg {
            ExportMessage::Logs(data) => {
                assert!(
                    data.windows(17).any(|w| w == b"hello integration"),
                    "log body not found in protobuf"
                );
            }
            other => panic!("expected Logs, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn layer_event_inside_span_carries_trace_context() {
        let (exporter, mut rx) = Exporter::start_test();
        let layer = OtlpLayer::new(exporter, "test-svc", "0.0.1", "test", true, true);
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
        let msg = rx.recv().await.expect("should receive log");
        match msg {
            ExportMessage::Logs(data) => {
                assert!(
                    data.windows(16).any(|w| w == trace_id_bytes),
                    "trace_id not propagated to log"
                );
            }
            other => panic!("expected Logs, got {:?}", other),
        }

        // Second message: the span trace
        let msg = rx.recv().await.expect("should receive trace");
        assert!(matches!(msg, ExportMessage::Traces(_)));
    }

    #[tokio::test]
    async fn field_collector_handles_all_types() {
        let (exporter, mut rx) = Exporter::start_test();
        let layer = OtlpLayer::new(exporter, "test-svc", "0.0.1", "test", true, true);
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        {
            let span = tracing::info_span!(
                "typed-span",
                str_field = "hello",
                i64_field = 42i64,
                u64_field = 100u64,
                bool_field = true,
                f64_field = 3.14f64,
            );
            let _enter = span.enter();
        }

        let msg = rx.recv().await.expect("should receive trace");
        match msg {
            ExportMessage::Traces(data) => {
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

    #[tokio::test]
    async fn parent_span_id_propagated_to_child() {
        let (exporter, mut rx) = Exporter::start_test();
        let layer = OtlpLayer::new(exporter, "test-svc", "0.0.1", "test", true, true);
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        {
            let parent = tracing::info_span!("parent-span");
            let _parent_enter = parent.enter();
            {
                let child = tracing::info_span!("child-span");
                let _child_enter = child.enter();
            } // child closes first
        } // parent closes second

        // First: child span
        let msg1 = rx.recv().await.expect("should receive child trace");
        match &msg1 {
            ExportMessage::Traces(data) => {
                assert!(
                    data.windows(10).any(|w| w == b"child-span"),
                    "child span name not found"
                );
            }
            other => panic!("expected Traces for child, got {:?}", other),
        }

        // Second: parent span
        let msg2 = rx.recv().await.expect("should receive parent trace");
        match &msg2 {
            ExportMessage::Traces(data) => {
                assert!(
                    data.windows(11).any(|w| w == b"parent-span"),
                    "parent span name not found"
                );
            }
            other => panic!("expected Traces for parent, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn layer_skips_log_export_when_disabled() {
        let (exporter, mut rx) = Exporter::start_test();
        let layer = OtlpLayer::new(exporter, "test-svc", "0.0.1", "test", true, false);
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        // Emit an event — should be suppressed
        tracing::info!("should not export");

        // Create and close a span — trace should still arrive
        {
            let span = tracing::info_span!("traced-span");
            let _enter = span.enter();
        }

        let msg = rx.recv().await.expect("should receive trace");
        assert!(
            matches!(msg, ExportMessage::Traces(_)),
            "expected Traces, got {:?}",
            msg
        );

        // No more messages expected
        let extra = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
        assert!(extra.is_err(), "expected no more messages");
    }

    #[tokio::test]
    async fn layer_skips_trace_export_when_disabled() {
        let (exporter, mut rx) = Exporter::start_test();
        let layer = OtlpLayer::new(exporter, "test-svc", "0.0.1", "test", false, true);
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        // Emit an event inside a span
        {
            let span = tracing::info_span!("suppressed-span");
            let _enter = span.enter();
            tracing::info!("logged event");
        }

        // Should receive the log but not the trace
        let msg = rx.recv().await.expect("should receive log");
        assert!(
            matches!(msg, ExportMessage::Logs(_)),
            "expected Logs, got {:?}",
            msg
        );

        // No trace message expected
        let extra = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
        assert!(extra.is_err(), "expected no trace message");
    }
}
