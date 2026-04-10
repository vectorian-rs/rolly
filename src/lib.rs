pub mod constants;
pub(crate) mod exporter;
pub mod metrics;
pub(crate) mod otlp_layer;
pub(crate) mod otlp_log;
pub(crate) mod otlp_metrics;
pub(crate) mod otlp_trace;
pub(crate) mod proto;
pub mod trace_id;
pub(crate) mod use_metrics;

pub use exporter::BackpressureStrategy;
pub use metrics::{counter, gauge, histogram, Counter, Gauge, Histogram};
pub use use_metrics::UseMetricsState;

use std::sync::Arc;
use std::time::Duration;

use exporter::{Exporter, ExporterConfig};
use otlp_layer::OtlpLayer;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// Sink for encoded OTLP protobuf payloads.
///
/// This is the single transport boundary for all three OTLP signals.
/// `OtlpLayer` calls `send_traces()` / `send_logs()` from tracing layer
/// callbacks on the application's hot path. The runtime-owned metrics
/// aggregation loop calls `send_metrics()`.
///
/// Implementations must be non-blocking. If the underlying channel is
/// full, the payload should be dropped silently.
pub trait TelemetrySink: Send + Sync + 'static {
    /// Send encoded `ExportTraceServiceRequest` protobuf bytes.
    fn send_traces(&self, data: Vec<u8>);
    /// Send encoded `ExportLogsServiceRequest` protobuf bytes.
    fn send_logs(&self, data: Vec<u8>);
    /// Send encoded `ExportMetricsServiceRequest` protobuf bytes.
    fn send_metrics(&self, data: Vec<u8>);
}

/// No-op sink that discards all telemetry data.
///
/// Useful for stderr-only setups, CLI tools, or tests where no OTLP
/// export is needed.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl TelemetrySink for NullSink {
    fn send_traces(&self, _data: Vec<u8>) {}
    fn send_logs(&self, _data: Vec<u8>) {}
    fn send_metrics(&self, _data: Vec<u8>) {}
}

/// Configuration for the telemetry stack.
pub struct TelemetryConfig {
    pub service_name: String,
    pub service_version: String,
    pub environment: String,
    /// OTLP HTTP endpoint for traces (e.g. "http://jaeger:4318").
    /// If None, trace export is disabled.
    pub otlp_traces_endpoint: Option<String>,
    /// OTLP HTTP endpoint for logs (e.g. "http://vector:4318").
    /// If None, log export is disabled. Can differ from traces endpoint.
    pub otlp_logs_endpoint: Option<String>,
    /// OTLP HTTP endpoint for metrics (e.g. "http://vector:4318").
    /// If None, metrics export is disabled.
    pub otlp_metrics_endpoint: Option<String>,
    /// Whether to emit JSON-formatted logs to stderr.
    pub log_to_stderr: bool,
    /// Polling interval for USE metrics (cpu, memory) from `/proc/self/stat`.
    /// If None, USE metrics collection is disabled.
    /// Only active on Linux; no-op on other platforms.
    pub use_metrics_interval: Option<Duration>,
    /// How often to flush aggregated metrics to the OTLP endpoint.
    /// Defaults to 10 seconds if None.
    pub metrics_flush_interval: Option<Duration>,
    /// Probabilistic trace sampling rate: 1.0 = export all traces, 0.01 = export 1%.
    /// Sampling is deterministic based on trace_id, so the same trace always gets the
    /// same decision across services. Metrics are never sampled.
    /// Defaults to 1.0 (no sampling) if None.
    pub sampling_rate: Option<f64>,
    /// What to do when the export channel is full.
    /// Defaults to `BackpressureStrategy::Drop`.
    pub backpressure_strategy: BackpressureStrategy,
    /// Custom OTel resource attributes appended after the standard three
    /// (service.name, service.version, deployment.environment).
    pub resource_attributes: Vec<(String, String)>,
}

