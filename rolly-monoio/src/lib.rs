//! Monoio-based transport for the rolly observability crate.
//!
//! Provides [`MonoioExporter`] for batching and shipping OTLP telemetry
//! over HTTP using raw TCP, plus convenience functions for one-liner
//! telemetry setup.

mod exporter;

// Re-export the public API from the exporter module
pub use exporter::ExportMessage;
pub use exporter::Exporter as MonoioExporter;
pub use exporter::ExporterConfig;
pub use exporter::StartError;

/// Guard that flushes pending telemetry on drop.
///
/// Hold this in your main function to ensure all spans are exported
/// before shutdown. Created by [`init_global_once`] or manually.
///
/// Because monoio is single-threaded and does not expose a
/// `Handle::try_current()` equivalent, the Drop implementation performs
/// a best-effort non-blocking shutdown: it sends a Shutdown message
/// through the channel. If called from within the monoio runtime, the
/// background task will process remaining messages before exiting.
pub struct TelemetryGuard {
    exporter: Option<exporter::Exporter>,
}

impl From<exporter::Exporter> for TelemetryGuard {
    fn from(exporter: exporter::Exporter) -> Self {
        Self {
            exporter: Some(exporter),
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(ref exporter) = self.exporter {
            // Best-effort non-blocking shutdown. Sends a Shutdown message
            // through the crossbeam channel via try_send. The background
            // monoio task will drain pending batches before exiting.
            // We cannot await or block here without risking deadlock on
            // the single-threaded monoio event loop.
            exporter.request_shutdown();
        }
    }
}

// Re-export everything from rolly core
pub use rolly::*;

#[cfg(feature = "_bench")]
#[doc(hidden)]
pub mod bench {
    pub use crate::exporter::{ExportMessage, Exporter, ExporterConfig};
    pub use rolly::bench::*;
}

use std::sync::Arc;
use std::time::Duration;

/// Errors returned by [`try_init_global`].
#[derive(Debug)]
#[non_exhaustive]
pub enum InitError {
    /// A global tracing subscriber is already set.
    SubscriberAlreadySet(tracing_subscriber::util::TryInitError),
    /// The exporter could not be started.
    Exporter(StartError),
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SubscriberAlreadySet(e) => write!(f, "global subscriber already set: {}", e),
            Self::Exporter(e) => write!(f, "failed to start exporter: {}", e),
        }
    }
}

impl std::error::Error for InitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SubscriberAlreadySet(e) => Some(e),
            Self::Exporter(e) => Some(e),
        }
    }
}

impl From<tracing_subscriber::util::TryInitError> for InitError {
    fn from(e: tracing_subscriber::util::TryInitError) -> Self {
        Self::SubscriberAlreadySet(e)
    }
}

impl From<StartError> for InitError {
    fn from(e: StartError) -> Self {
        Self::Exporter(e)
    }
}

/// Initialize the full telemetry stack and set the global subscriber.
///
/// Creates a [`MonoioExporter`], calls [`rolly::build_layer()`], installs the
/// global subscriber, and spawns background tasks for metrics aggregation.
///
/// # Panics
///
/// Panics if initialization fails for any reason. For fallible
/// initialization, use [`try_init_global`].
///
/// # Requirements
///
/// Must be called from within a monoio runtime context.
pub fn init_global_once(config: TelemetryConfig) -> TelemetryGuard {
    match try_init_global(config) {
        Ok(guard) => guard,
        Err(e) => panic!("failed to initialize telemetry: {}", e),
    }
}

/// Same as [`init_global_once`], but returns an error instead of panicking.
///
/// # Errors
///
/// Returns [`InitError::Exporter`] if the exporter cannot be started.
/// Returns [`InitError::SubscriberAlreadySet`] if a global tracing subscriber
/// is already installed.
pub fn try_init_global(config: TelemetryConfig) -> Result<TelemetryGuard, InitError> {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    let export_traces = config.otlp_traces_endpoint.is_some();
    let export_logs = config.otlp_logs_endpoint.is_some();
    let export_metrics = config.otlp_metrics_endpoint.is_some();

    let exporter = if export_traces || export_logs || export_metrics {
        let traces_url = config
            .otlp_traces_endpoint
            .as_deref()
            .map(|ep| format!("{}/v1/traces", ep));
        let logs_url = config
            .otlp_logs_endpoint
            .as_deref()
            .map(|ep| format!("{}/v1/logs", ep));
        let metrics_url = config
            .otlp_metrics_endpoint
            .as_deref()
            .map(|ep| format!("{}/v1/metrics", ep));
        Some(exporter::Exporter::start(ExporterConfig {
            traces_url,
            logs_url,
            metrics_url,
            backpressure_strategy: config.backpressure_strategy,
            ..ExporterConfig::default()
        })?)
    } else {
        None
    };

    let sink: Arc<dyn TelemetrySink> = match &exporter {
        Some(exp) => Arc::new(exp.clone()),
        None => Arc::new(NullSink),
    };

    let layer_config = LayerConfig {
        log_to_stderr: config.log_to_stderr,
        export_traces,
        export_logs,
        service_name: config.service_name.clone(),
        service_version: config.service_version.clone(),
        environment: config.environment.clone(),
        resource_attributes: config.resource_attributes.clone(),
        sampling_rate: config.sampling_rate.unwrap_or(1.0),
    };

    let layer = rolly::build_layer(&layer_config, sink.clone());

    tracing_subscriber::registry().with(layer).try_init()?;

    tracing::info!(
        service.name = layer_config.service_name.as_str(),
        service.version = layer_config.service_version.as_str(),
        environment = layer_config.environment.as_str(),
        "telemetry initialized"
    );

    // Spawn metrics aggregation loop
    if export_metrics {
        let flush_interval = config
            .metrics_flush_interval
            .unwrap_or(Duration::from_secs(10));
        let metrics_config = MetricsExportConfig {
            service_name: config.service_name,
            service_version: config.service_version,
            environment: config.environment,
            resource_attributes: config.resource_attributes,
            scope_name: "rolly".to_string(),
            scope_version: env!("CARGO_PKG_VERSION").to_string(),
            start_time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
        };
        spawn_metrics_loop(metrics_config, sink, flush_interval);
    }

    Ok(TelemetryGuard { exporter })
}

/// Start the metrics aggregation loop as a background monoio task.
///
/// Calls [`rolly::collect_and_encode_metrics()`] on the configured
/// interval and sends results through the provided sink.
pub fn spawn_metrics_loop(
    config: MetricsExportConfig,
    sink: Arc<dyn TelemetrySink>,
    interval: Duration,
) {
    monoio::spawn(async move {
        // Initial delay before first collection
        monoio::time::sleep(interval).await;
        loop {
            if let Some(data) = rolly::collect_and_encode_metrics(&config) {
                sink.send_metrics(data);
            }
            monoio::time::sleep(interval).await;
        }
    });
}
