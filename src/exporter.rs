use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;

/// Message types sent to the exporter background task.
#[derive(Debug)]
pub(crate) enum ExportMessage {
    Traces(Bytes),
    Logs(Bytes),
    Flush(tokio::sync::oneshot::Sender<()>),
    Shutdown,
}

/// Configuration for the exporter.
pub(crate) struct ExporterConfig {
    pub traces_url: Option<String>,
    pub logs_url: Option<String>,
    pub channel_capacity: usize,
}

/// Handle to the exporter background task.
#[derive(Clone)]
pub(crate) struct Exporter {
    tx: mpsc::Sender<ExportMessage>,
}

impl Exporter {
    /// Start the exporter background task. Returns a handle for sending data.
    pub fn start(config: ExporterConfig) -> Self {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build reqwest client");
        tokio::spawn(exporter_loop(rx, client, config.traces_url, config.logs_url));
        Self { tx }
    }

    /// Create an exporter for testing that doesn't spawn the HTTP loop.
    /// Returns the exporter and receiver so tests can read messages directly.
    #[cfg(test)]
    pub fn start_test() -> (Self, mpsc::Receiver<ExportMessage>) {
        let (tx, rx) = mpsc::channel(64);
        (Self { tx }, rx)
    }

    /// Send encoded trace data to the exporter.
    pub fn send_traces(&self, data: Vec<u8>) {
        let _ = self.tx.try_send(ExportMessage::Traces(Bytes::from(data)));
    }

    /// Send encoded log data to the exporter.
    pub fn send_logs(&self, data: Vec<u8>) {
        let _ = self.tx.try_send(ExportMessage::Logs(Bytes::from(data)));
    }

    /// Flush all pending data. Blocks until the exporter has processed everything.
    pub async fn flush(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(ExportMessage::Flush(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    /// Signal the exporter to stop after draining remaining messages.
    pub async fn shutdown(&self) {
        let _ = self.tx.send(ExportMessage::Shutdown).await;
    }
}

const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(400),
    Duration::from_millis(1600),
];

async fn exporter_loop(
    mut rx: mpsc::Receiver<ExportMessage>,
    client: reqwest::Client,
    traces_url: Option<String>,
    logs_url: Option<String>,
) {
    while let Some(msg) = rx.recv().await {
        match msg {
            ExportMessage::Traces(data) => {
                if let Some(ref url) = traces_url {
                    post_with_retry(&client, url, data).await;
                }
            }
            ExportMessage::Logs(data) => {
                if let Some(ref url) = logs_url {
                    post_with_retry(&client, url, data).await;
                }
            }
            ExportMessage::Flush(done) => {
                // All prior messages processed sequentially; signal completion.
                let _ = done.send(());
            }
            ExportMessage::Shutdown => {
                break;
            }
        }
    }
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
                    "pz-o11y: export attempt {}/{} to {} failed: HTTP {}",
                    attempt + 1,
                    RETRY_DELAYS.len(),
                    url,
                    resp.status()
                );
            }
            Err(e) => {
                eprintln!(
                    "pz-o11y: export attempt {}/{} to {} failed: {}",
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
        "pz-o11y: dropping batch after {} retries",
        RETRY_DELAYS.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exporter_queues_and_flushes_without_panic() {
        let config = ExporterConfig {
            traces_url: Some("http://127.0.0.1:1/v1/traces".to_string()),
            logs_url: Some("http://127.0.0.1:1/v1/logs".to_string()),
            channel_capacity: 16,
        };
        let exporter = Exporter::start(config);

        exporter.send_traces(vec![0x0A, 0x00]);
        exporter.send_logs(vec![0x0A, 0x00]);

        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn exporter_flush_completes() {
        let config = ExporterConfig {
            traces_url: Some("http://127.0.0.1:1/v1/traces".to_string()),
            logs_url: Some("http://127.0.0.1:1/v1/logs".to_string()),
            channel_capacity: 16,
        };
        let exporter = Exporter::start(config);

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

        let config = ExporterConfig {
            traces_url: Some(format!("http://{}/v1/traces", addr)),
            logs_url: Some(format!("http://{}/v1/logs", addr)),
            channel_capacity: 16,
        };
        let exporter = Exporter::start(config);

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
        let config = ExporterConfig {
            traces_url: Some(format!("http://{}/v1/traces", addr)),
            logs_url: None,
            channel_capacity: 16,
        };
        let exporter = Exporter::start(config);

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
}