/// Guard that flushes pending telemetry on drop.
///
/// Hold this in your main function to ensure all spans are exported before shutdown.
pub struct TelemetryGuard {
    exporter: Option<Exporter>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(ref exporter) = self.exporter {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(rt) = rt {
                rt.block_on(async {
                    exporter.flush().await;
                    exporter.shutdown().await;
                });
            }
        }
    }
}

/// Configuration for building telemetry layers.
pub struct LayerConfig {
    /// Whether to emit JSON-formatted logs to stderr.
    pub log_to_stderr: bool,
    /// Whether to export traces via the sink.
    pub export_traces: bool,
    /// Whether to export logs via the sink.
    pub export_logs: bool,
    /// Service name for OTLP resource attributes.
    pub service_name: String,
    /// Service version for OTLP resource attributes.
    pub service_version: String,
    /// Deployment environment for OTLP resource attributes.
    pub environment: String,
    /// Custom resource attributes.
    pub resource_attributes: Vec<(String, String)>,
    /// Probabilistic trace sampling rate (0.0–1.0). Defaults to 1.0.
    pub sampling_rate: f64,
}

/// Build telemetry layers without installing a global subscriber.
///
/// Returns a composed tracing layer that the caller adds to their own
/// subscriber registry. The caller owns `.init()` / `.try_init()`.
///
/// This does NOT install a global subscriber, spawn any tasks, or touch
/// global state. Use this when you need to compose rolly's layers with
/// your own.
pub fn build_layer(
    config: &LayerConfig,
    sink: Arc<dyn TelemetrySink>,
) -> impl tracing_subscriber::Layer<tracing_subscriber::Registry> {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info"));

    let fmt_layer = if config.log_to_stderr {
        Some(
            fmt::layer()
                .json()
                .with_target(true)
                .with_current_span(true)
                .with_span_list(false)
                .with_writer(std::io::stderr),
        )
    } else {
        None
    };

    let sampling_rate = config.sampling_rate.clamp(0.0, 1.0);
    let otlp_layer = if config.export_traces || config.export_logs {
        Some(OtlpLayer::new(otlp_layer::OtlpLayerConfig {
            sink,
            service_name: &config.service_name,
            service_version: &config.service_version,
            environment: &config.environment,
            resource_attributes: &config.resource_attributes,
            export_traces: config.export_traces,
            export_logs: config.export_logs,
            sampling_rate,
        }))
    } else {
        None
    };

    env_filter.and_then(fmt_layer).and_then(otlp_layer)
}

