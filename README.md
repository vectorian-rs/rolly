# rolly

Lightweight Rust observability. Hand-rolled OTLP protobuf over HTTP, built on [tracing](https://docs.rs/tracing).

7 direct dependencies. No `opentelemetry`, `tonic`, or `prost`.

## Crates

| Crate | Description |
|-------|-------------|
| [`rolly`](https://crates.io/crates/rolly) | Runtime-agnostic core. Encoding, metrics, tracing layer. Zero runtime deps. |
| [`rolly-tokio`](https://crates.io/crates/rolly-tokio) | Tokio transport. Batching HTTP exporter via reqwest. |
| [`rolly-monoio`](https://crates.io/crates/rolly-monoio) | Monoio transport. HTTP exporter via ureq on OS threads. |

All three share a workspace version. Add the runtime crate that matches your async runtime — it re-exports everything from `rolly` core.

## Quick Start (tokio)

```toml
[dependencies]
rolly-tokio = "0.15"
```

```rust
use rolly_tokio::{init_global_once, TelemetryConfig, BackpressureStrategy};
use std::time::Duration;

let _guard = init_global_once(TelemetryConfig {
    service_name: "my-service".into(),
    service_version: env!("CARGO_PKG_VERSION").into(),
    environment: "prod".into(),
    otlp_traces_endpoint: Some("http://collector:4318".into()),
    otlp_logs_endpoint: Some("http://collector:4318".into()),
    otlp_metrics_endpoint: Some("http://collector:4318".into()),
    log_to_stderr: true,
    use_metrics_interval: Some(Duration::from_secs(30)),
    metrics_flush_interval: None, // default 10s
    sampling_rate: Some(0.1),     // export 10% of traces
    backpressure_strategy: BackpressureStrategy::Drop,
    resource_attributes: vec![],
});

// All tracing spans and events are now exported as OTLP protobuf
tracing::info_span!("process_job", job_id = 42).in_scope(|| {
    tracing::info!("job completed");
});
```

Set any endpoint to `None` to disable that signal. Hold the guard until shutdown — it flushes pending telemetry on drop. For deterministic drain, call `guard.shutdown().await` instead.

## Quick Start (monoio)

```toml
[dependencies]
rolly-monoio = "0.15"
```

```rust
use rolly_monoio::{init_global_once, TelemetryConfig, BackpressureStrategy};

let _guard = init_global_once(TelemetryConfig {
    service_name: "my-service".into(),
    service_version: env!("CARGO_PKG_VERSION").into(),
    environment: "prod".into(),
    otlp_traces_endpoint: Some("http://collector:4318".into()),
    otlp_logs_endpoint: None,
    otlp_metrics_endpoint: None,
    log_to_stderr: true,
    use_metrics_interval: None,
    metrics_flush_interval: None,
    sampling_rate: None,
    backpressure_strategy: BackpressureStrategy::Drop,
    resource_attributes: vec![],
});
```

## Composable Setup (no global subscriber)

For frameworks that manage their own tracing subscriber:

```rust
use rolly_tokio::{TokioExporter, ExporterConfig, TelemetrySink};
use rolly::{build_layer, LayerConfig};
use std::sync::Arc;

let exporter = TokioExporter::start(ExporterConfig {
    traces_url: Some("http://collector:4318/v1/traces".into()),
    logs_url: None,
    metrics_url: None,
    ..ExporterConfig::default()
})?;

let sink: Arc<dyn TelemetrySink> = Arc::new(exporter.clone());
let layer = build_layer(&LayerConfig {
    log_to_stderr: false,
    export_traces: true,
    export_logs: false,
    service_name: "my-service".into(),
    service_version: "0.1.0".into(),
    environment: "prod".into(),
    resource_attributes: vec![],
    sampling_rate: 1.0,
    scope_name: "rolly".into(),
    scope_version: env!("CARGO_PKG_VERSION").into(),
}, sink);

tracing_subscriber::registry()
    .with(layer)
    .with(my_custom_layer)  // compose with your own layers
    .init();
```

## CLI / No Runtime

```rust
use rolly::{build_layer, LayerConfig, NullSink};
use std::sync::Arc;

let layer = build_layer(&LayerConfig {
    log_to_stderr: true,
    export_traces: false,
    export_logs: false,
    service_name: "my-cli".into(),
    service_version: "0.1.0".into(),
    environment: "dev".into(),
    resource_attributes: vec![],
    sampling_rate: 1.0,
    scope_name: "rolly".into(),
    scope_version: "0.1.0".into(),
}, Arc::new(NullSink));

tracing_subscriber::registry().with(layer).init();
```

## Signals

| Signal  | Format | Standard |
|---------|--------|----------|
| Traces  | OTLP `ExportTraceServiceRequest` protobuf | Yes |
| Logs    | OTLP `ExportLogsServiceRequest` protobuf  | Yes |
| Metrics | OTLP `ExportMetricsServiceRequest` protobuf | Yes |

All three follow the [OTLP specification](https://opentelemetry.io/docs/specs/otlp/) and work with any OTLP-compatible backend (Vector, Grafana Alloy, Jaeger, OTEL Collector).

## Metrics

Counter, Gauge, and Histogram with client-side aggregation:

```rust
use rolly::{counter, gauge, histogram};

let req = counter("http.server.requests", "Total HTTP requests");
req.add(1, &[("method", "GET"), ("status", "200")]);

let mem = gauge("process.memory.usage", "Memory usage in bytes");
mem.set(1_048_576.0, &[("unit", "bytes")]);

let latency = histogram(
    "http.server.duration",
    "Request latency in seconds",
    &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0],
);
latency.observe(0.042, &[("method", "GET"), ("route", "/api/users")]);
```

Attribute order does not matter. When called inside a tracing span, metrics automatically capture an **exemplar** with `trace_id` + `span_id` from the active span.

## OTel Semantic Fields

The tracing layer recognizes standard OTel control fields:

```rust
let span = tracing::info_span!(
    "http_request",
    "otel.kind" = "server",
    "otel.status_code" = tracing::field::Empty,
    "otel.status_message" = tracing::field::Empty,
);

// Set status after processing:
span.record("otel.status_code", "ok");
```

| Field | OTLP Mapping | Values |
|-------|-------------|--------|
| `otel.kind` | `SpanKind` | server, client, producer, consumer, internal |
| `otel.status_code` | `StatusCode` | ok, error, unset |
| `otel.status_message` | `SpanStatus.message` | any string |

These fields are consumed by the layer and do **not** appear as span attributes in exported data.

## Sampling

Trace sampling is **deterministic based on trace_id** — the same trace always gets the same decision across services.

- `Some(1.0)` or `None` — export all traces (default)
- `Some(0.1)` — export 10% of traces
- `Some(0.0)` — export no traces

Child spans and log events within a sampled-out trace are suppressed. Metrics are never sampled.

## Why not OpenTelemetry SDK?

- Version lock-step across `opentelemetry-*` crates
- ~120 transitive dependencies, 3+ minute compile times
- `drop()` doesn't flush (shutdown footgun)
- gRPC bloat from `tonic`/`prost`

rolly hand-rolls the protobuf wire format. 7 direct dependencies. The wire format has been stable since 2008.

## Verification

- 19 Kani proof harnesses for protobuf encoding correctness
- 2 TLA+ specifications (exporter protocol, metrics registry concurrency)
- 3 cargo-fuzz targets for crash/panic freedom
- 9 proptests for roundtrip encoding
- Wire-compatibility tests: rolly encodes, prost/opentelemetry-proto decodes

## License

MIT
