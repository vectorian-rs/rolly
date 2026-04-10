//! Tokio-based transport for the rolly observability crate.
//!
//! Provides the exporter, `init_global_once`, and `TelemetryGuard` for
//! batching and shipping OTLP telemetry over HTTP using tokio.

pub mod exporter;

// Re-export everything from rolly core
pub use rolly::*;
