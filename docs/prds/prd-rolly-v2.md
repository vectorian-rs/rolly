# PRD: rolly v2 — Runtime-Agnostic Observability

**Version:** 2.0
**Date:** 2026-04-09
**Status:** Draft
**Predecessor:** [prd-rolly-v1.md](prd-rolly-v1.md)
**Repository:** [github.com/vectorian-rs/rolly](https://github.com/vectorian-rs/rolly)
**License:** MIT

---

## 1. Problem Statement

rolly v1 delivers lightweight OTLP observability with hand-rolled protobuf, deterministic trace IDs, and zero-alloc hot paths. It works well — at scale, in production. But it is hardwired to tokio.

The transport layer (`Exporter`) uses `tokio::spawn`, `tokio::sync::mpsc`, `tokio::select!`, `tokio::time`, and `tokio::sync::Semaphore`. The `init()` function spawns background tasks via `tokio::spawn`. `TelemetryGuard::drop()` builds a `tokio::runtime::Builder` to flush on shutdown.

This means rolly cannot be used with:

- **monoio** — thread-per-core runtime used by harrow-monoio
- **glommio** — io_uring-based runtime
- **Raw io_uring** — custom runtimes built directly on io_uring
- **Sync services** — CLI tools, batch jobs, or any context without a tokio runtime

Additionally, `init()` calls `.init()` on the tracing subscriber unconditionally, which panics if a subscriber is already set. This makes rolly unsafe to use as a library — the caller cannot compose rolly's layers with their own.

## 2. Goals

1. **Runtime-agnostic core** — encoding, metrics, layers, and trace ID generation must have zero runtime dependency. The core crate is fully synchronous — no async traits, no futures, no runtime.
2. **Composable API** — callers build their own subscriber and compose rolly's layers into it. No forced global subscriber installation.
3. **Transparent naming** — function names communicate their side effects. If it panics on double-call, the name says so.
4. **Runtime-specific transport as separate crates** — `rolly-tokio`, `rolly-monoio`, and future runtimes ship their own exporter implementations.
5. **Preserve v1 strengths** — hand-rolled protobuf, zero-alloc hot path, deterministic trace IDs, exemplar-linked metrics, batching, backpressure, sampling.

## 3. Non-Goals

- Changing the protobuf encoding (it works, don't touch it)
- Changing the metrics registry (already runtime-agnostic, already good)
- Converting existing tracing-emitted RED/USE metric events into registry-backed instruments
- Changing the trace ID strategy (deterministic BLAKE3 from request ID)
- Adding gRPC transport (HTTP POST is simpler and sufficient)
- Supporting multi-runtime within a single process

## 4. Architecture

### 4.1 Crate Structure

```
rolly/                          # Core crate — runtime-agnostic, fully sync
├── src/
│   ├── lib.rs                  # Public API: build_layer, collect_*, TelemetrySink, NullSink
│   ├── otlp_layer.rs           # tracing Layer impl (minimal change: Exporter -> TelemetrySink)
│   ├── otlp_trace.rs           # Protobuf encoding for traces (unchanged)
│   ├── otlp_log.rs             # Protobuf encoding for logs (unchanged)
│   ├── otlp_metrics.rs         # Protobuf encoding for metrics (unchanged)
│   ├── proto.rs                # Low-level protobuf wire encoding (unchanged)
│   ├── metrics.rs              # MetricsRegistry, Counter, Gauge, Histogram (unchanged)
│   ├── trace_id.rs             # Deterministic trace ID generation (unchanged)
│   ├── constants.rs            # Field/metric name constants (unchanged)
│   └── use_metrics.rs          # Linux /proc polling + UseMetricsState (see §5.5)

rolly-tokio/                    # Tokio transport crate
├── src/
│   ├── lib.rs                  # TokioExporter, init_global_once, spawn_metrics_loop, re-exports
│   └── exporter.rs             # Current exporter.rs, minimally adapted

rolly-monoio/                   # Monoio transport crate (new)
├── src/
│   ├── lib.rs                  # MonoioExporter, init_global_once, re-exports
│   └── exporter.rs             # Monoio-native channels, HTTP, timers
```

### 4.2 Dependency Flow

```
Application (harrow, rie-worker, harrow-monoio, CLI tool)
    │
    ├── rolly-tokio (or rolly-monoio)     ← runtime-specific transport
    │       │
    │       └── rolly                      ← core: encoding, layers, metrics, trait
    │
    └── tracing-subscriber                 ← app owns the subscriber
```

The application depends on one runtime crate. The runtime crate depends on `rolly` core and re-exports its public API. The application never needs to depend on `rolly` directly (though it can).

`reqwest` also stays out of `rolly` core. It is effectively tokio-coupled in this codebase, so `rolly-monoio` must provide its own HTTP transport rather than reusing the `rolly-tokio` exporter implementation.

### 4.3 Module Coupling in v1 vs v2

**What moves out of `rolly` core:**

| Module | v1 Location | v2 Location | Reason |
|--------|-------------|-------------|--------|
| `exporter.rs` | `rolly/src/` | `rolly-tokio/src/` | tokio::spawn, mpsc, select!, Semaphore |
| `metrics_aggregation_loop` | `rolly/src/lib.rs` | `rolly-tokio/src/lib.rs` | tokio::spawn, tokio::time::interval |
| `TelemetryGuard::drop` flush | `rolly/src/lib.rs` | `rolly-tokio/src/lib.rs` | tokio::runtime::Builder |
| `use_metrics::start` spawn | `rolly/src/use_metrics.rs` | `rolly-tokio/src/lib.rs` | tokio::spawn |

**What is removed from `rolly` (not needed):**

| Module | Reason |
|--------|--------|
| `tower/request.rs` | Tower middleware is framework-specific. Belongs in harrow, not in the telemetry library. |
| `tower/propagation.rs` | Same. W3C traceparent injection is the framework's job. |
| `tower/mod.rs` | Module wrapper for removed tower code. |

**What stays in `rolly` core (unchanged or minimally changed):**

| Module | Why it stays |
|--------|-------------|
| `otlp_layer.rs` | Runtime-agnostic except for the transport handle. v2 makes one mechanical change: `Exporter` becomes `Arc<dyn TelemetrySink>`. |
| `otlp_trace.rs` | Pure protobuf encoding |
| `otlp_log.rs` | Pure protobuf encoding |
| `otlp_metrics.rs` | Pure protobuf encoding |
| `proto.rs` | Pure protobuf wire format |
| `metrics.rs` | std::sync only. No runtime. |
| `trace_id.rs` | Pure BLAKE3/UUID/rand |
| `constants.rs` | Pure constants |
| `use_metrics.rs` | `/proc` parsing and CPU-delta state stay; runtime-specific scheduling moves out |

## 5. Design Details

### 5.1 The TelemetrySink Trait

The core abstraction that decouples signal production from transport. It is the single transport boundary for all three OTLP signals:

```rust
// rolly/src/lib.rs

/// Sink for encoded OTLP protobuf payloads.
///
/// Implementations must be non-blocking. `send_traces()` and `send_logs()`
/// are called from tracing layer callbacks (on_close, on_event) on the
/// application's hot path. `send_metrics()` is called from the runtime-owned
/// metrics aggregation loop. If the underlying channel is full, drop the
/// payload and increment `telemetry_dropped_total()`.
pub trait TelemetrySink: Send + Sync + 'static {
    fn send_traces(&self, data: Vec<u8>);
    fn send_logs(&self, data: Vec<u8>);
    fn send_metrics(&self, data: Vec<u8>);
}
```

`TelemetrySink` is intentionally unified. `OtlpLayer` uses `send_traces()` / `send_logs()`. The runtime helpers (`spawn_metrics_loop()`, `init_global_once()`, and their monoio equivalents) use `send_metrics()`. There is one sink boundary, not a separate "layer-only" interface.

`OtlpLayer` currently holds an `Exporter` directly. In v2, both `OtlpLayer` and `OtlpLayerConfig` switch from `Exporter` to `Arc<dyn TelemetrySink>`. The layer's `on_close` and `on_event` methods remain the same apart from calling through the trait object.

The core crate also provides a no-op sink for stderr-only or test setups:

```rust
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl TelemetrySink for NullSink {
    fn send_traces(&self, _: Vec<u8>) {}
    fn send_logs(&self, _: Vec<u8>) {}
    fn send_metrics(&self, _: Vec<u8>) {}
}
```

The trait is intentionally minimal. No async methods, no flush, no shutdown. Those belong on the concrete exporter type, not the trait.

### 5.2 Core Public API (`rolly`)

```rust
// rolly/src/lib.rs

/// Build telemetry layers without installing a global subscriber.
///
/// Returns a composed tracing layer that the caller adds to their own
/// subscriber registry. The caller owns `.init()` / `.try_init()`.
///
/// If no OTLP endpoints are configured, the sink is unused and may be
/// a no-op implementation.
pub fn build_layer<S>(
    config: &TelemetryConfig,
    sink: Arc<dyn TelemetrySink>,
) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    // Returns: env_filter + optional fmt_layer + optional otlp_layer
    // Does NOT call .init()
    // Does NOT spawn any tasks
    // Does NOT touch the global subscriber
}

/// Collect current metric snapshots and encode as OTLP protobuf.
///
/// Returns the encoded `ExportMetricsServiceRequest` bytes, ready to
/// pass to `TelemetrySink::send_metrics()`. Returns None if no metrics
/// have been recorded since the last collection.
///
/// The caller decides when and how often to call this. Typical usage:
/// call on a timer (e.g. every 10-30s) from whatever scheduling
/// mechanism the runtime provides.
pub fn collect_and_encode_metrics(
    config: &MetricsExportConfig,
) -> Option<Vec<u8>> {
    let snapshots = metrics::global_registry().collect();
    if snapshots.is_empty() {
        return None;
    }
    Some(otlp_metrics::encode_export_metrics_request(
        &config.resource_attrs,
        &config.scope_name,
        &config.scope_version,
        &snapshots,
        config.start_time,
        now(),
    ))
}

/// Stateful baseline for USE metrics polling.
///
/// Carries the previous CPU sample needed to compute utilization as a delta.
/// The first successful poll establishes the baseline.
#[derive(Default)]
pub struct UseMetricsState {
    prev_cpu_ticks: Option<u64>,
    prev_instant: Option<std::time::Instant>,
}

/// Poll USE metrics once.
///
/// Reads `/proc` synchronously, updates `state`, and emits the same
/// metric-shaped `tracing::info!` events v1 emits today. The caller owns
/// scheduling. Only meaningful on Linux; returns immediately on other
/// platforms.
pub fn collect_use_metrics(state: &mut UseMetricsState) {
    use_metrics::poll_once(state);
}
```

### 5.3 Runtime Crate Public API (`rolly-tokio`)

```rust
// rolly-tokio/src/lib.rs

// Re-export everything from rolly core
pub use rolly::*;

/// Tokio-based OTLP exporter.
///
/// Spawns a background tokio task that batches and ships encoded payloads
/// over HTTP. Implements `TelemetrySink` for use with `rolly::build_layer`.
pub struct TokioExporter { /* ... */ }

impl TokioExporter {
    pub fn start(config: ExporterConfig) -> Self { /* tokio::spawn(...) */ }
    pub async fn flush(&self) { /* ... */ }
    pub async fn shutdown(&self) { /* ... */ }
}

impl TelemetrySink for TokioExporter { /* ... */ }

/// Guard that flushes pending telemetry on drop.
///
/// Holds the exporter and any spawned background tasks (metrics
/// aggregation, USE metrics polling). On drop, flushes and shuts
/// down the exporter.
pub struct TelemetryGuard { /* ... */ }

/// Initialize the full telemetry stack and set the global subscriber.
///
/// This is a convenience function for applications that want one-liner
/// telemetry setup. It:
/// 1. Creates a `TokioExporter`
/// 2. Calls `rolly::build_layer()` with it
/// 3. Installs the result as the **global** tracing subscriber
/// 4. Spawns the metrics aggregation loop (if configured)
/// 5. Spawns USE metrics polling (if configured, Linux only)
///
/// # Panics
///
/// Panics if a global tracing subscriber is already set. A process may
/// only have one global subscriber. If you need to compose rolly's
/// layers with other layers, use `build_layer()` instead and call
/// `.init()` yourself.
///
/// For fallible initialization, use `try_init_global()`.
pub fn init_global_once(config: TelemetryConfig) -> TelemetryGuard {
    // ...
}

/// Same as `init_global_once`, but returns an error instead of panicking
/// if a global subscriber is already set.
pub fn try_init_global(
    config: TelemetryConfig,
) -> Result<TelemetryGuard, TryInitError> {
    // ...
}

/// Start the metrics aggregation loop as a background tokio task.
///
/// Calls `rolly::collect_and_encode_metrics()` on the configured
/// interval and sends results through the provided sink.
///
/// Returns a `JoinHandle` the caller can use to abort the task.
pub fn spawn_metrics_loop(
    config: MetricsExportConfig,
    sink: Arc<dyn TelemetrySink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    // ...
}
```

### 5.4 How Callers Use It

**Simple app (one-liner, like today):**

```rust
// Uses rolly-tokio. One line. Panics if called twice — name says so.
let _guard = rolly_tokio::init_global_once(config);
```

**harrow (tokio web framework, composable):**

```rust
let exporter = rolly_tokio::TokioExporter::start(exporter_config);
let sink: Arc<dyn TelemetrySink> = Arc::new(exporter.clone());
let rolly_layer = rolly::build_layer(&telemetry_config, sink.clone());

tracing_subscriber::registry()
    .with(rolly_layer)
    .with(my_custom_layer)      // harrow can add its own layers
    .init();                     // harrow owns the subscriber

// Caller controls the metrics loop
let _metrics = rolly_tokio::spawn_metrics_loop(
    metrics_config, sink, Duration::from_secs(30),
);
```

**harrow-monoio (non-tokio):**

```rust
let exporter = rolly_monoio::MonoioExporter::start(exporter_config);
let sink: Arc<dyn TelemetrySink> = Arc::new(exporter.clone());
let rolly_layer = rolly::build_layer(&telemetry_config, sink.clone());

tracing_subscriber::registry()
    .with(rolly_layer)
    .init();

// Monoio-native metrics loop — caller schedules it however monoio does timers
monoio::spawn(async move {
    let mut interval = monoio::time::interval(Duration::from_secs(30));
    let mut use_state = rolly::UseMetricsState::default();
    loop {
        interval.tick().await;
        if let Some(data) = rolly::collect_and_encode_metrics(&metrics_config) {
            sink.send_metrics(data);
        }
        rolly::collect_use_metrics(&mut use_state);
    }
});
```

**CLI tool (no runtime, sync):**

```rust
// No exporter needed — just stderr logging
let rolly_layer = rolly::build_layer(&config, Arc::new(rolly::NullSink));
tracing_subscriber::registry()
    .with(rolly_layer)
    .init();
```

### 5.5 USE Metrics Refactor

`use_metrics.rs` currently bundles four concerns together:

1. **Reading `/proc/self/stat` / `/proc/self/statm`**
2. **Maintaining prior CPU state** (`prev_cpu_ticks`, `prev_instant`) so utilization can be computed as a delta
3. **Emitting metric-shaped `tracing::info!` events**
4. **Spawning a polling loop** via `tokio::spawn` + `tokio::time::interval`

v2 splits those concerns cleanly:

- **`UseMetricsState`** lives in `rolly` core and carries the previous CPU sample plus a `std::time::Instant`.
- **`rolly::collect_use_metrics(&mut state)`** does one synchronous poll, updates `state`, and emits the same tracing events as v1. The first successful poll establishes the CPU baseline; CPU utilization is emitted starting on the second poll.
- **Spawning/scheduling** moves to the runtime crates (or the application) and calls `collect_use_metrics()` on whatever timer the runtime provides.

This PRD does **not** convert USE metrics into registry-backed gauges. That is orthogonal cleanup; the goal here is runtime decoupling and explicit state ownership.

### 5.6 Tower Middleware Removal

v1 ships Tower middleware (`CfRequestIdLayer`, `PropagationLayer`) inside rolly behind a `tower` feature flag. These are removed in v2.

Tower middleware is framework-specific instrumentation — it belongs in the web framework (harrow), not in the telemetry library. rolly's job is encoding, layering, and transport. Request ID extraction, span creation around HTTP calls, and W3C traceparent injection are the framework's responsibility.

The tower middleware code can be moved to harrow or a separate crate if needed. The `trace_id` module (deterministic BLAKE3 hashing, span ID generation) stays in rolly core — it is not tower-specific.

### 5.7 TelemetryGuard Refactor

v1's `TelemetryGuard::drop()` builds a one-off tokio current-thread runtime to block on flush. This is a tokio dependency in the core crate.

v2: `TelemetryGuard` moves to the runtime crates. Each runtime crate provides its own guard that knows how to flush using its runtime. The core crate does not have a guard — it doesn't need one because it doesn't own the exporter.

### 5.8 ExporterConfig Exposure

v1 hardcodes exporter parameters inside `init()`:

```rust
channel_capacity: 1024,
batch_size: 512,
flush_interval: Duration::from_secs(1),
max_concurrent_exports: 4,
```

v2: `ExporterConfig` is a public struct on the runtime crate. Callers can tune these. `TelemetryConfig` provides sensible defaults for the convenience `init_global_once` path.

```rust
// rolly-tokio/src/lib.rs

pub struct ExporterConfig {
    pub traces_url: Option<String>,
    pub logs_url: Option<String>,
    pub metrics_url: Option<String>,
    pub channel_capacity: usize,        // default: 1024
    pub batch_size: usize,              // default: 512
    pub flush_interval: Duration,       // default: 1s
    pub max_concurrent_exports: usize,  // default: 4
    pub backpressure_strategy: BackpressureStrategy,
}
```

## 6. Migration Path

### 6.1 For rolly Users

| v1 | v2 | Change Required |
|----|-----|-----------------|
| `rolly::init(config)` | `rolly_tokio::init_global_once(config)` | Rename + change dep |
| `rolly::TelemetryConfig` | `rolly::TelemetryConfig` (core) | No change (re-exported) |
| `rolly::counter/gauge/histogram` | `rolly::counter/gauge/histogram` | No change (re-exported) |
| `rolly::request_layer()` | Removed. Move to harrow or application code. | Remove usage or vendor |
| `rolly::propagation_layer()` | Removed. Move to harrow or application code. | Remove usage or vendor |
| `rolly::telemetry_dropped_total()` | `rolly::telemetry_dropped_total()` | No change |

### 6.2 Version Strategy

All crates in the workspace share a single version number. When any crate ships, they all ship at the same version. This keeps the compatibility matrix trivial: `rolly` 0.12 always works with `rolly-tokio` 0.12.

All releases are pre-1.0 until the full trio (`rolly` + `rolly-tokio` + `rolly-monoio`) ships together. The 1.0 release is the complete runtime-agnostic platform.

| Phase | Version | What ships |
|-------|---------|------------|
| Phase 1 | 0.11.0 | `rolly` only. Trait + `build_layer` added, tower removed, `init()` deprecated. Non-breaking (except tower removal). |
| Phase 2 | 0.12.0 | `rolly` + `rolly-tokio`. Workspace created. Exporter moved. tokio removed from core. `init()` removed. |
| Phase 3 | 1.0.0 | `rolly` + `rolly-tokio` + `rolly-monoio`. Complete platform. Stable API. |

Use `workspace.package.version` in the root `Cargo.toml` and `version.workspace = true` in each member. Runtime crates depend on `rolly` at the same version via `rolly = { version = "=X.Y.Z", path = "../rolly" }`.

## 7. Implementation Order

### Phase 1: Core prep (non-breaking except tower removal)

1. Define `TelemetrySink` trait and `NullSink` in `rolly/src/lib.rs`
2. Implement `TelemetrySink` for existing `Exporter`
3. Change `OtlpLayer` and `OtlpLayerConfig` to hold `Arc<dyn TelemetrySink>` instead of `Exporter`
4. Add a test-only `MockTelemetrySink` backed by `std::sync::mpsc` and migrate `otlp_layer.rs` unit tests off `Exporter::start_test()`
5. Add `build_layer()` function
6. Add `collect_and_encode_metrics()` function
7. Split `use_metrics.rs` into `UseMetricsState` + `poll_once(&mut state)` + the existing tokio spawn wrapper
8. Remove tower middleware (`tower/` directory, `tower` feature flag, tower deps from Cargo.toml)
9. Existing `init()` still works — calls `build_layer()` internally and keeps existing tokio scheduling, marked deprecated

All existing tests pass (minus tower tests which are removed). Ship as 0.11.0.

### Phase 2: Create `rolly-tokio` and remove tokio from core

1. Create `rolly-tokio/` workspace member
2. Move `exporter.rs` to `rolly-tokio/src/exporter.rs`
3. Implement `TokioExporter` wrapping existing exporter logic
4. Implement `init_global_once()` and `try_init_global()`
5. Implement `spawn_metrics_loop()`
6. Implement `TelemetryGuard` with tokio-aware drop
7. Implement USE metrics spawning in `init_global_once`
8. Make `ExporterConfig` public with defaults
9. Re-export `rolly::*`
10. Migrate tokio integration tests to `rolly-tokio/tests/`
11. Migrate tokio benchmarks to `rolly-tokio/benches/`
12. Remove `exporter.rs`, `init()`, `TelemetryGuard`, `metrics_aggregation_loop` from core
13. Remove `tokio`, `reqwest`, `bytes` from `rolly/Cargo.toml`
14. Remove `tokio::spawn` from `use_metrics.rs`
15. Verify `rolly` compiles with zero runtime dependencies

Ship all workspace crates as 0.12.0. The core is now runtime-agnostic.

### Phase 3: Create `rolly-monoio` and release 1.0

1. Create `rolly-monoio/` workspace member
2. Implement `MonoioExporter` using monoio channels and HTTP client
3. Implement `init_global_once()` and `try_init_global()`
4. Implement `TelemetryGuard` with monoio-aware drop
5. Integration tests with monoio runtime

Ship all workspace crates as 1.0.0. The complete platform.

## 8. What Stays Exactly The Same

These are the strengths of v1 that v2 preserves without modification:

- **Hand-rolled protobuf** — `proto.rs`, `otlp_trace.rs`, `otlp_log.rs`, `otlp_metrics.rs`. Zero code generation, minimal allocations, proven wire-compatible.
- **Metrics registry** — `metrics.rs`. Already runtime-agnostic. std::sync primitives, cardinality limits, exemplar capture from tracing spans.
- **Deterministic trace IDs** — `trace_id.rs`. BLAKE3 from CloudFront request ID, UUID v4 fallback.
- **Sampling** — Deterministic, trace-ID-based, inherited from parent spans. Lives in `otlp_layer.rs`.
- **OtlpLayer tracing integration** — `otlp_layer.rs`. The `Layer<S>` impl, field collection, span extensions, event encoding. Only change: holds `Arc<dyn TelemetrySink>` instead of `Exporter` directly.
- **Backpressure strategy** — Drop-newest, non-blocking `try_send`. The strategy is the same; only the channel implementation varies by runtime.
- **Batching and concurrent export** — The logic moves to `rolly-tokio` but the algorithm (512-item batches, 1s flush, 4 concurrent workers, exponential backoff) is preserved.
- **All benchmarks** — Encoding and metrics benchmarks are pure. Hot-path and exporter benchmarks move to `rolly-tokio` dev-dependencies.

## 9. Risks

| Risk | Mitigation |
|------|------------|
| `Arc<dyn TelemetrySink>` adds vtable dispatch on hot path | Measure. The send methods do a `try_send` into a channel — the vtable indirection is noise compared to the channel operation. Benchmark before/after. |
| `reqwest` cannot be reused for `rolly-monoio` | Treat the exporter algorithm as reusable and the HTTP transport as runtime-specific. `rolly-monoio` should use a monoio-compatible client or a thin custom OTLP `POST` implementation. |
| monoio HTTP client ecosystem is less mature than reqwest | Start with a minimal HTTP client. OTLP export is a simple `POST` with `application/x-protobuf`. No TLS negotiation complexity if running inside a VPC. |
| Workspace coordination between 3 crates | Use a Cargo workspace. Pin `rolly` dependency in runtime crates to exact minor version. |
| Users forget to spawn metrics loop when using `build_layer` | Document clearly. The `init_global_once` convenience path handles metrics export and USE polling automatically. `build_layer` users must own both the metrics timer and any `UseMetricsState` they opt into. |
| Tower middleware removal breaks downstream | Tower code is small and self-contained. Downstream (harrow) can vendor the ~350 lines or the code can be published as a separate crate. Announce in CHANGELOG. |
