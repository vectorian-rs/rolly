# PRD: rolly v3 — Current State and Path to 1.0

**Version:** 3.1
**Date:** 2026-04-11
**Status:** Active
**Predecessor:** [prd-rolly-v2.md](prd-rolly-v2.md)
**Repository:** [github.com/vectorian-rs/rolly](https://github.com/vectorian-rs/rolly)
**License:** MIT
**Current workspace version:** 0.15.0

---

## 1. What rolly Is

rolly is a lightweight Rust observability library. Hand-rolled OTLP protobuf encoding over HTTP, built on `tracing`. 7 direct dependencies vs ~120 for the official OTel SDK, with 3x faster metrics hot path. Three crates in a workspace:

- **`rolly`** — runtime-agnostic core. Encoding, metrics registry, tracing layer, trace ID generation. Zero runtime dependencies.
- **`rolly-tokio`** — tokio transport. Batching HTTP exporter via reqwest, `TelemetryGuard`, convenience init functions.
- **`rolly-monoio`** — monoio transport. Batching HTTP exporter via ureq on OS threads, cooperative shutdown.

All three share a single workspace version via `[workspace.dependencies]`.

## 2. Current State (0.15.0 released)

### 2.1 Release History

| Version | Date | What shipped |
|---------|------|-------------|
| 0.11.0 | 2026-04-10 | `TelemetrySink` trait, `build_layer()`, tower removed |
| 0.12.0 | 2026-04-10 | `rolly-tokio` created, exporter moved, core runtime-agnostic |
| 0.13.0 | 2026-04-10 | `rolly-monoio` created, crossbeam channels, raw TCP |
| 0.14.0 | 2026-04-10 | Monoio ureq transport, TLS support |
| 0.15.0 | 2026-04-11 | OTel semantic fields, correctness hardening, CI pipeline |

Earlier work (pre-0.11): formal verification (Kani 19 proofs, TLA+ 2 specs), fuzzing (3 targets), property-based testing (9 proptests), HashDoS fix, mutex contention fix.

### 2.2 What 0.15.0 Delivered

**Features:**
- OTel semantic span fields: `otel.kind`, `otel.status_code`, `otel.status_message`
- Auto-generated trace IDs for root spans (UUID v4 fallback)
- `TelemetryGuard::shutdown()` async method for deterministic drain
- `init_global_once` graceful handling of subscriber-already-set
- Monoio: separate data/control channels, drain quota, `BatchState`/`BatchConfig`
- Monoio: `max_pending_batches` config (bounded memory under slow collector)
- Monoio: `max_concurrent_exports` config
- Monoio: cooperative shutdown for background loops
- Final metrics flush on shutdown (last interval no longer lost)
- GitHub Actions CI pipeline (check, fmt, clippy, test)
- E2E exemplar propagation test
- Monoio init-time URL validation

**Correctness fixes:**
- Tokio `shutdown()` now waits for exporter loop (was fire-and-forget)
- Tokio exporter: semaphore acquired before spawning (was unbounded JoinSet)
- Tokio exporter: `BatchState`/`BatchConfig` refactor (parity with monoio)
- Monoio flush/shutdown no longer starves under sustained load
- `InFlightGuard` panic safety on monoio worker threads
- Lock poisoning in metrics registry recovers instead of cascading panics
- Non-finite histogram boundaries filtered; non-finite observations rejected
- Counter u64→i64 clamped with `saturating_add`
- Hash collision detection in metrics (attrs equality check)
- Event parent context: uses `event.parent()` then `ctx.lookup_current()`
- `otel.*` fields work via `record_debug` path (`%value`)
- `scope_name` changed from `"pz-o11y"` to `"rolly"`
- `eprintln!` prefix `"pz-o11y"` → `"rolly-tokio"` in tokio exporter
- `max_concurrent_exports` clamped to >= 1 (both runtimes)

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
├── verification/
│   ├── specs/                      # TLA+ specifications (exporter, metrics_registry)
│   └── fuzz/                       # cargo-fuzz targets (hex, trace, log encoding)

rolly-tokio/                        # Tokio transport
├── src/
│   ├── lib.rs                      # TokioExporter, init_global_once, TelemetryGuard, spawn_metrics_loop
│   └── exporter.rs                 # BatchState/BatchConfig, bounded semaphore export

rolly-monoio/                       # Monoio transport
├── src/
│   ├── lib.rs                      # MonoioExporter, init_global_once, TelemetryGuard
│   └── exporter.rs                 # BatchState/BatchConfig, data/control channels, ureq on OS threads

.github/workflows/ci.yml            # GitHub Actions: check, fmt, clippy, test
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

harrow's `.o11y()` convenience method calls `rolly_tokio::init_global_once()`, which sets the global subscriber. When rie-worker initializes its own `tracing_subscriber::fmt()` before calling `.o11y()`, the second subscriber installation panics.

### 4.3 Fixes Applied

**In rolly (0.15.0):** `init_global_once` catches `SubscriberAlreadySet` and returns a no-op guard with a warning. Safety net only — when this triggers, the rolly OTLP layer is not installed, so no traces/logs are exported.

**Recommended in harrow:** Split `.o11y()` into `.o11y(config)` (full setup) and `.o11y_middleware(config)` (middleware + state only). `.o11y()` calls `.o11y_middleware()` internally.

## 5. Design Details

### 5.1 TelemetrySink Trait

```rust
pub trait TelemetrySink: Send + Sync + 'static {
    fn send_traces(&self, data: Vec<u8>);
    fn send_logs(&self, data: Vec<u8>);
    fn send_metrics(&self, data: Vec<u8>);
}
```

Intentionally minimal. No async methods, no flush, no shutdown. Implementations must be non-blocking.

### 5.2 OtlpLayer — OTel Semantic Fields

| tracing field | OTLP field | Values | Default |
|---------------|-----------|--------|---------|
| `otel.kind` | `SpanKind` | server, client, producer, consumer, internal | Internal |
| `otel.status_code` | `StatusCode` | ok, error, unset | Unset |
| `otel.status_message` | `SpanStatus.message` | any string | None |

These work via both `record_str` and `record_debug` paths (shared `record_field()` helper). They are consumed by the layer and do not appear as span attributes.

### 5.3 Trace ID Resolution

1. **Parent inheritance** — child spans always use parent's trace ID
2. **Explicit `trace_id` field** — 32-char hex string on the span
3. **Random generation** — root spans without `trace_id` get UUID v4

### 5.4 Event Parent Resolution

`on_event` uses `event.parent()` first (explicit parent), then falls back to `ctx.lookup_current()` (thread-local current span). This correctly handles `tracing::info!(parent: &span, ...)` cross-thread events.

### 5.5 Metrics Safety

- **Lock poisoning**: all `RwLock`/`Mutex` acquisitions recover via `unwrap_or_else(|p| p.into_inner())` — observability never crashes the application
- **Hash collisions**: `attrs_match()` verifies attribute equality after hash lookup; collisions are silently dropped, not merged
- **Counter overflow**: `u64` input clamped to `i64::MAX`, accumulator uses `saturating_add`
- **NaN/Infinity**: non-finite histogram boundaries filtered at construction; non-finite observations rejected in `observe()`
- **Cardinality**: per-metric limits with once-per-metric overflow warning

### 5.6 TelemetryGuard

Both runtime crates provide `TelemetryGuard` with:
- **`Drop`**: best-effort flush (background tasks aborted/signaled first, final metrics collected)
- **`async fn shutdown(self)`**: deterministic drain with ack from exporter loop
- **Final metrics flush**: `collect_and_encode_metrics` called before exporter shutdown so the last interval is not lost

### 5.7 Exporter Architecture (both runtimes)

Both exporters use the same `BatchState`/`BatchConfig` pattern:

```rust
struct BatchConfig {
    traces_url: Option<String>,
    logs_url: Option<String>,
    metrics_url: Option<String>,
    batch_size: usize,
    // monoio also has: max_pending_batches
}

struct BatchState {
    traces: Vec<Bytes>,
    logs: Vec<Bytes>,
    metrics: Vec<Bytes>,
    // tokio: JoinSet<()>
    // monoio: VecDeque<PendingPost>
}
```

Methods: `collect()`, `flush_all()`, `drain()` (tokio) / `drain_from()` (monoio).

**Tokio**: semaphore permit acquired *before* spawning (`try_acquire_owned`) — prevents unbounded JoinSet growth. Batches left for next flush cycle when all permits busy.

**Monoio**: data/control channel separation, `DRAIN_QUOTA` (1024) per iteration to prevent flush starvation, `max_pending_batches` (default 32) for memory bounding, `InFlightGuard` for panic-safe worker thread cleanup.

### 5.8 ExporterConfig

```rust
pub struct ExporterConfig {
    pub traces_url: Option<String>,
    pub logs_url: Option<String>,
    pub metrics_url: Option<String>,
    pub channel_capacity: usize,          // default: 1024
    pub batch_size: usize,                // default: 512
    pub flush_interval: Duration,         // default: 1s
    pub max_concurrent_exports: usize,    // default: 4 (clamped >= 1)
    pub max_pending_batches: usize,       // default: 32 (monoio only)
    pub backpressure_strategy: BackpressureStrategy,
}
```

### 5.9 Error Types

**rolly-tokio `InitError`**: `SubscriberAlreadySet` | `Exporter(StartError)` — tokio's `Exporter::start()` returns `Result`.

**rolly-monoio `InitError`**: `SubscriberAlreadySet` only — monoio's `Exporter::start()` is infallible (URL validation deferred to POST time with init-time warning). Both are `#[non_exhaustive]`.

## 6. What's Preserved from v1

- **Hand-rolled protobuf** — zero code generation, proven wire-compatible (19 Kani proofs + prost/opentelemetry-proto roundtrip tests)
- **Metrics registry** — std::sync only, cardinality limits, exemplar capture, collision-safe
- **Deterministic trace IDs** — BLAKE3 from request ID when provided, UUID v4 fallback
- **Sampling** — deterministic, trace-ID-based, inherited from parent spans
- **Backpressure** — drop-newest, non-blocking `try_send` with bounded export queues
- **Batching** — 512-item batches, 1s flush, 4 concurrent workers

## 7. What Remains Before 1.0

### 7.1 Next Release (0.16.0)

- [ ] **`trace_id` attribute duplication** — decide if keeping `trace_id` as both OTLP trace ID and span attribute is intentional (backend visibility) or should be removed
- [ ] **`OtlpLayerConfig` scope name/version configurable** — currently hardcoded to `"rolly"` + `CARGO_PKG_VERSION`. Add fields to `OtlpLayerConfig` and `LayerConfig`
- [ ] **Block backpressure strategy** — `BackpressureStrategy::Block` with configurable timeout for reliability-critical systems

### 7.2 Pre-1.0 Gate

- [ ] **API audit** — review all public types/functions for naming consistency and semver stability
- [ ] **Span events** — OTLP spans carry events (logs attached to spans). Currently events are standalone log records. Decide if needed for 1.0
- [ ] **Span links** — OTLP supports span links. Decide if needed for 1.0
- [ ] **README** — usage examples for all three crates, OTel field semantics, migration guide
- [ ] **Downstream verification** — verify harrow + rie-worker + harrow-monoio work with final API

### 7.3 Non-Goals for 1.0

- gRPC transport (HTTP POST is simpler and sufficient)
- Multi-runtime within a single process
- Converting tracing-emitted metric events into registry-backed instruments
- Changing the protobuf encoding
- Changing the metrics registry internals

## 8. Version Strategy

All crates share a single workspace version via `[workspace.dependencies]`.

| Version | What ships |
|---------|-----------|
| 0.15.0 | **Shipped.** OTel semantic fields, correctness hardening, CI pipeline |
| 0.16.0 | Scope name configurable, Block backpressure, trace_id decision |
| 1.0.0 | Stable API. All three crates published together. |

## 9. Risks

| Risk | Mitigation |
|------|------------|
| `init_global_once` no-op guard silently disables telemetry | Warning is logged. `build_layer()` is the recommended path for frameworks. |
| Only `Drop` backpressure — no guaranteed delivery option | Block strategy planned for 0.16.0. |
| Manual protobuf correctness burden | 19 Kani proofs + wire-compat tests + 3 fuzz targets. |
| Rapid version velocity (v0.1–v0.15 in 5 weeks) | Intentional pre-1.0. API stabilization period before 1.0. |
| No visible CI pipeline | **Fixed** — GitHub Actions added in 0.15.0. |
| Monoio deferred URL validation | **Mitigated** — init-time warning added in 0.15.0. |

## 10. Verification

| Category | Count | Location |
|----------|-------|----------|
| Kani proof harnesses | 19 | `proto.rs` (11), `metrics.rs` (4), `otlp_layer.rs` (3), `trace_id.rs` (1) |
| TLA+ specifications | 2 | `verification/specs/` (exporter, metrics_registry) |
| cargo-fuzz targets | 3 | `verification/fuzz/` (hex, trace, log encoding) |
| Proptests | 9 | Across core modules |
| Unit tests | 120+ | `rolly` core |
| Integration tests | 30+ | `rolly-tokio` (18 lib + e2e + shutdown + exemplar), `rolly-monoio` (7 e2e + 5 lib) |
| Wire-compat tests | Yes | rolly encodes → prost/opentelemetry-proto decodes |
| CI pipeline | Yes | `.github/workflows/ci.yml` |
