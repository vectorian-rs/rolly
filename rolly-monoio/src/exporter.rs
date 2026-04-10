use std::time::Duration;

use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender, TrySendError};

pub use rolly::BackpressureStrategy;

/// Message types sent to the exporter background task.
#[derive(Debug)]
pub enum ExportMessage {
    Traces(Bytes),
    Logs(Bytes),
    Metrics(Bytes),
    Flush(std::sync::mpsc::Sender<()>),
    Shutdown(std::sync::mpsc::Sender<()>),
}

/// Configuration for the exporter.
///
/// Use `Default` for standard values (1024 channel, 512 batch, 1s flush).
pub struct ExporterConfig {
    pub traces_url: Option<String>,
    pub logs_url: Option<String>,
    pub metrics_url: Option<String>,
    pub channel_capacity: usize,
    pub batch_size: usize,
    pub flush_interval: Duration,
    pub backpressure_strategy: BackpressureStrategy,
}

impl Default for ExporterConfig {
    fn default() -> Self {
        Self {
            traces_url: None,
            logs_url: None,
            metrics_url: None,
            channel_capacity: 1024,
            batch_size: 512,
            flush_interval: Duration::from_secs(1),
            backpressure_strategy: BackpressureStrategy::Drop,
        }
    }
}

/// Handle to the exporter background task.
#[derive(Clone)]
pub struct Exporter {
    tx: Sender<ExportMessage>,
    backpressure_strategy: BackpressureStrategy,
}

impl Exporter {
    /// Start the exporter background task. Returns a handle for sending data.
    ///
    /// Must be called from within a monoio runtime context.
    pub fn start(config: ExporterConfig) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(config.channel_capacity);

        monoio::spawn(exporter_loop(
            rx,
            config.traces_url,
            config.logs_url,
            config.metrics_url,
            config.batch_size,
            config.flush_interval,
        ));

        Self {
            tx,
            backpressure_strategy: config.backpressure_strategy,
        }
    }

    /// Create an exporter for testing that doesn't spawn the HTTP loop.
    /// Returns the exporter and receiver so tests can read messages directly.
    #[cfg(any(test, feature = "_bench"))]
    pub fn start_test() -> (Self, Receiver<ExportMessage>) {
        Self::start_test_with_capacity(64, BackpressureStrategy::Drop)
    }

    /// Create a test exporter with a specific channel capacity and backpressure strategy.
    #[cfg(any(test, feature = "_bench"))]
    pub fn start_test_with_capacity(
        capacity: usize,
        strategy: BackpressureStrategy,
    ) -> (Self, Receiver<ExportMessage>) {
        let (tx, rx) = crossbeam_channel::bounded(capacity);
        (
            Self {
                tx,
                backpressure_strategy: strategy,
            },
            rx,
        )
    }

    /// Send encoded trace data to the exporter (non-blocking).
    pub fn send_traces(&self, data: Vec<u8>) {
        match self.backpressure_strategy {
            BackpressureStrategy::Drop | _ => {
                if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
                    self.tx.try_send(ExportMessage::Traces(Bytes::from(data)))
                {
                    rolly::increment_dropped_total();
                }
            }
        }
    }

    /// Send encoded log data to the exporter (non-blocking).
    pub fn send_logs(&self, data: Vec<u8>) {
        match self.backpressure_strategy {
            BackpressureStrategy::Drop | _ => {
                if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
                    self.tx.try_send(ExportMessage::Logs(Bytes::from(data)))
                {
                    rolly::increment_dropped_total();
                }
            }
        }
    }

    /// Send encoded metrics data to the exporter (non-blocking).
    pub fn send_metrics(&self, data: Vec<u8>) {
        match self.backpressure_strategy {
            BackpressureStrategy::Drop | _ => {
                if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
                    self.tx.try_send(ExportMessage::Metrics(Bytes::from(data)))
                {
                    rolly::increment_dropped_total();
                }
            }
        }
    }

    /// Flush all pending data. Polls asynchronously without blocking the event loop.
    pub async fn flush(&self) {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        if self.tx.try_send(ExportMessage::Flush(done_tx)).is_err() {
            return;
        }
        poll_for_ack(done_rx).await;
    }

    /// Signal the exporter to stop after draining remaining messages.
    ///
    /// Flushes pending batches, then waits for the background task to exit.
    pub async fn shutdown(&self) {
        self.flush().await;
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        if self.tx.try_send(ExportMessage::Shutdown(done_tx)).is_err() {
            return;
        }
        poll_for_ack(done_rx).await;
    }

    /// Non-blocking shutdown signal. Sends a Shutdown message without waiting.
    ///
    /// Used by `TelemetryGuard::drop` where we cannot await.
    pub fn request_shutdown(&self) {
        let (done_tx, _) = std::sync::mpsc::channel();
        let _ = self.tx.try_send(ExportMessage::Shutdown(done_tx));
    }
}

