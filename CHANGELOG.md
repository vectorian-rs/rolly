# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.13.0] - 2026-04-10

### Added
- **New crate: `rolly-monoio`** — monoio-based transport for thread-per-core runtimes
  - `MonoioExporter` using crossbeam channels and raw TCP HTTP/1.1 POST
  - `init_global_once()` / `try_init_global()` for monoio runtime context
  - `TelemetryGuard` with best-effort non-blocking drop
  - Adaptive poll loop (10ms active, 100ms idle) to reduce CPU usage at idle
  - 21 tests including e2e roundtrip and backpressure validation

### Changed
- `shutdown()` in rolly-monoio now waits for drain completion (was fire-and-forget)
- `#[non_exhaustive]` added to `BackpressureStrategy`, `StartError`, and `InitError` across all crates — adding variants in future releases is no longer semver-breaking

## [0.12.0] - 2026-04-10

### Added
- **New crate: `rolly-tokio`** — tokio-based transport for rolly observability
  - `TokioExporter` — batching HTTP exporter with retry and concurrency control
  - `ExporterConfig` with `Default` impl (1024 channel, 512 batch, 1s flush, 4 concurrent)
  - `init_global_once()` / `try_init_global()` — one-liner telemetry setup
  - `spawn_metrics_loop()` — background metrics aggregation
  - `TelemetryGuard` — graceful flush/shutdown on drop, safe in async contexts
  - `InitError` / `StartError` — proper error types (no panics in fallible paths)
- `increment_dropped_total()` in rolly core — shared drop counter for all exporter implementations
- `BackpressureStrategy` moved to rolly core (runtime-agnostic)
- `_bench` feature on `rolly-tokio` with bench module re-exporting core + exporter internals

### Changed
- **Breaking:** `rolly` core no longer depends on `tokio`, `reqwest`, or `bytes` — zero runtime dependencies
- **Breaking:** `Exporter::start()` returns `Result<Self, StartError>` instead of panicking
- **Breaking:** `try_init_global()` returns `Result<TelemetryGuard, InitError>` (was `TryInitError`)
- `TelemetryGuard::drop` uses `block_in_place` inside multi-thread runtimes (no more "runtime within runtime" panic)
- Migrated tokio-dependent benchmarks (`hot_path`, `realistic_scenario`, `generate_flamecharts`) to `rolly-tokio`
- Migrated tokio-dependent tests (`backpressure`, `sampling_zero_rate`) to `rolly-tokio`
- E2e tests now use `rolly_tokio::init_global_once()` instead of deprecated `rolly::init()`

### Removed
- **Breaking:** `rolly::init()` — use `rolly_tokio::init_global_once()` or `rolly::build_layer()`
- **Breaking:** `rolly::TelemetryGuard` (core version) — use `rolly_tokio::TelemetryGuard`
- **Breaking:** `rolly::exporter` module — exporter lives in `rolly-tokio` now
- `tokio`, `reqwest`, `bytes` removed from rolly core dependencies
- `metrics_aggregation_loop` removed from core
- `use_metrics::start()` / `poll_loop()` removed from core (use `collect_use_metrics()` + your own scheduler)

## [0.11.0] - 2026-04-10

### Added
- `TelemetrySink` trait — runtime-agnostic transport boundary for OTLP signals
- `NullSink` — no-op sink for stderr-only setups, CLI tools, and tests
- `build_layer()` + `LayerConfig` — composable layer construction without installing a global subscriber
- `collect_and_encode_metrics()` + `MetricsExportConfig` — runtime-agnostic metrics collection and OTLP encoding
- `UseMetricsState` + `collect_use_metrics()` — runtime-agnostic USE metrics polling with explicit state ownership
- Custom `resource_attributes` field on `TelemetryConfig` for user-defined OTLP resource key-value pairs

### Changed
- `OtlpLayer` now holds `Arc<dyn TelemetrySink>` instead of `Exporter` directly
- `OtlpLayerConfig.exporter` renamed to `OtlpLayerConfig.sink` (takes `Arc<dyn TelemetrySink>`)
- `metrics_aggregation_loop` refactored to use `collect_and_encode_metrics()` internally
- `use_metrics` refactored: `poll_loop` now delegates to `poll_once(&mut UseMetricsState)`

