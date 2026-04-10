//! Tokio-based transport for the rolly observability crate.
//!
//! Provides [`TokioExporter`] for batching and shipping OTLP telemetry
//! over HTTP, plus convenience functions for one-liner telemetry setup.

// Re-export everything from rolly core
pub use rolly::*;
