# PRD: rolly v3 — Current State and Path to 1.0

**Version:** 3.0
**Date:** 2026-04-11
**Status:** Active
**Predecessor:** [prd-rolly-v2.md](prd-rolly-v2.md)
**Repository:** [github.com/vectorian-rs/rolly](https://github.com/vectorian-rs/rolly)
**License:** MIT
**Current workspace version:** 0.14.0

---

## 1. What rolly Is

rolly is a lightweight Rust observability library. Hand-rolled OTLP protobuf encoding over HTTP, built on `tracing`. Three crates in a workspace:

- **`rolly`** — runtime-agnostic core. Encoding, metrics registry, tracing layer, trace ID generation. Zero runtime dependencies.
- **`rolly-tokio`** — tokio transport. Batching HTTP exporter via reqwest, `TelemetryGuard`, convenience init functions.
- **`rolly-monoio`** — monoio transport. Batching HTTP exporter via ureq on OS threads, cooperative shutdown.

All three share a single workspace version.

## 2. Current State (0.14.0 released + uncommitted work)

### 2.1 What's Done

The v2 PRD's three-phase plan is complete through Phase 3:

| Phase | Version | Status |
|-------|---------|--------|
| Phase 1: Core prep | 0.11.0 | Done. `TelemetrySink` trait, `build_layer()`, tower removed. |
| Phase 2: Tokio split | 0.12.0 | Done. `rolly-tokio` created, exporter moved, core is runtime-agnostic. |
| Phase 3: Monoio | 0.13.0–0.14.0 | Done. `rolly-monoio` created, ureq transport, TLS support. |

Earlier work (pre-0.11): formal verification (Kani, TLA+), fuzzing, property-based testing, HashDoS fix, mutex contention fix — all complete.

### 2.2 Uncommitted Work (targeting 0.15.0)

Six files changed, ~850 lines added. This work is new functionality not covered by the v2 PRD:

**1. OTel semantic span fields (`rolly/src/otlp_layer.rs`)**

The layer now recognizes standard OpenTelemetry span control fields:

- `otel.kind` → mapped to OTLP `SpanKind` (server, client, producer, consumer, internal)
- `otel.status_code` → mapped to OTLP `StatusCode` (ok, error, unset)
- `otel.status_message` → mapped to OTLP `SpanStatus.message`

These are parsed by `FieldCollector` and stored in `SpanFields`. On `on_close`, they are emitted as proper OTLP span fields (not attributes). The `otel.*` fields are consumed by the layer and do not appear as span attributes — they are semantic control signals.

Constants added to `constants::fields`: `OTEL_KIND`, `OTEL_STATUS_CODE`, `OTEL_STATUS_MESSAGE`.

`on_record` also updates these fields, so span kind/status can be set after span creation via `span.record()`.

**2. Auto-generated trace IDs for root spans (`rolly/src/otlp_layer.rs`)**

Root spans without an explicit `trace_id` field now get a random trace ID via `generate_trace_id(None)`. Previously, root spans without a `trace_id` attribute would have `[0u8; 16]` — an invalid OTLP trace ID that backends reject or display incorrectly.

Trace ID resolution order:
1. Inherit from parent span (child spans always share parent's trace)
2. Use explicit `trace_id` attribute from span fields (existing behavior — deterministic BLAKE3 from request ID)
3. Generate a new random trace ID for root spans (new fallback)

**3. `TelemetryGuard::shutdown()` async method (both runtimes)**

New explicit `async fn shutdown(self)` on `TelemetryGuard` for deterministic drain. On `current_thread` tokio runtimes, `Drop` cannot block — it can only trigger a best-effort async drain. `shutdown()` gives callers a reliable way to flush before exit.

Implementation: aborts background tasks, then flushes and shuts down the exporter. `Drop` calls the same `abort_tasks()` helper.

**4. `init_global_once` graceful handling of "subscriber already set" (both runtimes)**

`init_global_once` no longer panics when a global tracing subscriber is already set. Instead it logs a warning and returns a no-op `TelemetryGuard`. It still panics on real errors (exporter start failures in tokio).

This fixes a crash in downstream frameworks (harrow) where the application sets up its own tracing subscriber before the framework calls `init_global_once`. See §4 for the full analysis.

**5. Monoio exporter: separate control channel**

`ExportMessage` split into data messages (`Traces`, `Logs`, `Metrics`) and `ControlMessage` (`Flush`, `Shutdown`) on a dedicated unbounded `crossbeam_channel`. This prevents control messages from being blocked behind a full data channel.

**6. Monoio: `max_concurrent_exports` config**

Added to `ExporterConfig` with default of 4. Matches tokio's config.

**7. Monoio: cooperative shutdown for background loops**

The metrics aggregation and USE metrics polling loops now check an `AtomicBool` shutdown flag each iteration. `TelemetryGuard::shutdown()` and `Drop` set this flag. Previously these loops ran until the monoio runtime exited.

## 3. Architecture

### 3.1 Crate Structure

```
rolly/                              # Core — runtime-agnostic, fully sync
├── src/
│   ├── lib.rs                      # Public API: build_layer, collect_*, TelemetrySink, NullSink
│   ├── otlp_layer.rs               # tracing Layer impl with OTel semantic field support
│   ├── otlp_trace.rs               # Protobuf encoding for traces
│   ├── otlp_log.rs                 # Protobuf encoding for logs
│   ├── otlp_metrics.rs             # Protobuf encoding for metrics
│   ├── proto.rs                    # Low-level protobuf wire encoding
│   ├── metrics.rs                  # MetricsRegistry, Counter, Gauge, Histogram
│   ├── trace_id.rs                 # Trace/span ID generation (BLAKE3 + UUID v4)
│   ├── constants.rs                # Field/metric name constants (incl. otel.* fields)
│   └── use_metrics.rs              # Linux /proc polling + UseMetricsState

rolly-tokio/                        # Tokio transport
├── src/
│   ├── lib.rs                      # TokioExporter, init_global_once, TelemetryGuard, spawn_metrics_loop
│   └── exporter.rs                 # Batching HTTP exporter via reqwest

rolly-monoio/                       # Monoio transport
├── src/
│   ├── lib.rs                      # MonoioExporter, init_global_once, TelemetryGuard
│   └── exporter.rs                 # Batching HTTP exporter via ureq on OS threads
```

### 3.2 Dependency Flow

```
Application (harrow, rie-worker, CLI tool)
    │
    ├── rolly-tokio (or rolly-monoio)     ← runtime-specific transport
    │       │
    │       └── rolly                      ← core: encoding, layers, metrics, trait
    │
    └── tracing-subscriber                 ← app owns the subscriber
```

### 3.3 Key Dependencies

**rolly (core):** blake3, rand, tracing, tracing-subscriber, uuid. Zero runtime deps. libc on Linux only.

**rolly-tokio:** rolly, bytes, reqwest (rustls-tls), tokio (sync, time, rt, rt-multi-thread).

**rolly-monoio:** rolly, bytes, crossbeam-channel, monoio, ureq, tracing-subscriber.

## 4. The Subscriber Ownership Problem

### 4.1 The Rule

Libraries and frameworks must never set the global tracing subscriber. That is the application's responsibility. Libraries provide `Layer`s; the application composes them.

### 4.2 What Happened

harrow's `.o11y()` convenience method calls `rolly_tokio::init_global_once()`, which sets the global subscriber. When rie-worker (an application using harrow) initializes its own `tracing_subscriber::fmt()` before calling `.o11y()`, the second subscriber installation panics.

### 4.3 Fixes Applied

**In rolly (0.15.0):** `init_global_once` catches `SubscriberAlreadySet` and returns a no-op guard with a warning. Safety net only — when this triggers, the rolly OTLP layer is not installed, so no traces/logs are exported.

**Recommended in harrow:** Split `.o11y()` into:
- `.o11y(config)` — full setup (subscriber + middleware) for simple users
- `.o11y_middleware(config)` — middleware + state only, for apps that own their subscriber

`.o11y()` calls `.o11y_middleware()` internally after subscriber init. DRY.

## 5. Design Details

### 5.1 TelemetrySink Trait

```rust
pub trait TelemetrySink: Send + Sync + 'static {
    fn send_traces(&self, data: Vec<u8>);
    fn send_logs(&self, data: Vec<u8>);
    fn send_metrics(&self, data: Vec<u8>);
}
```

Intentionally minimal. No async methods, no flush, no shutdown. Those belong on the concrete exporter type. Implementations must be non-blocking.

### 5.2 OtlpLayer — OTel Semantic Fields

The layer recognizes three control fields that map to OTLP span fields (not attributes):

| tracing field | OTLP field | Values | Default |
|---------------|-----------|--------|---------|
| `otel.kind` | `SpanKind` | server, client, producer, consumer, internal | Internal |
| `otel.status_code` | `StatusCode` | ok, error, unset | Unset |
| `otel.status_message` | `SpanStatus.message` | any string | None |

These fields are consumed by `FieldCollector` and stored in `SpanFields`. They do not appear as span attributes in the exported OTLP data. Case-insensitive parsing (e.g., both `"server"` and `"SERVER"` work).

When `status_code` is `Unset`, no `SpanStatus` is emitted. When it is `Ok` or `Error`, a `SpanStatus` with the code and optional message is included.

Usage:
```rust
let span = tracing::info_span!(
    "http_request",
    "otel.kind" = "server",
    "otel.status_code" = tracing::field::Empty,
);

// Later, after processing:
span.record("otel.status_code", "ok");
```

### 5.3 Trace ID Resolution

Order of precedence for trace ID assignment:

1. **Parent inheritance** — child spans always use their parent's trace ID
2. **Explicit `trace_id` field** — a 32-char hex string on the span (e.g., from harrow's deterministic BLAKE3 derivation)
3. **Random generation** — root spans without a `trace_id` get `generate_trace_id(None)` (UUID v4 based)

This ensures every exported span has a valid, non-zero trace ID.

### 5.4 TelemetryGuard

Both runtime crates provide `TelemetryGuard`:

```rust
pub struct TelemetryGuard {
    exporter: Option<Exporter>,
    task_handles: Vec<JoinHandle<()>>,  // tokio only
    background_shutdown: Option<Arc<AtomicBool>>,  // monoio only
}
```

**`Drop`:** Aborts background tasks (tokio) or signals shutdown (monoio), then best-effort flush. On multi-thread tokio, uses `block_in_place`. On current-thread tokio, spawns and hopes. On monoio, sends a non-blocking shutdown message.

**`async fn shutdown(self)`:** Deterministic drain. Aborts tasks / signals shutdown, then awaits flush and exporter shutdown. Prefer this over `Drop` when you need guaranteed delivery.

### 5.5 ExporterConfig

Both runtime crates expose identical config (same defaults):

```rust
pub struct ExporterConfig {
    pub traces_url: Option<String>,
    pub logs_url: Option<String>,
    pub metrics_url: Option<String>,
    pub channel_capacity: usize,          // default: 1024
    pub batch_size: usize,                // default: 512
    pub flush_interval: Duration,         // default: 1s
    pub max_concurrent_exports: usize,    // default: 4
    pub backpressure_strategy: BackpressureStrategy,
}
```

### 5.6 Monoio Exporter Architecture

The monoio exporter separates data and control into two channels:

- **Data channel** (`crossbeam_channel::bounded`): `ExportMessage` variants (`Traces`, `Logs`, `Metrics`). Subject to backpressure/drop.
- **Control channel** (`crossbeam_channel::unbounded`): `ControlMessage` variants (`Flush`, `Shutdown`). Never dropped.

HTTP POSTs run on spawned OS threads via `ureq`, keeping the monoio event loop responsive. The exporter loop polls both channels with `monoio::time::sleep` for batching intervals.

Background loops (metrics aggregation, USE metrics) check an `AtomicBool` shutdown flag each iteration for cooperative cancellation.

### 5.7 How Callers Use It

**Simple app (one-liner):**
```rust
let _guard = rolly_tokio::init_global_once(config);
```

**Framework (composable, owns its subscriber):**
```rust
let exporter = rolly_tokio::TokioExporter::start(exporter_config)?;
let sink: Arc<dyn TelemetrySink> = Arc::new(exporter.clone());
let rolly_layer = rolly::build_layer(&layer_config, sink.clone());

tracing_subscriber::registry()
    .with(rolly_layer)
    .with(my_custom_layer)
    .init();

let _metrics = rolly_tokio::spawn_metrics_loop(metrics_config, sink, Duration::from_secs(30));
```

**monoio runtime:**
```rust
let _guard = rolly_monoio::init_global_once(config);
// Or compose manually with build_layer + MonoioExporter::start
```

**CLI tool (no runtime, stderr only):**
```rust
let rolly_layer = rolly::build_layer(&config, Arc::new(rolly::NullSink));
tracing_subscriber::registry().with(rolly_layer).init();
```

## 6. What's Preserved from v1

These are the strengths rolly has carried since the beginning:

- **Hand-rolled protobuf** — `proto.rs`, `otlp_trace.rs`, `otlp_log.rs`, `otlp_metrics.rs`. Zero code generation, minimal allocations, proven wire-compatible.
- **Metrics registry** — `metrics.rs`. std::sync only. Cardinality limits, exemplar capture from tracing spans.
- **Deterministic trace IDs** — BLAKE3 from request ID when provided, UUID v4 fallback for root spans.
- **Sampling** — Deterministic, trace-ID-based, inherited from parent spans. Configurable rate.
- **Backpressure** — Drop-newest, non-blocking `try_send`. Strategy is the same; channel implementation varies by runtime.
- **Batching** — 512-item batches, 1s flush, 4 concurrent workers.

## 7. What Remains Before 1.0

### 7.1 Immediate (0.15.0)

The uncommitted work described in §2.2. Needs:

- [ ] Tests passing for all three crates
- [ ] CHANGELOG.md updated
- [ ] Version bumped to 0.15.0

### 7.2 Pre-1.0 Checklist

- [ ] **API audit** — review all public types/functions for naming consistency and stability. Once 1.0 ships, the API is frozen under semver.
- [ ] **`scope_name` hardcoded to "pz-o11y"** — `OtlpLayer::new()` sets `scope_name: "pz-o11y"`. This should be `"rolly"` or configurable. Artifact from the original project name.
- [ ] **Error type consistency** — `rolly-tokio` has `InitError` with `SubscriberAlreadySet` and `Exporter` variants. `rolly-monoio` has `InitError` with only `SubscriberAlreadySet`. Consider whether monoio should have an `Exporter` variant for start failures.
- [ ] **`trace_id` kept as attribute** — when a `trace_id` field is provided, it is both parsed as the OTLP trace ID and kept as a span attribute. Decide if the duplication is intentional (backend visibility) or should be removed.
- [ ] **Span events** — OTLP spans can carry events (logs attached to spans). Currently rolly exports events only as standalone log records. Consider whether span events should also appear in the span's events array.
- [ ] **Span links** — OTLP supports span links. Not currently implemented. Decide if needed for 1.0.
- [ ] **README** — update with usage examples for all three crates and the OTel field semantics.
- [ ] **Downstream verification** — verify harrow + rie-worker + harrow-monoio all work with the final API.

### 7.3 Non-Goals for 1.0

- gRPC transport (HTTP POST is simpler and sufficient)
- Multi-runtime within a single process
- Converting tracing-emitted metric events into registry-backed instruments
- Changing the protobuf encoding
- Changing the metrics registry internals

## 8. Version Strategy

All crates share a single workspace version. Releases are pre-1.0 until the full trio ships together with a stable API.

| Version | What ships |
|---------|-----------|
| 0.15.0 | OTel semantic fields, auto trace IDs, graceful init, TelemetryGuard::shutdown(), monoio exporter rework |
| 0.16.0+ | API audit fixes, scope_name fix, any remaining pre-1.0 items |
| 1.0.0 | Stable API. All three crates published together. |

## 9. Risks

| Risk | Mitigation |
|------|------------|
| `init_global_once` no-op guard silently disables telemetry | Warning is logged. Document clearly that composable `build_layer()` is the recommended path for frameworks. |
| `otel.*` field parsing adds overhead to every span | Parsing is a simple string match — negligible vs. channel send. Benchmark if concerned. |
| monoio OS-thread HTTP introduces thread exhaustion under load | `max_concurrent_exports` caps it at 4 by default. Document. |
| `scope_name: "pz-o11y"` leaks internal naming to backends | Fix before 1.0. |
| Monoio crate maturity vs tokio | monoio exporter has separate control channel, cooperative shutdown, and ureq TLS. Functional parity achieved. |
