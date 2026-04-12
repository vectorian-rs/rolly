use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;

pub use rolly::BackpressureStrategy;

/// Message types sent to the exporter background task.
#[derive(Debug)]
pub enum ExportMessage {
    Traces(Bytes),
    Logs(Bytes),
    Metrics(Bytes),
    Flush(tokio::sync::oneshot::Sender<()>),
    Shutdown(tokio::sync::oneshot::Sender<()>),
}

/// Configuration for the exporter.
///
/// Use `Default` for standard values (1024 channel, 512 batch, 1s flush, 4 concurrent).
#[derive(Debug, Clone)]
pub struct ExporterConfig {
    pub traces_url: Option<String>,
    pub logs_url: Option<String>,
    pub metrics_url: Option<String>,
    pub channel_capacity: usize,
    pub batch_size: usize,
    pub flush_interval: Duration,
    pub max_concurrent_exports: usize,
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
            backpressure_strategy: BackpressureStrategy::Drop,
        }
    }
}

/// Handle to the exporter background task.
#[derive(Clone)]
pub struct Exporter {
    tx: mpsc::Sender<ExportMessage>,
    #[allow(dead_code)] // Only one strategy currently; field reserved for future variants
    backpressure_strategy: BackpressureStrategy,
}

impl Exporter {
    /// Start the exporter background task. Returns a handle for sending data.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built (e.g. TLS
    /// backend misconfiguration) or if no tokio runtime is active.
    pub fn start(config: ExporterConfig) -> Result<Self, StartError> {
        // Verify a tokio runtime is available before doing anything.
        let _handle = tokio::runtime::Handle::try_current().map_err(|_| StartError::NoRuntime)?;

        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(StartError::HttpClient)?;
        let batch_config = BatchConfig {
            traces_url: config.traces_url,
            logs_url: config.logs_url,
            metrics_url: config.metrics_url,
            batch_size: config.batch_size,
        };
        tokio::spawn(exporter_loop(
            rx,
            client,
            batch_config,
            config.flush_interval,
            config.max_concurrent_exports.max(1),
        ));
        Ok(Self {
            tx,
            backpressure_strategy: config.backpressure_strategy,
        })
    }

    /// Create an exporter for testing that doesn't spawn the HTTP loop.
    /// Returns the exporter and receiver so tests can read messages directly.
    #[cfg(any(test, feature = "_bench"))]
    pub fn start_test() -> (Self, mpsc::Receiver<ExportMessage>) {
        Self::start_test_with_capacity(64, BackpressureStrategy::Drop)
    }

    /// Create a test exporter with a specific channel capacity and backpressure strategy.
    #[cfg(any(test, feature = "_bench"))]
    pub fn start_test_with_capacity(
        capacity: usize,
        strategy: BackpressureStrategy,
    ) -> (Self, mpsc::Receiver<ExportMessage>) {
        let (tx, rx) = mpsc::channel(capacity);
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
        self.try_send(ExportMessage::Traces(Bytes::from(data)));
    }

    /// Send encoded log data to the exporter.
    pub fn send_logs(&self, data: Vec<u8>) {
        self.try_send(ExportMessage::Logs(Bytes::from(data)));
    }

    /// Send encoded metrics data to the exporter.
    pub fn send_metrics(&self, data: Vec<u8>) {
        self.try_send(ExportMessage::Metrics(Bytes::from(data)));
    }