/// Initialize the telemetry stack and set the global subscriber.
///
/// This is a convenience function that creates an exporter, calls
/// `build_layer()`, installs the global subscriber, and spawns
/// background tasks for metrics and USE polling.
///
/// # Panics
///
/// Panics if a global tracing subscriber is already set.
///
/// # Deprecation
///
/// Use `build_layer()` for composable setups, or `rolly_tokio::init_global_once()`
/// once the runtime crate is available.
#[deprecated(note = "use build_layer() for composable setups, or rolly_tokio::init_global_once()")]
pub fn init(config: TelemetryConfig) -> TelemetryGuard {
    let export_traces = config.otlp_traces_endpoint.is_some();
    let export_logs = config.otlp_logs_endpoint.is_some();
    let export_metrics = config.otlp_metrics_endpoint.is_some();

    let metrics_url = config
        .otlp_metrics_endpoint
        .as_deref()
        .map(|ep| format!("{}/v1/metrics", ep));

    let exporter = if export_traces || export_logs || export_metrics {
        let traces_url = config
            .otlp_traces_endpoint
            .as_deref()
            .map(|ep| format!("{}/v1/traces", ep));
        let logs_url = config
            .otlp_logs_endpoint
            .as_deref()
            .map(|ep| format!("{}/v1/logs", ep));
        Some(Exporter::start(ExporterConfig {
            traces_url,
            logs_url,
            metrics_url: metrics_url.clone(),
            channel_capacity: 1024,
            batch_size: 512,
            flush_interval: Duration::from_secs(1),
            max_concurrent_exports: 4,
            backpressure_strategy: config.backpressure_strategy,
        }))
    } else {
        None
    };

    let sink: Arc<dyn TelemetrySink> = match &exporter {
        Some(exp) => Arc::new(exp.clone()),
        None => Arc::new(NullSink),
    };

    let layer_config = LayerConfig {
        log_to_stderr: config.log_to_stderr,
        export_traces,
        export_logs,
        service_name: config.service_name.clone(),
        service_version: config.service_version.clone(),
        environment: config.environment.clone(),
        resource_attributes: config.resource_attributes.clone(),
        sampling_rate: config.sampling_rate.unwrap_or(1.0),
    };

    let layer = build_layer(&layer_config, sink);

    tracing_subscriber::registry().with(layer).init();

    tracing::info!(
        service.name = config.service_name.as_str(),
        service.version = config.service_version.as_str(),
        environment = config.environment.as_str(),
        "telemetry initialized"
    );

    if let Some(interval) = config.use_metrics_interval {
        use_metrics::start(interval);
    }

    if let Some(ref exporter) = exporter {
        if metrics_url.is_some() {
            let flush_interval = config
                .metrics_flush_interval
                .unwrap_or(Duration::from_secs(10));
            let exporter = exporter.clone();
            let metrics_config = MetricsExportConfig {
                service_name: config.service_name,
                service_version: config.service_version,
                environment: config.environment,
                resource_attributes: config.resource_attributes,
                scope_name: "rolly".to_string(),
                scope_version: env!("CARGO_PKG_VERSION").to_string(),
                start_time: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64,
            };
            tokio::spawn(async move {
                metrics_aggregation_loop(exporter, flush_interval, metrics_config).await;
            });
        }
    }

    TelemetryGuard { exporter }
}

