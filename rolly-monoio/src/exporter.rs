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
    ///
    /// # Errors
    ///
    /// Returns an error if the URL configuration is invalid.
    pub fn start(config: ExporterConfig) -> Result<Self, StartError> {
        let (tx, rx) = crossbeam_channel::bounded(config.channel_capacity);

        // Parse URLs upfront to fail fast
        let traces_endpoint = config
            .traces_url
            .as_deref()
            .map(parse_url)
            .transpose()
            .map_err(StartError::InvalidUrl)?;
        let logs_endpoint = config
            .logs_url
            .as_deref()
            .map(parse_url)
            .transpose()
            .map_err(StartError::InvalidUrl)?;
        let metrics_endpoint = config
            .metrics_url
            .as_deref()
            .map(parse_url)
            .transpose()
            .map_err(StartError::InvalidUrl)?;

        monoio::spawn(exporter_loop(
            rx,
            traces_endpoint,
            logs_endpoint,
            metrics_endpoint,
            config.batch_size,
            config.flush_interval,
        ));

        Ok(Self {
            tx,
            backpressure_strategy: config.backpressure_strategy,
        })
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
        // First flush to ensure all batches are sent.
        self.flush().await;
        // Then signal shutdown and wait for the loop to confirm exit.
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
        // Fire-and-forget: the background task will drain batches before exiting.
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

// -- Error types --------------------------------------------------------------

/// Errors that can occur when starting the exporter.
#[derive(Debug)]
#[non_exhaustive]
pub enum StartError {
    /// A configured URL could not be parsed.
    InvalidUrl(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(msg) => write!(f, "invalid URL: {}", msg),
        }
    }
}

impl std::error::Error for StartError {}

// -- URL parsing --------------------------------------------------------------

/// Parsed HTTP endpoint.
#[derive(Clone, Debug)]
struct Endpoint {
    host: String,
    port: u16,
    path: String,
}

/// Parse an HTTP URL into host, port, path components.
///
/// Supports `http://host:port/path` format. HTTPS is not supported (raw TCP).
fn parse_url(url: &str) -> Result<Endpoint, String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("URL must start with http://: {}", url))?;

    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };

    let (host, port) = match authority.rfind(':') {
        Some(i) => {
            let h = &authority[..i];
            let p = authority[i + 1..]
                .parse::<u16>()
                .map_err(|e| format!("invalid port in {}: {}", url, e))?;
            (h.to_string(), p)
        }
        None => (authority.to_string(), 80),
    };

    Ok(Endpoint {
        host,
        port,
        path: path.to_string(),
    })
}

// -- Exporter background loop ------------------------------------------------

const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(400),
    Duration::from_millis(1600),
];

/// Polling interval when the channel has been empty (idle mode).
/// Reduces to `ACTIVE_CHECK_INTERVAL` as soon as a message arrives.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// Polling interval when messages are flowing.
const ACTIVE_CHECK_INTERVAL: Duration = Duration::from_millis(10);

async fn exporter_loop(
    rx: Receiver<ExportMessage>,
    traces_endpoint: Option<Endpoint>,
    logs_endpoint: Option<Endpoint>,
    metrics_endpoint: Option<Endpoint>,
    batch_size: usize,
    flush_interval: Duration,
) {
    let mut trace_batch: Vec<Bytes> = Vec::new();
    let mut log_batch: Vec<Bytes> = Vec::new();
    let mut metrics_batch: Vec<Bytes> = Vec::new();

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
                        flush_batch(&mut trace_batch, traces_endpoint.as_ref()).await;
                    }
                }
                Ok(ExportMessage::Logs(data)) => {
                    got_messages = true;
                    log_batch.push(data);
                    if log_batch.len() >= batch_size {
                        flush_batch(&mut log_batch, logs_endpoint.as_ref()).await;
                    }
                }
                Ok(ExportMessage::Metrics(data)) => {
                    got_messages = true;
                    metrics_batch.push(data);
                    if metrics_batch.len() >= batch_size {
                        flush_batch(&mut metrics_batch, metrics_endpoint.as_ref()).await;
                    }
                }
                Ok(ExportMessage::Flush(done)) => {
                    flush_batch(&mut trace_batch, traces_endpoint.as_ref()).await;
                    flush_batch(&mut log_batch, logs_endpoint.as_ref()).await;
                    flush_batch(&mut metrics_batch, metrics_endpoint.as_ref()).await;
                    let _ = done.send(());
                    time_since_flush = Duration::ZERO;
                }
                Ok(ExportMessage::Shutdown(done)) => {
                    flush_batch(&mut trace_batch, traces_endpoint.as_ref()).await;
                    flush_batch(&mut log_batch, logs_endpoint.as_ref()).await;
                    flush_batch(&mut metrics_batch, metrics_endpoint.as_ref()).await;
                    let _ = done.send(());
                    return;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    // All senders dropped; flush remaining and exit.
                    flush_batch(&mut trace_batch, traces_endpoint.as_ref()).await;
                    flush_batch(&mut log_batch, logs_endpoint.as_ref()).await;
                    flush_batch(&mut metrics_batch, metrics_endpoint.as_ref()).await;
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
            flush_batch(&mut trace_batch, traces_endpoint.as_ref()).await;
            flush_batch(&mut log_batch, logs_endpoint.as_ref()).await;
            flush_batch(&mut metrics_batch, metrics_endpoint.as_ref()).await;
        }
    }
}

