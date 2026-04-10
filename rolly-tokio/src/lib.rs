//! Tokio-based transport for the rolly observability crate.
//!
//! Provides [`TokioExporter`] for batching and shipping OTLP telemetry
//! over HTTP, plus convenience functions for one-liner telemetry setup.

mod exporter;

// Re-export the public API from the exporter module
pub use exporter::Exporter as TokioExporter;
pub use exporter::ExporterConfig;

/// Guard that flushes pending telemetry on drop.
///
/// Hold this in your main function to ensure all spans are exported
/// before shutdown. Created by [`init_global_once`] or manually.
pub struct TelemetryGuard {
    exporter: Option<exporter::Exporter>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(ref exporter) = self.exporter {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(rt) = rt {
                rt.block_on(async {
                    exporter.flush().await;
                    exporter.shutdown().await;
                });
            }
        }
    }
}

// Re-export everything from rolly core
pub use rolly::*;