/// Background task that periodically collects and exports aggregated metrics.
async fn metrics_aggregation_loop(
    exporter: Exporter,
    flush_interval: Duration,
    config: MetricsExportConfig,
) {
    let mut interval = tokio::time::interval(flush_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the first immediate tick
    interval.tick().await;

    loop {
        interval.tick().await;
        if let Some(data) = collect_and_encode_metrics(&config) {
            exporter.send_metrics(data);
        }
    }
}

/// Return the total number of telemetry messages dropped due to a full channel.
pub fn telemetry_dropped_total() -> u64 {
    exporter::dropped_total()
}

/// Configuration for metrics export encoding.
pub struct MetricsExportConfig {
    /// Service name, version, environment, plus any custom attributes.
    /// Converted to OTel KeyValue internally.
    pub service_name: String,
    pub service_version: String,
    pub environment: String,
    pub resource_attributes: Vec<(String, String)>,
    /// Instrumentation scope name.
    pub scope_name: String,
    /// Instrumentation scope version.
    pub scope_version: String,
    /// Start time for cumulative metrics (nanos since epoch).
    pub start_time: u64,
}

/// Collect current metric snapshots and encode as OTLP protobuf.
///
/// Returns the encoded `ExportMetricsServiceRequest` bytes, ready to
/// pass to `TelemetrySink::send_metrics()`. Returns `None` if no metrics
/// have been recorded since the last collection.
///
/// The caller decides when and how often to call this.
pub fn collect_and_encode_metrics(config: &MetricsExportConfig) -> Option<Vec<u8>> {
    use otlp_trace::{AnyValue, KeyValue};

    let snapshots = metrics::global_registry().collect();
    if snapshots.is_empty() {
        return None;
    }

    let mut resource_attrs = vec![
        KeyValue {
            key: "service.name".to_string(),
            value: AnyValue::String(config.service_name.clone()),
        },
        KeyValue {
            key: "service.version".to_string(),
            value: AnyValue::String(config.service_version.clone()),
        },
        KeyValue {
            key: "deployment.environment".to_string(),
            value: AnyValue::String(config.environment.clone()),
        },
    ];
    for (k, v) in &config.resource_attributes {
        resource_attrs.push(KeyValue {
            key: k.clone(),
            value: AnyValue::String(v.clone()),
        });
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    Some(otlp_metrics::encode_export_metrics_request(
        &resource_attrs,
        &config.scope_name,
        &config.scope_version,
        &snapshots,
        config.start_time,
        now,
    ))
}

/// Poll USE metrics once.
///
/// Reads `/proc/self/stat` and `/proc/self/statm` synchronously, updates
/// `state`, and emits metric-shaped `tracing::info!` events. The caller
/// owns scheduling. Only meaningful on Linux; returns immediately on
/// other platforms.
pub fn collect_use_metrics(state: &mut UseMetricsState) {
    use_metrics::poll_once(state);
}

#[cfg(feature = "_bench")]
#[doc(hidden)]
pub mod bench {
    pub use crate::exporter::{BackpressureStrategy, ExportMessage, Exporter, ExporterConfig};
    pub use crate::metrics::{
        counter, gauge, global_registry, histogram, Attrs, Counter, CounterDataPoint, Exemplar,
        ExemplarValue, Gauge, GaugeDataPoint, Histogram, HistogramDataPoint, MetricSnapshot,
        MetricsRegistry,
    };
    pub fn should_sample(trace_id: [u8; 16], sampling_rate: f64) -> bool {
        crate::otlp_layer::should_sample(trace_id, sampling_rate)
    }
    pub use crate::otlp_layer::{OtlpLayer, OtlpLayerConfig};
    pub use crate::otlp_log::{encode_export_logs_request, LogData, SeverityNumber};
    pub use crate::otlp_metrics::encode_export_metrics_request;
    pub use crate::otlp_trace::{
        encode_export_trace_request, encode_key_value, encode_resource, AnyValue, KeyValue,
        SpanData, SpanKind, SpanStatus, StatusCode,
    };
    pub use crate::proto::{encode_message_field, encode_message_field_in_place};
    pub use crate::trace_id::{generate_span_id, generate_trace_id, hex_encode};

    #[allow(clippy::result_unit_err)]
    pub fn hex_to_bytes_16(s: &str) -> Result<[u8; 16], ()> {
        crate::otlp_layer::hex_to_bytes_16(s)
    }

    // Thin wrappers for pub(crate) proto functions
    pub fn encode_varint_field(buf: &mut Vec<u8>, field: u32, val: u64) {
        crate::proto::encode_varint_field(buf, field, val);
    }
    pub fn encode_string_field(buf: &mut Vec<u8>, field: u32, s: &str) {
        crate::proto::encode_string_field(buf, field, s);
    }
    pub fn encode_bytes_field(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
        crate::proto::encode_bytes_field(buf, field, data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_with_none_endpoint_does_not_panic() {
        let _config = TelemetryConfig {
            service_name: "test-service".into(),
            service_version: "0.0.1".into(),
            environment: "test".into(),
            otlp_traces_endpoint: None,
            otlp_logs_endpoint: None,
            otlp_metrics_endpoint: None,
            log_to_stderr: false,
            use_metrics_interval: None,
            metrics_flush_interval: None,
            sampling_rate: None,
            backpressure_strategy: BackpressureStrategy::Drop,
            resource_attributes: vec![],
        };
    }

    #[test]
    fn telemetry_dropped_total_is_callable() {
        let _count = telemetry_dropped_total();
    }

    #[test]
    fn null_sink_accepts_data() {
        let sink: Box<dyn TelemetrySink> = Box::new(NullSink);
        sink.send_traces(vec![1, 2, 3]);
        sink.send_logs(vec![4, 5, 6]);
        sink.send_metrics(vec![7, 8, 9]);
    }
}