    fn try_send(&self, msg: ExportMessage) {
        match self.tx.try_send(msg) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                rolly::increment_dropped_total();
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                rolly::increment_dropped_total();
            }
        }
    }

    /// Flush all pending data. Blocks until the exporter has processed everything.
    pub async fn flush(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(ExportMessage::Flush(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    /// Signal the exporter to stop after draining remaining messages.
    /// Waits for the exporter loop to finish processing before returning.
    pub async fn shutdown(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(ExportMessage::Shutdown(tx)).await.is_ok() {
            let _ = rx.await;
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

// ── Error types ─────────────────────────────────────────────────────────

/// Errors that can occur when starting the exporter.
#[derive(Debug)]
#[non_exhaustive]
pub enum StartError {
    /// The HTTP client could not be built (e.g. TLS misconfiguration).
    HttpClient(reqwest::Error),
    /// No tokio runtime is active. Call from within a `#[tokio::main]`
    /// or `tokio::runtime::Runtime` context.
    NoRuntime,
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HttpClient(e) => write!(f, "failed to build HTTP client: {}", e),
            Self::NoRuntime => write!(f, "no tokio runtime active"),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::HttpClient(e) => Some(e),
            Self::NoRuntime => None,
        }
    }
}

// ── Exporter background loop ────────────────────────────────────────────

const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(400),
    Duration::from_millis(1600),
];

/// Immutable config for batch accumulation.
struct BatchConfig {
    traces_url: Option<String>,
    logs_url: Option<String>,
    metrics_url: Option<String>,
    batch_size: usize,
}

/// Mutable state for batch accumulation and in-flight export tasks.
struct BatchState {
    traces: Vec<Bytes>,
    logs: Vec<Bytes>,
    metrics: Vec<Bytes>,
    join_set: tokio::task::JoinSet<()>,
}

impl BatchState {
    fn new() -> Self {
        Self {
            traces: Vec::new(),
            logs: Vec::new(),
            metrics: Vec::new(),
            join_set: tokio::task::JoinSet::new(),
        }
    }

    fn batches_empty(&self) -> bool {
        self.traces.is_empty() && self.logs.is_empty() && self.metrics.is_empty()
    }

    /// Flush all batches and drain in-flight tasks, retrying until
    /// all local buffers are empty (handles semaphore contention).
    async fn flush_and_drain(
        &mut self,
        config: &BatchConfig,
        client: &reqwest::Client,
        semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
    ) {
        for _ in 0..64 {
            self.flush_all(config, client, semaphore);
            self.drain().await;
            if self.batches_empty() {
                return;
            }
            // Yield to let in-flight tasks complete and release permits.
            tokio::task::yield_now().await;
        }
    }

    /// Route a single message into the appropriate batch, flushing when
    /// the batch reaches `config.batch_size`.
    fn collect(
        &mut self,
        msg: ExportMessage,
        config: &BatchConfig,
        client: &reqwest::Client,
        semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
    ) {
        match msg {
            ExportMessage::Traces(data) => {
                self.traces.push(data);
                if self.traces.len() >= config.batch_size {
                    flush_batch(
                        &mut self.traces,
                        config.traces_url.as_deref(),
                        client,
                        semaphore,
                        &mut self.join_set,
                    );
                }
            }
            ExportMessage::Logs(data) => {
                self.logs.push(data);
                if self.logs.len() >= config.batch_size {
                    flush_batch(
                        &mut self.logs,
                        config.logs_url.as_deref(),
                        client,
                        semaphore,
                        &mut self.join_set,
                    );
                }
            }
            ExportMessage::Metrics(data) => {
                self.metrics.push(data);
                if self.metrics.len() >= config.batch_size {
                    flush_batch(
                        &mut self.metrics,
                        config.metrics_url.as_deref(),
                        client,
                        semaphore,
                        &mut self.join_set,
                    );
                }
            }
            // Control messages handled by the loop, not here
            ExportMessage::Flush(_) | ExportMessage::Shutdown(_) => {}
        }
    }

    /// Flush all partial batches into export tasks.
    fn flush_all(
        &mut self,
        config: &BatchConfig,
        client: &reqwest::Client,
        semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
    ) {
        flush_batch(
            &mut self.traces,
            config.traces_url.as_deref(),
            client,
            semaphore,
            &mut self.join_set,
        );
        flush_batch(
            &mut self.logs,
            config.logs_url.as_deref(),
            client,
            semaphore,
            &mut self.join_set,
        );
        flush_batch(
            &mut self.metrics,
            config.metrics_url.as_deref(),
            client,
            semaphore,
            &mut self.join_set,
        );
    }

    /// Wait for all in-flight export tasks to complete.
    async fn drain(&mut self) {
        while self.join_set.join_next().await.is_some() {}
    }

    /// Reap completed tasks without blocking.
    fn reap(&mut self) {
        while self.join_set.try_join_next().is_some() {}
    }
}

async fn exporter_loop(
    mut rx: mpsc::Receiver<ExportMessage>,
    client: reqwest::Client,
    config: BatchConfig,
    flush_interval: Duration,
    max_concurrent_exports: usize,
) {
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    let semaphore = Arc::new(Semaphore::new(max_concurrent_exports));
    let mut state = BatchState::new();
    let mut interval = tokio::time::interval(flush_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;

    loop {
        tokio::select! {
            biased;
            msg = rx.recv() => {
                match msg {
                    Some(ExportMessage::Flush(done)) => {
                        state.flush_and_drain(&config, &client, &semaphore).await;
                        let _ = done.send(());
                    }
                    Some(ExportMessage::Shutdown(done)) => {
                        state.flush_and_drain(&config, &client, &semaphore).await;
                        let _ = done.send(());
                        break;
                    }
                    Some(msg) => {
                        state.collect(msg, &config, &client, &semaphore);
                    }
                    None => {
                        state.flush_and_drain(&config, &client, &semaphore).await;
                        break;
                    }
                }
            }
            _ = interval.tick() => {
                state.flush_all(&config, &client, &semaphore);
            }
        }

        state.reap();
    }
}

/// Concatenate batch payloads and spawn a concurrent POST task.
///
/// The semaphore permit is acquired synchronously (try_acquire) before
/// spawning so that payloads cannot accumulate in an unbounded JoinSet
/// when the collector is slow. If no permit is available, the batch is
/// left for the next flush cycle.
fn flush_batch(
    batch: &mut Vec<Bytes>,
    url: Option<&str>,
    client: &reqwest::Client,
    semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
    join_set: &mut tokio::task::JoinSet<()>,
) {
    if batch.is_empty() {
        return;
    }
    let url = match url {
        Some(u) => u,
        None => {
            batch.clear();
            return;
        }
    };

    let permit = match semaphore.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return,
    };

    let total_len: usize = batch.iter().map(|b| b.len()).sum();
    let mut payload = Vec::with_capacity(total_len);
    for item in batch.drain(..) {
        payload.extend_from_slice(&item);
    }
    let data = Bytes::from(payload);
    let client = client.clone();
    let url = url.to_string();

    join_set.spawn(async move {
        let _permit = permit;
        post_with_retry(&client, &url, data).await;
    });
}

/// POST with exponential backoff. On total failure, drop the batch.
///
/// Uses `eprintln!` intentionally — not `tracing::warn!` — because this runs
/// inside the telemetry pipeline. Using tracing here would re-enter the OtlpLayer
/// and cause infinite recursion.
async fn post_with_retry(client: &reqwest::Client, url: &str, data: Bytes) {
    for (attempt, delay) in RETRY_DELAYS.iter().enumerate() {
        match client
            .post(url)
            .header("Content-Type", "application/x-protobuf")
            .body(data.clone()) // Bytes::clone is O(1) — just an Arc bump
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) => {
                eprintln!(
                    "rolly-tokio: export attempt {}/{} to {} failed: HTTP {}",
                    attempt + 1,
                    RETRY_DELAYS.len(),
                    url,
                    resp.status()
                );
            }
            Err(e) => {
                eprintln!(
                    "rolly-tokio: export attempt {}/{} to {} failed: {}",
                    attempt + 1,
                    RETRY_DELAYS.len(),
                    url,
                    e
                );
            }
        }
        tokio::time::sleep(*delay).await;
    }
    eprintln!(
        "rolly-tokio: dropping batch after {} retries",
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

    #[tokio::test]
    async fn drop_counter_increments_on_channel_full() {
        let before = rolly::telemetry_dropped_total();
        // Capacity 2: fill channel, then the 3rd send should drop
        let (exporter, _rx) = Exporter::start_test_with_capacity(2, BackpressureStrategy::Drop);
        exporter.send_traces(vec![0x0A]);
        exporter.send_traces(vec![0x0A]);
        // Channel is full now
        exporter.send_traces(vec![0x0A]);
        let delta = rolly::telemetry_dropped_total() - before;
        assert!(delta >= 1, "expected at least 1 drop, got {}", delta);
    }

    #[tokio::test]
    async fn drop_counter_increments_for_logs_and_traces() {
        let before = rolly::telemetry_dropped_total();
        let (exporter, _rx) = Exporter::start_test_with_capacity(1, BackpressureStrategy::Drop);
        exporter.send_traces(vec![0x0A]); // fills the channel
        exporter.send_traces(vec![0x0A]); // dropped
        exporter.send_logs(vec![0x0A]); // dropped
        let delta = rolly::telemetry_dropped_total() - before;
        assert!(delta >= 2, "expected at least 2 drops, got {}", delta);
    }

    fn test_config(traces_url: Option<String>, logs_url: Option<String>) -> ExporterConfig {
        ExporterConfig {
            traces_url,
            logs_url,
            metrics_url: None,
            channel_capacity: 16,
            batch_size: 512,
            flush_interval: Duration::from_secs(60),
            max_concurrent_exports: 4,
            backpressure_strategy: BackpressureStrategy::Drop,
        }
    }

    #[tokio::test]
    async fn exporter_queues_and_flushes_without_panic() {
        let config = test_config(
            Some("http://127.0.0.1:1/v1/traces".to_string()),
            Some("http://127.0.0.1:1/v1/logs".to_string()),
        );
        let exporter = Exporter::start(config).unwrap();

        exporter.send_traces(vec![0x0A, 0x00]);
        exporter.send_logs(vec![0x0A, 0x00]);

        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn exporter_flush_completes() {
        let config = test_config(
            Some("http://127.0.0.1:1/v1/traces".to_string()),
            Some("http://127.0.0.1:1/v1/logs".to_string()),
        );
        let exporter = Exporter::start(config).unwrap();

        tokio::time::timeout(Duration::from_secs(5), exporter.flush())
            .await
            .expect("flush should complete within timeout");

        exporter.shutdown().await;
    }

    async fn respond_with_status(listener: &tokio::net::TcpListener, status: &str) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 65536];
        let _ = stream.read(&mut buf).await;
        let resp = format!(
            "HTTP/1.1 {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            status
        );
        stream.write_all(resp.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn post_with_retry_succeeds_on_first_attempt() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            respond_with_status(&listener, "200 OK").await;
        });

        let client = reqwest::Client::new();
        let url = format!("http://{}/v1/traces", addr);
        post_with_retry(&client, &url, Bytes::from_static(b"test")).await;
    }

    #[tokio::test]
    async fn post_with_retry_retries_on_500_then_succeeds() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            respond_with_status(&listener, "500 Internal Server Error").await;
            respond_with_status(&listener, "200 OK").await;
        });

        let client = reqwest::Client::new();
        let url = format!("http://{}/v1/traces", addr);
        post_with_retry(&client, &url, Bytes::from_static(b"test")).await;
    }

    #[tokio::test]
    async fn post_with_retry_gives_up_after_all_retries() {
        // Takes ~2.1s due to real retry delays (100ms + 400ms + 1600ms)
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            for _ in 0..3 {
                respond_with_status(&listener, "500 Internal Server Error").await;
            }
        });

        let client = reqwest::Client::new();
        let url = format!("http://{}/v1/traces", addr);
        post_with_retry(&client, &url, Bytes::from_static(b"test")).await;
        // Returns without panic after exhausting retries
    }

    #[tokio::test]
    async fn exporter_sends_to_correct_url_paths() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (path_tx, mut path_rx) = mpsc::channel::<String>(16);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let path_tx = path_tx.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let path = request
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let _ = path_tx.send(path).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });

        let config = test_config(
            Some(format!("http://{}/v1/traces", addr)),
            Some(format!("http://{}/v1/logs", addr)),
        );
        let exporter = Exporter::start(config).unwrap();

        exporter.send_traces(vec![0x0A, 0x00]);
        exporter.send_logs(vec![0x0A, 0x00]);
        exporter.flush().await;

        let mut paths = Vec::new();
        while let Ok(Some(path)) =
            tokio::time::timeout(Duration::from_secs(5), path_rx.recv()).await
        {
            paths.push(path);
            if paths.len() >= 2 {
                break;
            }
        }

        assert!(
            paths.contains(&"/v1/traces".to_string()),
            "missing /v1/traces, got {:?}",
            paths
        );
        assert!(
            paths.contains(&"/v1/logs".to_string()),
            "missing /v1/logs, got {:?}",
            paths
        );

        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn exporter_skips_logs_when_no_logs_url() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (path_tx, mut path_rx) = mpsc::channel::<String>(16);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let path_tx = path_tx.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let path = request
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let _ = path_tx.send(path).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });

        // Only traces_url set, logs_url is None
        let config = test_config(Some(format!("http://{}/v1/traces", addr)), None);
        let exporter = Exporter::start(config).unwrap();

        exporter.send_traces(vec![0x0A, 0x00]);
        exporter.send_logs(vec![0x0A, 0x00]); // should be silently dropped
        exporter.flush().await;

        let mut paths = Vec::new();
        while let Ok(Some(path)) =
            tokio::time::timeout(Duration::from_millis(500), path_rx.recv()).await
        {
            paths.push(path);
        }

        assert!(
            paths.contains(&"/v1/traces".to_string()),
            "expected /v1/traces, got {:?}",
            paths
        );
        assert!(
            !paths.contains(&"/v1/logs".to_string()),
            "should NOT have received /v1/logs, got {:?}",
            paths
        );

        exporter.shutdown().await;
    }

    #[test]
    fn exporter_config_with_batch_settings() {
        let config = ExporterConfig {
            traces_url: None,
            logs_url: None,
            channel_capacity: 16,
            metrics_url: None,
            batch_size: 100,
            flush_interval: Duration::from_millis(500),
            max_concurrent_exports: 2,
            backpressure_strategy: BackpressureStrategy::Drop,
        };
        assert_eq!(config.batch_size, 100);
        assert_eq!(config.flush_interval, Duration::from_millis(500));
        assert_eq!(config.max_concurrent_exports, 2);
    }

    #[tokio::test]
    async fn exporter_batches_traces_up_to_batch_size() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (body_tx, mut body_rx) = mpsc::channel::<Vec<u8>>(16);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let body_tx = body_tx.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    buf.truncate(n);
                    // Extract body after \r\n\r\n
                    let request = &buf[..n];
                    if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let body = request[pos + 4..].to_vec();
                        let _ = body_tx.send(body).await;
                    }
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });

        let config = ExporterConfig {
            traces_url: Some(format!("http://{}/v1/traces", addr)),
            logs_url: None,
            metrics_url: None,
            channel_capacity: 16,
            batch_size: 3,
            flush_interval: Duration::from_secs(60),
            max_concurrent_exports: 4,
            backpressure_strategy: BackpressureStrategy::Drop,
        };
        let exporter = Exporter::start(config).unwrap();

        // Send exactly batch_size items
        let payload = vec![0x0A, 0x00]; // minimal protobuf
        exporter.send_traces(payload.clone());
        exporter.send_traces(payload.clone());
        exporter.send_traces(payload.clone());
        exporter.flush().await;

        // Should receive exactly 1 HTTP POST containing all 3 concatenated
        let body = tokio::time::timeout(Duration::from_secs(5), body_rx.recv())
            .await
            .expect("timeout waiting for POST")
            .expect("channel closed");

        // 3 * 2 bytes = 6 bytes total (concatenated payloads)
        assert_eq!(body.len(), 6);

        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn exporter_flushes_on_interval() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (done_tx, mut done_rx) = mpsc::channel::<()>(1);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let done_tx = done_tx.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 65536];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    let _ = done_tx.send(()).await;
                });
            }
        });

        let config = ExporterConfig {
            traces_url: Some(format!("http://{}/v1/traces", addr)),
            logs_url: None,
            metrics_url: None,
            channel_capacity: 16,
            batch_size: 100, // large batch size — won't trigger batch flush
            flush_interval: Duration::from_millis(200), // short interval
            max_concurrent_exports: 4,
            backpressure_strategy: BackpressureStrategy::Drop,
        };
        let exporter = Exporter::start(config).unwrap();

        // Send 1 item — won't reach batch_size, must flush on interval
        exporter.send_traces(vec![0x0A, 0x00]);

        // Should arrive within ~500ms (interval + processing)
        let result = tokio::time::timeout(Duration::from_millis(1000), done_rx.recv()).await;
        assert!(result.is_ok(), "data should arrive via interval flush");

        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn exporter_explicit_flush_drains_batch() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (done_tx, mut done_rx) = mpsc::channel::<()>(1);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let done_tx = done_tx.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 65536];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    let _ = done_tx.send(()).await;
                });
            }
        });

        let config = ExporterConfig {
            traces_url: Some(format!("http://{}/v1/traces", addr)),
            logs_url: None,
            metrics_url: None,
            channel_capacity: 16,
            batch_size: 100, // large — won't trigger automatically
            flush_interval: Duration::from_secs(60), // long — won't trigger on time
            max_concurrent_exports: 4,
            backpressure_strategy: BackpressureStrategy::Drop,
        };
        let exporter = Exporter::start(config).unwrap();

        exporter.send_traces(vec![0x0A, 0x00]);
        exporter.flush().await;

        // Should have been sent by now
        let result = tokio::time::timeout(Duration::from_millis(500), done_rx.recv()).await;
        assert!(result.is_ok(), "flush should drain pending batch");

        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn exporter_shutdown_drains_remaining_batch() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (done_tx, mut done_rx) = mpsc::channel::<()>(1);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let done_tx = done_tx.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 65536];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    let _ = done_tx.send(()).await;
                });
            }
        });

        let config = ExporterConfig {
            traces_url: Some(format!("http://{}/v1/traces", addr)),
            logs_url: None,
            metrics_url: None,
            channel_capacity: 16,
            batch_size: 100,
            flush_interval: Duration::from_secs(60),
            max_concurrent_exports: 4,
            backpressure_strategy: BackpressureStrategy::Drop,
        };
        let exporter = Exporter::start(config).unwrap();

        exporter.send_traces(vec![0x0A, 0x00]);
        exporter.shutdown().await;

        // Shutdown should have drained the batch
        let result = tokio::time::timeout(Duration::from_millis(500), done_rx.recv()).await;
        assert!(result.is_ok(), "shutdown should drain remaining batch");
    }

    #[tokio::test]
    async fn exporter_batches_traces_and_logs_independently() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (path_tx, mut path_rx) = mpsc::channel::<String>(16);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let path_tx = path_tx.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let path = request
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let _ = path_tx.send(path).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });

        let config = ExporterConfig {
            traces_url: Some(format!("http://{}/v1/traces", addr)),
            logs_url: Some(format!("http://{}/v1/logs", addr)),
            metrics_url: None,
            channel_capacity: 16,
            batch_size: 2,
            flush_interval: Duration::from_secs(60),
            max_concurrent_exports: 4,
            backpressure_strategy: BackpressureStrategy::Drop,
        };
        let exporter = Exporter::start(config).unwrap();

        // Fill trace batch (batch_size = 2)
        exporter.send_traces(vec![0x0A, 0x00]);
        exporter.send_traces(vec![0x0A, 0x00]);
        // Fill log batch (batch_size = 2)
        exporter.send_logs(vec![0x0A, 0x00]);
        exporter.send_logs(vec![0x0A, 0x00]);
        exporter.flush().await;

        let mut paths = Vec::new();
        while let Ok(Some(path)) =
            tokio::time::timeout(Duration::from_secs(5), path_rx.recv()).await
        {
            paths.push(path);
            if paths.len() >= 2 {
                break;
            }
        }

        assert!(
            paths.contains(&"/v1/traces".to_string()),
            "missing /v1/traces, got {:?}",
            paths
        );
        assert!(
            paths.contains(&"/v1/logs".to_string()),
            "missing /v1/logs, got {:?}",
            paths
        );

        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn exporter_sends_concurrently_not_sequentially() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomOrd};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let concurrent_c = concurrent.clone();
        let max_concurrent_c = max_concurrent.clone();

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let conc = concurrent_c.clone();
                let max_conc = max_concurrent_c.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 65536];
                    let _ = stream.read(&mut buf).await;
                    let current = conc.fetch_add(1, AtomOrd::SeqCst) + 1;
                    max_conc.fetch_max(current, AtomOrd::SeqCst);
                    // Hold the connection open for a bit to allow overlap
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    conc.fetch_sub(1, AtomOrd::SeqCst);
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });

        let config = ExporterConfig {
            traces_url: Some(format!("http://{}/v1/traces", addr)),
            logs_url: None,
            metrics_url: None,
            channel_capacity: 64,
            batch_size: 1, // flush on every message — creates many concurrent posts
            flush_interval: Duration::from_secs(60),
            max_concurrent_exports: 8,
            backpressure_strategy: BackpressureStrategy::Drop,
        };
        let exporter = Exporter::start(config).unwrap();

        // Send many items rapidly to trigger concurrent exports
        for _ in 0..8 {
            exporter.send_traces(vec![0x0A, 0x00]);
        }
        exporter.flush().await;

        assert!(
            max_concurrent.load(AtomOrd::SeqCst) > 1,
            "expected concurrent exports > 1, got {}",
            max_concurrent.load(AtomOrd::SeqCst)
        );

        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn exporter_limits_concurrent_exports() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomOrd};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let concurrent = Arc::new(AtomicUsize::new(0));

        let concurrent_c = concurrent.clone();
        let max_concurrent_c = max_concurrent.clone();

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let conc = concurrent_c.clone();
                let max_conc = max_concurrent_c.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 65536];
                    let _ = stream.read(&mut buf).await;
                    let current = conc.fetch_add(1, AtomOrd::SeqCst) + 1;
                    max_conc.fetch_max(current, AtomOrd::SeqCst);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    conc.fetch_sub(1, AtomOrd::SeqCst);
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });

        let max_exports = 2;
        let config = ExporterConfig {
            traces_url: Some(format!("http://{}/v1/traces", addr)),
            logs_url: None,
            metrics_url: None,
            channel_capacity: 64,
            batch_size: 1,
            flush_interval: Duration::from_secs(60),
            max_concurrent_exports: max_exports,
            backpressure_strategy: BackpressureStrategy::Drop,
        };
        let exporter = Exporter::start(config).unwrap();

        for _ in 0..8 {
            exporter.send_traces(vec![0x0A, 0x00]);
        }
        exporter.flush().await;

        assert!(
            max_concurrent.load(AtomOrd::SeqCst) <= max_exports,
            "expected max concurrent <= {}, got {}",
            max_exports,
            max_concurrent.load(AtomOrd::SeqCst)
        );

        exporter.shutdown().await;
    }
}
