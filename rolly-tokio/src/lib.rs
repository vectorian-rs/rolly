//! Tokio-based transport for the rolly observability crate.
//!
//! Provides [`TokioExporter`] for batching and shipping OTLP telemetry
//! over HTTP, plus convenience functions for one-liner telemetry setup.

mod exporter;

// Re-export the public API from the exporter module
pub use exporter::Exporter as TokioExporter;
pub use exporter::ExporterConfig;

// Re-export everything from rolly core
pub use rolly::*;