### Deprecated
- `init()` — use `build_layer()` for composable setups, or `rolly_tokio::init_global_once()` when available

### Removed
- **Breaking:** Tower middleware (`CfRequestIdLayer`, `PropagationLayer`) — moved to downstream frameworks
- **Breaking:** `tower` feature flag and `tower`, `http`, `pin-project-lite` dependencies

## [0.9.0] - 2026-03-17

### Added
- Configurable `BackpressureStrategy` (drop / block) for the exporter
- Vector-based end-to-end integration tests
- TLA+ formal specifications for exporter and metrics registry
- cargo-fuzz targets for hex, trace, and log encoding
- Proptest strategies for trace, log, and hex encoding
- Kani proof harnesses for the metrics module
- Remaining practical tests for v1.0.0 readiness
- Verification coverage SVG diagram

### Changed
- Consolidated project layout and applied diagram design system
- Replaced plotters dependency with hand-crafted SVG for architecture diagram
- Excluded non-essential files (`.agent-prompts/`, `.claude/`, `docs/prds/`) from crate package

### Fixed
- Tightened Kani proof harness bounds for tractable verification

## [0.5.1] - 2026-03-10

### Changed
- Updated crates.io README

## [0.5.0] - 2026-03-10

### Added
- Histogram instrument with client-side bucketing
- Exemplar support on Counter, Gauge, and Histogram
- Realistic e-commerce benchmark suite
- `Arc<Vec>` zero-copy collect path and SVG baseline charts
- Configurable cardinality limiting for metrics
- Optimized hashing (blake3) for metric label sets
- Allocation scaling benchmark with SVG table visualization
- 8/10/16-attribute counter and histogram comparison benchmarks
- Baseline and diff flamechart generation

### Changed
- **Breaking:** `TelemetryConfig` fields now accept `String` instead of `&'static str`
- Renamed crate from `ro11y` to `rolly`
- Moved benchmark-methodology.md to `docs/`
- Use Criterion as single source of truth for benchmark methodology

## [0.4.0] - 2026-03-08

### Added
- Deterministic head-based trace sampling with configurable `sampling_rate`
- Kani proofs, proptest, and wire-compatibility test infrastructure

## [0.3.0] - 2026-03-07

### Added
- Native OTLP metrics with Counter and Gauge instruments
- Metrics benchmarks for encoding, counter/gauge ops, and collect
- Proto/trace/log encoding benchmarks

### Changed
- **Breaking:** metrics encoding uses hand-rolled protobuf (no `prost` at runtime)

## [0.2.0] - 2026-03-07

### Changed
- **Breaking:** independent trace and log export configuration — separate endpoint URLs for traces and logs

## [0.1.3] - 2026-03-06

### Added
- Shared field/metric constants module
- Tower middleware feature-gated behind `cfg(feature = "tower")`

### Changed
- Renamed `cf_id` to `request_id` and made `hex_encode` public

## [0.1.0] - 2026-03-05

### Added
- Initial release of the rolly observability crate
- Hand-rolled OTLP protobuf encoding for traces and logs over HTTP
- `tracing`-based `OtlpLayer` subscriber
- Async batch exporter with configurable endpoints
- crates.io packaging and metadata

[Unreleased]: https://github.com/vectorian-rs/rolly/compare/v0.13.0...HEAD
[0.13.0]: https://github.com/vectorian-rs/rolly/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/vectorian-rs/rolly/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/vectorian-rs/rolly/compare/v0.9.0...v0.11.0
[0.9.0]: https://github.com/vectorian-rs/rolly/compare/v0.5.1...v0.9.0
[0.5.1]: https://github.com/vectorian-rs/rolly/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/vectorian-rs/rolly/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/vectorian-rs/rolly/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/vectorian-rs/rolly/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/vectorian-rs/rolly/compare/v0.1.3...v0.2.0
[0.1.3]: https://github.com/vectorian-rs/rolly/compare/v0.1.0...v0.1.3
[0.1.0]: https://github.com/vectorian-rs/rolly/releases/tag/v0.1.0