/// Concatenate batch payloads and POST them.
///
/// Protobuf repeated fields merge on concatenation, so multiple
/// `ExportTraceServiceRequest` payloads concatenated produce a valid
/// message with multiple `ResourceSpans`.
async fn flush_batch(batch: &mut Vec<Bytes>, endpoint: Option<&Endpoint>) {
    if batch.is_empty() {
        return;
    }
    let endpoint = match endpoint {
        Some(ep) => ep,
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

    post_with_retry(endpoint, &payload).await;
}

/// POST with exponential backoff. On total failure, drop the batch.
///
/// Uses `eprintln!` intentionally -- not `tracing::warn!` -- because this runs
/// inside the telemetry pipeline. Using tracing here would re-enter the OtlpLayer
/// and cause infinite recursion.
async fn post_with_retry(endpoint: &Endpoint, body: &[u8]) {
    for (attempt, delay) in RETRY_DELAYS.iter().enumerate() {
        match http_post(endpoint, body).await {
            Ok(status) if (200..300).contains(&status) => return,
            Ok(status) => {
                eprintln!(
                    "rolly-monoio: export attempt {}/{} to {}:{}{} failed: HTTP {}",
                    attempt + 1,
                    RETRY_DELAYS.len(),
                    endpoint.host,
                    endpoint.port,
                    endpoint.path,
                    status,
                );
            }
            Err(e) => {
                eprintln!(
                    "rolly-monoio: export attempt {}/{} to {}:{}{} failed: {}",
                    attempt + 1,
                    RETRY_DELAYS.len(),
                    endpoint.host,
                    endpoint.port,
                    endpoint.path,
                    e,
                );
            }
        }
        monoio::time::sleep(*delay).await;
    }
    eprintln!(
        "rolly-monoio: dropping batch after {} retries",
        RETRY_DELAYS.len()
    );
}

/// Perform a raw HTTP/1.1 POST using monoio's TcpStream (ownership-based I/O).
///
/// Opens a new TCP connection per request (no connection pooling). This is
/// simpler than managing a connection pool but pays the TCP handshake cost
/// on each export. Acceptable for typical OTLP flush intervals (1-10s).
async fn http_post(endpoint: &Endpoint, body: &[u8]) -> Result<u16, std::io::Error> {
    use monoio::io::{AsyncReadRent, AsyncWriteRentExt};

    let addr = format!("{}:{}", endpoint.host, endpoint.port);
    let mut stream = monoio::net::TcpStream::connect(addr).await?;

    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/x-protobuf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        endpoint.path, endpoint.host, endpoint.port, body.len()
    );

    let mut data = request.into_bytes();
    data.extend_from_slice(body);

    let (result, _) = stream.write_all(data).await;
    result?;

    // Read response status line. A single read is sufficient for the status
    // line in practice (< 50 bytes, arrives in one TCP segment). If the read
    // returns a partial status line, we parse status 0 and retry — degraded
    // but correct.
    let buf = vec![0u8; 256];
    let (result, buf) = stream.read(buf).await;
    let n = result?;

    let response = String::from_utf8_lossy(&buf[..n]);
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_counter_is_callable() {
        let _count = rolly::telemetry_dropped_total();
    }

    #[test]
    fn parse_url_basic() {
        let ep = parse_url("http://localhost:4318/v1/traces").unwrap();
        assert_eq!(ep.host, "localhost");
        assert_eq!(ep.port, 4318);
        assert_eq!(ep.path, "/v1/traces");
    }

    #[test]
    fn parse_url_default_port() {
        let ep = parse_url("http://example.com/v1/logs").unwrap();
        assert_eq!(ep.host, "example.com");
        assert_eq!(ep.port, 80);
        assert_eq!(ep.path, "/v1/logs");
    }

    #[test]
    fn parse_url_no_path() {
        let ep = parse_url("http://localhost:4318").unwrap();
        assert_eq!(ep.host, "localhost");
        assert_eq!(ep.port, 4318);
        assert_eq!(ep.path, "/");
    }

    #[test]
    fn parse_url_rejects_https() {
        assert!(parse_url("https://localhost:4318/v1/traces").is_err());
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