/// Poll a std::sync::mpsc receiver without blocking the monoio event loop.
async fn poll_for_ack(rx: std::sync::mpsc::Receiver<()>) {
    loop {
        match rx.try_recv() {
            Ok(()) => return,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                monoio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
        }
    }
}

impl rolly::TelemetrySink for Exporter {
    fn send_traces(&self, data: Vec<u8>) {
        self.send_traces(data);
    }
    fn send_logs(&self, data: Vec<u8>) {
        self.send_logs(data);
    }
    fn send_metrics(&self, data: Vec<u8>) {
        self.send_metrics(data);
    }
}

// -- Exporter background loop ------------------------------------------------

const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(400),
    Duration::from_millis(1600),
];

/// Polling interval when the channel has been empty (idle mode).
const IDLE_CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// Polling interval when messages are flowing.
const ACTIVE_CHECK_INTERVAL: Duration = Duration::from_millis(10);

async fn exporter_loop(
    rx: Receiver<ExportMessage>,
    traces_url: Option<String>,
    logs_url: Option<String>,
    metrics_url: Option<String>,
    batch_size: usize,
    flush_interval: Duration,
) {
    let mut trace_batch: Vec<Bytes> = Vec::new();
    let mut log_batch: Vec<Bytes> = Vec::new();
    let mut metrics_batch: Vec<Bytes> = Vec::new();

    // Track in-flight HTTP POSTs running on std::threads.
    let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut check_interval = IDLE_CHECK_INTERVAL;
    let mut time_since_flush = Duration::ZERO;

    loop {
        monoio::time::sleep(check_interval).await;
        time_since_flush += check_interval;

        // Drain all available messages (non-blocking).
        let mut got_messages = false;
        loop {
            match rx.try_recv() {
                Ok(ExportMessage::Traces(data)) => {
                    got_messages = true;
                    trace_batch.push(data);
                    if trace_batch.len() >= batch_size {
                        flush_batch(&mut trace_batch, traces_url.as_deref(), &in_flight);
                    }
                }
                Ok(ExportMessage::Logs(data)) => {
                    got_messages = true;
                    log_batch.push(data);
                    if log_batch.len() >= batch_size {
                        flush_batch(&mut log_batch, logs_url.as_deref(), &in_flight);
                    }
                }
                Ok(ExportMessage::Metrics(data)) => {
                    got_messages = true;
                    metrics_batch.push(data);
                    if metrics_batch.len() >= batch_size {
                        flush_batch(&mut metrics_batch, metrics_url.as_deref(), &in_flight);
                    }
                }
                Ok(ExportMessage::Flush(done)) => {
                    flush_batch(&mut trace_batch, traces_url.as_deref(), &in_flight);
                    flush_batch(&mut log_batch, logs_url.as_deref(), &in_flight);
                    flush_batch(&mut metrics_batch, metrics_url.as_deref(), &in_flight);
                    wait_for_in_flight(&in_flight).await;
                    let _ = done.send(());
                    time_since_flush = Duration::ZERO;
                }
                Ok(ExportMessage::Shutdown(done)) => {
                    flush_batch(&mut trace_batch, traces_url.as_deref(), &in_flight);
                    flush_batch(&mut log_batch, logs_url.as_deref(), &in_flight);
                    flush_batch(&mut metrics_batch, metrics_url.as_deref(), &in_flight);
                    wait_for_in_flight(&in_flight).await;
                    let _ = done.send(());
                    return;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    flush_batch(&mut trace_batch, traces_url.as_deref(), &in_flight);
                    flush_batch(&mut log_batch, logs_url.as_deref(), &in_flight);
                    flush_batch(&mut metrics_batch, metrics_url.as_deref(), &in_flight);
                    wait_for_in_flight(&in_flight).await;
                    return;
                }
            }
        }

        // Adaptive polling: fast when messages flow, slow when idle.
        check_interval = if got_messages {
            ACTIVE_CHECK_INTERVAL
        } else {
            IDLE_CHECK_INTERVAL
        };

        // Time-based flush
        if time_since_flush >= flush_interval {
            time_since_flush = Duration::ZERO;
            flush_batch(&mut trace_batch, traces_url.as_deref(), &in_flight);
            flush_batch(&mut log_batch, logs_url.as_deref(), &in_flight);
            flush_batch(&mut metrics_batch, metrics_url.as_deref(), &in_flight);
        }
    }
}

