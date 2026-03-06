# ro11y

Lightweight Rust observability. Hand-rolled OTLP protobuf over HTTP, built on [tracing](https://docs.rs/tracing).

## Core and middleware

ro11y has two layers:

**Generic core** — works with any Rust application, not just HTTP servers:
- Custom `tracing::Layer` that captures all spans and events
- Encodes them as OTLP protobuf (`ExportTraceServiceRequest`, `ExportLogsServiceRequest`)
- Ships via HTTP POST to any OTLP-compatible collector (Vector, Grafana Alloy, OTEL Collector)
- Dual output: OTLP HTTP primary + JSON stderr fallback (local dev / CloudWatch)
- Background exporter with 3-retry exponential backoff — telemetry never blocks your application
- Process metrics (CPU, memory) via `/proc` polling on Linux

**HTTP middleware** (optional) — framework-specific request instrumentation:
- Tower middleware for Axum (built-in today)
- Extracts request IDs (CloudFront, `x-request-id`, or any header), generates deterministic trace IDs via BLAKE3
- Creates request spans with method, path, status, latency
- Emits RED metrics (request duration, count, errors)
- W3C `traceparent` propagation for outbound requests

Any `tracing` span or event from anywhere in your application — HTTP handlers, background tasks, queue consumers, batch jobs — flows through the same OTLP export pipeline.

## Signals: what's standard, what's not

| Signal  | Format                         | Standard |
|---------|--------------------------------|----------|
| Traces  | OTLP `ExportTraceServiceRequest` protobuf | Yes |
| Logs    | OTLP `ExportLogsServiceRequest` protobuf  | Yes |
| Metrics | Structured log events with `metric`/`type`/`value` fields | No |

Traces and logs follow the [OTLP specification](https://opentelemetry.io/docs/specs/otlp/) and are encoded as native protobuf. Any OTLP-compatible backend can ingest them directly.

Metrics are **not** OTLP `ExportMetricsServiceRequest`. Instead, they are emitted as structured `tracing::info!()` events:

```rust
tracing::info!(
    metric = "http.server.request.duration",
    r#type = "histogram",
    value = 42.5,
    method = "GET",
    route = "/users/:id",
    status = 200,
);
```

These flow through the log pipeline and are converted to real metrics downstream by Vector's [`log_to_metric`](https://vector.dev/docs/reference/configuration/transforms/log_to_metric/) transform. This avoids the complexity of the OTLP metrics data model (histograms, exponential histograms, summaries, exemplars) at the cost of being non-standard at the wire level.

## Usage

```rust
use ro11y::{init, TelemetryConfig};
use std::time::Duration;

// Generic core — works for any application
let _guard = init(TelemetryConfig {
    service_name: "my-service",
    service_version: env!("CARGO_PKG_VERSION"),
    environment: "prod",
    otlp_endpoint: Some("http://vector:4318"),
    use_metrics_interval: Some(Duration::from_secs(30)),
});

// All tracing spans/events are now exported as OTLP protobuf
tracing::info_span!("process_job", job_id = 42).in_scope(|| {
    tracing::info!("job completed");
});
```

With HTTP middleware (Axum/Tower):

```rust
let app = axum::Router::new()
    .route("/health", axum::routing::get(health))
    .layer(ro11y::request_layer())       // inbound: request spans + RED metrics
    .layer(ro11y::propagation_layer());  // outbound: W3C traceparent injection
```

## Pipeline

```
Application (tracing) → ro11y (protobuf) → HTTP POST → Vector/Collector (OTLP) → storage
```

## Why not OpenTelemetry SDK?

- Version lock-step across `opentelemetry-*` crates
- ~120 transitive dependencies, 3+ minute compile times
- Shutdown footgun (`drop()` doesn't flush)
- gRPC bloat from `tonic`/`prost`

ro11y hand-rolls the protobuf wire format (~200 lines). The format has been stable since 2008.

## Dependencies

7 direct dependencies. No `opentelemetry`, `tonic`, or `prost`.

## License

MIT OR Apache-2.0
