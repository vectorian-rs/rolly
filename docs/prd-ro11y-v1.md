# PRD: ro11y — Lightweight Rust Observability

**Version:** 1.0
**Date:** 2026-03-05
**Status:** Active
**Repository:** [github.com/l1x/ro11y](https://github.com/l1x/ro11y)
**crates.io:** [crates.io/crates/ro11y](https://crates.io/crates/ro11y)
**License:** MIT OR Apache-2.0

---

## 1. Problem Statement

The OpenTelemetry Rust SDK is the standard approach for exporting telemetry from Rust services. In practice, it introduces significant pain:

- **Version lock-step** — all `opentelemetry-*` crates must version-match exactly. One patch bump breaks the others.
- **Dependency bloat** — ~120 transitive dependencies. gRPC export via `tonic`/`prost` adds 30+ seconds to compile times.
- **Shutdown footgun** — `TracerProvider::drop()` does not flush. Must call `shutdown()` explicitly before the tokio runtime drops, or lose tail spans.
- **Fragile bridge** — `tracing-opentelemetry` breaks on every OTEL release. Undocumented version matrix between `tracing-subscriber`, `opentelemetry`, and `opentelemetry_sdk`.
- **Metrics awkwardness** — `ObservableGauge` requires `Send + Sync + 'static` callbacks. Cardinality explosions cause memory bloat with no built-in limits.

ro11y replaces the OpenTelemetry SDK entirely with ~2,000 lines of hand-rolled code and 7 direct dependencies.

## 2. What ro11y Is

A Rust crate that bridges the `tracing` ecosystem to OTLP-compatible collectors via hand-rolled protobuf over HTTP. It is:

- **An OTLP exporter** — encodes traces and logs as native `ExportTraceServiceRequest` and `ExportLogsServiceRequest` protobuf, POSTs to any OTLP HTTP endpoint.
- **A tracing subscriber layer** — plugs into `tracing-subscriber` registry alongside `fmt::Layer` and `EnvFilter`. No new instrumentation API.
- **HTTP middleware** — Tower layers for inbound request instrumentation and outbound trace context propagation.
- **Framework-agnostic at the core** — the export pipeline works with any `tracing` span or event, not just HTTP. Queue consumers, batch jobs, CLI tools — anything that uses `tracing` gets OTLP export for free.

## 3. What ro11y Is Not

- Not a metrics SDK (see [Section 8: Metrics Gap](#8-metrics-gap))
- Not a dashboarding or alerting system
- Not a replacement for `tracing` — it builds on top of it
- Not a distributed tracing system — it exports to one (via Vector, OTEL Collector, etc.)

## 4. Current Implementation (v0.1.x)

### 4.1 Public API

```rust
// Initialize the full telemetry stack
pub fn init(config: TelemetryConfig) -> TelemetryGuard

pub struct TelemetryConfig {
    pub service_name: &'static str,
    pub service_version: &'static str,
    pub environment: &'static str,
    pub otlp_endpoint: Option<&'static str>,        // None = JSON stderr only
    pub use_metrics_interval: Option<Duration>,      // Linux /proc polling
}

pub struct TelemetryGuard { .. }  // Flushes on drop

// Tower middleware
pub fn request_layer() -> CfRequestIdLayer      // Inbound requests
pub fn propagation_layer() -> PropagationLayer   // Outbound requests

// ID generation
pub mod trace_id {
    pub fn generate_trace_id(request_id: Option<&str>) -> [u8; 16]
    pub fn generate_span_id() -> [u8; 8]
}
```

### 4.2 Architecture

```
init() sets up:
├── EnvFilter (RUST_LOG, default: info,tower_http=info)
├── fmt::Layer (JSON to stderr — CloudWatch fallback, always on)
└── OtlpLayer (custom tracing::Layer)
    ├── on_new_span() → assign trace_id, span_id, store in extensions
    ├── on_event() → encode as ExportLogsServiceRequest → POST /v1/logs
    └── on_close() → encode as ExportTraceServiceRequest → POST /v1/traces
        └── Exporter (background tokio task)
            ├── reqwest HTTP POST with Content-Type: application/x-protobuf
            ├── Exponential backoff: 100ms → 400ms → 1600ms
            └── Drop batch after 3 failures (fire-and-forget)
```

### 4.3 File Map

| File | Lines | Purpose |
|------|-------|---------|
| `lib.rs` | 147 | Public API: `init()`, `TelemetryConfig`, `TelemetryGuard` |
| `proto.rs` | 201 | Hand-rolled protobuf wire encoder (LEB128, tags, nested messages) |
| `otlp_trace.rs` | 305 | OTLP trace/span types and `ExportTraceServiceRequest` encoding |
| `otlp_log.rs` | 168 | OTLP log record types and `ExportLogsServiceRequest` encoding |
| `otlp_layer.rs` | ~200 | Custom `tracing::Layer` — span collection, field visitor, encoding dispatch |
| `exporter.rs` | 169 | Background HTTP exporter task with retry logic |
| `trace_id.rs` | 92 | Trace ID generation (BLAKE3 from CloudFront ID, or UUID v4) |
| `use_metrics.rs` | 160 | Linux `/proc/self/stat` CPU + memory polling |
| `tower/request.rs` | ~150 | `CfRequestIdLayer` — inbound request spans + RED metric events |
| `tower/propagation.rs` | ~80 | `PropagationLayer` — outbound W3C `traceparent` injection |
| `tower/mod.rs` | 6 | Module re-exports |

### 4.4 Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `blake3` | 1 | Hash CloudFront ID → deterministic trace_id |
| `bytes` | 1 | Efficient buffer operations (O(1) clone via Arc) |
| `http` | 1 | HTTP types for Tower middleware |
| `pin-project-lite` | 0.2 | Future pinning for Tower service |
| `rand` | 0.9 | Random span IDs |
| `reqwest` | 0.12 | HTTP POST to OTLP endpoint (rustls-tls) |
| `tokio` | 1 | Async runtime (sync channels, timers) |
| `tower` | 0.5 | Middleware traits (Layer, Service) |
| `tracing` | 0.1 | Instrumentation API |
| `tracing-subscriber` | 0.3 | Subscriber registry, JSON formatter, env filter |
| `uuid` | 1 | UUID v4 for trace IDs (when no CloudFront ID) |
| `libc` | 0.2 | Linux only: `sysconf()` for page size and clock ticks |

**Total: 7 runtime dependencies** (8 on Linux). No `opentelemetry`, `tonic`, or `prost`.

### 4.5 OTLP Signal Coverage

| Signal | OTLP Wire Format | Status |
|--------|------------------|--------|
| **Traces** | `ExportTraceServiceRequest` protobuf | **Implemented** |
| **Logs** | `ExportLogsServiceRequest` protobuf | **Implemented** |
| **Metrics** | `ExportMetricsServiceRequest` protobuf | **Not implemented** (see Section 8) |

### 4.6 Protobuf Encoding

The protobuf wire format is hand-rolled in `proto.rs` (~200 lines). No `.proto` files, no build-time codegen, no `prost` dependency. The wire format has been stable since 2008.

Primitives:
- `encode_varint()` — LEB128 encoding
- `encode_tag()` — field number + wire type
- `encode_varint_field()` / `encode_varint_field_always()` — tag + varint (skip or preserve zero)
- `encode_string_field()` — tag + length + UTF-8 bytes
- `encode_bytes_field()` — tag + length + raw bytes
- `encode_fixed64_field()` / `encode_fixed64_field_always()` — tag + 8 LE bytes
- `encode_message_field()` — tag + length + nested message

All primitives are tested against the protobuf specification.

### 4.7 Trace ID Strategy

| Source | Input | trace_id |
|--------|-------|----------|
| CloudFront → ALB → ECS | `x-amz-cf-id` header | `BLAKE3(request_id)[0..16]` — deterministic |
| Direct API call (no CF) | None | UUID v4 — random |
| Empty or `"-"` | Treated as None | UUID v4 — random |

Deterministic derivation ensures the same CloudFront request produces the same `trace_id` in all services without requiring W3C propagation for single-origin requests.

### 4.8 HTTP Middleware

**`CfRequestIdLayer`** (inbound requests):
1. Extract `x-amz-cf-id` header
2. Generate trace_id (BLAKE3 or UUID v4) and span_id
3. Create `info_span!("request", http.method, http.uri, http.status_code, http.latency_ms, cf.request_id, trace_id, span_id)`
4. On response: record status code, latency, emit RED metric events

**`PropagationLayer`** (outbound requests):
1. Read trace_id and span_id from current span extensions
2. Walk up parent spans if needed
3. Inject `traceparent: 00-{trace_id}-{span_id}-01` header (W3C format)

### 4.9 Metric Events (Current Workaround)

Metrics are emitted as structured `tracing::info!()` calls with convention fields:

```rust
tracing::info!(
    metric = "http.server.request.duration",
    r#type = "histogram",
    value = latency_ms,
    method = "GET",
    route = "/api/v1/users/:id",
    status = 200,
);
```

These flow through the logs pipeline and are converted to actual metrics downstream by Vector's `log_to_metric` transform. This is **not** standard OTLP — it is a convention.

### 4.10 Non-Functional Requirements

| NFR | Implementation |
|-----|---------------|
| Telemetry must not block the service | Fire-and-forget via tokio channel, background task |
| Graceful shutdown | `TelemetryGuard` flushes synchronously on drop |
| Retry on transient failure | 3 attempts with exponential backoff (100ms/400ms/1600ms) |
| No infinite recursion | Exporter uses `eprintln!()`, never `tracing::warn!()` |
| Dual output | OTLP HTTP primary + JSON stderr always on |

## 5. Target Pipeline

```
Service (tracing) → ro11y (protobuf HTTP) → Vector (OTLP source) → S3 (Parquet)
                                                                     ↓
                                                              Quickwit (search)
                                                              DataFusion (analytics)
                                                              ECharts (visualization)
```

### 5.1 Cost at Scale (2B requests/month, airline example)

| Component | Sizing | Monthly Cost |
|-----------|--------|-------------|
| Vector ECS | 2 tasks, 2 vCPU + 4 GB each | ~$145 |
| S3 Storage | ~565 GB average (30d traces/logs, 90d metrics) | ~$13 |
| S3 Requests | ~80K PUTs | ~$1 |
| Quickwit ECS | 1 task, 2 vCPU + 4 GB | ~$75 |
| DataFusion API ECS | 1 task, 1 vCPU + 2 GB | ~$40 |
| Dashboard CDN | CloudFront + S3 | ~$1 |
| Cross-AZ transfer | ~2.1 TB | ~$21 |
| **Total** | | **~$304/month** |

For comparison: Datadog for the same workload would cost $6,700–$20,000+/month depending on indexing.

## 6. Future: Feature-Flagged Middleware

Currently, HTTP middleware is Tower-only. The crate will support multiple HTTP frameworks via Cargo features:

```toml
[features]
default = ["tower"]
tower = ["dep:tower"]
harrow = ["dep:harrow-core"]
```

**Tower middleware** (`#[cfg(feature = "tower")]`):
- `src/tower/request.rs` — `CfRequestIdLayer` (exists today)
- `src/tower/propagation.rs` — `PropagationLayer` (exists today)

**Harrow middleware** (`#[cfg(feature = "harrow")]`):
- `src/harrow/request.rs` — `o11y_middleware` as `async fn(Request, Next) -> Response`
- `src/harrow/propagation.rs` — outbound traceparent injection

Both implementations share the same core primitives (`generate_trace_id()`, `generate_span_id()`, `info_span!()`, RED metric events). The `OtlpLayer` captures spans regardless of which middleware created them.

**Required API changes:**
- Make `trace_id::hex_encode` public (currently `pub(crate)`)
- Extract shared constants for span field names and metric event format

**Tracked in:** [panzerotti#119](https://github.com/l1x/panzerotti/issues/119)

## 7. Future: Visibility Improvements

### 7.1 Make `hex_encode` Public

`trace_id::hex_encode` is `pub(crate)` today. Both the Tower and Harrow middleware implementations need it to set `trace_id` and `span_id` as hex string fields on tracing spans. This should become `pub`.

### 7.2 Shared Constants

Span field names and metric event field names are currently string literals scattered across `tower/request.rs` and `otlp_layer.rs`. Extract these as public constants so middleware implementations (Tower and Harrow) stay in sync:

```rust
pub mod fields {
    pub const TRACE_ID: &str = "trace_id";
    pub const SPAN_ID: &str = "span_id";
    pub const HTTP_METHOD: &str = "http.method";
    pub const HTTP_URI: &str = "http.uri";
    pub const HTTP_STATUS_CODE: &str = "http.status_code";
    pub const HTTP_LATENCY_MS: &str = "http.latency_ms";
}

pub mod metrics {
    pub const REQUEST_DURATION: &str = "http.server.request.duration";
    pub const REQUEST_COUNT: &str = "http.server.request.count";
    pub const ERROR_COUNT: &str = "http.server.error.count";
}
```

## 8. Metrics Gap

### 8.1 What's Missing

OpenTelemetry defines seven metric instrument kinds:

| Instrument | Description | ro11y support |
|-----------|-------------|---------------|
| Counter | Monotonically increasing (request count) | Emulated as log event |
| Async Counter | Collected once per export | Not supported |
| UpDownCounter | Can increase or decrease (queue depth) | Not supported |
| Async UpDownCounter | Collected once per export | Not supported |
| Gauge | Point-in-time snapshot (CPU %) | Emulated as log event |
| Async Gauge | Collected once per export | Not supported |
| Histogram | Value distribution with buckets (latency p50/p99) | Emulated as log event — no bucketing |

Key OTLP metrics features not implemented:
- `ExportMetricsServiceRequest` protobuf encoder
- Client-side aggregation (SDK computes before export)
- Temporality (cumulative vs delta)
- Exemplars (link metric data point → trace_id)
- Histogram bucket boundaries and counts

### 8.2 Current Workaround

Metrics are emitted as `tracing::info!()` log events with `metric`, `type`, and `value` fields. They flow through the logs pipeline, are stored in the `logs/` Parquet table on S3, and are queryable via SQL:

```sql
SELECT attributes['metric'], CAST(attributes['value'] AS DOUBLE)
FROM logs
WHERE attributes['metric'] IS NOT NULL
ORDER BY timestamp DESC
```

Vector's `log_to_metric` transform can optionally convert these to actual metric types downstream.

### 8.3 Impact

At low-to-medium volume, the workaround is acceptable:
- Simple counters and gauges work fine as individual log events
- Values are stored as strings (no typed DOUBLE column in Parquet)
- Each request writes individual latency values — no pre-aggregation

At high volume (e.g. 2B requests/month), the lack of client-side aggregation becomes expensive:
- 4B+ metric log events stored in Parquet per month
- Histogram queries require full table scan over billions of rows
- With client-side histogram bucketing, 4B individual values would collapse to ~2.6M aggregated data points (one per metric per 10-second window per label set) — a **1,500x reduction**

### 8.4 Future: Native OTLP Metrics

Implementing `ExportMetricsServiceRequest` requires:

1. **Protobuf encoder** (~200–400 lines) — extend `proto.rs` with metric-specific encoding: `NumberDataPoint`, `HistogramDataPoint`, `ExponentialHistogramDataPoint`, `Metric`, `ScopeMetrics`, `ResourceMetrics`

2. **Instrument API** — define Counter, Gauge, Histogram types that collect data points. Must be `Send + Sync` for use across async tasks.

3. **Client-side aggregation** — accumulate values per metric name + label set over a configurable interval (e.g. 10 seconds), then flush as a single aggregated data point.

4. **Histogram bucketing** — explicit bucket boundaries (e.g. `[5, 10, 25, 50, 100, 250, 500, 1000]`). Track `bucket_counts`, `sum`, `count`, `min`, `max` per boundary set.

5. **Exporter extension** — add `ExportMessage::Metrics(Bytes)` variant, POST to `/v1/metrics`.

6. **Vector pipeline** — add OTLP metrics source routing to a dedicated `metrics/` Parquet table with typed columns:

```
timestamp       INT64 (nanos)
name            STRING
kind            STRING (counter | gauge | histogram)
value           DOUBLE
labels          MAP<STRING, STRING>
boundaries      LIST<DOUBLE>       -- histogram only
bucket_counts   LIST<INT64>        -- histogram only
sum             DOUBLE             -- histogram only
count           INT64              -- histogram only
```

**Estimated effort:** ~400–600 lines of Rust. The protobuf encoding primitives already exist in `proto.rs`; the new code is the metric types, aggregation logic, and histogram bucketing.

**When to invest:** When histogram accuracy or metric volume becomes a bottleneck. The current workaround works for the v1.0 launch; native metrics is a v1.1+ feature.

## 9. Reference Architecture Diagram

See [docs/signals-and-architecture.svg](signals-and-architecture.svg) for a visual overview of:
- OTLP signal coverage comparison (OpenTelemetry spec vs tracing crate vs ro11y)
- Architecture flow (ECS → Vector → S3 Parquet)
- Current vs future metrics storage approach

## 10. Milestones

| Version | Scope |
|---------|-------|
| **v0.1.x** (current) | Core OTLP traces + logs, Tower middleware, hand-rolled protobuf, JSON stderr fallback |
| **v0.2.0** | Feature-flagged middleware (Tower + Harrow), public `hex_encode`, shared field constants |
| **v0.3.0** | Native OTLP metrics (`ExportMetricsServiceRequest`), Counter + Gauge instruments |
| **v0.4.0** | Histogram instrument with client-side bucketing, exemplar support |
| **v1.0.0** | Stable API, full OTLP coverage (traces + logs + metrics), battle-tested at scale |
