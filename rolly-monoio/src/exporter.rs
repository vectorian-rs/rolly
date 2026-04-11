use std::collections::VecDeque;
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
}

#[derive(Debug)]
enum ControlMessage {
    Flush(std::sync::mpsc::Sender<()>),
    Shutdown(Option<std::sync::mpsc::Sender<()>>),
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
    pub max_concurrent_exports: usize,
    /// Maximum number of batches queued for HTTP POST before dropping.
    /// Bounds memory when the collector is slow or unreachable.
    pub max_pending_batches: usize,
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
            max_concurrent_exports: 4,
            max_pending_batches: 32,
            backpressure_strategy: BackpressureStrategy::Drop,
        }
    }
}

/// Handle to the exporter background task.
#[derive(Clone)]
pub struct Exporter {
    tx: Sender<ExportMessage>,
    control_tx: Option<Sender<ControlMessage>>,
    backpressure_strategy: BackpressureStrategy,
}

impl Exporter {
    /// Start the exporter background task. Returns a handle for sending data.
    ///
    /// Must be called from within a monoio runtime context.
    pub fn start(config: ExporterConfig) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(config.channel_capacity);
        let (control_tx, control_rx) = crossbeam_channel::unbounded();

        let batch_config = BatchConfig {
            traces_url: config.traces_url,
            logs_url: config.logs_url,
            metrics_url: config.metrics_url,
            batch_size: config.batch_size,
            max_pending_batches: config.max_pending_batches.max(1),
        };

        monoio::spawn(exporter_loop(
            rx,
            control_rx,
            batch_config,
            config.flush_interval,
            config.max_concurrent_exports.max(1),
        ));