/// Wait for all in-flight HTTP POSTs to complete without blocking the event loop.
async fn wait_for_in_flight(in_flight: &std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    while in_flight.load(std::sync::atomic::Ordering::Acquire) > 0 {
        monoio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Concatenate batch payloads and POST them on a background std::thread.
///
/// Protobuf repeated fields merge on concatenation, so multiple
/// `ExportTraceServiceRequest` payloads concatenated produce a valid
/// message with multiple `ResourceSpans`.
///
/// The HTTP POST runs on a spawned OS thread via `ureq` (blocking I/O
/// with TLS support). This keeps the monoio event loop responsive.
fn flush_batch(
    batch: &mut Vec<Bytes>,
    url: Option<&str>,
    in_flight: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    if batch.is_empty() {
        return;
    }
    let url = match url {
        Some(u) => u.to_string(),
        None => {
            batch.clear();
            return;
        }
    };

    let total_len: usize = batch.iter().map(|b| b.len()).sum();
    let mut payload = Vec::with_capacity(total_len);
    for item in batch.drain(..) {
        payload.extend_from_slice(&item);
    }

    in_flight.fetch_add(1, std::sync::atomic::Ordering::Release);
    let in_flight = in_flight.clone();
    std::thread::spawn(move || {
        post_with_retry(&url, &payload);
        in_flight.fetch_sub(1, std::sync::atomic::Ordering::Release);
    });
}

/// POST with exponential backoff. On total failure, drop the batch.
///
/// Uses `eprintln!` intentionally — not `tracing::warn!` — because this runs
/// inside the telemetry pipeline. Using tracing here would re-enter the OtlpLayer
/// and cause infinite recursion.
///
/// **Blocking:** Each POST blocks the thread for the duration of the HTTP
/// request (including retries with exponential backoff up to ~2.1s total).
/// The monoio event loop is stalled during this time. For local collectors
/// (localhost, <10ms latency) this is negligible. For remote endpoints with
/// higher latency, consider using `rolly-tokio` instead.
fn post_with_retry(url: &str, body: &[u8]) {
    for (attempt, delay) in RETRY_DELAYS.iter().enumerate() {
        match ureq::post(url)
            .header("Content-Type", "application/x-protobuf")
            .send(body)
        {
            Ok(response) if response.status().is_success() => return,
            Ok(response) => {
                eprintln!(
                    "rolly-monoio: export attempt {}/{} to {} failed: HTTP {}",
                    attempt + 1,
                    RETRY_DELAYS.len(),
                    url,
                    response.status().as_u16(),
                );
            }
            Err(e) => {
                eprintln!(
                    "rolly-monoio: export attempt {}/{} to {} failed: {}",
                    attempt + 1,
                    RETRY_DELAYS.len(),
                    url,
                    e,
                );
            }
        }
        std::thread::sleep(*delay);
    }
    eprintln!(
        "rolly-monoio: dropping batch after {} retries",
        RETRY_DELAYS.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_counter_is_callable() {
        let _count = rolly::telemetry_dropped_total();
    }

    #[test]
    fn exporter_config_defaults() {
        let config = ExporterConfig::default();
        assert_eq!(config.channel_capacity, 1024);
        assert_eq!(config.batch_size, 512);
        assert_eq!(config.flush_interval, Duration::from_secs(1));
    }

    #[test]
    fn drop_counter_increments_on_channel_full() {
        let before = rolly::telemetry_dropped_total();
        let (exporter, _rx) = Exporter::start_test_with_capacity(2, BackpressureStrategy::Drop);
        exporter.send_traces(vec![0x0A]);
        exporter.send_traces(vec![0x0A]);
        // Channel is full now
        exporter.send_traces(vec![0x0A]);
        let delta = rolly::telemetry_dropped_total() - before;
        assert!(delta >= 1, "expected at least 1 drop, got {}", delta);
    }

    #[test]
    fn drop_counter_increments_for_logs_and_traces() {
        let before = rolly::telemetry_dropped_total();
        let (exporter, _rx) = Exporter::start_test_with_capacity(1, BackpressureStrategy::Drop);
        exporter.send_traces(vec![0x0A]); // fills the channel
        exporter.send_traces(vec![0x0A]); // dropped
        exporter.send_logs(vec![0x0A]); // dropped
        let delta = rolly::telemetry_dropped_total() - before;
        assert!(delta >= 2, "expected at least 2 drops, got {}", delta);
    }

    #[test]
    fn exporter_config_with_batch_settings() {
        let config = ExporterConfig {
            traces_url: None,
            logs_url: None,
            metrics_url: None,
            channel_capacity: 16,
            batch_size: 100,
            flush_interval: Duration::from_millis(500),
            backpressure_strategy: BackpressureStrategy::Drop,
        };
        assert_eq!(config.batch_size, 100);
        assert_eq!(config.flush_interval, Duration::from_millis(500));
    }
}
