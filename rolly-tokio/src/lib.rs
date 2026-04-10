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
    task_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl From<exporter::Exporter> for TelemetryGuard {
    fn from(exporter: exporter::Exporter) -> Self {
        Self {
            exporter: Some(exporter),
            task_handles: Vec::new(),
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        for handle in &self.task_handles {
            handle.abort();
        }
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

use std::sync::Arc;
use std::time::Duration;

/// Initialize the full telemetry stack and set the global subscriber.
///
/// Creates a [`TokioExporter`], calls [`rolly::build_layer()`], installs the
/// global subscriber, and spawns background tasks for metrics aggregation
/// and USE polling.
///
/// # Panics
///
/// Panics if a global tracing subscriber is already set. For fallible
/// initialization, use [`try_init_global`].
pub fn init_global_once(config: TelemetryConfig) -> TelemetryGuard {
    match try_init_global(config) {
        Ok(guard) => guard,
        Err(e) => panic!("failed to set global subscriber: {}", e),
    }
}

/// Same as [`init_global_once`], but returns an error instead of panicking
/// if a global subscriber is already set.
pub fn try_init_global(
    config: TelemetryConfig,
) -> Result<TelemetryGuard, tracing_subscriber::util::TryInitError> {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    let export_traces = config.otlp_traces_endpoint.is_some();
    let export_logs = config.otlp_logs_endpoint.is_some();
    let export_metrics = config.otlp_metrics_endpoint.is_some();

    let metrics_url = config
        .otlp_metrics_endpoint
        .as_deref()
        .map(|ep| format!("{}/v1/metrics", ep));

    let exporter = if export_traces || export_logs || export_metrics {
        let traces_url = config
            .otlp_traces_endpoint
            .as_deref()
            .map(|ep| format!("{}/v1/traces", ep));
        let logs_url = config
            .otlp_logs_endpoint
            .as_deref()
            .map(|ep| format!("{}/v1/logs", ep));
        Some(exporter::Exporter::start(ExporterConfig {
            traces_url,
            logs_url,
            metrics_url,
            backpressure_strategy: config.backpressure_strategy,
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

    let mut task_handles = Vec::new();

    // Spawn USE metrics polling (Linux only)
    #[cfg(target_os = "linux")]
    if let Some(interval) = config.use_metrics_interval {
        let handle = tokio::spawn(async move {
            let mut state = UseMetricsState::default();
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                rolly::collect_use_metrics(&mut state);
            }
        });
        task_handles.push(handle);
    }

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
        let handle = spawn_metrics_loop(metrics_config, sink, flush_interval);
        task_handles.push(handle);
    }

    Ok(TelemetryGuard {
        exporter,
        task_handles,
    })
}

/// Start the metrics aggregation loop as a background tokio task.
///
/// Calls [`rolly::collect_and_encode_metrics()`] on the configured
/// interval and sends results through the provided sink.
///
/// Returns a `JoinHandle` the caller can use to abort the task.
pub fn spawn_metrics_loop(
    config: MetricsExportConfig,
    sink: Arc<dyn TelemetrySink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Some(data) = rolly::collect_and_encode_metrics(&config) {
                sink.send_metrics(data);
            }
        }
    })
}