        Self {
            tx,
            control_tx: Some(control_tx),
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
                control_tx: None,
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
        let Some(control_tx) = &self.control_tx else {
            return;
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        if control_tx.send(ControlMessage::Flush(done_tx)).is_err() {
            return;
        }
        poll_for_ack(done_rx).await;
    }

    /// Signal the exporter to stop after draining remaining messages.
    ///
    /// Flushes pending batches, then waits for the background task to exit.
    pub async fn shutdown(&self) {
        let Some(control_tx) = &self.control_tx else {
            return;
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        if control_tx
            .send(ControlMessage::Shutdown(Some(done_tx)))
            .is_err()
        {
            return;
        }
        poll_for_ack(done_rx).await;
    }

    /// Non-blocking shutdown signal. Sends a Shutdown message without waiting.
    ///
    /// Used by `TelemetryGuard::drop` where we cannot await.
    pub fn request_shutdown(&self) {
        if let Some(control_tx) = &self.control_tx {
            let _ = control_tx.send(ControlMessage::Shutdown(None));
        }
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

/// Maximum messages drained from `rx` per iteration before checking `control_rx`.
const DRAIN_QUOTA: usize = 1024;

/// Immutable config for batch accumulation and queuing.
struct BatchConfig {
    traces_url: Option<String>,
    logs_url: Option<String>,
    metrics_url: Option<String>,
    batch_size: usize,
    max_pending_batches: usize,
}

/// Mutable state for batch accumulation and pending HTTP posts.
struct BatchState {
    traces: Vec<Bytes>,
    logs: Vec<Bytes>,
    metrics: Vec<Bytes>,
    pending_posts: VecDeque<PendingPost>,
}

impl BatchState {
    fn new() -> Self {
        Self {
            traces: Vec::new(),
            logs: Vec::new(),
            metrics: Vec::new(),
            pending_posts: VecDeque::new(),
        }
    }

    /// Route a single message into the appropriate batch, flushing to
    /// the pending queue when the batch reaches `config.batch_size`.
    fn collect(&mut self, msg: ExportMessage, config: &BatchConfig) {
        match msg {
            ExportMessage::Traces(data) => {
                self.traces.push(data);
                if self.traces.len() >= config.batch_size {
                    queue_batch(
                        &mut self.traces,
                        config.traces_url.as_deref(),
                        &mut self.pending_posts,
                        config.max_pending_batches,
                    );
                }
            }
            ExportMessage::Logs(data) => {
                self.logs.push(data);
                if self.logs.len() >= config.batch_size {
                    queue_batch(
                        &mut self.logs,
                        config.logs_url.as_deref(),
                        &mut self.pending_posts,
                        config.max_pending_batches,
                    );
                }
            }
            ExportMessage::Metrics(data) => {
                self.metrics.push(data);
                if self.metrics.len() >= config.batch_size {
                    queue_batch(
                        &mut self.metrics,
                        config.metrics_url.as_deref(),
                        &mut self.pending_posts,
                        config.max_pending_batches,
                    );
                }
            }
        }
    }

    /// Drain all available messages from `rx` into batches (no quota).
    fn drain_from(&mut self, rx: &Receiver<ExportMessage>, config: &BatchConfig) {
        while let Ok(msg) = rx.try_recv() {
            self.collect(msg, config);
        }
    }

    /// Flush all partial batches into the pending queue.
    fn flush_all(&mut self, config: &BatchConfig) {
        queue_batch(
            &mut self.traces,
            config.traces_url.as_deref(),
            &mut self.pending_posts,
            config.max_pending_batches,
        );
        queue_batch(
            &mut self.logs,
            config.logs_url.as_deref(),
            &mut self.pending_posts,
            config.max_pending_batches,
        );
        queue_batch(
            &mut self.metrics,
            config.metrics_url.as_deref(),
            &mut self.pending_posts,
            config.max_pending_batches,
        );
    }
}

async fn exporter_loop(
    rx: Receiver<ExportMessage>,
    control_rx: Receiver<ControlMessage>,
    config: BatchConfig,
    flush_interval: Duration,
    max_concurrent_exports: usize,
) {
    let mut state = BatchState::new();
    let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut check_interval = IDLE_CHECK_INTERVAL;
    let mut time_since_flush = Duration::ZERO;

    loop {
        monoio::time::sleep(check_interval).await;
        time_since_flush += check_interval;

        // Drain data channel with a quota so control messages are never starved.
        let mut got_messages = false;
        let mut disconnected = false;
        for _ in 0..DRAIN_QUOTA {
            match rx.try_recv() {
                Ok(msg) => {
                    got_messages = true;
                    state.collect(msg, &config);
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        // Always check control channel — even under sustained load.
        while let Ok(control) = control_rx.try_recv() {
            state.drain_from(&rx, &config);
            state.flush_all(&config);

            match control {
                ControlMessage::Flush(done) => {
                    wait_for_drain(&mut state.pending_posts, &in_flight, max_concurrent_exports)
                        .await;
                    let _ = done.send(());
                    time_since_flush = Duration::ZERO;
                }
                ControlMessage::Shutdown(done) => {
                    wait_for_drain(&mut state.pending_posts, &in_flight, max_concurrent_exports)
                        .await;
                    if let Some(done) = done {
                        let _ = done.send(());
                    }
                    return;
                }
            }
        }

        if disconnected {
            state.flush_all(&config);
            wait_for_drain(&mut state.pending_posts, &in_flight, max_concurrent_exports).await;
            return;
        }

        check_interval = if got_messages {
            ACTIVE_CHECK_INTERVAL
        } else {
            IDLE_CHECK_INTERVAL
        };

        if time_since_flush >= flush_interval {
            time_since_flush = Duration::ZERO;
            state.flush_all(&config);
        }

        spawn_pending_posts(&mut state.pending_posts, &in_flight, max_concurrent_exports);
    }
}

struct PendingPost {
    url: String,
    payload: Vec<u8>,
}

/// Wait for all queued and in-flight HTTP POSTs to complete without blocking the event loop.
async fn wait_for_drain(
    pending_posts: &mut VecDeque<PendingPost>,
    in_flight: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
    max_concurrent_exports: usize,
) {
    loop {
        spawn_pending_posts(pending_posts, in_flight, max_concurrent_exports);
        if pending_posts.is_empty() && in_flight.load(std::sync::atomic::Ordering::Acquire) == 0 {
            return;
        }
        monoio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Concatenate a batch payload and queue it for bounded background POSTing.
///
/// If `pending_posts` is at capacity, the oldest batch is dropped and the
/// global drop counter is incremented. This bounds memory when the collector
/// is slow or unreachable.
///
/// Protobuf repeated fields merge on concatenation, so multiple
/// `ExportTraceServiceRequest` payloads concatenated produce a valid
/// message with multiple `ResourceSpans`.
fn queue_batch(
    batch: &mut Vec<Bytes>,
    url: Option<&str>,
    pending_posts: &mut VecDeque<PendingPost>,
    max_pending_batches: usize,
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

    while pending_posts.len() >= max_pending_batches {
        pending_posts.pop_front();
        rolly::increment_dropped_total();
    }

    pending_posts.push_back(PendingPost { url, payload });
}

fn spawn_pending_posts(
    pending_posts: &mut VecDeque<PendingPost>,
    in_flight: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
    max_concurrent_exports: usize,
) {
    while in_flight.load(std::sync::atomic::Ordering::Acquire) < max_concurrent_exports {
        let Some(pending) = pending_posts.pop_front() else {
            break;
        };
        in_flight.fetch_add(1, std::sync::atomic::Ordering::Release);
        let in_flight = in_flight.clone();
        std::thread::spawn(move || {
            post_with_retry(&pending.url, &pending.payload);
            in_flight.fetch_sub(1, std::sync::atomic::Ordering::Release);
        });
    }
}

/// POST with exponential backoff. On total failure, drop the batch.
///
/// Uses `eprintln!` intentionally — not `tracing::warn!` — because this runs
/// inside the telemetry pipeline. Using tracing here would re-enter the OtlpLayer
/// and cause infinite recursion.
///
/// **Blocking:** Each POST blocks the worker thread for the duration of the
/// HTTP request (including retries with exponential backoff up to ~2.1s total).
/// The monoio event loop remains responsive because this work runs off-thread.
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
        assert_eq!(config.max_concurrent_exports, 4);
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
            max_concurrent_exports: 2,
            max_pending_batches: 16,
            backpressure_strategy: BackpressureStrategy::Drop,
        };
        assert_eq!(config.batch_size, 100);
        assert_eq!(config.flush_interval, Duration::from_millis(500));
        assert_eq!(config.max_concurrent_exports, 2);
    }
}
