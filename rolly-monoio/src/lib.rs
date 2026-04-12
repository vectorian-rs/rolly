//! Monoio-based transport for the rolly observability crate.
//!
//! Provides [`MonoioExporter`] for batching and shipping OTLP telemetry
//! over HTTP, plus convenience functions for one-liner telemetry setup.
//!
//! # HTTP transport
//!
//! HTTP exports use [`ureq`] (synchronous, with TLS via rustls) on
//! spawned OS threads. The monoio event loop stays responsive — it
//! dispatches POST work to background threads and polls for completion.
//! For non-blocking async HTTP, use `rolly-tokio` instead.

mod exporter;

// Re-export the public API from the exporter module
pub use exporter::ExportMessage;
pub use exporter::Exporter as MonoioExporter;
pub use exporter::ExporterConfig;

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
    background_shutdown: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Stored so we can do one final metrics collect on shutdown.
    metrics_flush: Option<MetricsFlushState>,
}

struct MetricsFlushState {
    config: MetricsExportConfig,
    sink: Arc<dyn TelemetrySink>,
}

impl From<exporter::Exporter> for TelemetryGuard {
    fn from(exporter: exporter::Exporter) -> Self {
        Self {
            exporter: Some(exporter),
            background_shutdown: None,
            metrics_flush: None,
        }
    }
}

impl TelemetryGuard {
    /// Flush pending telemetry and stop background tasks.
    pub async fn shutdown(mut self) {
        self.signal_background_shutdown();
        self.final_metrics_flush();
        if let Some(exporter) = self.exporter.take() {
            exporter.shutdown().await;
        }
    }

    fn signal_background_shutdown(&self) {
        if let Some(flag) = &self.background_shutdown {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Collect any metrics still in the registry and send them through
    /// the sink, so the last interval is not lost on shutdown.
    fn final_metrics_flush(&self) {
        if let Some(ref mf) = self.metrics_flush {
            if let Some(data) = rolly::collect_and_encode_metrics(&mf.config) {
                mf.sink.send_metrics(data);
            }
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        self.signal_background_shutdown();
        self.final_metrics_flush();
        if let Some(ref exporter) = self.exporter {
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
///
/// Unlike `rolly_tokio::InitError`, this enum does not have an `Exporter`
/// variant because [`MonoioExporter::start()`] is infallible — URL
/// validation is deferred to POST time. The enum is `#[non_exhaustive]`
/// so variants can be added in future releases without breaking changes.
#[derive(Debug)]
#[non_exhaustive]
pub enum InitError {
    /// A global tracing subscriber is already set.
    SubscriberAlreadySet(tracing_subscriber::util::TryInitError),
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SubscriberAlreadySet(e) => write!(f, "global subscriber already set: {}", e),
        }
    }
}

impl std::error::Error for InitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SubscriberAlreadySet(e) => Some(e),
        }
    }
}

impl From<tracing_subscriber::util::TryInitError> for InitError {
    fn from(e: tracing_subscriber::util::TryInitError) -> Self {
        Self::SubscriberAlreadySet(e)
    }
}

/// Initialize the full telemetry stack and set the global subscriber.
///
/// Creates a [`MonoioExporter`], calls [`rolly::build_layer()`], installs the
/// global subscriber, and spawns background tasks for metrics aggregation.
///
/// # Panics
///
/// Panics on unexpected initialization errors. If a global tracing
/// subscriber is already set, logs a warning and returns a no-op guard
/// instead of panicking. For full control over error handling, use
/// [`try_init_global`].
///
/// # Requirements
///
/// Must be called from within a monoio runtime context.
pub fn init_global_once(config: TelemetryConfig) -> TelemetryGuard {
    match try_init_global(config) {
        Ok(guard) => guard,
        Err(InitError::SubscriberAlreadySet(_)) => {
            tracing::warn!(
                "rolly: global tracing subscriber already set, \
                 skipping telemetry initialization"
            );
            TelemetryGuard {
                exporter: None,
                background_shutdown: None,
                metrics_flush: None,
            }
        }
        #[allow(unreachable_patterns)]
        Err(e) => panic!("failed to initialize telemetry: {}", e),
    }
}

/// Same as [`init_global_once`], but returns an error instead of panicking.
///
/// # Errors
///
/// Returns [`InitError::SubscriberAlreadySet`] if a global tracing subscriber
/// is already installed.
pub fn try_init_global(config: TelemetryConfig) -> Result<TelemetryGuard, InitError> {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    let export_traces = config.otlp_traces_endpoint.is_some();
    let export_logs = config.otlp_logs_endpoint.is_some();
    let export_metrics = config.otlp_metrics_endpoint.is_some();
    let background_shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

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
            max_concurrent_exports: 4,
            ..ExporterConfig::default()
        }))
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

    #[cfg(target_os = "linux")]
    if let Some(interval) = config.use_metrics_interval {
        spawn_use_metrics_loop_until_shutdown(interval, background_shutdown.clone());
    }

    // Spawn metrics aggregation loop
    let metrics_flush = if export_metrics {
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
        let guard_state = MetricsFlushState {
            config: metrics_config.clone(),
            sink: sink.clone(),
        };
        spawn_metrics_loop_until_shutdown(
            metrics_config,
            sink,
            flush_interval,
            background_shutdown.clone(),
        );
        Some(guard_state)
    } else {
        None
    };

    Ok(TelemetryGuard {
        exporter,
        background_shutdown: Some(background_shutdown),
        metrics_flush,
    })
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
        monoio::time::sleep(interval).await;
        loop {
            if let Some(data) = rolly::collect_and_encode_metrics(&config) {
                sink.send_metrics(data);
            }
            monoio::time::sleep(interval).await;
        }
    });
}

fn spawn_metrics_loop_until_shutdown(
    config: MetricsExportConfig,
    sink: Arc<dyn TelemetrySink>,
    interval: Duration,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    monoio::spawn(async move {
        monoio::time::sleep(interval).await;
        loop {
            if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            if let Some(data) = rolly::collect_and_encode_metrics(&config) {
                sink.send_metrics(data);
            }
            monoio::time::sleep(interval).await;
        }
    });
}

#[cfg(target_os = "linux")]
fn spawn_use_metrics_loop_until_shutdown(
    interval: Duration,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    monoio::spawn(async move {
        let mut state = UseMetricsState::default();
        monoio::time::sleep(interval).await;
        loop {
            if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            rolly::collect_use_metrics(&mut state);
            monoio::time::sleep(interval).await;
        }
    });
}
