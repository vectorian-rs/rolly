//! Tokio-based transport for the rolly observability crate.
//!
//! Provides [`TokioExporter`] for batching and shipping OTLP telemetry
//! over HTTP, plus convenience functions for one-liner telemetry setup.

mod exporter;

// Re-export the public API from the exporter module
pub use exporter::ExportMessage;
pub use exporter::Exporter as TokioExporter;
pub use exporter::ExporterConfig;
pub use exporter::StartError;

/// Guard that flushes pending telemetry on drop.
///
/// Hold this in your main function to ensure all spans are exported
/// before shutdown. On a `current_thread` tokio runtime, `Drop` can
/// only trigger a best-effort async drain; call [`TelemetryGuard::shutdown`]
/// when you need deterministic delivery. Created by [`init_global_once`]
/// or manually.
pub struct TelemetryGuard {
    exporter: Option<exporter::Exporter>,
    task_handles: Vec<tokio::task::JoinHandle<()>>,
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
            task_handles: Vec::new(),
            metrics_flush: None,
        }
    }
}

impl TelemetryGuard {
    /// Flush pending telemetry and stop background tasks.
    ///
    /// Prefer this over relying on `Drop` when running on a
    /// `current_thread` tokio runtime and you need a deterministic drain.
    pub async fn shutdown(mut self) {
        self.abort_tasks();
        // take() so Drop won't fire it again
        if let Some(mf) = self.metrics_flush.take() {
            if let Some(data) = rolly::collect_and_encode_metrics(&mf.config) {
                mf.sink.send_metrics(data);
            }
        }
        if let Some(exporter) = self.exporter.take() {
            exporter.flush().await;
            exporter.shutdown().await;
        }
    }

    fn abort_tasks(&mut self) {
        for handle in self.task_handles.drain(..) {
            handle.abort();
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
        self.abort_tasks();
        self.final_metrics_flush();
        if let Some(ref exporter) = self.exporter {
            let flush_shutdown = async {
                exporter.flush().await;
                exporter.shutdown().await;
            };
            // If we're inside a tokio runtime, use block_in_place to avoid
            // the "cannot start a runtime from within a runtime" panic.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                // block_in_place requires a multi-thread runtime; on
                // current_thread it would deadlock. In that case, spawn
                // the work and hope the runtime lives long enough.
                if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
                    tokio::task::block_in_place(|| {
                        handle.block_on(flush_shutdown);
                    });
                } else {
                    // current_thread runtime: spawn the flush and let it
                    // complete on the next poll. We cannot block here.
                    let exporter = exporter.clone();
                    handle.spawn(async move {
                        exporter.flush().await;
                        exporter.shutdown().await;
                    });
                }
            } else {
                // No runtime active — create a temporary one for shutdown.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(rt) = rt {
                    rt.block_on(flush_shutdown);
                }
            }
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
    /// The exporter could not be started (e.g. no tokio runtime, TLS failure).
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
/// Creates a [`TokioExporter`], calls [`rolly::build_layer()`], installs the
/// global subscriber, and spawns background tasks for metrics aggregation
/// and USE polling.
///
/// # Panics
///
/// Panics if initialization fails for any reason. For fallible
/// initialization, use [`try_init_global`].
///
/// # Requirements
///
/// Must be called from within a tokio runtime context.
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
                task_handles: Vec::new(),
                metrics_flush: None,
            }
        }
        Err(e) => panic!("failed to initialize telemetry: {}", e),
    }
}

/// Same as [`init_global_once`], but returns an error instead of panicking.
///
/// # Errors
///
/// Returns [`InitError::Exporter`] if no tokio runtime is active or the
/// HTTP client cannot be built. Returns [`InitError::SubscriberAlreadySet`]
/// if a global tracing subscriber is already installed.
pub fn try_init_global(config: TelemetryConfig) -> Result<TelemetryGuard, InitError> {
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
        ..LayerConfig::default()
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
        let handle = spawn_metrics_loop(metrics_config, sink, flush_interval);
        task_handles.push(handle);
        Some(guard_state)
    } else {
        None
    };

    Ok(TelemetryGuard {
        exporter,
        task_handles,
        metrics_flush,
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
